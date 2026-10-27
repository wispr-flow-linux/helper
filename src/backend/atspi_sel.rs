//! AT-SPI2 selection reads — the non-destructive path for `GetSelectedTextViaCopy`.
//!
//! Instead of synthesizing Ctrl+C and snooping the clipboard (destructive: it
//! mutates the user's clipboard and pushes a keystroke into their app), this
//! queries the accessibility bus directly: find the focused accessible object,
//! grab its `org.a11y.atspi.Text` interface, and read the current selection
//! (`GetSelection` -> `GetText`). No input synthesis, no clipboard churn.
//!
//! The `atspi` crate is pure-Rust (over `zbus`, the same crate family the KWin
//! bridge uses) and async (tokio). The rest of the helper is blocking/threaded,
//! so we spin up a tiny single-thread tokio runtime per call and `block_on` it.
//! That keeps the single-static-binary property and avoids threading an async
//! runtime through the whole backend.
//!
//! The focused object is located by walking the accessibility tree from the
//! registry desktop root: enumerate applications, then descend each looking for
//! the node whose `StateSet` contains `State::Focused`. The walk is depth- and
//! breadth-bounded so an unresponsive or pathological app can't hang the call.
//!
//! Returns `Ok(Some(Selection))` when a focused Text object with a non-empty
//! selection is found, `Ok(None)` when AT-SPI is reachable but nothing usable is
//! selected (callers should fall back to the copy-probe), and `Err` when the
//! bus itself is unreachable.

use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::text::TextProxy;
use atspi::{ObjectRef, State};
use zbus::Connection;

use super::Selection;

/// Overall wall-clock budget for the whole AT-SPI probe (connect + walk +
/// reads). The copy-probe fallback is fast, so we'd rather give up early than
/// stall dictation behind a slow/buggy a11y provider.
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// Bound the tree walk: AT-SPI hierarchies can be deep (web content) and wide.
/// The focused editable is almost always shallow, so a modest cap keeps the walk
/// cheap without missing the common case.
const MAX_DEPTH: usize = 12;
const MAX_NODES: usize = 400;

/// Try to read the current selection over the AT-SPI2 bus.
///
/// `Ok(None)` means "bus reachable, nothing selected" (fall back to copy-probe);
/// `Err` means the accessibility bus is unavailable (also a fallback trigger,
/// but worth logging distinctly).
pub fn get_selection() -> Result<Option<Selection>, String> {
    // A current-thread runtime is enough: all the work is D-Bus round-trips, no
    // CPU-bound parallelism. Created per call so we hold no runtime between
    // requests (selection reads are infrequent and latency-tolerant).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;

    rt.block_on(async {
        match tokio::time::timeout(PROBE_TIMEOUT, probe()).await {
            Ok(res) => res,
            Err(_) => Err("AT-SPI probe timed out".to_string()),
        }
    })
}

async fn probe() -> Result<Option<Selection>, String> {
    // Announce ourselves as an assistive-tech client so toolkits (Qt, GTK) expose
    // their accessible tree + Text interfaces. Without this the focused object's
    // selection is often silently empty on a stock session — and on KDE the
    // active-app provider is the KWin bridge (not the AT-SPI tracker), so nothing
    // else flips this flag. Best-effort + idempotent; a failure isn't fatal.
    if let Err(e) = atspi::connection::set_session_accessibility(true).await {
        log::debug!("atspi_sel: set_session_accessibility(true) failed: {e}");
    }

    let a11y = AccessibilityConnection::new()
        .await
        .map_err(|e| format!("a11y bus connect: {e}"))?;
    let conn = a11y.connection();

    let Some(focused) = find_focused(conn).await? else {
        log::debug!("atspi: no focused accessible found");
        return Ok(None);
    };

    read_selection(conn, &focused).await
}

/// The accessibility hierarchy root: the registry's desktop object. All running
/// applications hang off it as children.
pub(super) async fn desktop_root(conn: &Connection) -> Result<AccessibleProxy<'static>, String> {
    AccessibleProxy::builder(conn)
        .destination("org.a11y.atspi.Registry")
        .map_err(|e| format!("root destination: {e}"))?
        .path("/org/a11y/atspi/accessible/root")
        .map_err(|e| format!("root path: {e}"))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| format!("root proxy: {e}"))
}

/// Locate the focused accessible by walking from the desktop root. Applications
/// are the desktop's direct children; we descend each looking for the node whose
/// state set contains `Focused`. Bounded by depth/node count so a misbehaving
/// provider can't make us walk forever.
pub(super) async fn find_focused(conn: &Connection) -> Result<Option<ObjectRef>, String> {
    let root = desktop_root(conn).await?;
    let apps = root.get_children().await.unwrap_or_default();

    let mut budget = MAX_NODES;
    for app in apps {
        if let Some(found) = descend_for_focus(conn, &app, 0, MAX_DEPTH, &mut budget).await {
            return Ok(Some(found));
        }
        if budget == 0 {
            break;
        }
    }
    Ok(None)
}

/// Depth-first search under `obj` for an accessible holding `State::Focused`.
/// `budget` is a shared node cap decremented across the whole walk.
pub(super) async fn descend_for_focus(
    conn: &Connection,
    obj: &ObjectRef,
    depth: usize,
    max_depth: usize,
    budget: &mut usize,
) -> Option<ObjectRef> {
    if depth > max_depth || *budget == 0 {
        return None;
    }
    *budget -= 1;

    let proxy = obj.as_accessible_proxy(conn).await.ok()?;

    // The focused widget itself reports Focused; return it directly.
    if let Ok(states) = proxy.get_state().await {
        if states.contains(State::Focused) {
            return Some(obj.clone());
        }
    }

    // Otherwise recurse into children. Bail the moment the budget runs out so a
    // huge subtree can't starve sibling applications.
    let children = proxy.get_children().await.ok()?;
    for child in children {
        if *budget == 0 {
            break;
        }
        if let Some(found) = Box::pin(descend_for_focus(
            conn,
            &child,
            depth + 1,
            max_depth,
            budget,
        ))
        .await
        {
            return Some(found);
        }
    }
    None
}

/// Read the selection from `obj`'s Text interface. Returns `Ok(None)` when the
/// object has no Text interface or no (non-empty) selection.
async fn read_selection(conn: &Connection, obj: &ObjectRef) -> Result<Option<Selection>, String> {
    let text = TextProxy::builder(conn)
        .destination(obj.name.as_str())
        .map_err(|e| format!("text destination: {e}"))?
        .path(obj.path.as_str())
        .map_err(|e| format!("text path: {e}"))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        // No Text interface on the focused object: not an error, just nothing
        // for us to read — let the caller fall back.
        .map_err(|e| format!("text proxy: {e}"))?;

    let n = match text.get_nselections().await {
        Ok(n) => n,
        // GetNSelections fails when the object doesn't really implement Text.
        Err(_) => return Ok(None),
    };
    if n <= 0 {
        return Ok(None);
    }

    // Concatenate every active selection range (most apps expose exactly one).
    let mut selected = String::new();
    for i in 0..n {
        let (start, end) = match text.get_selection(i).await {
            Ok(range) => range,
            Err(_) => continue,
        };
        if end <= start {
            continue;
        }
        if let Ok(s) = text.get_text(start, end).await {
            selected.push_str(&s);
        }
    }

    if selected.is_empty() {
        return Ok(None);
    }

    // `contents` is the full text of the focused field when cheaply available;
    // best-effort, and capped so a huge document doesn't bloat the IPC reply.
    let contents = match text.character_count().await {
        Ok(count) if count > 0 && count <= 100_000 => {
            text.get_text(0, count).await.unwrap_or_default()
        }
        _ => String::new(),
    };

    Ok(Some(Selection {
        selected_text: selected,
        contents,
        before_text: String::new(),
        after_text: String::new(),
    }))
}
