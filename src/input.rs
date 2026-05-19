use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    app::App,
    command::Command,
    exec,
    keymap::KeyBinding,
    mode::{FindDir, Mode},
    motion,
    notebook_state::NotebookEditMode,
    popup::{PopupAction, PopupContent, PopupTarget},
    selection::Selection,
};



/// Dispatch a key event to the appropriate handler based on the current mode.
pub fn handle_key(app: &mut App, key: KeyEvent) {
    app.message = None;

    // Popup takes priority over all other input.
    // For completion (InsertText) popups we keep the popup alive when the user
    // types printable chars or backspaces — the filter is updated afterwards.
    let had_completion_popup = app.popup.as_ref()
        .map(|p| p.on_confirm == PopupTarget::InsertText)
        .unwrap_or(false);

    if app.popup.is_some() {
        let action = crate::popup_input::handle_key(app, key);
        match action {
            PopupAction::Dismiss => {
                app.popup = None;
                return;
            }
            PopupAction::DismissPassthrough => {
                if had_completion_popup {
                    // Keep the popup alive; let the key reach handle_insert
                    // so the char is inserted, then sync the filter below.
                } else {
                    app.popup = None;
                }
                // fall through to normal handling below
            }
            PopupAction::Confirm(text) => {
                let target = app.popup.as_ref().map(|p| p.on_confirm.clone());
                app.popup = None;
                if let Some(target) = target {
                    handle_popup_confirm(app, target, text);
                }
                return;
            }
            PopupAction::Continue => return,
        }
    }

    // Ctrl+C is a global hint in all modes.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.message = Some("use :q to quit, :q! to force quit".into());
        return;
    }

    // Ctrl+Enter: save cell and close overlay from any mode.
    if app.notebook_cell_edit.is_some()
        && key.code == KeyCode::Enter
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        exec::execute(app, &Command::NotebookCloseCellEdit);
        return;
    }

    // Notebook navigation mode — only when NOT in the cell-edit overlay.
    if app.notebook.is_some() && app.notebook_cell_edit.is_none() {
        handle_notebook_key(app, key);
        return;
    }

    match app.mode.clone() {
        Mode::Normal => {
            let kb = KeyBinding::from(key);
            if let Some(cmds) = app.keymap.lookup_normal(&kb).map(|v| v.to_vec()) {
                exec::run_many(app, &cmds);
            }
        }
        Mode::Select => {
            let kb = KeyBinding::from(key);
            if let Some(cmds) = app.keymap.lookup_select(&kb).map(|v| v.to_vec()) {
                exec::run_many(app, &cmds);
            }
        }
        Mode::Insert => handle_insert(app, key),
        Mode::Command => handle_command(app, key),
        Mode::Goto => handle_goto(app, key),
        Mode::FindChar { dir, till } => handle_find_char(app, key, dir, till),
        Mode::Search { forward } => handle_search(app, key, forward),
    }

    // After inserting a char (or backspacing) while a completion popup was open,
    // sync the popup filter to the word prefix now at the cursor so the list
    // narrows as the user types.
    if had_completion_popup && app.mode == Mode::Insert {
        sync_completion_filter(app);
    }
}

// ---------------------------------------------------------------------------
// Insert mode
// ---------------------------------------------------------------------------

fn handle_insert(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            exec::execute(app, &Command::EnterNormal);
        }
        KeyCode::Backspace => {
            let pos = app.selection.head;
            if pos > 0 {
                begin_insert_edit(app);
                app.buffer.remove_raw(pos - 1, pos);
                app.selection = Selection::point(pos - 1);
                exec::recompute_highlights(app);
            }
        }
        KeyCode::Delete => {
            let pos = app.selection.head;
            let len = app.buffer.rope.len_chars();
            if pos < len {
                begin_insert_edit(app);
                app.buffer.remove_raw(pos, pos + 1);
                app.selection = Selection::point(pos.min(app.buffer.rope.len_chars().saturating_sub(1)));
                exec::recompute_highlights(app);
            }
        }
        KeyCode::Enter => {
            begin_insert_edit(app);
            let pos = app.selection.head;
            app.buffer.insert_raw(pos, "\n");
            app.selection = Selection::point(pos + 1);
            exec::recompute_highlights(app);
        }
        KeyCode::Left => {
            app.selection = motion::move_left(&app.buffer.rope, app.selection, false);
        }
        KeyCode::Right => {
            let pos = app.selection.head;
            let len = app.buffer.rope.len_chars();
            app.selection = Selection::point((pos + 1).min(len));
        }
        KeyCode::Up => {
            app.selection = motion::move_up(&app.buffer.rope, app.selection, false);
        }
        KeyCode::Down => {
            app.selection = motion::move_down(&app.buffer.rope, app.selection, false);
        }
        KeyCode::Tab => {
            begin_insert_edit(app);
            let pos = app.selection.head;
            app.buffer.insert_raw(pos, "\t");
            app.selection = Selection::point(pos + 1);
            exec::recompute_highlights(app);
        }
        // Ctrl+Space arrives as NUL (ASCII 0) on most terminals.
        KeyCode::Null => {
            exec::execute(app, &Command::LspRequestCompletion);
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+Space fallback for terminals that send it as Char(' ')+CONTROL.
                if c == ' ' {
                    exec::execute(app, &Command::LspRequestCompletion);
                }
                return;
            }
            begin_insert_edit(app);
            let pos = app.selection.head;
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            app.buffer.insert_raw(pos, s);
            app.selection = Selection::point(pos + 1);
            exec::recompute_highlights(app);
            exec::lsp_did_change(app);
            // Auto-trigger completion after `.` or `:`
            if c == '.' || c == ':' {
                exec::execute(app, &Command::LspRequestCompletion);
            }
        }
        _ => {}
    }

    exec::update_scroll(app);
}

// ---------------------------------------------------------------------------
// Command mode
// ---------------------------------------------------------------------------

fn handle_command(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.command_buf.clear();
        }
        KeyCode::Backspace => {
            app.command_buf.pop();
        }
        KeyCode::Enter => {
            let input = app.command_buf.trim().to_string();
            app.command_buf.clear();
            app.mode = Mode::Normal;
            if let Some(cmd) = Command::parse(&input) {
                exec::execute(app, &cmd);
            } else {
                app.message = Some(format!("Unknown command: {input}"));
            }
        }
        KeyCode::Char(c) => {
            app.command_buf.push(c);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Goto mode (after 'g')
// ---------------------------------------------------------------------------

fn handle_goto(app: &mut App, key: KeyEvent) {
    app.mode = Mode::Normal;
    let extend = false;
    match key.code {
        KeyCode::Char('g') => {
            app.selection = motion::goto_file_start(&app.buffer.rope, app.selection, extend);
        }
        KeyCode::Char('e') => {
            app.selection = motion::goto_file_end(&app.buffer.rope, app.selection, extend);
        }
        KeyCode::Char('h') | KeyCode::Char('s') => {
            app.selection = motion::move_line_first_non_ws(&app.buffer.rope, app.selection, extend);
        }
        KeyCode::Char('l') => {
            app.selection = motion::move_line_end(&app.buffer.rope, app.selection, extend);
        }
        // LSP goto bindings
        KeyCode::Char('d') => exec::execute(app, &Command::LspGotoDefinition),
        KeyCode::Char('r') => exec::execute(app, &Command::LspGotoReferences),
        KeyCode::Char('y') => exec::execute(app, &Command::LspGotoTypeDefinition),
        KeyCode::Char('i') => exec::execute(app, &Command::LspGotoImplementation),
        KeyCode::Esc => {}
        _ => {}
    }
    exec::update_scroll(app);
}

// ---------------------------------------------------------------------------
// FindChar mode
// ---------------------------------------------------------------------------

fn handle_find_char(app: &mut App, key: KeyEvent, dir: FindDir, till: bool) {
    app.mode = Mode::Normal;
    if let KeyCode::Char(c) = key.code {
        let rope = &app.buffer.rope;
        let sel = app.selection;
        app.selection = match dir {
            FindDir::Forward => motion::find_char_forward(rope, sel, c, till, false),
            FindDir::Backward => motion::find_char_backward(rope, sel, c, till, false),
        };
        exec::update_scroll(app);
    }
}

// ---------------------------------------------------------------------------
// Search mode
// ---------------------------------------------------------------------------

fn handle_search(app: &mut App, key: KeyEvent, forward: bool) {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.message = None;
        }
        KeyCode::Backspace => {
            app.search_query.pop();
            exec::search_compute_matches(app);
        }
        KeyCode::Enter => {
            app.mode = Mode::Normal;
            exec::search_compute_matches(app);
            exec::search_jump(app, !forward);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.search_query.push(c);
            // Live preview: recompute and jump as the user types.
            exec::search_compute_matches(app);
            if !app.search_matches.is_empty() {
                exec::search_jump(app, !forward);
                app.mode = Mode::Search { forward };
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Completion filter sync
// ---------------------------------------------------------------------------

/// After a char is inserted (or deleted) in Insert mode, update the completion
/// popup's filter string to match the word prefix immediately before the cursor.
/// Dismisses the popup if the current filter has no matches.
fn sync_completion_filter(app: &mut App) {
    let pos = app.selection.head;
    let prefix: String = {
        let rope = &app.buffer.rope;
        let mut i = pos;
        while i > 0 {
            let c = rope.char(i - 1);
            if c.is_alphanumeric() || c == '_' {
                i -= 1;
            } else {
                break;
            }
        }
        rope.slice(i..pos).to_string()
    };

    let dismiss = {
        let mut should_dismiss = false;
        if let Some(ref mut popup) = app.popup {
            if popup.on_confirm == PopupTarget::InsertText {
                if let PopupContent::List(ref mut list) = popup.content {
                    list.filter = prefix.clone();
                    list.selected = 0;
                    // Dismiss when something is typed but nothing matches.
                    should_dismiss =
                        !prefix.is_empty() && list.filtered_indices().is_empty();
                }
            }
        }
        should_dismiss
    };

    if dismiss {
        app.popup = None;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Snapshot the buffer once at the start of each Insert session for undo coalescing.
/// Subsequent edits in the same session use raw (no-snapshot) methods.
fn begin_insert_edit(app: &mut App) {
    if !app.insert_session_active {
        app.buffer.begin_edit_session();
        app.insert_session_active = true;
    }
}

// ---------------------------------------------------------------------------
// Notebook key handling
// ---------------------------------------------------------------------------

fn handle_notebook_key(app: &mut App, key: KeyEvent) {
    // Determine the current notebook edit mode (avoid borrowing app twice).
    let nb_mode = app
        .notebook
        .as_ref()
        .map(|(_, s)| s.mode.clone())
        .unwrap_or(NotebookEditMode::Navigate);

    match nb_mode {
        NotebookEditMode::Navigate => {
            // Command mode is handled separately.
            if app.mode == crate::mode::Mode::Command {
                handle_command(app, key);
                return;
            }
            let kb = KeyBinding::from(key);
            if let Some(cmds) = app.keymap.lookup_notebook_navigate(&kb).map(|v| v.to_vec()) {
                exec::run_many(app, &cmds);
            }
        }
        NotebookEditMode::Edit => {
            handle_notebook_edit_key(app, key);
        }
    }
}

fn handle_notebook_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            exec::execute(app, &Command::NotebookExitEdit);
        }
        KeyCode::Backspace => {
            begin_notebook_edit(app);
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                let rope = &mut nb.cells[state.focused_cell].source;
                if state.cursor_pos > 0 {
                    state.cursor_pos -= 1;
                    rope.remove(state.cursor_pos..state.cursor_pos + 1);
                    nb.modified = true;
                }
            }
        }
        KeyCode::Delete => {
            begin_notebook_edit(app);
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                let rope = &mut nb.cells[state.focused_cell].source;
                let len = rope.len_chars();
                if state.cursor_pos < len {
                    rope.remove(state.cursor_pos..state.cursor_pos + 1);
                    nb.modified = true;
                }
            }
        }
        KeyCode::Enter => {
            begin_notebook_edit(app);
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                let pos = state.cursor_pos;
                nb.cells[state.focused_cell].source.insert(pos, "\n");
                state.cursor_pos = pos + 1;
                nb.modified = true;
            }
        }
        KeyCode::Left => {
            if let Some((_, ref mut state)) = app.notebook {
                state.cursor_pos = state.cursor_pos.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if let Some((ref nb, ref mut state)) = app.notebook {
                let len = nb.cells[state.focused_cell].source.len_chars();
                state.cursor_pos = (state.cursor_pos + 1).min(len);
            }
        }
        KeyCode::Up => {
            if let Some((ref nb, ref mut state)) = app.notebook {
                let rope = &nb.cells[state.focused_cell].source;
                // Move to same column on previous line.
                let pos = state.cursor_pos.min(rope.len_chars().saturating_sub(1));
                if rope.len_chars() == 0 {
                    return;
                }
                let line_idx = rope.char_to_line(pos);
                if line_idx == 0 {
                    state.cursor_pos = 0;
                    return;
                }
                let col = pos - rope.line_to_char(line_idx);
                let prev_line_start = rope.line_to_char(line_idx - 1);
                let prev_line_len = rope.line(line_idx - 1).len_chars().saturating_sub(1);
                state.cursor_pos = prev_line_start + col.min(prev_line_len);
            }
        }
        KeyCode::Down => {
            if let Some((ref nb, ref mut state)) = app.notebook {
                let rope = &nb.cells[state.focused_cell].source;
                if rope.len_chars() == 0 {
                    return;
                }
                let pos = state.cursor_pos.min(rope.len_chars().saturating_sub(1));
                let line_idx = rope.char_to_line(pos);
                let total_lines = rope.len_lines();
                if line_idx + 1 >= total_lines {
                    return;
                }
                let col = pos - rope.line_to_char(line_idx);
                let next_line_start = rope.line_to_char(line_idx + 1);
                let next_line_len = rope.line(line_idx + 1).len_chars().saturating_sub(1);
                state.cursor_pos = next_line_start + col.min(next_line_len);
            }
        }
        KeyCode::Tab => {
            begin_notebook_edit(app);
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                let pos = state.cursor_pos;
                nb.cells[state.focused_cell].source.insert(pos, "\t");
                state.cursor_pos = pos + 1;
                nb.modified = true;
            }
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                return;
            }
            begin_notebook_edit(app);
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                let pos = state.cursor_pos;
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                nb.cells[state.focused_cell].source.insert(pos, s);
                state.cursor_pos = pos + 1;
                nb.modified = true;
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Popup confirm handler
// ---------------------------------------------------------------------------

fn handle_popup_confirm(app: &mut App, target: PopupTarget, text: String) {
    match target {
        PopupTarget::ExecuteCommand => {
            if let Some(cmd) = Command::parse(&text) {
                exec::execute(app, &cmd);
            } else {
                app.message = Some(format!("Unknown command: {text}"));
            }
        }
        PopupTarget::InsertText => {
            let pos = app.selection.head;
            // Delete the word prefix the user has already typed so the
            // completion replaces it rather than appending after it.
            let word_start = {
                let rope = &app.buffer.rope;
                let mut i = pos;
                while i > 0 {
                    let c = rope.char(i - 1);
                    if c.is_alphanumeric() || c == '_' {
                        i -= 1;
                    } else {
                        break;
                    }
                }
                i
            };
            if word_start < pos {
                app.buffer.remove(word_start, pos);
            }
            app.buffer.insert(word_start, &text);
            app.selection = Selection::point(word_start + text.chars().count());
            exec::recompute_highlights(app);
            exec::lsp_did_change(app);
        }
        PopupTarget::Dismiss => {}
    }
}

/// Snapshot the focused cell's rope at the start of each Edit session.
fn begin_notebook_edit(app: &mut App) {
    if let Some((ref nb, ref mut state)) = app.notebook {
        if !state.insert_session_active {
            let snapshot = nb.cells[state.focused_cell].source.clone();
            state.undo_stack.push((state.focused_cell, snapshot));
            state.redo_stack.clear();
            state.insert_session_active = true;
        }
    }
}

