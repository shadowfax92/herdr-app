//! Fork-only mod host helpers kept out of `src/mods.rs`: tracks all attached
//! `TerminalView` entities by terminal address (including overlays, peeks, and
//! remote terminals) so `herdr.selectionText(terminal?)` reads Ghostty's
//! formatted selection synchronously without touching the OS clipboard.

use super::*;

thread_local! {
    static TERM_VIEWS: RefCell<HashMap<String, Vec<gpui::WeakEntity<crate::terminal::TerminalView>>>> =
        RefCell::new(HashMap::new());
}

/// Register a terminal view by address so `herdr.selectionText()` can read its VT selection.
pub fn track_terminal_view(address: String, view: &gpui::Entity<crate::terminal::TerminalView>) {
    TERM_VIEWS.with_borrow_mut(|views| {
        let list = views.entry(address).or_default();
        let id = view.entity_id();
        list.retain(|w| w.entity_id() != id && w.upgrade().is_some());
        list.push(view.downgrade());
    });
}

/// Remove a detached terminal view from `TERM_VIEWS`.
pub fn untrack_terminal_view(address: &str, view: &gpui::Entity<crate::terminal::TerminalView>) {
    TERM_VIEWS.with_borrow_mut(|views| {
        let id = view.entity_id();
        if let Some(list) = views.get_mut(address) {
            list.retain(|w| w.entity_id() != id && w.upgrade().is_some());
            if list.is_empty() {
                views.remove(address);
            }
        }
    });
}

/// Read the formatted Ghostty selection text for `address` (or the focused terminal).
/// When multiple views share an address (e.g. a peek dialog over its tab pane),
/// prefer an on-screen, focused, and more recently attached view.
pub fn selection_text_for(address: Option<&str>, cx: &gpui::App) -> Option<String> {
    let target = match address {
        Some(a) if !a.is_empty() => a.to_owned(),
        _ => SNAPSHOT.with(|s| {
            let f = &s.borrow().focused;
            f["shown"].as_str().or_else(|| f["terminal"].as_str()).map(str::to_owned)
        })?,
    };
    TERM_VIEWS.with_borrow(|views| {
        let list = views.get(&target)?;
        let mut upgraded: Vec<_> = list.iter().filter_map(|w| w.upgrade()).enumerate().collect();
        upgraded.sort_by_key(|(idx, v)| {
            let view = v.read(cx);
            (!view.on_screen_this_frame(), !view.was_focused(), std::cmp::Reverse(*idx))
        });
        upgraded.into_iter().find_map(|(_, v)| v.read(cx).selection_text())
    })
}

pub(super) fn selection_text_host(a: &gpui_shell::HostArguments) -> gpui_shell::HostResult {
    let addr = match a.get(0) {
        None | Some(HostValue::Null) => None,
        Some(HostValue::Str(s)) => Some(s.clone()),
        Some(_) => return Err(HostError::new("selectionText: terminal must be a string")),
    };
    let text = gpui_shell::with_current_app(|cx| selection_text_for(addr.as_deref(), cx)).flatten();
    Ok(text.map(HostValue::Str).unwrap_or(HostValue::Null))
}
