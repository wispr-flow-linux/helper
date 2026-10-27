//! GNOME-Shell active-app + focus-events bridge for GNOME Wayland.
//!
//! This is the GNOME analog of the KDE/KWin bridge (`backend/kwin.rs`). Wayland
//! exposes no portable "what app owns the focused window" protocol, so each
//! compositor needs its own mechanism.
//!
//! ## Why an extension (and not `org.gnome.Shell.Introspect`)
//!
//! The earlier design used the built-in `org.gnome.Shell.Introspect` D-Bus API.
//! Live-tested on Fedora 43 / GNOME Shell 49.1 (Wayland), `Introspect.GetWindows`
//! returns **`org.freedesktop.DBus.Error.AccessDenied`** to a normal unsandboxed
//! caller — on modern GNOME it is gated behind the shell's `unsafe_mode`
//! developer toggle (OFF in normal sessions). So the bare Introspect API does
//! NOT work out of the box on GNOME 49.
//!
//! The robust path is the well-known "Window Calls" / "Focused Window D-Bus"
//! pattern: a tiny GNOME Shell **extension** that runs inside the privileged
//! gnome-shell process and exports its own D-Bus interface. The helper bundles
//! that extension (embedded in this binary via `include_str!`), installs it into
//! `~/.local/share/gnome-shell/extensions/<uuid>/` and enables it (disabling the
//! shell-wide version validation only on a shell its metadata does not list).
//! The extension exports:
//!
//!   * bus name : `org.wispr.flow.WindowBridge`
//!   * object   : `/org/wispr/flow/WindowBridge`
//!   * interface: `org.wispr.flow.WindowBridge`
//!       - `GetFocusedWindow() -> (s)`  JSON `{appId,title,pid,wmClass}`
//!       - `GetWindowList()    -> (s)`  JSON array of the same shape
//!       - `FocusChanged(s)`            signal carrying the focused-window JSON
//!
//! ## Relogin caveat (Wayland)
//!
//! GNOME Shell only **scans** the extensions directory at session start. A
//! freshly-installed extension on a running Wayland session is present-but-not-
//! loaded until the user logs out and back in (you cannot restart the Wayland
//! shell in place). So `start()` distinguishes two cases:
//!   * bridge D-Bus interface is live  -> seed cache + watch `FocusChanged`, Ok.
//!   * installed but not yet loaded     -> warn "log out and back in" + return
//!     Err, so the caller falls back to the Tier-1 AT-SPI / generic-Wayland path
//!     (exactly the behavior the old Introspect path produced on a denial).
//!
//! The integration shape mirrors `kwin.rs`: a `GnomeTracker` owns the connection,
//! caches the latest focused `ActiveApp`, exposes `current()` /
//! `get_running_apps()`, and — when focus detection is enabled — emits deduped
//! `AppInfoUpdate` requests on fd 3 via the `EventSink`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::{active_app_payload, ActiveApp, EventSink, RunningApp};

/// Extension uuid (directory name + identifier for `gnome-extensions`).
const EXT_UUID: &str = "wispr-flow-window-bridge@wispr.flow";
/// Bus name / object path / interface the extension exports from the shell.
const BRIDGE_DEST: &str = "org.wispr.flow.WindowBridge";
const BRIDGE_PATH: &str = "/org/wispr/flow/WindowBridge";
const BRIDGE_IFACE: &str = "org.wispr.flow.WindowBridge";

/// The bundled extension files, embedded so the helper stays a single static
/// binary. Installed verbatim into `~/.local/share/gnome-shell/extensions/<uuid>/`.
const EXT_METADATA: &str = include_str!("gnome_extension/metadata.json");
const EXT_EXTENSION_JS: &str = include_str!("gnome_extension/extension.js");

/// State shared between the signal-watcher thread (which re-emits on
/// `FocusChanged`) and the main dispatch loop (which reads `current()` /
/// toggles focus detection). Mirrors `kwin::Shared`.
struct Shared {
    cache: Mutex<Option<ActiveApp>>,
    events: EventSink,
    /// Gates `AppInfoUpdate` emission (toggled by `SetFocusChangeDetectorState`).
    focus_active: AtomicBool,
    /// Dedup key of the last emitted app, so a focus change that doesn't change
    /// the focused app+title doesn't spam duplicate events.
    last_emitted: Mutex<Option<String>>,
    counter: AtomicU64,
    pid: u32,
}

impl Shared {
    /// Update the cache from a freshly-observed focused app, and (if focus
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
            // without waiting for the next FocusChanged.
            let current = self.cache.lock().ok().and_then(|g| g.clone());
            if let Some(app) = current {
                self.emit(&app);
            }
        } else if let Ok(mut last) = self.last_emitted.lock() {
            *last = None;
        }
    }
}

/// Owns the zbus connection + the background signal-watcher thread. Dropping it
/// signals the watcher to stop.
pub struct GnomeTracker {
    shared: Arc<Shared>,
    conn: zbus::blocking::Connection,
    /// Set on drop to ask the watcher thread to exit at its next signal/wakeup.
    stop: Arc<AtomicBool>,
}

impl GnomeTracker {
    /// Start the bridge:
    ///   1. ensure the bundled extension is installed + enabled,
    ///   2. probe whether its D-Bus interface is actually live,
    ///   3. if live, seed the cache and spawn a `FocusChanged` watcher (Ok),
    ///      otherwise warn (relogin needed) and return Err so the caller falls
    ///      back to the Tier-1 AT-SPI / generic-Wayland path.
    pub fn start(events: EventSink) -> Result<GnomeTracker, String> {
        let pid = std::process::id();

        // (1) Install + enable. Best-effort: failures here only mean the bridge
        // won't come up (we still return Err below), they never half-init.
        if let Err(e) = ensure_installed() {
            log::warn!("gnome bridge: extension install failed: {e}");
        }

        let conn =
            zbus::blocking::Connection::session().map_err(|e| format!("session bus: {e}"))?;
        ensure_enabled(&conn);

        // (2) Probe: is the extension's D-Bus interface live in the shell?
        let proxy = make_proxy(&conn)?;
        let initial = match proxy.call::<_, _, (String,)>("GetFocusedWindow", &()) {
            Ok((json,)) => parse_focused(&json),
            Err(e) => {
                // Not live. On Wayland a freshly-installed extension only loads
                // after a logout/login (the shell scans extensions at session
                // start), so this is the expected first-run state. Warn clearly
                // and let the caller fall back.
                log::warn!(
                    "GNOME window bridge not active yet ({e}). The Wispr Flow GNOME \
                     window-detection extension is installed but GNOME loads extensions \
                     only at session start — please log out and back in to enable active-app \
                     detection. Falling back to limited detection for now."
                );
                return Err(format!("bridge not loaded (relogin required): {e}"));
            }
        };

        let shared = Arc::new(Shared {
            cache: Mutex::new(initial),
            events,
            focus_active: AtomicBool::new(true),
            last_emitted: Mutex::new(None),
            counter: AtomicU64::new(0),
            pid,
        });

        // Push the startup seed so Electron knows the focused app before the
        // first focus change (it only learns the app from these events).
        if let Some(app) = shared.cache.lock().ok().and_then(|g| g.clone()) {
            shared.emit(&app);
        }

        let stop = Arc::new(AtomicBool::new(false));

        // (3) Background watcher: block on the bridge's `FocusChanged` signal and
        // re-emit each focused-window JSON. zbus's blocking SignalIterator yields
        // Messages whose body is the `(s)` JSON string.
        let watcher_shared = shared.clone();
        let watcher_stop = stop.clone();
        let watcher_conn = conn.clone();
        std::thread::Builder::new()
            .name("gnome-window-bridge-watch".into())
            .spawn(move || {
                watch_loop(&watcher_conn, &watcher_shared, &watcher_stop);
            })
            .map_err(|e| format!("spawn watcher: {e}"))?;

        log::info!("GNOME Shell window-bridge extension active (focused-window detection live)");
        Ok(GnomeTracker { shared, conn, stop })
    }

    /// Latest focused app. Re-polls the bridge's `GetFocusedWindow` on demand
    /// (refreshing the cache) rather than trusting only the `FocusChanged`
    /// signal: mutter fires `notify::focus-window` only on an actual focus
    /// *change*, so a session whose focus never moves (and the extension's
    /// MRU fallback) would otherwise never update the cache past the startup
    /// seed. Falls back to the cached value if the synchronous poll fails.
    pub fn current(&self) -> Option<ActiveApp> {
        if let Ok(proxy) = make_proxy(&self.conn) {
            if let Ok((json,)) = proxy.call::<_, _, (String,)>("GetFocusedWindow", &()) {
                let fresh = parse_focused(&json);
                if fresh.is_some() {
                    if let Ok(mut g) = self.shared.cache.lock() {
                        g.clone_from(&fresh);
                    }
                    return fresh;
                }
            }
        }
        self.shared.cache.lock().ok().and_then(|g| g.clone())
    }

    /// Enable/disable `AppInfoUpdate` focus events.
    pub fn set_focus_detection(&self, active: bool) {
        if active {
            // Refresh the cache from the shell so the announcement is current.
            let _ = self.current();
        }
        self.shared.set_focus_detection(active);
    }

    /// Full running-app list via the bridge's `GetWindowList`, de-duped by appId
    /// (one entry per app, like a taskbar). Empty on error (best-effort).
    pub fn get_running_apps(&self) -> Vec<RunningApp> {
        let proxy = match make_proxy(&self.conn) {
            Ok(p) => p,
            Err(e) => {
                log::debug!("get_running_apps: proxy unavailable: {e}");
                return Vec::new();
            }
        };
        match proxy.call::<_, _, (String,)>("GetWindowList", &()) {
            Ok((json,)) => parse_window_list(&json),
            Err(e) => {
                log::debug!("GetWindowList failed: {e}");
                Vec::new()
            }
        }
    }
}

impl Drop for GnomeTracker {
    fn drop(&mut self) {
        // Ask the watcher to stop. It may stay blocked in the SignalIterator until
        // the next FocusChanged, at which point it checks `stop` and exits; the
        // thread is detached, so we don't join. We intentionally leave the
        // extension installed + enabled (re-running the helper reuses it; an
        // uninstall would force a relogin to re-enable next time).
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Background loop: block on `FocusChanged`, re-emit the carried JSON on each
/// fire. Falls back to a `GetFocusedWindow` re-poll if the signal body is empty.
fn watch_loop(conn: &zbus::blocking::Connection, shared: &Arc<Shared>, stop: &Arc<AtomicBool>) {
    let proxy = match make_proxy(conn) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("gnome watcher: proxy failed, focus events disabled: {e}");
            return;
        }
    };
    let signals = match proxy.receive_signal("FocusChanged") {
        Ok(s) => s,
        Err(e) => {
            log::warn!("gnome watcher: receive_signal FocusChanged failed: {e}");
            return;
        }
    };
    for msg in signals {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // The signal body is `(s)`: the focused-window JSON.
        match msg.body().deserialize::<(String,)>() {
            Ok((json,)) => {
                if let Some(app) = parse_focused(&json) {
                    shared.on_focus(app);
                } else {
                    log::debug!("gnome watcher: FocusChanged but no focused window");
                }
            }
            Err(e) => {
                log::debug!("gnome watcher: bad FocusChanged body ({e}); re-polling");
                if let Ok((json,)) = proxy.call::<_, _, (String,)>("GetFocusedWindow", &()) {
                    if let Some(app) = parse_focused(&json) {
                        shared.on_focus(app);
                    }
                }
            }
        }
    }
    log::debug!("gnome watcher: loop ended");
}

/// Build a blocking proxy onto the extension's bridge interface.
fn make_proxy(conn: &zbus::blocking::Connection) -> Result<zbus::blocking::Proxy<'static>, String> {
    zbus::blocking::Proxy::new(conn, BRIDGE_DEST, BRIDGE_PATH, BRIDGE_IFACE)
        .map_err(|e| format!("bridge proxy: {e}"))
}

// ---------------------------------------------------------------------------
// Install / enable
// ---------------------------------------------------------------------------

/// Path to the per-user extension directory `~/.local/share/gnome-shell/extensions/<uuid>/`.
fn extension_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })?;
    Some(base.join("gnome-shell/extensions").join(EXT_UUID))
}

/// Ensure the bundled extension is present on disk and matches the embedded
/// copy. Writes `metadata.json` + `extension.js` if missing or stale. Returns
/// `Ok(true)` if anything was (re)written, `Ok(false)` if already up to date.
fn ensure_installed() -> Result<bool, String> {
    let dir =
        extension_dir().ok_or_else(|| "cannot resolve extensions dir (no HOME)".to_string())?;
    let meta_path = dir.join("metadata.json");
    let js_path = dir.join("extension.js");

    let current_meta = std::fs::read_to_string(&meta_path).ok();
    let current_js = std::fs::read_to_string(&js_path).ok();
    let up_to_date = current_meta.as_deref() == Some(EXT_METADATA)
        && current_js.as_deref() == Some(EXT_EXTENSION_JS);
    if up_to_date {
        return Ok(false);
    }

    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    std::fs::write(&meta_path, EXT_METADATA).map_err(|e| format!("write metadata.json: {e}"))?;
    std::fs::write(&js_path, EXT_EXTENSION_JS).map_err(|e| format!("write extension.js: {e}"))?;
    log::info!("gnome bridge: installed extension to {}", dir.display());
    Ok(true)
}

/// Enable the extension, disabling GNOME's per-version extension validation
/// only when the running shell is missing from our `shell-version` list.
/// Best-effort: logs but never fails (the probe in `start()` is the real gate).
fn ensure_enabled(conn: &zbus::blocking::Connection) {
    // disable-extension-version-validation is a shell-wide setting: it lets
    // EVERY installed extension load on shell versions it never declared, and
    // nothing turns it back on. Our metadata already lists the supported
    // releases, so flip it only for a shell we can identify as newer than that
    // list. A shell we cannot identify (e.g. not on the bus yet at login) is
    // left alone: the next start retries, and a listed shell loads us anyway.
    let Some(shell) = shell_major_version(conn) else {
        log::debug!("gnome bridge: shell version unknown; leaving version validation alone");
        run_ok("gnome-extensions", &["enable", EXT_UUID]);
        return;
    };
    if needs_validation_override(&shell, EXT_METADATA) {
        log::warn!(
            "gnome bridge: GNOME Shell {shell} is not in the extension's shell-version list; \
             disabling extension version validation so the bridge can load"
        );
        run_ok(
            "gsettings",
            &[
                "set",
                "org.gnome.shell",
                "disable-extension-version-validation",
                "true",
            ],
        );
    }
    // Enable via the gnome-extensions CLI (which talks to the shell's
    // org.gnome.Shell.Extensions D-Bus service under the hood).
    run_ok("gnome-extensions", &["enable", EXT_UUID]);
}

/// The RUNNING shell's major version, from its `ShellVersion` D-Bus property.
/// Asking the live shell (not `gnome-shell --version` on disk) matters after an
/// upgrade without relogin, when the binary and the loaded shell differ; it
/// also spawns nothing.
fn shell_major_version(conn: &zbus::blocking::Connection) -> Option<String> {
    // One property read: no property cache (it would GetAll and add a match rule).
    let proxy: zbus::blocking::Proxy<'_> = zbus::blocking::proxy::Builder::new(conn)
        .destination("org.gnome.Shell")
        .ok()?
        .path("/org/gnome/Shell")
        .ok()?
        .interface("org.gnome.Shell")
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .ok()?;
    let version: String = proxy
        .get_property("ShellVersion")
        .map_err(|e| log::debug!("gnome bridge: ShellVersion unavailable: {e}"))
        .ok()?;
    parse_shell_major(&version)
}

/// "50.1" -> "50"; also takes pre-releases like "51.alpha".
fn parse_shell_major(version: &str) -> Option<String> {
    let major = version.trim().split('.').next()?;
    (!major.is_empty() && major.bytes().all(|b| b.is_ascii_digit())).then(|| major.to_string())
}

/// First GNOME Shell that loads ES-module extensions. extension.js is an ES
/// module, so no setting makes it load on 42-44 (main repo #50).
const FIRST_ESM_SHELL: u32 = 45;

/// Whether validation must be disabled for the bridge to load: true when the
/// shell's major version is absent from the metadata's `shell-version` list
/// (or the metadata has no usable list, which the shell would reject too),
/// unless the shell predates ES-module extensions and cannot load it anyway.
fn needs_validation_override(major: &str, metadata: &str) -> bool {
    if major.parse::<u32>().is_ok_and(|m| m < FIRST_ESM_SHELL) {
        return false;
    }
    let Ok(meta) = serde_json::from_str::<Value>(metadata) else {
        return true;
    };
    let listed = meta
        .get("shell-version")
        .and_then(Value::as_array)
        .is_some_and(|versions| versions.iter().any(|v| v.as_str() == Some(major)));
    !listed
}

/// Run a command, logging the outcome. Never panics; missing binaries are just
/// a debug log (the probe decides whether the bridge is actually usable).
fn run_ok(cmd: &str, args: &[&str]) {
    match std::process::Command::new(cmd).args(args).output() {
        Ok(out) if out.status.success() => {
            log::debug!("gnome bridge: `{cmd} {}` ok", args.join(" "));
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            log::debug!(
                "gnome bridge: `{cmd} {}` exited {}: {}",
                args.join(" "),
                out.status,
                err.trim()
            );
        }
        Err(e) => {
            log::debug!("gnome bridge: `{cmd}` not runnable: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// JSON parsing of the bridge's `(s)` returns
// ---------------------------------------------------------------------------

/// Parse a `GetFocusedWindow` / `FocusChanged` JSON string into `ActiveApp`.
/// Returns `None` when there is no focused window (empty appId+title+wmClass).
fn parse_focused(json: &str) -> Option<ActiveApp> {
    let v: Value = serde_json::from_str(json).ok()?;
    let app = window_to_app(&v);
    if app.bundle_id.is_empty() && app.window_title.is_empty() && app.app_name.is_empty() {
        None
    } else {
        Some(app)
    }
}

/// Parse a `GetWindowList` JSON array into a de-duped (by appId) `RunningApp` list.
fn parse_window_list(json: &str) -> Vec<RunningApp> {
    let v: Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out: Vec<RunningApp> = Vec::new();
    for w in arr {
        let app = window_to_app(w);
        if app.bundle_id.is_empty() {
            continue;
        }
        if out.iter().any(|a| a.bundle_id == app.bundle_id) {
            continue;
        }
        out.push(RunningApp {
            bundle_id: app.bundle_id,
            name: app.app_name,
        });
    }
    out
}

/// Map a `{appId,title,pid,wmClass}` JSON object to `ActiveApp`. Mirrors the
/// fields the old `window_to_app` filled: bundle_id from appId (falling back to
/// wmClass), window_title from title, app_name from /proc/<pid>/comm or the
/// appId's short form.
fn window_to_app(v: &Value) -> ActiveApp {
    let app_id = v
        .get("appId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let wm_class = v
        .get("wmClass")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let pid = v.get("pid").and_then(Value::as_u64).unwrap_or(0);

    // bundle_id: prefer the real .desktop appId, fall back to the WM class.
    let bundle_id = if app_id.is_empty() {
        wm_class.clone()
    } else {
        app_id.clone()
    };

    // app_name: prefer /proc/<pid>/comm (mirrors kwin.rs/x11), then the appId's
    // short form, then the WM class.
    let mut app_name = String::new();
    if pid != 0 {
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            let comm = comm.trim();
            if !comm.is_empty() {
                app_name = comm.to_string();
            }
        }
    }
    if app_name.is_empty() {
        app_name = if !app_id.is_empty() {
            short_name(&app_id)
        } else {
            wm_class.clone()
        };
    }

    ActiveApp {
        app_name,
        bundle_id,
        window_title: title,
        url: String::new(),
    }
}

/// "org.gnome.Nautilus.desktop" -> "Nautilus"; leaves names without dots untouched.
fn short_name(app_id: &str) -> String {
    let base = app_id.strip_suffix(".desktop").unwrap_or(app_id);
    base.rsplit('.').next().unwrap_or(base).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shell_major_version() {
        assert_eq!(parse_shell_major("50.1").as_deref(), Some("50"));
        assert_eq!(parse_shell_major("51.alpha").as_deref(), Some("51"));
        assert_eq!(parse_shell_major("45").as_deref(), Some("45"));
        assert_eq!(parse_shell_major(" 49.0 ").as_deref(), Some("49"));
        assert_eq!(parse_shell_major(""), None);
        assert_eq!(parse_shell_major(".1"), None);
        assert_eq!(parse_shell_major("GNOME Shell 50.1"), None);
    }

    /// The shell-wide validation switch is only touched when the bundled
    /// metadata cannot load on the running shell. 50 is the shell this was
    /// validated on, so the bundled list must keep it.
    #[test]
    fn overrides_validation_only_for_unlisted_shells() {
        assert!(!needs_validation_override("50", EXT_METADATA));
        assert!(!needs_validation_override("45", EXT_METADATA));
        assert!(needs_validation_override("99", EXT_METADATA));
    }

    /// Shells before 45 cannot import an ES-module extension, so flipping the
    /// shell-wide switch there would change the setting and still not load us.
    #[test]
    fn skips_validation_override_before_esm_shells() {
        assert!(!needs_validation_override("42", EXT_METADATA));
        assert!(!needs_validation_override("44", EXT_METADATA));
        assert!(!needs_validation_override("44", "not json"));
        assert!(needs_validation_override(
            "45",
            r#"{"shell-version":["46"]}"#
        ));
    }

    /// Metadata the shell could not match against (not JSON, no list, a
    /// non-array list, non-string entries) still needs the override.
    #[test]
    fn overrides_validation_without_a_usable_version_list() {
        assert!(needs_validation_override("50", "not json"));
        assert!(needs_validation_override("50", r#"{"uuid":"x"}"#));
        assert!(needs_validation_override("50", r#"{"shell-version":"50"}"#));
        assert!(needs_validation_override("50", r#"{"shell-version":[50]}"#));
        assert!(!needs_validation_override(
            "50",
            r#"{"shell-version":["49","50"]}"#
        ));
    }
}
