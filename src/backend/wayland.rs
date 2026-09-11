//! Wayland backend.
//!
//! Injection uses an in-process uinput virtual keyboard (`uinput.rs`) — real
//! kernel input events the compositor routes to the focused surface, which is
//! the only compositor-agnostic way to synthesize input on Wayland (XTEST does
//! not reach native Wayland windows). Clipboard uses `wl-clipboard`
//! (`wl-copy`/`wl-paste`).
//!
//! Implemented:
//!   * PasteText        — wl-copy the text, then uinput Ctrl+V
//!   * SimulateKeyPress — VK -> evdev (keymap.rs) -> uinput chord
//!   * GetSelectedText  — copy-probe: save clipboard, uinput Ctrl+C, read, restore
//!   * GetAccessibilityStatus — true when uinput is usable
//!
//! Best-effort / known gaps (Wayland exposes almost nothing here without
//! compositor-specific D-Bus/portal calls):
//!   * GetActiveAppInfo / GetRunningApps — no portable protocol; returns empty.
//!     KDE could be wired via KWin D-Bus and GNOME via a shell extension later.
//!   * Held-modifier snapshot/release (the Windows helper's GetKeyState dance)
//!     needs reading /dev/input; not done yet (TODO).
//!   * Simultaneous text/plain + text/html clipboard offer (wl-copy sets one
//!     payload); we offer plain text, which is what paste targets read (TODO).

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::uinput::UInput;
use super::{ActiveApp, Backend, Result, RunningApp, Selection};
use crate::keymap;

/// Wayland injection + clipboard + selection. Active-app identity is **not**
/// handled here: `detect()` selects an `ActiveAppProvider` (KWin / GNOME ext /
/// AT-SPI) independently and composes it on top, so this backend stays a pure,
/// compositor-agnostic injector. The active-app methods below therefore return
/// empty / no-op (the composing layer supplies identity from the provider).
pub struct WaylandBackend {
    uinput: UInput,
    has_wl_copy: bool,
    has_wl_paste: bool,
    swap_left_alt_control: bool,
}

impl WaylandBackend {
    pub fn connect() -> Result<WaylandBackend> {
        let uinput = UInput::create()?;
        let has_wl_copy = which("wl-copy");
        let has_wl_paste = which("wl-paste");
        // Read once per helper start; restart after changing keyboard options.
        let swap_left_alt_control = super::is_gnome()
            && run_capture(
                "gsettings",
                &["get", "org.gnome.desktop.input-sources", "xkb-options"],
            )
            .map(|options| options.split('\'').any(|s| s == "ctrl:swap_lalt_lctl"))
            .unwrap_or(false);
        if !has_wl_copy || !has_wl_paste {
            log::warn!("wl-clipboard not fully present (wl-copy={has_wl_copy}, wl-paste={has_wl_paste}) — \
                        PasteText/selection ops need both; install `wl-clipboard`");
        }
        Ok(WaylandBackend {
            uinput,
            has_wl_copy,
            has_wl_paste,
            swap_left_alt_control,
        })
    }

    /// Set the clipboard, offering plain text plus (optionally) HTML. Prefers the
    /// in-process ext-data-control owner (a single source advertising both MIME
    /// types, like the Windows helper's dual CF_*); falls back to `wl-copy`
    /// (plain text only) if the protocol isn't available.
    fn clipboard_set_rich(&self, text: &str, html: Option<&str>) -> Result<()> {
        let mut offers: Vec<super::wl_clipboard::Offer> = vec![(
            "text/plain;charset=utf-8".to_string(),
            text.as_bytes().to_vec(),
        )];
        if let Some(h) = html {
            offers.push(("text/html".to_string(), h.as_bytes().to_vec()));
        }
        match super::wl_clipboard::set_clipboard(offers) {
            Ok(()) => Ok(()),
            Err(e) => {
                if self.has_wl_copy {
                    log::warn!("in-process clipboard unavailable ({e}); falling back to wl-copy (plain text only)");
                    run_with_stdin("wl-copy", &["--type", "text/plain;charset=utf-8"], text)
                } else {
                    Err(format!(
                        "clipboard set failed ({e}) and no wl-copy fallback"
                    ))
                }
            }
        }
    }

    fn clipboard_set(&self, text: &str) -> Result<()> {
        self.clipboard_set_rich(text, None)
    }

    fn clipboard_get(&self) -> Result<String> {
        if !self.has_wl_paste {
            return Err("wl-paste not available".into());
        }
        // -n: don't append a trailing newline. Empty clipboard => exit 1; treat as "".
        Ok(run_capture("wl-paste", &["-n"]).unwrap_or_default())
    }

    fn chord(&mut self, key: u16, mods: &[u16]) -> Result<()> {
        let map = |key| remap_modifier(key, self.swap_left_alt_control);
        self.uinput
            .chord(map(key), &mods.iter().copied().map(map).collect::<Vec<_>>())
    }
}

// ponytail: handles GNOME's left Alt/Ctrl swap; use the compositor keymap
// when supporting additional remaps. Physical held-key snapshots stay untouched.
fn remap_modifier(key: u16, swap_left_alt_control: bool) -> u16 {
    match (key, swap_left_alt_control) {
        (29, true) => 56, // logical Control -> physical Left Alt
        (56, true) => 29, // logical Alt -> physical Left Control
        _ => key,
    }
}

impl Backend for WaylandBackend {
    fn paste_text(&mut self, text: &str, html: Option<&str>) -> Result<()> {
        // Offer text/plain (+ text/html when supplied), like the Windows helper's
        // dual-format clipboard, then synthesize Ctrl+V.
        self.clipboard_set_rich(text, html)?;
        // Let the new selection owner register before Ctrl+V reads it.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let ctrl = keymap::flag_to_evdev("Control").unwrap();
        let v = keymap::vk_to_evdev(b'V' as u32).ok_or("no evdev for V")?;
        self.chord(v, &[ctrl])
    }

    fn simulate_key_press(&mut self, keycode_vk: u32, flags: &[String]) -> Result<()> {
        let key = keymap::vk_to_evdev(keycode_vk)
            .ok_or_else(|| format!("unmapped VK {keycode_vk} (no evdev code)"))?;
        let mods: Vec<u16> = flags
            .iter()
            .filter_map(|f| keymap::flag_to_evdev(f))
            .collect();
        // TODO: snapshot & release physically-held modifiers around injection
        // (mirrors the Windows helper's GetKeyState dance); needs /dev/input read.
        self.chord(key, &mods)
    }

    fn get_active_app(&mut self) -> Result<ActiveApp> {
        // No native active-app source on Wayland; the composing `ActiveAppProvider`
        // (KWin / GNOME ext / AT-SPI, chosen in `detect()`) supplies identity.
        Ok(ActiveApp::default())
    }

    fn get_running_apps(&mut self) -> Result<Vec<RunningApp>> {
        // See `get_active_app`: supplied by the composing provider.
        Ok(Vec::new())
    }

    fn get_selected_text(&mut self) -> Result<Selection> {
        // Prefer the non-destructive AT-SPI2 Text-interface read (no keystroke,
        // no clipboard churn). Fall back to the copy-probe below when the a11y
        // bus is unreachable or the focused object exposes no selection.
        match super::atspi_sel::get_selection() {
            Ok(Some(sel)) => return Ok(sel),
            Ok(None) => log::debug!("AT-SPI: no selection, falling back to copy-probe"),
            Err(e) => log::debug!("AT-SPI unavailable ({e}); falling back to copy-probe"),
        }
        // Copy-probe fallback: save clipboard, synth Ctrl+C, read, restore.
        // Approximate and destructive-then-restored.
        let saved = self.clipboard_get().unwrap_or_default();
        let ctrl = keymap::flag_to_evdev("Control").unwrap();
        let c = keymap::vk_to_evdev(b'C' as u32).ok_or("no evdev for C")?;
        self.chord(c, &[ctrl])?;
        std::thread::sleep(std::time::Duration::from_millis(80));
        let selected = self.clipboard_get().unwrap_or_default();
        let _ = self.clipboard_set(&saved);
        Ok(Selection {
            selected_text: selected.clone(),
            contents: selected,
            before_text: String::new(),
            after_text: String::new(),
        })
    }

    fn accessibility_status(&mut self) -> bool {
        // We hold a live uinput device => injection works. (Reading selection via
        // AT-SPI is a separate capability tracked under get_selected_text.)
        true
    }

    fn set_focus_detection(&mut self, _active: bool) {
        // Focus events come from the composing `ActiveAppProvider`, not here.
    }

    fn name(&self) -> &'static str {
        "wayland"
    }
}

/// Heuristic: is this a KDE Plasma session (so the KWin scripting bridge is
/// worth trying)? Checks the standard desktop-environment env vars. Used by
/// `mod.rs::pick_active_app_provider`.
pub(crate) fn is_kde() -> bool {
    let probe = |k: &str| std::env::var(k).unwrap_or_default().to_ascii_uppercase();
    probe("XDG_CURRENT_DESKTOP").contains("KDE")
        || probe("XDG_SESSION_DESKTOP").contains("KDE")
        || std::env::var_os("KDE_FULL_SESSION").is_some()
}

fn which(prog: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(prog).is_file()))
        .unwrap_or(false)
}

fn run_with_stdin(prog: &str, args: &[&str], input: &str) -> Result<()> {
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {prog}: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(input.as_bytes())
        .map_err(|e| format!("write {prog} stdin: {e}"))?;
    let status = child.wait().map_err(|e| format!("wait {prog}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{prog} exited with {status}"))
    }
}

/// Run `prog args`, capturing stdout, but never block the (single-threaded)
/// dispatch loop forever: if the child doesn't finish within `CAPTURE_TIMEOUT`
/// it's killed and we return an error. `wl-paste` can hang on some compositors
/// (e.g. mutter has no ext/wlr-data-control and a misbehaving selection owner
/// can stall the read), which would otherwise freeze every subsequent command.
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(1500);

fn run_capture(prog: &str, args: &[&str]) -> Result<String> {
    let mut child = Command::new(prog)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {prog}: {e}"))?;

    // Drain stdout on a helper thread so a child that fills the pipe can't
    // deadlock; killing the child on timeout closes the pipe and unblocks it.
    let mut stdout = child.stdout.take().ok_or("no stdout pipe")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + CAPTURE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let buf = rx
                    .recv_timeout(Duration::from_millis(200))
                    .unwrap_or_default();
                if !status.success() {
                    return Err(format!("{prog} exited with {status}"));
                }
                return Ok(String::from_utf8_lossy(&buf).into_owned());
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{prog} timed out after {CAPTURE_TIMEOUT:?}"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("wait {prog}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::remap_modifier;
    use crate::keymap;

    #[test]
    fn left_alt_control_swap_preserves_other_keys_and_default_mapping() {
        let ctrl = keymap::flag_to_evdev("Control").unwrap();
        let alt = keymap::flag_to_evdev("Alt").unwrap();
        assert_eq!(remap_modifier(ctrl, true), alt);
        assert_eq!(remap_modifier(alt, true), ctrl);
        for key in 0..=0x2ff {
            assert_eq!(remap_modifier(key, false), key);
            if key != ctrl && key != alt {
                assert_eq!(remap_modifier(key, true), key);
            }
        }
    }
}
