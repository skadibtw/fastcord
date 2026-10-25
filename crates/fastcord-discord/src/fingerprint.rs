//! The pre-login fingerprint (ADR 0005, IDENTITY_AUDIT A5): obtained from the
//! unauthenticated `GET /experiments` and sent as `X-Fingerprint` on the login
//! requests that have no token yet. Never a captured value.

use serde::Deserialize;

/// The answer also carries experiment assignments; nothing near this size is
/// a real one.
const BODY_BYTE_BUDGET: usize = 256 * 1024;

/// The fingerprint for this process, fetched with `http` (a client carrying the
/// identity headers) on first use. Only a success is kept: the endpoint allows
/// three calls per two minutes per address, and a failed fetch is tried again
/// at the next login attempt rather than never.
// ponytail: one per process, not persisted; persist it beside the installation
// id if a capture shows the official client keeps it across launches.
pub(crate) async fn fingerprint(http: &reqwest::Client) -> Option<String> {
    static FINGERPRINT: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    FINGERPRINT
        .get_or_try_init(|| async { fetch(http).await.ok_or(()) })
        .await
        .ok()
        .cloned()
}

async fn fetch(http: &reqwest::Client) -> Option<String> {
    let mut request = http
        .get(format!("{}/experiments", crate::route::api_base()))
        .build()
        .ok()?;
    if let Some(cookies) = crate::cookies::header(request.url()) {
        request
            .headers_mut()
            .insert(reqwest::header::COOKIE, cookies);
    }
    let mut response = http.execute(request).await.ok()?;
    crate::cookies::store(response.url(), response.headers());
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > BODY_BYTE_BUDGET - body.len() {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    parse(&body)
}

/// `{"fingerprint": "<snowflake>.<hash>", "assignments": [...]}`; the
/// assignments are skipped unread.
fn parse(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Experiments {
        fingerprint: String,
    }
    let fingerprint = serde_json::from_slice::<Experiments>(body)
        .ok()?
        .fingerprint;
    // Fingerprints and installation ids share one shape.
    crate::gateway::is_installation_id(&fingerprint).then_some(fingerprint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_is_read_and_checked() {
        assert_eq!(
            parse(br#"{"fingerprint":"1084179945133187083.JQddgNMmwJPghoBtFmaH7jTmdsw","assignments":[[4130837190,0,1,-1,0]]}"#)
                .as_deref(),
            Some("1084179945133187083.JQddgNMmwJPghoBtFmaH7jTmdsw")
        );
        for body in [
            "",
            "{}",
            r#"{"assignments":[]}"#,
            r#"{"fingerprint":5}"#,
            r#"{"fingerprint":"not a fingerprint"}"#,
            r#"{"fingerprint":"1.a\r\nx"}"#,
        ] {
            assert_eq!(parse(body.as_bytes()), None, "{body}");
        }
    }
}
