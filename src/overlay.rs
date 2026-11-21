//! The optional dictation indicator runs as a separate Wayland client.
//! Only the current phase crosses its stdin/stdout pipes.

use std::io::{self, Write};
use std::os::fd::AsFd;

use rustix::event::{poll, PollFd, PollFlags};
use rustix::fs::{memfd_create, MemfdFlags};
use tiny_skia::{Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_output, wl_region};
use wayland_client::protocol::{wl_registry, wl_shm};
use wayland_client::protocol::{wl_shm_pool, wl_surface};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1};
use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1};

type OverlayError = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, OverlayError>;

const WIDTH: u32 = 68;
const HEIGHT: u32 = 32;
const SCALE_UNIT: u32 = 120;
const DEFAULT_SCALE: u32 = 2 * SCALE_UNIT;

#[derive(Clone, Copy)]
enum Phase {
    Starting,
    Listening,
    Processing,
}

struct Frame {
    surface: ObjectId,
    revision: u64,
}

struct Surface {
    wl: wl_surface::WlSurface,
    role: ZwlrLayerSurfaceV1,
    phase: Phase,
    revision: u64,
    configured: bool,
    output: Option<wl_output::WlOutput>,
    scaling: Option<(WpFractionalScaleV1, WpViewport)>,
    scale: u32,
}

impl Surface {
    fn destroy(self) {
        self.role.destroy();
        if let Some((fractional, viewport)) = self.scaling {
            fractional.destroy();
            viewport.destroy();
        }
        self.wl.destroy();
    }
}

struct Overlay {
    compositor: wl_compositor::WlCompositor,
    shell: ZwlrLayerShellV1,
    shm: wl_shm::WlShm,
    scaling: Option<(WpFractionalScaleManagerV1, WpViewporter)>,
    toplevels: Option<crate::cosmic::Toplevels>,
    surface: Option<Surface>,
    error: Option<OverlayError>,
}

pub fn run() -> Result<()> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<Overlay>(&connection)?;
    let qh = queue.handle();
    let scaling = globals
        .bind(&qh, 1..=1, ())
        .ok()
        .zip(globals.bind(&qh, 1..=1, ()).ok());
    let toplevels = crate::cosmic::Toplevels::connect(&connection).ok();
    let mut overlay = Overlay {
        compositor: globals.bind(&qh, 3..=6, ())?,
        shell: globals.bind(&qh, 1..=4, ())?,
        shm: globals.bind(&qh, 1..=1, ())?,
        scaling,
        toplevels,
        surface: None,
        error: None,
    };
    let stdin = io::stdin();
    let mut incoming = [0; 256];
    let mut pending = String::new();

    loop {
        if let Some(toplevels) = &mut overlay.toplevels {
            toplevels.dispatch_pending()?;
        }
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
        let output = self
            .toplevels
            .as_ref()
            .and_then(|tracker| tracker.active_output());
        self.map(phase, revision, output, qh);
        Ok(())
    }

    fn map(
        &mut self,
        phase: Phase,
        revision: u64,
        output: Option<wl_output::WlOutput>,
        qh: &QueueHandle<Self>,
    ) {
        let wl = self.compositor.create_surface(qh, ());
        let region = self.compositor.create_region(qh, ());
        wl.set_input_region(Some(&region));
        region.destroy();
        let scaling = self.scaling.as_ref().map(|(manager, viewporter)| {
            let fractional = manager.get_fractional_scale(&wl, qh, ());
            let viewport = viewporter.get_viewport(&wl, qh, ());
            viewport.set_destination(WIDTH as i32, HEIGHT as i32);
            (fractional, viewport)
        });
        if scaling.is_none() {
            wl.set_buffer_scale((DEFAULT_SCALE / SCALE_UNIT) as i32);
        }
        let scale = if scaling.is_some() {
            SCALE_UNIT
        } else {
            DEFAULT_SCALE
        };

        // The active seat's output is the fallback if the window spans
        // monitors or the toplevel protocol is unavailable.
        let role = self.shell.get_layer_surface(
            &wl,
            output.as_ref(),
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
            output,
            scaling,
            scale,
        });
    }

    fn paint(shm: &wl_shm::WlShm, surface: &Surface, qh: &QueueHandle<Self>) -> Result<()> {
        let pixmap = indicator(surface.phase, surface.scale)?;
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
        surface.wl.frame(
            qh,
            Frame {
                surface: surface.wl.id(),
                revision: surface.revision,
            },
        );
        surface.wl.commit();
        Ok(())
    }
}

fn indicator(phase: Phase, scale: u32) -> Result<Pixmap> {
    let width = (WIDTH * scale).div_ceil(SCALE_UNIT);
    let height = (HEIGHT * scale).div_ceil(SCALE_UNIT);
    let transform =
        Transform::from_scale(width as f32 / WIDTH as f32, height as f32 / HEIGHT as f32);
    let mut pixmap = Pixmap::new(width, height).ok_or("could not allocate overlay buffer")?;
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
                if matches!(phase, Phase::Starting) {
                    paint.set_color_rgba8(255, 203, 125, alpha);
                } else {
                    paint.set_color_rgba8(205, 217, 245, alpha);
                }
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
                let (revision, phase, had_output) =
                    (surface.revision, surface.phase, surface.output.is_some());
                if let Some(surface) = overlay.surface.take() {
                    surface.destroy();
                }
                if had_output {
                    // A disconnected output closes its layer surfaces.
                    // Retry once via the active seat before using Electron.
                    overlay.map(phase, revision, None, qh);
                    Ok(())
                } else {
                    writeln!(io::stdout().lock(), "{revision} lost").map_err(Into::into)
                }
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            overlay.error = Some(error);
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for Overlay {
    fn event(
        overlay: &mut Self,
        proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else {
            return;
        };
        let Some(surface) = overlay.surface.as_mut().filter(|surface| {
            surface
                .scaling
                .as_ref()
                .is_some_and(|(fractional, _)| fractional == proxy)
        }) else {
            return;
        };
        let scale = scale.clamp(SCALE_UNIT / 2, 8 * SCALE_UNIT);
        if surface.scale != scale {
            surface.scale = scale;
            if surface.configured {
                if let Err(error) = Overlay::paint(&overlay.shm, surface, qh) {
                    overlay.error = Some(error);
                }
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, Frame> for Overlay {
    fn event(
        overlay: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        frame: &Frame,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if overlay.surface.as_ref().is_some_and(|surface| {
            surface.revision == frame.revision && surface.wl.id() == frame.surface
        }) {
            if let Err(error) = writeln!(io::stdout().lock(), "{} shown", frame.revision) {
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

delegate_noop!(Overlay: ignore WpFractionalScaleManagerV1);
delegate_noop!(Overlay: ignore WpViewporter);
delegate_noop!(Overlay: ignore WpViewport);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_scale_preserves_logical_size() {
        let image = indicator(Phase::Listening, 150).unwrap();
        assert_eq!((image.width(), image.height()), (85, 40));
    }
}
