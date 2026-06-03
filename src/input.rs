use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    app::App,
    command::Command,
    exec,
    keymap::KeyBinding,
    lsp_manager::LspLocation,
    mode::{FindDir, Mode},
    motion,
    popup::{PopupAction, PopupContent, PopupTarget},
    selection::Selection,
};

/// Dispatch a key event to the appropriate handler based on the current mode.
pub fn handle_key(app: &mut App, key: KeyEvent) {
    app.message = None;

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
                    // Keep the popup alive; let the key reach handle_insert.
                } else {
                    app.popup = None;
                }
                // fall through
            }
            PopupAction::ClosePassthrough => {
                // Always close (even for completion), then let the key fall through.
                app.popup = None;
                // fall through
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

    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.message = Some("use :q to quit, :q! to force quit".into());
        return;
    }

    // Ctrl+Enter: execute cell (notebook view) or close focused overlay.
    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
        if app.notebook_focused_edit() {
            exec::execute(app, &Command::NotebookCloseCellEdit);
        } else if app.notebook.is_some() {
            exec::execute(app, &Command::NotebookExecuteCell);
        }
        if app.notebook.is_some() {
            return;
        }
    }

    match app.mode.clone() {
        Mode::Normal => {
            // Esc in Normal mode while a notebook is open (not full-screen overlay)
            // toggles back to Notebook navigation — check before the keymap so the
            // Esc→EnterNormal binding doesn't shadow it.
            if key.code == KeyCode::Esc
                && app.notebook.is_some()
                && !app.notebook_focused_edit()
            {
                app.mode = Mode::Notebook;
            } else {
                let kb = KeyBinding::from(key);
                if let Some(cmds) = app.keymap.lookup_normal(&kb).map(|v| v.to_vec()) {
                    exec::run_many(app, &cmds);
                }
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
        Mode::Goto { .. } => handle_goto(app, key),
        Mode::FindChar { dir, till } => handle_find_char(app, key, dir, till),
        Mode::Search { forward } => handle_search(app, key, forward),
        Mode::Notebook => handle_notebook_mode(app, key),
        Mode::Jump { .. } => handle_jump(app, key),
        Mode::Fold => handle_fold(app, key),
    }

    // Sync completion popup filter after insertions.
    if had_completion_popup && app.mode == Mode::Insert {
        sync_completion_filter(app);
    }

    // Keep the focused notebook cell's stored source in sync with app.buffer.
    if app.notebook.is_some() && !app.notebook_focused_edit() {
        sync_buffer_to_notebook(app);
        // Insert mode already calls lsp_did_change on every typed character;
        // avoid the duplicate rope.to_string() + LSP write.
        if !matches!(app.mode, Mode::Insert) {
            exec::lsp_did_change(app);
        }
    }
}

// ---------------------------------------------------------------------------
// Insert mode
// ---------------------------------------------------------------------------

fn handle_insert(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            // Always clear any popup (e.g. completion) when leaving Insert mode.
            // The popup logic keeps it alive through DismissPassthrough so normal
            // typing still works, but an explicit Esc should always dismiss it.
            app.popup = None;
            exec::execute(app, &Command::EnterNormal);
        }
        KeyCode::Backspace => {
            let pos = app.selection.head;
            if pos > 0 {
                begin_insert_edit(app);
                app.buffer.remove_raw(pos - 1, pos);
                app.selection = Selection::point(pos - 1);
                exec::recompute_highlights(app);
                exec::lsp_did_change(app);
                // Shorter prefix may now match items, so allow popups again.
                app.completion_suppressed_prefix = None;
            }
        }
        KeyCode::Delete => {
            let pos = app.selection.head;
            let len = app.buffer.rope.len_chars();
            if pos < len {
                begin_insert_edit(app);
                app.buffer.remove_raw(pos, pos + 1);
                app.selection = Selection::point(
                    pos.min(app.buffer.rope.len_chars().saturating_sub(1)),
                );
                exec::recompute_highlights(app);
                exec::lsp_did_change(app);
            }
        }
        KeyCode::Enter => {
            begin_insert_edit(app);
            let pos = app.selection.head;
            let unit = crate::indent::unit(app.config.editor.expand_tabs, app.config.editor.tab_width);
            if crate::indent::is_bracket_pair(&app.buffer.rope, pos) {
                // {|} → {\n    |\n} : expand bracket pair onto three lines.
                let inner = crate::indent::for_new_line(&app.buffer.rope, pos, &unit);
                let base = crate::indent::for_line_above(&app.buffer.rope, pos);
                let inner_len = inner.chars().count();
                let to_insert = format!("\n{inner}\n{base}");
                app.buffer.insert_raw(pos, &to_insert);
                app.selection = Selection::point(pos + 1 + inner_len);
            } else {
                let ind = crate::indent::for_new_line(&app.buffer.rope, pos, &unit);
                let ind_len = ind.chars().count();
                app.buffer.insert_raw(pos, &format!("\n{ind}"));
                app.selection = Selection::point(pos + 1 + ind_len);
            }
            exec::recompute_highlights(app);
            exec::lsp_did_change(app);
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
            let unit = crate::indent::unit(app.config.editor.expand_tabs, app.config.editor.tab_width);
            app.buffer.insert_raw(pos, &unit);
            app.selection = Selection::point(pos + unit.chars().count());
            exec::recompute_highlights(app);
            exec::lsp_did_change(app);
        }
        KeyCode::Null => {
            exec::execute(app, &Command::LspRequestCompletion);
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if c == ' ' {
                    exec::execute(app, &Command::LspRequestCompletion);
                } else if c == 'k' {
                    // Kill to end of line (Emacs-style), staying in Insert mode.
                    let pos = app.selection.head;
                    let rope = &app.buffer.rope;
                    if rope.len_chars() > 0 {
                        let eol = motion::move_line_end(rope, Selection::point(pos), false).head;
                        if pos <= eol {
                            begin_insert_edit(app);
                            let del_end = (eol + 1).min(app.buffer.rope.len_chars());
                            let text = app.buffer.rope.slice(pos..del_end).to_string();
                            app.clipboard = text.clone();
                            crate::clipboard::write(&text);
                            app.buffer.remove_raw(pos, del_end);
                            app.selection = Selection::point(pos);
                            exec::recompute_highlights(app);
                            exec::lsp_did_change(app);
                        }
                    }
                    exec::update_scroll(app);
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
            if c == '.' || c == ':' {
                // Trigger characters always fire a fresh completion request.
                app.completion_suppressed_prefix = None;
                exec::execute(app, &Command::LspRequestCompletion);
            } else if c.is_alphanumeric() || c == '_' {
                let has_popup = app.popup.as_ref()
                    .map(|p| p.on_confirm == crate::popup::PopupTarget::InsertText)
                    .unwrap_or(false);
                if !has_popup {
                    // Check whether the current prefix extends one that already
                    // returned no matches — if so, more typing won't help.
                    let suppressed = app.completion_suppressed_prefix.as_ref()
                        .map(|sup| {
                            let cur = crate::motion::word_prefix_at(
                                &app.buffer.rope, app.selection.head,
                            );
                            cur.starts_with(sup.as_str())
                        })
                        .unwrap_or(false);
                    if !suppressed {
                        exec::execute(app, &Command::LspRequestCompletion);
                    }
                }
            } else {
                // Non-identifier char (space, punctuation, etc.) resets suppression
                // so the next word starts fresh.
                app.completion_suppressed_prefix = None;
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
                crate::history::record(app, cmd.name());
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
    // Capture whether we entered goto mode from Select (so motions extend).
    let extend = matches!(app.mode, Mode::Goto { extend: true });
    // Transition back to the appropriate mode before dispatching.
    app.mode = if extend { Mode::Select } else { Mode::Normal };

    match key.code {
        KeyCode::Char('g') => {
            app.selection = motion::goto_file_start(&app.buffer.rope, app.selection, extend);
        }
        KeyCode::Char('e') => {
            app.selection = motion::goto_file_end(&app.buffer.rope, app.selection, extend);
        }
        KeyCode::Char('h') => {
            app.selection =
                motion::move_line_first_non_ws(&app.buffer.rope, app.selection, extend);
        }
        KeyCode::Char('l') => {
            app.selection = motion::move_line_end(&app.buffer.rope, app.selection, extend);
        }
        KeyCode::Char('z') => exec::execute(app, &Command::ScrollCursorCenter),
        KeyCode::Char('d') => exec::execute(app, &Command::LspGotoDefinition),
        KeyCode::Char('r') => exec::execute(app, &Command::LspGotoReferences),
        KeyCode::Char('y') => exec::execute(app, &Command::LspGotoTypeDefinition),
        KeyCode::Char('i') => exec::execute(app, &Command::LspGotoImplementation),
        KeyCode::Char('w') => exec::execute(app, &Command::EnterJumpMode),
        KeyCode::Char('b') => exec::execute(app, &Command::OpenBufferPicker),
        KeyCode::Char('s') => exec::execute(app, &Command::OpenSymbolPicker),
        KeyCode::Char('D') => exec::execute(app, &Command::OpenDiagnosticPicker),
        KeyCode::Char('a') => exec::execute(app, &Command::LspCodeActions),
        KeyCode::Char('c') => exec::execute(app, &Command::CommentRegion),
        KeyCode::Char('k') => exec::execute(app, &Command::LspShowDocumentation),
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
            app.search.query.pop();
            exec::search_compute_matches(app);
        }
        KeyCode::Enter => {
            app.mode = Mode::Normal;
            exec::search_compute_matches(app);
            exec::search_jump(app, !forward);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.search.query.push(c);
            exec::search_compute_matches(app);
            if !app.search.matches.is_empty() {
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

fn sync_completion_filter(app: &mut App) {
    let pos = app.selection.head;
    let prefix = crate::motion::word_prefix_at(&app.buffer.rope, pos);

    let dismiss = {
        let mut should_dismiss = false;
        if let Some(ref mut popup) = app.popup {
            if popup.on_confirm == PopupTarget::InsertText {
                if let PopupContent::List(ref mut list) = popup.content {
                    list.filter = prefix.clone();
                    list.selected = 0;
                    // Dismiss when there's nothing left to complete, or when
                    // the current prefix matches no items.
                    should_dismiss = prefix.is_empty() || list.filtered_indices().is_empty();
                }
            }
        }
        should_dismiss
    };

    if dismiss {
        if !prefix.is_empty() {
            // Only suppress future requests when a non-empty prefix returned no
            // matches — an empty prefix means the user backspaced completely and
            // should get fresh completions when they start typing again.
            app.completion_suppressed_prefix = Some(prefix);
        }
        app.popup = None;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn begin_insert_edit(app: &mut App) {
    if !app.insert_session_active {
        app.buffer.begin_edit_session();
        app.insert_session_active = true;
    }
}

// ---------------------------------------------------------------------------
// Notebook mode
// ---------------------------------------------------------------------------

fn handle_notebook_mode(app: &mut App, key: KeyEvent) {
    let kb = KeyBinding::from(key);
    if let Some(cmds) = app.keymap.lookup_notebook(&kb).map(|v| v.to_vec()) {
        let was_notebook = app.mode == Mode::Notebook;
        exec::run_many(app, &cmds);
        // When the binding transitions to Insert mode, unfold and ensure LSP has current content.
        if was_notebook && app.mode == Mode::Insert {
            if let Some((_, ref mut state)) = app.notebook {
                state.folded_cells.remove(&state.focused_cell);
            }
            exec::lsp_did_change(app);
        }
    }
}

fn sync_buffer_to_notebook(app: &mut App) {
    if let Some((ref mut nb, ref state)) = app.notebook {
        let idx = state.focused_cell;
        if idx < nb.cells.len() {
            nb.cells[idx].source = app.buffer.rope.clone();
            if app.buffer.modified {
                nb.modified = true;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fold mode (after 'z')
// ---------------------------------------------------------------------------

fn handle_fold(app: &mut App, key: KeyEvent) {
    app.mode = Mode::Normal;
    app.popup = None;
    match key.code {
        KeyCode::Char('a') => exec::execute(app, &Command::FoldToggle),
        KeyCode::Char('A') => exec::execute(app, &Command::FoldToggleAll),
        KeyCode::Esc => {}
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Jump mode
// ---------------------------------------------------------------------------

fn handle_jump(app: &mut App, key: KeyEvent) {
    let extend = matches!(app.mode, Mode::Jump { extend: true });
    match key.code {
        KeyCode::Esc => {
            app.mode = if extend { Mode::Select } else { Mode::Normal };
            app.jump_labels.clear();
            app.jump_typed.clear();
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.jump_typed.push(c);
            let typed = app.jump_typed.clone();

            // Exact match → jump, extending selection if we came from Select mode.
            if let Some(&(pos, _)) = app.jump_labels.iter().find(|(_, l)| *l == typed) {
                if extend {
                    app.selection = Selection::new(app.selection.anchor, pos);
                    app.mode = Mode::Select;
                } else {
                    app.selection = Selection::point(pos);
                    app.mode = Mode::Normal;
                }
                app.jump_labels.clear();
                app.jump_typed.clear();
                exec::update_scroll(app);
                return;
            }

            // No label starts with the typed prefix → cancel.
            if !app.jump_labels.iter().any(|(_, l)| l.starts_with(typed.as_str())) {
                app.mode = if extend { Mode::Select } else { Mode::Normal };
                app.jump_labels.clear();
                app.jump_typed.clear();
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
                crate::history::record(app, cmd.name());
                exec::execute(app, &cmd);
            } else {
                app.message = Some(format!("Unknown command: {text}"));
            }
        }
        PopupTarget::InsertText => {
            let pos = app.selection.head;
            let word_start = crate::motion::word_start_at(&app.buffer.rope, pos);
            if word_start < pos {
                app.buffer.remove(word_start, pos);
            }
            app.buffer.insert(word_start, &text);
            app.selection = Selection::point(word_start + text.chars().count());
            exec::recompute_highlights(app);
            exec::lsp_did_change(app);
        }
        PopupTarget::Dismiss => {}
        PopupTarget::ApplyCodeAction => {
            if let Ok(idx) = text.parse::<usize>() {
                exec::apply_code_action(app, idx);
            }
        }
        PopupTarget::RestoreRecovery => {
            crate::recovery::handle_choice(app, &text);
        }
        PopupTarget::Navigate => {
            let parts: Vec<&str> = text.splitn(3, '\0').collect();
            if parts.len() == 3 {
                let path = std::path::PathBuf::from(parts[0]);
                let line: usize = parts[1].parse().unwrap_or(0);
                let character: usize = parts[2].parse().unwrap_or(0);
                if exec::is_special_path(&path) {
                    exec::switch_to_special_buffer(app, path.to_str().unwrap_or("*scratch*"));
                } else if path.extension().and_then(|e| e.to_str()) == Some("ipynb") {
                    exec::open_as_notebook(app, &path);
                } else {
                    exec::jump_to_location(app, &LspLocation { path, line, character });
                }
            }
        }
    }
}
