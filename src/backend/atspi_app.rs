//! AT-SPI universal active-app tracker (Tier-1 baseline).
//!
//! A desktop-agnostic active-window/active-app provider built on the
//! accessibility bus (`atspi` crate, already a dependency for selection in
//! `atspi_sel.rs`). Unlike the KWin script bridge (KDE) and the GNOME Shell
//! Introspect bridge (GNOME), this works unprivileged on GNOME, wlroots
//! compositors (Sway/Hyprland/niri) and X11 — anywhere the AT-SPI registry is
//! running — so it is the universal fallback that keeps active-app identity from
//! ever going blank.
//!
//! ## Mechanism
//!
//! We subscribe to two AT-SPI event classes on the a11y bus and treat either as
//! a focus change:
//!   * `window:activate`  (`WindowEvents::Activate`) — a toplevel window became
//!     the active window. This is the most reliable "the user switched apps"
//!     signal and carries the window accessible directly (so we get the title
//!     cheaply from its `name`).
//!   * `object:state-changed:focused` (`ObjectEvents::StateChanged` with
//!     `state == Focused`, `enabled == 1`) — a widget gained keyboard focus.
//!     Catches focus moves that don't re-activate a toplevel (e.g. focus moving
//!     between widgets, or apps that don't emit window:activate). We resolve the
//!     widget's owning Application and its active window for the title.
//!
//! Both events implement `EventProperties`, giving us the **sender** (the
//! application's unique D-Bus name, e.g. `:1.42`) and the source object path.
//! From the sender we resolve a PID via the session bus
//! `org.freedesktop.DBus.GetConnectionUnixProcessID`, then read
//! `/proc/<pid>/comm` and `/proc/<pid>/exe` for a friendly `app_name` and a
//! best-effort `bundle_id` (exe basename) — mirroring how `kwin.rs` / `x11.rs`
//! fill `bundle_id` from WM_CLASS / exe. If the PID path fails we fall back to
//! the AT-SPI Application accessible's `name`.
//!
//! ## Threading / runtime
//!
//! `atspi` is async (tokio). The rest of the helper is blocking/threaded, so:
//!   * `start()` uses a short-lived current-thread runtime (like `atspi_sel.rs`)
//!     to connect and seed the initial focused app — this also doubles as the
//!     reachability check: if the a11y bus is down, `start()` returns `Err` and
//!     the caller falls back.
//!   * the focus watcher runs on a dedicated background thread with its own
//!     current-thread runtime, driving the atspi event stream for the tracker's
//!     lifetime.
//!   * `get_running_apps()` spins up a short-lived runtime per call.
//!
//! ## Reliability caveats (per toolkit)
//!
//! AT-SPI only sees apps that expose accessibility. GTK and Qt (with their
//! a11y bridges) report well. Many Electron apps, some terminals, and most
//! games expose nothing, so their fields may be empty — we degrade gracefully
//! and never panic. The shape mirrors `kwin::KwinTracker` / `gnome::GnomeTracker`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use atspi::events::object::ObjectEvents;
use atspi::events::window::WindowEvents;
use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::{Event, ObjectRef, Role, State};
use zbus::export::futures_util::stream::StreamExt;
use zbus::Connection;

use super::{active_app_payload, ActiveApp, EventSink, RunningApp};

/// Wall-clock budget for the one-shot seed / running-apps tree walks. The bus
/// round-trips are cheap; we just don't want a pathological provider to stall
/// startup or a `GetRunningApps` request.
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// Bound the focus resolution / running-apps walk so a misbehaving provider
/// can't make us walk forever (mirrors `atspi_sel.rs`).
const MAX_DEPTH: usize = 12;
const MAX_NODES: usize = 400;

/// Most recent widget that gained keyboard focus (not window activations).
/// Post-paste edit observation (`atspi_edit`) starts from this instead of
/// walking a possibly huge accessibility tree.
pub static LAST_FOCUSED_WIDGET: Mutex<Option<ObjectRef>> = Mutex::new(None);

/// State shared between the background focus-watcher thread and the dispatch
/// thread that reads `current()` / toggles focus detection. Mirrors
/// `gnome::Shared` field-for-field.
struct Shared {
    cache: Mutex<Option<ActiveApp>>,
    events: EventSink,
    /// Gates `AppInfoUpdate` emission (toggled by `SetFocusChangeDetectorState`).
    focus_active: AtomicBool,
    /// Dedup key of the last emitted app, so a focus event that doesn't change
    /// the focused app+title doesn't spam duplicate events.
    last_emitted: Mutex<Option<String>>,
    counter: AtomicU64,
    pid: u32,
}

impl Shared {
    /// Update the cache from a freshly-resolved focused app, and (if focus
    /// detection is on) emit an `AppInfoUpdate`.
    fn on_focus(&self, app: ActiveApp) {
        if let Ok(mut g) = self.cache.lock() {
            *g = Some(app.clone());
        }
        if self.focus_active.load(Ordering::Relaxed) {
            self.emit(&app);
        }
    }

    /// Emit an `AppInfoUpdate` request on fd 3 unless it duplicates the last one.
    fn emit(&self, app: &ActiveApp) {
        let key = format!("{}\u{1f}{}", app.bundle_id, app.window_title);
        if let Ok(mut last) = self.last_emitted.lock() {
            if last.as_deref() == Some(key.as_str()) {
                return;
            }
            *last = Some(key);
        }
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let uuid = format!("evt-{}-{}", self.pid, n);
        let env = crate::proto::request("AppInfoUpdate", active_app_payload(app), &uuid);
        let _ = self.events.send(env);
    }

    fn set_focus_detection(&self, active: bool) {
        self.focus_active.store(active, Ordering::Relaxed);
        if active {
            // Re-enabling must re-announce even an unchanged app (see
            // RecordingStarted in main.rs), so forget the dedup key first.
            if let Ok(mut last) = self.last_emitted.lock() {
                *last = None;
            }
            // Push the current focus immediately so a fresh subscriber gets state
            // without waiting for the next focus change.
            let current = self.cache.lock().ok().and_then(|g| g.clone());
            if let Some(app) = current {
                self.emit(&app);
            }
        } else if let Ok(mut last) = self.last_emitted.lock() {
            *last = None;
        }
    }
}

/// Owns the background focus-watcher thread. Dropping it signals the watcher to
/// stop at its next event/wakeup.
pub struct AtspiTracker {
    shared: Arc<Shared>,
    /// Set on drop to ask the watcher thread to exit.
    stop: Arc<AtomicBool>,
}

impl AtspiTracker {
    /// Connect to the a11y bus, seed the current focused app, and spawn a
    /// background watcher for focus changes.
    ///
    /// Returns `Err` (changing nothing observable) if the accessibility bus /
    /// registry is unreachable, so the caller can fall back — exactly like
    /// `KwinTracker::start` / `GnomeTracker::start`.
    pub fn start(events: EventSink) -> Result<AtspiTracker, String> {
        let pid = std::process::id();

        // One-shot runtime for the connectivity check + initial seed. If the bus
        // is down, this surfaces as an Err and we never half-initialize.
        let seed = {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("tokio runtime: {e}"))?;
            rt.block_on(async {
                match tokio::time::timeout(PROBE_TIMEOUT, seed_focused()).await {
                    Ok(res) => res,
                    Err(_) => Err("AT-SPI seed timed out".to_string()),
                }
            })?
        };

        let shared = Arc::new(Shared {
            cache: Mutex::new(seed),
            events,
            focus_active: AtomicBool::new(true),
            last_emitted: Mutex::new(None),
            counter: AtomicU64::new(0),
            pid,
        });

        let stop = Arc::new(AtomicBool::new(false));

        let watcher_shared = shared.clone();
        let watcher_stop = stop.clone();
        std::thread::Builder::new()
            .name("atspi-focus-watch".into())
            .spawn(move || {
                // Dedicated current-thread runtime drives the atspi event stream
                // for the tracker's whole lifetime.
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        log::warn!(
                            "atspi watcher: runtime build failed, focus events disabled: {e}"
                        );
                        return;
                    }
                };
                rt.block_on(watch_loop(&watcher_shared, &watcher_stop));
                log::debug!("atspi watcher: loop ended");
            })
            .map_err(|e| format!("spawn watcher: {e}"))?;

        log::info!("AT-SPI universal active-app tracker started");
        Ok(AtspiTracker { shared, stop })
    }

    /// Latest focused app, or None if nothing focused / not yet seen.
    pub fn current(&self) -> Option<ActiveApp> {
        self.shared.cache.lock().ok().and_then(|g| g.clone())
    }

    /// Enable/disable `AppInfoUpdate` focus events.
    pub fn set_focus_detection(&self, active: bool) {
        self.shared.set_focus_detection(active);
    }

    /// Enumerate the AT-SPI desktop's application children. Empty on error
    /// (best-effort), deduped by bundle_id.
    pub fn get_running_apps(&self) -> Vec<RunningApp> {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::debug!("get_running_apps: runtime build failed: {e}");
                return Vec::new();
            }
        };
        rt.block_on(async {
            match tokio::time::timeout(PROBE_TIMEOUT, enumerate_apps()).await {
                Ok(v) => v,
                Err(_) => {
                    log::debug!("get_running_apps: timed out");
                    Vec::new()
                }
            }
        })
    }
}

impl Drop for AtspiTracker {
    fn drop(&mut self) {
        // Ask the watcher to stop. It may stay blocked in the event stream until
        // the next event, at which point it checks `stop` and exits; the thread
        // is detached, so we don't join. No persistent state is installed on the
        // bus (we only add match rules on our own connection, which is dropped
        // with the runtime), so there's nothing else to tear down.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The accessibility hierarchy root: the registry's desktop object. All running
/// applications hang off it as children. (Same construction as `atspi_sel.rs`.)
async fn desktop_root(conn: &Connection) -> Result<AccessibleProxy<'static>, String> {
    AccessibleProxy::builder(conn)
        .destination("org.a11y.atspi.Registry")
        .map_err(|e| format!("root destination: {e}"))?
        .path("/org/a11y/atspi/accessible/root")
        .map_err(|e| format!("root path: {e}"))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| format!("root proxy: {e}"))
}

/// Open the a11y connection, then walk the tree for a window/widget reporting
/// `Active`/`Focused` to seed the cache at startup. `Ok(None)` means the bus is
/// reachable but nothing focused was found (not an error).
///
/// CRITICAL: we first set the session `org.a11y.Status.IsEnabled` property to
/// `true`. Toolkits (Qt, GTK) only register their accessible tree with the
/// AT-SPI registry and start emitting focus/window events once an assistive-tech
/// client has flipped this flag. Without it, the registry only lists the handful
/// of always-on a11y helper services and no focus events ever arrive — verified
/// live on KDE Plasma 6 Wayland (IsEnabled defaulted to false; GUI apps were
/// invisible until enabled). This is the standard AT client handshake.
async fn seed_focused() -> Result<Option<ActiveApp>, String> {
    // Announce ourselves as an assistive-tech client so apps expose a11y. Best
    // effort: a failure here isn't fatal (some apps may already be exposing).
    if let Err(e) = atspi::connection::set_session_accessibility(true).await {
        log::debug!("atspi: set_session_accessibility(true) failed: {e}");
    }

    let a11y = AccessibilityConnection::new()
        .await
        .map_err(|e| format!("a11y bus connect: {e}"))?;
    let conn = a11y.connection();

    let root = desktop_root(conn).await?;
    let apps = root.get_children().await.unwrap_or_default();

    // Drop desktop-shell / compositor "apps" (gnome-shell, kwin, plasmashell, …):
    // their stage permanently reports `Active`, so a naive walk returns the shell
    // instead of the user's focused window (observed on GNOME: gnome-shell at
    // index 0 shadowed the focused Calculator). The event-driven watcher is
    // unaffected — it resolves the source object of each window:activate directly.
    let mut user_apps = Vec::with_capacity(apps.len());
    for app in apps {
        let is_shell = match app.as_accessible_proxy(conn).await {
            Ok(p) => p.name().await.map(|n| is_shell_app(&n)).unwrap_or(false),
            Err(_) => false,
        };
        if is_shell {
            continue;
        }
        user_apps.push(app);
    }

    // Prefer a `Focused` window/widget (authoritative for keyboard focus); fall
    // back to an `Active` toplevel for toolkits that only set `Active`. Each pass
    // is a bounded walk over the user apps.
    for want in [State::Focused, State::Active] {
        let mut budget = MAX_NODES;
        for app in &user_apps {
            if let Some(found) = descend_for_state(conn, app, 0, &mut budget, want).await {
                return Ok(Some(found));
            }
            if budget == 0 {
                break;
            }
        }
    }
    Ok(None)
}

/// Desktop-environment shells / compositors expose an always-`Active` stage that
/// is not the user's focused app; skip them when seeding (matched on the AT-SPI
/// application name). The event-driven watcher doesn't need this — it keys off
/// the activated window's own source object.
fn is_shell_app(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "gnome-shell"
            | "plasmashell"
            | "kwin"
            | "kwin_wayland"
            | "kwin_x11"
            | "mutter"
            | "mutter-x11-frames"
            | "ksmserver"
    ) || n.contains("gnome-shell")
}

/// Depth-first search under `obj` for an object in state `want` (`Focused` or
/// `Active`), returning a fully-resolved `ActiveApp` for the owning application.
async fn descend_for_state(
    conn: &Connection,
    obj: &ObjectRef,
    depth: usize,
    budget: &mut usize,
    want: State,
) -> Option<ActiveApp> {
    if depth > MAX_DEPTH || *budget == 0 {
        return None;
    }
    *budget -= 1;

    let proxy = obj.as_accessible_proxy(conn).await.ok()?;

    if let Ok(states) = proxy.get_state().await {
        if states.contains(want) {
            // Resolve via the source object — this finds the owning application
            // and a window title.
            return Some(resolve_active_app(conn, obj).await);
        }
    }

    let children = proxy.get_children().await.ok()?;
    for child in &children {
        if *budget == 0 {
            break;
        }
        if let Some(found) = Box::pin(descend_for_state(conn, child, depth + 1, budget, want)).await
        {
            return Some(found);
        }
    }
    None
}

/// Background loop: register for focus/window-activate events and resolve each
/// into an `ActiveApp`, updating the shared cache. Runs until `stop` is set
/// (checked on every event).
async fn watch_loop(shared: &Arc<Shared>, stop: &Arc<AtomicBool>) {
    let a11y = match AccessibilityConnection::new().await {
        Ok(c) => c,
        Err(e) => {
            log::warn!("atspi watcher: a11y connect failed, focus events disabled: {e}");
            return;
        }
    };

    // Tell the registry + match the two event classes we care about.
    if let Err(e) = a11y.register_event::<WindowEvents>().await {
        log::debug!("atspi watcher: register WindowEvents failed: {e}");
    }
    if let Err(e) = a11y.register_event::<ObjectEvents>().await {
        log::debug!("atspi watcher: register ObjectEvents failed: {e}");
    }

    let conn = a11y.connection().clone();
    let mut events = std::pin::pin!(a11y.event_stream());

    loop {
        // Wake periodically even with no events so a dropped tracker exits
        // reasonably promptly instead of blocking on the stream forever.
        let next = match tokio::time::timeout(Duration::from_millis(500), events.next()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                log::debug!("atspi watcher: event stream ended");
                break;
            }
            Err(_) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
        };
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let ev = match next {
            Ok(ev) => ev,
            Err(e) => {
                log::debug!("atspi watcher: bad event: {e}");
                continue;
            }
        };

        // The source object: window for Activate, widget for state-changed.
        let source = match interesting_source(&ev) {
            Some(s) => s,
            None => continue,
        };
        if matches!(ev, Event::Object(_)) {
            if let Ok(mut last) = LAST_FOCUSED_WIDGET.lock() {
                *last = Some(source.clone());
            }
        }

        let app = resolve_active_app(&conn, &source).await;
        // Skip totally-empty resolutions (apps with no a11y at all) so we don't
        // overwrite a good cached app with a blank one.
        if app.app_name.is_empty() && app.bundle_id.is_empty() && app.window_title.is_empty() {
            continue;
        }
        shared.on_focus(app);
    }
}

/// Decide whether an event is a focus change we care about and, if so, return
/// the source accessible to resolve. We accept `window:activate` and
/// `object:state-changed:focused` (only when the focus was gained, enabled==1).
fn interesting_source(ev: &Event) -> Option<ObjectRef> {
    match ev {
        Event::Window(WindowEvents::Activate(e)) => Some(e.item.clone()),
        Event::Object(ObjectEvents::StateChanged(e)) => {
            if e.state == State::Focused && e.enabled == 1 {
                Some(e.item.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Resolve a source accessible (a window or a focused widget) into an
/// `ActiveApp`: owning application identity (via sender PID, then AT-SPI name)
/// plus the active window's title. All steps are best-effort and never panic.
async fn resolve_active_app(conn: &Connection, source: &ObjectRef) -> ActiveApp {
    // 1. App identity from the unique bus name (sender) -> PID -> /proc.
    let (mut app_name, mut bundle_id) = identity_from_sender(conn, source.name.as_str()).await;

    // 2. AT-SPI Application accessible: a friendly name fallback + a place to
    //    find the active window for the title.
    let app_ref = match source.as_accessible_proxy(conn).await {
        Ok(p) => p.get_application().await.ok(),
        Err(_) => None,
    };

    if app_name.is_empty() {
        if let Some(app_ref) = &app_ref {
            if let Ok(p) = app_ref.as_accessible_proxy(conn).await {
                if let Ok(n) = p.name().await {
                    let n = n.trim();
                    if !n.is_empty() {
                        app_name = n.to_string();
                    }
                }
            }
        }
    }
    if bundle_id.is_empty() && !app_name.is_empty() {
        // Best-effort: use the lower-cased app name as a synthetic id so the
        // field isn't blank (mirrors kwin/x11 deriving id from WM_CLASS).
        bundle_id = app_name.to_ascii_lowercase();
    }

    // 3. Window title.
    let window_title = window_title_for(conn, source, app_ref.as_ref()).await;

    ActiveApp {
        app_name,
        bundle_id,
        window_title,
        url: String::new(),
    }
}

/// Resolve `(app_name, bundle_id)` from a unique bus name by asking the session
/// bus for the owning PID, then reading `/proc/<pid>/comm` (friendly name) and
/// `/proc/<pid>/exe` basename (best-effort bundle_id). Returns empties on any
/// failure (e.g. sandboxed apps where the PID isn't resolvable).
async fn identity_from_sender(conn: &Connection, sender: &str) -> (String, String) {
    // The a11y bus connection is NOT the session bus; PID resolution must go
    // through the session bus's org.freedesktop.DBus. But the a11y bus also
    // implements GetConnectionUnixProcessID for names on *its* bus, and `sender`
    // here is a name on the a11y bus, so we query the a11y bus directly.
    let pid = match dbus_pid_for(conn, sender).await {
        Some(p) => p,
        None => return (String::new(), String::new()),
    };
    if pid == 0 {
        return (String::new(), String::new());
    }

    let mut app_name = String::new();
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        let comm = comm.trim();
        if !comm.is_empty() {
            app_name = comm.to_string();
        }
    }

    let mut bundle_id = String::new();
    if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        if let Some(base) = exe.file_name().and_then(|s| s.to_str()) {
            if !base.is_empty() {
                bundle_id = base.to_string();
            }
        }
    }
    if app_name.is_empty() {
        app_name = bundle_id.clone();
    }

    (app_name, bundle_id)
}

/// Ask `org.freedesktop.DBus.GetConnectionUnixProcessID` on the given
/// connection's bus for the PID owning `name`.
async fn dbus_pid_for(conn: &Connection, name: &str) -> Option<u32> {
    let reply = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(name,),
        )
        .await
        .ok()?;
    reply.body().deserialize::<u32>().ok()
}

/// Find a window title for the focused app. If `source` is itself a toplevel
/// (Frame/Window), use its `name`; otherwise look under the application for an
/// active/showing toplevel and use that one's `name`.
async fn window_title_for(
    conn: &Connection,
    source: &ObjectRef,
    app_ref: Option<&ObjectRef>,
) -> String {
    // If the source itself is a toplevel, its name is the title.
    if let Ok(p) = source.as_accessible_proxy(conn).await {
        if let Ok(role) = p.get_role().await {
            if matches!(role, Role::Frame | Role::Window | Role::Dialog) {
                if let Ok(n) = p.name().await {
                    let n = n.trim();
                    if !n.is_empty() {
                        return n.to_string();
                    }
                }
            }
        }
    }

    // Otherwise search the application's children for the active toplevel.
    if let Some(app_ref) = app_ref {
        if let Ok(app_proxy) = app_ref.as_accessible_proxy(conn).await {
            if let Ok(children) = app_proxy.get_children().await {
                // Prefer an Active window; fall back to the first named toplevel.
                let mut fallback = String::new();
                for child in &children {
                    let Ok(p) = child.as_accessible_proxy(conn).await else {
                        continue;
                    };
                    let role = p.get_role().await.ok();
                    if !matches!(
                        role,
                        Some(Role::Frame) | Some(Role::Window) | Some(Role::Dialog)
                    ) {
                        continue;
                    }
                    let name = p.name().await.unwrap_or_default();
                    let name = name.trim().to_string();
                    if name.is_empty() {
                        continue;
                    }
                    if let Ok(states) = p.get_state().await {
                        if states.contains(State::Active) {
                            return name;
                        }
                    }
                    if fallback.is_empty() {
                        fallback = name;
                    }
                }
                if !fallback.is_empty() {
                    return fallback;
                }
            }
        }
    }

    String::new()
}

/// Enumerate the desktop root's application children into `RunningApp`s, deduped
/// by bundle_id. Best-effort; returns whatever resolves within the budget.
async fn enumerate_apps() -> Vec<RunningApp> {
    let a11y = match AccessibilityConnection::new().await {
        Ok(c) => c,
        Err(e) => {
            log::debug!("enumerate_apps: a11y connect failed: {e}");
            return Vec::new();
        }
    };
    let conn = a11y.connection();

    let root = match desktop_root(conn).await {
        Ok(r) => r,
        Err(e) => {
            log::debug!("enumerate_apps: {e}");
            return Vec::new();
        }
    };
    let apps = root.get_children().await.unwrap_or_default();

    let mut out: Vec<RunningApp> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for app in &apps {
        // Friendly name from the Application accessible.
        let name = match app.as_accessible_proxy(conn).await {
            Ok(p) => p.name().await.unwrap_or_default().trim().to_string(),
            Err(_) => String::new(),
        };

        // Best-effort bundle_id from the sender PID's exe basename, else the name.
        let (proc_name, exe_id) = identity_from_sender(conn, app.name.as_str()).await;
        let mut bundle_id = exe_id;
        if bundle_id.is_empty() {
            bundle_id = if !name.is_empty() {
                name.to_ascii_lowercase()
            } else {
                proc_name.to_ascii_lowercase()
            };
        }

        let display_name = if !name.is_empty() {
            name
        } else if !proc_name.is_empty() {
            proc_name
        } else {
            bundle_id.clone()
        };

        if bundle_id.is_empty() && display_name.is_empty() {
            continue;
        }
        let dedup_key = if bundle_id.is_empty() {
            display_name.clone()
        } else {
            bundle_id.clone()
        };
        if !seen.insert(dedup_key) {
            continue;
        }
        out.push(RunningApp {
            bundle_id,
            name: display_name,
        });
    }

    out
}
