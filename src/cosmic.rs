//! Read-only COSMIC toplevel tracking for the helper and its layer-shell client.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1 as cosmic_handle, zcosmic_toplevel_info_v1 as cosmic_info,
};
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1 as foreign_handle, ext_foreign_toplevel_list_v1 as foreign_list,
};

use crate::backend::{
    active_app_payload, ActiveApp, ActiveAppProvider, EventSink, Result, RunningApp,
};

pub fn is_cosmic() -> bool {
    ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .any(|value| {
            value
                .split(':')
                .any(|name| name.eq_ignore_ascii_case("COSMIC"))
        })
}

pub fn print_capabilities() -> Result<()> {
    let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
    let (globals, _) =
        registry_queue_init::<State>(&connection).map_err(|error| error.to_string())?;
    globals.contents().with_list(|globals| {
        let has = |name, version| {
            globals
                .iter()
                .any(|g| g.interface == name && g.version >= version)
        };
        let print =
            |option, supported| println!("{option}={}", if supported { "yes" } else { "no" });
        print(
            "cosmic-toplevel",
            has("zcosmic_toplevel_info_v1", 2) && has("ext_foreign_toplevel_list_v1", 1),
        );
        print("layer-shell", has("zwlr_layer_shell_v1", 1));
        print(
            "clipboard-preservation",
            has("ext_data_control_manager_v1", 1),
        );
    });
    Ok(())
}

#[derive(Default)]
struct Window {
    extension: Option<cosmic_handle::ZcosmicToplevelHandleV1>,
    app_id: String,
    title: String,
    activation: Option<u64>,
    initialized: bool,
    outputs: Vec<wl_output::WlOutput>,
}

type Focus = (u64, ActiveApp);

#[derive(Default)]
struct State {
    manager: Option<cosmic_info::ZcosmicToplevelInfoV1>,
    windows: HashMap<ObjectId, Window>,
    outputs: HashMap<u32, wl_output::WlOutput>,
    sequence: u64,
}

impl State {
    fn active(&self) -> Option<(&ObjectId, &Window)> {
        self.windows
            .iter()
            .filter_map(|(id, w)| w.activation.map(|seq| (seq, id, w)))
            .max_by_key(|(seq, _, _)| *seq)
            .map(|(_, id, window)| (id, window))
    }

    fn focus(&self) -> Option<Focus> {
        self.active().and_then(|(_, window)| {
            Some((
                window.activation?,
                ActiveApp {
                    app_name: window.app_id.clone(),
                    bundle_id: window.app_id.clone(),
                    window_title: window.title.clone(),
                    url: String::new(),
                },
            ))
        })
    }

    fn running(&self) -> Vec<RunningApp> {
        self.windows
            .values()
            .filter(|w| !w.app_id.is_empty())
            .map(|w| (w.app_id.clone(), w.app_id.clone()))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .map(|(bundle_id, name)| RunningApp { bundle_id, name })
            .collect()
    }
}

pub struct Toplevels {
    queue: EventQueue<State>,
    state: State,
}

impl Toplevels {
    pub fn connect(connection: &Connection) -> Result<Self> {
        let (globals, mut queue) =
            registry_queue_init::<State>(connection).map_err(|error| error.to_string())?;
        let qh = queue.handle();
        let mut state = State {
            manager: Some(
                globals
                    .bind(&qh, 2..=3, ())
                    .map_err(|error| error.to_string())?,
            ),
            ..State::default()
        };

        // Bind outputs before asking for window extensions, or the compositor
        // cannot send output references on this connection.
        let registry = globals.registry();
        globals.contents().with_list(|globals| {
            for global in globals.iter().filter(|g| g.interface == "wl_output") {
                state.outputs.insert(
                    global.name,
                    registry.bind(global.name, global.version.min(4), &qh, ()),
                );
            }
        });
        let _list: foreign_list::ExtForeignToplevelListV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|error| error.to_string())?;
        // The first sync enumerates windows; the second receives metadata for
        // the COSMIC extension objects created by the enumeration callbacks.
        for _ in 0..2 {
            queue
                .roundtrip(&mut state)
                .map_err(|error| error.to_string())?;
        }
        let mut toplevels = Self { queue, state };
        toplevels.wait_for_initial_state()?;
        Ok(toplevels)
    }

    fn wait_for_initial_state(&mut self) -> Result<()> {
        // COSMIC may defer State and OutputEnter until the next refresh.
        // A roundtrip only acknowledges get_cosmic_toplevel.
        let deadline = Instant::now() + Duration::from_millis(80);
        while self
            .state
            .windows
            .values()
            .any(|window| !window.initialized)
        {
            self.dispatch_pending()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || self.state.active().is_some() {
                break;
            }
            let Some(read) = self.queue.prepare_read() else {
                continue;
            };
            self.queue.flush().map_err(|error| error.to_string())?;
            let mut fds = [PollFd::from_borrowed_fd(
                read.connection_fd(),
                PollFlags::IN,
            )];
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: remaining.subsec_nanos().into(),
            };
            match poll(&mut fds, Some(&timeout)) {
                Ok(0) => break,
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.to_string()),
            }
            match read.read() {
                Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                result => {
                    result.map_err(|error| error.to_string())?;
                }
            }
        }
        Ok(())
    }

    pub fn dispatch_pending(&mut self) -> Result<()> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// A spanning window is ambiguous; let layer-shell choose the active seat's
    /// output in that case.
    pub fn active_output(&self) -> Option<wl_output::WlOutput> {
        let (_, window) = self.state.active()?;
        let mut outputs = window
            .outputs
            .iter()
            .filter(|output| self.state.outputs.values().any(|bound| bound == *output));
        let output = outputs.next()?;
        outputs.next().is_none().then(|| output.clone())
    }
}

struct Published {
    current: Option<Focus>,
    running: Vec<RunningApp>,
    emit: bool,
    sequence: u64,
    events: EventSink,
}

impl Published {
    fn announce(&mut self) {
        if let (true, Some((_, app))) = (self.emit, &self.current) {
            self.sequence += 1;
            let uuid = format!("cosmic-focus-{}", self.sequence);
            let event = crate::proto::request("AppInfoUpdate", active_app_payload(app), &uuid);
            let _ = self.events.send(event);
        }
    }

    fn refresh(&mut self, toplevels: &Toplevels) {
        let current = toplevels.state.focus();
        self.running = toplevels.state.running();
        if self.current != current {
            self.current = current;
            self.announce();
        }
    }
}

pub struct CosmicTracker(Arc<Mutex<Published>>);

impl CosmicTracker {
    pub fn start(events: EventSink) -> Result<Self> {
        let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
        let mut toplevels = Toplevels::connect(&connection)?;
        let published = Arc::new(Mutex::new(Published {
            current: toplevels.state.focus(),
            running: toplevels.state.running(),
            emit: true,
            sequence: 0,
            events,
        }));
        let shared = Arc::clone(&published);
        std::thread::Builder::new()
            .name("cosmic-focus-watch".into())
            .spawn(move || {
                while Arc::strong_count(&shared) > 1 {
                    if let Err(error) = toplevels.queue.blocking_dispatch(&mut toplevels.state) {
                        log::debug!("COSMIC window tracking ended: {error}");
                        if let Ok(mut view) = shared.lock() {
                            view.current = None;
                            view.running.clear();
                        }
                        break;
                    }
                    if let Ok(mut view) = shared.lock() {
                        view.refresh(&toplevels);
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self(published))
    }
}

impl ActiveAppProvider for CosmicTracker {
    fn paste_target(&self) -> Option<(u64, ActiveApp)> {
        self.0.lock().ok()?.current.clone()
    }

    fn current(&self) -> Option<ActiveApp> {
        self.0
            .lock()
            .ok()?
            .current
            .as_ref()
            .map(|(_, app)| app.clone())
    }

    fn get_running_apps(&self) -> Vec<RunningApp> {
        self.0
            .lock()
            .map(|view| view.running.clone())
            .unwrap_or_default()
    }

    fn set_focus_detection(&self, active: bool) {
        if let Ok(mut view) = self.0.lock() {
            view.emit = active;
            view.announce();
        }
    }

    fn name(&self) -> &'static str {
        "cosmic"
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => {
                state
                    .outputs
                    .entry(name)
                    .or_insert_with(|| registry.bind(name, version.min(4), qh, ()));
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.outputs.remove(&name) {
                    if output.version() >= 3 {
                        output.release();
                    }
                    for window in state.windows.values_mut() {
                        window.outputs.retain(|old| old != &output);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<foreign_list::ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &foreign_list::ExtForeignToplevelListV1,
        event: foreign_list::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            foreign_list::Event::Toplevel { toplevel } => {
                let id = toplevel.id();
                let extension = state
                    .manager
                    .as_ref()
                    .map(|manager| manager.get_cosmic_toplevel(&toplevel, qh, id.clone()));
                state.windows.insert(
                    id,
                    Window {
                        extension,
                        ..Window::default()
                    },
                );
            }
            foreign_list::Event::Finished => state.windows.clear(),
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, foreign_list::ExtForeignToplevelListV1, [
        foreign_list::EVT_TOPLEVEL_OPCODE => (foreign_handle::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<foreign_handle::ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &foreign_handle::ExtForeignToplevelHandleV1,
        event: foreign_handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        if let foreign_handle::Event::Closed = event {
            if let Some(extension) = state.windows.remove(&id).and_then(|w| w.extension) {
                extension.destroy();
            }
            handle.destroy();
        } else if let Some(window) = state.windows.get_mut(&id) {
            match event {
                foreign_handle::Event::AppId { app_id } => window.app_id = app_id,
                foreign_handle::Event::Title { title } => window.title = title,
                _ => {}
            }
        }
    }
}

impl Dispatch<cosmic_handle::ZcosmicToplevelHandleV1, ObjectId> for State {
    fn event(
        state: &mut Self,
        _: &cosmic_handle::ZcosmicToplevelHandleV1,
        event: cosmic_handle::Event,
        id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(window) = state.windows.get_mut(id) else {
            return;
        };
        match event {
            cosmic_handle::Event::State { state: states } => {
                window.initialized = true;
                let active = states
                    .as_chunks::<4>()
                    .0
                    .contains(&(cosmic_handle::State::Activated as u32).to_ne_bytes());
                if !active {
                    window.activation = None;
                } else if window.activation.is_none() {
                    state.sequence += 1;
                    window.activation = Some(state.sequence);
                }
            }
            cosmic_handle::Event::OutputEnter { output } => {
                if !window.outputs.contains(&output) {
                    window.outputs.push(output);
                }
            }
            cosmic_handle::Event::OutputLeave { output } => {
                window.outputs.retain(|old| old != &output);
            }
            _ => {}
        }
    }
}

impl Dispatch<cosmic_info::ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &cosmic_info::ZcosmicToplevelInfoV1,
        _: cosmic_info::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    wayland_client::event_created_child!(State, cosmic_info::ZcosmicToplevelInfoV1, [
        cosmic_info::EVT_TOPLEVEL_OPCODE => (cosmic_handle::ZcosmicToplevelHandleV1, ObjectId::null()),
    ]);
}

delegate_noop!(State: ignore wl_output::WlOutput);
