//! Paste-chord selection: which key chord `PasteText` synthesizes after
//! setting the clipboard.
//!
//! Default is Ctrl+V. Resolution order for each paste:
//!   1. `WISPR_LINUX_HELPER_PASTE_KEYS` env var — force a chord globally
//!      (`ctrl+v` or `ctrl+shift+v`). Always wins when set.
//!   2. Per-app detection — the composing layer (`Composed`) inspects the
//!      focused app right before delegating the paste and records whether it
//!      is a terminal emulator. Terminals get Ctrl+Shift+V (their paste
//!      binding; Ctrl+V is literal-insert in shells), everything else keeps
//!      the stock Ctrl+V so non-terminal apps see zero behavior change.

const PASTE_KEYS_ENV: &str = "WISPR_LINUX_HELPER_PASTE_KEYS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteChord {
    pub key_vk: u32,
    pub flags: &'static [&'static str],
}

const CTRL_V: PasteChord = PasteChord {
    key_vk: b'V' as u32,
    flags: &["Control"],
};

const CTRL_SHIFT_V: PasteChord = PasteChord {
    key_vk: b'V' as u32,
    flags: &["Control", "Shift"],
};

thread_local! {
    /// Per-paste flag recorded by the composing layer after inspecting the
    /// focused app. The request dispatch loop in main.rs is single-threaded,
    /// so a thread-local is race-free: it is set immediately before the
    /// matching `paste_text` delegation and consumed synchronously inside it.
    static FOCUSED_IS_TERMINAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record whether the currently focused app is a terminal emulator. Called by
/// `Composed::paste_text` right before delegating to the injector.
pub fn set_focused_is_terminal(is_terminal: bool) {
    FOCUSED_IS_TERMINAL.with(|c| c.set(is_terminal));
}

/// Resolve the chord to synthesize for this paste.
pub fn chord() -> Result<PasteChord, String> {
    let env = std::env::var(PASTE_KEYS_ENV).unwrap_or_default();
    resolve(&env, FOCUSED_IS_TERMINAL.with(|c| c.get()))
}

/// Pure resolution (separated from env/thread-local reads for testability).
fn resolve(env_value: &str, focused_is_terminal: bool) -> Result<PasteChord, String> {
    if !env_value.trim().is_empty() {
        return parse(env_value); // explicit user override always wins
    }
    Ok(if focused_is_terminal {
        CTRL_SHIFT_V
    } else {
        CTRL_V
    })
}

fn parse(value: &str) -> Result<PasteChord, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "ctrl+v" => Ok(CTRL_V),
        "ctrl+shift+v" => Ok(CTRL_SHIFT_V),
        _ => Err(format!("{PASTE_KEYS_ENV} supports ctrl+v and ctrl+shift+v")),
    }
}

/// True if `app_name` / `bundle_id` identify a terminal emulator. Terminal
/// names are matched only as whole identifiers: the surrounding characters
/// must be non-alphanumeric, so short names such as `warp`, `rio`, and `foot`
/// do not match Warpinator, Prior, or Footnote.
pub fn looks_like_terminal(app_name: &str, bundle_id: &str) -> bool {
    const IDENTIFIERS: &[&str] = &[
        "kitty",
        "alacritty",
        "ghostty",
        "wezterm",
        "konsole",
        "foot",
        "footclient",
        "gnome-terminal",
        "kgx",
        "ptyxis",
        "xterm",
        "uxterm",
        "urxvt",
        "terminator",
        "tilix",
        "yakuake",
        "guake",
        "terminology",
        "contour",
        "blackbox",
        "tabby",
        "extraterm",
        "cool-retro-term",
        "sakura",
        "qterminal",
        "lxterminal",
        "xfce4-terminal",
        "mate-terminal",
        "warp",
        "waveterm",
        "rio",
        "hyper",
    ];
    const EXACT: &[&str] = &[
        "st",
        "st-256color",
        "term",
        "eterm",
        "aterm",
    ];
    const DESKTOP_IDS: &[&str] = &[
        "org.gnome.console",
        "org.gnome.ptyxis",
        "org.gnome.terminal",
    ];

    let name = app_name.to_ascii_lowercase();
    let id = bundle_id.to_ascii_lowercase();
    let name = name.strip_suffix(".desktop").unwrap_or(&name);
    let id = id.strip_suffix(".desktop").unwrap_or(&id);

    DESKTOP_IDS.contains(&name)
        || DESKTOP_IDS.contains(&id)
        || EXACT.contains(&name)
        || EXACT.contains(&id)
        || IDENTIFIERS
            .iter()
            .any(|identifier| bounded_match(name, identifier) || bounded_match(id, identifier))
}

fn bounded_match(value: &str, identifier: &str) -> bool {
    value.match_indices(identifier).any(|(start, _)| {
        let end = start + identifier.len();
        let starts_at_boundary = value[..start]
            .chars()
            .next_back()
            .map_or(true, |c| !c.is_alphanumeric());
        let ends_at_boundary = value[end..]
            .chars()
            .next()
            .map_or(true, |c| !c.is_alphanumeric());
        starts_at_boundary && ends_at_boundary
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detected_chord(app_name: &str, bundle_id: &str) -> PasteChord {
        resolve("", looks_like_terminal(app_name, bundle_id)).unwrap()
    }

    #[test]
    fn defaults_to_ctrl_v() {
        assert_eq!(resolve("", false), Ok(CTRL_V));
    }

    #[test]
    fn env_override_wins_over_terminal_detection() {
        assert_eq!(resolve("ctrl+v", true), Ok(CTRL_V));
        assert_eq!(resolve("ctrl+shift+v", false), Ok(CTRL_SHIFT_V));
    }

    #[test]
    fn terminal_gets_ctrl_shift_v() {
        assert_eq!(resolve("", true), Ok(CTRL_SHIFT_V));
    }

    #[test]
    fn rejects_other_chords() {
        assert!(parse("ctrl+x").is_err());
    }

    #[test]
    fn detects_terminals() {
        for (app_name, bundle_id) in [
            ("kitty", "kitty"),
            ("", "org.kde.konsole.desktop"),
            ("", "com.mitchellh.ghostty.desktop"),
            ("Alacritty", "Alacritty"),
            ("st", "st-256color"),
        ] {
            assert_eq!(
                detected_chord(app_name, bundle_id),
                CTRL_SHIFT_V,
                "{app_name:?} / {bundle_id:?}"
            );
        }
    }

    #[test]
    fn detects_gnome_terminal_desktop_ids() {
        for bundle_id in [
            "org.gnome.Console",
            "org.gnome.Console.desktop",
            "org.gnome.Ptyxis",
            "org.gnome.Ptyxis.desktop",
        ] {
            assert_eq!(
                detected_chord("", bundle_id),
                CTRL_SHIFT_V,
                "{bundle_id:?}"
            );
        }
    }

    #[test]
    fn detects_short_terminal_identifiers_at_boundaries() {
        for (app_name, bundle_id) in [
            ("warp", ""),
            ("", "dev.warp.Warp"),
            ("rio", ""),
            ("hyper", ""),
            ("foot", ""),
            ("footclient", ""),
            ("", "org.codeberg.dnkl.foot"),
        ] {
            assert_eq!(
                detected_chord(app_name, bundle_id),
                CTRL_SHIFT_V,
                "{app_name:?} / {bundle_id:?}"
            );
        }
    }

    #[test]
    fn ignores_names_containing_terminal_identifiers() {
        for (app_name, bundle_id) in [
            ("Warpinator", "org.x.Warpinator"),
            ("Riot", "com.riotgames.RiotClientServices"),
            ("Prior", "org.example.Prior"),
            ("Visual Studio Code", "visual-studio-code.desktop"),
            ("Studio", "org.example.Studio"),
            ("Hypervisor", "org.example.Hypervisor"),
            ("Footnote", "org.example.Footnote"),
            ("Football", "org.example.Football"),
            ("Long-term Notes", "org.st.CubeIDE"),
            ("firefox", "firefox.desktop"),
        ] {
            assert_eq!(
                detected_chord(app_name, bundle_id),
                CTRL_V,
                "{app_name:?} / {bundle_id:?}"
            );
        }
    }
}
