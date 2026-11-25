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
}
