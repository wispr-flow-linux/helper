//! Linux helper settings stored separately from the Electron profile.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::backend::Result;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipboardMode {
    Preserve,
    #[default]
    KeepTranscript,
}

impl ClipboardMode {
    fn parse(text: &str) -> Result<Self> {
        match text.trim() {
            "preserve" => Ok(Self::Preserve),
            "keep-transcript" => Ok(Self::KeepTranscript),
            _ => Err("clipboard mode must be preserve or keep-transcript".into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::KeepTranscript => "keep-transcript",
        }
    }

    pub fn read() -> Result<Self> {
        Self::read_from(&path()?)
    }

    fn read_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(mode) => Self::parse(&mode),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("could not read clipboard mode: {error}")),
        }
    }

    fn write(self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .ok_or("clipboard settings directory missing")?;
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        let temporary = dir.join(format!(".clipboard-mode.{}", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        let result = (|| {
            writeln!(file, "{}", self.name())?;
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|error| error.to_string())
    }
}

fn path() -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or("cannot resolve clipboard settings directory")?;
    Ok(config.join("wispr-flow/clipboard-mode"))
}

pub fn clipboard_command(args: &[String]) -> Result<()> {
    let mode = match args {
        [] => ClipboardMode::read()?,
        [option] => {
            let mode = ClipboardMode::parse(option)?;
            mode.write(&path()?)?;
            mode
        }
        _ => return Err("usage: wispr-flow --clipboard-mode [preserve|keep-transcript]".into()),
    };
    println!("clipboard-mode={}", mode.name());
    Ok(())
}
