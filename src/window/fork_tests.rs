// Fork-only AppWindow tests included by `src/window.rs` (`mod tests`).

#[test]
fn closing_top_dialog_restores_focus_to_overlay_terminal() {
    in_own_process("restore_focus_overlay_terminal");
}

#[gpui::test]
fn restore_focus_overlay_terminal(cx: &mut gpui::TestAppContext) {
    use serde_json::json;
    if !running_alone("restore_focus_overlay_terminal") {
        return;
    }
    let (view, cx) = open_window(cx, "unused", "", json!({
        "id": "root", "type": "block", "content": { "tab": "active" }
    }));
    cx.update(|window, cx| view.update(cx, |this, cx| {
        this.live.insert("t1".into(), json!({"address": "t1", "state": "live"}));
        this.live.insert("t2".into(), json!({"address": "t2", "state": "live"}));
        this.model.add_space("sp1", "", "/tmp", crate::model::Tab::new("tb1", "", "p1", "t1"));
        this.sync(window, cx);

        // Plain modal dialog (no terminal): closing a dialog on top restores its frame focus.
        this.open_overlay("plain".into(), json!({ "kind": "dialog", "modal": true }), window, cx);
        let plain_focus = this.overlays.iter().find(|o| o.id == "plain").unwrap().focus.clone();
        this.open_overlay("top".into(), json!({ "kind": "dialog", "modal": true }), window, cx);
        assert!(!plain_focus.is_focused(window));
        this.close_overlay("top", window, cx);
        assert!(plain_focus.is_focused(window), "closing top dialog restores frame focus for non-terminal dialog");
        this.close_overlay("plain", window, cx);

        // Terminal-hosting modal dialog: closing a dialog on top restores focus to its TerminalView.
        this.open_overlay("scratch".into(), json!({
            "kind": "dialog",
            "width": 0.8,
            "height": 0.8,
            "layout": {
                "type": "column",
                "children": [
                    { "id": "scratch-term", "type": "block", "content": { "terminal": "t2" } }
                ]
            }
        }), window, cx);
        let scratch_focus = this.terms.values().find(|t| t.address == "t2").expect("overlay terminal attached").focus.clone();
        assert!(scratch_focus.is_focused(window), "open_overlay focuses overlay terminal");
        this.open_overlay("note".into(), json!({ "kind": "dialog", "modal": true }), window, cx);
        assert!(!scratch_focus.is_focused(window), "top dialog takes focus");
        this.close_overlay("note", window, cx);
        assert!(scratch_focus.is_focused(window), "closing top dialog restores focus to overlay terminal");
    }));
}
