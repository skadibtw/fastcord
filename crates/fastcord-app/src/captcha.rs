//! Shows a Discord-issued CAPTCHA to the person in a short-lived native web
//! view and returns the key their own interaction produced. Nothing here
//! solves or bypasses a challenge; the window exists only while one is pending.
//!
//! The widget is bound to the `discord.com` origin, so the view opens a static
//! `discord.com` page and replaces its body with the challenge widget. The view
//! owns its own thread and event loop (never the UI thread's) and a throwaway
//! profile directory that is deleted afterwards, so no cookies persist.

use fastcord_discord::{CaptchaChallenge, CaptchaSolution};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptchaError {
    /// No embedded view on this platform or the web-view runtime is missing.
    Unsupported,
    /// The window was closed before a challenge was completed.
    Dismissed,
}

impl std::fmt::Display for CaptchaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str(
                "Discord asked for a CAPTCHA, which this platform cannot show. Use QR or token login.",
            ),
            Self::Dismissed => f.write_str("CAPTCHA closed before it was completed. Log in again."),
        }
    }
}

/// The page that replaces the body: loads the provider script and reports the
/// response key to the host through `window.ipc`. Values are JSON-escaped.
fn widget_script(challenge: &CaptchaChallenge) -> Option<String> {
    if challenge.service != "hcaptcha" {
        return None;
    }
    let sitekey = serde_json::to_string(&challenge.sitekey).ok()?;
    let rqdata = serde_json::to_string(&challenge.rqdata).ok()?;
    Some(format!(
        r#"document.addEventListener('DOMContentLoaded', function () {{
  document.documentElement.innerHTML = '<head><meta charset="utf-8"><title>CAPTCHA</title></head><body style="margin:0;padding:24px;display:flex;justify-content:center;background:#fff"><div id="c"></div></body>';
  window.fcLoaded = function () {{
    var options = {{ sitekey: {sitekey}, callback: function (t) {{ window.ipc.postMessage(t); }} }};
    var rqdata = {rqdata};
    if (rqdata) {{ options.rqdata = rqdata; }}
    hcaptcha.render('c', options);
  }};
  var s = document.createElement('script');
  s.src = 'https://js.hcaptcha.com/1/api.js?render=explicit&onload=fcLoaded';
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
        .with_title("fastcord - complete the CAPTCHA")
        .with_inner_size(LogicalSize::new(440.0, 640.0))
        .build(&event_loop)
        .map_err(|_| CaptchaError::Unsupported)?;
    let profile = std::env::temp_dir().join(format!("fastcord-captcha-{}", std::process::id()));
    let mut context = WebContext::new(Some(profile.clone()));
    let answer: Arc<Mutex<Option<String>>> = Arc::default();
    let sink = Arc::clone(&answer);
    let proxy = event_loop.create_proxy();
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url("https://discord.com/robots.txt")
        .with_initialization_script(script)
        .with_ipc_handler(move |request| {
            let key = request.into_body();
            if !key.is_empty() && key.len() <= 8192 {
                *sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(key);
                let _ = proxy.send_event(());
            }
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
        .ok_or(CaptchaError::Dismissed)
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
        }
    }

    #[test]
    fn only_hcaptcha_is_shown_and_values_are_escaped() {
        assert!(widget_script(&challenge("recaptcha", "k")).is_none());
        let script = widget_script(&challenge("hcaptcha", "a\"</script>")).unwrap();
        assert!(script.contains(r#"sitekey: "a\"</script>""#));
        assert!(script.contains("var rqdata = null;"));
    }
}
