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

pub struct UInput {
    file: File,
}

trait EventWriter {
    fn write_event(&mut self, type_: u16, code: u16, value: i32) -> Result<()>;
}

impl EventWriter for File {
    fn write_event(&mut self, type_: u16, code: u16, value: i32) -> Result<()> {
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
        self.write_all(bytes)
            .map_err(|e| format!("uinput write: {e}"))
    }
}

fn write_key(writer: &mut impl EventWriter, code: u16, press: bool) -> Result<()> {
    writer.write_event(EV_KEY, code, if press { 1 } else { 0 })?;
    writer.write_event(EV_SYN, SYN_REPORT, 0)
}

fn write_chord(writer: &mut impl EventWriter, key: u16, mods: &[u16], held: &[u16]) -> Result<()> {
    // A physical modifier belongs to another evdev device. A synthetic release
    // can temporarily remove its seat-wide effect while we inject the chord,
    // but pressing it here afterwards would transfer ownership to this virtual
    // device. The later physical release cannot clear that synthetic press.
    for &modifier in held {
        let _ = write_key(writer, modifier, false);
    }

    let mut first_error = None;
    let mut synthetic_modifiers = Vec::new();
    for &modifier in mods {
        synthetic_modifiers.push(modifier);
        if let Err(error) = write_key(writer, modifier, true) {
            first_error = Some(error);
            break;
        }
    }

    let mut key_may_be_pressed = false;
    if first_error.is_none() {
        key_may_be_pressed = true;
        if let Err(error) = write_key(writer, key, true) {
            first_error = Some(error);
        }
    }
    if first_error.is_none() {
        match write_key(writer, key, false) {
            Ok(()) => key_may_be_pressed = false,
            Err(error) => first_error = Some(error),
        }
    }

    // A failed event write can happen after the kernel accepted the preceding
    // key-down. Release every key whose press was attempted, preserving the
    // first operation error while still attempting the complete cleanup.
    if key_may_be_pressed {
        if let Err(error) = write_key(writer, key, false) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    for &modifier in synthetic_modifiers.iter().rev() {
        if let Err(error) = write_key(writer, modifier, false) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
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
        let name = b"Wispr Flow Linux Helper";
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
    /// Any modifier the user is *physically* holding at injection time is
    /// released on the synthetic device before the chord so it cannot corrupt
    /// the injected key. It is deliberately not pressed again on the synthetic
    /// device: that would transfer the modifier to a device whose later release
    /// cannot arrive from the physical keyboard. When `/dev/input` isn't
    /// readable (no `input` group / uaccess ACL), the held set is empty and this
    /// degrades to a plain chord — see [`held_modifiers`].
    pub fn chord(&mut self, key: u16, mods: &[u16]) -> Result<()> {
        let held = held_modifiers();
        write_chord(&mut self.file, key, mods, &held)
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{write_chord, EventWriter, Result, EV_KEY};

    const KEY_LEFTCTRL: u16 = 29;
    const KEY_V: u16 = 47;
    const KEY_LEFTMETA: u16 = 125;

    #[derive(Default)]
    struct RecordingWriter {
        event_index: usize,
        fail_once_at: Option<usize>,
        pressed: BTreeSet<u16>,
    }

    impl EventWriter for RecordingWriter {
        fn write_event(&mut self, type_: u16, code: u16, value: i32) -> Result<()> {
            let current_index = self.event_index;
            self.event_index += 1;
            if self.fail_once_at == Some(current_index) {
                self.fail_once_at = None;
                return Err("injected event write failure".into());
            }
            if type_ == EV_KEY {
                if value == 1 {
                    self.pressed.insert(code);
                } else if value == 0 {
                    self.pressed.remove(&code);
                }
            }
            Ok(())
        }
    }

    #[test]
    fn physically_held_modifier_is_not_left_pressed_on_virtual_device() {
        let mut writer = RecordingWriter::default();

        write_chord(&mut writer, KEY_V, &[KEY_LEFTCTRL], &[KEY_LEFTMETA]).unwrap();

        assert_eq!(writer.pressed, BTreeSet::new());
    }

    #[test]
    fn write_failure_releases_synthetic_modifiers() {
        let mut writer = RecordingWriter {
            fail_once_at: Some(2),
            ..RecordingWriter::default()
        };

        assert!(write_chord(&mut writer, KEY_V, &[KEY_LEFTCTRL], &[]).is_err());

        assert_eq!(writer.pressed, BTreeSet::new());
    }
}
