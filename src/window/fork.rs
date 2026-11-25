//! Fork-only `AppWindow` helpers kept out of `src/window.rs` so upstream rebases
//! only touch one-line hooks.

use super::*;

impl AppWindow {
    /// Resolve keyboard focus for a modal overlay, targeting its first visible
    /// terminal block (matching `open_overlay`) instead of the outer frame.
    pub(super) fn overlay_focus(&self, overlay: &Overlay) -> FocusHandle {
        overlay
            .layout
            .as_ref()
            .and_then(|l| {
                l.visible_blocks()
                    .into_iter()
                    .find(|n| matches!(n.content(), Some(Content::Terminal { .. })))
            })
            .and_then(|n| self.terms.get(&n.id))
            .map(|t| t.focus.clone())
            .unwrap_or_else(|| overlay.focus.clone())
    }

    /// Dispatch `terminal.paste { terminal?, text? }`.
    /// When `text` is supplied, validates against unsafe control characters
    /// (matching `herdr-tools/crates/comment/src/format.rs:84-92`: only `\n` and `\t`
    /// allowed) and sends directly via bracketed paste without touching the clipboard.
    pub(super) fn dispatch_terminal_paste(&mut self, a: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let explicit_terminal = match a.get("terminal") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => return self.fail("terminal.paste: argument terminal must be a string"),
        };
        let text = match a.get("text") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => return self.fail("terminal.paste: argument text must be a string"),
        };
        if let Some(text) = text
            && text.chars().any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
        {
            return self.fail("terminal.paste: text contains unsupported control characters");
        }
        let view = match explicit_terminal {
            Some(address) => self.shown_terminal_view(address, window, cx),
            None => match self.focused_terminal(window, cx) {
                Some(t) => Ok(t.view.clone()),
                None if text.is_none() => return,
                None => Err("no focused terminal to paste into".into()),
            },
        };
        match (view, text) {
            (Ok(view), Some(text)) => view.read(cx).paste_text(text),
            (Ok(view), None) => view.update(cx, |v, cx| v.paste(cx)),
            (Err(error), _) => self.fail(error),
        }
    }
}
