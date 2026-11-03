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

use super::{Action, NotifyError, OnClick, Pending, escape_markup};

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
    open_label: String,
    mark_read_label: String,
    click: OnClick,
    reply: std_mpsc::SyncSender<Result<(), NotifyError>>,
}

static WORKER: OnceLock<Option<mpsc::Sender<Request>>> = OnceLock::new();

pub(super) fn show(
    title: &str,
    body: &str,
    open_label: &str,
    mark_read_label: &str,
    click: OnClick,
) -> Result<(), NotifyError> {
    let worker = WORKER
        .get_or_init(start)
        .as_ref()
        .ok_or(NotifyError::Failed)?;
    let (reply, answer) = std_mpsc::sync_channel(1);
    worker
        .try_send(Request {
            title: title.to_owned(),
            body: body.to_owned(),
            open_label: open_label.to_owned(),
            mark_read_label: mark_read_label.to_owned(),
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
    owner: String,
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
        Ok(Self {
            connection,
            signals,
            actions: false,
            markup: false,
            owner: String::new(),
        })
    }

    /// Resolve the current authority only when a toast is requested. A daemon
    /// restart must neither strand future toasts nor reuse old callback IDs.
    async fn refresh(&mut self) -> Result<bool, NotifyError> {
        let reply = self
            .connection
            .call_method(Some(SERVICE), PATH, Some(SERVICE), "GetCapabilities", &())
            .await
            .map_err(|_| NotifyError::Unsupported)?;
        // A method-return serial is not proof of its sender. Authenticate the
        // authority with the bus daemon before trusting capability data.
        let authority = self
            .connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetNameOwner",
                &(SERVICE,),
            )
            .await
            .map_err(|_| NotifyError::Unsupported)?;
        require_sender(&authority, "org.freedesktop.DBus")?;
        let owner: String = authority
            .body()
            .deserialize()
            .map_err(|_| NotifyError::Failed)?;
        require_sender(&reply, &owner)?;
        let capabilities: Vec<String> = reply
            .body()
            .deserialize()
            .map_err(|_| NotifyError::Failed)?;
        let changed = self.owner != owner;
        self.owner = owner;
        self.actions = capabilities.iter().any(|c| c == "actions");
        self.markup = capabilities.iter().any(|c| c == "body-markup");
        Ok(changed)
    }

    async fn refresh_authority(&mut self, pending: &mut Pending<u32>) -> Result<(), NotifyError> {
        let refreshed = self.refresh().await;
        if refreshed != Ok(false) {
            *pending = Pending::new();
        }
        refreshed.map(|_| ())
    }

    async fn notify(
        &self,
        title: &str,
        body: &str,
        open_label: &str,
        mark_read_label: &str,
    ) -> Result<u32, NotifyError> {
        let body = if self.markup {
            escape_markup(body)
        } else {
            body.to_owned()
        };
        let actions: &[&str] = if self.actions {
            &["default", open_label, "mark-read", mark_read_label]
        } else {
            &[]
        };
        let hints: HashMap<&str, Value<'_>> = HashMap::new();
        let reply = self
            .connection
            .call_method(
                Some(self.owner.as_str()),
                PATH,
                Some(SERVICE),
                "Notify",
                &("fastcord", 0u32, "", title, body, actions, hints, -1i32),
            )
            .await
            .map_err(|_| NotifyError::Failed)?;
        require_sender(&reply, &self.owner)?;
        reply.body().deserialize().map_err(|_| NotifyError::Failed)
    }
}

fn require_sender(message: &Message, owner: &str) -> Result<(), NotifyError> {
    if message
        .header()
        .sender()
        .is_some_and(|sender| sender.as_str() == owner)
    {
        Ok(())
    } else {
        Err(NotifyError::Failed)
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
                if let Err(error) = server.refresh_authority(&mut pending).await {
                    let _ = request.reply.send(Err(error));
                    continue;
                }
                let result = server.notify(&request.title, &request.body, &request.open_label, &request.mark_read_label).await;
                if let Ok(id) = result
                    && server.actions
                {
                    pending.insert(id, request.click);
                }
                let _ = request.reply.send(result.map(|_| ()));
            }
            signal = server.signals.next() => match signal {
                Some(Ok(message)) => on_signal(&message, &server.owner, &mut pending),
                Some(Err(_)) => {}
                None => return,
            },
        }
    }
}

fn on_signal(message: &Message, owner: &str, pending: &mut Pending<u32>) {
    let header = message.header();
    if header.sender().map(|sender| sender.as_str()) != Some(owner) {
        return;
    }
    match header.member().map(|member| member.as_str()) {
        Some("ActionInvoked") => {
            if let Ok((id, action)) = message.body().deserialize::<(u32, String)>()
                && let Some(action) = Action::from_id(&action)
                && let Some(click) = pending.take(&id)
            {
                click(action);
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

    #[test]
    fn method_reply_serial_does_not_authenticate_service_or_bus_ownership() {
        let call = Message::method_call(PATH, "GetCapabilities")
            .unwrap()
            .build(&())
            .unwrap();
        let spoof = Message::method_return(&call.header())
            .unwrap()
            .sender(":1.99")
            .unwrap()
            .build(&(vec!["actions"],))
            .unwrap();
        assert_eq!(require_sender(&spoof, ":1.10"), Err(NotifyError::Failed));
        assert_eq!(
            require_sender(&spoof, "org.freedesktop.DBus"),
            Err(NotifyError::Failed)
        );
        let legitimate = Message::method_return(&call.header())
            .unwrap()
            .sender(":1.10")
            .unwrap()
            .build(&(vec!["actions"],))
            .unwrap();
        assert_eq!(require_sender(&legitimate, ":1.10"), Ok(()));
    }

    #[test]
    fn actions_require_the_pinned_server_and_are_consumed_once() {
        fn signal(sender: &str, member: &str, body: &(u32, &str)) -> Message {
            Message::signal(PATH, SERVICE, member)
                .unwrap()
                .sender(sender)
                .unwrap()
                .build(body)
                .unwrap()
        }
        let mut pending = Pending::new();
        let (sent, received) = std_mpsc::channel();
        pending.insert(
            7,
            Box::new(move |action| {
                sent.send(action).unwrap();
            }),
        );
        on_signal(
            &signal(":1.99", "ActionInvoked", &(7, "mark-read")),
            ":1.10",
            &mut pending,
        );
        on_signal(
            &signal(":1.10", "ActionInvoked", &(7, "unrecognized")),
            ":1.10",
            &mut pending,
        );
        on_signal(
            &signal(":1.10", "ActionInvoked", &(8, "mark-read")),
            ":1.10",
            &mut pending,
        );
        assert!(received.try_recv().is_err());
        on_signal(
            &signal(":1.10", "ActionInvoked", &(7, "mark-read")),
            ":1.10",
            &mut pending,
        );
        assert_eq!(received.try_recv().unwrap(), Action::MarkRead);
        on_signal(
            &signal(":1.10", "ActionInvoked", &(7, "default")),
            ":1.10",
            &mut pending,
        );
        assert!(received.try_recv().is_err());
        pending.insert(9, Box::new(|_| panic!("closed callback must not run")));
        let closed = Message::signal(PATH, SERVICE, "NotificationClosed")
            .unwrap()
            .sender(":1.10")
            .unwrap()
            .build(&(9u32, 2u32))
            .unwrap();
        on_signal(&closed, ":1.10", &mut pending);
        assert!(pending.take(&9).is_none());
    }

    #[tokio::test]
    #[ignore = "needs an isolated session D-Bus without a notification server"]
    async fn forged_method_returns_cannot_install_authority_or_register_notification_ids() {
        async fn reply_from(real: &Connection, replier: &Connection, member: &str, notify: bool) {
            let mut calls = MessageStream::from(real);
            while let Some(Ok(call)) = calls.next().await {
                if call
                    .header()
                    .member()
                    .is_some_and(|name| name.as_str() == member)
                {
                    // Claim even the reserved sender: the real bus rewrites it
                    // to this connection's unique name. Matching reply serial
                    // alone still makes zbus return this forged response.
                    let builder = Message::method_return(&call.header())
                        .unwrap()
                        .sender("org.freedesktop.DBus")
                        .unwrap();
                    let reply = if notify {
                        builder.build(&7u32).unwrap()
                    } else {
                        builder.build(&(vec!["actions"],)).unwrap()
                    };
                    replier.send(&reply).await.unwrap();
                    return;
                }
            }
        }
        let legitimate = Connection::session().await.unwrap();
        legitimate.request_name(SERVICE).await.unwrap();
        let attacker = Connection::session().await.unwrap();
        let mut server = Server::connect().await.unwrap();
        let mut pending = Pending::new();
        pending.insert(
            7,
            Box::new(|_| panic!("authority failure must clear pending callbacks")),
        );
        let (result, ()) = tokio::join!(
            server.refresh_authority(&mut pending),
            reply_from(&legitimate, &attacker, "GetCapabilities", false)
        );
        assert_eq!(result, Err(NotifyError::Failed));
        assert!(server.owner.is_empty());
        assert!(pending.take(&7).is_none());
        let (result, ()) = tokio::join!(
            server.refresh_authority(&mut pending),
            reply_from(&legitimate, &legitimate, "GetCapabilities", false)
        );
        result.unwrap();
        assert_eq!(server.owner, legitimate.unique_name().unwrap().as_str());
        let (result, ()) = tokio::join!(
            server.notify("QA", "body", "Open", "Mark read"),
            reply_from(&legitimate, &attacker, "Notify", true)
        );
        assert_eq!(
            result,
            Err(NotifyError::Failed),
            "forged IDs never become callbacks"
        );
        let (result, ()) = tokio::join!(
            server.notify("QA", "body", "Open", "Mark read"),
            reply_from(&legitimate, &legitimate, "Notify", true)
        );
        assert_eq!(result, Ok(7));
    }

    #[tokio::test]
    #[ignore = "needs an isolated session D-Bus without a notification server"]
    async fn daemon_replacement_refreshes_authority_and_drops_old_ids() {
        async fn answer_capability(connection: &Connection, replacement: Option<&Connection>) {
            let mut messages = MessageStream::from(connection);
            while let Some(Ok(call)) = messages.next().await {
                if call
                    .header()
                    .member()
                    .is_some_and(|member| member.as_str() == "GetCapabilities")
                {
                    if let Some(replacement) = replacement {
                        connection.release_name(SERVICE).await.unwrap();
                        replacement.request_name(SERVICE).await.unwrap();
                    }
                    connection
                        .reply(&call.header(), &(vec!["actions"],))
                        .await
                        .unwrap();
                    return;
                }
            }
        }
        let first = Connection::session().await.unwrap();
        first.request_name(SERVICE).await.unwrap();
        let mut server = Server::connect().await.unwrap();
        let mut pending = Pending::new();
        let (result, ()) = tokio::join!(
            server.refresh_authority(&mut pending),
            answer_capability(&first, None)
        );
        result.unwrap();
        let old_owner = server.owner.clone();
        pending.insert(
            7,
            Box::new(|_| panic!("old daemon's callback must be dropped")),
        );
        let second = Connection::session().await.unwrap();
        let (result, ()) = tokio::join!(
            server.refresh_authority(&mut pending),
            answer_capability(&first, Some(&second))
        );
        assert_eq!(
            result,
            Err(NotifyError::Failed),
            "owner changed between capability reply and authority lookup"
        );
        assert!(pending.take(&7).is_none());
        let (result, ()) = tokio::join!(
            server.refresh_authority(&mut pending),
            answer_capability(&second, None)
        );
        result.unwrap();
        assert_ne!(server.owner, old_owner);
        assert!(pending.take(&7).is_none());
        let (sent, received) = std_mpsc::channel();
        pending.insert(
            7,
            Box::new(move |action| {
                sent.send(action).unwrap();
            }),
        );
        for (sender, expected) in [(old_owner.as_str(), false), (server.owner.as_str(), true)] {
            let signal = Message::signal(PATH, SERVICE, "ActionInvoked")
                .unwrap()
                .sender(sender)
                .unwrap()
                .build(&(7u32, "mark-read"))
                .unwrap();
            on_signal(&signal, &server.owner, &mut pending);
            assert_eq!(
                received.try_recv().ok(),
                expected.then_some(Action::MarkRead)
            );
        }
    }

    /// A stand-in notification server on the real session bus: it accepts one
    /// toast as id 7 and reports a click on it.
    async fn serve(seen: std_mpsc::SyncSender<(String, String, Vec<String>)>) {
        let connection = Connection::session().await.unwrap();
        connection.request_name(SERVICE).await.unwrap();
        let mut messages = MessageStream::from(&connection);
        let mut shown = 0;
        while let Some(Ok(call)) = messages.next().await {
            let header = call.header();
            if header.message_type() != Type::MethodCall {
                continue;
            }
            match header.member().map(|member| member.as_str()) {
                Some("GetCapabilities") => {
                    let capabilities = if shown < 2 {
                        vec!["actions", "body", "body-markup"]
                    } else {
                        Vec::new()
                    };
                    connection.reply(&header, &(capabilities,)).await.unwrap();
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
                    let action = if shown == 0 { "default" } else { "mark-read" };
                    shown += 1;
                    connection
                        .emit_signal(
                            None::<&str>,
                            PATH,
                            SERVICE,
                            "ActionInvoked",
                            &(7u32, action),
                        )
                        .await
                        .unwrap();
                }
                _ => {}
            }
        }
    }

    /// Run under `dbus-run-session -- cargo test -p fastcord-platform
    /// notify::linux::tests::a_toast_reaches_the_server_and_its_click_comes_back
    /// -- --ignored --exact`: a real isolated session bus, no desktop needed.
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
        for expected in [Action::Open, Action::MarkRead] {
            let (clicked, click) = std_mpsc::sync_channel(1);
            assert_eq!(
                super::super::show("Ann", "a <b> & c", "Open", "Mark as read", move |action| {
                    clicked.send(action).unwrap();
                }),
                Ok(())
            );
            let (title, body, actions) = toast.recv_timeout(ANSWER).unwrap();
            assert_eq!(title, "Ann");
            assert_eq!(body, "a &lt;b&gt; &amp; c", "escaped for a markup server");
            assert_eq!(actions, ["default", "Open", "mark-read", "Mark as read"]);
            assert_eq!(
                click.recv_timeout(ANSWER).expect("the action ran"),
                expected
            );
        }
        let (sent, received) = std_mpsc::sync_channel(1);
        assert_eq!(
            super::super::show(
                "QA",
                "plain <body>",
                "Open",
                "Mark as read",
                move |action| {
                    let _ = sent.send(action);
                }
            ),
            Ok(())
        );
        let (_, body, actions) = toast.recv_timeout(ANSWER).unwrap();
        assert_eq!(body, "plain <body>");
        assert!(actions.is_empty(), "no action controls without capability");
        assert_eq!(
            received.recv_timeout(ANSWER),
            Err(std_mpsc::RecvTimeoutError::Disconnected),
            "unsupported actions retain no callback"
        );
    }
}
