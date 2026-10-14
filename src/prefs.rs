//! Read the app's user preferences from its electron-store `config.json`.
//!
//! Some helper behaviours are gated on user settings the app never sends over
//! IPC; the macOS helper reads them from the same file. It lives in the app's
//! userData dir, which on Linux is `$XDG_CONFIG_HOME/Wispr Flow` (the app's
//! productName). Read on each use: the file is small and the user can flip a
//! setting at any time.

use std::path::PathBuf;

use serde_json::Value;

/// `prefs.user.<key>` as a bool, or `None` when the file, the key or a boolean
/// value is missing (callers apply the app's own default).
pub fn user_bool(key: &str) -> Option<bool> {
    let raw = std::fs::read_to_string(config_path()?).ok()?;
    user_bool_in(&raw, key)
}

fn user_bool_in(raw: &str, key: &str) -> Option<bool> {
    let config: Value = serde_json::from_str(raw).ok()?;
    config.get("prefs")?.get("user")?.get(key)?.as_bool()
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("Wispr Flow").join("config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str =
        r#"{ "prefs": { "user": { "enabled": true, "disabled": false, "name": "x" } } }"#;

    #[test]
    fn user_bool_in_reads_nested_user_prefs() {
        assert_eq!(user_bool_in(CONFIG, "enabled"), Some(true));
        assert_eq!(user_bool_in(CONFIG, "disabled"), Some(false));
    }

    #[test]
    fn user_bool_in_is_none_for_missing_or_non_bool_values() {
        assert_eq!(user_bool_in(CONFIG, "absent"), None);
        assert_eq!(user_bool_in(CONFIG, "name"), None);
        assert_eq!(user_bool_in("not json", "enabled"), None);
    }
}
