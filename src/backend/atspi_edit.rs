//! Post-paste edit observation — feeds the app's "auto-learn corrected words".
//!
//! After `PasteText` lands, the app expects one `UpdateEditedText` request per
//! `pasteId` once the user has finished editing the dictated text; it diffs
//! `pastedText` against `editedText` itself and adds the changed words to the
//! dictionary. macOS/Windows read the field through AX/UIA; here we read it over
//! AT-SPI.
//!
//! Method: right after the paste, find the focused accessible, read its full text
//! and locate the pasted span (anchored on the caret). Remember the text before
//! and after that span, then poll the same field. The "edited text" is whatever
//! sits between those two anchors. Observation ends on focus loss, a quiet
//! period, a hard cap, or the next paste. If the field can't be read (no Text
//! interface, terminal, a11y off) nothing is sent — same as before this module.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::ObjectRefExt;
use atspi::proxy::text::TextProxy;
use atspi::{Role, State};
use serde_json::{json, Value};

use super::atspi_app::LAST_FOCUSED_WIDGET;
use super::atspi_sel::{descend_for_focus, desktop_root};
use crate::proto;

/// Let the Ctrl+V land (and the toolkit emit its text update) before baselining.
const SETTLE: Duration = Duration::from_millis(300);
const BASELINE_TRIES: u32 = 4;
const POLL: Duration = Duration::from_millis(400);
/// Quiet time after the last observed change before we call the edit finished.
const IDLE_END: Duration = Duration::from_secs(15);
/// Never watch one paste longer than this.
const MAX_WATCH: Duration = Duration::from_secs(120);
/// Skip huge documents; the IPC frame cap is 30k and these aren't dictation fields.
const MAX_FIELD_CHARS: i32 = 100_000;
const WALK_TIMEOUT: Duration = Duration::from_secs(8);
const WALK_DEPTH: usize = 40;
const WALK_NODES: usize = 6_000;
const MAX_REPORT_CHARS: usize = 8_000;

/// Bumped by every new paste so the previous watcher ends ("superseded").
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Start observing the field the paste just went into. Returns immediately.
pub fn watch(paste_id: String, pasted: String, tx: Sender<Value>) {
    if paste_id.is_empty() || pasted.trim().is_empty() {
        return;
    }
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => return log::debug!("atspi_edit: runtime: {e}"),
        };
        match rt.block_on(run(&paste_id, &pasted, generation)) {
            Ok(Some((edited, reason))) => {
                log::info!(
                    "atspi_edit: pasteId={paste_id} end={reason} changed={}",
                    edited != pasted
                );
                let payload = json!({ "payload": {
                    "pasteId": paste_id,
                    "pastedText": pasted,
                    "editedText": edited,
                    "contentObservationEndReason": reason,
                }});
                let _ = tx.send(proto::request(
                    "UpdateEditedText",
                    payload,
                    &format!("edit-{paste_id}"),
                ));
            }
            Ok(None) => {}
            Err(e) => log::debug!("atspi_edit: pasteId={paste_id} not observed: {e}"),
        }
    });
}

/// `Ok(None)`: observation ended with nothing trustworthy to report.
async fn run(
    paste_id: &str,
    pasted: &str,
    generation: u64,
) -> Result<Option<(String, &'static str)>, String> {
    tokio::time::sleep(SETTLE).await;
    let a11y = AccessibilityConnection::new()
        .await
        .map_err(|e| format!("a11y connect: {e}"))?;
    let conn = a11y.connection();

    // Prefer the widget the focus tracker last saw; fall back to walking the active window.
    let tracked = LAST_FOCUSED_WIDGET.lock().ok().and_then(|g| g.clone());
    let mut focused = None;
    if let Some(obj) = tracked {
        if let Ok(p) = obj.as_accessible_proxy(conn).await {
            if p.get_state()
                .await
                .map(|s| s.contains(State::Focused))
                .unwrap_or(false)
            {
                focused = Some(obj);
            }
        }
    }
    let focused = match focused {
        Some(f) => f,
        None => tokio::time::timeout(WALK_TIMEOUT, find_focused_in_active_window(conn))
            .await
            .map_err(|_| "focus walk timed out".to_string())??
            .ok_or("no focused accessible")?,
    };
    log::debug!("atspi_edit: focused {}{}", focused.name, focused.path);
    let text = TextProxy::builder(conn)
        .destination(focused.name.as_str())
        .and_then(|b| b.path(focused.path.as_str()))
        .map_err(|e| format!("text proxy: {e}"))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| format!("no Text interface: {e}"))?;
    let acc = focused
        .as_accessible_proxy(conn)
        .await
        .map_err(|e| format!("accessible: {e}"))?;
    if acc
        .get_role()
        .await
        .map(|r| r == Role::PasswordText)
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let read = || async {
        let n = text.character_count().await.map_err(|e| e.to_string())?;
        if n > MAX_FIELD_CHARS {
            return Err("field too large".to_string());
        }
        text.get_text(0, n)
            .await
            .map(|s| s.chars().collect::<Vec<char>>())
            .map_err(|e| e.to_string())
    };

    // The paste can land a beat after Ctrl+V (Chromium reads the clipboard async),
    // so retry the baseline for a while before giving up.
    let pasted_chars: Vec<char> = pasted.chars().collect();
    let mut found = None;
    for attempt in 0..BASELINE_TRIES {
        let baseline = read().await?;
        let caret = text.caret_offset().await.unwrap_or(-1);
        if let Some(start) = locate(&baseline, &pasted_chars, caret) {
            found = Some((baseline, start));
            break;
        }
        if attempt + 1 < BASELINE_TRIES {
            tokio::time::sleep(POLL).await;
        }
    }
    let (baseline, start) = found.ok_or("pasted text not found in field")?;
    let before = baseline[..start].to_vec();
    let after = baseline[start + pasted_chars.len()..].to_vec();
    log::debug!(
        "atspi_edit: pasteId={paste_id} observing (before={} after={})",
        before.len(),
        after.len()
    );

    let began = Instant::now();
    let mut last_change = Instant::now();
    let mut current = baseline;
    let reason = loop {
        tokio::time::sleep(POLL).await;
        if GENERATION.load(Ordering::SeqCst) != generation {
            break "superseded";
        }
        if let Ok(now) = read().await {
            if now != current {
                current = now;
                last_change = Instant::now();
            }
        } else {
            break "focus_changed";
        }
        let still_focused = match acc.get_state().await {
            Ok(s) => s.contains(State::Focused),
            Err(_) => false,
        };
        if !still_focused {
            break "focus_changed";
        }
        if last_change.elapsed() >= IDLE_END {
            break "idle_timeout";
        }
        if began.elapsed() >= MAX_WATCH {
            break "max_duration";
        }
    };

    // The edit region is what sits between the unchanged text around the paste.
    // If either anchor was damaged we can't tell paste edits from other edits.
    if current.len() < before.len() + after.len()
        || !current.starts_with(&before)
        || !current.ends_with(&after)
    {
        log::debug!("atspi_edit: pasteId={paste_id} anchors changed — not reporting");
        return Ok(None);
    }
    let edited: String = current[before.len()..current.len() - after.len()]
        .iter()
        .collect();
    if edited.trim().is_empty() || edited.chars().count() > MAX_REPORT_CHARS {
        return Ok(None);
    }
    Ok(Some((edited, reason)))
}

/// Locate the focused field by descending only the active top-level window(s).
/// Web content (Chromium/Electron) nests the field far deeper than the shared
/// shallow walk used for selection reads reaches, so this one gets a larger
/// depth/node budget — affordable because it runs off the paste path.
async fn find_focused_in_active_window(
    conn: &zbus::Connection,
) -> Result<Option<atspi::ObjectRef>, String> {
    let root = desktop_root(conn).await?;
    for app in root.get_children().await.unwrap_or_default() {
        let Ok(app_proxy) = app.as_accessible_proxy(conn).await else {
            continue;
        };
        for win in app_proxy.get_children().await.unwrap_or_default() {
            let Ok(win_proxy) = win.as_accessible_proxy(conn).await else {
                continue;
            };
            if !win_proxy
                .get_state()
                .await
                .map(|s| s.contains(State::Active))
                .unwrap_or(false)
            {
                continue;
            }
            let mut budget = WALK_NODES;
            if let Some(f) = descend_for_focus(conn, &win, 0, WALK_DEPTH, &mut budget).await {
                return Ok(Some(f));
            }
        }
    }
    Ok(None)
}

/// Char offset of `needle` in `hay`: the occurrence ending at the caret if the
/// toolkit reports one (a paste leaves the caret right after the text), else the
/// last occurrence.
fn locate(hay: &[char], needle: &[char], caret: i32) -> Option<usize> {
    let n = needle.len();
    if n == 0 || n > hay.len() {
        return None;
    }
    if caret >= n as i32 {
        let s = caret as usize - n;
        if hay.get(s..s + n) == Some(needle) {
            return Some(s);
        }
    }
    (0..=hay.len() - n)
        .rev()
        .find(|&i| &hay[i..i + n] == needle)
}

#[cfg(test)]
mod tests {
    use super::locate;

    fn c(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn caret_anchor_beats_earlier_duplicate() {
        let hay = c("hi hi");
        assert_eq!(locate(&hay, &c("hi"), 5), Some(3));
        assert_eq!(locate(&hay, &c("hi"), 2), Some(0));
    }

    #[test]
    fn falls_back_to_last_occurrence_and_misses() {
        assert_eq!(locate(&c("a b a"), &c("a"), -1), Some(4));
        assert_eq!(locate(&c("abc"), &c("x"), 3), None);
        assert_eq!(locate(&c("é日本 x"), &c("日本"), 3), Some(1));
    }
}
