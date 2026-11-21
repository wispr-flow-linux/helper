//! IPC for the temporary paste selection and the restored clipboard owner.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::{fs::FileTypeExt, process::CommandExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{wl_clipboard, Result};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_REQUEST_BYTES: u64 = 64 * 1024 * 1024;
const OWNER_ARG: &str = "--clipboard-owner";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Reply {
    Ready,
    Current,
    Done,
    Error(String),
}

pub(super) fn reply(output: &mut impl Write, reply: Reply) -> Result<()> {
    serde_json::to_writer(&mut *output, &reply).map_err(|error| error.to_string())?;
    writeln!(output)
        .and_then(|_| output.flush())
        .map_err(|error| error.to_string())
}

pub fn is_owner_arg(arg: &str) -> bool {
    arg == OWNER_ARG
}

pub fn run() -> Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let result = (|| {
        let mut line = Vec::new();
        (&mut input)
            .take(MAX_REQUEST_BYTES)
            .read_until(b'\n', &mut line)
            .map_err(|error| error.to_string())?;
        if line.last() != Some(&b'\n') {
            return Err("clipboard request is incomplete or too large".into());
        }
        let (plain, html): (String, Option<String>) =
            serde_json::from_slice(&line).map_err(|_| "invalid clipboard request")?;
        drop(line);
        let mut offers = vec![("text/plain;charset=utf-8".into(), plain.into_bytes())];
        if let Some(html) = html {
            offers.push(("text/html".into(), html.into_bytes()));
        }
        wl_clipboard::serve_paste(offers, &mut input, &mut output)
    })();
    if let Err(error) = &result {
        let _ = reply(&mut output, Reply::Error(error.clone()));
    }
    result
}

fn systemd_session() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .and_then(|dir| std::fs::metadata(dir.join("systemd/private")).ok())
        .is_some_and(|metadata| metadata.file_type().is_socket())
}

pub struct Paste {
    input: File,
    replies: Receiver<Reply>,
    done: bool,
}

impl Paste {
    pub fn start(text: &str, html: Option<&str>) -> Result<Self> {
        let mut packet = serde_json::to_vec(&(text, html)).map_err(|error| error.to_string())?;
        packet.push(b'\n');
        if packet.len() as u64 > MAX_REQUEST_BYTES {
            return Err("clipboard request too large".into());
        }
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        // A separate scope survives the Electron service's cgroup cleanup.
        // Under desktop launchers without systemd a separate group survives
        // normal application shutdown.
        let mut command = if systemd_session() {
            let mut command = Command::new("systemd-run");
            command.args(["--user", "--scope", "--collect", "--quiet", "--"]);
            command.arg(&executable);
            command
        } else {
            Command::new(&executable)
        };
        let mut child = command
            .arg(OWNER_ARG)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("clipboard owner could not start: {error}"))?;
        let input = child
            .stdin
            .take()
            .ok_or("clipboard owner stdin unavailable")?;
        let output = child
            .stdout
            .take()
            .ok_or("clipboard owner stdout unavailable")?;
        let (messages, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output)
                .lines()
                .map_while(std::result::Result::ok)
            {
                let Ok(message) = serde_json::from_str(&line) else {
                    break;
                };
                if messages.send(message).is_err() {
                    break;
                }
            }
            let _ = child.wait();
        });

        let mut paste = Self {
            input: File::from(OwnedFd::from(input)),
            replies,
            done: false,
        };
        let timeout = wl_clipboard::SETUP_TIMEOUT + CONTROL_TIMEOUT;
        let deadline = Instant::now() + timeout;
        wl_clipboard::write_with_timeout(&mut paste.input, &packet, deadline)?;
        paste.expect(
            Reply::Ready,
            deadline.saturating_duration_since(Instant::now()),
        )?;
        Ok(paste)
    }

    fn expect(&mut self, expected: Reply, timeout: Duration) -> Result<()> {
        match self.replies.recv_timeout(timeout) {
            Ok(Reply::Error(error)) => {
                self.done = true;
                Err(error)
            }
            Ok(Reply::Done) => {
                self.done = true;
                if expected == Reply::Done {
                    Ok(())
                } else {
                    Err("clipboard changed before paste".into())
                }
            }
            Ok(reply) if reply == expected => Ok(()),
            Ok(_) => Err("invalid clipboard owner response".into()),
            Err(error) => Err(format!("clipboard owner: {error}")),
        }
    }

    pub fn check_current(&mut self) -> Result<()> {
        self.input
            .write_all(b"C")
            .map_err(|_| "clipboard changed before paste")?;
        self.expect(Reply::Current, CONTROL_TIMEOUT)
    }

    pub fn finish(mut self, sent: bool) -> Result<()> {
        if self.done {
            return Ok(());
        }
        // Broken pipe can mean the owner already finished after a new copy.
        let _ = self.input.write_all(if sent { b"P" } else { b"A" });
        self.expect(Reply::Done, CONTROL_TIMEOUT)
    }
}
