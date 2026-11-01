//! Clipboard-copy fallback shared by X11 and Wayland.

use super::{Result, Selection};

pub(super) fn get_selection<B>(
    backend: &mut B,
    read: fn(&B) -> Result<String>,
    write: fn(&B, &str) -> Result<()>,
    copy: impl FnOnce(&mut B) -> Result<()>,
) -> Result<Selection> {
    let saved = read(backend)?;
    // Ctrl+C can leave the clipboard untouched when nothing is selected.
    let selected = write(backend, "")
        .and_then(|()| copy(backend))
        .and_then(|()| read(backend));
    // Restore even after a failed clear, copy, or read.
    write(backend, &saved)?;
    let selected = selected?;
    Ok(Selection {
        selected_text: selected.clone(),
        contents: selected,
        ..Selection::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Clipboard(RefCell<String>);

    impl Clipboard {
        fn read(&self) -> Result<String> {
            Ok(self.0.borrow().clone())
        }

        fn write(&self, text: &str) -> Result<()> {
            *self.0.borrow_mut() = text.to_string();
            Ok(())
        }
    }

    #[test]
    fn no_selection_does_not_return_clipboard_contents() {
        let mut clipboard = Clipboard(RefCell::new("private clipboard text".into()));
        let selected = get_selection(
            &mut clipboard,
            Clipboard::read,
            Clipboard::write,
            |_| Ok(()),
        )
        .unwrap();
        assert!(selected.selected_text.is_empty());
        assert!(selected.contents.is_empty());
        assert_eq!(clipboard.read().unwrap(), "private clipboard text");
    }

    #[test]
    fn selection_equal_to_saved_clipboard_is_still_returned() {
        let mut clipboard = Clipboard(RefCell::new("selected text".into()));
        let selected = get_selection(&mut clipboard, Clipboard::read, Clipboard::write, |c| {
            c.write("selected text")
        })
        .unwrap();
        assert_eq!(selected.selected_text, "selected text");
        assert_eq!(selected.contents, "selected text");
        assert_eq!(clipboard.read().unwrap(), "selected text");
    }

    #[test]
    fn copied_selection_replaces_old_clipboard_only_temporarily() {
        let mut clipboard = Clipboard(RefCell::new("old clipboard".into()));
        let selected = get_selection(&mut clipboard, Clipboard::read, Clipboard::write, |c| {
            c.write("new selection")
        })
        .unwrap();
        assert_eq!(selected.selected_text, "new selection");
        assert_eq!(clipboard.read().unwrap(), "old clipboard");
    }

    #[test]
    fn copy_failure_restores_clipboard() {
        let mut clipboard = Clipboard(RefCell::new("old clipboard".into()));
        let result = get_selection(&mut clipboard, Clipboard::read, Clipboard::write, |c| {
            c.write("partial copy")?;
            Err("copy failed".into())
        });
        assert_eq!(result.unwrap_err(), "copy failed");
        assert_eq!(clipboard.read().unwrap(), "old clipboard");
    }

    #[test]
    fn save_failure_does_not_modify_clipboard() {
        let mut clipboard = Clipboard(RefCell::new("old clipboard".into()));
        let result = get_selection(
            &mut clipboard,
            |_| Err("read failed".into()),
            |_, _| panic!("must not overwrite unreadable clipboard"),
            |_| panic!("must not copy without saving clipboard"),
        );
        assert_eq!(result.unwrap_err(), "read failed");
    }

    #[test]
    fn clear_failure_does_not_copy() {
        let mut clipboard = Clipboard(RefCell::new("old clipboard".into()));
        let result = get_selection(
            &mut clipboard,
            Clipboard::read,
            |_, _| Err("write failed".into()),
            |_| panic!("must not copy without clearing clipboard"),
        );
        assert_eq!(result.unwrap_err(), "write failed");
        assert_eq!(clipboard.read().unwrap(), "old clipboard");
    }

    #[test]
    fn read_failure_after_copy_restores_clipboard() {
        let mut clipboard = Clipboard(RefCell::new("old clipboard".into()));
        let result = get_selection(
            &mut clipboard,
            |c| match c.read()?.as_str() {
                "unreadable" => Err("read failed".into()),
                text => Ok(text.to_string()),
            },
            Clipboard::write,
            |c| c.write("unreadable"),
        );
        assert_eq!(result.unwrap_err(), "read failed");
        assert_eq!(clipboard.read().unwrap(), "old clipboard");
    }

    #[test]
    fn restore_failure_is_reported() {
        let mut clipboard = Clipboard(RefCell::new("old clipboard".into()));
        let result = get_selection(
            &mut clipboard,
            Clipboard::read,
            |c, text| {
                if text == "old clipboard" {
                    Err("restore failed".into())
                } else {
                    c.write(text)
                }
            },
            |_| Ok(()),
        );
        assert_eq!(result.unwrap_err(), "restore failed");
    }
}
