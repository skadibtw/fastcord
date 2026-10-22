//! A minimal HTTP/1.1 reader and writer: Content-Length bodies, keep-alive.

use std::collections::BTreeMap;
use std::io;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// The path without its query, still percent-encoded.
    pub path: String,
    pub query: String,
    /// Header names lowercased.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// `Connection: close` ends the exchange after the answer.
    pub fn wants_close(&self) -> bool {
        self.header("connection")
            .is_some_and(|value| value.eq_ignore_ascii_case("close"))
    }

    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    pub fn query_pairs(&self) -> Vec<(String, String)> {
        self.query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (percent_decode(key), percent_decode(value))
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Malformed,
    TooLarge,
    /// `Transfer-Encoding` is not supported; bodies must carry a length.
    Chunked,
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", "application/json".to_owned())],
            body: value.to_string().into_bytes(),
        }
    }

    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn json_body(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// Parses the request line and headers (everything before the blank line).
pub fn parse_head(head: &str) -> Result<Request, ParseError> {
    let mut lines = head.split("\r\n");
    let line = lines.next().ok_or(ParseError::Malformed)?;
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next(), parts.next(), parts.next());
    let (Some(method), Some(target), Some(version), None) = (method, target, version, parts.next())
    else {
        return Err(ParseError::Malformed);
    };
    if method.is_empty() || !target.starts_with('/') || !version.starts_with("HTTP/1.") {
        return Err(ParseError::Malformed);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(ParseError::Malformed)?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    if headers.contains_key("transfer-encoding") {
        return Err(ParseError::Chunked);
    }
    Ok(Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query: query.to_owned(),
        headers,
        body: Vec::new(),
    })
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Reads one request. `buf` carries bytes already read (pipelining and
/// keep-alive); `Ok(None)` is a clean end of the connection between requests.
pub async fn read_request<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<Option<Result<Request, ParseError>>> {
    let head_end = loop {
        if let Some(end) = find_head_end(buf) {
            break end;
        }
        if buf.len() > MAX_HEAD {
            return Ok(Some(Err(ParseError::TooLarge)));
        }
        let mut chunk = [0u8; 8192];
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = match std::str::from_utf8(&buf[..head_end]) {
        Ok(head) => head.to_owned(),
        Err(_) => return Ok(Some(Err(ParseError::Malformed))),
    };
    let mut request = match parse_head(&head) {
        Ok(request) => request,
        Err(error) => return Ok(Some(Err(error))),
    };
    let length = match request.header("content-length") {
        None => 0,
        Some(value) => match value.parse::<usize>() {
            Ok(length) if length <= MAX_BODY => length,
            Ok(_) => return Ok(Some(Err(ParseError::TooLarge))),
            Err(_) => return Ok(Some(Err(ParseError::Malformed))),
        },
    };
    let total = head_end + 4 + length;
    while buf.len() < total {
        let mut chunk = [0u8; 8192];
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    request.body = buf[head_end + 4..total].to_vec();
    buf.drain(..total);
    Ok(Some(Ok(request)))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        _ => "Unknown",
    }
}

pub async fn write_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: &Response,
    close: bool,
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
        response.status,
        reason(response.status),
        response.body.len(),
        if close { "close" } else { "keep-alive" },
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes()).await?;
    writer.write_all(&response.body).await?;
    writer.flush().await
}

/// `%XX` escapes decoded; `+` stays a plus (these are not form bodies).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[test]
    fn head_parses_target_query_and_lowercases_headers() {
        let request = parse_head(
            "GET /api/v10/channels/5/messages?limit=50&before=9 HTTP/1.1\r\nHost: x\r\nAuthorization: abc\r\nContent-Length: 0",
        )
        .unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/api/v10/channels/5/messages");
        assert_eq!(request.header("authorization"), Some("abc"));
        assert_eq!(
            request.query_pairs(),
            [("limit".into(), "50".into()), ("before".into(), "9".into())]
        );
    }

    #[test]
    fn malformed_and_chunked_heads_are_refused() {
        for head in [
            "",
            "GET",
            "GET / HTTP/1.1 extra",
            "GET nope HTTP/1.1",
            "GET / FTP/1.0",
        ] {
            assert_eq!(parse_head(head), Err(ParseError::Malformed), "{head:?}");
        }
        assert_eq!(
            parse_head("POST / HTTP/1.1\r\nTransfer-Encoding: chunked"),
            Err(ParseError::Chunked)
        );
        assert_eq!(
            parse_head("GET / HTTP/1.1\r\nbad header"),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn percent_escapes_decode() {
        assert_eq!(percent_decode("%F0%9F%91%8D"), "\u{1F44D}");
        assert_eq!(percent_decode("a%2Fb%"), "a/b%");
        assert_eq!(percent_decode("wave%3A12"), "wave:12");
    }

    #[tokio::test]
    async fn requests_are_read_across_partial_writes_and_pipelined() {
        let (mut client, mut server) = duplex(64);
        let writer = tokio::spawn(async move {
            let one = "POST /a HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
            let two = "GET /b?x=1 HTTP/1.1\r\n\r\n";
            for chunk in format!("{one}{two}").as_bytes().chunks(7) {
                client.write_all(chunk).await.unwrap();
            }
        });
        let mut buf = Vec::new();
        let first = read_request(&mut server, &mut buf)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            (first.method.as_str(), first.body.as_slice()),
            ("POST", &b"hello"[..])
        );
        let second = read_request(&mut server, &mut buf)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!((second.path.as_str(), second.query.as_str()), ("/b", "x=1"));
        writer.await.unwrap();
        assert!(read_request(&mut server, &mut buf).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_bodies_are_refused_and_responses_carry_length() {
        let (mut client, mut server) = duplex(1024);
        client
            .write_all(b"POST / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        assert_eq!(
            read_request(&mut server, &mut buf)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            ParseError::TooLarge
        );
        let mut out = Vec::new();
        write_response(
            &mut out,
            &Response::json(200, &serde_json::json!({"a": 1})),
            false,
        )
        .await
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\nContent-Length: 7\r\n"));
        assert!(text.ends_with("\r\n\r\n{\"a\":1}"));
    }
}
