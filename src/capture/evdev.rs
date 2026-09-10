//! evdev key capture — reads `/dev/input/event*` directly.
//!
//! Sits below the display server, so it works identically on Wayland and X11
//! (the read-side mirror of the [uinput injection](crate::backend) write path).
//! Needs read access to the input devices: the logind `uaccess` ACL (granted to
//! the active session on most desktops) or membership in the `input` group.
//! Without it no device is readable and capture is a no-op (with a warning) —
//! `wispr-flow --doctor` and the shipped udev rule address this.

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

/// How often the supervisor re-scans `/dev/input` for keyboards it is not
/// already reading. Polling rather than inotify is deliberate: udev creates the
/// event node *before* logind applies the `uaccess` ACL, so a fresh node is
/// routinely unreadable at `IN_CREATE` and readable a few milliseconds later.
/// Covering that with inotify takes `IN_ATTRIB` plus an open-retry — which is
/// what a rescan already is, with no race left to lose. One `read_dir` plus one
/// ioctl per node is microseconds of work.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

// Widest keycode we map; a (KEY_MAX/8 + 1)-byte bitmap covers every code.
const KEY_MAX: usize = 0x2ff;
const BITMAP_LEN: usize = (KEY_MAX / 8) + 1;

// ioctl request numbers (asm-generic `_IOC` encoding, shared by x86_64/aarch64):
//   _IOC(dir, type, nr, size) = (dir<<30) | (size<<16) | (type<<8) | nr
// `'E'` (0x45) is the evdev ioctl group. Both reads, so dir = _IOC_READ = 2.
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

// EVIOCGNAME(len): the device's human-readable name.  nr = 0x06
const fn eviocgname() -> libc::c_ulong {
    ioc_read(0x06, NAME_LEN as u64)
}
// Matches the uinput `name[80]` field the kernel copies the name out of.
const NAME_LEN: usize = 80;

/// Name the helper gives its own uinput virtual keyboard (`backend::uinput`).
const OWN_INJECTION_NAME: &[u8] = b"Wispr Flow Linux Helper";

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

/// Device paths that currently have a live reader thread, letting a rescan tell
/// "already reading this one" from "new device". Readers remove their own path
/// as they exit, which is what makes recovery work: the kernel reuses `eventN`
/// names, so a device that re-enumerates under its old name is re-adopted on
/// the next pass.
type Watched = Arc<Mutex<HashSet<PathBuf>>>;

/// Lock the watched-path set. The release profile is `panic = "abort"`, so a
/// poisoned mutex is unreachable there; elsewhere the set is still sound to use
/// as-is, and losing key capture is a worse outcome than reusing it.
fn lock(watched: &Watched) -> MutexGuard<'_, HashSet<PathBuf>> {
    watched.lock().unwrap_or_else(|e| e.into_inner())
}

/// True if this device is a helper's own injection keyboard. Capture must skip
/// it: the rescan runs *after* the injection backend is up (the one-shot scan it
/// replaced ran before, so upstream never met this), and reading our own virtual
/// keyboard would report every injected character back to the app as a user
/// keypress. Matching on the name also skips a device left behind by an earlier
/// helper process, which is equally not real input.
fn is_own_injection_device(fd: libc::c_int) -> bool {
    let mut buf = [0u8; NAME_LEN];
    if unsafe { libc::ioctl(fd, eviocgname(), buf.as_mut_ptr()) } < 0 {
        return false; // no name to compare; treat as a normal device
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    &buf[..end] == OWN_INJECTION_NAME
}

/// Open every readable keyboard under `/dev/input` whose path is not in `skip`,
/// returning `(path, file)` pairs with blocking fds ready for `read`.
fn open_keyboards(skip: &HashSet<PathBuf>) -> Vec<(PathBuf, File)> {
    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(e) => {
            log::warn!("evdev capture: cannot read /dev/input: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let path = entry.path();
        if !is_event_node(&path) || skip.contains(&path) {
            continue;
        }
        // Blocking fd: open() never blocks on evdev, but read() must, so the
        // per-device thread can park until the next key event.
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(f) => f,
            Err(_) => continue, // not readable -> skip (permission or busy)
        };
        let fd = file.as_raw_fd();
        if is_keyboard(fd) && !is_own_injection_device(fd) {
            out.push((path, file));
        }
    }
    out
}

/// Start a reader thread for every keyboard not already being read. Returns how
/// many readers were started.
fn adopt_keyboards(
    watched: &Watched,
    events: &EventSink,
    index: &Arc<AtomicU64>,
    pid: u32,
) -> usize {
    let known = lock(watched).clone();
    let mut started = 0;
    for (path, file) in open_keyboards(&known) {
        log::info!("evdev capture: watching {}", path.display());
        // Claim the path before spawning: a reader that finishes early must not
        // race ahead of the insert and leave a stale entry no rescan can clear.
        lock(watched).insert(path.clone());

        // Bookkeeping rides in the closure rather than `read_device`, so the
        // reader stays a plain read loop: whatever ends it — ENODEV on unplug,
        // EOF, any read error — releases the path for the next pass to re-adopt.
        let reader = {
            let path = path.clone();
            let events = events.clone();
            let index = index.clone();
            let watched = watched.clone();
            move || {
                read_device(&path, file, &events, &index, pid);
                lock(&watched).remove(&path);
            }
        };
        let builder = std::thread::Builder::new().name("key-capture-evdev".to_string());
        match builder.spawn(reader) {
            Ok(_) => started += 1,
            Err(e) => {
                log::warn!("evdev capture: failed to spawn reader thread: {e}");
                lock(watched).remove(&path);
            }
        }
    }
    started
}

/// Start evdev capture: one reader thread per keyboard, plus a supervisor that
/// adopts keyboards appearing later. Returns a [`HeldKeys`] handle, or `None`
/// when no device is readable (so the caller can fall back).
pub fn start(events: EventSink) -> Option<Box<dyn HeldKeys>> {
    let watched: Watched = Arc::new(Mutex::new(HashSet::new()));
    let index = Arc::new(AtomicU64::new(0));
    let pid = std::process::id();

    // First pass is synchronous: the caller picks its capture backend from what
    // is readable right now, and the warning below has to fire before we return.
    if adopt_keyboards(&watched, &events, &index, pid) == 0 {
        log::warn!(
            "evdev capture: no readable keyboard under /dev/input — push-to-talk \
             and the in-app shortcut recorder will NOT work. Run \
             `wispr-flow --install-udev-rules`, or add the user to the `input` \
             group (then re-login)."
        );
        return None;
    }

    // Then keep looking. Without this the helper goes permanently deaf to any
    // device that re-enumerates: a USB hub losing power across suspend/resume
    // invalidates every open fd with ENODEV, and the replacement nodes — same
    // `eventN` names, seconds later — were never reopened. Capture died silently
    // while `EvdevHeld` kept answering stale-key queries from a fresh scan, so
    // the app saw a plausible keyboard that simply never pressed anything.
    let builder = std::thread::Builder::new().name("key-capture-evdev-scan".to_string());
    let supervise = move || loop {
        std::thread::sleep(RESCAN_INTERVAL);
        adopt_keyboards(&watched, &events, &index, pid);
    };
    if let Err(e) = builder.spawn(supervise) {
        log::warn!(
            "evdev capture: hotplug supervisor not started ({e}) — keyboards that \
             appear or re-enumerate later will be ignored until restart"
        );
    }
    Some(Box::new(EvdevHeld))
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
                // ENODEV on unplug, etc. Drop this device; others keep running.
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
