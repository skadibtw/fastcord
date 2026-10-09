//! Authenticated refresh of expired Discord attachment CDN URLs.
//!
//! `POST /attachments/refresh-urls` takes the URLs to refresh and answers with
//! `original`/`refreshed` pairs (Userdoccers "Cloud Uploads", S7). The caller
//! keeps its attachment identity and replaces only the URL it fetches.
use std::fmt;

use serde::{Deserialize, Serialize};

/// A newly signed URL for one requested URL. Both URLs are deliberately omitted
/// from `Debug` because their queries are secrets.
#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct RefreshedAttachmentUrl {
    pub original: String,
    pub refreshed: String,
}

impl fmt::Debug for RefreshedAttachmentUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshedAttachmentUrl")
            .field("original", &"[REDACTED]")
            .field("refreshed", &"[REDACTED]")
            .finish()
    }
}

#[derive(Serialize)]
pub(crate) struct RefreshRequest<'a> {
    pub attachment_urls: &'a [String],
}

#[derive(Deserialize)]
pub(crate) struct RefreshResponse {
    pub refreshed_urls: Vec<RefreshedAttachmentUrl>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refreshed_url_debug_never_discloses_url_contents() {
        let refreshed = RefreshedAttachmentUrl {
            original: "opaque-original-marker".to_owned(),
            refreshed: "opaque-refreshed-marker".to_owned(),
        };
        let debug = format!("{refreshed:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("opaque-original-marker"));
        assert!(!debug.contains("opaque-refreshed-marker"));
    }

    #[test]
    fn refresh_request_posts_only_attachment_urls() {
        let urls = vec!["opaque-request-marker".to_owned()];
        let request = RefreshRequest {
            attachment_urls: &urls,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({"attachment_urls": ["opaque-request-marker"]})
        );
    }

    #[test]
    fn refreshed_response_pairs_original_with_refreshed_and_redacts_both() {
        let response: RefreshResponse = serde_json::from_value(serde_json::json!({
            "refreshed_urls": [{
                "original": "opaque-original-marker",
                "refreshed": "opaque-refreshed-marker"
            }]
        }))
        .unwrap();
        assert_eq!(
            response.refreshed_urls[0].original,
            "opaque-original-marker"
        );
        assert_eq!(
            response.refreshed_urls[0].refreshed,
            "opaque-refreshed-marker"
        );
        let debug = format!("{:?}", response.refreshed_urls[0]);
        assert!(!debug.contains("marker"));
    }
}
