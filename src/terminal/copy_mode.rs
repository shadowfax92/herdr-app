//! Native vi copy mode for `TerminalView` + fork-specific inline comment glue.
//!
//! Why this lives in the local fork instead of a JS mod:
//! - Motions and visual selection mutate Ghostty's `Vt` synchronously (<0.1 ms)
//!   with zero IPC round-trips or selection teardown/replay flicker.
//! - `TerminalView::paint_grid` renders an independent block cursor while a
//!   visual selection is active, and Ghostty highlights `/`/`?` search matches
//!   directly in the GPU grid.
//! - Copy mode state lives on `TerminalView`, so opening and closing the modal
//!   `shadowfax-comment` note card returns focus straight back to copy mode at
//!   the exact cursor and scroll offset with 0 ms latency.
//!
//! Mirrors the owner's Herdr TUI fork (`/Users/shadowfax/code/side-projects/herdr-custom-plugins/my-herdr-fork`):
//! - Copy mode state & keys: `src/client/shell/copy_mode.rs`, `src/copy_mode.rs`
//! - Word/paragraph/search motions: `src/pane/terminal.rs`
//! - Bottom 1-row `COPY` mode bar: `src/client/shell/render.rs`
//! - Popup close preserving copy mode: `src/client/shell/state.rs`
//! - Responsive comment popup sizing: `~/.config/herdr/plugins/config/shadowfax.tools/config.toml`

use super::{CellMetrics, TerminalEvent, TerminalView, cell_font, paint_cell_text, scroll_delta_steps};
use crate::vt::{CellPosition, Frame, Vt};
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, Keystroke, MouseButton, ParentElement, Pixels, Point, ScrollWheelEvent,
    SharedString, Size, Styled, TextRun, Window, div, fill, point, px, size,
};
use serde_json::{Value, json};

/// Punctuation separators matching `my-herdr-fork/src/pane/terminal.rs:97`.
const WORD_SEPARATORS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~";

/// Maximum rows scanned per word/paragraph motion to stay instant on huge scrollback.
const MOTION_ROW_LIMIT: usize = 1000;

/// Maximum incremental Ghostty search ticks per search commit.
const SEARCH_TICK_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionKind {
    Char,
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectionAnchor {
    anchor: CellPosition,
    kind: SelectionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchDir {
    Forward,
    Backward,
}

#[derive(Clone, Debug, Default)]
struct SearchState {
    prompt: Option<(SearchDir, String)>,
    last_query: String,
    last_dir: Option<SearchDir>,
    active: bool,
    matches: Vec<(CellPosition, CellPosition)>,
    current: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextClass {
    Whitespace,
    Separator,
    Word,
}

#[derive(Clone, Copy, Debug)]
enum WordMotion {
    NextStart,
    PrevStart,
    NextEnd,
    NextBigStart,
    PrevBigStart,
    NextBigEnd,
}

/// Effect returned to `TerminalView` after handling one copy-mode key.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CopyOutcome {
    Stay { send_bytes: Vec<u8> },
    Comment(String),
    Exit { copy: Option<String>, send_bytes: Vec<u8> },
}

#[derive(Clone, Debug)]
pub(crate) struct CopyMode {
    cursor: CellPosition,
    preferred_col: u16,
    selection: Option<SelectionAnchor>,
    entry_offset_from_bottom: u64,
    app_pages_up: usize,
    search: SearchState,
    status: Option<String>,
    last_dims: (u16, u16),
    last_alt_screen: bool,
}

/// Owner-specific fork glue: build the `shadowfax-comment` inline note card
/// overlay spec with the TUI's responsive viewport-width sizing profiles
/// (`<=310` cols: 95%x90%, `311..399` cols: 90%x90%, `>=400` cols: 50%x70%).
pub(crate) fn comment_overlay_spec(
    terminal: &str,
    quote: String,
    viewport_width: Pixels,
    cell_width: Pixels,
) -> (String, Value) {
    let cw = f32::from(cell_width).max(1.0);
    let cols = (f32::from(viewport_width) / cw).floor() as u16;
    let (width, height) = comment_capture_size(cols);
    (
        "shadowfax-comment-note".into(),
        json!({
            "mod": "shadowfax-comment",
            "kind": "dialog",
            "modal": true,
            "frame": "none",
            "width": width,
            "height": height,
            "props": {
                "action": "note",
                "terminal": terminal,
                "quote": quote,
            }
        }),
    )
}

pub(crate) fn comment_capture_size(cols: u16) -> (f64, f64) {
    if cols <= 310 {
        (0.95, 0.90)
    } else if cols <= 399 {
        (0.90, 0.90)
    } else {
        (0.50, 0.70)
    }
}

/// Project a copy-mode screen cursor into viewport `(col, row)`, clamping to the
/// current grid bounds so even a resize frame before the next reconcile stays in bounds.
pub(super) fn project_cursor(vt: &Vt, copy_cursor: Option<CellPosition>) -> Option<(usize, usize)> {
    let pos = copy_cursor?;
    let sb = vt.scrollbar();
    let total = sb.total.max(1);
    let row = u64::from(pos.row).min(total.saturating_sub(1));
    let col = pos.col.min(vt.cols.saturating_sub(1)) as usize;
    (row >= sb.offset && row < sb.offset.saturating_add(sb.len))
        .then_some((col, (row - sb.offset) as usize))
}

/// Paint an independent high-contrast block cursor in copy mode (visible even during selection).
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_cursor(
    frame: &Frame,
    x: usize,
    y: usize,
    origin: Point<Pixels>,
    cell: Size<Pixels>,
    metrics: CellMetrics,
    window: &mut Window,
    cx: &mut App,
) {
    let wide = frame.rows.get(y).and_then(|r| r.get(x)).is_some_and(|c| c.wide);
    let w = cell.width * if wide { 2.0 } else { 1.0 };
    let color = crate::settings::color("accent");
    let p = origin + point(cell.width * x as f32, cell.height * y as f32);
    let scale = window.scale_factor();
    let snap = |v: Pixels| px((f32::from(v) * scale).round() / scale);
    let x0 = snap(p.x);
    let x1 = snap(p.x + w);
    let y0 = snap(p.y);
    let y1 = snap(p.y + cell.height);
    window.paint_quad(fill(Bounds::new(point(x0, y0), size(x1 - x0, y1 - y0)), color));
    if let Some(c) = frame.rows.get(y).and_then(|r| r.get(x)).filter(|c| !c.text.is_empty()) {
        let run = TextRun {
            len: c.text.len(),
            font: cell_font(c.bold, c.italic),
            color: crate::settings::color("accent_foreground"),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(c.text.clone().into(), px(crate::settings::font_size()), &[run], None);
        paint_cell_text(&line, p, metrics, window, cx);
    }
}

impl TerminalView {
    /// Enter native vi copy mode on this terminal, closing any open find bar and
    /// clearing any in-progress IME composition so copy mode owns all keys cleanly.
    pub fn enter_copy_mode(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        if self.search_open {
            if let Some(window) = window {
                self.close_search(window, cx);
            } else {
                self.search_open = false;
                self.search_keys = false;
                self.pending_search_nav = None;
                self.search_task.take();
                self.grid.borrow_mut().vt.clear_search();
            }
        }
        if self.copy_mode.is_none() {
            let mut g = self.grid.borrow_mut();
            g.marked = None;
            self.copy_mode = Some(CopyMode::enter(&mut g.vt));
            drop(g);
            self.selection_changed(cx);
            self.publish_scroll_if_changed(cx);
            cx.notify();
        }
    }

    /// Exit native vi copy mode, restoring scroll / alt-screen pages.
    pub fn exit_copy_mode(&mut self, cx: &mut Context<Self>) {
        if let Some(mut cm) = self.copy_mode.take() {
            let bytes = cm.finish_exit(&mut self.grid.borrow_mut().vt);
            if !bytes.is_empty() {
                self.send(&bytes);
            }
            self.selection_changed(cx);
            self.publish_scroll_if_changed(cx);
            cx.notify();
        }
    }

    /// Apply a `CopyOutcome` to this terminal; returns an inline comment quote when
    /// `emit_comment` is false so `AppWindow::dispatch` can open the note card synchronously.
    fn apply_copy_outcome(
        &mut self,
        outcome: CopyOutcome,
        emit_comment: bool,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let mut comment = None;
        match outcome {
            CopyOutcome::Stay { send_bytes } => {
                if !send_bytes.is_empty() {
                    self.send(&send_bytes);
                }
            }
            CopyOutcome::Comment(quote) => {
                if emit_comment {
                    cx.emit(TerminalEvent::CopyComment { quote });
                } else {
                    comment = Some(quote);
                }
            }
            CopyOutcome::Exit { copy, send_bytes } => {
                self.copy_mode = None;
                if let Some(text) = copy {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                    #[cfg(target_os = "linux")]
                    cx.write_to_primary(ClipboardItem::new_string(text));
                }
                if !send_bytes.is_empty() {
                    self.send(&send_bytes);
                }
            }
        }
        self.selection_changed(cx);
        self.publish_scroll_if_changed(cx);
        cx.notify();
        comment
    }

    /// Drive native vi copy mode from `terminal.copy_mode`.
    pub fn copy_mode_command(
        &mut self,
        action: &str,
        key: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(Value, Option<String>), String> {
        let mut comment = None;
        match action {
            "toggle" => {
                if self.copy_mode.is_some() {
                    self.exit_copy_mode(cx);
                } else {
                    self.enter_copy_mode(Some(window), cx);
                }
            }
            "enter" => self.enter_copy_mode(Some(window), cx),
            "exit" => self.exit_copy_mode(cx),
            "status" => {
                if let Some(cm) = self.copy_mode.as_mut()
                    && cm.reconcile(&mut self.grid.borrow_mut().vt)
                {
                    self.selection_changed(cx);
                }
            }
            "key" => {
                let key = key.ok_or("terminal.copy_mode action \"key\" needs a key")?;
                let Some(mut cm) = self.copy_mode.take() else {
                    return Err("terminal.copy_mode: copy mode is not active".into());
                };
                let outcome = cm.handle_key_str(key, &mut self.grid.borrow_mut().vt);
                self.copy_mode = Some(cm);
                let outcome = outcome?;
                comment = self.apply_copy_outcome(outcome, false, cx);
            }
            other => {
                return Err(format!(
                    "terminal.copy_mode: unknown action {other:?} (use toggle, enter, exit, key or status)"
                ));
            }
        }
        let status = match &self.copy_mode {
            Some(cm) => cm.status_json(&self.grid.borrow().vt),
            None => json!({ "active": false }),
        };
        Ok((status, comment))
    }

    /// Route a GPUI `KeyDownEvent` into copy mode when active; returns true if consumed.
    pub(super) fn handle_copy_mode_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(mut cm) = self.copy_mode.take() else {
            return false;
        };
        let outcome = cm.handle_keystroke(&ev.keystroke, &mut self.grid.borrow_mut().vt);
        self.copy_mode = Some(cm);
        self.apply_copy_outcome(outcome, true, cx);
        cx.stop_propagation();
        true
    }

    /// Handle wheel scrolling while copy mode is active without leaking mouse or
    /// arrow escape sequences to the PTY.
    pub(super) fn handle_copy_mode_scroll(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) -> bool {
        if self.copy_mode.is_none() {
            return false;
        }
        let mut g = self.grid.borrow_mut();
        let steps = scroll_delta_steps(
            &mut self.scrolled,
            ev.delta,
            ev.touch_phase,
            g.cell,
            g.vt.cols,
            g.vt.rows,
            crate::settings::get().terminal_scroll_speed,
        );
        let n = steps.y.unsigned_abs() as usize;
        if n > 0 && !g.vt.alt_screen() {
            let up = steps.y > 0;
            g.vt.scroll_viewport(if up { -(n as isize) } else { n as isize });
            drop(g);
            self.publish_scroll_if_changed(cx);
            cx.notify();
        }
        true
    }

    /// Reconcile copy mode and build render state `(copy_cursor, copy_bar, in_copy_mode)`.
    pub(super) fn prepare_copy_mode_render(
        &mut self,
        address: &str,
        cx: &mut Context<Self>,
    ) -> (Option<CellPosition>, Option<AnyElement>, bool) {
        let cleared = self
            .copy_mode
            .as_mut()
            .is_some_and(|cm| cm.reconcile(&mut self.grid.borrow_mut().vt));
        if cleared {
            self.selection_changed(cx);
        }
        let copy_cursor = self.copy_mode.as_ref().map(|cm| cm.cursor());
        let copy_bar = self
            .copy_mode
            .as_ref()
            .map(|cm| cm.render_bar(address, &self.grid.borrow().vt));
        (copy_cursor, copy_bar, self.copy_mode.is_some())
    }

    /// Drop copy mode's selection anchor if an external command (`terminal.clear_selection`,
    /// `terminal.select`, `terminal.select_all`) mutated Ghostty's selection directly.
    pub(super) fn sync_copy_mode_external_selection(&mut self) {
        if let Some(cm) = self.copy_mode.as_mut() {
            cm.selection = None;
        }
    }
}

impl CopyMode {
    /// Enter copy mode, mirroring `my-herdr-fork/src/client/shell/copy_mode.rs:69-89`:
    /// start at the live terminal cursor when at the bottom, or bottom-left of the
    /// visible viewport when already scrolled up into history.
    pub(crate) fn enter(vt: &mut Vt) -> Self {
        vt.clear_selection();
        let sb = vt.scrollbar();
        let total = sb.total.max(1);
        let at_bottom = sb.offset.saturating_add(sb.len) >= sb.total;
        let entry_offset_from_bottom = sb.total.saturating_sub(sb.offset.saturating_add(sb.len));
        let cursor = if at_bottom {
            let mut pos = vt.cursor_screen_position();
            pos.col = pos.col.min(vt.cols.saturating_sub(1));
            pos.row = pos.row.min(total.saturating_sub(1) as u32);
            pos
        } else {
            CellPosition {
                row: sb.offset.saturating_add(sb.len).saturating_sub(1).min(total.saturating_sub(1)) as u32,
                col: 0,
            }
        };
        let alt = vt.alt_screen();
        Self {
            cursor,
            preferred_col: cursor.col,
            selection: None,
            entry_offset_from_bottom,
            app_pages_up: 0,
            search: SearchState::default(),
            status: None,
            last_dims: (vt.cols, vt.rows),
            last_alt_screen: alt,
        }
    }

    pub(crate) fn cursor(&self) -> CellPosition {
        self.cursor
    }

    /// Reconcile copy-mode coordinates if the terminal resized, switched screens,
    /// or shrank scrollback (e.g. `clear` / `\x1b[3J`). Returns true if an active
    /// selection was cleared so the caller can publish `SelectionChanged`.
    pub(crate) fn reconcile(&mut self, vt: &mut Vt) -> bool {
        let dims = (vt.cols, vt.rows);
        let alt = vt.alt_screen();
        let dims_changed = dims != self.last_dims;
        let alt_changed = alt != self.last_alt_screen;
        let mut selection_cleared = false;

        if alt_changed {
            self.last_alt_screen = alt;
            // Invariant: leaving or entering alt-screen invalidates any prior
            // alt-screen paging depth so we never send stale PageDowns into a
            // newly started fullscreen app or back into a normal shell.
            self.app_pages_up = 0;
            if self.selection.take().is_some() {
                vt.clear_selection();
                selection_cleared = true;
            }
            if self.search.active {
                vt.clear_search();
            }
            self.search = SearchState::default();
        }
        if dims_changed {
            self.last_dims = dims;
            if self.selection.take().is_some() {
                vt.clear_selection();
                selection_cleared = true;
            }
            if self.search.active {
                self.search.matches = find_all_matches(vt, &self.search.last_query);
                self.search.current = None;
            }
        }

        let total = vt.scrollbar().total.max(1) as u32;
        let max_row = total.saturating_sub(1);
        let max_col = vt.cols.saturating_sub(1);
        self.cursor.row = self.cursor.row.min(max_row);
        self.cursor.col = self.cursor.col.min(max_col);
        self.preferred_col = self.preferred_col.min(max_col);
        if self.selection.is_some_and(|s| s.anchor.row > max_row) {
            self.selection = None;
            vt.clear_selection();
            selection_cleared = true;
        }
        selection_cleared
    }

    pub(crate) fn status_json(&self, vt: &Vt) -> Value {
        let mode = match self.selection.map(|s| s.kind) {
            None => "copy",
            Some(SelectionKind::Char) => "visual",
            Some(SelectionKind::Line) => "v-line",
        };
        json!({
            "active": true,
            "mode": mode,
            "cursor": { "row": self.cursor.row, "col": self.cursor.col },
            "selection": self.selection.map(|s| json!({
                "kind": match s.kind { SelectionKind::Char => "char", SelectionKind::Line => "line" },
                "anchor": { "row": s.anchor.row, "col": s.anchor.col },
                "text": vt.selection_text(),
            })),
            "search": {
                "prompt": self.search.prompt.as_ref().map(|(d, q)| json!({
                    "dir": match d { SearchDir::Forward => "forward", SearchDir::Backward => "backward" },
                    "query": q,
                })),
                "active": self.search.active,
                "query": self.search.last_query,
                "total": self.search.matches.len(),
                "current": self.search.current,
            },
            "app_pages_up": self.app_pages_up,
            "status": self.status,
        })
    }

    /// Exit copy mode without copying, restoring pre-entry scroll position or alt-screen pages.
    pub(crate) fn finish_exit(&mut self, vt: &mut Vt) -> Vec<u8> {
        self.reconcile(vt);
        self.selection = None;
        vt.clear_selection();
        if self.search.active {
            vt.clear_search();
        }
        self.search = SearchState::default();
        // Invariant (alt-screen app_pages_up restore): `reconcile` resets `app_pages_up`
        // on any screen transition, so `vt.alt_screen() && app_pages_up > 0` holds iff
        // we paged THIS active alt-screen session (matching my-herdr-fork copy_mode.rs:920-927).
        if vt.alt_screen() && self.app_pages_up > 0 {
            let pages = std::mem::take(&mut self.app_pages_up);
            return b"\x1b[6~".repeat(pages);
        }
        if !vt.alt_screen() {
            if self.entry_offset_from_bottom == 0 {
                vt.scroll_to_bottom();
            } else {
                let sb = vt.scrollbar();
                let target = sb.total.saturating_sub(sb.len).saturating_sub(self.entry_offset_from_bottom);
                vt.scroll_to_row(target as usize);
            }
        }
        Vec::new()
    }

    /// Handle a live GPUI `Keystroke`.
    pub(crate) fn handle_keystroke(&mut self, ks: &Keystroke, vt: &mut Vt) -> CopyOutcome {
        self.reconcile(vt);
        // Ignore bare modifier presses.
        if matches!(ks.key.as_str(), "shift" | "control" | "alt" | "platform" | "function") {
            return CopyOutcome::Stay { send_bytes: Vec::new() };
        }
        if self.search.prompt.is_some() {
            return self.handle_search_prompt_keystroke(ks, vt);
        }
        // Cmd+C in copy mode behaves like `y` (copy selection and exit).
        if ks.modifiers.platform && !ks.modifiers.control && !ks.modifiers.alt && ks.key.eq_ignore_ascii_case("c") {
            return self.copy_and_exit(vt);
        }
        if ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform {
            return match ks.key.as_str() {
                "u" => self.move_page(-1, true, vt),
                "d" => self.move_page(1, true, vt),
                "b" => self.move_page(-1, false, vt),
                "f" => self.move_page(1, false, vt),
                _ => CopyOutcome::Stay { send_bytes: Vec::new() },
            };
        }
        if ks.modifiers.alt || ks.modifiers.platform {
            return CopyOutcome::Stay { send_bytes: Vec::new() };
        }
        match ks.key.as_str() {
            "escape" => return self.handle_escape(vt),
            "enter" => return self.copy_and_exit(vt),
            "left" => { self.move_cursor(0, -1, vt); return CopyOutcome::Stay { send_bytes: Vec::new() }; }
            "down" => { self.move_cursor(1, 0, vt); return CopyOutcome::Stay { send_bytes: Vec::new() }; }
            "up" => { self.move_cursor(-1, 0, vt); return CopyOutcome::Stay { send_bytes: Vec::new() }; }
            "right" => { self.move_cursor(0, 1, vt); return CopyOutcome::Stay { send_bytes: Vec::new() }; }
            "home" => { self.move_line_start(vt); return CopyOutcome::Stay { send_bytes: Vec::new() }; }
            "end" => { self.move_line_end(vt); return CopyOutcome::Stay { send_bytes: Vec::new() }; }
            "pageup" => return self.move_page(-1, false, vt),
            "pagedown" => return self.move_page(1, false, vt),
            "space" => return self.handle_command_char(' ', vt),
            _ => {}
        }
        if let Some(ch) = command_char_from_keystroke(ks) {
            return self.handle_command_char(ch, vt);
        }
        CopyOutcome::Stay { send_bytes: Vec::new() }
    }

    /// Programmatic driver for `terminal.copy_mode --action key --key <name>`,
    /// supporting single keys plus `"/needle"` / `"?needle"` search shortcuts.
    pub(crate) fn handle_key_str(&mut self, key: &str, vt: &mut Vt) -> Result<CopyOutcome, String> {
        self.reconcile(vt);
        if self.search.prompt.is_none() && key.len() > 1 {
            if let Some(needle) = key.strip_prefix('/') {
                self.open_search(SearchDir::Forward);
                if let Some((_, q)) = self.search.prompt.as_mut() {
                    q.push_str(needle);
                }
                return Ok(self.commit_search(vt));
            }
            if let Some(needle) = key.strip_prefix('?') {
                self.open_search(SearchDir::Backward);
                if let Some((_, q)) = self.search.prompt.as_mut() {
                    q.push_str(needle);
                }
                return Ok(self.commit_search(vt));
            }
        }
        if self.search.prompt.is_some() {
            return Ok(match key {
                "escape" | "esc" => {
                    self.search.prompt = None;
                    CopyOutcome::Stay { send_bytes: Vec::new() }
                }
                "enter" => self.commit_search(vt),
                "backspace" => {
                    if let Some((_, q)) = self.search.prompt.as_mut() {
                        q.pop();
                    }
                    CopyOutcome::Stay { send_bytes: Vec::new() }
                }
                "space" => {
                    if let Some((_, q)) = self.search.prompt.as_mut() {
                        q.push(' ');
                    }
                    CopyOutcome::Stay { send_bytes: Vec::new() }
                }
                s => {
                    if let Some((_, q)) = self.search.prompt.as_mut() {
                        q.push_str(s);
                    }
                    CopyOutcome::Stay { send_bytes: Vec::new() }
                }
            });
        }
        let outcome = match key {
            "escape" | "esc" => self.handle_escape(vt),
            "enter" => self.copy_and_exit(vt),
            "left" => { self.move_cursor(0, -1, vt); CopyOutcome::Stay { send_bytes: Vec::new() } }
            "down" => { self.move_cursor(1, 0, vt); CopyOutcome::Stay { send_bytes: Vec::new() } }
            "up" => { self.move_cursor(-1, 0, vt); CopyOutcome::Stay { send_bytes: Vec::new() } }
            "right" => { self.move_cursor(0, 1, vt); CopyOutcome::Stay { send_bytes: Vec::new() } }
            "home" => { self.move_line_start(vt); CopyOutcome::Stay { send_bytes: Vec::new() } }
            "end" => { self.move_line_end(vt); CopyOutcome::Stay { send_bytes: Vec::new() } }
            "pageup" | "ctrl-b" => self.move_page(-1, false, vt),
            "pagedown" | "ctrl-f" => self.move_page(1, false, vt),
            "ctrl-u" => self.move_page(-1, true, vt),
            "ctrl-d" => self.move_page(1, true, vt),
            "space" => self.handle_command_char(' ', vt),
            s if s.chars().count() == 1 => {
                let ch = s.chars().next().unwrap();
                self.handle_command_char(ch, vt)
            }
            other => return Err(format!("terminal.copy_mode: unknown key {other:?}")),
        };
        Ok(outcome)
    }

    fn handle_search_prompt_keystroke(&mut self, ks: &Keystroke, vt: &mut Vt) -> CopyOutcome {
        if ks.modifiers.control || ks.modifiers.alt || ks.modifiers.platform {
            return CopyOutcome::Stay { send_bytes: Vec::new() };
        }
        match ks.key.as_str() {
            "escape" => {
                self.search.prompt = None;
                CopyOutcome::Stay { send_bytes: Vec::new() }
            }
            "enter" => self.commit_search(vt),
            "backspace" => {
                if let Some((_, q)) = self.search.prompt.as_mut() {
                    q.pop();
                }
                CopyOutcome::Stay { send_bytes: Vec::new() }
            }
            "space" => {
                if let Some((_, q)) = self.search.prompt.as_mut() {
                    q.push(' ');
                }
                CopyOutcome::Stay { send_bytes: Vec::new() }
            }
            _ => {
                if let Some(ch) = command_char_from_keystroke(ks)
                    && !ch.is_control()
                    && let Some((_, q)) = self.search.prompt.as_mut()
                {
                    q.push(ch);
                }
                CopyOutcome::Stay { send_bytes: Vec::new() }
            }
        }
    }

    fn handle_command_char(&mut self, ch: char, vt: &mut Vt) -> CopyOutcome {
        self.status = None;
        match ch {
            'h' => self.move_cursor(0, -1, vt),
            'j' => self.move_cursor(1, 0, vt),
            'k' => self.move_cursor(-1, 0, vt),
            'l' => self.move_cursor(0, 1, vt),
            '0' => self.move_line_start(vt),
            '^' => self.move_first_non_blank(vt),
            '$' => self.move_line_end(vt),
            'g' => self.move_history(true, vt),
            'G' => self.move_history(false, vt),
            'w' => self.move_word(WordMotion::NextStart, vt),
            'b' => self.move_word(WordMotion::PrevStart, vt),
            'e' => self.move_word(WordMotion::NextEnd, vt),
            'W' => self.move_word(WordMotion::NextBigStart, vt),
            'B' => self.move_word(WordMotion::PrevBigStart, vt),
            'E' => self.move_word(WordMotion::NextBigEnd, vt),
            '{' => self.move_paragraph(-1, vt),
            '}' => self.move_paragraph(1, vt),
            'v' | ' ' => self.toggle_selection(SelectionKind::Char, vt),
            'V' => self.toggle_selection(SelectionKind::Line, vt),
            'o' => self.swap_selection_ends(vt),
            '/' => self.open_search(SearchDir::Forward),
            '?' => self.open_search(SearchDir::Backward),
            'n' => self.repeat_search(false, vt),
            'N' => self.repeat_search(true, vt),
            'c' => return self.trigger_comment(vt),
            'y' => return self.copy_and_exit(vt),
            'q' => {
                let send_bytes = self.finish_exit(vt);
                return CopyOutcome::Exit { copy: None, send_bytes };
            }
            _ => {}
        }
        CopyOutcome::Stay { send_bytes: Vec::new() }
    }

    /// 2-stage Esc invariant (matching my-herdr-fork copy_mode.rs:149-174):
    /// 1) If a selection or active search exists, clear it and stay in Copy mode.
    /// 2) Otherwise exit Copy mode and restore scroll / alt-screen pages.
    fn handle_escape(&mut self, vt: &mut Vt) -> CopyOutcome {
        self.status = None;
        if self.search.prompt.take().is_some() {
            return CopyOutcome::Stay { send_bytes: Vec::new() };
        }
        if self.selection.take().is_some() {
            vt.clear_selection();
            return CopyOutcome::Stay { send_bytes: Vec::new() };
        }
        if self.search.active {
            vt.clear_search();
            self.search.active = false;
            self.search.matches.clear();
            self.search.current = None;
            self.search.last_query.clear();
            self.search.last_dir = None;
            return CopyOutcome::Stay { send_bytes: Vec::new() };
        }
        let send_bytes = self.finish_exit(vt);
        CopyOutcome::Exit { copy: None, send_bytes }
    }

    /// Inline comment invariant (matching my-herdr-fork state.rs:1837-1855 +
    /// herdr-tools `inline_comments = true`):
    /// - With a non-empty selection, extract text directly from Ghostty, clear the
    ///   selection highlight, and KEEP `CopyMode` active at the exact cursor and
    ///   scroll position so closing the modal note card returns directly to copy mode.
    /// - Without a selection, show an inline status hint and stay in copy mode.
    fn trigger_comment(&mut self, vt: &mut Vt) -> CopyOutcome {
        if self.selection.is_some()
            && let Some(quote) = vt.selection_text().filter(|s| !s.trim().is_empty())
        {
            self.selection = None;
            vt.clear_selection();
            self.status = None;
            return CopyOutcome::Comment(quote);
        }
        self.status = Some("Inline comments require a selection (press v or V first)".into());
        CopyOutcome::Stay { send_bytes: Vec::new() }
    }

    fn copy_and_exit(&mut self, vt: &mut Vt) -> CopyOutcome {
        let copy = if self.selection.is_some() {
            vt.selection_text()
        } else if let Some((start, end)) = self.search.current.and_then(|i| self.search.matches.get(i).copied()) {
            vt.set_selection(start, end);
            vt.selection_text()
        } else {
            None
        };
        let send_bytes = self.finish_exit(vt);
        CopyOutcome::Exit { copy, send_bytes }
    }

    fn move_cursor(&mut self, row_delta: i32, col_delta: i32, vt: &mut Vt) {
        let total = vt.scrollbar().total.max(1) as u32;
        let max_col = vt.cols.saturating_sub(1);
        if col_delta != 0 {
            self.cursor.col = (self.cursor.col as i32 + col_delta).clamp(0, max_col as i32) as u16;
            self.preferred_col = self.cursor.col;
        }
        if row_delta != 0 {
            self.cursor.row = (self.cursor.row as i64 + row_delta as i64).clamp(0, total.saturating_sub(1) as i64) as u32;
            self.cursor.col = self.preferred_col.min(max_col);
        }
        self.reveal_cursor(vt, false);
        self.sync_selection(vt);
    }

    fn move_line_start(&mut self, vt: &mut Vt) {
        self.cursor.col = 0;
        self.preferred_col = 0;
        self.sync_selection(vt);
    }

    fn move_first_non_blank(&mut self, vt: &mut Vt) {
        let mut col = None;
        vt.visit_row_cells(self.cursor.row, |x, _, cps| {
            if is_non_blank_cps(cps) {
                col = Some(x);
                return false;
            }
            true
        });
        self.cursor.col = col.unwrap_or(0).min(vt.cols.saturating_sub(1));
        self.preferred_col = self.cursor.col;
        self.sync_selection(vt);
    }

    fn move_line_end(&mut self, vt: &mut Vt) {
        let mut col = None;
        vt.visit_row_cells(self.cursor.row, |x, _, cps| {
            if is_non_blank_cps(cps) {
                col = Some(x);
            }
            true
        });
        self.cursor.col = col.unwrap_or(0).min(vt.cols.saturating_sub(1));
        self.preferred_col = self.cursor.col;
        self.sync_selection(vt);
    }

    fn move_history(&mut self, top: bool, vt: &mut Vt) {
        let total = vt.scrollbar().total.max(1) as u32;
        if top {
            self.cursor = CellPosition { row: 0, col: 0 };
            self.preferred_col = 0;
            if !vt.alt_screen() {
                vt.scroll_to_top();
            }
        } else {
            self.cursor = CellPosition { row: total.saturating_sub(1), col: 0 };
            self.preferred_col = 0;
            if !vt.alt_screen() {
                vt.scroll_to_bottom();
            }
        }
        self.sync_selection(vt);
    }

    fn move_page(&mut self, direction: i8, half_page: bool, vt: &mut Vt) -> CopyOutcome {
        self.status = None;
        if vt.alt_screen() {
            // Alt-screen paging: forward PageUp/PageDown to the fullscreen app and drop
            // any selection because the app's viewport shifts underneath it.
            if direction < 0 {
                self.app_pages_up = self.app_pages_up.saturating_add(1);
            } else {
                self.app_pages_up = self.app_pages_up.saturating_sub(1);
            }
            if self.selection.take().is_some() {
                vt.clear_selection();
            }
            let bytes = if direction < 0 { b"\x1b[5~".to_vec() } else { b"\x1b[6~".to_vec() };
            return CopyOutcome::Stay { send_bytes: bytes };
        }
        let rows = vt.rows;
        let lines = page_lines(rows, half_page) as u32;
        let sb = vt.scrollbar();
        let total = sb.total.max(1) as u32;
        let max_offset = sb.total.saturating_sub(sb.len);
        if direction < 0 {
            self.cursor.row = self.cursor.row.saturating_sub(lines);
            let next_offset = sb.offset.saturating_sub(lines as u64);
            vt.scroll_to_row(next_offset as usize);
        } else {
            self.cursor.row = self.cursor.row.saturating_add(lines).min(total.saturating_sub(1));
            let next_offset = sb.offset.saturating_add(lines as u64).min(max_offset);
            vt.scroll_to_row(next_offset as usize);
        }
        self.cursor.col = self.preferred_col.min(vt.cols.saturating_sub(1));
        self.reveal_cursor(vt, false);
        self.sync_selection(vt);
        CopyOutcome::Stay { send_bytes: Vec::new() }
    }

    /// Incremental word/WORD motion matching `my-herdr-fork/src/pane/terminal.rs:943-1090`.
    /// Loads rows lazily in the motion direction (starting from `cursor.row`) so
    /// same-row motions only touch 1 row instead of scanning `2 * MOTION_ROW_LIMIT`.
    fn move_word(&mut self, motion: WordMotion, vt: &mut Vt) {
        #[derive(Clone, Copy)]
        struct Atom {
            point: Option<CellPosition>,
            end_col: u16,
            class: TextClass,
        }

        let push_row = |vt: &Vt, row: usize, out: &mut Vec<Atom>| {
            vt.visit_row_cells(row as u32, |x, width, cps| {
                out.push(Atom {
                    point: Some(CellPosition { row: row as u32, col: x }),
                    end_col: x.saturating_add(width.saturating_sub(1)),
                    class: classify_cps(cps),
                });
                true
            });
        };

        let total = vt.scrollbar().total.max(1) as usize;
        let cur_row = self.cursor.row as usize;
        let start_row = cur_row.saturating_sub(MOTION_ROW_LIMIT);
        let end_row = cur_row.saturating_add(MOTION_ROW_LIMIT).min(total.saturating_sub(1));

        let mut cur_atoms = Vec::with_capacity(vt.cols as usize + 1);
        push_row(vt, cur_row, &mut cur_atoms);
        let Some(current) = cur_atoms.iter().position(|a| {
            a.point.is_some_and(|p| self.cursor.col >= p.col && self.cursor.col <= a.end_col)
        }) else {
            return;
        };

        let current_class = cur_atoms[current].class;

        let target = match motion {
            WordMotion::NextStart | WordMotion::NextEnd | WordMotion::NextBigStart | WordMotion::NextBigEnd => {
                // Virtual newline separator between rows so words never merge across rows.
                cur_atoms.push(Atom { point: None, end_col: 0, class: TextClass::Whitespace });
                let mut next_row = cur_row + 1;
                let mut get = |i: usize| -> Option<Atom> {
                    while i >= cur_atoms.len() && next_row <= end_row {
                        push_row(vt, next_row, &mut cur_atoms);
                        cur_atoms.push(Atom { point: None, end_col: 0, class: TextClass::Whitespace });
                        next_row += 1;
                    }
                    cur_atoms.get(i).copied()
                };
                match motion {
                    WordMotion::NextStart => {
                        let mut i = current + 1;
                        if current_class != TextClass::Whitespace {
                            while get(i).is_some_and(|a| a.class == current_class) { i += 1; }
                        }
                        while get(i).is_some_and(|a| a.class == TextClass::Whitespace) { i += 1; }
                        get(i).and_then(|a| a.point)
                    }
                    WordMotion::NextBigStart => {
                        let mut i = current + 1;
                        if current_class != TextClass::Whitespace {
                            while get(i).is_some_and(|a| a.class != TextClass::Whitespace) { i += 1; }
                        }
                        while get(i).is_some_and(|a| a.class == TextClass::Whitespace) { i += 1; }
                        get(i).and_then(|a| a.point)
                    }
                    WordMotion::NextEnd => {
                        let mut i = current + 1;
                        while get(i).is_some_and(|a| a.class == TextClass::Whitespace) { i += 1; }
                        let Some(first) = get(i) else { return; };
                        let cls = first.class;
                        while get(i + 1).is_some_and(|a| a.class == cls) { i += 1; }
                        get(i).and_then(|a| a.point)
                    }
                    WordMotion::NextBigEnd => {
                        let mut i = current + 1;
                        while get(i).is_some_and(|a| a.class == TextClass::Whitespace) { i += 1; }
                        if get(i).is_none() { return; }
                        while get(i + 1).is_some_and(|a| a.class != TextClass::Whitespace) { i += 1; }
                        get(i).and_then(|a| a.point)
                    }
                    _ => unreachable!(),
                }
            }
            WordMotion::PrevStart | WordMotion::PrevBigStart => {
                // Cells strictly before `current`, ordered backwards so index 0 is immediately left.
                let mut rev_atoms: Vec<Atom> = cur_atoms[..current].iter().copied().rev().collect();
                let mut prev_row = cur_row;
                let mut scratch = Vec::with_capacity(vt.cols as usize);
                let mut get = |i: usize| -> Option<Atom> {
                    while i >= rev_atoms.len() && prev_row > start_row {
                        prev_row -= 1;
                        rev_atoms.push(Atom { point: None, end_col: 0, class: TextClass::Whitespace });
                        scratch.clear();
                        push_row(vt, prev_row, &mut scratch);
                        rev_atoms.extend(scratch.iter().copied().rev());
                    }
                    rev_atoms.get(i).copied()
                };
                let mut i = 0;
                while get(i).is_some_and(|a| a.class == TextClass::Whitespace) { i += 1; }
                let Some(first) = get(i) else { return; };
                let cls = first.class;
                match motion {
                    WordMotion::PrevStart => {
                        while get(i + 1).is_some_and(|a| a.class == cls) { i += 1; }
                    }
                    WordMotion::PrevBigStart => {
                        while get(i + 1).is_some_and(|a| a.class != TextClass::Whitespace) { i += 1; }
                    }
                    _ => unreachable!(),
                }
                get(i).and_then(|a| a.point)
            }
        };

        if let Some(pos) = target {
            self.cursor = pos;
            self.preferred_col = pos.col;
            self.reveal_cursor(vt, false);
            self.sync_selection(vt);
        }
    }

    fn move_paragraph(&mut self, direction: i8, vt: &mut Vt) {
        let total = vt.scrollbar().total.max(1) as usize;
        let current = self.cursor.row as usize;
        let is_blank = |r: usize| {
            let mut blank = true;
            let ok = vt.visit_row_cells(r as u32, |_, _, cps| {
                if is_non_blank_cps(cps) {
                    blank = false;
                    return false;
                }
                true
            });
            ok && blank
        };
        let limit = total.min(MOTION_ROW_LIMIT);
        // Skip any consecutive blank lines we start on, then jump to the next blank line
        // bounding the paragraph (classic Vim `{`/`}`, also matching TUI on non-blank rows).
        let mut saw_non_blank = !is_blank(current);
        let mut target_row = None;
        for distance in 1..limit {
            let candidate = if direction < 0 {
                match current.checked_sub(distance) {
                    Some(r) => r,
                    None => break,
                }
            } else {
                let r = current.saturating_add(distance);
                if r >= total { break; }
                r
            };
            if is_blank(candidate) {
                if saw_non_blank {
                    target_row = Some(candidate as u32);
                    break;
                }
            } else {
                saw_non_blank = true;
            }
        }
        // Fall back to top/bottom when no blank line sits in that direction.
        let next = target_row.unwrap_or(if direction < 0 { 0 } else { total.saturating_sub(1) as u32 });
        self.cursor = CellPosition { row: next, col: 0 };
        self.preferred_col = 0;
        self.reveal_cursor(vt, false);
        self.sync_selection(vt);
    }

    fn toggle_selection(&mut self, kind: SelectionKind, vt: &mut Vt) {
        match self.selection {
            Some(sel) if sel.kind == kind => {
                self.selection = None;
                vt.clear_selection();
            }
            Some(mut sel) => {
                sel.kind = kind;
                self.selection = Some(sel);
                self.sync_selection(vt);
            }
            None => {
                self.selection = Some(SelectionAnchor { anchor: self.cursor, kind });
                self.sync_selection(vt);
            }
        }
    }

    fn swap_selection_ends(&mut self, vt: &mut Vt) {
        let Some(mut sel) = self.selection else { return };
        std::mem::swap(&mut sel.anchor, &mut self.cursor);
        self.preferred_col = self.cursor.col;
        self.selection = Some(sel);
        self.reveal_cursor(vt, false);
        self.sync_selection(vt);
    }

    fn sync_selection(&self, vt: &mut Vt) {
        let Some(sel) = self.selection else { return };
        match sel.kind {
            SelectionKind::Char => {
                vt.set_selection(sel.anchor, self.cursor);
            }
            SelectionKind::Line => {
                let min_row = sel.anchor.row.min(self.cursor.row);
                let max_row = sel.anchor.row.max(self.cursor.row);
                let end_col = vt.cols.saturating_sub(1);
                vt.set_selection(
                    CellPosition { row: min_row, col: 0 },
                    CellPosition { row: max_row, col: end_col },
                );
            }
        }
    }

    fn reveal_cursor(&self, vt: &mut Vt, reserve_mode_bar_row: bool) {
        if vt.alt_screen() {
            return;
        }
        let sb = vt.scrollbar();
        let visible_rows = sb.len.max(1);
        let max_offset_in_view =
            visible_rows.saturating_sub(if reserve_mode_bar_row && visible_rows > 1 { 2 } else { 1 });
        let row = self.cursor.row as u64;
        if row < sb.offset {
            vt.scroll_to_row(row as usize);
        } else if row > sb.offset.saturating_add(max_offset_in_view) {
            vt.scroll_to_row(row.saturating_sub(max_offset_in_view) as usize);
        }
    }

    fn open_search(&mut self, dir: SearchDir) {
        self.status = None;
        self.search.prompt = Some((dir, String::new()));
    }

    fn commit_search(&mut self, vt: &mut Vt) -> CopyOutcome {
        let Some((dir, query)) = self.search.prompt.take() else {
            return CopyOutcome::Stay { send_bytes: Vec::new() };
        };
        if query.is_empty() {
            return CopyOutcome::Stay { send_bytes: Vec::new() };
        }
        self.run_search(query, dir, false, vt);
        CopyOutcome::Stay { send_bytes: Vec::new() }
    }

    fn repeat_search(&mut self, reverse: bool, vt: &mut Vt) {
        let Some(base_dir) = self.search.last_dir else { return };
        if self.search.last_query.is_empty() {
            return;
        }
        let dir = if reverse {
            match base_dir {
                SearchDir::Forward => SearchDir::Backward,
                SearchDir::Backward => SearchDir::Forward,
            }
        } else {
            base_dir
        };
        let query = self.search.last_query.clone();
        self.run_search(query, dir, true, vt);
    }

    fn run_search(&mut self, query: String, dir: SearchDir, repeat: bool, vt: &mut Vt) {
        // Drive Ghostty's native search for GPU-rendered match highlights in paint_grid.
        vt.start_search(&query);
        for _ in 0..SEARCH_TICK_LIMIT {
            if vt.search_progress().complete {
                break;
            }
            vt.progress_search();
        }
        let matches = find_all_matches(vt, &query);
        self.search.last_query = query.clone();
        if !repeat {
            self.search.last_dir = Some(dir);
        }
        self.search.active = true;
        self.search.matches = matches;
        if self.search.matches.is_empty() {
            self.search.current = None;
            self.status = Some(format!("No match: {query}"));
            return;
        }
        let origin = if repeat {
            self.search
                .current
                .and_then(|i| self.search.matches.get(i).copied())
                .filter(|(s, _)| *s == self.cursor)
                .map(|(s, e)| match dir {
                    SearchDir::Forward => e,
                    SearchDir::Backward => s,
                })
                .unwrap_or(self.cursor)
        } else {
            self.cursor
        };
        let target_idx = match dir {
            SearchDir::Forward => self
                .search
                .matches
                .iter()
                .position(|(s, _)| (s.row, s.col) > (origin.row, origin.col))
                .unwrap_or(0),
            SearchDir::Backward => self
                .search
                .matches
                .iter()
                .rposition(|(_, e)| (e.row, e.col) < (origin.row, origin.col))
                .unwrap_or(self.search.matches.len() - 1),
        };
        self.search.current = Some(target_idx);
        let (start, _) = self.search.matches[target_idx];
        self.cursor = start;
        self.preferred_col = start.col;
        self.status = None;
        self.reveal_cursor(vt, true);
        self.sync_selection(vt);
    }

    /// Render the bottom 1-row `COPY` mode bar inside `TerminalView`, anchored to
    /// the bottom edge without resizing the terminal grid.
    pub(crate) fn render_bar(&self, address: &str, vt: &Vt) -> AnyElement {
        let c = crate::settings::color;
        let sb = vt.scrollbar();
        let total = sb.total.max(1);
        let pill = match self.selection.map(|s| s.kind) {
            None => "COPY",
            Some(SelectionKind::Char) => "VISUAL",
            Some(SelectionKind::Line) => "V-LINE",
        };
        let mut right = format!("{}/{}", self.cursor.row as u64 + 1, total);
        if self.app_pages_up > 0 {
            right.push_str(&format!(" [app +{}]", self.app_pages_up));
        }

        let center: AnyElement = if let Some((dir, query)) = &self.search.prompt {
            let prefix = match dir {
                SearchDir::Forward => "/",
                SearchDir::Backward => "?",
            };
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_color(c("foreground"))
                        .child(format!("{prefix}{query}█")),
                )
                .child(div().flex_none().text_color(c("muted")).child("enter search · esc cancel"))
                .into_any_element()
        } else if let Some(msg) = &self.status {
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_color(c("blocked"))
                .child(msg.clone())
                .into_any_element()
        } else {
            let match_info = if self.search.active {
                match self.search.current {
                    Some(idx) => format!(" [{}/{}]", idx + 1, self.search.matches.len()),
                    None => " [0/0]".to_owned(),
                }
            } else {
                String::new()
            };
            let exit_hint = if self.selection.is_some() || self.search.active {
                "esc clear · q exit"
            } else {
                "q/esc exit"
            };
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_color(c("muted"))
                .child(format!(
                    "hjkl/w/b/{{}} move · v/V select · / search{match_info} · c comment · y copy · {exit_hint}"
                ))
                .into_any_element()
        };

        div()
            .id(SharedString::from(format!("copy-bar-{address}")))
            .debug_selector(|| "copy-bar".to_owned())
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(24.0))
            .px_2()
            .gap_2()
            .flex()
            .items_center()
            .bg(c("surface"))
            .border_t_1()
            .border_color(c("border"))
            .text_xs()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex_none()
                    .px(px(6.0))
                    .py(px(1.0))
                    .rounded_sm()
                    .bg(c("accent"))
                    .text_color(c("accent_foreground"))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(pill),
            )
            .child(center)
            .child(div().flex_none().text_color(c("muted")).child(right))
            .into_any_element()
    }
}

fn page_lines(rows: u16, half_page: bool) -> usize {
    if rows <= 2 {
        1
    } else if half_page {
        usize::from(rows / 2)
    } else {
        usize::from(rows - 2)
    }
}

fn is_non_blank_cps(cps: &[u32]) -> bool {
    cps.iter().filter_map(|c| char::from_u32(*c)).any(|c| !c.is_whitespace())
}

fn classify_cps(cps: &[u32]) -> TextClass {
    let Some(ch) = cps.first().and_then(|c| char::from_u32(*c)) else {
        return TextClass::Whitespace;
    };
    if ch.is_whitespace() {
        TextClass::Whitespace
    } else if ch.is_ascii() && WORD_SEPARATORS.contains(ch) {
        TextClass::Separator
    } else {
        TextClass::Word
    }
}

fn command_char_from_keystroke(ks: &Keystroke) -> Option<char> {
    if ks.modifiers.control || ks.modifiers.alt || ks.modifiers.platform {
        return None;
    }
    if let Some(s) = &ks.key_char {
        let mut chars = s.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            // If shift is set but key_char stayed equal to unshifted key (e.g. simulated `shift-6`),
            // map it through shifted_ascii.
            if ks.modifiers.shift && s == &ks.key {
                return Some(shifted_ascii(ch).unwrap_or(ch));
            }
            return Some(ch);
        }
    }
    let mut chars = ks.key.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if ks.modifiers.shift {
        Some(shifted_ascii(ch).unwrap_or(ch))
    } else {
        Some(ch)
    }
}

fn shifted_ascii(ch: char) -> Option<char> {
    match ch {
        'a'..='z' => Some(ch.to_ascii_uppercase()),
        '1' => Some('!'),
        '2' => Some('@'),
        '3' => Some('#'),
        '4' => Some('$'),
        '5' => Some('%'),
        '6' => Some('^'),
        '7' => Some('&'),
        '8' => Some('*'),
        '9' => Some('('),
        '0' => Some(')'),
        '-' => Some('_'),
        '=' => Some('+'),
        '[' => Some('{'),
        ']' => Some('}'),
        '\\' => Some('|'),
        ';' => Some(':'),
        '\'' => Some('"'),
        ',' => Some('<'),
        '.' => Some('>'),
        '/' => Some('?'),
        '`' => Some('~'),
        _ => None,
    }
}

/// Case-insensitive substring scan across screen/scrollback rows, returning exact
/// inclusive `(start, end)` screen cell coordinates per match without per-cell `String` allocs.
fn find_all_matches(vt: &Vt, query: &str) -> Vec<(CellPosition, CellPosition)> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let total = vt.scrollbar().total as u32;
    let mut results = Vec::new();
    let mut lower = String::with_capacity(vt.cols as usize);
    let mut spans: Vec<(usize, usize, u16, u16)> = Vec::with_capacity(vt.cols as usize);
    for row in 0..total {
        lower.clear();
        spans.clear();
        let ok = vt.visit_row_cells(row, |x, width, cps| {
            let start = lower.len();
            if cps.is_empty() {
                lower.push(' ');
            } else {
                for ch in cps.iter().filter_map(|c| char::from_u32(*c)) {
                    for lc in ch.to_lowercase() {
                        lower.push(lc);
                    }
                }
                if lower.len() == start {
                    lower.push(' ');
                }
            }
            let end = lower.len();
            spans.push((start, end, x, x.saturating_add(width.saturating_sub(1))));
            true
        });
        if !ok {
            continue;
        }
        let mut search_from = 0;
        while let Some(rel) = lower[search_from..].find(&needle) {
            let byte_start = search_from + rel;
            let byte_end = byte_start + needle.len();
            let start_col = spans
                .iter()
                .find(|&&(s, e, _, _)| byte_start >= s && byte_start < e)
                .map(|s| s.2);
            let end_col = spans
                .iter()
                .rev()
                .find(|&&(s, e, _, _)| byte_end > s && byte_end <= e)
                .map(|s| s.3);
            if let (Some(sc), Some(ec)) = (start_col, end_col) {
                results.push((CellPosition { row, col: sc }, CellPosition { row, col: ec }));
            }
            search_from = byte_end.max(byte_start + 1);
            if search_from >= lower.len() {
                break;
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vt_with_lines(cols: u16, rows: u16, lines: &[&str]) -> Vt {
        let mut vt = Vt::new(cols, rows);
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                vt.write(b"\r\n");
            }
            vt.write(line.as_bytes());
        }
        vt
    }

    #[test]
    fn entry_starts_at_live_cursor_or_viewport_bottom_left_when_scrolled() {
        let mut vt = vt_with_lines(30, 4, &["l0", "l1", "l2", "l3", "l4", "prompt> "]);
        let cm = CopyMode::enter(&mut vt);
        assert_eq!(cm.cursor(), vt.cursor_screen_position());
        assert_eq!(cm.entry_offset_from_bottom, 0);

        vt.scroll_to_row(0);
        let cm_scrolled = CopyMode::enter(&mut vt);
        assert_eq!(cm_scrolled.cursor(), CellPosition { row: 3, col: 0 });
        assert!(cm_scrolled.entry_offset_from_bottom > 0);
    }

    #[test]
    fn basic_line_word_and_paragraph_motions() {
        let mut vt = vt_with_lines(
            40,
            8,
            &[
                "  foo.bar baz-qux",
                "second line",
                "",
                "next paragraph",
            ],
        );
        let mut cm = CopyMode::enter(&mut vt);
        cm.handle_key_str("g", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 0 });

        cm.handle_key_str("^", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 2 });

        cm.handle_key_str("w", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 5 }); // '.'
        cm.handle_key_str("w", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 6 }); // 'b' in bar
        cm.handle_key_str("e", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 8 }); // 'r' in bar
        cm.handle_key_str("W", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 10 }); // 'baz-qux'
        cm.handle_key_str("E", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 16 }); // 'x'
        cm.handle_key_str("B", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 10 }); // 'baz-qux'
        cm.handle_key_str("b", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 6 }); // 'bar'

        // Word motions cross row boundaries both forward and backward.
        cm.handle_key_str("$", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 16 });
        cm.handle_key_str("w", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 1, col: 0 }); // 'second'
        cm.handle_key_str("b", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 14 }); // 'qux'
        cm.handle_key_str("0", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 0 });

        cm.handle_key_str("}", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 2, col: 0 }); // blank line
        cm.handle_key_str("G", &mut vt).unwrap();
        assert_eq!(cm.cursor().row, vt.scrollbar().total as u32 - 1);
        cm.handle_key_str("{", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 2, col: 0 });
    }

    #[test]
    fn char_and_line_selection_o_swap_and_comment_preserves_copy_mode() {
        let mut vt = vt_with_lines(30, 6, &["alpha beta", "gamma delta", "epsilon"]);
        let mut cm = CopyMode::enter(&mut vt);
        cm.handle_key_str("g", &mut vt).unwrap();

        // `c` without selection stays in copy mode with an inline status hint.
        let out = cm.handle_key_str("c", &mut vt).unwrap();
        assert_eq!(out, CopyOutcome::Stay { send_bytes: Vec::new() });
        assert!(cm.status.as_deref().unwrap_or("").contains("selection"));

        // `v` / `space` char selection + `o` swap.
        cm.handle_key_str("space", &mut vt).unwrap();
        cm.handle_key_str("e", &mut vt).unwrap();
        assert_eq!(vt.selection_text().as_deref(), Some("alpha"));
        cm.handle_key_str("o", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 0 });

        // `V` switches to linewise and spans across rows.
        cm.handle_key_str("V", &mut vt).unwrap();
        cm.handle_key_str("j", &mut vt).unwrap();
        let saved_cursor = cm.cursor();
        assert_eq!(vt.selection_text().as_deref(), Some("alpha beta\ngamma delta"));

        // `c` extracts quote, clears selection, and keeps CopyMode alive at exact cursor!
        let out = cm.handle_key_str("c", &mut vt).unwrap();
        assert_eq!(out, CopyOutcome::Comment("alpha beta\ngamma delta".into()));
        assert!(cm.selection.is_none());
        assert_eq!(vt.selection_text(), None);
        assert_eq!(cm.cursor(), saved_cursor);
    }

    #[test]
    fn search_forward_backward_repeat_and_copy_match() {
        let mut vt = vt_with_lines(30, 6, &["one foo two", "three bar", "four FOO five"]);
        let mut cm = CopyMode::enter(&mut vt);
        cm.handle_key_str("g", &mut vt).unwrap();

        cm.handle_key_str("/foo", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 4 });
        assert_eq!(cm.search.matches.len(), 2);

        cm.handle_key_str("n", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 2, col: 5 });

        cm.handle_key_str("N", &mut vt).unwrap();
        assert_eq!(cm.cursor(), CellPosition { row: 0, col: 4 });

        // `y` with no explicit selection copies the current search match and exits.
        let out = cm.handle_key_str("y", &mut vt).unwrap();
        assert_eq!(out, CopyOutcome::Exit { copy: Some("foo".into()), send_bytes: Vec::new() });
    }

    #[test]
    fn two_stage_escape_clears_selection_then_search_then_exits() {
        let mut vt = vt_with_lines(30, 6, &["hello world", "second line"]);
        let mut cm = CopyMode::enter(&mut vt);
        cm.handle_key_str("g", &mut vt).unwrap();
        cm.handle_key_str("/world", &mut vt).unwrap();
        cm.handle_key_str("v", &mut vt).unwrap();
        cm.handle_key_str("l", &mut vt).unwrap();

        // 1st Esc clears selection, keeps search and copy mode.
        assert_eq!(cm.handle_key_str("escape", &mut vt).unwrap(), CopyOutcome::Stay { send_bytes: Vec::new() });
        assert!(cm.selection.is_none());
        assert!(cm.search.active);

        // 2nd Esc clears search, keeps copy mode.
        assert_eq!(cm.handle_key_str("escape", &mut vt).unwrap(), CopyOutcome::Stay { send_bytes: Vec::new() });
        assert!(!cm.search.active);

        // 3rd Esc exits copy mode.
        assert_eq!(cm.handle_key_str("escape", &mut vt).unwrap(), CopyOutcome::Exit { copy: None, send_bytes: Vec::new() });
    }

    #[test]
    fn alt_screen_paging_tracks_app_pages_up_and_resets_across_screen_switches() {
        let mut vt = Vt::new(30, 6);
        vt.write(b"\x1b[?1049hpage content");
        assert!(vt.alt_screen());
        let mut cm = CopyMode::enter(&mut vt);
        cm.handle_key_str("V", &mut vt).unwrap();
        assert!(cm.selection.is_some());

        // ctrl-u sends PageUp, clears selection, increments app_pages_up.
        assert_eq!(cm.handle_key_str("ctrl-u", &mut vt).unwrap(), CopyOutcome::Stay { send_bytes: b"\x1b[5~".to_vec() });
        assert!(cm.selection.is_none());
        assert_eq!(cm.app_pages_up, 1);

        assert_eq!(cm.handle_key_str("pageup", &mut vt).unwrap(), CopyOutcome::Stay { send_bytes: b"\x1b[5~".to_vec() });
        assert_eq!(cm.app_pages_up, 2);

        assert_eq!(cm.handle_key_str("ctrl-d", &mut vt).unwrap(), CopyOutcome::Stay { send_bytes: b"\x1b[6~".to_vec() });
        assert_eq!(cm.app_pages_up, 1);

        // Exit while still on alt-screen restores remaining 1 PageDown.
        assert_eq!(
            cm.handle_key_str("q", &mut vt).unwrap(),
            CopyOutcome::Exit { copy: None, send_bytes: b"\x1b[6~".to_vec() }
        );

        // Leaving alt-screen then re-entering a new alt-screen app must NOT leak stale PageDowns.
        let mut cm2 = CopyMode::enter(&mut vt);
        cm2.handle_key_str("ctrl-u", &mut vt).unwrap();
        assert_eq!(cm2.app_pages_up, 1);
        vt.write(b"\x1b[?1049l");
        cm2.reconcile(&mut vt);
        assert_eq!(cm2.app_pages_up, 0);
        vt.write(b"\x1b[?1049hnew fullscreen app");
        assert_eq!(
            cm2.handle_key_str("q", &mut vt).unwrap(),
            CopyOutcome::Exit { copy: None, send_bytes: Vec::new() }
        );

        // Entering copy mode on normal screen, then an alt-screen app opens and we page up:
        // exiting while still in that alt-screen app restores its PageDown.
        vt.write(b"\x1b[?1049l");
        let mut cm3 = CopyMode::enter(&mut vt);
        vt.write(b"\x1b[?1049hfullscreen later");
        cm3.handle_key_str("ctrl-u", &mut vt).unwrap();
        assert_eq!(cm3.app_pages_up, 1);
        assert_eq!(
            cm3.handle_key_str("q", &mut vt).unwrap(),
            CopyOutcome::Exit { copy: None, send_bytes: b"\x1b[6~".to_vec() }
        );
    }

    #[test]
    fn reconcile_handles_resize_alt_switch_and_scrollback_shrink() {
        let mut vt = vt_with_lines(30, 6, &["l0", "l1", "l2", "l3", "l4", "l5", "l6", "target word"]);
        let mut cm = CopyMode::enter(&mut vt);
        cm.handle_key_str("g", &mut vt).unwrap();
        cm.handle_key_str("/target", &mut vt).unwrap();
        assert_eq!(cm.search.matches.len(), 1);
        assert!(cm.search.current.is_some());

        // Resizing drops selection and clears stale search current index while keeping active query valid.
        cm.handle_key_str("v", &mut vt).unwrap();
        vt.resize(15, 4, 8, 16);
        assert!(cm.reconcile(&mut vt));
        assert!(cm.selection.is_none());
        assert!(cm.search.current.is_none());

        // Clearing scrollback without resizing clamps cursor into remaining rows.
        cm.handle_key_str("G", &mut vt).unwrap();
        vt.write(b"\x1b[3J\x1b[2J\x1b[H");
        cm.reconcile(&mut vt);
        assert!(cm.cursor().row < vt.scrollbar().total.max(1) as u32);

        // Switching to alt-screen clears stale primary-screen search state completely.
        vt.write(b"\x1b[?1049hfresh alt");
        cm.reconcile(&mut vt);
        assert!(!cm.search.active);
        assert!(cm.search.matches.is_empty());
    }

    #[test]
    fn responsive_comment_capture_sizes_match_tui_profiles() {
        assert_eq!(comment_capture_size(180), (0.95, 0.90));
        assert_eq!(comment_capture_size(310), (0.95, 0.90));
        assert_eq!(comment_capture_size(311), (0.90, 0.90));
        assert_eq!(comment_capture_size(399), (0.90, 0.90));
        assert_eq!(comment_capture_size(400), (0.50, 0.70));
        assert_eq!(comment_capture_size(520), (0.50, 0.70));
    }

    /// Verifies Captain's Amendment 1: with `gpui_base` kit keys and the real
    /// `keys.json` structure (global `ctrl-a` prefixes, `alt-*`, `layout:shadowfax`,
    /// and `terminal` bindings including `ctrl-u`/`ctrl-d`/`ctrl-b`/`ctrl-f`),
    /// every §1 copy-mode key reaches `CopyMode` without leaking to the PTY, while
    /// pane/tab switching keys still reach the window.
    #[gpui::test]
    fn copy_mode_keys_reach_copy_mode_under_real_keys_json(cx: &mut gpui::TestAppContext) {
        use gpui::*;
        use std::cell::RefCell;
        use std::rc::Rc;

        struct Root {
            term: Entity<TerminalView>,
            actions: Rc<RefCell<Vec<String>>>,
            _sub: Subscription,
        }

        impl Render for Root {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let mut ctx = KeyContext::default();
                ctx.add("Herdr");
                ctx.set("layout", "shadowfax");
                div()
                    .key_context(ctx)
                    .size_full()
                    .on_action(cx.listener(|this, a: &gpui_shell::action::ShellAction, _, cx| {
                        this.actions.borrow_mut().push(a.id().to_owned());
                        cx.stop_propagation();
                    }))
                    .child(self.term.clone())
            }
        }

        cx.update(gpui_base::init);
        let keys_json = r#"{
            "global": {
                "cmd-1": "tab.select:1",
                "ctrl-tab": "tab.next",
                "ctrl-a ctrl-a": "send:\u0001",
                "ctrl-a h": "pane.go:left",
                "ctrl-a 1": "tab.select:1",
                "ctrl-a [": "terminal.copy_mode",
                "alt-w": "terminal.copy_mode",
                "alt-h": "pane.go:left",
                "alt-l": "pane.go:right",
                "alt-a": "tab.prev",
                "alt-;": "tab.next"
            },
            "layout:shadowfax": {
                "ctrl-a h": "frame-shadowfax.k:ctrl-a h",
                "ctrl-a 1": "frame-shadowfax.k:ctrl-a 1",
                "alt-h": "frame-shadowfax.k:alt-h",
                "alt-l": "frame-shadowfax.k:alt-l",
                "alt-a": "frame-shadowfax.k:alt-a",
                "alt-;": "frame-shadowfax.k:alt-;",
                "alt-w": "frame-shadowfax.k:alt-w"
            },
            "terminal": {
                "cmd-f": "terminal.search",
                "cmd-v": "terminal.paste",
                "ctrl-u": "send:\u0015",
                "ctrl-d": "send:\u0004",
                "ctrl-b": "send:\u0002",
                "ctrl-f": "send:\u0006",
                "alt-left": "send:\u001bb",
                "cmd-left": "send:\u0001"
            }
        }"#;
        let bindings = crate::keys::parse(keys_json).expect("keys parse");
        cx.update(|app| crate::keys::install(bindings, app));

        let actions = Rc::new(RefCell::new(Vec::<String>::new()));
        let comments = Rc::new(RefCell::new(Vec::<String>::new()));
        let (root, cx) = cx.add_window_view(|_, cx| {
            let term = cx.new(|cx| {
                let t = TerminalView::attach(crate::hc::offline(), "t1".into(), (40, 8), cx);
                let mut text = String::new();
                for i in 0..20 {
                    if i == 10 {
                        text.push_str("\r\n");
                    } else {
                        text.push_str(&format!("  row{i:02} alpha.beta foo-bar\r\n"));
                    }
                }
                t.write_vt(text.as_bytes());
                t
            });
            let comments_sub = comments.clone();
            let _sub = cx.subscribe(&term, move |_, _, ev: &TerminalEvent, _| {
                if let TerminalEvent::CopyComment { quote } = ev {
                    comments_sub.borrow_mut().push(quote.clone());
                }
            });
            Root { term, actions: actions.clone(), _sub }
        });
        cx.run_until_parked();

        // Opening find bar then entering copy mode must close find bar and route keys to copy mode.
        cx.update(|window, app| {
            let term = root.read(app).term.clone();
            let focus = term.read(app).focus_handle().clone();
            focus.focus(window, app);
            term.update(app, |t, cx| {
                t.open_search(window, cx);
                t.grid.borrow_mut().marked = Some("ime".into());
                t.enter_copy_mode(Some(window), cx);
                assert!(!t.search_open, "enter_copy_mode closes open find bar");
                assert!(t.grid.borrow().marked.is_none(), "enter_copy_mode clears IME marked text");
            });
        });
        cx.run_until_parked();

        let cursor = |cx: &mut gpui::VisualTestContext| {
            root.read_with(cx, |r, cx| r.term.read(cx).copy_mode.as_ref().map(|cm| cm.cursor()))
        };
        assert!(cursor(cx).is_some());

        cx.simulate_keystrokes("g");
        cx.run_until_parked();
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 0 }));

        // Motions: ^ w e W E B b $ 0 h j k l arrows home end { } G
        cx.simulate_keystrokes("shift-6"); // ^
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 2 }));
        cx.simulate_keystrokes("w");
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 8 }));
        cx.simulate_keystrokes("e");
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 12 }));
        cx.simulate_keystrokes("shift-w"); // W
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 19 }));
        cx.simulate_keystrokes("shift-e"); // E
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 25 }));
        cx.simulate_keystrokes("shift-b"); // B
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 19 }));
        cx.simulate_keystrokes("b");
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 14 }));
        cx.simulate_keystrokes("shift-4"); // $
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 25 }));
        cx.simulate_keystrokes("0");
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 0 }));
        cx.simulate_keystrokes("l j k h right down up left end home");
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 0 }));
        cx.simulate_keystrokes("shift-]"); // }
        assert_eq!(cursor(cx), Some(CellPosition { row: 10, col: 0 }));
        cx.simulate_keystrokes("shift-g"); // G
        let last_row = cursor(cx).unwrap().row;
        assert!(last_row >= 19);
        cx.simulate_keystrokes("shift-["); // {
        assert_eq!(cursor(cx), Some(CellPosition { row: 10, col: 0 }));

        // Paging keys must reach copy mode even when terminal section binds ctrl-u/d/b/f!
        cx.simulate_keystrokes("ctrl-u");
        assert!(cursor(cx).unwrap().row < 10, "ctrl-u paged up");
        cx.simulate_keystrokes("ctrl-d");
        assert!(cursor(cx).unwrap().row >= 10, "ctrl-d paged down");
        cx.simulate_keystrokes("ctrl-b");
        assert!(cursor(cx).unwrap().row < 10, "ctrl-b paged up");
        cx.simulate_keystrokes("ctrl-f");
        assert!(cursor(cx).unwrap().row >= 10, "ctrl-f paged down");
        cx.simulate_keystrokes("pageup pagedown");

        // Search (/foo, n, N, ?row00)
        cx.simulate_keystrokes("g / f o x backspace o enter");
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 19 }));
        cx.simulate_keystrokes("n");
        assert_eq!(cursor(cx), Some(CellPosition { row: 1, col: 19 }));
        cx.simulate_keystrokes("shift-n");
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 19 }));
        cx.simulate_keystrokes("shift-/ r o w 0 0 enter");
        assert_eq!(cursor(cx), Some(CellPosition { row: 0, col: 2 }));

        // Selection (v, space, V, o) and inline comment `c` keeping copy mode active!
        cx.simulate_keystrokes("v l o");
        cx.simulate_keystrokes("space"); // toggles Char off
        cx.simulate_keystrokes("shift-v"); // V line selection
        cx.simulate_keystrokes("c");
        cx.run_until_parked();
        assert_eq!(comments.borrow().len(), 1, "c emitted CopyComment");
        assert!(comments.borrow()[0].contains("row00"), "quote={:?}", comments.borrow()[0]);
        assert!(cursor(cx).is_some(), "c keeps copy mode active");

        // External terminal.clear_selection must clear copy_mode.selection too (e.g. Alt-c).
        cx.simulate_keystrokes("v");
        cx.update(|_, app| {
            let term = root.read(app).term.clone();
            term.update(app, |t, cx| {
                assert!(t.copy_mode.as_ref().unwrap().selection.is_some());
                t.clear_selection(cx);
                assert!(t.copy_mode.as_ref().unwrap().selection.is_none());
            });
        });

        // Pane/tab switching keys STILL work in copy mode!
        actions.borrow_mut().clear();
        cx.simulate_keystrokes("alt-h alt-l alt-a alt-; cmd-1 ctrl-a h ctrl-a 1");
        cx.run_until_parked();
        assert_eq!(
            *actions.borrow(),
            vec![
                "frame-shadowfax.k:alt-h",
                "frame-shadowfax.k:alt-l",
                "frame-shadowfax.k:alt-a",
                "frame-shadowfax.k:alt-;",
                "tab.select:1",
                "frame-shadowfax.k:ctrl-a h",
                "frame-shadowfax.k:ctrl-a 1",
            ]
        );

        // Terminal-only `send:` bindings must NOT leak into the PTY during copy mode.
        cx.simulate_keystrokes("alt-left cmd-left");
        cx.run_until_parked();
        root.read_with(cx, |r, cx| assert!(r.term.read(cx).take_sent().is_empty(), "no bytes leaked to PTY"));

        // 2-stage Esc: selection -> search -> exit.
        cx.simulate_keystrokes("v");
        cx.simulate_keystrokes("escape");
        assert!(cursor(cx).is_some(), "1st Esc cleared selection, stayed in copy mode");
        cx.simulate_keystrokes("escape");
        assert!(cursor(cx).is_some(), "2nd Esc cleared search, stayed in copy mode");
        cx.simulate_keystrokes("escape");
        assert!(cursor(cx).is_none(), "3rd Esc exited copy mode");

        // Re-enter and test `y`, `enter`, and `q`.
        cx.update(|window, app| {
            let term = root.read(app).term.clone();
            term.update(app, |t, cx| t.enter_copy_mode(Some(window), cx));
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("g shift-v y");
        cx.run_until_parked();
        assert!(cursor(cx).is_none());
        cx.update(|_, app| assert!(app.read_from_clipboard().unwrap().text().unwrap().contains("row00")));

        cx.update(|window, app| {
            let term = root.read(app).term.clone();
            term.update(app, |t, cx| t.enter_copy_mode(Some(window), cx));
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("q");
        cx.run_until_parked();
        assert!(cursor(cx).is_none());
    }
}
