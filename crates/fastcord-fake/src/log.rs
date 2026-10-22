//! The request log: one JSON line per HTTP request and Gateway op, on stdout
//! and in a file. Bodies and headers pass through [`crate::redact`] first.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::redact;

pub struct Log {
    file: Mutex<Option<File>>,
    echo: bool,
}

/// What one exchange looked like from the client's side.
pub struct Entry<'a> {
    /// `http` or `ws`.
    pub kind: &'static str,
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    /// The status returned (HTTP), 101 for the upgrade and the close code for
    /// a closed socket (ws), `None` for an op the server only receives.
    pub status: Option<u16>,
    pub body_bytes: usize,
    pub headers: Option<&'a BTreeMap<String, String>>,
    pub body: Option<Value>,
    /// `UNIMPLEMENTED` or `RATE_LIMITED` when the fake did not really serve it.
    pub note: Option<&'static str>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Log {
    pub fn open(path: Option<&Path>, echo: bool) -> std::io::Result<Self> {
        let file = path
            .map(|path| File::options().create(true).append(true).open(path))
            .transpose()?;
        Ok(Self {
            file: Mutex::new(file),
            echo,
        })
    }

    pub fn line(entry: &Entry<'_>, ts_ms: u64) -> Value {
        // Redaction happens here, on the way in, so nothing unredacted is ever written.
        let body = entry.body.as_ref().map(redact::json);
        let mut line = json!({
            "ts_ms": ts_ms,
            "kind": entry.kind,
            "method": entry.method,
            "path": entry.path,
            "query": entry.query,
            "status": entry.status,
            "body_bytes": entry.body_bytes,
            "body": body,
        });
        if let Some(headers) = entry.headers {
            line["headers"] = redact::headers(headers);
        }
        if let Some(note) = entry.note {
            line["note"] = note.into();
        }
        line
    }

    pub fn record(&self, entry: &Entry<'_>) {
        let line = Self::line(entry, now_ms()).to_string();
        if self.echo {
            let query = if entry.query.is_empty() {
                String::new()
            } else {
                format!("?{}", entry.query)
            };
            let status = entry
                .status
                .map_or_else(String::new, |s| format!(" -> {s}"));
            let note = entry.note.map_or_else(String::new, |n| format!(" [{n}]"));
            println!(
                "{} {} {}{query}{status} ({} B){note}",
                entry.kind.to_uppercase(),
                entry.method,
                entry.path,
                entry.body_bytes
            );
        }
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_carry_the_documented_fields_with_credentials_redacted() {
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_owned(), "fake-token".to_owned());
        headers.insert("user-agent".to_owned(), "ua".to_owned());
        let entry = Entry {
            kind: "http",
            method: "POST",
            path: "/api/v10/auth/login",
            query: "a=b",
            status: Some(404),
            body_bytes: 40,
            headers: Some(&headers),
            body: Some(json!({"login": "me", "password": "hunter2", "captcha_key": "k"})),
            note: Some("UNIMPLEMENTED"),
        };
        let line = Log::line(&entry, 1234);
        for key in [
            "ts_ms",
            "kind",
            "method",
            "path",
            "query",
            "status",
            "body_bytes",
            "body",
        ] {
            assert!(line.get(key).is_some(), "{key}");
        }
        let text = line.to_string();
        assert!(!text.contains("hunter2") && !text.contains("fake-token"));
        assert_eq!(line["body"]["login"], "me");
        assert_eq!(line["headers"]["user-agent"], "ua");
        assert_eq!(line["note"], "UNIMPLEMENTED");
    }
}
