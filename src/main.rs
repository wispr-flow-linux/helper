//! Wispr Flow — Linux helper (clean-room).
//!
//! Speaks the helper IPC contract recovered in `docs/reference/ipc-contract.md`:
//!   * reads commands on **stdin (fd 0)**
//!   * writes all protocol output to **fd 3** (stdout/fd 1 is non-IPC; stderr/fd 2 is logging)
//!   * framing: escaped pretty-JSON envelopes joined by `'|'`
//!   * answers `IsReady` with `ACK` (the readiness + keepalive handshake)
//!
//! This is the skeleton: handshake + dispatch + the highest-value X11 commands
//! (PasteText, SimulateKeyPress, GetActiveAppInfo, GetRunningApps,
//! GetSelectedTextViaCopy, GetAccessibilityStatus). Everything else is ACK'd as a
//! safe no-op so the app stays healthy. See README.md for what's next.

mod backend;
mod capture;
mod keymap;
mod paste;
mod proto;

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::sync::mpsc;
use std::time::Instant;

use serde_json::{json, Value};

use backend::Backend;
use proto::{Incoming, Kind};

/// Handle to the fd-3 IPC return channel. Cheaply cloneable; every send is
/// queued to the single writer thread (see [`spawn_fd3_writer`]), so responses
/// and async helper-initiated events can be emitted from any thread without
/// interleaving a frame.
#[derive(Clone)]
struct IpcWriter {
    tx: mpsc::Sender<Value>,
}

impl IpcWriter {
    fn send(&self, envelope: &Value) {
        if self.tx.send(envelope.clone()).is_err() {
            log::error!("fd3 writer thread gone — dropping message");
        }
    }
}

/// Spawn the sole owner of fd 3: drains the channel, encodes each envelope, and
/// writes+flushes it. fd 3 is created by the parent (Electron spawns the helper
/// with a 4th "pipe" stdio entry); we take exclusive ownership for the process
/// lifetime and never touch fds 0/1/2 here.
fn spawn_fd3_writer(rx: mpsc::Receiver<Value>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut file = unsafe { std::fs::File::from_raw_fd(3) };
        for envelope in rx {
            match proto::encode(&envelope) {
                Ok(frame) => {
                    if let Err(e) = file.write_all(&frame).and_then(|_| file.flush()) {
                        log::error!("fd3 write failed: {e}");
                        break;
                    }
                }
                Err(e) => log::error!("encode failed: {e}"),
            }
        }
    })
}

fn main() {
    // `--version`: print and exit before any service setup. This is the probe
    // surface for `wispr-flow --doctor` — it proves the binary dynamically
    // links and reaches main() (e.g. catches `GLIBC_… not found` aborts)
    // without starting key capture, backend detection, or touching fd 3.
    // stdout is safe here: this path is never taken when Electron spawns the
    // helper (it passes no arguments).
    if std::env::args().any(|a| a == "--version") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return;
    }

    // Logs go to stderr (fd 2). stdout (fd 1) must stay non-IPC.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    let started = Instant::now();
    log::info!(
        "wispr-flow-linux-helper starting (pid {})",
        std::process::id()
    );

    let (tx, rx) = mpsc::channel::<Value>();
    let _writer = spawn_fd3_writer(rx);
    let ipc = IpcWriter { tx: tx.clone() };
    // Global key capture: streams `KeypressEvent`s on fd 3 so push-to-talk and
    // the in-app shortcut recorder work (the app has no hotkey detection of its
    // own — see capture/mod.rs). XInput2 on X11, evdev elsewhere. The returned
    // handle answers `CheckStaleKeys`. Independent of the focus/injection backend.
    let held_keys = capture::spawn(tx.clone());
    // The backend gets its own sink for async helper-initiated events (focus).
    let mut be = backend::detect(tx);
    log::info!("backend = {}", be.name());

    let mut decoder = proto::FrameDecoder::default();
    let mut stdin = std::io::stdin();
    let mut chunk = [0u8; 8192];

    loop {
        let n = match stdin.read(&mut chunk) {
            Ok(0) => {
                log::info!("stdin closed (EOF) — shutting down");
                break;
            }
            Ok(n) => n,
            Err(e) => {
                log::error!("stdin read error: {e}");
                break;
            }
        };
        for body in decoder.feed(&chunk[..n]) {
            match Incoming::parse(&body) {
                Ok(msg) => {
                    if msg.kind == Kind::Response {
                        // Electron-initiated responses (e.g. ACK to our events) — nothing to do yet.
                        log::debug!("<- response {} uuid={}", msg.command, msg.uuid);
                        continue;
                    }
                    handle_request(&mut *be, held_keys.as_ref(), &ipc, &msg, started);
                }
                Err(e) => log::error!("failed to parse message: {e}; body={body:?}"),
            }
        }
    }
}

fn handle_request(
    be: &mut dyn Backend,
    held_keys: &dyn capture::HeldKeys,
    ipc: &IpcWriter,
    msg: &Incoming,
    started: Instant,
) {
    let uuid = msg.uuid.as_str();
    // payload-bearing commands wrap their data under `.payload`
    let payload = msg.payload.get("payload").cloned().unwrap_or(Value::Null);

    match msg.command.as_str() {
        // ---- readiness / keepalive ----
        "IsReady" => {
            // ACK satisfies the readiness check; HelperLaunchTiming is optional and
            // read as `e.HelperLaunchTiming?.helperMainEntryMs`. Both go in one envelope.
            let ms = started.elapsed().as_millis() as u64;
            let mut inner = serde_json::Map::new();
            inner.insert("ACK".to_string(), json!(true));
            inner.insert(
                "HelperLaunchTiming".to_string(),
                json!({ "helperMainEntryMs": ms }),
            );
            ipc.send(&proto::envelope("HelperAPIResponse", inner, uuid));
        }

        // ---- core paste / keys ----
        "PasteText" => {
            let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
            let html = payload.get("htmlText").and_then(Value::as_str).filter(|s| !s.is_empty());
            match be.paste_text(text, html) {
                Ok(()) => ipc.send(&proto::ack(uuid)),
                Err(e) => {
                    log::error!("PasteText failed: {e}");
                    ipc.send(&proto::error(&format!("PasteText failed: {e}"), uuid));
                }
            }
        }
        "SimulateKeyPress" => {
            let keycode = payload.get("keycode").and_then(Value::as_u64).unwrap_or(0) as u32;
            let flags: Vec<String> = payload
                .get("flags")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            match be.simulate_key_press(keycode, &flags) {
                Ok(()) => ipc.send(&proto::ack(uuid)),
                Err(e) => {
                    log::error!("SimulateKeyPress failed: {e}");
                    ipc.send(&proto::error(&format!("SimulateKeyPress failed: {e}"), uuid));
                }
            }
        }

        // ---- active / running apps ----
        "GetActiveAppInfo" => match be.get_active_app() {
            Ok(a) => ipc.send(&proto::response("ActiveAppInfo", backend::active_app_payload(&a), uuid)),
            Err(e) => ipc.send(&proto::error(&format!("GetActiveAppInfo failed: {e}"), uuid)),
        },
        "GetAppInfo" => match be.get_active_app() {
            Ok(a) => ipc.send(&proto::response(
                "AppInfo",
                json!({ "payload": { "appName": a.app_name, "bundleId": a.bundle_id, "url": a.url } }),
                uuid,
            )),
            Err(e) => ipc.send(&proto::error(&format!("GetAppInfo failed: {e}"), uuid)),
        },
        "GetRunningApps" => match be.get_running_apps() {
            Ok(apps) => {
                let list: Vec<Value> = apps
                    .iter()
                    .map(|a| json!({ "bundleId": a.bundle_id, "name": a.name }))
                    .collect();
                ipc.send(&proto::response(
                    "RunningApps",
                    json!({ "payload": { "apps": list } }),
                    uuid,
                ));
            }
            Err(e) => ipc.send(&proto::error(&format!("GetRunningApps failed: {e}"), uuid)),
        },

        // ---- selection / accessibility ----
        "GetSelectedTextViaCopy" => match be.get_selected_text() {
            Ok(s) => ipc.send(&proto::response(
                "SelectedTextViaCopy",
                json!({ "payload": {
                    "selectedText": s.selected_text,
                    "beforeText": s.before_text,
                    "afterText": s.after_text,
                    "contents": s.contents,
                } }),
                uuid,
            )),
            Err(e) => ipc.send(&proto::error(&format!("GetSelectedTextViaCopy failed: {e}"), uuid)),
        },
        "GetAccessibilityStatus" => {
            let status = be.accessibility_status();
            ipc.send(&proto::response(
                "AccessibilityStatus",
                json!({ "payload": { "status": status } }),
                uuid,
            ));
        }
        "RequestAccessibilityPermission" | "StartAccessibilityServices" => {
            // No OS permission gate equivalent on Linux X11 — ACK as success.
            ipc.send(&proto::ack(uuid));
        }

        // ---- focus tracking ----
        "SetFocusChangeDetectorState" => {
            // payload.active toggles helper-initiated `AppInfoUpdate` focus events.
            let active = payload
                .get("active")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            be.set_focus_detection(active);
            log::info!("focus change detector -> {}", if active { "on" } else { "off" });
            ipc.send(&proto::ack(uuid));
        }

        // ---- stale-key recovery ----
        // The app polls this every ~5s with the keycodes it believes are held;
        // we answer with the subset that is NOT physically held right now (so it
        // can drop keys stuck by a missed release / unplugged device). Keycodes
        // are Windows VK codes (see keymap / ipc-contract.md §6).
        "CheckStaleKeys" => {
            let queried: Vec<u64> = payload
                .get("keycodes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            let held = held_keys.held_vks();
            let stale: Vec<u64> = queried
                .into_iter()
                .filter(|vk| !held.contains(&(*vk as u32)))
                .collect();
            ipc.send(&proto::response(
                "StaleKeysResponse",
                json!({ "payload": { "staleKeys": stale } }),
                uuid,
            ));
        }

        // ---- lifecycle ----
        "HelperAppShutdown" => {
            log::info!("HelperAppShutdown received — exiting");
            ipc.send(&proto::ack(uuid));
            std::process::exit(0);
        }

        // ---- everything else: safe no-op ACK ----
        // Covers focus/interval/analytics/feature-flag/BLE/floating-panel commands
        // we don't (yet) implement. ACK keeps the app healthy; see commands.json
        // for the full surface and ipc-contract.md §4 for which are stubs.
        other => {
            log::debug!("unhandled command '{other}' (uuid={uuid}) — ACK no-op");
            ipc.send(&proto::ack(uuid));
        }
    }
}
