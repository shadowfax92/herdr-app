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

    /// Open the inline comment note card when copy mode presses `c`.
    pub(super) fn open_copy_comment(&mut self, addr: &str, quote: String, window: &mut Window, cx: &mut Context<Self>) {
        let (id, spec) = crate::terminal::copy_mode::comment_overlay_spec(
            addr,
            quote,
            window.viewport_size().width,
            crate::terminal::cell_size(window).width,
        );
        self.open_overlay(id, spec, window, cx);
    }

    /// Dispatch `terminal.copy_mode { terminal?, action?, key? }`.
    pub(super) fn dispatch_terminal_copy_mode(&mut self, a: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let view = match a.get("terminal") {
            None | Some(Value::Null) => self
                .focused_terminal(window, cx)
                .map(|t| t.view.clone())
                .ok_or_else(|| "no focused terminal for copy mode".into()),
            Some(Value::String(address)) => self.shown_terminal_view(address, window, cx),
            Some(_) => return self.fail("terminal.copy_mode: terminal must be a string"),
        };
        let action = match a.get("action") {
            None | Some(Value::Null) => "toggle",
            Some(Value::String(s)) => s.as_str(),
            Some(_) => return self.fail("terminal.copy_mode: action must be a string"),
        };
        let key = match a.get("key") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => return self.fail("terminal.copy_mode: key must be a string"),
        };
        match view.and_then(|v| {
            let addr = v.read(cx).address.clone();
            v.update(cx, |v, cx| v.copy_mode_command(action, key, window, cx)).map(|r| (addr, r))
        }) {
            Ok((addr, (status, comment))) => {
                if let Some(quote) = comment {
                    self.open_copy_comment(&addr, quote, window, cx);
                }
                self.answer(Ok(status));
            }
            Err(error) => self.fail(error),
        }
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
