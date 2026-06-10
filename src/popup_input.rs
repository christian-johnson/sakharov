use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    app::App,
    popup::{DocPanel, PopupAction, PopupContent, PopupTarget},
};

/// Handle a key event when a popup is open.
/// Returns the action to take; the caller is responsible for acting on it.
pub fn handle_key(app: &mut App, key: KeyEvent) -> PopupAction {
    let popup = match app.popup.as_mut() {
        Some(p) => p,
        None => return PopupAction::Continue,
    };

    // KeyHints are purely informational — any key dismisses with passthrough.
    if matches!(popup.content, PopupContent::KeyHints(_)) {
        return PopupAction::DismissPassthrough;
    }

    let is_completion = popup.on_confirm == PopupTarget::InsertText;

    // Completion popups use a passive → focused two-state model.
    //
    // Passive (default): the popup is a hint overlay; all keys fall through
    // to insert mode so Enter inserts a newline, typed chars update the buffer,
    // etc.  Press Tab to engage with the list.
    //
    // Focused (after Tab): navigation keys are captured.  Press Tab or Esc to
    // dismiss and return to plain insert mode.
    if is_completion {
        if let PopupContent::List(ref mut list) = popup.content {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);

            // Confirm the current selection, inserting its payload/label.
            let confirm = |list: &crate::popup::ListState| -> PopupAction {
                if let Some(item) = list.selected_item() {
                    PopupAction::Confirm(item.confirm_payload())
                } else {
                    PopupAction::Dismiss
                }
            };

            if !list.focused {
                // Passive: hint overlay. Tab engages; everything else types.
                return match key.code {
                    KeyCode::Tab => {
                        list.focused = true;
                        PopupAction::Continue
                    }
                    _ => PopupAction::DismissPassthrough,
                };
            }

            // ---- Focused, Search sub-mode ('/' row open) ----------------------
            // Printable keys build the fuzzy query (reusing the palette scoring);
            // arrows / Ctrl-n/p still navigate. Esc backs out to Nav.
            if list.search.is_some() {
                return match key.code {
                    KeyCode::Esc => {
                        list.search = None;
                        list.selected = 0;
                        PopupAction::Continue
                    }
                    // Tab fully disengages back to passive typing.
                    KeyCode::Tab => {
                        list.search = None;
                        list.doc = None;
                        list.focused = false;
                        PopupAction::Continue
                    }
                    KeyCode::Enter if !ctrl => confirm(list),
                    KeyCode::Down => { list.move_down(); PopupAction::Continue }
                    KeyCode::Up | KeyCode::BackTab => { list.move_up(); PopupAction::Continue }
                    KeyCode::Char('n') if ctrl => { list.move_down(); PopupAction::Continue }
                    KeyCode::Char('p') if ctrl => { list.move_up(); PopupAction::Continue }
                    KeyCode::Backspace => { list.pop_search_char(); PopupAction::Continue }
                    KeyCode::Char(c) if !ctrl && !alt => {
                        list.push_search_char(c);
                        PopupAction::Continue
                    }
                    _ => PopupAction::Continue,
                };
            }

            // ---- Focused, Nav sub-mode --------------------------------------
            return match key.code {
                // Esc peels one layer: close docs first, else dismiss.
                KeyCode::Esc => {
                    if list.doc.is_some() {
                        list.doc = None;
                        PopupAction::Continue
                    } else {
                        list.focused = false;
                        PopupAction::Dismiss
                    }
                }
                // Tab → back to passive mode; popup stays alive.
                KeyCode::Tab => {
                    list.focused = false;
                    list.doc = None;
                    PopupAction::Continue
                }
                KeyCode::Enter if !ctrl => confirm(list),
                // '/' opens the fuzzy-search row (closes docs — letters type there).
                KeyCode::Char('/') if !ctrl => {
                    list.search = Some(String::new());
                    list.doc = None;
                    list.selected = 0;
                    PopupAction::Continue
                }
                // 'K' toggles the documentation side panel for the selection.
                // Filled in by exec::refresh_completion_doc on the Continue path.
                KeyCode::Char('K') if !ctrl => {
                    if list.doc.is_some() {
                        list.doc = None;
                    } else {
                        list.doc = Some(DocPanel { lines: Vec::new(), loading: false });
                    }
                    PopupAction::Continue
                }
                KeyCode::Down => { list.move_down(); PopupAction::Continue }
                KeyCode::Up | KeyCode::BackTab => { list.move_up(); PopupAction::Continue }
                KeyCode::Char('j') if !ctrl => { list.move_down(); PopupAction::Continue }
                KeyCode::Char('k') if !ctrl => { list.move_up(); PopupAction::Continue }
                KeyCode::Char('n') if ctrl => { list.move_down(); PopupAction::Continue }
                KeyCode::Char('p') if ctrl => { list.move_up(); PopupAction::Continue }
                // Any other key: deactivate popup and let the key reach insert mode.
                _ => {
                    list.focused = false;
                    list.doc = None;
                    PopupAction::ClosePassthrough
                }
            };
        }
        return PopupAction::DismissPassthrough;
    }

    // Two-phase list: ESC transitions typing → navigating; second ESC dismisses.
    if let PopupContent::List(ref mut s) = popup.content {
        if s.two_phase {
            match key.code {
                KeyCode::Esc => {
                    if s.navigating {
                        return PopupAction::Dismiss;
                    } else {
                        s.navigating = true;
                        return PopupAction::Continue;
                    }
                }
                KeyCode::Char('j') if s.navigating => {
                    s.move_down();
                    return PopupAction::Continue;
                }
                KeyCode::Char('k') if s.navigating => {
                    s.move_up();
                    return PopupAction::Continue;
                }
                KeyCode::Char('i') if s.navigating => {
                    s.navigating = false;
                    return PopupAction::Continue;
                }
                KeyCode::Char(_) if s.navigating => {
                    // In navigation mode, printable keys are consumed but do nothing.
                    return PopupAction::Continue;
                }
                _ => {}
            }
        }
    }

    match key.code {
        KeyCode::Esc => PopupAction::Dismiss,

        KeyCode::Enter => match &popup.content {
            PopupContent::List(state) => {
                if let Some(item) = state.selected_item() {
                    PopupAction::Confirm(item.confirm_payload())
                } else {
                    PopupAction::Dismiss
                }
            }
            PopupContent::Text(_) => PopupAction::Dismiss,
            PopupContent::KeyHints(_) => unreachable!(),
        },

        KeyCode::Up | KeyCode::BackTab => {
            if let PopupContent::List(ref mut s) = popup.content {
                s.move_up();
            } else if let PopupContent::Text(ref mut s) = popup.content {
                s.scroll_up();
            }
            PopupAction::Continue
        }

        KeyCode::Down | KeyCode::Tab => {
            if let PopupContent::List(ref mut s) = popup.content {
                s.move_down();
            } else if let PopupContent::Text(ref mut s) = popup.content {
                s.scroll_down(10);
            }
            PopupAction::Continue
        }

        // Ctrl+N / Ctrl+P — alternative navigation
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let PopupContent::List(ref mut s) = popup.content {
                s.move_down();
            }
            PopupAction::Continue
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let PopupContent::List(ref mut s) = popup.content {
                s.move_up();
            }
            PopupAction::Continue
        }

        KeyCode::Backspace => {
            if let PopupContent::List(ref mut s) = popup.content {
                s.pop_filter_char();
            }
            PopupAction::Continue
        }

        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            if let PopupContent::List(ref mut s) = popup.content {
                s.push_filter_char(c);
            }
            PopupAction::Continue
        }

        _ => PopupAction::Continue,
    }
}
