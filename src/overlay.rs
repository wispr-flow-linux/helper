//! The optional dictation indicator runs as a separate Wayland client.
//! Only the current phase crosses its stdin/stdout pipes.

use std::io::{self, Write};
use std::os::fd::AsFd;

use rustix::event::{poll, PollFd, PollFlags};
use rustix::fs::{memfd_create, MemfdFlags};
use tiny_skia::{Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_region};
use wayland_client::protocol::{wl_registry, wl_shm};
use wayland_client::protocol::{wl_shm_pool, wl_surface};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1};
use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1};

type OverlayError = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, OverlayError>;

const WIDTH: u32 = 68;
const HEIGHT: u32 = 32;
const SCALE: i32 = 2;

#[derive(Clone, Copy)]
enum Phase {
    Starting,
    Listening,
    Processing,
}

struct Surface {
    wl: wl_surface::WlSurface,
    role: ZwlrLayerSurfaceV1,
    phase: Phase,
    revision: u64,
    configured: bool,
}

impl Surface {
    fn destroy(self) {
        self.role.destroy();
        self.wl.destroy();
    }
}

struct Overlay {
    compositor: wl_compositor::WlCompositor,
    shell: ZwlrLayerShellV1,
    shm: wl_shm::WlShm,
    surface: Option<Surface>,
    error: Option<OverlayError>,
}

pub fn run() -> Result<()> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<Overlay>(&connection)?;
    let qh = queue.handle();
    let mut overlay = Overlay {
        compositor: globals.bind(&qh, 3..=6, ())?,
        shell: globals.bind(&qh, 1..=4, ())?,
        shm: globals.bind(&qh, 1..=1, ())?,
        surface: None,
        error: None,
    };
    let stdin = io::stdin();
    let mut incoming = [0; 256];
    let mut pending = String::new();

    loop {
        queue.dispatch_pending(&mut overlay)?;
        if let Some(error) = overlay.error.take() {
            return Err(error);
        }
        let Some(read) = queue.prepare_read() else {
            continue;
        };
        let socket_interest = match queue.flush() {
            Ok(()) => PollFlags::IN,
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                PollFlags::IN | PollFlags::OUT
            }
            Err(error) => return Err(error.into()),
        };
        let socket = read.connection_fd();
        let mut fds = [
            PollFd::from_borrowed_fd(socket, socket_interest),
            PollFd::new(&stdin, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Err(rustix::io::Errno::INTR) => continue,
            result => result?,
        };
        let [wayland, input] = fds.map(|fd| fd.revents());
        let disconnected = PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL;
        if wayland.intersects(disconnected) {
            return Err("Wayland disconnected".into());
        }
        if wayland.contains(PollFlags::IN) {
            match read.read() {
                // Wayland may consume an internal event without dispatching
                // anything to this queue.
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                result => {
                    result?;
                }
            }
        } else {
            drop(read);
        }
        if input.intersects(PollFlags::IN | disconnected) {
            // Use the fd directly so buffered stdin cannot hide a ready command
            // from poll. Each line contains only a revision and a phase.
            let count = rustix::io::read(stdin.as_fd(), &mut incoming)?;
            if count == 0 {
                return Ok(());
            }
            pending.push_str(std::str::from_utf8(&incoming[..count])?);
            while let Some((line, rest)) = pending.split_once('\n') {
                overlay.handle_line(line, &qh)?;
                pending = rest.to_owned();
            }
            if pending.len() > 64 {
                return Err("dictation overlay command too long".into());
            }
        }
    }
}

impl Overlay {
    fn handle_line(&mut self, line: &str, qh: &QueueHandle<Self>) -> Result<()> {
        let Some((revision, phase)) = line.split_once(' ') else {
            return Err("invalid dictation overlay command".into());
        };
        let revision = revision.parse()?;
        let phase = match phase {
            "starting" => Phase::Starting,
            "listening" => Phase::Listening,
            "processing" => Phase::Processing,
            "idle" => {
                if let Some(surface) = self.surface.take() {
                    surface.destroy();
                }
                return Ok(());
            }
            _ => return Err("invalid dictation overlay phase".into()),
        };
        if let Some(surface) = &mut self.surface {
            surface.revision = revision;
            surface.phase = phase;
            if surface.configured {
                Self::paint(&self.shm, surface, qh)?;
            }
            return Ok(());
        }
        let wl = self.compositor.create_surface(qh, ());
        let region = self.compositor.create_region(qh, ());
        wl.set_input_region(Some(&region));
        region.destroy();
        wl.set_buffer_scale(SCALE);

        // With no output specified COSMIC picks its active seat's output.
        let role = self.shell.get_layer_surface(
            &wl,
            None,
            Layer::Overlay,
            "wispr-flow-dictation".into(),
            qh,
            (),
        );
        role.set_anchor(Anchor::Bottom);
        role.set_size(WIDTH, HEIGHT);
        role.set_margin(0, 0, 12, 0);
        role.set_exclusive_zone(0);
        role.set_keyboard_interactivity(KeyboardInteractivity::None);
        wl.commit();
        self.surface = Some(Surface {
            wl,
            role,
            phase,
            revision,
            configured: false,
        });
        Ok(())
    }

    fn paint(shm: &wl_shm::WlShm, surface: &Surface, qh: &QueueHandle<Self>) -> Result<()> {
        let pixmap = indicator(surface.phase)?;
        let fd = memfd_create("wispr-flow-dictation", MemfdFlags::CLOEXEC)?;
        let mut file = std::fs::File::from(fd);
        let mut argb = Vec::with_capacity(pixmap.data().len());
        for rgba in pixmap.data().as_chunks::<4>().0 {
            let pixel = u32::from_be_bytes([rgba[3], rgba[0], rgba[1], rgba[2]]);
            argb.extend_from_slice(&pixel.to_ne_bytes());
        }
        file.write_all(&argb)?;
        let width = pixmap.width() as i32;
        let height = pixmap.height() as i32;
        let pool = shm.create_pool(file.as_fd(), argb.len() as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        pool.destroy();
        surface.wl.attach(Some(&buffer), 0, 0);
        surface.wl.damage(0, 0, WIDTH as i32, HEIGHT as i32);
        // Keep the Electron fallback until the compositor services a frame.
        surface.wl.frame(qh, surface.revision);
        surface.wl.commit();
        Ok(())
    }
}

fn indicator(phase: Phase) -> Result<Pixmap> {
    let transform = Transform::from_scale(SCALE as f32, SCALE as f32);
    let mut pixmap = Pixmap::new(WIDTH * SCALE as u32, HEIGHT * SCALE as u32)
        .ok_or("could not allocate overlay buffer")?;
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    let mut background = PathBuilder::new();
    background.move_to(16.0, 0.5);
    background.line_to(52.0, 0.5);
    background.cubic_to(72.7, 0.5, 72.7, 31.5, 52.0, 31.5);
    background.line_to(16.0, 31.5);
    background.cubic_to(-4.7, 31.5, -4.7, 0.5, 16.0, 0.5);
    background.close();
    let path = background.finish().ok_or("invalid indicator shape")?;
    paint.set_color_rgba8(28, 32, 42, 250);
    pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, transform, None);
    paint.set_color_rgba8(161, 177, 204, 130);
    pixmap.stroke_path(&path, &paint, &Stroke::default(), transform, None);

    match phase {
        Phase::Starting | Phase::Processing => {
            for (x, alpha) in [(26.0, 155), (34.0, 204), (42.0, 255)] {
                let dot = PathBuilder::from_circle(x, 16.0, 2.4).ok_or("invalid dot")?;
                paint.set_color_rgba8(205, 217, 245, alpha);
                pixmap.fill_path(&dot, &paint, tiny_skia::FillRule::Winding, transform, None);
            }
        }
        Phase::Listening => {
            let dot = PathBuilder::from_circle(18.0, 16.0, 3.7).ok_or("invalid record dot")?;
            paint.set_color_rgba8(255, 108, 122, 255);
            pixmap.fill_path(&dot, &paint, tiny_skia::FillRule::Winding, transform, None);
            paint.set_color_rgba8(248, 250, 253, 255);
            for (x, height) in [(33.0, 5.0), (39.0, 10.0), (45.0, 16.0), (51.0, 9.0)] {
                let bar = Rect::from_xywh(x, 16.0 - height / 2.0, 2.5, height)
                    .ok_or("invalid waveform bar")?;
                pixmap.fill_rect(bar, &paint, transform, None);
            }
        }
    }
    Ok(pixmap)
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Overlay {
    fn event(
        overlay: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(surface) = overlay
            .surface
            .as_mut()
            .filter(|surface| surface.role == *proxy)
        else {
            return;
        };
        let result = match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                proxy.ack_configure(serial);
                surface.configured = true;
                Self::paint(&overlay.shm, surface, qh)
            }
            zwlr_layer_surface_v1::Event::Closed => {
                let result = writeln!(io::stdout().lock(), "{} lost", surface.revision);
                if let Some(surface) = overlay.surface.take() {
                    surface.destroy();
                }
                result.map_err(Into::into)
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            overlay.error = Some(error);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u64> for Overlay {
    fn event(
        overlay: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        revision: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if overlay
            .surface
            .as_ref()
            .is_some_and(|surface| surface.revision == *revision)
        {
            if let Err(error) = writeln!(io::stdout().lock(), "{revision} shown") {
                overlay.error = Some(error.into());
            }
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Overlay {
    fn event(
        _: &mut Self,
        proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            proxy.destroy();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Overlay {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Overlay: ignore wl_compositor::WlCompositor);
delegate_noop!(Overlay: ignore wl_region::WlRegion);
delegate_noop!(Overlay: ignore wl_shm::WlShm);
delegate_noop!(Overlay: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Overlay: ignore wl_surface::WlSurface);
delegate_noop!(Overlay: ignore ZwlrLayerShellV1);
