//! AT-SPI2 text context for dictation: `TextBoxInfo` + `AppContextUpdate`.
//!
//! Wispr Flow formats a dictation with the text around the cursor: the focused
//! field split at the caret (`TextBoxInfo`) and the text shown near it, e.g. the
//! conversation being replied to (`AppContextUpdate`, whose `nearestTexts` also
//! seeds proper-noun extraction). The macOS helper reads both from the AX tree
//! when `DictationStart` arrives and pushes them as helper-initiated requests;
//! without them every transcription goes out with no context. This module
//! reads the same over the accessibility bus.
//!
//! * A focus watcher keeps the source of the last `object:state-changed:focused`
//!   event, so the focused widget is known without searching the tree (web
//!   content trees are deep). A bounded search of the active window covers the
//!   time before the first focus event.
//! * A worker thread serves `DictationStart` and `GetTextBoxInfo` off the
//!   dispatch thread, each under a time budget.
//!
//! A focused widget only counts while its window is active, so text from a
//! background window never leaks into a dictation aimed at an app without
//! accessibility (most terminals). Password fields are never read. Context is
//! pushed only while the app's context-awareness setting
//! (`prefs.user.useAxContext`, default on) is enabled, like the macOS helper.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use atspi::connection::AccessibilityConnection;
use atspi::events::object::{ObjectEvents, StateChangedEvent};
use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::text::TextProxy;
use atspi::{Event, Interface, ObjectRef, Role, State};
use serde_json::{json, Map, Value};
use zbus::export::futures_util::stream::StreamExt;
use zbus::Connection;

use super::EventSink;

/// Budget for locating the focused widget and reading its text.
const FOCUS_BUDGET: Duration = Duration::from_millis(400);
/// Budget for gathering the text around the focused widget.
const NEAREST_BUDGET: Duration = Duration::from_millis(700);
/// Delay before re-subscribing to focus events after the a11y bus went away.
const FOCUS_RETRY: Duration = Duration::from_secs(5);

/// Caps keep every frame well under the app's 30 000-char IPC limit even after
/// JSON and frame escaping.
const MAX_BEFORE: i32 = 2000;
const MAX_SELECTED: i32 = 2000;
const MAX_AFTER: i32 = 1000;
const MAX_NEAREST_CHARS: usize = 6000;
const MAX_NODE_CHARS: i32 = 500;
/// Bounds for the tree walks (focus search and nearest-text collection).
const MAX_NEAREST_NODES: usize = 800;
const MAX_FOCUS_NODES: usize = 1500;
const MAX_DEPTH: usize = 64;

/// The last widget reported focused by AT-SPI.
type FocusSlot = Arc<Mutex<Option<ObjectRef>>>;

enum Job {
    DictationStart { include_offscreen: bool },
    GetTextBoxInfo { uuid: String },
}

/// Handle to the context worker; cheap to call from the dispatch thread.
pub struct ContextService {
    tx: mpsc::Sender<Job>,
}

impl ContextService {
    /// Spawn the focus watcher and the context worker. Never fails: when the
    /// a11y bus is unreachable each request reports `accessibilityIsFunctioning:
    /// false` and the watcher keeps retrying.
    pub fn start(events: EventSink) -> ContextService {
        let focus = FocusSlot::default();
        let (tx, rx) = mpsc::channel();

        let watcher_focus = focus.clone();
        let watcher = std::thread::Builder::new()
            .name("atspi-focus-cache".into())
            .spawn(move || {
                if let Some(rt) = runtime() {
                    rt.block_on(track_focus(&watcher_focus));
                }
            });
        if let Err(e) = watcher {
            log::warn!("atspi context: focus watcher not started: {e}");
        }

        let worker = std::thread::Builder::new()
            .name("atspi-context".into())
            .spawn(move || serve(&rx, &focus, &events));
        if let Err(e) = worker {
            log::warn!("atspi context: worker not started: {e}");
        }

        ContextService { tx }
    }

    /// `DictationStart`: push `TextBoxInfo` then `AppContextUpdate` for the
    /// focused widget.
    pub fn on_dictation_start(&self, include_offscreen: bool) {
        let _ = self.tx.send(Job::DictationStart { include_offscreen });
    }

    /// `GetTextBoxInfo`: answer with a `TextBoxInfo` response echoing `uuid`.
    pub fn get_text_box_info(&self, uuid: &str) {
        let _ = self.tx.send(Job::GetTextBoxInfo {
            uuid: uuid.to_string(),
        });
    }
}

fn runtime() -> Option<tokio::runtime::Runtime> {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => Some(rt),
        Err(e) => {
            log::warn!("atspi context: tokio runtime: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Focus tracking
// ---------------------------------------------------------------------------

/// Keep `slot` pointing at the last focused widget, re-subscribing whenever the
/// a11y bus goes away (it can be restarted under a running session).
async fn track_focus(slot: &FocusSlot) {
    loop {
        if let Err(e) = watch_focus(slot).await {
            log::debug!("atspi context: focus watch stopped ({e}); retrying");
        }
        if let Ok(mut g) = slot.lock() {
            *g = None;
        }
        tokio::time::sleep(FOCUS_RETRY).await;
    }
}

async fn watch_focus(slot: &FocusSlot) -> Result<(), String> {
    // Toolkits only expose their tree once an assistive client asked for it.
    if let Err(e) = atspi::connection::set_session_accessibility(true).await {
        log::debug!("atspi context: set_session_accessibility(true) failed: {e}");
    }
    let a11y = AccessibilityConnection::new()
        .await
        .map_err(|e| format!("a11y bus connect: {e}"))?;
    a11y.register_event::<StateChangedEvent>()
        .await
        .map_err(|e| format!("register focus events: {e}"))?;
    let mut events = std::pin::pin!(a11y.event_stream());
    while let Some(event) = events.next().await {
        if let Ok(Event::Object(ObjectEvents::StateChanged(e))) = event {
            if e.state == State::Focused && e.enabled == 1 {
                if let Ok(mut g) = slot.lock() {
                    *g = Some(e.item);
                }
            }
        }
    }
    Err("event stream ended".into())
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

fn serve(rx: &mpsc::Receiver<Job>, focus: &FocusSlot, events: &EventSink) {
    let Some(rt) = runtime() else {
        return;
    };
    let pid = std::process::id();
    let counter = AtomicU64::new(0);
    let event_uuid = || format!("ctx-{pid}-{}", counter.fetch_add(1, Ordering::Relaxed));

    for job in rx {
        match job {
            Job::DictationStart { include_offscreen } => {
                // The app's context-awareness switch; on unless explicitly off.
                if crate::prefs::user_bool("useAxContext") == Some(false) {
                    log::debug!("atspi context: useAxContext is off; not capturing");
                    continue;
                }
                rt.block_on(capture_dictation_context(
                    focus,
                    include_offscreen,
                    events,
                    &event_uuid,
                ));
            }
            Job::GetTextBoxInfo { uuid } => {
                let info = rt.block_on(async {
                    let Ok(conn) = connect().await else {
                        return TextBoxInfo::unavailable(false);
                    };
                    match within(FOCUS_BUDGET, focused_widget(&conn, focus)).await {
                        Some(Some(widget)) => {
                            within(FOCUS_BUDGET, read_text_box(&conn, &widget.object))
                                .await
                                .unwrap_or_else(|| TextBoxInfo::unavailable(true))
                        }
                        _ => TextBoxInfo::unavailable(true),
                    }
                });
                let _ = events.send(crate::proto::response("TextBoxInfo", info.payload(), &uuid));
            }
        }
    }
}

async fn capture_dictation_context(
    focus: &FocusSlot,
    include_offscreen: bool,
    events: &EventSink,
    event_uuid: &dyn Fn() -> String,
) {
    let send = |name: &str, body: Value| {
        let _ = events.send(crate::proto::request(name, body, &event_uuid()));
    };
    let conn = match connect().await {
        Ok(c) => c,
        Err(e) => {
            log::debug!("atspi context: {e}");
            send("TextBoxInfo", TextBoxInfo::unavailable(false).payload());
            return;
        }
    };
    let Some(Some(widget)) = within(FOCUS_BUDGET, focused_widget(&conn, focus)).await else {
        log::debug!("atspi context: no focused accessible in the active window");
        send("TextBoxInfo", TextBoxInfo::unavailable(true).payload());
        return;
    };

    let info = within(FOCUS_BUDGET, read_text_box(&conn, &widget.object))
        .await
        .unwrap_or_else(|| TextBoxInfo::unavailable(true));
    send("TextBoxInfo", info.payload());

    let started = Instant::now();
    let nearest = within(
        NEAREST_BUDGET,
        nearest_texts(&conn, &widget, include_offscreen),
    )
    .await
    .unwrap_or_default();
    let duration_ms = started.elapsed().as_millis() as u64;
    if nearest.is_empty() && widget.window_title.is_empty() {
        return;
    }
    log::info!(
        "atspi context: {} chars of nearest text in {duration_ms} ms",
        nearest.chars().count()
    );
    send(
        "AppContextUpdate",
        app_context_payload(
            &widget.window_title,
            &nearest,
            &widget.description,
            duration_ms,
        ),
    );
}

async fn connect() -> Result<Connection, String> {
    AccessibilityConnection::new()
        .await
        .map(|a11y| a11y.connection().clone())
        .map_err(|e| format!("a11y bus connect: {e}"))
}

/// `fut` or `None` when it outruns `budget`.
async fn within<F: std::future::Future>(budget: Duration, fut: F) -> Option<F::Output> {
    tokio::time::timeout(budget, fut).await.ok()
}

// ---------------------------------------------------------------------------
// Focused widget
// ---------------------------------------------------------------------------

/// The focused widget plus what the context needs from its surroundings.
struct Widget {
    object: ObjectRef,
    /// Ancestors from the parent up to (and including) the toplevel window.
    ancestors: Vec<ObjectRef>,
    window_title: String,
    /// The widget's accessible description, else its name (`elementDescription`).
    description: String,
}

/// The focused widget of the active window: the focus watcher's last report
/// when still valid, else a bounded search of the active windows.
async fn focused_widget(conn: &Connection, focus: &FocusSlot) -> Option<Widget> {
    let cached = focus.lock().ok().and_then(|g| g.clone());
    if let Some(object) = cached {
        if let Some(widget) = validate_focus(conn, object).await {
            return Some(widget);
        }
    }
    let object = search_focus(conn).await?;
    let widget = validate_focus(conn, object).await?;
    if let Ok(mut g) = focus.lock() {
        *g = Some(widget.object.clone());
    }
    Some(widget)
}

/// `Some` when `object` still holds focus and its toplevel window is active.
async fn validate_focus(conn: &Connection, object: ObjectRef) -> Option<Widget> {
    let proxy = object.as_accessible_proxy(conn).await.ok()?;
    if !proxy.get_state().await.ok()?.contains(State::Focused) {
        return None;
    }
    let description = match proxy.description().await {
        Ok(d) if !d.trim().is_empty() => d.trim().to_string(),
        _ => proxy.name().await.unwrap_or_default().trim().to_string(),
    };

    let mut ancestors = Vec::new();
    let mut window_title = String::new();
    let mut current = object.clone();
    for _ in 0..MAX_DEPTH {
        let parent = match current.as_accessible_proxy(conn).await {
            Ok(proxy) => match proxy.parent().await {
                Ok(parent) => parent,
                Err(_) => break,
            },
            Err(_) => break,
        };
        if is_null(&parent) {
            break;
        }
        let Ok(parent_proxy) = parent.as_accessible_proxy(conn).await else {
            break;
        };
        let role = parent_proxy.get_role().await.ok()?;
        if role == Role::Application {
            break;
        }
        ancestors.push(parent.clone());
        if is_toplevel(role) {
            // A focused widget in an inactive window is stale: the user moved
            // to another app (possibly one without accessibility).
            if !parent_proxy.get_state().await.ok()?.contains(State::Active) {
                return None;
            }
            window_title = parent_proxy
                .name()
                .await
                .unwrap_or_default()
                .trim()
                .to_string();
            break;
        }
        current = parent;
    }

    Some(Widget {
        object,
        ancestors,
        window_title,
        description,
    })
}

/// Depth-first search of every application's active toplevels for a focused
/// widget, skipping hidden subtrees.
async fn search_focus(conn: &Connection) -> Option<ObjectRef> {
    let root = AccessibleProxy::builder(conn)
        .destination("org.a11y.atspi.Registry")
        .ok()?
        .path("/org/a11y/atspi/accessible/root")
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    let mut budget = MAX_FOCUS_NODES;
    for app in root.get_children().await.ok()? {
        let Ok(app_proxy) = app.as_accessible_proxy(conn).await else {
            continue;
        };
        for window in app_proxy.get_children().await.unwrap_or_default() {
            let Ok(window_proxy) = window.as_accessible_proxy(conn).await else {
                continue;
            };
            let active = window_proxy
                .get_state()
                .await
                .is_ok_and(|s| s.contains(State::Active));
            if !active {
                continue;
            }
            if let Some(found) = descend_for_focus(conn, &window, 0, &mut budget).await {
                return Some(found);
            }
            if budget == 0 {
                return None;
            }
        }
    }
    None
}

async fn descend_for_focus(
    conn: &Connection,
    object: &ObjectRef,
    depth: usize,
    budget: &mut usize,
) -> Option<ObjectRef> {
    if depth > MAX_DEPTH || *budget == 0 {
        return None;
    }
    *budget -= 1;
    let proxy = object.as_accessible_proxy(conn).await.ok()?;
    let states = proxy.get_state().await.ok()?;
    if states.contains(State::Focused) {
        return Some(object.clone());
    }
    if depth > 0 && !states.contains(State::Showing) {
        return None;
    }
    for child in proxy.get_children().await.ok()? {
        if *budget == 0 {
            break;
        }
        if let Some(found) = Box::pin(descend_for_focus(conn, &child, depth + 1, budget)).await {
            return Some(found);
        }
    }
    None
}

fn is_toplevel(role: Role) -> bool {
    matches!(role, Role::Frame | Role::Window | Role::Dialog)
}

fn is_null(object: &ObjectRef) -> bool {
    object.path.as_str().ends_with("/null")
}

fn object_key(object: &ObjectRef) -> String {
    format!("{}{}", object.name.as_str(), object.path.as_str())
}

// ---------------------------------------------------------------------------
// TextBoxInfo
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq)]
struct TextBoxInfo {
    accessibility_is_functioning: bool,
    could_not_get_text_box_info: bool,
    is_editable: bool,
    before_text: String,
    selected_text: String,
    after_text: String,
    focused_element_hash: Option<String>,
}

impl TextBoxInfo {
    fn unavailable(accessibility_is_functioning: bool) -> TextBoxInfo {
        TextBoxInfo {
            accessibility_is_functioning,
            could_not_get_text_box_info: true,
            ..TextBoxInfo::default()
        }
    }

    /// `TextBoxInfo` body. `contents` is the text window read around the caret
    /// (the whole field when it fits the caps).
    fn payload(&self) -> Value {
        let mut payload = Map::new();
        payload.insert(
            "accessibilityIsFunctioning".into(),
            json!(self.accessibility_is_functioning),
        );
        payload.insert("afterText".into(), json!(self.after_text));
        payload.insert("beforeText".into(), json!(self.before_text));
        payload.insert(
            "contents".into(),
            json!(format!(
                "{}{}{}",
                self.before_text, self.selected_text, self.after_text
            )),
        );
        payload.insert(
            "couldNotGetTextBoxInfo".into(),
            json!(self.could_not_get_text_box_info),
        );
        // Optional: omitted rather than null when unknown.
        if let Some(hash) = &self.focused_element_hash {
            payload.insert("focusedElementHash".into(), json!(hash));
        }
        payload.insert("isEditable".into(), json!(self.is_editable));
        payload.insert("selectedText".into(), json!(self.selected_text));
        json!({ "payload": payload })
    }
}

/// Read the focused widget's text split at the caret (or around the selection).
async fn read_text_box(conn: &Connection, object: &ObjectRef) -> TextBoxInfo {
    let mut info = TextBoxInfo {
        accessibility_is_functioning: true,
        focused_element_hash: Some(element_hash(object)),
        ..TextBoxInfo::default()
    };
    let Ok(proxy) = object.as_accessible_proxy(conn).await else {
        info.could_not_get_text_box_info = true;
        return info;
    };
    info.is_editable = proxy
        .get_state()
        .await
        .is_ok_and(|s| s.contains(State::Editable));
    if proxy
        .get_role()
        .await
        .is_ok_and(|r| r == Role::PasswordText)
    {
        return info;
    }
    let has_text = proxy
        .get_interfaces()
        .await
        .is_ok_and(|i| i.contains(Interface::Text));
    let text = match has_text {
        true => text_proxy(conn, object).await,
        false => None,
    };
    let Some(text) = text else {
        info.could_not_get_text_box_info = true;
        return info;
    };

    let count = text.character_count().await.unwrap_or(0).max(0);
    let caret = text.caret_offset().await.unwrap_or(-1);
    // Ask for selection 0 directly: atspi-proxies 0.6 sends `GetNselections`,
    // which toolkits reject (the method is `GetNSelections`). No selection
    // reads as an error or an empty range, both handled by `caret_span`.
    let selection = text.get_selection(0).await.ok();
    let (start, end) = caret_span(count, caret, selection);
    info.before_text = clean(&read(&text, (start - MAX_BEFORE).max(0), start).await);
    info.selected_text = clean(&read(&text, start, end.min(start + MAX_SELECTED)).await);
    info.after_text = clean(&read(&text, end, (end + MAX_AFTER).min(count)).await);
    info
}

/// The `(start, end)` character span the caret/selection covers, clamped to
/// `0..=count`. No caret (e.g. read-only text) reads as a caret at the end.
fn caret_span(count: i32, caret: i32, selection: Option<(i32, i32)>) -> (i32, i32) {
    let clamp = |v: i32| v.clamp(0, count);
    match selection {
        Some((s, e)) if e > s => (clamp(s), clamp(e)),
        _ if caret >= 0 => (clamp(caret), clamp(caret)),
        _ => (count, count),
    }
}

async fn read(text: &TextProxy<'_>, start: i32, end: i32) -> String {
    if end <= start {
        return String::new();
    }
    text.get_text(start, end).await.unwrap_or_default()
}

async fn text_proxy<'a>(conn: &'a Connection, object: &ObjectRef) -> Option<TextProxy<'a>> {
    TextProxy::builder(conn)
        .destination(object.name.as_str().to_owned())
        .ok()?
        .path(object.path.as_str().to_owned())
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()
}

/// Drop the object-replacement characters that stand in for embedded children
/// (links, images) in toolkit text.
fn clean(text: &str) -> String {
    text.replace('\u{fffc}', "")
}

fn element_hash(object: &ObjectRef) -> String {
    let mut hasher = DefaultHasher::new();
    object_key(object).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// ---------------------------------------------------------------------------
// AppContextUpdate
// ---------------------------------------------------------------------------

fn app_context_payload(
    window_title: &str,
    nearest_texts: &str,
    description: &str,
    duration_ms: u64,
) -> Value {
    let mut payload = Map::new();
    payload.insert("conversationId".into(), json!(window_title));
    payload.insert("nearestTexts".into(), json!(nearest_texts));
    if !description.is_empty() {
        payload.insert("elementDescription".into(), json!(description));
    }
    payload.insert("nearestTextDurationMs".into(), json!(duration_ms));
    json!({ "payload": payload })
}

/// Text shown around the focused widget, nearest first: each ancestor's subtree
/// in turn (parent, grandparent, ... window), breadth-first, skipping what an
/// inner ring already covered. Hidden subtrees are skipped unless
/// `include_offscreen`. Stops at the node/char caps; the caller's time budget
/// bounds the rest.
async fn nearest_texts(conn: &Connection, widget: &Widget, include_offscreen: bool) -> String {
    let mut visited = HashSet::from([object_key(&widget.object)]);
    let mut collector = TextCollector::default();
    let mut budget = MAX_NEAREST_NODES;
    for ancestor in &widget.ancestors {
        let mut queue = VecDeque::from([ancestor.clone()]);
        while let Some(object) = queue.pop_front() {
            if budget == 0 || collector.is_full() {
                return collector.finish();
            }
            if !visited.insert(object_key(&object)) {
                continue;
            }
            budget -= 1;
            let Ok(proxy) = object.as_accessible_proxy(conn).await else {
                continue;
            };
            let Ok(states) = proxy.get_state().await else {
                continue;
            };
            if !include_offscreen && !states.contains(State::Showing) {
                continue;
            }
            if let Some(text) = node_text(conn, &object, &proxy).await {
                collector.push(&text);
            }
            queue.extend(proxy.get_children().await.unwrap_or_default());
        }
    }
    collector.finish()
}

/// Visible text of one node: its Text interface content, else the name of a
/// text-bearing role (labels, headings, list items, ...). Controls such as
/// buttons are skipped; their names are UI chrome, not context.
async fn node_text(
    conn: &Connection,
    object: &ObjectRef,
    proxy: &AccessibleProxy<'_>,
) -> Option<String> {
    let role = proxy.get_role().await.ok()?;
    if role == Role::PasswordText {
        return None;
    }
    let has_text = proxy
        .get_interfaces()
        .await
        .is_ok_and(|i| i.contains(Interface::Text));
    if has_text {
        let text = text_proxy(conn, object).await?;
        let count = text.character_count().await.ok()?;
        return Some(clean(&read(&text, 0, count.min(MAX_NODE_CHARS)).await));
    }
    if is_text_role(role) {
        return proxy.name().await.ok();
    }
    None
}

fn is_text_role(role: Role) -> bool {
    matches!(
        role,
        Role::Label
            | Role::Heading
            | Role::Paragraph
            | Role::Static
            | Role::Caption
            | Role::ListItem
            | Role::TableCell
            | Role::Link
            | Role::Comment
            | Role::BlockQuote
            | Role::Article
            | Role::Footnote
    )
}

/// Accumulates distinct, non-blank text snippets up to `MAX_NEAREST_CHARS`.
#[derive(Default)]
struct TextCollector {
    seen: HashSet<String>,
    parts: Vec<String>,
    chars: usize,
}

impl TextCollector {
    fn push(&mut self, text: &str) {
        let text = text.trim();
        if text.chars().count() < 2 || self.is_full() || !self.seen.insert(text.to_string()) {
            return;
        }
        let room = MAX_NEAREST_CHARS - self.chars;
        let part: String = text.chars().take(room).collect();
        self.chars += part.chars().count();
        self.parts.push(part);
    }

    fn is_full(&self) -> bool {
        self.chars >= MAX_NEAREST_CHARS
    }

    fn finish(self) -> String {
        self.parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_span_prefers_selection_then_caret_then_end() {
        assert_eq!(caret_span(10, 3, Some((2, 6))), (2, 6));
        assert_eq!(caret_span(10, 3, Some((4, 4))), (3, 3)); // empty selection
        assert_eq!(caret_span(10, 3, None), (3, 3));
        assert_eq!(caret_span(10, -1, None), (10, 10)); // no caret
        assert_eq!(caret_span(10, 42, Some((8, 99))), (8, 10)); // clamped
    }

    #[test]
    fn clean_drops_embedded_object_characters() {
        assert_eq!(clean("see \u{fffc} here"), "see  here");
    }

    #[test]
    fn collector_dedupes_skips_blanks_and_caps_chars() {
        let mut c = TextCollector::default();
        c.push("  hello  ");
        c.push("hello");
        c.push(" ");
        c.push("x");
        c.push("world");
        assert_eq!(c.finish(), "hello\nworld");

        let mut c = TextCollector::default();
        c.push(&"a".repeat(MAX_NEAREST_CHARS + 10));
        c.push("more");
        assert!(c.is_full());
        assert_eq!(c.finish().chars().count(), MAX_NEAREST_CHARS);
    }

    // Both payloads are decoded by the app's quicktype validator; pin the shapes.
    #[test]
    fn text_box_info_payload_shape() {
        let info = TextBoxInfo {
            accessibility_is_functioning: true,
            is_editable: true,
            before_text: "Hello ".into(),
            selected_text: "big".into(),
            after_text: " world".into(),
            focused_element_hash: Some("abc".into()),
            ..TextBoxInfo::default()
        };
        let p = &info.payload()["payload"];
        assert_eq!(p["beforeText"], "Hello ");
        assert_eq!(p["selectedText"], "big");
        assert_eq!(p["afterText"], " world");
        assert_eq!(p["contents"], "Hello big world");
        assert_eq!(p["isEditable"], true);
        assert_eq!(p["couldNotGetTextBoxInfo"], false);
        assert_eq!(p["accessibilityIsFunctioning"], true);
        assert_eq!(p["focusedElementHash"], "abc");

        let p = &TextBoxInfo::unavailable(false).payload()["payload"];
        assert_eq!(p["couldNotGetTextBoxInfo"], true);
        assert_eq!(p["accessibilityIsFunctioning"], false);
        assert!(p.get("focusedElementHash").is_none());
    }

    #[test]
    fn app_context_payload_shape() {
        let p = &app_context_payload("#general", "hi\nthere", "Message", 12)["payload"];
        assert_eq!(p["conversationId"], "#general");
        assert_eq!(p["nearestTexts"], "hi\nthere");
        assert_eq!(p["elementDescription"], "Message");
        assert_eq!(p["nearestTextDurationMs"], 12);

        let p = &app_context_payload("", "", "", 0)["payload"];
        assert!(p.get("elementDescription").is_none());
    }
}
