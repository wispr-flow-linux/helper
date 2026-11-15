//! Paste-chord selection: which key chord `PasteText` synthesizes after
//! setting the clipboard.
//!
//! Resolution order for each paste (first hit wins):
//!   1. `WISPR_LINUX_HELPER_PASTE_KEYS` env var: force one chord globally.
//!   2. User rules in `paste-rules.json`: the focused window's app id / name
//!      and title are matched against each rule in order.
//!   3. Built-in terminal detection: terminal emulators get Ctrl+Shift+V,
//!      because Ctrl+V never reaches their paste handler (it is quoted-insert
//!      in readline, visual-block in vim). Disable with `"detect_terminals": false`.
//!   4. The file's `"default"`, else Ctrl+V.
//!
//! The rules file is re-read whenever its mtime changes, so edits (by hand or
//! via `wispr-flow-linux-helper paste-rules ...`) apply to the next paste
//! without restarting the app. A broken file never blocks a paste: the error
//! is logged and resolution continues with the built-in behaviour.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::backend::ActiveApp;

pub const PASTE_KEYS_ENV: &str = "WISPR_LINUX_HELPER_PASTE_KEYS";

/// A key chord: one key (Windows VK code, see keymap.rs) plus modifier flag
/// names in the form the keymap understands ("Control", "Shift", "Alt", "Super").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteChord {
    pub key_vk: u32,
    pub flags: Vec<&'static str>,
}

impl PasteChord {
    pub fn ctrl_v() -> Self {
        PasteChord {
            key_vk: b'V' as u32,
            flags: vec!["Control"],
        }
    }

    pub fn ctrl_shift_v() -> Self {
        PasteChord {
            key_vk: b'V' as u32,
            flags: vec!["Control", "Shift"],
        }
    }

    /// Parse `ctrl+shift+v`, `shift+insert`, `ctrl+alt+f5`, ... Case-insensitive.
    /// The last token is the key, everything before it is a modifier.
    pub fn parse(value: &str) -> Result<Self, String> {
        let lower = value.trim().to_ascii_lowercase();
        let mut tokens: Vec<&str> = lower.split('+').map(str::trim).collect();
        let key = tokens.pop().filter(|k| !k.is_empty());
        let key = key.ok_or_else(|| format!("empty key chord '{value}'"))?;
        let mut flags = Vec::new();
        for token in tokens {
            let flag = match token {
                "ctrl" | "control" => "Control",
                "shift" => "Shift",
                "alt" => "Alt",
                "super" | "meta" | "cmd" | "command" => "Super",
                other => return Err(format!("unknown modifier '{other}' in '{value}'")),
            };
            if !flags.contains(&flag) {
                flags.push(flag);
            }
        }
        let key_vk = key_to_vk(key).ok_or_else(|| {
            format!("unknown key '{key}' in '{value}' (use a letter, a digit, insert, or f1-f12)")
        })?;
        Ok(PasteChord { key_vk, flags })
    }
}

/// Renders in the same syntax `parse` accepts (`ctrl+shift+v`).
impl std::fmt::Display for PasteChord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for flag in &self.flags {
            let name = match *flag {
                "Control" => "ctrl",
                "Shift" => "shift",
                "Alt" => "alt",
                _ => "super",
            };
            write!(f, "{name}+")?;
        }
        match self.key_vk {
            45 => write!(f, "insert"),
            vk @ 112..=123 => write!(f, "f{}", vk - 111),
            vk => write!(f, "{}", (vk as u8 as char).to_ascii_lowercase()),
        }
    }
}

/// Key name -> Windows VK code. Deliberately small: the keys a paste binding
/// realistically uses.
fn key_to_vk(key: &str) -> Option<u32> {
    if key.len() == 1 {
        let c = key.as_bytes()[0];
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            return Some(c.to_ascii_uppercase() as u32);
        }
        return None;
    }
    match key {
        "insert" | "ins" => Some(45),
        _ => {
            let n: u32 = key.strip_prefix('f')?.parse().ok()?;
            (1..=12).contains(&n).then_some(111 + n) // VK_F1 = 112
        }
    }
}

/// On-disk shape of `paste-rules.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RulesFile {
    /// Chord for windows no rule (and no terminal detection) matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Built-in terminal detection (Ctrl+Shift+V). On unless set to false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_terminals: Option<bool>,
    #[serde(default)]
    pub rules: Vec<RuleSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    /// App id (WM class / desktop id), its last dot-segment, or the process
    /// name. Case-insensitive, whole-string match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Regex searched in the window title (unanchored; add `^`/`$` to anchor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub keys: String,
}

struct Rule {
    app: Option<String>,
    title: Option<Regex>,
    chord: PasteChord,
}

impl Rule {
    fn matches(&self, app: &ActiveApp) -> bool {
        let app_ok = self
            .app
            .as_deref()
            .is_none_or(|want| app_matches(want, app));
        let title_ok = self
            .title
            .as_ref()
            .is_none_or(|re| re.is_match(&app.window_title));
        app_ok && title_ok
    }
}

/// `want` is already lowercased. Matches the full app id (with or without a
/// `.desktop` suffix), its last dot-segment (`konsole` for `org.kde.konsole`),
/// or the process name.
fn app_matches(want: &str, app: &ActiveApp) -> bool {
    let id = app.bundle_id.to_ascii_lowercase();
    let id = id.strip_suffix(".desktop").unwrap_or(&id);
    let last_segment = id.rsplit('.').next().unwrap_or(id);
    want == id || want == last_segment || want == app.app_name.to_ascii_lowercase()
}

/// A validated, ready-to-match rule set.
pub struct Compiled {
    default: PasteChord,
    detect_terminals: bool,
    rules: Vec<Rule>,
}

impl Default for Compiled {
    fn default() -> Self {
        Compiled {
            default: PasteChord::ctrl_v(),
            detect_terminals: true,
            rules: Vec::new(),
        }
    }
}

impl Compiled {
    pub fn compile(file: &RulesFile) -> Result<Self, String> {
        let default = match &file.default {
            Some(keys) => PasteChord::parse(keys).map_err(|e| format!("default: {e}"))?,
            None => PasteChord::ctrl_v(),
        };
        let mut rules = Vec::with_capacity(file.rules.len());
        for (i, spec) in file.rules.iter().enumerate() {
            let n = i + 1;
            if spec.app.is_none() && spec.title.is_none() {
                return Err(format!(
                    "rule {n}: needs \"app\" and/or \"title\" (use \"default\" for a catch-all)"
                ));
            }
            let title = match &spec.title {
                Some(pattern) => {
                    Some(Regex::new(pattern).map_err(|e| format!("rule {n}: title regex: {e}"))?)
                }
                None => None,
            };
            rules.push(Rule {
                app: spec.app.as_ref().map(|a| a.trim().to_ascii_lowercase()),
                title,
                chord: PasteChord::parse(&spec.keys).map_err(|e| format!("rule {n}: {e}"))?,
            });
        }
        Ok(Compiled {
            default,
            detect_terminals: file.detect_terminals.unwrap_or(true),
            rules,
        })
    }

    /// Chord for `app`, plus a short reason for logs and the CLI.
    pub fn chord_for(&self, app: &ActiveApp) -> (PasteChord, String) {
        if let Some((i, rule)) = self.rules.iter().enumerate().find(|(_, r)| r.matches(app)) {
            return (rule.chord.clone(), format!("rule {}", i + 1));
        }
        if self.detect_terminals && looks_like_terminal(&app.app_name, &app.bundle_id) {
            return (PasteChord::ctrl_shift_v(), "terminal detection".into());
        }
        (self.default.clone(), "default".into())
    }
}

/// `$XDG_CONFIG_HOME/wispr-flow/paste-rules.json` (falling back to `~/.config`).
pub fn rules_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?).join(".config"),
    };
    Some(base.join("wispr-flow").join("paste-rules.json"))
}

/// Read and parse the rules file. `Ok(None)` when it does not exist.
pub fn load_file(path: &Path) -> Result<Option<RulesFile>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("parse {}: {e}", path.display()))
}

/// The live rule set used by the dispatch loop: reloads the file when its
/// mtime changes.
pub struct Rules {
    path: Option<PathBuf>,
    seen_mtime: Option<SystemTime>,
    compiled: Compiled,
}

impl Rules {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut rules = Rules {
            path,
            seen_mtime: None,
            compiled: Compiled::default(),
        };
        rules.reload_if_changed();
        rules
    }

    fn reload_if_changed(&mut self) {
        let Some(path) = &self.path else { return };
        // A missing file reads as mtime None, so deleting the file also reloads.
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime == self.seen_mtime {
            return;
        }
        self.seen_mtime = mtime;
        self.compiled = match load_file(path).and_then(|f| match f {
            Some(file) => Compiled::compile(&file).map(Some),
            None => Ok(None),
        }) {
            Ok(Some(compiled)) => {
                log::info!(
                    "paste rules: loaded {} rule(s) from {}",
                    compiled.rules.len(),
                    path.display()
                );
                compiled
            }
            Ok(None) => Compiled::default(),
            Err(e) => {
                log::error!("paste rules ignored, using built-in behaviour: {e}");
                Compiled::default()
            }
        };
    }

    /// Chord for a paste into `app`, plus the reason it was chosen.
    pub fn chord_for(&mut self, app: &ActiveApp) -> (PasteChord, String) {
        let env = std::env::var(PASTE_KEYS_ENV).unwrap_or_default();
        if !env.trim().is_empty() {
            match PasteChord::parse(&env) {
                Ok(chord) => return (chord, format!("{PASTE_KEYS_ENV} override")),
                Err(e) => log::warn!("{PASTE_KEYS_ENV} ignored: {e}"),
            }
        }
        self.reload_if_changed();
        self.compiled.chord_for(app)
    }
}

/// True if the app name / id look like a terminal emulator.
///
/// Names are compared as whole tokens (the id split on `.`, `_`, `/` and
/// spaces), never as substrings, so `warp` matches `dev.warp.Warp` but not
/// Warpinator, and `st` does not match Steam.
pub fn looks_like_terminal(app_name: &str, bundle_id: &str) -> bool {
    const TOKENS: &[&str] = &[
        "kitty",
        "alacritty",
        "ghostty",
        "wezterm",
        "wezterm-gui",
        "konsole",
        "yakuake",
        "foot",
        "footclient",
        "gnome-terminal",
        "gnome-terminal-server",
        "kgx",
        "ptyxis",
        "xterm",
        "uxterm",
        "urxvt",
        "rxvt",
        "terminator",
        "tilix",
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
        "warp-terminal",
        "waveterm",
        "rio",
        "hyper",
        "st",
        "st-256color",
        "eterm",
        "aterm",
    ];
    // Ids whose distinguishing segment is too generic to list as a token.
    const FULL_IDS: &[&str] = &["org.gnome.console", "com.raggesilver.blackbox"];

    let name = app_name.to_ascii_lowercase();
    let id = bundle_id.to_ascii_lowercase();
    let id = id.strip_suffix(".desktop").unwrap_or(&id);
    if FULL_IDS.contains(&id) {
        return true;
    }
    let is_token = |s: &str| TOKENS.contains(&s);
    name.split([' ', '/']).any(is_token) || id.split(['.', '_', '/', ' ']).any(is_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap;

    fn app(name: &str, id: &str, title: &str) -> ActiveApp {
        ActiveApp {
            app_name: name.into(),
            bundle_id: id.into(),
            window_title: title.into(),
            ..Default::default()
        }
    }

    fn compiled(json: &str) -> Compiled {
        Compiled::compile(&serde_json::from_str(json).unwrap()).unwrap()
    }

    #[test]
    fn parses_chords() {
        assert_eq!(PasteChord::parse("ctrl+v"), Ok(PasteChord::ctrl_v()));
        assert_eq!(
            PasteChord::parse(" Ctrl + Shift + V "),
            Ok(PasteChord::ctrl_shift_v())
        );
        assert_eq!(
            PasteChord::parse("shift+insert"),
            Ok(PasteChord {
                key_vk: 45,
                flags: vec!["Shift"]
            })
        );
        assert_eq!(PasteChord::parse("f12").unwrap().key_vk, 123);
        for text in [
            "ctrl+v",
            "ctrl+shift+v",
            "shift+insert",
            "alt+super+f5",
            "7",
        ] {
            assert_eq!(PasteChord::parse(text).unwrap().to_string(), text);
        }
    }

    #[test]
    fn rejects_bad_chords() {
        assert!(PasteChord::parse("").is_err());
        assert!(PasteChord::parse("ctrl+").is_err());
        assert!(PasteChord::parse("hyper+v").is_err());
        assert!(PasteChord::parse("ctrl+f13").is_err());
        assert!(PasteChord::parse("ctrl+paste").is_err());
    }

    /// Every chord the parser accepts must be injectable on both backends.
    #[test]
    fn parsed_chords_map_to_both_backends() {
        let mut keys: Vec<String> = ('a'..='z').chain('0'..='9').map(String::from).collect();
        keys.push("insert".into());
        keys.extend((1..=12).map(|n| format!("f{n}")));
        for key in keys {
            let chord = PasteChord::parse(&format!("ctrl+shift+alt+super+{key}")).unwrap();
            assert!(keymap::vk_to_evdev(chord.key_vk).is_some(), "evdev {key}");
            assert!(keymap::vk_to_keysym(chord.key_vk).is_some(), "keysym {key}");
            for flag in &chord.flags {
                assert!(keymap::flag_to_evdev(flag).is_some(), "evdev {flag}");
                assert!(keymap::flag_to_keysym(flag).is_some(), "keysym {flag}");
            }
        }
    }

    #[test]
    fn no_file_keeps_stock_behaviour() {
        let c = Compiled::default();
        assert_eq!(
            c.chord_for(&app("firefox", "firefox", "x")).0,
            PasteChord::ctrl_v()
        );
        assert_eq!(
            c.chord_for(&app("konsole", "org.kde.konsole", "x")).0,
            PasteChord::ctrl_shift_v()
        );
        assert_eq!(c.chord_for(&ActiveApp::default()).0, PasteChord::ctrl_v());
    }

    #[test]
    fn first_matching_rule_wins() {
        let c = compiled(
            r#"{ "rules": [
                { "app": "konsole", "title": "tmux$", "keys": "shift+insert" },
                { "app": "org.kde.konsole", "keys": "ctrl+alt+v" },
                { "title": "(?i)codex", "keys": "ctrl+shift+v" }
            ] }"#,
        );
        let konsole_tmux = app("konsole", "org.kde.konsole", "~ : tmux");
        assert_eq!(c.chord_for(&konsole_tmux).0.key_vk, 45);
        assert_eq!(c.chord_for(&konsole_tmux).1, "rule 1");
        let konsole = app("konsole", "org.kde.konsole", "~ : bash");
        assert_eq!(c.chord_for(&konsole).0.flags, vec!["Control", "Alt"]);
        let chatgpt = app("ChatGPT", "chatgpt", "Codex - project");
        assert_eq!(c.chord_for(&chatgpt).0, PasteChord::ctrl_shift_v());
        assert_eq!(c.chord_for(&chatgpt).1, "rule 3");
    }

    #[test]
    fn app_matches_id_last_segment_or_process_name() {
        let c = compiled(r#"{ "rules": [ { "app": "Konsole", "keys": "shift+insert" } ] }"#);
        for a in [
            app("", "org.kde.konsole", ""),
            app("", "org.kde.konsole.desktop", ""),
            app("konsole", "", ""),
        ] {
            assert_eq!(c.chord_for(&a).1, "rule 1");
        }
        // Whole-string match only: no substring hits.
        assert_ne!(
            c.chord_for(&app("konsolepart", "x.konsolepart", "")).1,
            "rule 1"
        );
    }

    #[test]
    fn default_and_detect_terminals_are_configurable() {
        let c = compiled(r#"{ "default": "shift+insert", "detect_terminals": false }"#);
        assert_eq!(c.chord_for(&app("kitty", "kitty", "")).0.key_vk, 45);
        assert_eq!(c.chord_for(&app("firefox", "firefox", "")).0.key_vk, 45);
    }

    #[test]
    fn rejects_invalid_files() {
        let bad = [
            r#"{ "rules": [ { "keys": "ctrl+v" } ] }"#,
            r#"{ "rules": [ { "app": "x", "keys": "ctrl+nope" } ] }"#,
            r#"{ "rules": [ { "title": "(", "keys": "ctrl+v" } ] }"#,
            r#"{ "default": "nope" }"#,
        ];
        for json in bad {
            let file: RulesFile = serde_json::from_str(json).unwrap();
            assert!(Compiled::compile(&file).is_err(), "{json}");
        }
        // Typos in field names are errors, not silently ignored rules.
        assert!(serde_json::from_str::<RulesFile>(r#"{ "rulez": [] }"#).is_err());
    }

    #[test]
    fn detects_terminals() {
        for (name, id) in [
            ("kitty", "kitty"),
            ("", "org.kde.konsole.desktop"),
            ("", "com.mitchellh.ghostty"),
            ("Alacritty", "Alacritty"),
            ("st", "st-256color"),
            ("", "org.gnome.Console"),
            ("", "org.gnome.Ptyxis"),
            ("", "dev.warp.Warp"),
            ("foot", "foot"),
            ("wezterm-gui", "org.wezfurlong.wezterm"),
        ] {
            assert!(looks_like_terminal(name, id), "{name} / {id}");
        }
    }

    #[test]
    fn ignores_non_terminals() {
        for (name, id) in [
            ("firefox", "firefox.desktop"),
            ("Steam", "steam.desktop"),
            ("warpinator", "org.x.Warpinator"),
            ("football", "com.example.football"),
            ("hyperion", "hyperion"),
            ("", ""),
        ] {
            assert!(!looks_like_terminal(name, id), "{name} / {id}");
        }
    }
}
