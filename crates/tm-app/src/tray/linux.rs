//! StatusNotifierItem tray icon for Linux desktops.
//!
//! The icon is a small D-Bus service, not a window: a panel registers itself as
//! `org.kde.StatusNotifierWatcher`, we serve `org.kde.StatusNotifierItem` on
//! `/StatusNotifierItem` and a `com.canonical.dbusmenu` menu on `/MenuBar`, and
//! call `RegisterStatusNotifierItem` once. That is the whole tray protocol on
//! this platform — no GTK, no libappindicator — so the cross-built binary needs
//! no system library beyond the session bus it already talks to.
//!
//! Threading follows the Windows tray: one dedicated thread owns the icon and
//! the connection it lives on, and it publishes `READY` only once a panel knows
//! about the icon, because a window hidden behind an icon that never appeared
//! cannot be brought back. That thread is also the only thread that talks to
//! the bus: the UI thread stores the advertised status in an atomic and wakes
//! the thread through a channel, so a frame never writes to a socket.
//!
//! Registration belongs to one watcher instance. A panel restart (plasmashell,
//! the AppIndicator extension reloading) starts a watcher that knows nothing
//! about items registered with the previous one: the icon disappears while the
//! connection stays healthy, so the thread re-checks which process owns the
//! watcher name, re-registers when it changed, and marks the icon failed (which
//! brings a hidden window back) when it cannot.
//!
//! `NewStatus` is emitted with `Connection::emit_signal` rather than through a
//! `#[zbus(signal)]` declaration. The declaration would only be reachable as an
//! `async fn` that needs an executor this crate does not have, and the emit has
//! to happen on the tray thread — so the signal is hand-built and this note is
//! the reason the introspection XML does not list it. No StatusNotifier host
//! reads this interface's introspection; they all watch the signal itself.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};

use zbus::zvariant::{self, ObjectPath, OwnedValue, Value};

use super::{
    MENU_ITEMS, TrayAction, action_for_menu_id, children_for_depth, event_is_click, menu_label,
    status_name,
};

/// Object path the spec fixes for the icon itself.
const SNI_PATH: &str = "/StatusNotifierItem";
/// Object path of the menu. Chosen here and advertised in `Menu`; a panel asks
/// this object for the entries itself when the user right-clicks.
const MENU_PATH: &str = "/MenuBar";
const SNI_INTERFACE: &str = "org.kde.StatusNotifierItem";
const WATCHER_SERVICE: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
const DBUS_SERVICE: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";

/// Icon-theme name of the application icon the Linux install ships; also what
/// the `ToolTip` advertises, so a panel that can resolve a themed icon does not
/// need the pixmap.
const APP_ICON_NAME: &str = "io.github.aufkrawall.Taskman";
/// `Id` the spec wants: a short, stable, lowercase name without spaces.
const ITEM_ID: &str = "taskman";
/// Category that makes a task manager a background/system service: a host puts
/// the icon next to the other always-on tools instead of the app launchers.
const CATEGORY: &str = "SystemServices";

/// dbusmenu protocol version this menu implements.
const MENU_VERSION: u32 = 3;
/// Label direction. The app ships left-to-right in every language it has, and
/// a wrong answer here only mis-aligns the accelerator column.
const TEXT_DIRECTION: &str = "ltr";
/// dbusmenu status: `normal` unless the menu is disabled or its service failed.
/// The entries are always offered, so it never changes.
const MENU_STATUS: &str = "normal";

/// Lifecycle of the icon, the same three states the Windows tray thread
/// publishes: only a `READY` icon may have the window hidden behind it.
const STARTING: u8 = 0;
const READY: u8 = 1;
const FAILED: u8 = 2;

/// How often the tray thread checks that the session bus and its watcher are
/// still there while it waits for the UI. A bus or a panel that goes away takes
/// the icon with it, and a window hidden behind a lost icon has to come back.
///
/// Five seconds, not half a minute: a restarted panel leaves the icon missing
/// until this tick notices, and while the window is hidden there is nothing
/// else to click. The check is one `GetNameOwner` round trip on the session
/// bus, so a short interval costs nothing.
const BUS_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// One `IconPixmap` entry: width, height, then ARGB32 network-order bytes.
type IconPixmap = (i32, i32, Vec<u8>);
/// `ToolTip`: icon name, pixmaps, title, subtitle.
type ToolTip = (String, Vec<IconPixmap>, String, String);
/// Properties of a menu entry, as the `a{sv}` every dbusmenu call exchanges.
type ItemProperties = HashMap<String, OwnedValue>;
/// One dbusmenu layout item: `(id, properties, children)`.
type LayoutItem = (i32, ItemProperties, Vec<OwnedValue>);

/// Revision of the menu layout. A host compares it against the layout it
/// already holds; the entries themselves never change, so it only moves when a
/// new icon serves a new menu object.
static REVISION: AtomicU32 = AtomicU32::new(1);

/// A message from the UI thread to the tray thread.
enum Wake {
    /// The advertised status changed; publish it.
    Status,
    /// The app is shutting down.
    Quit,
}

/// State the tray thread and the UI thread both need.
#[derive(Default)]
struct Shared {
    /// What the `Status` property advertises: `Active` while the icon can bring
    /// the window back, `Passive` when a panel should hide it. The UI thread
    /// writes it, the D-Bus getter reads it, and the tray thread is the one
    /// that announces a change on the bus.
    active: AtomicBool,
    /// Lifecycle of the icon; see [`STARTING`], [`READY`], [`FAILED`].
    state: AtomicU8,
}

/// The UI thread's handle on the tray icon.
///
/// The handle never touches the bus: the icon appears when the tray thread has
/// registered it, [`Self::is_ready`] is the only thing allowed to decide that
/// a window may be hidden, and even a status change is handed over as a
/// message instead of a socket write.
pub(crate) struct TrayShell {
    shared: Arc<Shared>,
    /// Wakes the tray thread. `None` once this handle shut down.
    wake: Option<Sender<Wake>>,
    /// Last value this handle applied.
    visible: Option<bool>,
    shutdown: bool,
}

impl TrayShell {
    /// Start the tray thread and remember what it should show first. A failed
    /// spawn means no icon, the same visible outcome as a host without a tray.
    pub(crate) fn new(initially_visible: bool) -> Option<Self> {
        let shared = Arc::new(Shared::default());
        shared.active.store(initially_visible, Ordering::SeqCst);
        shared.state.store(STARTING, Ordering::Release);
        let (wake, wake_rx) = std::sync::mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("tm-tray".into())
            .spawn(move || tray_thread_main(thread_shared, wake_rx));
        if spawned.is_err() {
            shared.state.store(FAILED, Ordering::Release);
            return None;
        }
        // A fresh icon serves a fresh menu object, so a host that kept the
        // previous revision has to see a different one.
        REVISION.fetch_add(1, Ordering::AcqRel);
        Some(Self {
            shared,
            wake: Some(wake),
            visible: Some(initially_visible),
            shutdown: false,
        })
    }

    /// The icon exists and can bring the window back.
    pub(crate) fn is_ready(&self) -> bool {
        !self.shutdown && self.shared.state.load(Ordering::Acquire) == READY
    }

    /// The icon could not be created (or its thread ended): nothing will ever
    /// appear in the panel.
    pub(crate) fn has_failed(&self) -> bool {
        self.shared.state.load(Ordering::Acquire) == FAILED
    }

    /// Advertise the icon as `Active` (the window can be restored from it) or
    /// `Passive` (a panel that hides passive items may drop it).
    ///
    /// The served property is the atomic below, so it is already correct for
    /// the next host that asks; the `NewStatus` announcement is the tray
    /// thread's job. The connection and the thread stay up either way, so
    /// flipping back does not have to wait for a new icon.
    pub(crate) fn set_visible(&mut self, visible: bool) {
        if self.shutdown || self.visible == Some(visible) {
            return;
        }
        self.visible = Some(visible);
        self.shared.active.store(visible, Ordering::SeqCst);
        self.wake_thread(Wake::Status);
    }

    /// The menu is drawn and themed by the StatusNotifier host, unlike the
    /// Win32 popup menu, so there is nothing to apply here. The method exists
    /// so the tray lifecycle in `main` stays one piece of code on both desktops.
    pub(crate) fn sync_menu_theme(&mut self, _theme: eframe::egui::Theme) {}

    /// End the tray thread, which drops the connection and with it the bus name
    /// and the icon, so a clean exit leaves no ghost entry behind.
    pub(crate) fn shutdown(&mut self) {
        if self.shutdown {
            return;
        }
        self.shutdown = true;
        self.visible = Some(false);
        self.shared.active.store(false, Ordering::SeqCst);
        // The queued `Quit` survives dropping the sender, and a thread that is
        // already gone needs no message at all.
        self.wake_thread(Wake::Quit);
        self.wake = None;
    }

    /// Hand one message to the tray thread.
    fn wake_thread(&self, wake: Wake) {
        if let Some(sender) = &self.wake {
            let _ = sender.send(wake);
        }
    }
}

/// Entry of the dedicated tray thread: serve the icon, keep it registered and
/// stop when the UI asks.
///
/// The connection is created here and never on the UI thread: a blocking
/// connect, a name request and the watcher calls are all bus round trips, and
/// the frame pipeline must not wait for the session bus. While this thread
/// waits, the connection's own executor thread dispatches the incoming D-Bus
/// calls, so waiting costs nothing.
fn tray_thread_main(shared: Arc<Shared>, wake: Receiver<Wake>) {
    let Some((connection, unique_name)) = serve_icon(&shared) else {
        shared.state.store(FAILED, Ordering::Release);
        return;
    };
    // Only now does the panel know the icon exists, and only now may the
    // window disappear into it.
    shared.state.store(READY, Ordering::Release);

    // The watcher this icon is registered with. A restarted panel owns the same
    // well-known name from a new unique name, and that new watcher has never
    // heard of this icon.
    let mut watcher = watcher_owner(&connection);
    // Whether the loss of the icon was already reported, so a panel that stays
    // away for a while does not repeat it (or re-focus the window) every tick.
    let mut loss_reported = false;
    loop {
        match wake.recv_timeout(BUS_CHECK_INTERVAL) {
            Ok(Wake::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Wake::Status) => {
                let status = status_name(shared.active.load(Ordering::Acquire));
                publish_status(&connection, status);
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if connection.is_closed() {
            tracing::warn!("tray icon lost its session bus connection");
            // The icon died with the bus, and a hidden window has no other way
            // back: the loop is about to end, so ask the UI for a frame now.
            // Without this, an invisible window only runs `logic` when someone
            // requests a repaint, and nobody will: `rescue_window_hidden_without_tray`
            // would never observe the failure.
            shared.state.store(FAILED, Ordering::Release);
            crate::signal_tray(crate::TRAY_ACTION_OPEN);
            break;
        }
        let current = watcher_owner(&connection);
        if current.is_some() && current == watcher {
            continue;
        }
        match register_with_watcher(&connection, &unique_name) {
            Ok(()) => {
                // A reactivated watcher starts with no items, so a successful
                // registration means the icon is back. Re-read the owner when
                // the check above could not name one: registering may have
                // started the watcher itself.
                watcher = current.or_else(|| watcher_owner(&connection));
                loss_reported = false;
                shared.state.store(READY, Ordering::Release);
            }
            Err(error) => {
                watcher = None;
                if loss_reported {
                    continue;
                }
                loss_reported = true;
                log_registration_failure(&error);
                // The icon is gone for good: a window hidden behind it cannot
                // be brought back, so say so and ask the UI for a frame now
                // instead of at the next sampler tick.
                tracing::warn!("the tray icon is gone; restoring a window hidden behind it");
                shared.state.store(FAILED, Ordering::Release);
                crate::signal_tray(crate::TRAY_ACTION_OPEN);
            }
        }
    }

    shared.state.store(FAILED, Ordering::Release);
    // Dropping the last connection closes the socket, which releases the bus
    // name and with it the icon.
    drop(connection);
}

/// Open the session bus, serve both objects on it and register the icon with
/// the panel's watcher. `None` means there is no icon, and the reason has been
/// logged once.
fn serve_icon(shared: &Arc<Shared>) -> Option<(zbus::blocking::Connection, String)> {
    let service = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    let builder = match zbus::blocking::connection::Builder::session() {
        Ok(builder) => builder,
        Err(error) => {
            tracing::warn!(%error, "cannot open a session bus connection for the tray icon");
            return None;
        }
    };
    let connection = builder
        .serve_at(
            SNI_PATH,
            StatusNotifierItem {
                shared: Arc::clone(shared),
            },
        )
        .and_then(|builder| builder.serve_at(MENU_PATH, TrayMenu))
        .and_then(|builder| builder.build());
    let connection = match connection {
        Ok(connection) => connection,
        Err(error) => {
            tracing::warn!(%error, "cannot serve the tray icon on the session bus");
            return None;
        }
    };
    if let Err(error) = connection.request_name(service.as_str()) {
        tracing::warn!(%error, service, "cannot own the tray icon's bus name");
        return None;
    }
    let Some(unique_name) = connection.unique_name().map(|name| name.to_string()) else {
        tracing::warn!("the tray icon's session bus connection has no unique name");
        return None;
    };
    if let Err(error) = register_with_watcher(&connection, &unique_name) {
        log_registration_failure(&error);
        return None;
    }
    Some((connection, unique_name))
}

/// Ask whichever watcher owns the bus name to show this icon.
///
/// There is deliberately no "is a watcher running?" pre-check: a watcher with a
/// D-Bus activation file is exactly what this call starts, and a missing one
/// comes back from the call itself as `ServiceUnknown`/`NameHasNoOwner`.
fn register_with_watcher(
    connection: &zbus::blocking::Connection,
    unique_name: &str,
) -> Result<(), zbus::Error> {
    // The unique name is what the watcher needs: it reads the icon off this
    // connection at the spec's fixed object path.
    connection
        .call_method(
            Some(WATCHER_SERVICE),
            WATCHER_PATH,
            Some(WATCHER_INTERFACE),
            "RegisterStatusNotifierItem",
            &(unique_name,),
        )
        .map(|_| ())
}

/// One line for the reason the icon will not appear.
fn log_registration_failure(error: &zbus::Error) {
    if is_missing_watcher(error) {
        tracing::warn!(
            "no org.kde.StatusNotifierWatcher on the session bus (GNOME needs the AppIndicator extension); \
             the tray icon is unavailable and the window stays visible"
        );
    } else {
        tracing::warn!(
            %error,
            "the StatusNotifierWatcher rejected the tray icon; the window stays visible"
        );
    }
}

/// Does this error mean "nothing on the bus owns the watcher name"?
fn is_missing_watcher(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
        ),
        _ => false,
    }
}

/// The unique bus name of the panel's StatusNotifierWatcher, or `None` when
/// nothing owns the well-known name right now.
fn watcher_owner(connection: &zbus::blocking::Connection) -> Option<String> {
    let reply = connection
        .call_method(
            Some(DBUS_SERVICE),
            DBUS_PATH,
            Some(DBUS_INTERFACE),
            "GetNameOwner",
            &(WATCHER_SERVICE,),
        )
        .ok()?;
    let body = reply.body();
    body.deserialize::<String>().ok()
}

/// Announce the status the UI thread asked for.
///
/// `NewStatus` is the signal every StatusNotifier host watches, and a panel
/// that hides passive items is driven by exactly this one; the generic
/// `PropertiesChanged` is deliberately not sent for the property.
fn publish_status(connection: &zbus::blocking::Connection, status: &'static str) {
    if let Err(error) =
        connection.emit_signal(None::<&str>, SNI_PATH, SNI_INTERFACE, "NewStatus", &status)
    {
        tracing::debug!(%error, "cannot publish the tray status");
    }
}

/// The `org.kde.StatusNotifierItem` object: one icon, its tooltip and what a
/// click on it means.
struct StatusNotifierItem {
    shared: Arc<Shared>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    /// Left click. `x`/`y` are the click position a panel supplies; this app
    /// only ever opens its window, so the position is ignored.
    fn activate(&self, _x: i32, _y: i32) {
        crate::signal_tray(TrayAction::Open.signal_code());
    }

    /// Middle click, which the spec defines as "the user wants this item's
    /// content": the same window a left click opens.
    fn secondary_activate(&self, _x: i32, _y: i32) {
        crate::signal_tray(TrayAction::Open.signal_code());
    }

    /// Right click. The menu is the `com.canonical.dbusmenu` object a panel
    /// asks for itself (see `TrayMenu`), so there is nothing to pop up here.
    fn context_menu(&self, _x: i32, _y: i32) {}

    /// Scroll wheel. An icon may react to scrolling; this one has nothing to
    /// scroll through.
    fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(property(emits_changed_signal = "const"))]
    fn category(&self) -> String {
        CATEGORY.to_owned()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn id(&self) -> String {
        ITEM_ID.to_owned()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn title(&self) -> String {
        tm_core::i18n::tr(tm_core::i18n::K::WindowTitle).to_owned()
    }

    /// `Active` while the icon can bring the window back, `Passive` when a panel
    /// that hides passive items may drop it. `NewStatus` announces the change.
    #[zbus(property(emits_changed_signal = "false"))]
    fn status(&self) -> String {
        status_name(self.shared.active.load(Ordering::Acquire)).to_owned()
    }

    /// Themed icon name the Linux install puts in `hicolor`.
    #[zbus(property(emits_changed_signal = "const"))]
    fn icon_name(&self) -> String {
        APP_ICON_NAME.to_owned()
    }

    /// The icon itself, for hosts that cannot resolve a themed name (and for
    /// the moment right after install, before an icon cache refresh).
    #[zbus(property(emits_changed_signal = "const"))]
    fn icon_pixmap(&self) -> Vec<IconPixmap> {
        item_pixmap()
    }

    /// Left click activates the item (`Activate` -> Open/restore the window)
    /// rather than popping up the context menu; right click still opens the
    /// `com.canonical.dbusmenu` menu.
    #[zbus(property(emits_changed_signal = "const"))]
    fn item_is_menu(&self) -> bool {
        false
    }

    /// Object path of the menu a panel calls for the context menu.
    #[zbus(property(emits_changed_signal = "const"))]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::from_static_str_unchecked(MENU_PATH)
    }

    /// The spec spells this property `ToolTip`, which is not what the Rust
    /// method name would be turned into.
    #[zbus(property(emits_changed_signal = "const"), name = "ToolTip")]
    fn tooltip(&self) -> ToolTip {
        (
            APP_ICON_NAME.to_owned(),
            item_pixmap(),
            tm_core::i18n::tr(tm_core::i18n::K::WindowTitle).to_owned(),
            String::new(),
        )
    }
}

/// The `com.canonical.dbusmenu` object behind `Menu`: the entries a panel shows
/// on right click, and the events it reports back.
struct TrayMenu;

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl TrayMenu {
    /// The menu below `parent_id`, with the entries in the order
    /// [`MENU_ITEMS`] fixes.
    ///
    /// `property_names` is the host's filter; answering with every property is
    /// allowed and keeps this object free of per-host branching.
    #[zbus(out_args("revision", "layout"))]
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, LayoutItem)> {
        let item = if parent_id == 0 {
            let children = if children_for_depth(recursion_depth) {
                menu_children()?
            } else {
                Vec::new()
            };
            (0, HashMap::new(), children)
        } else {
            let Some(action) = action_for_menu_id(parent_id) else {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "no menu item {parent_id}"
                )));
            };
            (parent_id, label_properties(action)?, Vec::new())
        };
        Ok((revision(), item))
    }

    /// The same properties for a set of ids, which is what a host asks for when
    /// it only needs to refresh a menu it already has. Ids this menu does not
    /// serve are left out, which the spec allows.
    #[zbus(out_args("properties"))]
    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<Vec<(i32, ItemProperties)>> {
        let mut properties = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(action) = action_for_menu_id(id) {
                properties.push((id, label_properties(action)?));
            }
        }
        Ok(properties)
    }

    /// One property of one entry.
    #[zbus(out_args("value"))]
    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let Some(action) = action_for_menu_id(id) else {
            return Err(zbus::fdo::Error::InvalidArgs(format!("no menu item {id}")));
        };
        label_properties(action)?.remove(&name).ok_or_else(|| {
            zbus::fdo::Error::InvalidArgs(format!("menu item {id} has no property `{name}`"))
        })
    }

    /// A host reporting that the user activated an entry.
    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_is_click(&event_id) {
            dispatch_menu_entry(id);
        }
    }

    /// The batched form of `Event`, which hosts use when several entries change
    /// state at once.
    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) {
        for (id, event_id, _data, _timestamp) in events {
            if event_is_click(&event_id) {
                dispatch_menu_entry(id);
            }
        }
    }

    /// Whether the menu has to be rebuilt before it is shown. The entries and
    /// their labels are fixed until the language changes, which happens through
    /// a full restart, so a host never has to wait.
    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    #[zbus(out_args("updates_needed", "id_errors"))]
    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn version(&self) -> u32 {
        MENU_VERSION
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn text_direction(&self) -> String {
        TEXT_DIRECTION.to_owned()
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn status(&self) -> String {
        MENU_STATUS.to_owned()
    }
}

/// Hand a menu entry's action to the UI thread through the same queue the
/// Windows menu signals use, so "Exit" runs the app's normal close path and
/// "Open" restores the window instead of starting a second one.
fn dispatch_menu_entry(id: i32) {
    if let Some(action) = action_for_menu_id(id) {
        crate::signal_tray(action.signal_code());
    }
}

/// One leaf entry per menu item, in the `av` shape a layout item carries its
/// children in.
fn menu_children() -> zbus::fdo::Result<Vec<OwnedValue>> {
    MENU_ITEMS
        .iter()
        .map(|(id, action)| menu_entry(*id, *action))
        .collect()
}

/// One leaf entry, converted to the variant a child list holds.
fn menu_entry(id: i32, action: TrayAction) -> zbus::fdo::Result<OwnedValue> {
    let item: LayoutItem = (id, label_properties(action)?, Vec::new());
    OwnedValue::try_from(Value::from(item)).map_err(dbus_failure)
}

/// The properties a layout item serves: the translated label plus the two flags
/// every host honours.
fn label_properties(action: TrayAction) -> zbus::fdo::Result<ItemProperties> {
    let mut properties = ItemProperties::with_capacity(3);
    properties.insert("label".to_owned(), owned_string(menu_label(action))?);
    properties.insert("enabled".to_owned(), OwnedValue::from(true));
    properties.insert("visible".to_owned(), OwnedValue::from(true));
    Ok(properties)
}

/// A variant holding a string, the shape an `a{sv}` entry needs.
fn owned_string(value: &str) -> zbus::fdo::Result<OwnedValue> {
    OwnedValue::try_from(Value::from(value)).map_err(dbus_failure)
}

/// The app icon as the spec wants it: one 64x64 pixmap in ARGB32 network order.
fn item_pixmap() -> Vec<IconPixmap> {
    let icon = crate::icon_data();
    vec![(
        icon.width as i32,
        icon.height as i32,
        super::argb32_network_order(&icon.rgba),
    )]
}

/// Current layout revision.
fn revision() -> u32 {
    REVISION.load(Ordering::Acquire)
}

/// A D-Bus error for a value that could not be built.
fn dbus_failure(error: zvariant::Error) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(error.to_string())
}
