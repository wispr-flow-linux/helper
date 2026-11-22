//! XKB option and layout resolver for Wayland input synthesis.
//!
//! On Wayland, synthetic keystrokes are injected via `/dev/uinput` as raw evdev
//! scancodes. Compositors (such as Mutter / GNOME Shell) attach an XKB keymap
//! seat-wide to all input devices, including virtual uinput keyboards.
//!
//! When the user has configured XKB options (e.g. `ctrl:swap_lalt_lctl`,
//! `ctrl:swap_lwin_lctl`, `ctrl:swapcaps`, `altwin:swap_alt_win`) or layouts
//! like Dvorak, injecting hardcoded scancodes (like evdev 29 for Control or 47
//! for 'V') produces the swapped or shifted keysym in the focused application
//! (e.g. Alt+V instead of Ctrl+V).
//!
//! `XkbResolver` discovers the active session XKB options and layout, and maps
//! modifier flags and paste/copy keys to the physical evdev codes that the
//! compositor translates into the desired keysyms.

use std::fs;
use std::path::Path;
use std::process::Command;

// Standard Linux evdev codes (linux/input-event-codes.h)
pub const EVDEV_KEY_LEFTCTRL: u16 = 29;
pub const EVDEV_KEY_LEFTSHIFT: u16 = 42;
pub const EVDEV_KEY_LEFTALT: u16 = 56;
pub const EVDEV_KEY_CAPSLOCK: u16 = 58;
pub const EVDEV_KEY_LEFTMETA: u16 = 125;

pub const EVDEV_KEY_C: u16 = 46;
pub const EVDEV_KEY_V: u16 = 47;
pub const EVDEV_KEY_I: u16 = 31;
pub const EVDEV_KEY_DOT: u16 = 52;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XkbResolver {
    pub options: Vec<String>,
    pub layout: String,
    ctrl_evdev: u16,
    alt_evdev: u16,
    meta_evdev: u16,
    paste_v_evdev: u16,
    copy_c_evdev: u16,
}

impl XkbResolver {
    pub fn from_options_and_layout(options: Vec<String>, layout: String) -> Self {
        let mut ctrl_evdev = EVDEV_KEY_LEFTCTRL;
        let mut alt_evdev = EVDEV_KEY_LEFTALT;
        let mut meta_evdev = EVDEV_KEY_LEFTMETA;

        for opt in &options {
            match opt.trim() {
                "ctrl:swap_lalt_lctl" => {
                    ctrl_evdev = EVDEV_KEY_LEFTALT;
                    alt_evdev = EVDEV_KEY_LEFTCTRL;
                }
                "ctrl:swap_lwin_lctl" => {
                    ctrl_evdev = EVDEV_KEY_LEFTMETA;
                    meta_evdev = EVDEV_KEY_LEFTCTRL;
                }
                "ctrl:swapcaps" => {
                    ctrl_evdev = EVDEV_KEY_CAPSLOCK;
                }
                "altwin:swap_alt_win" | "altwin:swap_lalt_lwin" => {
                    alt_evdev = EVDEV_KEY_LEFTMETA;
                    meta_evdev = EVDEV_KEY_LEFTALT;
                }
                _ => {}
            }
        }

        let is_dvorak = layout.to_ascii_lowercase().contains("dvorak");
        let paste_v_evdev = if is_dvorak {
            EVDEV_KEY_DOT
        } else {
            EVDEV_KEY_V
        };
        let copy_c_evdev = if is_dvorak { EVDEV_KEY_I } else { EVDEV_KEY_C };

        Self {
            options,
            layout,
            ctrl_evdev,
            alt_evdev,
            meta_evdev,
            paste_v_evdev,
            copy_c_evdev,
        }
    }

    pub fn modifier_to_evdev(&self, flag: &str) -> Option<u16> {
        Some(match flag {
            "Control" => self.ctrl_evdev,
            "Alt" => self.alt_evdev,
            "Command" | "Meta" | "Super" => self.meta_evdev,
            "Shift" => EVDEV_KEY_LEFTSHIFT,
            _ => return None,
        })
    }

    pub fn paste_v_key(&self) -> u16 {
        self.paste_v_evdev
    }

    pub fn copy_c_key(&self) -> u16 {
        self.copy_c_evdev
    }

    pub fn ctrl_evdev(&self) -> u16 {
        self.ctrl_evdev
    }

    pub fn alt_evdev(&self) -> u16 {
        self.alt_evdev
    }

    pub fn meta_evdev(&self) -> u16 {
        self.meta_evdev
    }

    pub fn detect() -> Self {
        let (env_options, env_layout) = Self::detect_from_env();
        let (desktop_options, desktop_layout) = if env_options.is_empty() {
            Self::detect_from_desktop()
        } else {
            (Vec::new(), String::new())
        };

        let mut options = env_options;
        if options.is_empty() {
            options = desktop_options;
        }

        let mut layout = env_layout;
        if layout.is_empty() {
            layout = desktop_layout;
        }

        // System fallback if still empty
        if options.is_empty() && layout.is_empty() {
            let (sys_opts, sys_layout) = Self::detect_etc_keyboard();
            if options.is_empty() {
                options = sys_opts;
            }
            if layout.is_empty() {
                layout = sys_layout;
            }
        }

        Self::from_options_and_layout(options, layout)
    }

    fn detect_from_env() -> (Vec<String>, String) {
        let mut options = Vec::new();

        if let Ok(v) = std::env::var("WISPR_XKB_OPTIONS") {
            for part in v.split(&[',', ' '][..]) {
                let p = part.trim();
                if !p.is_empty() {
                    options.push(p.to_string());
                }
            }
        }

        if let Ok(v) = std::env::var("WISPR_SWAP_LALT_LCTL") {
            let val = v.trim().to_ascii_lowercase();
            if (val == "1" || val == "true" || val == "yes")
                && !options.iter().any(|o| o == "ctrl:swap_lalt_lctl")
            {
                options.push("ctrl:swap_lalt_lctl".to_string());
            }
        }

        if options.is_empty() {
            if let Ok(v) = std::env::var("XKB_DEFAULT_OPTIONS") {
                for part in v.split(',') {
                    let p = part.trim();
                    if !p.is_empty() {
                        options.push(p.to_string());
                    }
                }
            }
        }

        let layout = std::env::var("XKB_DEFAULT_LAYOUT").unwrap_or_default();

        (options, layout)
    }

    fn detect_from_desktop() -> (Vec<String>, String) {
        let (gnome_opts, gnome_layout) = Self::detect_gnome();
        if !gnome_opts.is_empty() || !gnome_layout.is_empty() {
            return (gnome_opts, gnome_layout);
        }

        Self::detect_kde()
    }

    fn detect_gnome() -> (Vec<String>, String) {
        let mut options = Vec::new();
        let mut layout = String::new();

        if let Ok(output) = Command::new("gsettings")
            .args(["get", "org.gnome.desktop.input-sources", "xkb-options"])
            .output()
        {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                options = parse_gsettings_list(&raw);
            }
        }

        if let Ok(output) = Command::new("gsettings")
            .args(["get", "org.gnome.desktop.input-sources", "sources"])
            .output()
        {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                layout = parse_gsettings_sources(&raw);
            }
        }

        (options, layout)
    }

    fn detect_kde() -> (Vec<String>, String) {
        let mut options = Vec::new();
        let mut layout = String::new();

        if let Some(home) = std::env::var_os("HOME") {
            let path = Path::new(&home).join(".config/kxkbrc");
            if let Ok(content) = fs::read_to_string(path) {
                let (opts, lay) = parse_kxkbrc(&content);
                options = opts;
                layout = lay;
            }
        }

        (options, layout)
    }

    fn detect_etc_keyboard() -> (Vec<String>, String) {
        if let Ok(content) = fs::read_to_string("/etc/default/keyboard") {
            parse_etc_default_keyboard(&content)
        } else {
            (Vec::new(), String::new())
        }
    }
}

impl Default for XkbResolver {
    fn default() -> Self {
        Self::from_options_and_layout(Vec::new(), String::new())
    }
}

fn parse_gsettings_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let s = trimmed.strip_prefix("@as ").unwrap_or(trimmed).trim();
    if !s.starts_with('[') || !s.ends_with(']') {
        return Vec::new();
    }
    let inner = &s[1..s.len() - 1];
    inner
        .split(',')
        .map(|item| item.trim().trim_matches('\'').trim_matches('"').trim())
        .filter(|item| !item.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn parse_gsettings_sources(raw: &str) -> String {
    let mut layouts = Vec::new();
    let parts: Vec<&str> = raw.split(&['\'', '"'][..]).collect();
    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 1 {
            let p = part.trim();
            if p != "xkb" && !p.is_empty() {
                layouts.push(p);
            }
        }
    }
    layouts.join(",")
}

fn parse_kxkbrc(content: &str) -> (Vec<String>, String) {
    let mut options = Vec::new();
    let mut layout = String::new();
    let mut in_layout_section = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_layout_section = trimmed.eq_ignore_ascii_case("[Layout]");
            continue;
        }
        if !in_layout_section {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            match k.trim() {
                "Options" => {
                    for opt in v.split(',') {
                        let o = opt.trim();
                        if !o.is_empty() {
                            options.push(o.to_string());
                        }
                    }
                }
                "LayoutList" | "Layout" if layout.is_empty() => {
                    layout = v.trim().to_string();
                }
                _ => {}
            }
        }
    }
    (options, layout)
}

fn parse_etc_default_keyboard(content: &str) -> (Vec<String>, String) {
    let mut options = Vec::new();
    let mut layout = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let val = v.trim().trim_matches('"').trim_matches('\'').trim();
            match k.trim() {
                "XKBOPTIONS" => {
                    for opt in val.split(',') {
                        let o = opt.trim();
                        if !o.is_empty() {
                            options.push(o.to_string());
                        }
                    }
                }
                "XKBLAYOUT" => {
                    layout = val.to_string();
                }
                _ => {}
            }
        }
    }
    (options, layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mapping_returns_standard_evdev_codes() {
        let res = XkbResolver::default();
        assert_eq!(res.modifier_to_evdev("Control"), Some(EVDEV_KEY_LEFTCTRL));
        assert_eq!(res.modifier_to_evdev("Alt"), Some(EVDEV_KEY_LEFTALT));
        assert_eq!(res.modifier_to_evdev("Super"), Some(EVDEV_KEY_LEFTMETA));
        assert_eq!(res.modifier_to_evdev("Shift"), Some(EVDEV_KEY_LEFTSHIFT));
        assert_eq!(res.paste_v_key(), EVDEV_KEY_V);
        assert_eq!(res.copy_c_key(), EVDEV_KEY_C);
    }

    #[test]
    fn swap_lalt_lctl_swaps_control_and_alt() {
        let res = XkbResolver::from_options_and_layout(
            vec!["ctrl:swap_lalt_lctl".to_string()],
            String::new(),
        );
        assert_eq!(res.modifier_to_evdev("Control"), Some(EVDEV_KEY_LEFTALT));
        assert_eq!(res.modifier_to_evdev("Alt"), Some(EVDEV_KEY_LEFTCTRL));
        assert_eq!(res.modifier_to_evdev("Super"), Some(EVDEV_KEY_LEFTMETA));
    }

    #[test]
    fn swap_lwin_lctl_swaps_control_and_super() {
        let res = XkbResolver::from_options_and_layout(
            vec!["ctrl:swap_lwin_lctl".to_string()],
            String::new(),
        );
        assert_eq!(res.modifier_to_evdev("Control"), Some(EVDEV_KEY_LEFTMETA));
        assert_eq!(res.modifier_to_evdev("Super"), Some(EVDEV_KEY_LEFTCTRL));
        assert_eq!(res.modifier_to_evdev("Alt"), Some(EVDEV_KEY_LEFTALT));
    }

    #[test]
    fn swapcaps_maps_control_to_capslock() {
        let res =
            XkbResolver::from_options_and_layout(vec!["ctrl:swapcaps".to_string()], String::new());
        assert_eq!(res.modifier_to_evdev("Control"), Some(EVDEV_KEY_CAPSLOCK));
    }

    #[test]
    fn altwin_swap_swaps_alt_and_win() {
        let res = XkbResolver::from_options_and_layout(
            vec!["altwin:swap_alt_win".to_string()],
            String::new(),
        );
        assert_eq!(res.modifier_to_evdev("Alt"), Some(EVDEV_KEY_LEFTMETA));
        assert_eq!(res.modifier_to_evdev("Super"), Some(EVDEV_KEY_LEFTALT));
        assert_eq!(res.modifier_to_evdev("Control"), Some(EVDEV_KEY_LEFTCTRL));
    }

    #[test]
    fn dvorak_layout_resolves_paste_and_copy_keys() {
        let res = XkbResolver::from_options_and_layout(Vec::new(), "us(dvorak)".to_string());
        assert_eq!(res.paste_v_key(), EVDEV_KEY_DOT);
        assert_eq!(res.copy_c_key(), EVDEV_KEY_I);
    }

    #[test]
    fn parse_gsettings_list_handles_gvariant_formats() {
        assert_eq!(parse_gsettings_list("@as []"), Vec::<String>::new());
        assert_eq!(parse_gsettings_list("[]"), Vec::<String>::new());
        assert_eq!(
            parse_gsettings_list("['caps:swapescape', 'ctrl:swap_lalt_lctl']"),
            vec![
                "caps:swapescape".to_string(),
                "ctrl:swap_lalt_lctl".to_string()
            ]
        );
    }

    #[test]
    fn parse_gsettings_sources_extracts_layouts() {
        assert_eq!(
            parse_gsettings_sources("[('xkb', 'us+dvorak'), ('xkb', 'in+eng')]"),
            "us+dvorak,in+eng"
        );
    }

    #[test]
    fn parse_kxkbrc_extracts_options_and_layout() {
        let ini = r#"
[Layout]
Options=ctrl:swap_lalt_lctl,caps:swapescape
LayoutList=us,dvorak
"#;
        let (opts, layout) = parse_kxkbrc(ini);
        assert_eq!(
            opts,
            vec![
                "ctrl:swap_lalt_lctl".to_string(),
                "caps:swapescape".to_string()
            ]
        );
        assert_eq!(layout, "us,dvorak");
    }

    #[test]
    fn parse_etc_default_keyboard_extracts_options() {
        let content = r#"
XKBLAYOUT="us,in"
XKBOPTIONS="ctrl:swap_lalt_lctl"
"#;
        let (opts, layout) = parse_etc_default_keyboard(content);
        assert_eq!(opts, vec!["ctrl:swap_lalt_lctl".to_string()]);
        assert_eq!(layout, "us,in");
    }
}
