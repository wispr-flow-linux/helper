//! In-process Wayland clipboard owner (ext-data-control).
//!
//! `wl-copy` can only serve a single payload, so it can't offer a plain-text and
//! an HTML representation of the same paste simultaneously. The Windows helper
//! sets two formats at once (CF_UNICODETEXT + CF_TEXT); to match that — and to
//! offer `text/html` for rich targets — we own the selection ourselves.
//!
//! We use `ext_data_control_manager_v1` (the standardized successor to wlroots'
//! `zwlr_data_control`), which lets a non-focused client set/own the clipboard
//! without an input-event serial. KWin/wlroots/etc. advertise it. On a session
//! that lacks it, [`set_clipboard`] errors and the caller falls back to wl-copy.
//!
//! Each call spawns a thread to serve the selection. Dictation snapshots the
//! current selection before publishing the transcript and restores it after
//! the target has had time to paste.

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
use rustix::pipe::{pipe_with, PipeFlags};
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::{
    Event as DeviceEvent, ExtDataControlDeviceV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::{
    Event as OfferEvent, ExtDataControlOfferV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::{
    Event as SourceEvent, ExtDataControlSourceV1,
};

use super::{
    clipboard_process::{reply, Reply},
    Result,
};

/// A MIME type plus the bytes to serve for it.
pub type Offer = (String, Vec<u8>);

pub(super) const SETUP_TIMEOUT: Duration = Duration::from_secs(3);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_SNAPSHOT_FORMATS: usize = 64;
const SEND_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
// The synthetic keystroke and selection notification travel over different
// channels. Allow delayed readers in native Wayland and XWayland targets.
const PASTE_WINDOW: Duration = Duration::from_millis(350);

#[derive(Default)]
struct State {
    selection: Option<ExtDataControlOfferV1>,
    source: Option<ObjectId>,
}

type MimeTypes = Mutex<Vec<String>>;
struct Clipboard {
    queue: EventQueue<State>,
    manager: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    state: State,
}

impl Clipboard {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().map_err(|e| format!("wl connect: {e}"))?;
        let (globals, queue) =
            registry_queue_init::<State>(&conn).map_err(|e| format!("registry init: {e}"))?;
        let qh = queue.handle();
        let manager: ExtDataControlManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| "ext_data_control_manager not available")?;
        let seat: WlSeat = globals
            .bind(&qh, 1..=9, ())
            .map_err(|e| format!("wl_seat bind: {e}"))?;
        let device = manager.get_data_device(&seat, &qh, ());
        Ok(Self {
            queue,
            manager,
            device,
            state: State::default(),
        })
    }

    fn sync(&mut self) -> Result<()> {
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn set(&mut self, offers: Vec<Offer>, deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline {
            return Err("clipboard setup timed out".into());
        }
        publish(
            &self.manager,
            &self.device,
            &self.queue.handle(),
            &mut self.state,
            offers,
        );
        self.sync()
    }

    fn serve(&mut self) -> Result<()> {
        while self.state.source.is_some() {
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Set the clipboard and serve it until something else takes ownership.
pub fn set_clipboard(offers: Vec<Offer>) -> Result<()> {
    let (ready, result) = mpsc::sync_channel(1);
    let deadline = Instant::now() + SETUP_TIMEOUT;
    std::thread::spawn(move || {
        match Clipboard::connect().and_then(|mut clipboard| {
            clipboard.set(offers, deadline)?;
            if clipboard.state.source.is_none() {
                return Err("clipboard changed before paste".into());
            }
            Ok(clipboard)
        }) {
            Ok(mut clipboard) => {
                if ready.send(Ok(())).is_ok() {
                    let _ = clipboard.serve();
                }
            }
            Err(error) => {
                let _ = ready.send(Err(error));
            }
        }
    });
    result
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| format!("clipboard setup: {error}"))?
}

pub(super) fn serve_paste(
    offers: Vec<Offer>,
    input: &mut (impl Read + AsFd),
    output: &mut impl Write,
) -> Result<()> {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    let mut clipboard = Clipboard::connect()?;
    let saved = snapshot(&mut clipboard.queue, &mut clipboard.state, deadline)?;
    clipboard.set(offers, deadline)?;

    // A dropped app pipe is an abort. The clipboard owner still restores the
    // previous selection before becoming independent of the app.
    let decision = (|| {
        if clipboard.state.source.is_none() {
            return Err("clipboard changed before paste".into());
        }
        reply(output, Reply::Ready)?;
        if wait_command(&mut clipboard, input)? != Some(b'C') {
            return Ok(false);
        }
        clipboard.sync()?;
        if clipboard.state.source.is_none() {
            return Err("clipboard changed before paste".into());
        }
        reply(output, Reply::Current)?;
        Ok(wait_command(&mut clipboard, input)? == Some(b'P'))
    })();
    if matches!(decision, Ok(true)) {
        dispatch_paste_window(&mut clipboard.queue, &mut clipboard.state)?;
    }
    // Only replace our own temporary selection. A newer copy always wins.
    clipboard.sync()?;
    if clipboard.state.source.is_some() {
        if saved.is_empty() {
            clipboard.device.set_selection(None);
            clipboard.state.source = None;
            clipboard.sync()?;
        } else {
            clipboard.set(saved, Instant::now() + SETUP_TIMEOUT)?;
        }
    }
    let completion = match decision {
        Ok(_) => Reply::Done,
        Err(error) => Reply::Error(error),
    };
    let _ = reply(output, completion);
    clipboard.serve()
}

fn wait_command(clipboard: &mut Clipboard, input: &mut (impl Read + AsFd)) -> Result<Option<u8>> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        clipboard
            .queue
            .dispatch_pending(&mut clipboard.state)
            .map_err(|e| e.to_string())?;
        if clipboard.state.source.is_none() {
            return Ok(None);
        }
        let Some(read) = clipboard.queue.prepare_read() else {
            continue;
        };
        clipboard.queue.flush().map_err(|e| e.to_string())?;
        let mut fds = [
            PollFd::from_borrowed_fd(read.connection_fd(), PollFlags::IN),
            PollFd::new(&*input, PollFlags::IN),
        ];
        if !wait_fds(&mut fds, deadline)? {
            return Ok(None);
        }
        let [display, command] = fds.map(|fd| fd.revents());
        if !display.is_empty() {
            match read.read() {
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                result => {
                    result.map_err(|e| e.to_string())?;
                }
            }
        } else {
            drop(read);
        }
        if !command.is_empty() {
            let mut byte = [0];
            let count = input.read(&mut byte).map_err(|e| e.to_string())?;
            return Ok((count == 1).then_some(byte[0]));
        }
    }
}

fn publish(
    manager: &ExtDataControlManagerV1,
    device: &ExtDataControlDeviceV1,
    qh: &QueueHandle<State>,
    state: &mut State,
    offers: Vec<Offer>,
) {
    let mimes: Vec<_> = offers.iter().map(|(mime, _)| mime.clone()).collect();
    let source = manager.create_data_source(qh, ClipboardSource::new(offers));
    for mime in mimes {
        source.offer(mime);
    }
    device.set_selection(Some(&source));
    state.source = Some(source.id());
}

struct ClipboardSource {
    offers: Vec<(String, Arc<[u8]>)>,
    transfers: Arc<AtomicUsize>,
}

impl ClipboardSource {
    fn new(offers: Vec<Offer>) -> Self {
        Self {
            offers: offers
                .into_iter()
                .map(|(mime, data)| (mime, data.into()))
                .collect(),
            transfers: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn enqueue(&self, mime: &str, fd: OwnedFd) {
        let Some((_, data)) = self.offers.iter().find(|(name, _)| name == mime) else {
            return;
        };
        const MAX_TRANSFERS: usize = 16;
        if self.transfers.fetch_add(1, Ordering::Relaxed) >= MAX_TRANSFERS {
            self.transfers.fetch_sub(1, Ordering::Relaxed);
            log::debug!("clipboard transfer limit reached");
            return;
        }
        let transfers = Arc::clone(&self.transfers);
        let data = Arc::clone(data);
        let deadline = Instant::now() + SEND_TIMEOUT;
        if std::thread::Builder::new()
            .name("clipboard-writer".into())
            .spawn(move || {
                let mut file = File::from(fd);
                if let Err(error) = write_with_timeout(&mut file, &data, deadline) {
                    log::debug!("clipboard send did not complete: {error}");
                }
                transfers.fetch_sub(1, Ordering::Relaxed);
            })
            .is_err()
        {
            self.transfers.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

pub(super) fn write_with_timeout(
    file: &mut File,
    mut data: &[u8],
    deadline: Instant,
) -> Result<()> {
    let flags = fcntl_getfl(&*file).map_err(|error| error.to_string())?;
    fcntl_setfl(&*file, flags | OFlags::NONBLOCK).map_err(|error| error.to_string())?;
    while !data.is_empty() {
        if !wait_fds(&mut [PollFd::new(&*file, PollFlags::OUT)], deadline)? {
            return Err("clipboard transfer timed out".into());
        }
        match file.write(data) {
            Ok(0) => return Err("clipboard transfer pipe is closed".into()),
            Ok(count) => data = &data[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn snapshot(
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Instant,
) -> Result<Vec<Offer>> {
    queue.roundtrip(state).map_err(|e| e.to_string())?;
    let Some(offer) = state.selection.clone() else {
        return Ok(Vec::new());
    };
    let mut mimes = offer
        .data::<MimeTypes>()
        .ok_or("clipboard MIME types unavailable")?
        .lock()
        .map_err(|e| e.to_string())?
        .clone();
    let mut seen = HashSet::new();
    mimes.retain(|mime| seen.insert(mime.clone()));
    if mimes.is_empty() || mimes.len() > MAX_SNAPSHOT_FORMATS {
        return Err(format!(
            "clipboard offered {} formats; cannot preserve it",
            mimes.len()
        ));
    }

    let deadline = (Instant::now() + SNAPSHOT_TIMEOUT).min(deadline);
    let (formats, aliases) = snapshot_formats(mimes);
    if formats.is_empty() {
        return Err("clipboard has no data formats to preserve".into());
    }
    let mut snapshot = Snapshot {
        saved: Vec::with_capacity(formats.len() + aliases.len()),
        remaining: MAX_SNAPSHOT_BYTES,
    };
    let transfers = request_formats(queue, &offer, formats)?;
    read_snapshot(transfers, deadline, &mut snapshot)?;

    if !aliases.is_empty() {
        // Read X11 aliases after the real data. Some owners serialize these
        // conversions and stall the substantive transfers in the same batch.
        let transfers = request_formats(queue, &offer, aliases)?;
        read_snapshot(transfers, deadline, &mut snapshot)?;
    }

    queue.roundtrip(state).map_err(|e| e.to_string())?;
    if state.selection.as_ref() != Some(&offer) {
        return Err("clipboard changed while taking its snapshot".into());
    }
    Ok(snapshot.into_offers())
}

type SnapshotFormat = (usize, String);

fn snapshot_formats(mimes: Vec<String>) -> (Vec<SnapshotFormat>, Vec<SnapshotFormat>) {
    let aliases_are_redundant = mimes.iter().any(|mime| mime == "SAVE_TARGETS")
        && mimes
            .iter()
            .any(|mime| matches!(mime.as_str(), "UTF8_STRING" | "text/plain;charset=utf-8"));
    mimes
        .into_iter()
        .enumerate()
        // SAVE_TARGETS requests clipboard persistence on X11. It carries
        // no data and some owners leave its transfer pipe open.
        .filter(|(_, mime)| mime != "SAVE_TARGETS")
        .partition(|(_, mime)| {
            !aliases_are_redundant
                || !matches!(
                    mime.as_str(),
                    "text/plain" | "COMPOUND_TEXT" | "TEXT" | "STRING"
                )
        })
}

fn request_formats(
    queue: &mut EventQueue<State>,
    offer: &ExtDataControlOfferV1,
    formats: Vec<SnapshotFormat>,
) -> Result<Vec<Transfer>> {
    let mut transfers = Vec::with_capacity(formats.len());
    for (order, mime) in formats {
        let (reader, writer) = pipe_with(PipeFlags::CLOEXEC).map_err(|e| e.to_string())?;
        offer.receive(mime.clone(), writer.as_fd());
        transfers.push(Transfer {
            reader: File::from(reader),
            order,
            mime,
            data: Vec::new(),
        });
    }
    queue.flush().map_err(|e| e.to_string())?;
    Ok(transfers)
}

struct Snapshot {
    saved: Vec<(usize, Offer)>,
    remaining: usize,
}

impl Snapshot {
    fn into_offers(mut self) -> Vec<Offer> {
        self.saved.sort_unstable_by_key(|(order, _)| *order);
        self.saved.into_iter().map(|(_, offer)| offer).collect()
    }
}

struct Transfer {
    reader: File,
    order: usize,
    mime: String,
    data: Vec<u8>,
}

fn read_snapshot(
    mut transfers: Vec<Transfer>,
    deadline: Instant,
    snapshot: &mut Snapshot,
) -> Result<()> {
    let mut buffer = [0; 8192];
    while !transfers.is_empty() {
        let ready: Vec<_> = {
            let mut fds: Vec<_> = transfers
                .iter()
                .map(|transfer| PollFd::new(&transfer.reader, PollFlags::IN))
                .collect();
            if !wait_fds(&mut fds, deadline)? {
                recover_stalled_text_formats(snapshot, &transfers)?;
                break;
            }
            fds.iter().map(PollFd::revents).collect()
        };

        // Remove finished pipes in reverse order so indices stay valid.
        for (index, flags) in ready.into_iter().enumerate().rev() {
            if !flags.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
                continue;
            }
            let count = match transfers[index].reader.read(&mut buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result.map_err(|e| e.to_string())?,
            };
            if count == 0 {
                let transfer = transfers.swap_remove(index);
                snapshot
                    .saved
                    .push((transfer.order, (transfer.mime, transfer.data)));
            } else {
                snapshot.remaining = snapshot
                    .remaining
                    .checked_sub(count)
                    .ok_or("clipboard snapshot too large")?;
                transfers[index].data.extend_from_slice(&buffer[..count]);
            }
        }
    }
    Ok(())
}

fn recover_stalled_text_formats(snapshot: &mut Snapshot, stalled: &[Transfer]) -> Result<()> {
    let saved = &mut snapshot.saved;
    // COSMIC/XWayland can advertise legacy aliases for browser text even
    // when the owner cannot deliver them. Try them normally first. Keep a
    // snapshot only when we have valid UTF-8 and every missing flavor is a
    // known alias or private metadata for an already captured rich-text copy.
    let utf8 = ["text/plain;charset=utf-8", "UTF8_STRING"]
        .into_iter()
        .find_map(|mime| {
            saved.iter().find_map(|(_, (name, data))| {
                (name == mime && !data.is_empty() && std::str::from_utf8(data).is_ok())
                    .then_some(data)
            })
        });
    let has_html = saved
        .iter()
        .any(|(_, (mime, data))| mime == "text/html" && !data.is_empty());
    // Firefox also offers its private URL flavor when the public copy
    // is a plain URL, with no HTML. Keep the public link in that case.
    let has_plain_url = utf8
        .and_then(|data| std::str::from_utf8(data).ok())
        .and_then(|text| text.split_once("://"))
        .is_some_and(|(scheme, rest)| {
            matches!(scheme, "http" | "https" | "file" | "ftp")
                && !rest.is_empty()
                && !rest.chars().any(char::is_whitespace)
        });
    let optional = |transfer: &Transfer| {
        transfer.data.is_empty()
            && match transfer.mime.as_str() {
                "text/plain" | "COMPOUND_TEXT" | "TEXT" | "STRING" => true,
                "text/x-moz-url-priv" => has_html || has_plain_url,
                _ => false,
            }
    };
    let Some(utf8) = utf8.filter(|_| stalled.iter().all(optional)) else {
        return Err(format!(
            "clipboard snapshot timed out ({} format(s) still pending)",
            stalled.len()
        ));
    };

    // Some Wayland consumers request only the bare MIME name.
    if let Some(plain) = stalled.iter().find(|offer| offer.mime == "text/plain") {
        snapshot.remaining = snapshot
            .remaining
            .checked_sub(utf8.len())
            .ok_or("clipboard snapshot too large")?;
        saved.push((plain.order, (plain.mime.clone(), utf8.clone())));
    }
    log::debug!(
        "preserved clipboard text after {} optional formats stalled",
        stalled.len()
    );
    Ok(())
}

fn dispatch_paste_window(queue: &mut EventQueue<State>, state: &mut State) -> Result<()> {
    let deadline = Instant::now() + PASTE_WINDOW;
    while state.source.is_some() {
        queue.dispatch_pending(state).map_err(|e| e.to_string())?;
        if state.source.is_none() {
            break;
        }
        let Some(read) = queue.prepare_read() else {
            continue;
        };
        queue.flush().map_err(|e| e.to_string())?;
        if !wait_readable(read.connection_fd(), deadline)? {
            break;
        }
        match read.read() {
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
            result => {
                result.map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

fn wait_readable(fd: impl AsFd, deadline: Instant) -> Result<bool> {
    wait_fds(&mut [PollFd::new(&fd, PollFlags::IN)], deadline)
}

fn wait_fds(fds: &mut [PollFd<'_>], deadline: Instant) -> Result<bool> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let timeout = Timespec {
            tv_sec: remaining.as_secs() as i64,
            tv_nsec: remaining.subsec_nanos().into(),
        };
        match poll(fds, Some(&timeout)) {
            Err(rustix::io::Errno::INTR) => continue,
            result => return result.map(|n| n != 0).map_err(|e| e.to_string()),
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ClipboardSource> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: SourceEvent,
        payload: &ClipboardSource,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            SourceEvent::Send { mime_type, fd } => payload.enqueue(&mime_type, fd),
            SourceEvent::Cancelled => {
                if state.source.as_ref().is_some_and(|id| *id == source.id()) {
                    state.source = None;
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: DeviceEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            DeviceEvent::Selection { id } => {
                if let Some(old) = std::mem::replace(&mut state.selection, id) {
                    old.destroy();
                }
            }
            DeviceEvent::PrimarySelection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, MimeTypes::default()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, MimeTypes> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        event: OfferEvent,
        mimes: &MimeTypes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let OfferEvent::Offer { mime_type } = event {
            if let Ok(mut types) = mimes.lock() {
                types.push(mime_type);
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(mime: &str) -> (Transfer, File) {
        let (reader, writer) = pipe_with(PipeFlags::CLOEXEC).unwrap();
        (
            Transfer {
                reader: reader.into(),
                order: 0,
                mime: mime.into(),
                data: Vec::new(),
            },
            writer.into(),
        )
    }

    fn collect_snapshot(
        transfers: Vec<Transfer>,
        deadline: Instant,
        remaining: usize,
    ) -> Result<Vec<Offer>> {
        let mut snapshot = Snapshot {
            saved: Vec::new(),
            remaining,
        };
        read_snapshot(transfers, deadline, &mut snapshot)?;
        Ok(snapshot.into_offers())
    }

    #[test]
    fn snapshot_defers_bridged_text_aliases_and_excludes_x11_control_targets() {
        let mimes = [
            "text/plain",
            "SAVE_TARGETS",
            "UTF8_STRING",
            "COMPOUND_TEXT",
            "text/x-moz-url-priv",
            "image/png",
        ]
        .map(str::to_owned)
        .to_vec();
        let (formats, aliases) = snapshot_formats(mimes);
        assert_eq!(
            formats,
            vec![
                (2, "UTF8_STRING".into()),
                (4, "text/x-moz-url-priv".into()),
                (5, "image/png".into())
            ]
        );
        assert_eq!(
            aliases,
            vec![(0, "text/plain".into()), (3, "COMPOUND_TEXT".into())]
        );
    }

    #[test]
    fn snapshot_preserves_plain_firefox_links_with_a_stalled_private_format() {
        let (public, mut writer) = transfer("UTF8_STRING");
        let link = b"https://example.invalid/item";
        writer.write_all(link).unwrap();
        drop(writer);
        let (private, _private_writer) = transfer("text/x-moz-url-priv");
        let saved = collect_snapshot(
            vec![private, public],
            Instant::now() + Duration::from_millis(50),
            128,
        )
        .unwrap();
        assert_eq!(saved, vec![("UTF8_STRING".into(), link.to_vec())]);
    }

    #[test]
    fn snapshot_requires_private_browser_data_without_a_public_url_or_html() {
        let (public, mut writer) = transfer("UTF8_STRING");
        writer.write_all(b"ordinary text").unwrap();
        drop(writer);
        let (private, _private_writer) = transfer("text/x-moz-url-priv");
        assert!(collect_snapshot(
            vec![private, public],
            Instant::now() + Duration::from_millis(50),
            128,
        )
        .unwrap_err()
        .contains("timed out"));
    }

    #[test]
    fn snapshot_services_another_pipe_while_the_first_waits() {
        let (first, mut first_writer) = transfer("text/plain");
        let (second, mut second_writer) = transfer("application/octet-stream");
        let bytes = vec![254; 256 * 1024];
        let expected = bytes.clone();
        let producer = std::thread::spawn(move || {
            // Some owners serve requests in their own order. The first pipe
            // cannot become ready until we drain the second, larger pipe.
            second_writer.write_all(&bytes).unwrap();
            drop(second_writer);
            first_writer.write_all(b"before").unwrap();
        });
        let mut snapshot = collect_snapshot(
            vec![first, second],
            Instant::now() + Duration::from_secs(2),
            512 * 1024,
        )
        .unwrap();
        producer.join().unwrap();
        snapshot.sort();
        assert_eq!(
            snapshot,
            vec![
                ("application/octet-stream".into(), expected),
                ("text/plain".into(), b"before".to_vec()),
            ]
        );
    }

    #[test]
    fn backpressured_reader_does_not_block_another_reader() {
        let mime = "text/plain";
        let data = vec![b'x'; 256 * 1024];
        let source = ClipboardSource::new(vec![(mime.to_owned(), data.clone())]);
        let (_stall_reader, stall_writer) = pipe_with(PipeFlags::CLOEXEC).unwrap();
        let (reader, writer) = pipe_with(PipeFlags::CLOEXEC).unwrap();

        source.enqueue(mime, stall_writer);
        source.enqueue(mime, writer);
        assert!(wait_readable(&reader, Instant::now() + PASTE_WINDOW).unwrap());

        let mut received = Vec::new();
        File::from(reader).read_to_end(&mut received).unwrap();
        assert_eq!(received, data);
    }

    #[test]
    fn snapshot_recovers_browser_text_when_legacy_targets_stall() {
        let (utf8, mut utf8_writer) = transfer("text/plain;charset=utf-8");
        utf8_writer.write_all("café".as_bytes()).unwrap();
        drop(utf8_writer);
        let (html, mut html_writer) = transfer("text/html");
        html_writer.write_all(b"<b>sample</b>").unwrap();
        drop(html_writer);
        let (plain, _plain_writer) = transfer("text/plain");
        let (legacy, _legacy_writer) = transfer("COMPOUND_TEXT");
        let (private, _private_writer) = transfer("text/x-moz-url-priv");
        let mut saved = collect_snapshot(
            vec![plain, legacy, private, html, utf8],
            Instant::now() + Duration::from_millis(50),
            128,
        )
        .unwrap();
        saved.sort();
        assert_eq!(
            saved,
            vec![
                ("text/html".into(), b"<b>sample</b>".to_vec()),
                ("text/plain".into(), "café".as_bytes().to_vec()),
                (
                    "text/plain;charset=utf-8".into(),
                    "café".as_bytes().to_vec()
                )
            ]
        );
    }

    #[test]
    fn snapshot_still_rejects_a_stalled_unique_format_with_readable_text() {
        let (utf8, mut writer) = transfer("UTF8_STRING");
        writer.write_all(b"sample").unwrap();
        drop(writer);
        let (image, _image_writer) = transfer("image/png");
        assert!(collect_snapshot(
            vec![utf8, image],
            Instant::now() + Duration::from_millis(50),
            128,
        )
        .unwrap_err()
        .contains("timed out"));
    }

    #[test]
    fn snapshot_rejects_a_stalled_owner_and_oversized_data() {
        let (stalled, _writer) = transfer("text/plain");
        assert!(collect_snapshot(vec![stalled], Instant::now(), 64)
            .unwrap_err()
            .contains("timed out"));
        let (large, mut writer) = transfer("application/octet-stream");
        writer.write_all(b"too large").unwrap();
        drop(writer);
        assert_eq!(
            collect_snapshot(vec![large], Instant::now() + Duration::from_secs(1), 4),
            Err("clipboard snapshot too large".into())
        );
    }
}
