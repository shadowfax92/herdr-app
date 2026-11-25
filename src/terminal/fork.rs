//! Fork-only `TerminalView` helpers kept out of `src/terminal.rs` so upstream
//! rebases only touch one-line hooks.

use super::*;

impl TerminalView {
    /// Paste explicit text directly using bracketed paste without touching the OS clipboard.
    pub fn paste_text(&self, text: &str) {
        let bytes = self.grid.borrow().vt.paste_bytes(text);
        self.send(&bytes);
    }

    /// Return the current Ghostty selection formatted as plain text.
    pub fn selection_text(&self) -> Option<String> {
        self.grid.borrow().vt.selection_text()
    }

    /// Cached focus state from the latest render, used to rank overlapping views
    /// for the same terminal address (e.g. a peek overlay over its tab pane).
    pub(crate) fn was_focused(&self) -> bool {
        self.was_focused
    }

    #[cfg(test)]
    pub(crate) fn write_vt(&self, bytes: &[u8]) {
        self.grid.borrow_mut().vt.write(bytes);
    }

    #[cfg(test)]
    pub(crate) fn take_sent(&self) -> Vec<u8> {
        std::mem::take(&mut *self.last_sent.borrow_mut())
    }
}
