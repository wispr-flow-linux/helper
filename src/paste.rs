const PASTE_KEYS_ENV: &str = "WISPR_LINUX_HELPER_PASTE_KEYS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteChord {
    pub key_vk: u32,
    pub flags: &'static [&'static str],
}

const CTRL_V: PasteChord = PasteChord {
    key_vk: b'V' as u32,
    flags: &["Control"],
};

const CTRL_SHIFT_V: PasteChord = PasteChord {
    key_vk: b'V' as u32,
    flags: &["Control", "Shift"],
};

pub fn chord() -> Result<PasteChord, String> {
    parse(&std::env::var(PASTE_KEYS_ENV).unwrap_or_default())
}

fn parse(value: &str) -> Result<PasteChord, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "ctrl+v" => Ok(CTRL_V),
        "ctrl+shift+v" => Ok(CTRL_SHIFT_V),
        _ => Err(format!("{PASTE_KEYS_ENV} supports ctrl+v and ctrl+shift+v")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_ctrl_v() {
        assert_eq!(parse(""), Ok(CTRL_V));
    }

    #[test]
    fn accepts_ctrl_shift_v() {
        assert_eq!(parse("ctrl+shift+v"), Ok(CTRL_SHIFT_V));
    }

    #[test]
    fn rejects_other_chords() {
        assert!(parse("ctrl+x").is_err());
    }
}
