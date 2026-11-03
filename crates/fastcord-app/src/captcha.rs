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
        r#"(function () {{
if (window.top !== window) {{ return; }}
window.setTimeout(function () {{
  if (!window.fcWidgetReady) {{ window.ipc.postMessage('{FAILED}'); }}
}}, 20000);
document.addEventListener('DOMContentLoaded', function () {{
  document.documentElement.innerHTML = '<head><meta charset="utf-8"><title>CAPTCHA</title></head><body style="margin:0;padding:24px;display:flex;justify-content:center;background:#fff"><div id="c"></div></body>';
  window.fcLoaded = function () {{
    var options = {{ sitekey: {sitekey}, callback: function (t) {{ window.ipc.postMessage(t); }} }};
    var rqdata = {rqdata};
    try {{
      var widget = hcaptcha.render('c', options);
      if (rqdata) {{ hcaptcha.setData(widget, {{ rqdata: rqdata }}); }}
    }} catch (_) {{
      window.ipc.postMessage('{FAILED}');
      return;
    }}
    window.fcWidgetReady = true;
  }};
  var s = document.createElement('script');
  s.src = 'https://js.hcaptcha.com/1/api.js?render=explicit&onload=fcLoaded';
  s.onerror = function () {{ window.ipc.postMessage('{FAILED}'); }};
  document.head.appendChild(s);
}});
}})();"#
    ))
}

#[cfg(windows)]
pub async fn solve(challenge: CaptchaChallenge) -> Result<CaptchaSolution, CaptchaError> {
    let script = widget_script(&challenge).ok_or(CaptchaError::Unsupported)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("captcha-view".into())
        .spawn(move || {
            let result = run_window(script, || tx.is_closed());
            let _ = tx.send(result);
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

/// A fresh profile for one challenge; never reuse storage from an earlier login.
#[cfg(any(windows, test))]
struct Profile(std::path::PathBuf);

#[cfg(any(windows, test))]
impl Profile {
    fn create() -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "fastcord-captcha-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(any(windows, test))]
impl Drop for Profile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(windows)]
fn create_window(
    event_loop: &tao::event_loop::EventLoop<()>,
) -> Result<tao::window::Window, CaptchaError> {
    use tao::platform::windows::WindowBuilderExtWindows;
    // Both tao and iced's winit default to "Window Class". Reusing winit's
    // class dispatches tao window data to the wrong callback and crashes.
    tao::window::WindowBuilder::new()
        .with_window_classname("fastcord.CaptchaWindow")
        .with_title(tr("fastcord - complete the CAPTCHA"))
        .with_inner_size(tao::dpi::LogicalSize::new(440.0, 640.0))
        .build(event_loop)
        .map_err(|_| CaptchaError::Unsupported)
}

#[cfg(windows)]
fn run_window(script: String, cancelled: impl Fn() -> bool) -> Result<String, CaptchaError> {
    if cancelled() {
        return Err(CaptchaError::Dismissed);
    }
    use std::sync::{Arc, Mutex};

    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use tao::platform::windows::EventLoopBuilderExtWindows;
    use wry::{WebContext, WebViewBuilder};

    let mut event_loop = EventLoopBuilder::new().with_any_thread(true).build();
    let window = create_window(&event_loop)?;
    let profile = Profile::create().map_err(|_| CaptchaError::Unsupported)?;
    let mut context = WebContext::new(Some(profile.0.clone()));
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
            // Release WebView2 storage handles before removing its directory.
            drop(context);
            drop(profile);
            return Err(error);
        }
    };
    event_loop.run_return(|event, _, control_flow| {
        // Only a live challenge polls. Dropping the owning QR attempt closes
        // the native view too, rather than leaving a retired CAPTCHA behind.
        *control_flow = ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(250),
        );
        if cancelled() {
            *control_flow = ControlFlow::Exit;
            return;
        }
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
    drop(profile);
    if cancelled() {
        return Err(CaptchaError::Dismissed);
    }
    answer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or(Err(CaptchaError::Dismissed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    #[ignore = "opens native windows; no account or network"]
    fn native_window_coexists_with_iced_window_class() {
        const CHILD: &str = "FASTCORD_CAPTCHA_CLASS_TEST_CHILD";
        if std::env::var(CHILD).as_deref() == Ok("1") {
            use tao::platform::windows::EventLoopBuilderExtWindows as _;
            use winit::platform::windows::EventLoopBuilderExtWindows as _;
            let app_loop = winit::event_loop::EventLoop::builder()
                .with_any_thread(true)
                .build()
                .unwrap();
            #[allow(deprecated)]
            let _app_window = app_loop
                .create_window(winit::window::Window::default_attributes().with_visible(false))
                .unwrap();
            let captcha_loop = tao::event_loop::EventLoopBuilder::new()
                .with_any_thread(true)
                .build();
            let window = create_window(&captcha_loop).unwrap();
            assert!(window.inner_size().width > 0);
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "captcha::tests::native_window_coexists_with_iced_window_class",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "iced + CAPTCHA native window process failed: {status}"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("iced + CAPTCHA native window process exceeded 30 seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

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
    fn challenge_profiles_are_isolated_and_removed_on_early_return() {
        let first = Profile::create().unwrap();
        let second = Profile::create().unwrap();
        assert_ne!(first.0, second.0);
        std::fs::write(first.0.join("session"), b"synthetic").unwrap();
        let path = first.0.clone();
        drop(first);
        assert!(!path.exists());
        assert!(second.0.exists());
        let path = second.0.clone();
        drop(second);
        assert!(!path.exists());
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
