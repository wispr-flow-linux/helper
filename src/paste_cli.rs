//! `wispr-flow-linux-helper paste-rules ...`: a small terminal configurator for
//! `paste-rules.json` (see paste.rs for the file format and resolution order).
//!
//! It lives in the helper binary on purpose: it needs the same focused-window
//! detection the helper uses (KWin bridge / GNOME extension / AT-SPI / X11),
//! and this way it works on every desktop the helper does with no extra
//! dependencies. `pick` is the interactive path: focus the target window, then
//! choose the chord.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use crate::backend::{self, ActiveApp};
use crate::paste::{self, Compiled, PasteChord, RuleSpec, RulesFile};

const USAGE: &str = "\
Usage: wispr-flow-linux-helper paste-rules <command>

  list                      show the rules and where the file lives
  pick [--delay SECONDS]    focus a window, then choose how to paste into it
  add --keys CHORD [--app APP] [--title REGEX]
                            add (or replace) a rule without prompts
  remove N                  remove rule number N (as shown by `list`)
  default CHORD             chord for windows that match nothing
  check                     validate the rules file

CHORD is e.g. ctrl+v, ctrl+shift+v, shift+insert.
APP is the app id, its last segment (konsole for org.kde.konsole) or the
process name. REGEX is searched in the window title.
Changes apply to the next dictation; no restart needed.";

type CliResult = Result<(), String>;

/// Entry point. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    // The installed helper is setgid (input group, for /dev/uinput and
    // /dev/input). The configurator needs neither, and must not create the
    // user's config file with that group.
    if unsafe { libc::setgid(libc::getgid()) } != 0 {
        eprintln!(
            "paste-rules: cannot drop setgid: {}",
            std::io::Error::last_os_error()
        );
        return 1;
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("error"))
        .target(env_logger::Target::Stderr)
        .init();

    let Some(path) = paste::rules_path() else {
        eprintln!("paste-rules: neither XDG_CONFIG_HOME nor HOME is set");
        return 1;
    };
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        None | Some("list") => list(&path),
        Some("pick") => pick(&path, rest),
        Some("add") => add(&path, rest),
        Some("remove") => remove(&path, rest),
        Some("default") => set_default(&path, rest),
        Some("check") => check(&path),
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("paste-rules: {e}");
            1
        }
    }
}

fn load(path: &Path) -> Result<RulesFile, String> {
    Ok(paste::load_file(path)?.unwrap_or_default())
}

/// Validate, then write atomically (temp file + rename) so the running helper
/// never reads a half-written file.
fn save(path: &Path, file: &RulesFile) -> CliResult {
    Compiled::compile(file)?;
    let dir = path.parent().ok_or("rules path has no parent directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let mut json = serde_json::to_string_pretty(file).map_err(|e| format!("encode: {e}"))?;
    json.push('\n');
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))?;
    println!("Saved {}", path.display());
    Ok(())
}

fn describe(rule: &RuleSpec) -> String {
    let mut parts = Vec::new();
    if let Some(app) = &rule.app {
        parts.push(format!("app {app}"));
    }
    if let Some(title) = &rule.title {
        parts.push(format!("title /{title}/"));
    }
    format!("{} -> {}", parts.join(", "), rule.keys)
}

fn list(path: &Path) -> CliResult {
    println!("Rules file: {}", path.display());
    let Some(file) = paste::load_file(path)? else {
        println!(
            "(not created yet: terminals paste with ctrl+shift+v, everything else with ctrl+v)"
        );
        return Ok(());
    };
    Compiled::compile(&file)?;
    if file.rules.is_empty() {
        println!("No rules.");
    }
    for (i, rule) in file.rules.iter().enumerate() {
        println!("{:>3}. {}", i + 1, describe(rule));
    }
    println!(
        "Terminal detection: {}",
        if file.detect_terminals.unwrap_or(true) {
            "on (ctrl+shift+v)"
        } else {
            "off"
        }
    );
    println!("Default: {}", file.default.as_deref().unwrap_or("ctrl+v"));
    Ok(())
}

fn check(path: &Path) -> CliResult {
    match paste::load_file(path)? {
        Some(file) => {
            Compiled::compile(&file)?;
            println!("{}: OK, {} rule(s)", path.display(), file.rules.len());
        }
        None => println!("{}: not created yet", path.display()),
    }
    Ok(())
}

/// Value of `--flag VALUE` in `args`, if present.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>, String> {
    match args.iter().position(|a| a == flag) {
        None => Ok(None),
        Some(i) => args
            .get(i + 1)
            .map(|v| Some(v.as_str()))
            .ok_or_else(|| format!("{flag} needs a value")),
    }
}

/// Insert `rule`, replacing a rule with the same app + title. Title rules go
/// first so they are tried before a broader app-only rule (first match wins).
fn upsert(file: &mut RulesFile, rule: RuleSpec) {
    let same = |r: &RuleSpec| {
        r.title == rule.title
            && r.app.as_deref().map(str::to_ascii_lowercase)
                == rule.app.as_deref().map(str::to_ascii_lowercase)
    };
    if let Some(existing) = file.rules.iter_mut().find(|r| same(r)) {
        *existing = rule;
    } else if rule.title.is_some() {
        file.rules.insert(0, rule);
    } else {
        file.rules.push(rule);
    }
}

fn add(path: &Path, args: &[String]) -> CliResult {
    let keys = flag_value(args, "--keys")?.ok_or("add needs --keys CHORD")?;
    let rule = RuleSpec {
        app: flag_value(args, "--app")?.map(String::from),
        title: flag_value(args, "--title")?.map(String::from),
        keys: PasteChord::parse(keys)?.to_string(),
    };
    let mut file = load(path)?;
    println!("Rule: {}", describe(&rule));
    upsert(&mut file, rule);
    save(path, &file)
}

fn remove(path: &Path, args: &[String]) -> CliResult {
    let mut file = load(path)?;
    let n: usize = args
        .first()
        .and_then(|a| a.parse().ok())
        .filter(|n| (1..=file.rules.len()).contains(n))
        .ok_or_else(|| {
            format!(
                "remove needs a rule number between 1 and {}",
                file.rules.len()
            )
        })?;
    let removed = file.rules.remove(n - 1);
    println!("Removed: {}", describe(&removed));
    save(path, &file)
}

fn set_default(path: &Path, args: &[String]) -> CliResult {
    let keys = args.first().ok_or("default needs a CHORD")?;
    let mut file = load(path)?;
    file.default = Some(PasteChord::parse(keys)?.to_string());
    save(path, &file)
}

/// Print `question`, read one trimmed line. EOF is an error (aborts the command).
fn ask(question: &str) -> Result<String, String> {
    print!("{question} ");
    std::io::stdout()
        .flush()
        .map_err(|e| format!("stdout: {e}"))?;
    let mut line = String::new();
    let n = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("stdin: {e}"))?;
    if n == 0 {
        return Err("aborted (end of input)".into());
    }
    Ok(line.trim().to_string())
}

/// Watch the focused window for `delay` seconds and return the last one seen.
fn focused_window_after(delay: u64) -> Result<ActiveApp, String> {
    // The provider pushes focus events into this channel; nothing reads them
    // here, but the receiver must stay alive while the provider runs.
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut be = backend::detect_identity_only(tx);
    for left in (1..=delay).rev() {
        print!("\rSwitch to the target window... {left} ");
        std::io::stdout()
            .flush()
            .map_err(|e| format!("stdout: {e}"))?;
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("\r{:<40}", "Captured.");
    let app = be.get_active_app()?;
    if app.bundle_id.is_empty() && app.app_name.is_empty() && app.window_title.is_empty() {
        return Err("could not identify the focused window on this desktop \
                    (no active-window source); use `add` with --app/--title instead"
            .into());
    }
    Ok(app)
}

fn pick(path: &Path, args: &[String]) -> CliResult {
    let delay = match flag_value(args, "--delay")? {
        Some(v) => v.parse().map_err(|_| format!("bad --delay '{v}'"))?,
        None => 5,
    };
    let mut file = load(path)?;
    let current = Compiled::compile(&file)?;

    let app = focused_window_after(delay)?;
    let (chord, why) = current.chord_for(&app);
    println!("  app id : {}", app.bundle_id);
    println!("  process: {}", app.app_name);
    println!("  title  : {}", app.window_title);
    println!("  pastes with {chord} now ({why})");
    println!();

    println!("Paste into it with:  1) ctrl+v   2) ctrl+shift+v   3) shift+insert");
    let keys = match ask("Number, or type a chord:")?.as_str() {
        "1" => "ctrl+v".to_string(),
        "2" => "ctrl+shift+v".to_string(),
        "3" => "shift+insert".to_string(),
        other => PasteChord::parse(other)?.to_string(),
    };

    let app_key = if app.bundle_id.is_empty() {
        app.app_name.clone()
    } else {
        app.bundle_id.clone()
    };
    println!();
    println!("Apply to:  1) every window of {app_key}   2) only this exact title   3) titles matching a regex");
    let title = match ask("Number [1]:")?.as_str() {
        "" | "1" => None,
        "2" => Some(format!("^{}$", regex::escape(&app.window_title))),
        "3" => Some(ask("Regex (searched in the title):")?),
        other => return Err(format!("unknown choice '{other}'")),
    };
    if title.is_none() && app_key.is_empty() {
        return Err("this window reports no app id; pick a title match instead".into());
    }

    let rule = RuleSpec {
        app: (!app_key.is_empty()).then_some(app_key),
        title,
        keys,
    };
    println!("Rule: {}", describe(&rule));
    upsert(&mut file, rule);
    save(path, &file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(app: Option<&str>, title: Option<&str>, keys: &str) -> RuleSpec {
        RuleSpec {
            app: app.map(String::from),
            title: title.map(String::from),
            keys: keys.into(),
        }
    }

    #[test]
    fn upsert_replaces_same_target_and_orders_title_rules_first() {
        let mut file = RulesFile::default();
        upsert(&mut file, rule(Some("konsole"), None, "ctrl+shift+v"));
        upsert(&mut file, rule(Some("kate"), None, "ctrl+v"));
        upsert(
            &mut file,
            rule(Some("konsole"), Some("tmux$"), "shift+insert"),
        );
        upsert(&mut file, rule(Some("Konsole"), None, "shift+insert"));
        assert_eq!(
            file.rules,
            vec![
                rule(Some("konsole"), Some("tmux$"), "shift+insert"),
                rule(Some("Konsole"), None, "shift+insert"),
                rule(Some("kate"), None, "ctrl+v"),
            ]
        );
    }

    #[test]
    fn save_round_trips_and_rejects_invalid_rules() {
        let dir = std::env::temp_dir().join(format!("wispr-paste-cli-{}", std::process::id()));
        let path = dir.join("nested").join("paste-rules.json");
        let mut file = RulesFile::default();
        upsert(&mut file, rule(Some("konsole"), None, "ctrl+shift+v"));
        save(&path, &file).unwrap();
        assert_eq!(paste::load_file(&path).unwrap(), Some(file.clone()));

        file.rules.push(rule(None, None, "ctrl+v"));
        assert!(save(&path, &file).is_err());
        // The rejected save left the good file in place.
        assert_eq!(paste::load_file(&path).unwrap().unwrap().rules.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flag_value_reads_pairs() {
        let args: Vec<String> = ["--app", "kitty", "--keys"].map(String::from).to_vec();
        assert_eq!(flag_value(&args, "--app"), Ok(Some("kitty")));
        assert_eq!(flag_value(&args, "--title"), Ok(None));
        assert!(flag_value(&args, "--keys").is_err());
    }
}
