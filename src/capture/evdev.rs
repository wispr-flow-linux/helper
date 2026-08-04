//! evdev key capture — reads `/dev/input/event*` directly.
//!
//! Sits below the display server, so it works identically on Wayland and X11
//! (the read-side mirror of the [uinput injection](crate::backend) write path).
//! Needs read access to the input devices: the logind `uaccess` ACL (granted to
//! the active session on most desktops) or membership in the `input` group.
//! Without it no device is readable and capture is a no-op (with a warning) —
//! `wispr-flow --doctor` and the shipped udev rule address this.
//!
//! **The device set is not fixed at startup.** Keyboards come and go constantly:
//! a Bluetooth keyboard creates a fresh `event*` node on every reconnect, a USB
//! hub/dock re-enumerates its downstream devices, autosuspend drops and restores
//! a device, and an app that autostarts at login may well run before any of them
//! are attached. Each reader thread ends when its device disappears, so a
//! start-once enumeration silently decays to watching nothing — push-to-talk and
//! the in-app shortcut recorder then stay dead until Wispr Flow is restarted,
//! with no error anywhere. A monitor thread therefore rescans on `inotify`
//! activity under `/dev/input` (plus a periodic backstop) and adopts every
//! keyboard that appears, including one that returns under a new node.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::{emit_keypress, HeldKeys};
use crate::backend::uinput;
use crate::backend::EventSink;
use crate::keymap;

// evdev event type / key-state values (<linux/input-event-codes.h>).
const EV_KEY: u16 = 0x01;
const KEY_PRESS: i32 = 1;
const KEY_RELEASE: i32 = 0;
// Representative keys used to distinguish a real keyboard from a mouse/gamepad
// (which carry BTN_* codes but not letter keys).
const KEY_A: u16 = 30;
const KEY_Z: u16 = 44;

// Widest keycode we map; a (KEY_MAX/8 + 1)-byte bitmap covers every code.
const KEY_MAX: usize = 0x2ff;
const BITMAP_LEN: usize = (KEY_MAX / 8) + 1;

/// Buffer for the device name read via `EVIOCGNAME` (the kernel caps names well
/// under this).
const NAME_LEN: usize = 256;

/// Backstop rescan period for the hotplug monitor. `inotify` covers the normal
/// case immediately; this bounds the two ways a change can still be missed — a
/// dropped/overflowed inotify queue, and a node re-created in the brief window
/// between a device dying and its reader thread deregistering the path. A scan
/// is a `readdir` plus an `open`+ioctl per *unwatched* node, the same work the
/// `EVIOCGKEY` stale-key poll already does every ~5 s.
const RESCAN_INTERVAL: Duration = Duration::from_secs(10);

/// Settle delay after inotify activity: one physical device creates several
/// nodes, and udev applies the `uaccess` ACL a moment *after* the node appears
/// (an open right at `IN_CREATE` would fail with `EACCES`). Coalesces the burst
/// into one scan and lets permissions land first.
const HOTPLUG_SETTLE: Duration = Duration::from_millis(120);

// ioctl request numbers (asm-generic `_IOC` encoding, shared by x86_64/aarch64):
//   _IOC(dir, type, nr, size) = (dir<<30) | (size<<16) | (type<<8) | nr
// `'E'` (0x45) is the evdev ioctl group. All reads, so dir = _IOC_READ = 2.
const fn ioc_read(nr: u64, size: u64) -> libc::c_ulong {
    ((2u64 << 30) | (size << 16) | (0x45u64 << 8) | nr) as libc::c_ulong
}
// EVIOCGKEY(len): global state bitmap of currently-pressed keys.  nr = 0x18
const fn eviocgkey() -> libc::c_ulong {
    ioc_read(0x18, BITMAP_LEN as u64)
}
// EVIOCGBIT(EV_KEY, len): capability bitmap of the keys a device can emit.
//   nr = 0x20 + ev_type  → 0x21 for EV_KEY
const fn eviocgbit_key() -> libc::c_ulong {
    ioc_read(0x20 + EV_KEY as u64, BITMAP_LEN as u64)
}
// EVIOCGNAME(len): device name string.  nr = 0x06
const fn eviocgname(len: u64) -> libc::c_ulong {
    ioc_read(0x06, len)
}

/// Event nodes with a live reader thread, shared between the initial scan, the
/// hotplug monitor, and the readers themselves. A rescan skips what is already
/// watched: opening one device twice would duplicate every `KeypressEvent` and
/// tear a gap in the shared `index` sequence the app cross-checks.
type Watched = Arc<Mutex<HashSet<PathBuf>>>;

/// Lock the registry, ignoring poisoning: a panicking reader thread must not
/// take the rest of key capture down with it.
fn lock(watched: &Watched) -> MutexGuard<'_, HashSet<PathBuf>> {
    watched.lock().unwrap_or_else(|e| e.into_inner())
}

fn bit_set(bitmap: &[u8], code: u16) -> bool {
    let (byte, bit) = (code as usize / 8, code as u32 % 8);
    byte < bitmap.len() && (bitmap[byte] >> bit) & 1 == 1
}

fn is_event_node(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.starts_with("event"))
}

/// True if the open device advertises ordinary keyboard keys (KEY_A..KEY_Z),
/// which filters out mice, touchpads, and other non-keyboard event nodes.
fn is_keyboard(fd: libc::c_int) -> bool {
    let mut bitmap = [0u8; BITMAP_LEN];
    if unsafe { libc::ioctl(fd, eviocgbit_key(), bitmap.as_mut_ptr()) } < 0 {
        return false;
    }
    bit_set(&bitmap, KEY_A) && bit_set(&bitmap, KEY_Z)
}

/// The open device's `EVIOCGNAME` string, or `None` when the ioctl fails.
fn device_name(fd: libc::c_int) -> Option<String> {
    let mut buf = [0u8; NAME_LEN];
    let n = unsafe { libc::ioctl(fd, eviocgname(NAME_LEN as u64), buf.as_mut_ptr()) };
    if n <= 0 {
        return None;
    }
    // The ioctl returns the byte count including the trailing NUL.
    let bytes = &buf[..(n as usize).min(NAME_LEN)];
    let bytes = bytes.split(|&b| b == 0).next().unwrap_or(bytes);
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// True for the helper's *own* uinput virtual keyboard. Injection creates it
/// after capture starts, so only a rescan can ever see it — and watching it
/// would echo every key we inject (a `PasteText` Ctrl+V, a `SimulateKeyPress`
/// chord) straight back to the app as user input. Other virtual keyboards
/// (`ydotool`, remote-desktop agents) are deliberately *not* filtered: those
/// carry real input into this session.
fn is_own_virtual_keyboard(name: &str) -> bool {
    name == uinput::DEVICE_NAME
}

/// Open and start a reader thread for every keyboard under `/dev/input` that is
/// readable and not already watched. Returns how many were newly adopted.
fn scan(events: &EventSink, index: &Arc<AtomicU64>, watched: &Watched) -> usize {
    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(e) => {
            log::warn!("evdev capture: cannot read /dev/input: {e}");
            return 0;
        }
    };
    let pid = std::process::id();
    let mut adopted = 0;
    for entry in dir.flatten() {
        let path = entry.path();
        if !is_event_node(&path) || lock(watched).contains(&path) {
            continue;
        }
        // Blocking fd: open() never blocks on evdev, but read() must, so the
        // per-device thread can park until the next key event.
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(f) => f,
            Err(_) => continue, // not readable -> skip (permission or busy)
        };
        if !is_keyboard(file.as_raw_fd()) {
            continue;
        }
        let name = device_name(file.as_raw_fd()).unwrap_or_default();
        if is_own_virtual_keyboard(&name) {
            continue;
        }
        if !lock(watched).insert(path.clone()) {
            continue; // adopted concurrently
        }
        let (reader_events, reader_index) = (events.clone(), index.clone());
        let reader_watched = watched.clone();
        let reader_path = path.clone();
        let builder = std::thread::Builder::new().name("key-capture-evdev".to_string());
        match builder.spawn(move || {
            read_device(&reader_path, file, &reader_events, &reader_index, pid);
            // Deregister so the same node is re-adopted if the device returns.
            forget_device(&reader_path, &reader_watched);
        }) {
            Ok(_) => {
                log::info!("evdev capture: watching {} ({name})", path.display());
                adopted += 1;
            }
            Err(e) => {
                lock(watched).remove(&path);
                log::warn!("evdev capture: failed to spawn reader thread: {e}");
            }
        }
    }
    adopted
}

/// Drop a device from the registry when its reader thread ends, and say so
/// loudly if it was the last one — that state is exactly the silent-death case
/// (no keyboard watched, so no hotkey can ever fire).
fn forget_device(path: &Path, watched: &Watched) {
    let mut set = lock(watched);
    set.remove(path);
    if set.is_empty() {
        log::warn!(
            "evdev capture: no keyboard is being watched after {} went away — \
             push-to-talk and the in-app shortcut recorder are dead until one \
             (re)appears",
            path.display()
        );
    }
}

/// Start evdev capture: adopt the keyboards present now, then keep watching for
/// ones that appear later. Returns a [`HeldKeys`] handle, or `None` only when
/// nothing is readable *and* hotplug monitoring is unavailable (so the caller
/// can fall back — there is nothing left that could start working).
pub fn start(events: EventSink) -> Option<Box<dyn HeldKeys>> {
    let watched: Watched = Arc::new(Mutex::new(HashSet::new()));
    let index = Arc::new(AtomicU64::new(0));

    if scan(&events, &index, &watched) == 0 {
        log::warn!(
            "evdev capture: no readable keyboard under /dev/input — push-to-talk \
             and the in-app shortcut recorder will NOT work until one appears. \
             If this persists, run `wispr-flow --install-udev-rules`, or add the \
             user to the `input` group (then re-login)."
        );
    }

    match spawn_hotplug_monitor(events, index, watched.clone()) {
        Ok(()) => Some(Box::new(EvdevHeld)),
        Err(e) => {
            log::warn!(
                "evdev capture: hotplug monitor unavailable ({e}) — a keyboard \
                 connected or reconnected later will NOT be picked up until \
                 Wispr Flow is restarted"
            );
            // Devices adopted at startup still work; with none, capture is dead.
            if lock(&watched).is_empty() {
                None
            } else {
                Some(Box::new(EvdevHeld))
            }
        }
    }
}

/// Watch `/dev/input` for device changes and rescan on each one.
///
/// `IN_CREATE` catches a new node; `IN_ATTRIB` catches udev applying the
/// `uaccess` ACL to a node we could not open a moment earlier (the common case
/// on a fresh plug); `IN_MOVED_TO` catches a node renamed into place.
fn spawn_hotplug_monitor(
    events: EventSink,
    index: Arc<AtomicU64>,
    watched: Watched,
) -> Result<(), String> {
    // Non-blocking so the drain loop terminates on EAGAIN; poll() does the
    // waiting and gives us the periodic backstop for free.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(format!(
            "inotify_init1: {}",
            std::io::Error::last_os_error()
        ));
    }
    let dir = std::ffi::CString::new("/dev/input").expect("path literal has no NUL");
    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            dir.as_ptr(),
            libc::IN_CREATE | libc::IN_ATTRIB | libc::IN_MOVED_TO,
        )
    };
    if wd < 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(format!("inotify_add_watch /dev/input: {e}"));
    }
    std::thread::Builder::new()
        .name("key-capture-hotplug".to_string())
        .spawn(move || monitor_loop(fd, &events, &index, &watched))
        .map(|_| ())
        .map_err(|e| {
            unsafe { libc::close(fd) };
            format!("spawn monitor thread: {e}")
        })
}

/// Rescan whenever `/dev/input` changes, and at least every
/// [`RESCAN_INTERVAL`]. Runs for the process lifetime.
fn monitor_loop(fd: libc::c_int, events: &EventSink, index: &Arc<AtomicU64>, watched: &Watched) {
    let mut buf = [0u8; 4096];
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = RESCAN_INTERVAL.as_millis() as libc::c_int;
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if ready < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // Keep going on the timer alone rather than losing hotplug entirely.
            log::warn!("evdev capture: hotplug poll failed: {e} — rescanning on the timer only");
            std::thread::sleep(RESCAN_INTERVAL);
        } else if ready > 0 {
            // The specific events don't matter: any change under /dev/input
            // means "rescan". Drain the queue so poll() blocks again after.
            while unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) } > 0 {}
            std::thread::sleep(HOTPLUG_SETTLE);
        }
        scan(events, index, watched);
    }
}

/// Blocking read loop for one device: decode `input_event`s and emit a
/// `KeypressEvent` for every key press/release (auto-repeat is ignored).
fn read_device(path: &Path, mut file: File, events: &EventSink, index: &AtomicU64, pid: u32) {
    let evsize = std::mem::size_of::<libc::input_event>();
    let mut buf = vec![0u8; evsize * 64];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => {
                log::info!("evdev capture: {} closed (EOF)", path.display());
                return;
            }
            Ok(n) => n,
            Err(e) => {
                // ENODEV on unplug, etc. Drop this device; others keep running,
                // and the hotplug monitor re-adopts it when it comes back.
                log::info!("evdev capture: {} read ended: {e}", path.display());
                return;
            }
        };
        // The kernel only ever returns whole `input_event`s; guard the slice
        // regardless in case of a short trailing read.
        let mut off = 0;
        while off + evsize <= n {
            // SAFETY: a `Vec<u8>` is byte-aligned, so read the struct unaligned.
            let ev: libc::input_event =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const _) };
            off += evsize;
            if ev.type_ != EV_KEY {
                continue;
            }
            let press = match ev.value {
                KEY_PRESS => true,
                KEY_RELEASE => false,
                _ => continue, // skip auto-repeat (value == 2)
            };
            let Some(vk) = keymap::evdev_to_vk(ev.code) else {
                continue; // unmapped physical key — nothing the app understands
            };
            emit_keypress(events, index, pid, vk, press);
        }
    }
}

/// Stale-key querier backed by `EVIOCGKEY` across all readable event devices.
struct EvdevHeld;

impl HeldKeys for EvdevHeld {
    fn held_vks(&self) -> HashSet<u32> {
        let mut held = HashSet::new();
        let dir = match std::fs::read_dir("/dev/input") {
            Ok(d) => d,
            Err(_) => return held,
        };
        for entry in dir.flatten() {
            let path = entry.path();
            if !is_event_node(&path) {
                continue;
            }
            let file = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
            {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut bitmap = [0u8; BITMAP_LEN];
            if unsafe { libc::ioctl(file.as_raw_fd(), eviocgkey(), bitmap.as_mut_ptr()) } < 0 {
                continue;
            }
            for code in 0..=KEY_MAX as u16 {
                if bit_set(&bitmap, code) {
                    if let Some(vk) = keymap::evdev_to_vk(code) {
                        held.insert(vk);
                    }
                }
            }
        }
        held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // EVIOCGNAME(256) = _IOC(_IOC_READ, 'E', 0x06, 256): pin the encoding, since
    // a wrong request number silently returns -1 and every device would then
    // read as unnamed — which would defeat the own-device filter below.
    #[test]
    fn eviocgname_matches_kernel_encoding() {
        assert_eq!(eviocgname(NAME_LEN as u64), 0x8100_4506);
    }

    // Watching our own uinput keyboard would echo injected keys back to the app
    // as user input. Other virtual keyboards must still be watched: they carry
    // real input into the session.
    #[test]
    fn own_virtual_keyboard_is_filtered_by_name() {
        assert!(is_own_virtual_keyboard(uinput::DEVICE_NAME));
        assert!(!is_own_virtual_keyboard("Keychron Keychron K2"));
        assert!(!is_own_virtual_keyboard("RustDesk UInput Keyboard"));
        assert!(!is_own_virtual_keyboard(""));
    }

    // A rescan must never adopt a device twice (duplicate KeypressEvents + a gap
    // in the index sequence), and a reader thread ending must release the path
    // so the device is re-adopted when it comes back under the same node.
    #[test]
    fn registry_dedupes_and_releases_on_reader_exit() {
        let watched: Watched = Arc::new(Mutex::new(HashSet::new()));
        let path = PathBuf::from("/dev/input/event42");

        assert!(lock(&watched).insert(path.clone()));
        assert!(!lock(&watched).insert(path.clone()));

        forget_device(&path, &watched);
        assert!(lock(&watched).is_empty());
        assert!(lock(&watched).insert(path.clone()));
    }

    // Poisoning happens when a reader thread panics; capture must keep working.
    #[test]
    fn registry_lock_survives_poisoning() {
        let watched: Watched = Arc::new(Mutex::new(HashSet::new()));
        let poisoner = watched.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().expect("fresh mutex");
            panic!("reader thread died");
        })
        .join();

        assert!(watched.lock().is_err(), "mutex should be poisoned");
        lock(&watched).insert(PathBuf::from("/dev/input/event0"));
        assert_eq!(lock(&watched).len(), 1);
    }

    #[test]
    fn only_event_nodes_are_considered() {
        assert!(is_event_node(Path::new("/dev/input/event0")));
        assert!(is_event_node(Path::new("/dev/input/event22")));
        assert!(!is_event_node(Path::new("/dev/input/mouse0")));
        assert!(!is_event_node(Path::new("/dev/input/by-id")));
    }
}
