use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    app::App,
    popup::{PopupAction, PopupContent},
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

    match key.code {
        KeyCode::Esc => PopupAction::Dismiss,

        KeyCode::Enter => match &popup.content {
            PopupContent::List(state) => {
                if let Some(item) = state.selected_item() {
                    PopupAction::Confirm(item.label.clone())
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
