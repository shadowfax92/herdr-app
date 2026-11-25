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

#[test]
fn terminal_paste_text_targets_terminal_without_touching_clipboard() {
    in_own_process("paste_text_and_selection_text");
}

#[gpui::test]
fn paste_text_and_selection_text(cx: &mut gpui::TestAppContext) {
    use gpui::ClipboardItem;
    use serde_json::json;
    if !running_alone("paste_text_and_selection_text") {
        return;
    }
    let (view, cx) = open_window(cx, "unused", "", json!({
        "id": "root", "type": "block", "content": { "tab": "active" }
    }));
    cx.update(|window, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string("SENTINEL_CLIPBOARD".into()));
        view.update(cx, |this, cx| {
            this.live.insert("t1".into(), json!({"address": "t1", "state": "live"}));
            this.live.insert("t2".into(), json!({"address": "t2", "state": "live"}));
            this.live.insert("box/t3".into(), json!({"address": "box/t3", "state": "live"}));
            this.model.add_space("sp1", "", "/tmp", crate::model::Tab::new("tb1", "", "p1", "t1"));
            assert!(this.model.split("p1", crate::layout::Axis::Row, "p2", "t2"));
            assert!(this.model.split("p2", crate::layout::Axis::Row, "p3", "box/t3"));
            this.model.focus_pane("p1");
            this.sync(window, cx);

            // Enable bracketed paste and write text on t2 (unfocused pane) and remote box/t3.
            this.terms["p2"].view.read(cx).write_vt(b"\x1b[?2004hhello world\r\n");
            this.terms["p2"].view.update(cx, |v, cx| {
                v.selection_command("terminal.select", &json!({"at": {"row": 0, "col": 0}, "mode": "word"}), cx).unwrap();
            });
            this.terms["p3"].view.read(cx).write_vt(b"remote output\r\n");
            this.terms["p3"].view.update(cx, |v, cx| {
                v.selection_command("terminal.select", &json!({"at": {"row": 0, "col": 0}, "mode": "word"}), cx).unwrap();
            });

            // selectionText reads local and remote terminals synchronously without touching clipboard.
            assert_eq!(crate::mods::fork::selection_text_for(Some("t2"), cx).as_deref(), Some("hello"));
            assert_eq!(crate::mods::fork::selection_text_for(Some("box/t3"), cx).as_deref(), Some("remote"));

            // When a peek overlay shows the same address as a pane, the overlay's selection wins.
            this.open_overlay("peek-t2".into(), json!({
                "kind": "dialog",
                "width": 0.8,
                "height": 0.8,
                "layout": {
                    "type": "column",
                    "children": [{ "id": "peek-term", "type": "block", "content": { "terminal": "t2" } }]
                }
            }), window, cx);
            let peek_view = this.terms.iter().find(|(id, t)| *id != "p2" && t.address == "t2").unwrap().1.view.clone();
            peek_view.read(cx).write_vt(b"overlay text\r\n");
            peek_view.update(cx, |v, cx| {
                v.mark_shown();
                v.selection_command("terminal.select", &json!({"at": {"row": 0, "col": 0}, "mode": "word"}), cx).unwrap();
            });
            this.terms["p2"].view.read(cx).mark_shown();
            assert_eq!(crate::mods::fork::selection_text_for(Some("t2"), cx).as_deref(), Some("overlay"));
            this.close_overlay("peek-t2", window, cx);

            // terminal.paste with {terminal, text} sends bracketed bytes (allowing \n and \t) to t2, not focused t1.
            this.run("terminal.paste", json!({"terminal": "t2", "text": "> hello\n\n\tfix it\n"}), window, cx);
            assert_eq!(this.terms["p1"].view.read(cx).take_sent(), Vec::<u8>::new());
            assert_eq!(this.terms["p2"].view.read(cx).take_sent(), b"\x1b[200~> hello\n\n\tfix it\n\x1b[201~");

            // Control characters (ESC, CR, Ctrl-C, C1 CSI) must be rejected without writing to the PTY.
            for bad in ["bad\x1b[201~inject", "bad\rsubmit", "bad\x03ctrlc", "bad\u{009b}201~c1"] {
                let res = this.run_as("terminal.paste", json!({"terminal": "t2", "text": bad}), false, window, cx);
                assert!(res.get("error").is_some(), "expected control-char error for {bad:?}, got {res}");
                assert_eq!(this.terms["p2"].view.read(cx).take_sent(), Vec::<u8>::new());
            }

            assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()).as_deref(), Some("SENTINEL_CLIPBOARD"));
        });
    });
}
