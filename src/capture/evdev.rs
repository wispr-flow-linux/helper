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
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

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

/// Open every readable keyboard under `/dev/input`, returning `(path, file)`
/// pairs with blocking fds ready for `read`. Empty when none are readable.
fn open_keyboards() -> Vec<(PathBuf, File)> {
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
        if !is_event_node(&path) {
            continue;
        }
        // Blocking fd: open() never blocks on evdev, but read() must, so the
        // per-device thread can park until the next key event.
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(f) => f,
            Err(_) => continue, // not readable -> skip (permission or busy)
        };
        if is_keyboard(file.as_raw_fd()) {
            out.push((path, file));
        }
    }
    out
}

/// Start evdev capture: one reader thread per keyboard. Returns a [`HeldKeys`]
/// handle, or `None` when no device is readable (so the caller can fall back).
pub fn start(events: EventSink) -> Option<Box<dyn HeldKeys>> {
    let keyboards = open_keyboards();
    if keyboards.is_empty() {
        log::warn!(
            "evdev capture: no readable keyboard under /dev/input — push-to-talk \
             and the in-app shortcut recorder will NOT work. Run \
             `wispr-flow --install-udev-rules`, or add the user to the `input` \
             group (then re-login)."
        );
        return None;
    }
    let index = Arc::new(AtomicU64::new(0));
    let pid = std::process::id();
    let mut held = Vec::with_capacity(keyboards.len());
    for (path, file) in keyboards {
        log::info!("evdev capture: watching {}", path.display());
        // Keep a dup of the reader's fd so `CheckStaleKeys` costs one ioctl per
        // keyboard instead of re-opening every /dev/input node per query.
        match file.try_clone() {
            Ok(dup) => held.push(dup),
            Err(e) => log::warn!(
                "evdev capture: cannot dup {} for stale-key queries: {e}",
                path.display()
            ),
        }
        let events = events.clone();
        let index = index.clone();
        let builder = std::thread::Builder::new().name("key-capture-evdev".to_string());
        if let Err(e) = builder.spawn(move || read_device(&path, file, &events, &index, pid)) {
            log::warn!("evdev capture: failed to spawn reader thread: {e}");
        }
    }
    Some(Box::new(EvdevHeld { keyboards: held }))
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

/// Stale-key querier backed by `EVIOCGKEY` on the captured keyboards.
///
/// It holds dups of the reader threads' fds rather than re-opening
/// `/dev/input/event*` per query: `open()` on an idle evdev node can wake its
/// device, and across a typical desktop's switch/audio-jack/HDMI nodes a full
/// re-scan takes ~0.5 s — past the app's 500 ms `CheckStaleKeys` deadline, so
/// every reply arrived late and stale-key recovery never ran. Only keyboards
/// emit `KeypressEvent`s, so they are the only keys the app can ask about.
struct EvdevHeld {
    keyboards: Vec<File>,
}

impl HeldKeys for EvdevHeld {
    fn held_vks(&self) -> HashSet<u32> {
        let mut held = HashSet::new();
        for file in &self.keyboards {
            let mut bitmap = [0u8; BITMAP_LEN];
            // Fails (ENODEV) once the keyboard is unplugged; it then holds nothing.
            if unsafe { libc::ioctl(file.as_raw_fd(), eviocgkey(), bitmap.as_mut_ptr()) } < 0 {
                continue;
            }
            held.extend(held_vks_in(&bitmap));
        }
        held
    }
}

/// VK codes of the keys set in an `EVIOCGKEY` state bitmap (unmapped codes skipped).
fn held_vks_in(bitmap: &[u8]) -> impl Iterator<Item = u32> + '_ {
    (0..=KEY_MAX as u16)
        .filter(|&code| bit_set(bitmap, code))
        .filter_map(keymap::evdev_to_vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap_with(codes: &[u16]) -> [u8; BITMAP_LEN] {
        let mut bitmap = [0u8; BITMAP_LEN];
        for &code in codes {
            bitmap[code as usize / 8] |= 1 << (code % 8);
        }
        bitmap
    }

    #[test]
    fn held_vks_in_maps_set_bits_to_vk_codes() {
        // KEY_A (30) -> VK 'A' (65), KEY_LEFTCTRL (29) -> VK_LCONTROL (162).
        let bitmap = bitmap_with(&[KEY_A, 29]);
        let mut held: Vec<u32> = held_vks_in(&bitmap).collect();
        held.sort_unstable();
        assert_eq!(held, vec![65, 162]);
    }

    #[test]
    fn held_vks_in_is_empty_for_a_clear_bitmap() {
        assert_eq!(held_vks_in(&[0u8; BITMAP_LEN]).count(), 0);
    }
}
