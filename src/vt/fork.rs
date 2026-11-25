//! Fork-only `Vt` helpers kept out of `src/vt.rs` so upstream rebases stay clean.
//! Provides screen-space cursor/selection setters and zero-allocation row cell
//! traversal used by native vi copy mode (`src/terminal/copy_mode.rs`).

use super::*;

impl Vt {
    /// Expose the live terminal cursor in screen-space coordinates for copy mode entry.
    pub fn cursor_screen_position(&self) -> CellPosition {
        self.cursor_position()
    }

    /// Install a Ghostty selection from screen-space cell coordinates.
    pub fn set_selection(&mut self, start: CellPosition, end: CellPosition) -> bool {
        let (start, end) = if (start.row, start.col) <= (end.row, end.col) {
            (start, end)
        } else {
            (end, start)
        };
        let Some(start_ref) = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, start.col, start.row) else {
            return false;
        };
        let Some(end_ref) = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, end.col, end.row) else {
            return false;
        };
        let selection = GhosttySelection {
            size: std::mem::size_of::<GhosttySelection>(),
            start: start_ref,
            end: end_ref,
            ..Default::default()
        };
        unsafe {
            ghostty_terminal_set(
                self.term,
                GHOSTTY_TERMINAL_OPT_SELECTION,
                &selection as *const _ as *const c_void,
            ) == GHOSTTY_SUCCESS
        }
    }

    /// Visit non-spacer cells of screen row `y` as `(start_col, cell_width, codepoints)`
    /// without heap-allocating per cell. Return `false` from `f` to stop early.
    /// Returns `false` if row `y` is out of bounds.
    pub fn visit_row_cells(&self, y: u32, mut f: impl FnMut(u16, u16, &[u32]) -> bool) -> bool {
        let Some(mut r) = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, 0, y) else {
            return false;
        };
        for x in 0..self.cols {
            r.x = x;
            let wide = unsafe {
                let mut cell: GhosttyCell = Default::default();
                ghostty_grid_ref_cell(&r, &mut cell);
                let mut wide: GhosttyCellWide = GHOSTTY_CELL_WIDE_NARROW;
                ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &mut wide as *mut _ as *mut c_void);
                wide
            };
            if wide == GHOSTTY_CELL_WIDE_SPACER_TAIL || wide == GHOSTTY_CELL_WIDE_SPACER_HEAD {
                continue;
            }
            let mut cps = [0u32; 16];
            let mut len = 0usize;
            unsafe { ghostty_grid_ref_graphemes(&r, cps.as_mut_ptr(), cps.len(), &mut len) };
            let width = if wide == GHOSTTY_CELL_WIDE_WIDE { 2 } else { 1 };
            if !f(x, width, &cps[..len.min(cps.len())]) {
                break;
            }
        }
        true
    }

    /// Read screen row `y` as `(start_col, cell_width, grapheme)` cells.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn row_cells(&self, y: u32) -> Option<Vec<(u16, u16, String)>> {
        let mut cells = Vec::with_capacity(self.cols as usize);
        let ok = self.visit_row_cells(y, |x, width, cps| {
            let mut text: String = cps.iter().filter_map(|c| char::from_u32(*c)).collect();
            if text.is_empty() {
                text.push(' ');
            }
            cells.push((x, width, text));
            true
        });
        ok.then_some(cells)
    }

    /// Read screen row `y` as plain text (empty cells become spaces).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn row_text(&self, y: u32) -> Option<String> {
        let mut text = String::with_capacity(self.cols as usize);
        let ok = self.visit_row_cells(y, |_, _, cps| {
            let before = text.len();
            text.extend(cps.iter().filter_map(|c| char::from_u32(*c)));
            if text.len() == before {
                text.push(' ');
            }
            true
        });
        ok.then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_selection_and_selection_text_work_in_both_directions() {
        let mut vt = Vt::new(30, 5);
        vt.write(b"alpha beta gamma\r\nsecond line here\r\n");
        assert!(vt.set_selection(CellPosition { row: 0, col: 6 }, CellPosition { row: 0, col: 9 }));
        assert_eq!(vt.selection_text().as_deref(), Some("beta"));
        // Reversed coordinates select the same inclusive range.
        assert!(vt.set_selection(CellPosition { row: 1, col: 5 }, CellPosition { row: 0, col: 11 }));
        assert_eq!(vt.selection_text().as_deref(), Some("gamma\nsecond"));
        vt.clear_selection();
        assert_eq!(vt.selection_text(), None);
    }

    #[test]
    fn row_text_handles_scrollback_and_wide_chars() {
        let mut vt = Vt::new(20, 3);
        vt.write("first line\r\n日本語 ok\r\nthird\r\nfourth\r\n".as_bytes());
        assert_eq!(vt.row_text(0).as_deref().map(str::trim_end), Some("first line"));
        let wide_cells = vt.row_cells(1).expect("scrollback row exists");
        assert_eq!(wide_cells[0], (0, 2, "日".to_owned()));
        assert_eq!(wide_cells[1], (2, 2, "本".to_owned()));
        assert_eq!(wide_cells[2], (4, 2, "語".to_owned()));
        assert_eq!(vt.row_text(1).as_deref().map(str::trim_end), Some("日本語 ok"));
        assert!(vt.row_text(999).is_none());
    }

    #[test]
    fn paste_bytes_respects_bracketed_paste_mode() {
        let mut vt = Vt::new(40, 5);
        assert_eq!(vt.paste_bytes("line1\nline2"), b"line1\rline2");
        vt.write(b"\x1b[?2004h");
        assert_eq!(vt.paste_bytes("line1\nline2"), b"\x1b[200~line1\nline2\x1b[201~");
        // Stray end-of-bracketed-paste sequences are stripped before wrapping.
        assert_eq!(vt.paste_bytes("a\x1b[201~b"), b"\x1b[200~ab\x1b[201~");
    }
}
