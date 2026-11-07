//! evdev key capture — reads `/dev/input/event*` directly.
//!
//! Sits below the display server, so it works identically on Wayland and X11
//! (the read-side mirror of the [uinput injection](crate::backend) write path).
//! Needs read access to the input devices: the logind `uaccess` ACL (granted to
//! the active session on most desktops) or membership in the `input` group.
//! Without it no device is readable and capture is a no-op (with a warning) —
//! `wispr-flow --doctor` and the shipped udev rule address this.
//!
//! The keyboard set is not fixed at startup. Keyboards come and go (USB replug,
//! a KVM switching away and back, a Bluetooth reconnect), and each return is a
//! brand-new event node. An inotify watch on `/dev/input` opens new keyboards
//! as they appear; a reader whose device is removed drops it.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use super::{emit_keypress, HeldKeys};
use crate::backend::{uinput, EventSink};
use crate::keymap;

const INPUT_DIR: &str = "/dev/input";
const SYSFS_INPUT_DIR: &str = "/sys/class/input";

// evdev event type / key-state values (<linux/input-event-codes.h>).
const EV_KEY: u16 = 0x01;
const KEY_PRESS: i32 = 1;
const KEY_RELEASE: i32 = 0;
// Representative keys used to distinguish a real keyboard from a mouse/gamepad
// (which carry BTN_* codes but not letter keys).
const KEY_A: u16 = 30;
const KEY_Z: u16 = 44;
// Mouse thumb buttons: a pointer device with either is watched too, so they
// can be bound as shortcuts (Mouse 4/5 push-to-talk).
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

// Widest keycode we map; a (KEY_MAX/8 + 1)-byte bitmap covers every code.
const KEY_MAX: usize = 0x2ff;
const BITMAP_LEN: usize = (KEY_MAX / 8) + 1;

// ioctl request numbers (asm-generic `_IOC` encoding, shared by x86_64/aarch64):
//   _IOC(dir, type, nr, size) = (dir<<30) | (size<<16) | (type<<8) | nr
// `'E'` (0x45) is the evdev ioctl group. All reads, so dir = _IOC_READ = 2.
const fn ioc_read(nr: u64, size: u64) -> libc::c_ulong {
    ((2u64 << 30) | (size << 16) | (0x45u64 << 8) | nr) as libc::c_ulong
}
// EVIOCGNAME(len): device name, NUL-terminated.  nr = 0x06
const fn eviocgname(len: usize) -> libc::c_ulong {
    ioc_read(0x06, len as u64)
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

fn bit_set(bitmap: &[u8], code: u16) -> bool {
    let (byte, bit) = (code as usize / 8, code as u32 % 8);
    byte < bitmap.len() && (bitmap[byte] >> bit) & 1 == 1
}

fn is_event_node(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.starts_with("event"))
}

/// True if a key capability bitmap has ordinary keyboard keys (KEY_A..KEY_Z) or
/// mouse thumb buttons, which filters out touchpads, plain two/three-button
/// mice, joysticks, and other event nodes with nothing the app can bind.
fn is_bindable(bitmap: &[u8]) -> bool {
    (bit_set(bitmap, KEY_A) && bit_set(bitmap, KEY_Z))
        || bit_set(bitmap, BTN_SIDE)
        || bit_set(bitmap, BTN_EXTRA)
}

/// [`is_bindable`] for an open device, from `EVIOCGBIT(EV_KEY)`.
fn is_keyboard(fd: libc::c_int) -> bool {
    let mut bitmap = [0u8; BITMAP_LEN];
    if unsafe { libc::ioctl(fd, eviocgbit_key(), bitmap.as_mut_ptr()) } < 0 {
        return false;
    }
    is_bindable(&bitmap)
}

/// Decide from sysfs, without opening the node, whether `path` is worth
/// watching. Opening an evdev node runs the driver's open(), which resumes a
/// USB HID device from autosuspend, so probing every node by opening it wakes
/// joysticks, pedals, and anything else plugged in. `None` when sysfs doesn't
/// have the answer; the caller then opens the node and asks the ioctl.
fn sysfs_bindable(path: &Path) -> Option<bool> {
    let dev = Path::new(SYSFS_INPUT_DIR)
        .join(path.file_name()?)
        .join("device");
    let caps = std::fs::read_to_string(dev.join("capabilities/key")).ok()?;
    if !is_bindable(&parse_sysfs_bitmap(&caps)) {
        return Some(false);
    }
    let name = std::fs::read_to_string(dev.join("name")).ok()?;
    Some(name.trim_end_matches('\n') != uinput::DEVICE_NAME)
}

/// Decode a sysfs capability bitmap (`capabilities/key`) into the same byte
/// layout `EVIOCGBIT` fills. The kernel prints it as hex `unsigned long` words,
/// most significant first, separated by spaces, with leading zero words left
/// out. Bits above `KEY_MAX` are dropped.
fn parse_sysfs_bitmap(s: &str) -> [u8; BITMAP_LEN] {
    const WORD_BYTES: usize = std::mem::size_of::<libc::c_ulong>();
    let mut bitmap = [0u8; BITMAP_LEN];
    for (i, word) in s.split_whitespace().rev().enumerate() {
        let Ok(word) = libc::c_ulong::from_str_radix(word, 16) else {
            continue;
        };
        for (j, byte) in word.to_le_bytes().into_iter().enumerate() {
            if let Some(slot) = bitmap.get_mut(i * WORD_BYTES + j) {
                *slot = byte;
            }
        }
    }
    bitmap
}

/// The device's name (`EVIOCGNAME`), or `None` if the ioctl fails.
fn device_name(fd: libc::c_int) -> Option<String> {
    let mut buf = [0u8; 256];
    if unsafe { libc::ioctl(fd, eviocgname(buf.len()), buf.as_mut_ptr()) } < 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// State shared by the hotplug watcher and every reader thread.
struct Capture {
    events: EventSink,
    index: AtomicU64,
    pid: u32,
    /// Live readers by device number (`st_rdev`). A node can be reported
    /// several times (startup scan, `IN_CREATE`, each `IN_ATTRIB`); this keeps
    /// it to one reader. The open file is shared so stale-key queries reuse it
    /// instead of opening nodes again.
    watched: Mutex<HashMap<u64, Arc<File>>>,
}

impl Capture {
    /// Try every event node under `/dev/input`. Returns how many new readers
    /// were started.
    fn scan(self: &Arc<Self>) -> usize {
        let dir = match std::fs::read_dir(INPUT_DIR) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("evdev capture: cannot read {INPUT_DIR}: {e}");
                return 0;
            }
        };
        dir.flatten().filter(|e| self.try_add(&e.path())).count()
    }

    /// Start a reader for `path` if it is a readable keyboard that isn't
    /// already watched. Returns true when a new reader was started.
    fn try_add(self: &Arc<Self>, path: &Path) -> bool {
        if !is_event_node(path) || sysfs_bindable(path) == Some(false) {
            return false;
        }
        // Blocking fd: open() never blocks on evdev, but read() must, so the
        // per-device thread can park until the next key event.
        let file = match OpenOptions::new().read(true).open(path) {
            Ok(f) => f,
            // Not readable (yet): a fresh node exists before udev grants
            // access, and the IN_ATTRIB from that grant retries it.
            Err(_) => return false,
        };
        let fd = file.as_raw_fd();
        if !is_keyboard(fd) {
            return false;
        }
        if device_name(fd).as_deref() == Some(uinput::DEVICE_NAME) {
            return false; // our own injection device
        }
        let rdev = match file.metadata() {
            Ok(m) => m.rdev(),
            Err(_) => return false,
        };
        let file = Arc::new(file);
        let fresh = self.watched.lock().is_ok_and(|mut w| {
            use std::collections::hash_map::Entry;
            match w.entry(rdev) {
                Entry::Vacant(slot) => {
                    slot.insert(file.clone());
                    true
                }
                Entry::Occupied(_) => false,
            }
        });
        if !fresh {
            return false;
        }

        log::info!("evdev capture: watching {}", path.display());
        let cap = self.clone();
        let thread_path = path.to_path_buf();
        let builder = std::thread::Builder::new().name("key-capture-evdev".to_string());
        let spawned = builder.spawn(move || {
            read_device(&thread_path, &file, &cap.events, &cap.index, cap.pid);
            if let Ok(mut w) = cap.watched.lock() {
                w.remove(&rdev);
            }
            // The kernel can hand this device number to a replacement node
            // before we get here, and the watcher would have skipped it as
            // already watched. Rescan so it isn't lost.
            cap.scan();
        });
        if let Err(e) = spawned {
            log::warn!("evdev capture: failed to spawn reader thread: {e}");
            if let Ok(mut w) = self.watched.lock() {
                w.remove(&rdev);
            }
            return false;
        }
        true
    }
}

/// Start evdev capture: one reader thread per keyboard, plus a hotplug watcher
/// that adds keyboards as they appear. Returns a [`HeldKeys`] handle, or `None`
/// when nothing is readable now and nothing can be picked up later.
pub fn start(events: EventSink) -> Option<Box<dyn HeldKeys>> {
    let cap = Arc::new(Capture {
        events,
        index: AtomicU64::new(0),
        pid: std::process::id(),
        watched: Mutex::new(HashMap::new()),
    });
    let found = cap.scan();
    let hotplug = watch_hotplug(cap.clone());
    if let Err(e) = &hotplug {
        log::warn!(
            "evdev capture: cannot watch {INPUT_DIR} for new keyboards ({e}); a \
             keyboard that is unplugged or switched away will stay dead until \
             restart"
        );
    }
    if found == 0 {
        log::warn!(
            "evdev capture: no readable keyboard under /dev/input — push-to-talk \
             and the in-app shortcut recorder will NOT work until one appears. \
             If a keyboard is connected, run `wispr-flow --install-udev-rules`, \
             or add the user to the `input` group (then re-login)."
        );
        if hotplug.is_err() {
            return None;
        }
    }
    Some(Box::new(EvdevHeld(cap)))
}

/// Watch `/dev/input` and add keyboards as they appear. A new node is created
/// (`IN_CREATE`) before udev applies its permissions, so opening it then usually
/// fails; udev's chmod/ACL change raises `IN_ATTRIB`, which retries it.
fn watch_hotplug(cap: Arc<Capture>) -> std::io::Result<()> {
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh inotify descriptor that nothing else owns.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mask = libc::IN_CREATE | libc::IN_ATTRIB;
    if unsafe { libc::inotify_add_watch(fd, c"/dev/input".as_ptr(), mask) } < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let builder = std::thread::Builder::new().name("key-capture-hotplug".to_string());
    builder.spawn(move || {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = match file.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    log::warn!("evdev capture: hotplug watch ended: {e}");
                    return;
                }
            };
            for ev in parse_inotify(&buf[..n]) {
                match ev {
                    // Events were dropped; we don't know what changed.
                    Inotify::Overflow => {
                        cap.scan();
                    }
                    Inotify::Name(name) => {
                        cap.try_add(&Path::new(INPUT_DIR).join(OsStr::from_bytes(name)));
                    }
                }
            }
        }
    })?;
    Ok(())
}

/// One decoded inotify record.
#[derive(Debug, PartialEq, Eq)]
enum Inotify<'a> {
    /// A change to the named entry in the watched directory.
    Name(&'a [u8]),
    /// `IN_Q_OVERFLOW`: the kernel dropped events.
    Overflow,
}

/// Decode a buffer of `inotify_event` records. Records without a name (events
/// on the directory itself) are skipped.
fn parse_inotify(buf: &[u8]) -> Vec<Inotify<'_>> {
    let hdr = std::mem::size_of::<libc::inotify_event>();
    let mut out = Vec::new();
    let mut off = 0;
    while off + hdr <= buf.len() {
        // SAFETY: a `Vec<u8>` is byte-aligned, so read the struct unaligned.
        let ev: libc::inotify_event =
            unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const _) };
        let start = off + hdr;
        let end = (start + ev.len as usize).min(buf.len());
        off = start + ev.len as usize;
        if ev.mask & libc::IN_Q_OVERFLOW != 0 {
            out.push(Inotify::Overflow);
            continue;
        }
        // `name` is NUL-padded out to `len`.
        let raw = &buf[start..end];
        let name = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
        if !name.is_empty() {
            out.push(Inotify::Name(name));
        }
    }
    out
}

/// Blocking read loop for one device: decode `input_event`s and emit a
/// `KeypressEvent` for every key press/release (auto-repeat is ignored).
fn read_device(path: &Path, mut file: &File, events: &EventSink, index: &AtomicU64, pid: u32) {
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
                // and the hotplug watcher picks it up again if it comes back.
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

/// Stale-key querier backed by `EVIOCGKEY` on the devices the readers already
/// hold open. Keys on any other node never reach the app, so the app can't
/// believe they are held, and opening those nodes would wake them.
struct EvdevHeld(Arc<Capture>);

impl HeldKeys for EvdevHeld {
    fn held_vks(&self) -> HashSet<u32> {
        let mut held = HashSet::new();
        let files: Vec<Arc<File>> = match self.0.watched.lock() {
            Ok(w) => w.values().cloned().collect(),
            Err(_) => return held,
        };
        for file in files {
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

    /// Encode one `inotify_event` record the way the kernel lays it out: the
    /// fixed header, then `name` NUL-padded to `len` bytes.
    fn record(mask: u32, name: &str, len: u32) -> Vec<u8> {
        let ev = libc::inotify_event {
            wd: 1,
            mask,
            cookie: 0,
            len,
        };
        let mut out = unsafe {
            std::slice::from_raw_parts(
                &ev as *const _ as *const u8,
                std::mem::size_of::<libc::inotify_event>(),
            )
        }
        .to_vec();
        let mut padded = name.as_bytes().to_vec();
        padded.resize(len as usize, 0);
        out.extend(padded);
        out
    }

    #[test]
    fn parse_inotify_reads_padded_names_in_sequence() {
        let mut buf = record(libc::IN_CREATE, "event9", 16);
        buf.extend(record(libc::IN_ATTRIB, "event11", 16));
        assert_eq!(
            parse_inotify(&buf),
            vec![Inotify::Name(b"event9"), Inotify::Name(b"event11")]
        );
    }

    #[test]
    fn parse_inotify_reports_overflow_and_skips_nameless() {
        let mut buf = record(libc::IN_ATTRIB, "", 0); // the directory itself
        buf.extend(record(libc::IN_Q_OVERFLOW, "", 0));
        assert_eq!(parse_inotify(&buf), vec![Inotify::Overflow]);
    }

    #[test]
    fn parse_inotify_ignores_truncated_tail() {
        let mut buf = record(libc::IN_CREATE, "event3", 16);
        buf.extend_from_slice(&[0u8; 5]); // less than a header
        assert_eq!(parse_inotify(&buf), vec![Inotify::Name(b"event3")]);
    }

    // `capabilities/key` strings captured from real devices (64-bit kernel).
    const SYSFS_KEYBOARD: &str = "1000000000007 ff800000000007ff febeffdfffefffff fffffffffffffffe";
    const SYSFS_MOUSE: &str = "1f0000 0 0 0 0"; // BTN_LEFT..BTN_EXTRA
    const SYSFS_JOYSTICK: &str = "ffffffffffffffff 0 0 0 0 0 0 ffff00000000 0 0 0 0";

    #[test]
    fn sysfs_bitmap_places_words_least_significant_last() {
        let mouse = parse_sysfs_bitmap(SYSFS_MOUSE);
        for code in 0x110..=0x114 {
            assert!(bit_set(&mouse, code), "BTN 0x{code:x}");
        }
        assert!(!bit_set(&mouse, 0x115));
        assert!(!bit_set(&mouse, KEY_A));

        let joystick = parse_sysfs_bitmap(SYSFS_JOYSTICK);
        assert!(bit_set(&joystick, 0x120)); // BTN_TRIGGER
        assert!(bit_set(&joystick, 0x2c0)); // BTN_TRIGGER_HAPPY1
        assert!(!bit_set(&joystick, 0x110));
    }

    #[test]
    fn sysfs_bitmap_classifies_keyboards_and_thumb_button_mice_only() {
        assert!(is_bindable(&parse_sysfs_bitmap(SYSFS_KEYBOARD)));
        assert!(is_bindable(&parse_sysfs_bitmap(SYSFS_MOUSE)));
        assert!(!is_bindable(&parse_sysfs_bitmap(SYSFS_JOYSTICK)));
        assert!(!is_bindable(&parse_sysfs_bitmap("0"))); // pedals: no keys
        assert!(!is_bindable(&parse_sysfs_bitmap("70000 0 0 0 0"))); // 3-button mouse
    }

    #[test]
    fn sysfs_bitmap_ignores_junk_and_bits_past_key_max() {
        let words = vec!["ffffffffffffffff"; 20].join(" ");
        let wide = parse_sysfs_bitmap(&words);
        assert!(bit_set(&wide, KEY_MAX as u16));
        assert!(!is_bindable(&parse_sysfs_bitmap("zz\n")));
    }
}
