//! In-process uinput virtual keyboard (Wayland injection primitive).
//!
//! On Wayland there is no portable X11-style synthetic-input API: XTEST events
//! don't reach native Wayland surfaces. The reliable, compositor-agnostic path
//! is to create a *real* virtual input device via `/dev/uinput` and write kernel
//! key events to it — the compositor sees them as ordinary hardware input and
//! routes them to the focused surface like any keyboard.
//!
//! This is what `ydotool` does, but in-process: we don't shell out to `ydotool`
//! (which needs its `ydotoold` daemon running). We just need write access to
//! `/dev/uinput` (typically granted to the active-session user via a logind
//! `uaccess` udev rule / ACL; otherwise the `uinput` group or root).
//!
//! Codes written here are Linux evdev `KEY_*` codes (see `keymap::vk_to_evdev`),
//! NOT X11 keysyms.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

use super::Result;
use crate::keymap;

// --- evdev event types (<linux/input-event-codes.h>) ---
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_REPORT: u16 = 0x00;

// --- uinput ioctls (<linux/uinput.h>), x86_64 encodings ---
//   UI_DEV_CREATE   = _IO('U', 1)            = 0x5501
//   UI_DEV_DESTROY  = _IO('U', 2)            = 0x5502
//   UI_SET_EVBIT    = _IOW('U', 100, int)    = 0x40045564
//   UI_SET_KEYBIT   = _IOW('U', 101, int)    = 0x40045565
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_SET_EVBIT: libc::c_ulong = 0x40045564;
const UI_SET_KEYBIT: libc::c_ulong = 0x40045565;

const BUS_USB: u16 = 0x03;
/// We enable the full standard key range so any mapped VK can be injected.
const KEY_MAX: u16 = 0x2ff;

/// Name this virtual keyboard reports to the kernel (`EVIOCGNAME`). Also read by
/// `capture::evdev`, which must never watch our own device: it rescans
/// `/dev/input` for hot-plugged keyboards, and this device is created *after*
/// capture starts, so only the name distinguishes it. Reading it back would feed
/// every injected keystroke to the app as a `KeypressEvent`.
pub const DEVICE_NAME: &str = "Wispr Flow Linux Helper";

pub struct UInput {
    file: File,
}

impl UInput {
    /// Probe whether `/dev/uinput` is openable for writing without creating a
    /// device (used by backend detection so we can fall back gracefully).
    pub fn available() -> bool {
        OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")
            .is_ok()
    }

    /// Create the virtual keyboard. Must be kept alive for the process lifetime;
    /// dropping it destroys the device.
    pub fn create() -> Result<UInput> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")
            .map_err(|e| format!("open /dev/uinput: {e} (need write access — logind uaccess ACL, `uinput` group, or root)"))?;
        let fd = file.as_raw_fd();

        // Declare the event types and key range this device emits.
        ioctl_set(fd, UI_SET_EVBIT, EV_KEY as libc::c_int)?;
        ioctl_set(fd, UI_SET_EVBIT, EV_SYN as libc::c_int)?;
        for code in 1..=KEY_MAX {
            // Best-effort: a few codes in the range are gaps; ignore EINVAL.
            unsafe { libc::ioctl(fd, UI_SET_KEYBIT, code as libc::c_int) };
        }

        // Legacy device-setup path (write a uinput_user_dev, then UI_DEV_CREATE):
        // widely supported and avoids the newer UI_DEV_SETUP/abs_setup structs.
        let mut dev: libc::uinput_user_dev = unsafe { std::mem::zeroed() };
        let name = DEVICE_NAME.as_bytes();
        for (i, &b) in name.iter().enumerate() {
            dev.name[i] = b as libc::c_char;
        }
        dev.id.bustype = BUS_USB;
        dev.id.vendor = 0x1234;
        dev.id.product = 0x5678;
        dev.id.version = 1;

        let bytes = unsafe {
            std::slice::from_raw_parts(
                &dev as *const _ as *const u8,
                std::mem::size_of::<libc::uinput_user_dev>(),
            )
        };
        (&file)
            .write_all(bytes)
            .map_err(|e| format!("write uinput_user_dev: {e}"))?;

        if unsafe { libc::ioctl(fd, UI_DEV_CREATE) } < 0 {
            return Err(format!(
                "UI_DEV_CREATE: {}",
                std::io::Error::last_os_error()
            ));
        }

        // The compositor needs a moment to enumerate the new device before it
        // will route events from it; injecting too early drops the first keys.
        std::thread::sleep(std::time::Duration::from_millis(200));

        Ok(UInput { file })
    }

    fn emit(&mut self, type_: u16, code: u16, value: i32) -> Result<()> {
        let ev = libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const _ as *const u8,
                std::mem::size_of::<libc::input_event>(),
            )
        };
        self.file
            .write_all(bytes)
            .map_err(|e| format!("uinput write: {e}"))
    }

    fn syn(&mut self) -> Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    /// Press (value=1) or release (value=0) a single evdev key, with a SYN.
    pub fn key(&mut self, code: u16, press: bool) -> Result<()> {
        self.emit(EV_KEY, code, if press { 1 } else { 0 })?;
        self.syn()
    }

    /// Press a chord: hold `mods` (in order), tap `key`, release everything in
    /// reverse.
    ///
    /// CRITICAL: the modifier-down → key-down → key-up → modifier-up events are
    /// emitted as one *contiguous* batch with **no inter-event sleep**. On
    /// KWin/Wayland a quiescent gap after a virtual modifier-down causes the
    /// compositor to drop the modifier before the key arrives, so an injected
    /// Ctrl+V degrades to a bare `v` (the entire paste path silently failed this
    /// way). Counter-intuitively, an "observe the modifier" delay here is the
    /// bug, not the fix — verified: 0 ms → modifier applied, ≥8 ms → dropped.
    /// See docs/learnings/wayland-injection.md.
    ///
    /// Mirrors the Windows helper's GetKeyState dance: any modifier the user is
    /// *physically* holding at injection time is released first and restored
    /// afterwards, so e.g. a held Ctrl doesn't turn our injected `v` into a
    /// stray Ctrl+V (or our injected Ctrl+V into Ctrl+Shift+V). When
    /// `/dev/input` isn't readable (no `input` group / uaccess ACL), the held
    /// set is empty and this degrades to a plain chord — see [`held_modifiers`].
    pub fn chord(&mut self, key: u16, mods: &[u16]) -> Result<()> {
        let held = held_modifiers();
        for &m in &held {
            let _ = self.key(m, false);
        }
        for &m in mods {
            self.key(m, true)?;
        }
        self.key(key, true)?;
        self.key(key, false)?;
        for &m in mods.iter().rev() {
            self.key(m, false)?;
        }
        // Restore physically-held modifiers (reverse order). Best-effort.
        for &m in held.iter().rev() {
            let _ = self.key(m, true);
        }
        Ok(())
    }
}

/// Snapshot the modifier keys the user is *physically* holding right now, by
/// querying every readable `/dev/input/event*` device with `EVIOCGKEY` (a
/// bitmap of currently-pressed keycodes) and intersecting with the modifier set.
///
/// Returns an empty list — and is therefore a no-op for `chord` — when no event
/// device is readable. Reading `/dev/input` typically needs the `input` group
/// or the logind `uaccess` ACL; on sessions without it, the snapshot is simply
/// skipped (the common case at paste time has no modifier held anyway).
pub fn held_modifiers() -> Vec<u16> {
    use std::collections::BTreeSet;
    // EVIOCGKEY(len) = _IOC(_IOC_READ=2, 'E'=0x45, 0x18, len). KEY_MAX=0x2ff ->
    // a 96-byte bitmap covers every keycode we care about.
    const BITMAP_LEN: usize = (KEY_MAX as usize / 8) + 1;
    let req: libc::c_ulong =
        ((2u64 << 30) | ((BITMAP_LEN as u64) << 16) | (0x45 << 8) | 0x18) as libc::c_ulong;

    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut held = BTreeSet::new();
    for entry in dir.flatten() {
        let path = entry.path();
        let is_event = path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event {
            continue;
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(f) => f,
            Err(_) => continue, // not readable -> skip this device
        };
        let mut bitmap = [0u8; BITMAP_LEN];
        let r = unsafe { libc::ioctl(file.as_raw_fd(), req, bitmap.as_mut_ptr()) };
        if r < 0 {
            continue;
        }
        for &m in keymap::EVDEV_MODIFIERS {
            let (byte, bit) = (m as usize / 8, m as u32 % 8);
            if byte < bitmap.len() && (bitmap[byte] >> bit) & 1 == 1 {
                held.insert(m);
            }
        }
    }
    held.into_iter().collect()
}

impl Drop for UInput {
    fn drop(&mut self) {
        unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY) };
    }
}

fn ioctl_set(fd: libc::c_int, req: libc::c_ulong, arg: libc::c_int) -> Result<()> {
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        return Err(format!(
            "ioctl {req:#x}({arg}): {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}
