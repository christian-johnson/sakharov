use serde_json::Value;

use crate::{
    app::{language_for_path, App},
    buffer::Buffer,
    highlight::Highlighter,
    lsp::{lsp_pos_to_char, path_to_uri},
    lsp_manager::{LspEvent, LspLocation, LspRequestKind},
    mode::Mode,
    selection::Selection,
};

/// Send a `textDocument/codeAction` request for the current selection range.
pub(super) fn lsp_code_actions_request(app: &mut App) {
    let lang = match app.current_language() {
        Some(l) => l.to_owned(),
        None => {
            app.message = Some("No language server configured for this file".into());
            return;
        }
    };
    let path = match app.buffer.path.clone() {
        Some(p) => p,
        None => {
            app.message = Some("Save the file before using LSP features".into());
            return;
        }
    };
    let sel = app.selection;
    let start_char = sel.start();
    let end_char = sel.end();
    let rope = app.buffer.rope.clone();

    if !app.lsp.request_code_actions(&lang, &path, &rope, start_char, end_char) {
        app.message = Some("LSP server initializing — try again in a moment".into());
    }
}

pub(super) fn lsp_request(app: &mut App, kind: LspRequestKind) {
    let lang = match app.current_language() {
        Some(l) => l.to_owned(),
        None => {
            app.message = Some("No language server configured for this file".into());
            return;
        }
    };
    let path = match app.buffer.path.clone() {
        Some(p) => p,
        None => {
            app.message = Some("Save the file before using LSP features".into());
            return;
        }
    };
    let char_idx = app.selection.head;
    let rope = app.buffer.rope.clone();

    if !app.lsp.request(kind, &lang, &path, &rope, char_idx) {
        app.message = Some("LSP server initializing — try again in a moment".into());
    }
}

/// Notify the LSP server of a buffer change (called after each Insert-mode edit).
pub fn lsp_did_change(app: &mut App) {
    let lang = match app.current_language() {
        Some(l) => l.to_owned(),
        None => return,
    };
    let path = match app.buffer.path.clone() {
        Some(p) => p,
        None => return,
    };
    let text = app.buffer.rope.to_string();

    if let Some(ref session) = app.notebook_cell_edit {
        if app.lsp.notebook_sync_supported(&lang) {
            let notebook_uri = path_to_uri(&session.notebook_path);
            let cell_uri = path_to_uri(&path);
            app.lsp.notebook_did_change_cell(&lang, &notebook_uri, &cell_uri, &text);
            return;
        }
    }

    app.lsp.did_change(&lang, &path, &text);
}

/// Drain LSP events and apply them to the editor state.
pub fn process_lsp_events(app: &mut App) {
    let events = app.lsp.poll();
    for event in events {
        handle_lsp_event(app, event);
    }
}

fn handle_lsp_event(app: &mut App, event: LspEvent) {
    match event {
        LspEvent::Initialized { language } => {
            let notebook_lang = app.notebook.as_ref()
                .map(|(nb, _)| nb.metadata.kernel_language.clone());

            if notebook_lang.as_deref() == Some(&language) && !app.notebook_focused_edit() {
                if app.lsp.notebook_sync_supported(&language) {
                    super::notebook::notebook_lsp_open(app);
                }
                if !app.lsp.notebook_sync_supported(&language) {
                    if let Some(path) = app.buffer.path.clone() {
                        let text = app.buffer.rope.to_string();
                        app.lsp.did_open(&language, &path, &text);
                    }
                }
                return;
            }

            if app.current_language() == Some(&language) {
                if let Some(path) = app.buffer.path.clone() {
                    let text = app.buffer.rope.to_string();
                    app.lsp.did_open(&language, &path, &text);
                }
            }
        }
        LspEvent::Diagnostics { path: _, ref items } => {
            let _ = items;
            super::rebuild_diag_cache(app);
        }
        LspEvent::CompletionResult { items } => {
            if app.mode == Mode::Insert && !items.is_empty() {
                let popup_items: Vec<crate::popup::ListItem> = items
                    .iter()
                    .map(|item| crate::popup::ListItem {
                        label: item.insert_text.clone().unwrap_or_else(|| item.label.clone()),
                        detail: item.detail.clone(),
                        kind: item.kind.clone(),
                        payload: None,
                    })
                    .collect();
                // Seed filter with the word prefix before the cursor so amber
                // highlighting is visible from the moment the popup opens.
                let prefix = word_prefix_at_cursor(app);
                let mut popup = crate::popup::Popup::completion(popup_items);
                if let crate::popup::PopupContent::List(ref mut list) = popup.content {
                    list.filter = prefix;
                }
                app.popup = Some(popup);
            }
        }
        LspEvent::HoverResult { content } => {
            app.popup = Some(crate::popup::Popup::documentation("hover", &content));
        }
        LspEvent::DefinitionResult { location } => {
            if let Some(loc) = location {
                jump_to_location(app, &loc);
            } else {
                app.message = Some("No definition found".into());
            }
        }
        LspEvent::ReferencesResult { locations } => {
            if locations.is_empty() {
                app.message = Some("No references found".into());
            } else if locations.len() == 1 {
                jump_to_location(app, &locations[0]);
            } else {
                // Jump to first; full location-list popup is Phase 4.
                jump_to_location(app, &locations[0]);
            }
        }
        LspEvent::CodeActionsResult { actions } => {
            if actions.is_empty() {
                app.message = Some("No code actions available".into());
                return;
            }
            let items: Vec<crate::popup::ListItem> = actions
                .iter()
                .enumerate()
                .map(|(i, action)| {
                    let title = action
                        .get("title")
                        .and_then(|t| t.as_str())
                        .unwrap_or("(unnamed)")
                        .to_owned();
                    let kind = action
                        .get("kind")
                        .and_then(|k| k.as_str())
                        .map(str::to_owned);
                    crate::popup::ListItem {
                        label: title,
                        detail: kind,
                        kind: None,
                        payload: Some(i.to_string()),
                    }
                })
                .collect();
            app.pending_code_actions = actions;
            app.popup = Some(crate::popup::Popup::code_actions(items));
        }
    }
}

pub fn jump_to_location(app: &mut App, loc: &LspLocation) {
    let target = loc.path.canonicalize().ok().unwrap_or_else(|| loc.path.clone());
    let same_file = if app.buffer.path.is_none() && loc.path.as_os_str().is_empty() {
        true
    } else {
        let current = app.buffer.path.as_ref().and_then(|p| p.canonicalize().ok());
        current.map(|c| c == target).unwrap_or(false)
    };

    if same_file {
        let char_idx = lsp_pos_to_char(&app.buffer.rope, loc.line, loc.character);
        app.selection = Selection::point(char_idx);
        super::update_scroll(app);
    } else {
        open_file_at(app, &target, loc.line, loc.character);
    }
}

/// Load `path` into the editor buffer and place the cursor at (line, character).
pub fn open_file_at(app: &mut App, path: &std::path::Path, line: usize, character: usize) {
    if let (Some(ref lang), Some(ref old_path)) = (
        app.lsp_language.clone(),
        app.buffer.path.clone(),
    ) {
        app.lsp.did_close(lang, old_path);
    }

    let new_buffer = match path.to_str() {
        Some(s) => Buffer::from_path(s).unwrap_or_else(|_| {
            let mut b = Buffer::new_empty();
            b.path = Some(path.to_path_buf());
            b
        }),
        None => {
            app.message = Some(format!("Cannot open: {}", path.display()));
            return;
        }
    };

    app.buffer = new_buffer;
    app.selection = Selection::point(0);
    app.scroll_row = 0;
    app.scroll_col = 0;
    app.insert_session_active = false;

    let new_lang = language_for_path(Some(path)).map(str::to_owned);
    app.lsp_language = new_lang.clone();
    app.highlighter = Highlighter::new(Some(path));
    super::recompute_highlights(app);

    let file_dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(ref lang) = new_lang {
        if let Some(server_config) = app.config.language_servers.get(lang).cloned() {
            app.lsp.ensure_server(lang, &server_config, file_dir);
        }
        if app.lsp.is_ready(lang) {
            let text = app.buffer.rope.to_string();
            app.lsp.did_open(lang, path, &text);
        }
    }

    let char_idx = lsp_pos_to_char(&app.buffer.rope, line, character);
    app.selection = Selection::point(char_idx);
    super::update_scroll(app);

    // Canonicalize before dedup so relative vs absolute comparisons work correctly.
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !app.open_buffers.iter().any(|stored| {
        stored.canonicalize().unwrap_or_else(|_| stored.clone()) == canon
    }) {
        app.open_buffers.push(canon);
    }

    app.git_diff = crate::git::diff_marks(path);
    super::rebuild_diag_cache(app);

    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
    app.message = Some(format!("Opened {} (line {})", name, line + 1));
}

/// Apply a code action selected from the popup.
/// `idx` is the index into `app.pending_code_actions`.
pub fn apply_code_action(app: &mut App, idx: usize) {
    let action = match app.pending_code_actions.get(idx).cloned() {
        Some(a) => a,
        None => return,
    };

    if let Some(edit) = action.get("edit").cloned() {
        apply_workspace_edit(app, edit);
    }

    if let Some(command_obj) = action.get("command") {
        let lang = match app.current_language() {
            Some(l) => l.to_owned(),
            None => return,
        };
        let cmd_id = command_obj
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_owned();
        let args = command_obj
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Null);
        if !cmd_id.is_empty() {
            app.lsp.execute_command(&lang, &cmd_id, args);
        }
    }
}

/// Apply a `WorkspaceEdit` to the current buffer (same-file edits only).
fn apply_workspace_edit(app: &mut App, edit: Value) {
    let current_uri = match app.buffer.path.as_deref().map(path_to_uri) {
        Some(u) => u,
        None => return,
    };

    // Collect TextEdits for the current file from either `changes` or `documentChanges`.
    let raw_edits: Vec<Value> = if let Some(changes) = edit.get("changes").and_then(|c| c.as_object()) {
        changes
            .get(&current_uri)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    } else if let Some(doc_changes) = edit.get("documentChanges").and_then(|dc| dc.as_array()) {
        doc_changes
            .iter()
            .filter(|item| {
                item.get("textDocument")
                    .and_then(|td| td.get("uri"))
                    .and_then(|u| u.as_str())
                    .map(|u| u == current_uri)
                    .unwrap_or(false)
            })
            .flat_map(|item| {
                item.get("edits")
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    } else {
        return;
    };

    if raw_edits.is_empty() {
        return;
    }

    // Parse edits and sort in reverse order so later edits don't shift earlier positions.
    let mut text_edits: Vec<(usize, usize, String)> = raw_edits
        .iter()
        .filter_map(|e| {
            let range = e.get("range")?;
            let start = range.get("start")?;
            let end = range.get("end")?;
            let sl = start.get("line")?.as_u64()? as usize;
            let sc = start.get("character")?.as_u64()? as usize;
            let el = end.get("line")?.as_u64()? as usize;
            let ec = end.get("character")?.as_u64()? as usize;
            let new_text = e.get("newText")?.as_str()?.to_owned();
            let start_idx = lsp_pos_to_char(&app.buffer.rope, sl, sc);
            let end_idx = lsp_pos_to_char(&app.buffer.rope, el, ec);
            Some((start_idx, end_idx, new_text))
        })
        .collect();
    text_edits.sort_by(|a, b| b.0.cmp(&a.0));

    app.buffer.begin_edit_session();
    for (start, end, new_text) in text_edits {
        if start < end {
            app.buffer.remove_raw(start, end);
        }
        if !new_text.is_empty() {
            app.buffer.insert_raw(start, &new_text);
        }
    }
    super::recompute_highlights(app);
}

/// Extract the alphanumeric/underscore word ending at the cursor — used to
/// pre-seed the completion popup filter so highlights are visible immediately.
fn word_prefix_at_cursor(app: &crate::app::App) -> String {
    let pos = app.selection.head;
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
}
