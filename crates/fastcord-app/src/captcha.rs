//! Shows a Discord-issued CAPTCHA to the person in a short-lived native web
//! view and returns the key their own interaction produced. Nothing here
//! solves or bypasses a challenge; the window exists only while one is pending.
//!
//! The widget is bound to the `discord.com` origin, so the view opens a static
//! `discord.com` page and replaces its body with the challenge widget. The view
//! owns its own thread and event loop (never the UI thread's) and a throwaway
//! profile directory that is deleted afterwards, so no cookies persist.

use fastcord_discord::{CaptchaChallenge, CaptchaSolution};

use crate::i18n::tr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptchaError {
    /// No embedded view on this platform or the web-view runtime is missing.
    Unsupported,
    /// The window was closed before a challenge was completed.
    #[cfg_attr(not(windows), allow(dead_code))]
    Dismissed,
    /// The page never got the widget: the view has its own, direct network path.
    #[cfg_attr(not(windows), allow(dead_code))]
    Unreachable,
}

impl std::fmt::Display for CaptchaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str(tr(
                "Discord asked for a CAPTCHA, which this platform cannot show. Use QR or token login.",
            )),
            Self::Dismissed => {
                f.write_str(tr("CAPTCHA closed before it was completed. Log in again."))
            }
            Self::Unreachable => f.write_str(tr(
                "The CAPTCHA did not load. This window connects directly and does not use fastcord's network recovery: if discord.com or js.hcaptcha.com is blocked here, use QR or token login.",
            )),
        }
    }
}

/// The page reports the key the person's interaction produced, or that the
/// widget never loaded. A key can never be mistaken for the failure marker.
const FAILED: &str = "fc-captcha-failed";

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Report<'a> {
    Key(&'a str),
    Failed,
}

#[cfg_attr(not(windows), allow(dead_code))]
fn report(text: &str) -> Option<Report<'_>> {
    if text == FAILED {
        return Some(Report::Failed);
    }
    let key = text.trim();
    (!key.is_empty() && key.len() <= 8192 && !key.starts_with(FAILED)).then_some(Report::Key(key))
}

/// The page that replaces the body: loads the provider script and reports the
/// response key to the host through `window.ipc`. Values are JSON-escaped.
#[cfg_attr(not(windows), allow(dead_code))]
fn widget_script(challenge: &CaptchaChallenge) -> Option<String> {
    if challenge.service != "hcaptcha" {
        return None;
    }
    let sitekey = serde_json::to_string(&challenge.sitekey).ok()?;
    let rqdata = serde_json::to_string(&challenge.rqdata).ok()?;
    Some(format!(
        r#"window.setTimeout(function () {{
  if (!window.fcWidgetReady) {{ window.ipc.postMessage('{FAILED}'); }}
}}, 20000);
document.addEventListener('DOMContentLoaded', function () {{
  document.documentElement.innerHTML = '<head><meta charset="utf-8"><title>CAPTCHA</title></head><body style="margin:0;padding:24px;display:flex;justify-content:center;background:#fff"><div id="c"></div></body>';
  window.fcLoaded = function () {{
    var options = {{ sitekey: {sitekey}, callback: function (t) {{ window.ipc.postMessage(t); }} }};
    var rqdata = {rqdata};
    if (rqdata) {{ options.rqdata = rqdata; }}
    window.fcWidgetReady = true;
    hcaptcha.render('c', options);
  }};
  var s = document.createElement('script');
  s.src = 'https://js.hcaptcha.com/1/api.js?render=explicit&onload=fcLoaded';
  s.onerror = function () {{ window.ipc.postMessage('{FAILED}'); }};
  document.head.appendChild(s);
}});"#
    ))
}

#[cfg(windows)]
pub async fn solve(challenge: CaptchaChallenge) -> Result<CaptchaSolution, CaptchaError> {
    let script = widget_script(&challenge).ok_or(CaptchaError::Unsupported)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("captcha-view".into())
        .spawn(move || {
            let _ = tx.send(run_window(script));
        })
        .map_err(|_| CaptchaError::Unsupported)?;
    let key = rx.await.map_err(|_| CaptchaError::Dismissed)??;
    Ok(CaptchaSolution {
        key,
        rqtoken: challenge.rqtoken,
        session_id: challenge.session_id,
    })
}

#[cfg(not(windows))]
pub async fn solve(_challenge: CaptchaChallenge) -> Result<CaptchaSolution, CaptchaError> {
    // ponytail: wry needs the main thread on macOS and GTK on Linux, which iced owns;
    // enable per platform once the view can share or own that loop.
    Err(CaptchaError::Unsupported)
}

#[cfg(windows)]
fn run_window(script: String) -> Result<String, CaptchaError> {
    use std::sync::{Arc, Mutex};

    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use tao::platform::windows::EventLoopBuilderExtWindows;
    use tao::window::WindowBuilder;
    use wry::{WebContext, WebViewBuilder};

    let mut event_loop = EventLoopBuilder::new().with_any_thread(true).build();
    let window = WindowBuilder::new()
        .with_title(tr("fastcord - complete the CAPTCHA"))
        .with_inner_size(LogicalSize::new(440.0, 640.0))
        .build(&event_loop)
        .map_err(|_| CaptchaError::Unsupported)?;
    let profile = std::env::temp_dir().join(format!("fastcord-captcha-{}", std::process::id()));
    let mut context = WebContext::new(Some(profile.clone()));
    let answer: Arc<Mutex<Option<Result<String, CaptchaError>>>> = Arc::default();
    let sink = Arc::clone(&answer);
    let proxy = event_loop.create_proxy();
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url("https://discord.com/robots.txt")
        .with_initialization_script(script)
        .with_ipc_handler(move |request| {
            let body = request.into_body();
            let report = match report(&body) {
                Some(Report::Key(key)) => Ok(key.to_owned()),
                Some(Report::Failed) => Err(CaptchaError::Unreachable),
                None => return,
            };
            *sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
            let _ = proxy.send_event(());
        })
        .build(&window)
        .map_err(|_| CaptchaError::Unsupported);
    let webview = match webview {
        Ok(webview) => webview,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&profile);
            return Err(error);
        }
    };
    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(()) => *control_flow = ControlFlow::Exit,
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
    drop(webview);
    drop(window);
    drop(context);
    let _ = std::fs::remove_dir_all(&profile);
    answer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or(Err(CaptchaError::Dismissed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge(service: &str, sitekey: &str) -> CaptchaChallenge {
        CaptchaChallenge {
            sitekey: sitekey.into(),
            service: service.into(),
            rqdata: None,
            rqtoken: None,
            session_id: None,
        }
    }

    #[test]
    fn only_hcaptcha_is_shown_and_values_are_escaped() {
        assert!(widget_script(&challenge("recaptcha", "k")).is_none());
        let script = widget_script(&challenge("hcaptcha", "a\"</script>")).unwrap();
        assert!(script.contains(r#"sitekey: "a\"</script>""#));
        assert!(script.contains("var rqdata = null;"));
        // A widget that never loads reports itself instead of hanging.
        assert!(script.contains(FAILED));
        assert!(script.contains("fcWidgetReady"));
    }

    #[test]
    fn the_page_reports_only_a_key_or_its_own_failure() {
        assert_eq!(report("token-key"), Some(Report::Key("token-key")));
        assert_eq!(report("  token-key \n"), Some(Report::Key("token-key")));
        assert_eq!(report(FAILED), Some(Report::Failed));
        // Empty, oversized and marker-prefixed messages are not keys.
        assert_eq!(report(""), None);
        assert_eq!(report("   "), None);
        assert_eq!(report(&"k".repeat(8193)), None);
        assert_eq!(
            report(&format!("{FAILED}-but-not-the-marker")),
            None,
            "a key that only starts like the marker is refused"
        );
    }
}
