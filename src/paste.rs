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

/// True if `app_name` / `bundle_id` look like a terminal emulator. Terminals
/// paste with Ctrl+Shift+V; Ctrl+V never reaches their clipboard handler (it
/// is quoted-insert in readline, visual-block in vim, etc.).
pub fn looks_like_terminal(app_name: &str, bundle_id: &str) -> bool {
    // Distinctive names: substring match is safe (covers "org.kde.konsole",
    // "com.mitchellh.ghostty.desktop", "kitty", ...).
    const SUBSTR: &[&str] = &[
        "kitty",
        "alacritty",
        "ghostty",
        "wezterm",
        "konsole",
        "foot",
        "gnome-terminal",
        "kgx",
        "xterm",
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
    // Short/ambiguous names: exact match only ("st" as a substring would
    // false-positive on half the dictionary).
    const EXACT: &[&str] = &["st", "st-256color", "term", "eterm", "aterm"];
    let name = app_name.to_ascii_lowercase();
    let id = bundle_id.to_ascii_lowercase();
    let id_base = id.strip_suffix(".desktop").unwrap_or(&id);
    SUBSTR.iter().any(|t| name.contains(t) || id.contains(t))
        || EXACT.iter().any(|t| name == *t || id_base == *t)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(looks_like_terminal("kitty", "kitty"));
        assert!(looks_like_terminal("", "org.kde.konsole.desktop"));
        assert!(looks_like_terminal("", "com.mitchellh.ghostty.desktop"));
        assert!(looks_like_terminal("Alacritty", "Alacritty"));
        assert!(looks_like_terminal("st", "st-256color"));
    }

    #[test]
    fn ignores_non_terminals() {
        assert!(!looks_like_terminal("firefox", "firefox.desktop"));
        // "st" must not substring-match arbitrary names:
        assert!(!looks_like_terminal("Star Editor", "star.desktop"));
        assert!(!looks_like_terminal("Steam", "steam.desktop"));
    }
}
