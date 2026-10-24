//! The Linux tray icon: a freedesktop StatusNotifierItem with a `dbusmenu`
//! menu on the session bus, which KDE Plasma, Xfce, Cinnamon, LXQt, waybar and
//! GNOME with the AppIndicator extension show. Wayland has no other tray.
//!
//! One thread owns a current-thread runtime and the bus connection; the two
//! objects answer the host's calls and the thread otherwise waits for a state
//! change or the watcher restarting, so nothing polls. Without a
//! `org.kde.StatusNotifierWatcher` there is no host and [`TrayError::NoHost`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread::JoinHandle;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::fdo;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, interface};

use super::{Command, TrayError, ico_entry, menu, state, tooltip};

type Sink = Box<dyn FnMut(Command) + Send>;
type SharedSink = Arc<Mutex<Sink>>;
/// One ARGB32 (network byte order) image: width, height, pixels.
type Pixmap = (i32, i32, Vec<u8>);

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const ICON: &[u8] = include_bytes!("../../../../assets/icons/app/fastcord.ico");
/// Icon sizes offered to the host, which picks one for its panel.
const SIZES: [u8; 3] = [24, 32, 48];

/// Wakes the icon thread after `set_state`; `None` while no icon runs.
static WAKE: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);
/// The menu layout's revision, raised on every state change.
static REVISION: AtomicU32 = AtomicU32::new(1);

pub struct Handle {
    wake: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

pub fn refresh() {
    if let Ok(wake) = WAKE.lock()
        && let Some(wake) = wake.as_ref()
    {
        // A full queue already holds a wake-up; the thread reads the latest state.
        let _ = wake.try_send(());
    }
}

pub fn start(sink: Sink) -> Result<Handle, TrayError> {
    let (wake, woken) = mpsc::channel(1);
    let (ready, started) = std_mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("fastcord-tray".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = ready.send(Err(TrayError::Failed));
                return;
            };
            runtime.block_on(run(sink, woken, ready));
        })
        .map_err(|_| TrayError::Failed)?;
    match started.recv() {
        Ok(Ok(())) => {
            if let Ok(mut slot) = WAKE.lock() {
                *slot = Some(wake.clone());
            }
            Ok(Handle {
                wake: Some(wake),
                thread: Some(thread),
            })
        }
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(TrayError::Failed)
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Closing every sender ends the thread, which drops the connection: the
        // bus releases the name and the host removes the icon.
        if let Ok(mut slot) = WAKE.lock() {
            *slot = None;
        }
        self.wake = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn run(
    sink: Sink,
    mut woken: mpsc::Receiver<()>,
    ready: std_mpsc::SyncSender<Result<(), TrayError>>,
) {
    let connection = match serve(Arc::new(Mutex::new(sink))).await {
        Ok(connection) => connection,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let Ok(dbus) = fdo::DBusProxy::new(&connection).await else {
        let _ = ready.send(Err(TrayError::Failed));
        return;
    };
    // Subscribed before registering, so a watcher restart is never missed.
    let Ok(mut owners) = dbus
        .receive_name_owner_changed_with_args(&[(0, WATCHER)])
        .await
    else {
        let _ = ready.send(Err(TrayError::Failed));
        return;
    };
    if let Err(error) = register(&connection).await {
        let _ = ready.send(Err(error));
        return;
    }
    let _ = ready.send(Ok(()));
    loop {
        tokio::select! {
            wake = woken.recv() => {
                if wake.is_none() {
                    return;
                }
                changed(&connection).await;
            }
            owner = owners.next() => {
                let Some(owner) = owner else { return };
                // A new watcher (the panel restarted) knows nothing of the icon.
                if owner.args().is_ok_and(|args| args.new_owner().is_some()) {
                    let _ = register(&connection).await;
                }
            }
        }
    }
}

/// Connects and publishes the item and its menu under the conventional name.
async fn serve(sink: SharedSink) -> Result<Connection, TrayError> {
    let connection = Connection::session().await.map_err(|_| TrayError::NoHost)?;
    let server = connection.object_server();
    let item = Item {
        sink: Arc::clone(&sink),
        icon: pixmaps(),
    };
    server
        .at(ITEM_PATH, item)
        .await
        .map_err(|_| TrayError::Failed)?;
    server
        .at(MENU_PATH, Menu { sink })
        .await
        .map_err(|_| TrayError::Failed)?;
    connection
        .request_name(service())
        .await
        .map_err(|_| TrayError::Failed)?;
    Ok(connection)
}

/// The conventional well-known name of this process's item.
fn service() -> String {
    format!("org.kde.StatusNotifierItem-{}-1", std::process::id())
}

async fn register(connection: &Connection) -> Result<(), TrayError> {
    connection
        .call_method(
            Some(WATCHER),
            WATCHER_PATH,
            Some(WATCHER),
            "RegisterStatusNotifierItem",
            &(service(),),
        )
        .await
        .map(drop)
        .map_err(|_| TrayError::NoHost)
}

/// Tells the host the tooltip and the menu changed.
async fn changed(connection: &Connection) {
    let revision = REVISION.fetch_add(1, Ordering::Relaxed) + 1;
    let server = connection.object_server();
    if let Ok(item) = server.interface::<_, Item>(ITEM_PATH).await {
        let _ = Item::new_tool_tip(item.signal_emitter()).await;
    }
    if let Ok(menu) = server.interface::<_, Menu>(MENU_PATH).await {
        let _ = Menu::layout_updated(menu.signal_emitter(), revision, 0).await;
    }
}

fn send(sink: &SharedSink, command: Command) {
    if let Ok(mut sink) = sink.lock() {
        sink(command);
    }
}

/// The committed icon's PNG entries as ARGB32 pixmaps.
fn pixmaps() -> Vec<Pixmap> {
    SIZES
        .into_iter()
        .filter_map(|size| ico_entry(ICON, size).and_then(argb))
        .collect()
}

/// An 8-bit RGBA PNG as one ARGB32 pixmap; `None` for anything else.
fn argb(png_bytes: &[u8]) -> Option<Pixmap> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes))
        .read_info()
        .ok()?;
    let mut rgba = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut rgba).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let pixels = rgba[..info.buffer_size()]
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, a]| [a, r, g, b])
        .collect();
    Some((
        i32::try_from(info.width).ok()?,
        i32::try_from(info.height).ok()?,
        pixels,
    ))
}

struct Item {
    sink: SharedSink,
    icon: Vec<Pixmap>,
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    fn activate(&self, _x: i32, _y: i32) {
        send(&self.sink, Command::Open);
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        send(&self.sink, Command::Open);
    }

    /// Hosts show the `Menu` object themselves; nothing to draw here.
    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(property)]
    fn category(&self) -> &str {
        "Communications"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "fastcord"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "fastcord"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    /// Empty: the host uses `IconPixmap`.
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<Pixmap> {
        self.icon.clone()
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<Pixmap>, String, String) {
        (String::new(), Vec::new(), tooltip(state()), String::new())
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(MENU_PATH).expect("constant object path")
    }

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// `com.canonical.dbusmenu`: the root (id 0) and one entry per [`menu`] item,
/// with the item's [`Command`] id.
struct Menu {
    sink: SharedSink,
}

/// One layout node: id, properties, children (each a variant of a node).
type Node = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

fn owned(value: Value<'_>) -> OwnedValue {
    value
        .try_to_owned()
        .expect("menu values hold no file descriptors")
}

fn properties(id: i32) -> HashMap<String, OwnedValue> {
    let mut properties = HashMap::new();
    if id == 0 {
        properties.insert("children-display".to_owned(), owned("submenu".into()));
    } else if let Some((_, label)) = menu(state())
        .into_iter()
        .find(|(command, _)| command.id() as i32 == id)
    {
        properties.insert("label".to_owned(), owned(label.into()));
    }
    properties
}

/// The layout under `parent`: the root with its entries, or a leaf entry.
fn layout(parent: i32) -> Node {
    if parent != 0 {
        return (parent, properties(parent), Vec::new());
    }
    let children = menu(state())
        .into_iter()
        .map(|(command, _)| {
            let id = command.id() as i32;
            owned(Value::from((id, properties(id), Vec::<Value<'_>>::new())))
        })
        .collect();
    (0, properties(0), children)
}

impl Menu {
    fn event_one(&self, id: i32, event: &str) {
        if event == "clicked"
            && let Some(command) = usize::try_from(id).ok().and_then(Command::from_id)
        {
            send(&self.sink, command);
        }
    }
}

#[interface(name = "com.canonical.dbusmenu")]
impl Menu {
    fn get_layout(&self, parent_id: i32, _depth: i32, _names: Vec<String>) -> (u32, Node) {
        (REVISION.load(Ordering::Relaxed), layout(parent_id))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        ids.into_iter().map(|id| (id, properties(id))).collect()
    }

    fn get_property(&self, id: i32, name: String) -> fdo::Result<OwnedValue> {
        properties(id)
            .remove(&name)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("no property {name}")))
    }

    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        self.event_one(id, &event_id);
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, event, _, _) in events {
            self.event_one(id, &event);
        }
        Vec::new()
    }

    /// The layout never changes on opening; it is sent on state changes.
    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(signal)]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_becomes_argb_pixmaps_of_every_offered_size() {
        let icons = pixmaps();
        let sizes: Vec<_> = icons.iter().map(|(w, h, _)| (*w, *h)).collect();
        assert_eq!(sizes, [(24, 24), (32, 32), (48, 48)]);
        for (w, h, pixels) in &icons {
            assert_eq!(pixels.len(), (w * h * 4) as usize);
        }
        // The corner is transparent and the centre opaque in the committed mark.
        let (_, _, pixels) = &icons[1];
        assert_eq!(pixels[0], 0, "alpha first");
        assert_eq!(pixels[(16 * 32 + 16) * 4], 255);
    }

    #[test]
    fn the_menu_layout_lists_the_state_s_entries_under_the_root() {
        let (id, root, children) = layout(0);
        assert_eq!(id, 0);
        assert!(root.contains_key("children-display"));
        let expected = menu(state());
        assert_eq!(children.len(), expected.len());
        let (leaf, props, nested) = layout(Command::Quit.id() as i32);
        assert_eq!(leaf, Command::Quit.id() as i32);
        assert_eq!(
            props
                .get("label")
                .map(|v| String::try_from(v.clone()).unwrap()),
            Some("Quit".to_owned())
        );
        assert!(nested.is_empty());
    }

    /// Against a stand-in watcher on a real session bus (run under
    /// `dbus-run-session`): the icon registers, the host reads its properties
    /// and menu, and a menu click comes back as a command.
    #[test]
    #[ignore = "needs a D-Bus session bus"]
    fn the_icon_registers_with_a_watcher_and_its_menu_clicks_come_back() {
        // Multi-threaded, so the stand-in host answers while the test waits.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (registered_tx, registered) = std_mpsc::channel();
        let host = runtime.block_on(async {
            let connection = Connection::session().await.unwrap();
            connection
                .object_server()
                .at(
                    WATCHER_PATH,
                    StandInWatcher {
                        seen: Mutex::new(registered_tx),
                    },
                )
                .await
                .unwrap();
            connection.request_name(WATCHER).await.unwrap();
            connection
        });
        let (clicks_tx, clicks) = std_mpsc::channel();
        let tray = start(Box::new(move |command| {
            let _ = clicks_tx.send(command);
        }))
        .expect("the tray starts once a watcher exists");
        let wait = std::time::Duration::from_secs(5);
        let service = registered.recv_timeout(wait).unwrap();
        runtime.block_on(async {
            let reply = host
                .call_method(
                    Some(service.as_str()),
                    ITEM_PATH,
                    Some("org.freedesktop.DBus.Properties"),
                    "Get",
                    &("org.kde.StatusNotifierItem", "Category"),
                )
                .await
                .unwrap();
            let category: OwnedValue = reply.body().deserialize().unwrap();
            assert_eq!(String::try_from(category).unwrap(), "Communications");
            let reply = host
                .call_method(
                    Some(service.as_str()),
                    MENU_PATH,
                    Some("com.canonical.dbusmenu"),
                    "GetLayout",
                    &(0i32, -1i32, Vec::<String>::new()),
                )
                .await
                .unwrap();
            let (_, (root, _, children)): (u32, Node) = reply.body().deserialize().unwrap();
            assert_eq!(root, 0);
            assert_eq!(children.len(), menu(state()).len());
            host.call_method(
                Some(service.as_str()),
                MENU_PATH,
                Some("com.canonical.dbusmenu"),
                "Event",
                &(Command::Quit.id() as i32, "clicked", Value::from(""), 0u32),
            )
            .await
            .unwrap();
        });
        assert_eq!(clicks.recv_timeout(wait), Ok(Command::Quit));
        drop(tray);
    }

    struct StandInWatcher {
        seen: Mutex<std_mpsc::Sender<String>>,
    }

    #[interface(name = "org.kde.StatusNotifierWatcher")]
    impl StandInWatcher {
        fn register_status_notifier_item(&self, service: String) {
            let _ = self.seen.lock().unwrap().send(service);
        }
    }
}
