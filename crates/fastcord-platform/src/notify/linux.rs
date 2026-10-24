//! Linux notifications through the freedesktop `org.freedesktop.Notifications`
//! D-Bus service, which GNOME, KDE and the common standalone daemons provide.
//!
//! One worker thread owns a current-thread runtime and the session-bus
//! connection, started by the first toast. It sends `Notify` and waits for the
//! server's `ActionInvoked` ("default" is the click) and `NotificationClosed`
//! signals; nothing runs between them. No session bus or notification server
//! is [`NotifyError::Unsupported`].

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use ashpd::zbus::message::Type;
use ashpd::zbus::zvariant::Value;
use ashpd::zbus::{Connection, MatchRule, Message, MessageStream};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use super::{NotifyError, OnClick, Pending, escape_markup};

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// How long a toast may take to be accepted by the server.
const ANSWER: Duration = Duration::from_secs(5);
/// Toasts queued for the worker; a full queue fails the toast.
const QUEUE: usize = 4;
/// Signals buffered between worker wakes.
const SIGNALS: usize = 16;

struct Request {
    title: String,
    body: String,
    click: OnClick,
    reply: std_mpsc::SyncSender<Result<(), NotifyError>>,
}

static WORKER: OnceLock<Option<mpsc::Sender<Request>>> = OnceLock::new();

pub(super) fn show(title: &str, body: &str, click: OnClick) -> Result<(), NotifyError> {
    let worker = WORKER
        .get_or_init(start)
        .as_ref()
        .ok_or(NotifyError::Failed)?;
    let (reply, answer) = std_mpsc::sync_channel(1);
    worker
        .try_send(Request {
            title: title.to_owned(),
            body: body.to_owned(),
            click,
            reply,
        })
        .map_err(|_| NotifyError::Failed)?;
    answer
        .recv_timeout(ANSWER)
        .unwrap_or(Err(NotifyError::Failed))
}

fn start() -> Option<mpsc::Sender<Request>> {
    let (sender, requests) = mpsc::channel(QUEUE);
    std::thread::Builder::new()
        .name("fastcord-notify".to_owned())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(run(requests));
        })
        .ok()?;
    Some(sender)
}

/// The connected notification server and what it supports.
struct Server {
    connection: Connection,
    signals: MessageStream,
    /// It reports clicks (`actions` capability).
    actions: bool,
    /// It reads the body as markup (`body-markup` capability).
    markup: bool,
}

impl Server {
    async fn connect() -> ashpd::zbus::Result<Self> {
        let connection = Connection::session().await?;
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .interface(SERVICE)?
            .path(PATH)?
            .build();
        // Subscribed before the first Notify, so no click can be missed.
        let signals = MessageStream::for_match_rule(rule, &connection, Some(SIGNALS)).await?;
        let reply = connection
            .call_method(Some(SERVICE), PATH, Some(SERVICE), "GetCapabilities", &())
            .await?;
        let capabilities: Vec<String> = reply.body().deserialize()?;
        let has = |name: &str| capabilities.iter().any(|c| c == name);
        Ok(Self {
            actions: has("actions"),
            markup: has("body-markup"),
            connection,
            signals,
        })
    }

    async fn notify(&self, title: &str, body: &str) -> Result<u32, NotifyError> {
        let body = if self.markup {
            escape_markup(body)
        } else {
            body.to_owned()
        };
        let actions: &[&str] = if self.actions {
            &["default", "Open"]
        } else {
            &[]
        };
        let hints: HashMap<&str, Value<'_>> = HashMap::new();
        let reply = self
            .connection
            .call_method(
                Some(SERVICE),
                PATH,
                Some(SERVICE),
                "Notify",
                &("fastcord", 0u32, "", title, body, actions, hints, -1i32),
            )
            .await
            .map_err(|_| NotifyError::Failed)?;
        reply.body().deserialize().map_err(|_| NotifyError::Failed)
    }
}

async fn run(mut requests: mpsc::Receiver<Request>) {
    let Ok(mut server) = Server::connect().await else {
        while let Some(request) = requests.recv().await {
            let _ = request.reply.send(Err(NotifyError::Unsupported));
        }
        return;
    };
    let mut pending = Pending::new();
    loop {
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else { return };
                let result = server.notify(&request.title, &request.body).await;
                if let Ok(id) = result
                    && server.actions
                {
                    pending.insert(id, request.click);
                }
                let _ = request.reply.send(result.map(|_| ()));
            }
            signal = server.signals.next() => match signal {
                Some(Ok(message)) => on_signal(&message, &mut pending),
                Some(Err(_)) => {}
                None => return,
            },
        }
    }
}

fn on_signal(message: &Message, pending: &mut Pending<u32>) {
    let header = message.header();
    match header.member().map(|member| member.as_str()) {
        Some("ActionInvoked") => {
            if let Ok((id, action)) = message.body().deserialize::<(u32, String)>()
                && action == "default"
                && let Some(click) = pending.take(&id)
            {
                click();
            }
        }
        Some("NotificationClosed") => {
            if let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() {
                pending.take(&id);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc as std_mpsc;

    use ashpd::zbus::zvariant::OwnedValue;

    use super::*;

    /// A stand-in notification server on the real session bus: it accepts one
    /// toast as id 7 and reports a click on it.
    async fn serve(seen: std_mpsc::SyncSender<(String, String, Vec<String>)>) {
        let connection = Connection::session().await.unwrap();
        connection.request_name(SERVICE).await.unwrap();
        let mut messages = MessageStream::from(&connection);
        while let Some(Ok(call)) = messages.next().await {
            let header = call.header();
            if header.message_type() != Type::MethodCall {
                continue;
            }
            match header.member().map(|member| member.as_str()) {
                Some("GetCapabilities") => {
                    connection
                        .reply(&header, &(vec!["actions", "body", "body-markup"],))
                        .await
                        .unwrap();
                }
                Some("Notify") => {
                    type Notify = (
                        String,
                        u32,
                        String,
                        String,
                        String,
                        Vec<String>,
                        HashMap<String, OwnedValue>,
                        i32,
                    );
                    let (_app, _replaces, _icon, title, body, actions, _hints, _timeout): Notify =
                        call.body().deserialize().unwrap();
                    connection.reply(&header, &7u32).await.unwrap();
                    seen.send((title, body, actions)).unwrap();
                    connection
                        .emit_signal(
                            None::<&str>,
                            PATH,
                            SERVICE,
                            "ActionInvoked",
                            &(7u32, "default"),
                        )
                        .await
                        .unwrap();
                }
                _ => {}
            }
        }
    }

    /// Run under `dbus-run-session -- cargo test -p fastcord-platform
    /// notify::linux -- --ignored`: a real session bus, no desktop needed.
    #[test]
    #[ignore = "needs a session D-Bus without a notification server"]
    fn a_toast_reaches_the_server_and_its_click_comes_back() {
        let (seen, toast) = std_mpsc::sync_channel(1);
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(serve(seen));
        });
        std::thread::sleep(Duration::from_millis(500));
        let (clicked, click) = std_mpsc::sync_channel(1);
        assert_eq!(
            super::super::show("Ann", "a <b> & c", move || {
                clicked.send(()).unwrap();
            }),
            Ok(())
        );
        let (title, body, actions) = toast.recv_timeout(ANSWER).unwrap();
        assert_eq!(title, "Ann");
        assert_eq!(body, "a &lt;b&gt; &amp; c", "escaped for a markup server");
        assert_eq!(actions, ["default", "Open"]);
        click.recv_timeout(ANSWER).expect("the click ran");
    }
}
