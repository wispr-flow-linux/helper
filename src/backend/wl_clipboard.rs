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

use super::Result;

/// A MIME type plus the bytes to serve for it.
pub type Offer = (String, Vec<u8>);

const SETUP_TIMEOUT: Duration = Duration::from_secs(3);
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
type Completion = mpsc::Sender<Result<()>>;

/// Set the clipboard and serve it until something else takes ownership.
pub fn set_clipboard(offers: Vec<Offer>) -> Result<()> {
    match start_owner(offers, None)? {
        true => Ok(()),
        false => Err("ext_data_control_manager not available".into()),
    }
}

/// Publish a temporary clipboard. Returns None if the protocol is unavailable.
/// Wait for the receiver after injecting paste so successive dictations cannot
/// snapshot each other's temporary selections.
pub fn prepare_paste(offers: Vec<Offer>) -> Result<Option<mpsc::Receiver<Result<()>>>> {
    let (tx, rx) = mpsc::channel();
    Ok(start_owner(offers, Some(tx))?.then_some(rx))
}

fn start_owner(offers: Vec<Offer>, restore: Option<Completion>) -> Result<bool> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let deadline = Instant::now() + SETUP_TIMEOUT;
    std::thread::spawn(move || {
        if let Err(error) = setup_and_serve(offers, restore, &ready_tx, deadline) {
            // A disconnected receiver means the caller has already started
            // pasting. Its completion channel handles restoration errors.
            let _ = ready_tx.try_send(Err(error));
        }
    });
    ready_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|e| format!("clipboard setup: {e}"))?
}

fn setup_and_serve(
    offers: Vec<Offer>,
    restore: Option<Completion>,
    ready: &mpsc::SyncSender<Result<bool>>,
    deadline: Instant,
) -> Result<()> {
    let conn = Connection::connect_to_env().map_err(|e| format!("wl connect: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("registry init: {e}"))?;
    let qh = queue.handle();
    let manager: ExtDataControlManagerV1 = match globals.bind(&qh, 1..=1, ()) {
        Ok(manager) => manager,
        Err(_) => {
            let _ = ready.send(Ok(false));
            return Ok(());
        }
    };
    let seat: WlSeat = globals
        .bind(&qh, 1..=9, ())
        .map_err(|e| format!("wl_seat bind: {e}"))?;
    let device = manager.get_data_device(&seat, &qh, ());
    let mut state = State::default();

    let saved = if restore.is_some() {
        Some(snapshot(&mut queue, &mut state, deadline)?)
    } else {
        None
    };
    if Instant::now() >= deadline {
        return Err("clipboard setup timed out".into());
    }
    publish(&manager, &device, &qh, &mut state, offers)?;
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    if state.source.is_none() {
        return Err("clipboard changed before paste".into());
    }
    let caller_ready = ready.send(Ok(true)).is_ok();

    if let (Some(saved), Some(complete)) = (saved, restore) {
        let result = (|| {
            if caller_ready {
                dispatch_paste_window(&mut queue, &mut state)?;
            }
            // Process cancellation before restoring so a copy made during
            // the paste window takes precedence over the saved selection.
            queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
            if state.source.is_some() {
                if saved.is_empty() {
                    device.set_selection(None);
                    state.source = None;
                } else {
                    publish(&manager, &device, &qh, &mut state, saved)?;
                }
                queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
            }
            Ok(())
        })();
        let _ = complete.send(result);
    }

    while state.source.is_some() {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn publish(
    manager: &ExtDataControlManagerV1,
    device: &ExtDataControlDeviceV1,
    qh: &QueueHandle<State>,
    state: &mut State,
    offers: Vec<Offer>,
) -> Result<()> {
    let mimes: Vec<_> = offers.iter().map(|(mime, _)| mime.clone()).collect();
    let source = manager.create_data_source(qh, ClipboardSource::new(offers)?);
    for mime in mimes {
        source.offer(mime);
    }
    device.set_selection(Some(&source));
    state.source = Some(source.id());
    Ok(())
}

struct ClipboardSource {
    offers: Vec<(String, Arc<[u8]>)>,
    writer: mpsc::SyncSender<(OwnedFd, Arc<[u8]>, Instant)>,
}

impl ClipboardSource {
    fn new(offers: Vec<Offer>) -> Result<Self> {
        let (writer, transfers) = mpsc::sync_channel::<(OwnedFd, Arc<[u8]>, Instant)>(16);
        std::thread::Builder::new()
            .name("clipboard-writer".into())
            .spawn(move || {
                for (fd, data, deadline) in transfers {
                    if let Err(error) = send_offer(File::from(fd), &data, deadline) {
                        log::debug!("clipboard send did not complete: {error}");
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            offers: offers
                .into_iter()
                .map(|(mime, data)| (mime, data.into()))
                .collect(),
            writer,
        })
    }

    fn enqueue(&self, mime: &str, fd: OwnedFd) {
        if let Some((_, data)) = self.offers.iter().find(|(name, _)| name == mime) {
            // Keep the compositor queue responsive even if a history manager
            // requests a selection and then stops reading from its pipe.
            let deadline = Instant::now() + SEND_TIMEOUT;
            if self
                .writer
                .try_send((fd, Arc::clone(data), deadline))
                .is_err()
            {
                log::debug!("clipboard transfer queue is full or closed");
            }
        }
    }
}

fn send_offer(mut file: File, mut data: &[u8], deadline: Instant) -> Result<()> {
    let flags = fcntl_getfl(&file).map_err(|error| error.to_string())?;
    fcntl_setfl(&file, flags | OFlags::NONBLOCK).map_err(|error| error.to_string())?;
    while !data.is_empty() {
        if !wait_fds(&mut [PollFd::new(&file, PollFlags::OUT)], deadline)? {
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

    // Request all formats before reading so a slow owner cannot consume the
    // entire deadline before the other formats are even requested.
    let mut transfers = Vec::with_capacity(mimes.len());
    for (order, mime) in mimes.into_iter().enumerate() {
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
    let saved = read_snapshot(
        transfers,
        (Instant::now() + SNAPSHOT_TIMEOUT).min(deadline),
        MAX_SNAPSHOT_BYTES,
    )?;

    queue.roundtrip(state).map_err(|e| e.to_string())?;
    if state.selection.as_ref() != Some(&offer) {
        return Err("clipboard changed while taking its snapshot".into());
    }
    Ok(saved)
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
    mut remaining: usize,
) -> Result<Vec<Offer>> {
    let mut saved = Vec::with_capacity(transfers.len());
    let mut buffer = [0; 8192];
    while !transfers.is_empty() {
        let ready: Vec<_> = {
            let mut fds: Vec<_> = transfers
                .iter()
                .map(|transfer| PollFd::new(&transfer.reader, PollFlags::IN))
                .collect();
            if !wait_fds(&mut fds, deadline)? {
                return Err(format!(
                    "clipboard snapshot timed out ({} format(s) still pending)",
                    transfers.len()
                ));
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
                saved.push((transfer.order, (transfer.mime, transfer.data)));
            } else {
                remaining = remaining
                    .checked_sub(count)
                    .ok_or("clipboard snapshot too large")?;
                transfers[index].data.extend_from_slice(&buffer[..count]);
            }
        }
    }
    saved.sort_unstable_by_key(|(order, _)| *order);
    Ok(saved.into_iter().map(|(_, offer)| offer).collect())
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
        let mut snapshot = read_snapshot(
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
    fn snapshot_rejects_a_stalled_owner_and_oversized_data() {
        let (stalled, _writer) = transfer("text/plain");
        assert!(read_snapshot(vec![stalled], Instant::now(), 64)
            .unwrap_err()
            .contains("timed out"));
        let (large, mut writer) = transfer("application/octet-stream");
        writer.write_all(b"too large").unwrap();
        drop(writer);
        assert_eq!(
            read_snapshot(vec![large], Instant::now() + Duration::from_secs(1), 4),
            Err("clipboard snapshot too large".into())
        );
    }
}
