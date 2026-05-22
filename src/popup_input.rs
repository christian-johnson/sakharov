use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    app::App,
    popup::{PopupAction, PopupContent, PopupTarget},
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

    // Completion popups (InsertText) are "soft": only navigation keys are
    // captured.  Any other key dismisses the popup and falls through to normal
    // handling so that typing into the buffer continues uninterrupted.
    let is_completion = popup.on_confirm == PopupTarget::InsertText;

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
                    // Return payload when present (navigate/location pickers),
                    // otherwise fall back to the label (command palette, completion).
                    let text = item.payload.as_deref().unwrap_or(&item.label).to_owned();
                    PopupAction::Confirm(text)
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
            if is_completion {
                // Let the backspace reach handle_insert to delete the character.
                PopupAction::DismissPassthrough
            } else {
                if let PopupContent::List(ref mut s) = popup.content {
                    s.pop_filter_char();
                }
                PopupAction::Continue
            }
        }

        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            if is_completion {
                // Let the character reach handle_insert so the buffer updates.
                PopupAction::DismissPassthrough
            } else {
                if let PopupContent::List(ref mut s) = popup.content {
                    s.push_filter_char(c);
                }
                PopupAction::Continue
            }
        }

        _ => {
            if is_completion {
                PopupAction::DismissPassthrough
            } else {
                PopupAction::Continue
            }
        }
    }
}
