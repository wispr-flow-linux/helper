//! No-op backend: lets the helper complete the IsReady/ACK handshake and stay
//! alive on sessions where no real backend is available, instead of looking
//! like a crashed helper (which would make Electron relaunch-loop). All OS
//! operations report failure / empty results.

use super::{ActiveApp, Backend, Result, RunningApp, Selection};
use crate::paste::PasteChord;

pub struct StubBackend;

impl Backend for StubBackend {
    fn paste_text(&mut self, _text: &str, _html: Option<&str>, _chord: &PasteChord) -> Result<()> {
        Err("no backend available (stub)".into())
    }
    fn simulate_key_press(&mut self, _keycode_vk: u32, _flags: &[String]) -> Result<()> {
        Err("no backend available (stub)".into())
    }
    fn get_active_app(&mut self) -> Result<ActiveApp> {
        Ok(ActiveApp::default())
    }
    fn get_running_apps(&mut self) -> Result<Vec<RunningApp>> {
        Ok(Vec::new())
    }
    fn get_selected_text(&mut self) -> Result<Selection> {
        Ok(Selection::default())
    }
    fn accessibility_status(&mut self) -> bool {
        false
    }
    fn name(&self) -> &'static str {
        "stub"
    }
}
