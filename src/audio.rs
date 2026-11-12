//! Dictation playback muting. A separate guard owns the snapshot so helper
//! crashes (including SIGKILL) close its pipe and restore playback.
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

type Result<T> = std::result::Result<T, String>;
#[derive(Clone, Deserialize)]
struct Sink {
    name: String,
    mute: bool,
}
trait Mixer {
    fn sinks(&mut self) -> Result<Vec<Sink>>;
    fn mute(&mut self, name: &str, muted: bool) -> Result<()>;
}
struct Pulse;
fn pactl(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("/usr/bin/timeout")
        .args(["3s", "/usr/bin/pactl"])
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "pactl failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(out.stdout)
}
impl Mixer for Pulse {
    fn sinks(&mut self) -> Result<Vec<Sink>> {
        serde_json::from_slice(&pactl(&["--format=json", "list", "sinks"])?)
            .map_err(|e| e.to_string())
    }
    fn mute(&mut self, name: &str, muted: bool) -> Result<()> {
        pactl(&["set-sink-mute", name, if muted { "1" } else { "0" }]).map(|_| ())
    }
}
#[derive(Default)]
struct Session {
    active: bool,
    saved: BTreeMap<String, bool>,
}
impl Session {
    fn start(&mut self, mixer: &mut impl Mixer) -> Result<()> {
        self.active = true;
        self.refresh(mixer)
    }
    fn refresh(&mut self, mixer: &mut impl Mixer) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let mut errors = Vec::new();
        for sink in mixer.sinks()? {
            // Never overwrite the original snapshot on duplicate starts or polls.
            self.saved.entry(sink.name.clone()).or_insert(sink.mute);
            if !sink.mute {
                if let Err(e) = mixer.mute(&sink.name, true) {
                    errors.push(e);
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
    fn stop(&mut self, mixer: &mut impl Mixer) -> Result<()> {
        self.active = false;
        if self.saved.is_empty() {
            return Ok(());
        }
        let present = mixer.sinks()?;
        let mut errors = Vec::new();
        for sink in present {
            if let Some(&muted) = self.saved.get(&sink.name) {
                if sink.mute == muted {
                    self.saved.remove(&sink.name);
                } else {
                    match mixer.mute(&sink.name, muted) {
                        Ok(()) => {
                            self.saved.remove(&sink.name);
                        }
                        Err(e) => errors.push(e),
                    }
                }
            }
        }
        // Disconnected devices stay pending, restoring on reconnect while alive.
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

pub struct Guard {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}
impl Guard {
    pub fn spawn() -> Result<Self> {
        let mut cmd = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
        cmd.arg("--audio-guard")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // Do not retain Electron's IPC pipe in the guardian.
        unsafe {
            cmd.pre_exec(|| {
                libc::close(3);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().ok_or("missing guard stdout")?);
        Ok(Self {
            child,
            input,
            output,
        })
    }
    pub fn command(&mut self, start: bool) -> Result<()> {
        self.input
            .as_mut()
            .ok_or("guard closed")?
            .write_all(if start { b"S" } else { b"E" })
            .map_err(|e| e.to_string())?;
        let mut reply = String::new();
        self.output
            .read_line(&mut reply)
            .map_err(|e| e.to_string())?;
        if reply.trim() == "OK" {
            Ok(())
        } else {
            Err(format!("audio guard: {}", reply.trim()))
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.wait();
    }
}
static EXIT: AtomicBool = AtomicBool::new(false);
extern "C" fn terminate(_: libc::c_int) {
    EXIT.store(true, Ordering::Relaxed);
}

pub fn run_guard() {
    unsafe {
        libc::signal(libc::SIGTERM, terminate as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, terminate as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, terminate as *const () as libc::sighandler_t);
    }
    let mut session = Session::default();
    let mut mixer = Pulse;
    let mut stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    while !EXIT.load(Ordering::Relaxed) {
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fd, 1, 150) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if ready > 0 {
            let mut byte = [0];
            if stdin.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            let result = match byte[0] {
                b'S' => session.start(&mut mixer),
                b'E' => session.stop(&mut mixer),
                _ => Err("invalid audio command".into()),
            };
            let reply = match result {
                Ok(()) => "OK".to_string(),
                Err(e) => {
                    eprintln!("[audio-mute] {e}");
                    format!("ERR {}", e.replace('\n', " "))
                }
            };
            if writeln!(stdout, "{reply}")
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break;
            }
        } else {
            let result = if session.active {
                session.refresh(&mut mixer)
            } else {
                session.stop(&mut mixer)
            };
            if let Err(e) = result {
                eprintln!("[audio-mute] {e}");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
    // Retry transient server failures during shutdown; never change volumes.
    for _ in 0..3 {
        match session.stop(&mut mixer) {
            Ok(()) => break,
            Err(e) => eprintln!("[audio-mute] restore: {e}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        sinks: BTreeMap<String, bool>,
        fail: bool,
    }
    impl Mixer for Fake {
        fn sinks(&mut self) -> Result<Vec<Sink>> {
            Ok(self
                .sinks
                .iter()
                .map(|(name, mute)| Sink {
                    name: name.clone(),
                    mute: *mute,
                })
                .collect())
        }
        fn mute(&mut self, name: &str, mute: bool) -> Result<()> {
            if self.fail {
                return Err("failure".into());
            }
            self.sinks.insert(name.into(), mute);
            Ok(())
        }
    }
    #[test]
    fn preserves_mixed_states_duplicate_start_and_stop() {
        let mut m = Fake {
            sinks: BTreeMap::from([("speakers".into(), false), ("hdmi".into(), true)]),
            fail: false,
        };
        let before = m.sinks.clone();
        let mut s = Session::default();
        s.start(&mut m).unwrap();
        s.start(&mut m).unwrap();
        assert!(m.sinks.values().all(|v| *v));
        s.stop(&mut m).unwrap();
        s.stop(&mut m).unwrap();
        assert_eq!(m.sinks, before);
    }
    #[test]
    fn mutes_hotplug_and_restores_reconnected_sink() {
        let mut m = Fake::default();
        let mut s = Session::default();
        s.start(&mut m).unwrap();
        m.sinks.insert("usb".into(), false);
        s.refresh(&mut m).unwrap();
        assert!(m.sinks["usb"]);
        m.sinks.clear();
        s.stop(&mut m).unwrap();
        assert!(!s.saved.is_empty());
        m.sinks.insert("usb".into(), true);
        s.stop(&mut m).unwrap();
        assert!(!m.sinks["usb"]);
    }
    #[test]
    fn failed_restore_is_retried_without_losing_snapshot() {
        let mut m = Fake {
            sinks: BTreeMap::from([("a".into(), false)]),
            fail: false,
        };
        let mut s = Session::default();
        s.start(&mut m).unwrap();
        m.fail = true;
        assert!(s.stop(&mut m).is_err());
        assert!(!s.saved["a"]);
        m.fail = false;
        s.stop(&mut m).unwrap();
        assert!(!m.sinks["a"]);
    }
    #[test]
    fn failed_mute_does_not_overwrite_snapshot() {
        let mut m = Fake {
            sinks: BTreeMap::from([("a".into(), false)]),
            fail: true,
        };
        let mut s = Session::default();
        assert!(s.start(&mut m).is_err());
        m.fail = false;
        s.refresh(&mut m).unwrap();
        s.stop(&mut m).unwrap();
        assert!(!m.sinks["a"]);
    }
}
