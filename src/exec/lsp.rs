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
            app.messages.show("No language server configured for this file");
            return;
        }
    };
    let path = match app.buffer.path.clone() {
        Some(p) => p,
        None => {
            app.messages.show("Save the file before using LSP features");
            return;
        }
    };
    let sel = app.selection;
    let start_char = sel.start();
    let end_char = sel.end();
    let rope = app.buffer.rope.clone();

    if !app.lsp.request_code_actions(&lang, &path, &rope, start_char, end_char) {
        app.messages.show("LSP server initializing — try again in a moment");
    }
}

/// Route a hover / signature-help / references request through the notebook's
/// shadow concatenated document so it sees cross-cell context (pylsp only
/// concatenates cells itself for completion and definition). Returns false when
/// not applicable (no notebook, markdown cell, server not ready) — the caller
/// falls back to the ordinary per-cell request.
fn notebook_shadow_request(app: &mut App, kind: LspRequestKind) -> bool {
    let Some((nb, state)) = app.notebook.as_ref() else { return false };
    let focused = state.focused_cell;
    if nb.cells.get(focused).map(|c| c.cell_type != crate::notebook::CellType::Code).unwrap_or(true) {
        return false;
    }
    let lang = nb.metadata.kernel_language.clone();
    if !app.lsp.is_ready(&lang) {
        return false;
    }
    let (text, map) = crate::notebook::concat_source(nb, Some((focused, &app.buffer.rope)));
    let Some(&(_, start_line)) = map.iter().find(|&&(idx, _)| idx == focused) else {
        return false;
    };
    let shadow = crate::notebook::concat_virtual_path(&nb.path, &lang);
    let (line, character) = crate::lsp::char_to_lsp_pos(&app.buffer.rope, app.selection.head);
    app.lsp.request_via_shadow_doc(
        kind,
        &lang,
        &shadow,
        &text,
        line + start_line as u32,
        character,
    )
}

pub(super) fn lsp_request(app: &mut App, kind: LspRequestKind) {
    // For completion requests, fall back to buffer symbols when LSP isn't available.
    if matches!(kind, LspRequestKind::Completion) {
        let lang = app.current_language().unwrap_or("").to_owned();
        let lsp_ready = !lang.is_empty() && app.lsp.is_ready(&lang);

        if lsp_ready {
            let path = match app.buffer.path.clone() {
                Some(p) => p,
                None => {
                    show_buffer_completions(app);
                    return;
                }
            };
            let char_idx = app.selection.head;
            let rope = app.buffer.rope.clone();
            if !app.lsp.request(kind, &lang, &path, &rope, char_idx) {
                show_buffer_completions(app);
            }
        } else {
            show_buffer_completions(app);
        }
        return;
    }

    // Notebook hover/references: go through the shadow concatenated document
    // for cross-cell context. (Definition stays on the cell URI — pylsp handles
    // that one notebook-aware natively, and answers with cell URIs we map back.)
    if matches!(kind, LspRequestKind::Hover | LspRequestKind::References)
        && notebook_shadow_request(app, kind)
    {
        return;
    }

    let lang = match app.current_language() {
        Some(l) => l.to_owned(),
        None => {
            app.messages.show("No language server configured for this file");
            return;
        }
    };
    let path = match app.buffer.path.clone() {
        Some(p) => p,
        None => {
            app.messages.show("Save the file before using LSP features");
            return;
        }
    };
    let char_idx = app.selection.head;
    let rope = app.buffer.rope.clone();

    if !app.lsp.request(kind, &lang, &path, &rope, char_idx) {
        app.messages.show("LSP server initializing — try again in a moment");
    }
}

/// Show a completion popup seeded with buffer symbols only (no LSP).
pub(super) fn show_buffer_completions(app: &mut App) {
    let lang = app.current_language().unwrap_or("").to_owned();
    let symbols = crate::symbols::extract_symbols(&app.buffer.rope, &lang);
    if symbols.is_empty() {
        return;
    }
    let prefix = word_prefix_at_cursor(app);
    let items: Vec<crate::popup::ListItem> = symbols
        .iter()
        .map(|s| crate::popup::ListItem {
            label: s.name.clone(),
            detail: None,
            kind: Some(symbol_kind_badge(s.kind, &app.config.ui.symbol_icons)),
            payload: None,
            ..Default::default()
        })
        .collect();
    open_completion_popup(app, items, prefix);
}

/// Minimum interval between signature-help requests (configurable via
/// `editor.lsp_signature_throttle_ms`, 0 = no throttle). Inside a call the
/// hint refreshes on every keystroke; for a notebook each refresh rebuilds and
/// retransmits the whole concatenated notebook, so cap the rate and let
/// `pump_signature_help` fire one trailing refresh for the final cursor state.
fn sig_help_throttle(app: &App) -> std::time::Duration {
    std::time::Duration::from_millis(app.config.editor.lsp_signature_throttle_ms)
}

/// Trailing edge of the signature-help throttle, called once per frame from
/// the run loop: fires a deferred refresh once the throttle window elapses,
/// so the active-parameter marker always settles on the latest keystroke.
pub fn pump_signature_help(app: &mut App) {
    if !app.sig_help_deferred {
        return;
    }
    if app.mode != Mode::Insert {
        app.sig_help_deferred = false;
        return;
    }
    let window_elapsed = app
        .sig_help_last
        .map(|t| t.elapsed() >= sig_help_throttle(app))
        .unwrap_or(true);
    if window_elapsed {
        // lsp_signature_help clears the deferred flag and re-arms the throttle.
        lsp_signature_help(app);
    }
}

/// Request `textDocument/signatureHelp` at the cursor so the active call's
/// argument list can be shown in the minibuffer. No-op (and clears any stale
/// signature) when no language server is ready for the current document.
/// Rate-limited to one request per throttle window; calls inside the window
/// are deferred to `pump_signature_help` (trailing edge).
pub fn lsp_signature_help(app: &mut App) {
    let throttle = sig_help_throttle(app);
    if !throttle.is_zero()
        && app.sig_help_last.is_some_and(|t| t.elapsed() < throttle)
    {
        app.sig_help_deferred = true;
        return;
    }
    app.sig_help_last = Some(std::time::Instant::now());
    app.sig_help_deferred = false;

    let lang = app.current_language().unwrap_or("").to_owned();
    if lang.is_empty() || !app.lsp.is_ready(&lang) {
        app.signature_help = None;
        return;
    }
    // Notebook: route through the shadow concatenated document so calls to
    // functions defined/imported in other cells resolve.
    if notebook_shadow_request(app, LspRequestKind::SignatureHelp) {
        return;
    }
    let path = match app.buffer.path.clone() {
        Some(p) => p,
        None => return,
    };
    let char_idx = app.selection.head;
    let rope = app.buffer.rope.clone();
    app.lsp.request(LspRequestKind::SignatureHelp, &lang, &path, &rope, char_idx);
}

/// `lsp_did_change` for a single insertion of `inserted` at char index `at`
/// (the Insert-mode hot path): sends a range delta to incremental-sync servers
/// instead of stringifying and retransmitting the whole document on every
/// keystroke. Notebook cells keep the existing full-cell sync — cells are
/// small and the notebook protocol replaces cell text wholesale.
pub fn lsp_did_change_insert(app: &mut App, at: usize, inserted: &str) {
    if app.notebook.is_some() {
        return lsp_did_change(app);
    }
    // Deltas are only valid if the server's copy is exactly the pre-edit
    // document. Command edits (open-line, delete, paste, undo…) mutate the
    // buffer without notifying the LSP, so verify the length the server last
    // saw; on mismatch send the full text instead (which resyncs everything).
    let len = app.buffer.rope.len_chars();
    let expected_old = len.saturating_sub(inserted.chars().count());
    if app.buffer.lsp_synced_chars != Some(expected_old) {
        return lsp_did_change(app);
    }
    let Some(lang) = app.current_language().map(str::to_owned) else { return };
    let Some(path) = app.buffer.path.clone() else { return };
    app.buffer.lsp_synced_chars = Some(len);
    // The text before `at` is untouched by the edit, so the position of `at`
    // is identical in the pre- and post-edit document — safe to compute
    // against the post-edit rope.
    let start = crate::lsp::char_to_lsp_pos_utf16(&app.buffer.rope, at);
    app.lsp.did_change_delta(&lang, &path, start, start, inserted, &app.buffer.rope);
}

/// `lsp_did_change` for a single removal: `removed` was deleted at char index
/// `at`. See `lsp_did_change_insert` for the delta rationale and the stale-sync
/// guard.
pub fn lsp_did_change_remove(app: &mut App, at: usize, removed: &str) {
    if app.notebook.is_some() {
        return lsp_did_change(app);
    }
    let len = app.buffer.rope.len_chars();
    let expected_old = len + removed.chars().count();
    if app.buffer.lsp_synced_chars != Some(expected_old) {
        return lsp_did_change(app);
    }
    let Some(lang) = app.current_language().map(str::to_owned) else { return };
    let Some(path) = app.buffer.path.clone() else { return };
    app.buffer.lsp_synced_chars = Some(len);
    let start = crate::lsp::char_to_lsp_pos_utf16(&app.buffer.rope, at);
    let end = crate::lsp::advance_lsp_pos_utf16(start, removed);
    app.lsp.did_change_delta(&lang, &path, start, end, "", &app.buffer.rope);
}

/// Notify the LSP server of a buffer change (full-text sync). Used by
/// command-driven edits and as the fallback whenever a range delta can't be
/// trusted; Insert-mode keystrokes go through `lsp_did_change_insert`/`_remove`.
pub fn lsp_did_change(app: &mut App) {
    let lang = match app.current_language() {
        Some(l) => l.to_owned(),
        None => return,
    };
    let path = match app.buffer.path.clone() {
        Some(p) => p,
        None => return,
    };
    // A full sync makes the server's copy exactly the current rope — re-arm
    // the incremental path.
    app.buffer.lsp_synced_chars = Some(app.buffer.rope.len_chars());
    let text = app.buffer.rope.to_string();

    if let Some((nb, _)) = app.notebook.as_ref() {
        // The manager routes per server: notebookDocument/didChange to servers
        // with the notebook open, plain didChange on the cell doc to the rest.
        let notebook_uri = path_to_uri(&nb.path);
        let cell_uri = path_to_uri(&path);
        app.lsp.notebook_did_change_cell(&lang, &notebook_uri, &cell_uri, &text);
        return;
    }

    app.lsp.did_change(&lang, &path, &text);
}

/// Fire a one-shot warm-up completion for the freshly-opened plain-file buffer.
/// pylsp/jedi parses a file's imports lazily on the first request, so the cold
/// cache otherwise stalls the user's *first* completion/hover by seconds; this
/// pulls that cost forward to open time. `LspManager::prewarm` no-ops after the
/// first call per document and only fires when a completion server has the doc
/// open. Skipped for notebooks (their per-cell / shadow-doc sync warms via use).
fn prewarm_current(app: &mut App, language: &str) {
    if app.notebook.is_some() {
        return;
    }
    if let Some(path) = app.buffer.path.clone() {
        app.lsp
            .prewarm(language, &path, &app.buffer.rope, app.selection.head);
    }
}

/// Drain LSP events and apply them to the editor state.
/// Returns true when any event was applied (the caller should redraw).
pub fn process_lsp_events(app: &mut App) -> bool {
    let events = app.lsp.poll();
    let log = app.lsp.take_lifecycle_log();
    let any = !events.is_empty() || !log.is_empty();
    for msg in log {
        app.messages.show(msg);
    }
    for event in events {
        handle_lsp_event(app, event);
    }
    any
}

fn handle_lsp_event(app: &mut App, event: LspEvent) {
    match event {
        LspEvent::Initialized { language } => {
            let notebook_lang = app.notebook.as_ref()
                .map(|(nb, _)| nb.metadata.kernel_language.clone());

            if notebook_lang.as_deref() == Some(&language) && !app.notebook_focused_edit() {
                // Idempotent per server: only the newly-initialized server
                // actually receives the (re-)open.
                super::notebook::notebook_lsp_open(app);
                return;
            }

            if app.current_language() == Some(&language) {
                if let Some(path) = app.buffer.path.clone() {
                    let text = app.buffer.rope.to_string();
                    app.lsp.did_open(&language, &path, &text);
                }
                prewarm_current(app, &language);
            }
        }
        LspEvent::Diagnostics => {
            super::rebuild_diag_cache(app);
        }
        LspEvent::PrewarmComplete { .. } => {
            // Nothing to apply — the warm-up's only effect is the now-hot server
            // cache. The completion line is logged by `LspManager::poll`.
        }
        LspEvent::CompletionResult { items } => {
            if app.mode == Mode::Insert {
                // Convert LSP items.
                let mut popup_items: Vec<crate::popup::ListItem> = items
                    .iter()
                    .map(|item| crate::popup::ListItem {
                        label: item.insert_text.clone().unwrap_or_else(|| item.label.clone()),
                        detail: item.detail.clone(),
                        kind: item.kind.clone(),
                        payload: None,
                        documentation: item.documentation.clone(),
                        resolve_data: item.data.clone(),
                    })
                    .collect();

                // Merge buffer symbols that aren't already covered by LSP results.
                let lang = app.current_language().unwrap_or("").to_owned();
                let symbols = crate::symbols::extract_symbols(&app.buffer.rope, &lang);
                let lsp_labels: std::collections::HashSet<String> =
                    popup_items.iter().map(|i| i.label.clone()).collect();
                for sym in &symbols {
                    if !lsp_labels.contains(&sym.name) {
                        popup_items.push(crate::popup::ListItem {
                            label: sym.name.clone(),
                            detail: Some("buf".into()),
                            kind: Some(symbol_kind_badge(sym.kind, &app.config.ui.symbol_icons)),
                            payload: None,
                            ..Default::default()
                        });
                    }
                }

                if !popup_items.is_empty() {
                    let prefix = word_prefix_at_cursor(app);
                    open_completion_popup(app, popup_items, prefix);
                }
            }
        }
        LspEvent::CompletionResolved { documentation, detail } => {
            if let Some(idx) = app.completion.pending_resolve.take() {
                if let Some(popup) = app.popup.as_mut() {
                    if popup.on_confirm == crate::popup::PopupTarget::InsertText {
                        if let crate::popup::PopupContent::List(ref mut list) = popup.content {
                            if let Some(item) = list.items.get_mut(idx) {
                                // Record the result (Some even when empty) so this
                                // item is treated as resolved and never re-requested.
                                item.documentation =
                                    Some(documentation.unwrap_or_default());
                                if item.detail.is_none() {
                                    item.detail = detail;
                                }
                            }
                            // `detail` participates in match scoring.
                            list.invalidate_filter_cache();
                        }
                    }
                }
                // Refresh so the doc panel reflects the newly-resolved content
                // (and may kick off a resolve for whatever is now selected).
                refresh_completion_doc(app);
            }
        }
        LspEvent::HoverResult { content } => {
            if content.is_empty() {
                app.messages.show("No documentation available");
            } else {
                app.popup = Some(crate::popup::Popup::documentation("hover", &content));
            }
        }
        LspEvent::SignatureHelpResult { signature } => {
            // Only meaningful while typing in Insert mode; the minibuffer shows it.
            app.signature_help = if app.mode == Mode::Insert { signature } else { None };
        }
        LspEvent::DefinitionResult { location } => {
            if let Some(loc) = location {
                jump_to_location(app, &loc);
            } else {
                app.messages.show("No definition found");
            }
        }
        LspEvent::ReferencesResult { locations } => {
            if locations.is_empty() {
                app.messages.show("No references found");
            } else if locations.len() == 1 {
                jump_to_location(app, &locations[0]);
            } else {
                // Location-list popup: Enter jumps via the Navigate confirm path
                // (which understands notebook virtual-cell paths).
                let items = reference_items(app, &locations);
                app.popup = Some(crate::popup::Popup::navigate("references", items));
            }
        }
        LspEvent::FormattingResult { edits } => {
            if !edits.is_empty() {
                if let Some(ref path) = app.buffer.path.clone() {
                    let uri = path_to_uri(path);
                    let fake_edit = serde_json::json!({ "changes": { uri: edits } });
                    apply_workspace_edit(app, fake_edit);
                }
                // Sync the new buffer content back to the server so the next format
                // request sees the already-formatted text, not the old pre-format version.
                lsp_did_change(app);
            }
            if app.pending_format_save {
                app.pending_format_save = false;
                do_save(app);
            }
        }
        LspEvent::CodeActionsResult { actions } => {
            if actions.is_empty() {
                app.messages.show("No code actions available");
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
                        payload: Some(crate::popup::ConfirmPayload::CodeAction(i)),
                        ..Default::default()
                    }
                })
                .collect();
            app.pending_code_actions = actions;
            app.popup = Some(crate::popup::Popup::code_actions(items));
        }
    }
}

/// One trimmed source line from a rope (empty when out of range).
fn line_text(rope: &ropey::Rope, line: usize) -> String {
    if line < rope.len_lines() {
        rope.line(line).to_string().trim().to_owned()
    } else {
        String::new()
    }
}

/// Build navigate-popup items for a references result. Locations inside the
/// notebook's shadow concatenated document (or its virtual cell docs) are
/// rewritten to cell virtual paths with cell-relative lines, so confirming an
/// item jumps to the cell in-place through `jump_to_location`.
fn reference_items(app: &App, locations: &[LspLocation]) -> Vec<crate::popup::ListItem> {
    locations
        .iter()
        .map(|loc| {
            if let Some((nb, state)) = app.notebook.as_ref() {
                let lang = &nb.metadata.kernel_language;
                let shadow = crate::notebook::concat_virtual_path(&nb.path, lang);
                let cell_loc = if crate::lsp::diagnostic_key(&loc.path)
                    == crate::lsp::diagnostic_key(&shadow)
                {
                    let over = (state.focused_cell, &app.buffer.rope);
                    crate::notebook::cell_for_concat_line(nb, Some(over), loc.line)
                } else {
                    crate::notebook::cell_index_for_virtual_path(nb, &loc.path)
                        .map(|idx| (idx, loc.line))
                };
                if let Some((idx, line)) = cell_loc {
                    // The focused cell's live text is in the buffer, not nb.cells.
                    let text = if idx == state.focused_cell {
                        line_text(&app.buffer.rope, line)
                    } else {
                        nb.cells
                            .get(idx)
                            .map(|c| line_text(&c.source, line))
                            .unwrap_or_default()
                    };
                    return crate::popup::ListItem::navigate(
                        text,
                        format!("cell {}:{}", idx + 1, line + 1),
                        &crate::notebook::cell_virtual_path(&nb.path, lang, idx),
                        line,
                        loc.character,
                    );
                }
            }

            // Plain file: prefer the open buffer's text, fall back to disk.
            let file = loc
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| loc.path.to_string_lossy().into_owned());
            let in_buffer = app
                .buffer
                .path
                .as_deref()
                .map(|p| crate::lsp::diagnostic_key(p) == crate::lsp::diagnostic_key(&loc.path))
                .unwrap_or(false);
            let text = if in_buffer {
                line_text(&app.buffer.rope, loc.line)
            } else {
                std::fs::read_to_string(&loc.path)
                    .ok()
                    .and_then(|s| s.lines().nth(loc.line).map(|l| l.trim().to_owned()))
                    .unwrap_or_default()
            };
            let label = if text.is_empty() {
                format!("{file}:{}", loc.line + 1)
            } else {
                text
            };
            crate::popup::ListItem::navigate(
                label,
                format!("{file}:{}", loc.line + 1),
                &loc.path,
                loc.line,
                loc.character,
            )
        })
        .collect()
}

pub fn jump_to_location(app: &mut App, loc: &LspLocation) {
    // Notebook: locations inside the notebook's virtual documents refer to
    // cells, not on-disk files. A virtual cell path maps directly (positions
    // are cell-relative); a shadow concatenated-document path maps via the
    // concat line offsets. Jump to the cell in-place either way.
    let cell_target = app.notebook.as_ref().and_then(|(nb, state)| {
        if let Some(idx) = crate::notebook::cell_index_for_virtual_path(nb, &loc.path) {
            return Some((idx, loc.line));
        }
        let lang = &nb.metadata.kernel_language;
        let shadow = crate::notebook::concat_virtual_path(&nb.path, lang);
        if crate::lsp::diagnostic_key(&loc.path) == crate::lsp::diagnostic_key(&shadow) {
            let over = (state.focused_cell, &app.buffer.rope);
            return crate::notebook::cell_for_concat_line(nb, Some(over), loc.line);
        }
        None
    });
    if let Some((idx, line)) = cell_target {
        let current = app.notebook.as_ref().map(|(_, s)| s.focused_cell);
        if Some(idx) != current {
            super::switch_focused_cell(app, idx);
        }
        let char_idx = lsp_pos_to_char(&app.buffer.rope, line, loc.character);
        app.selection = Selection::point(char_idx);
        super::update_scroll(app);
        return;
    }

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
    // Redirect special buffer names to their own switch handler.
    if super::is_special_path(path) {
        super::save_current_special_buffer(app);
        super::switch_to_special_buffer(app, path.to_str().unwrap_or("*scratch*"));
        return;
    }

    // Redirect .ipynb files to the notebook loader.
    if path.extension().and_then(|e| e.to_str()) == Some("ipynb") {
        super::open_as_notebook(app, path);
        return;
    }

    let Some(path_str) = path.to_str() else {
        app.messages.show(format!("Cannot open: {}", path.display()));
        return;
    };

    // Save scratch content when leaving it.
    super::save_current_special_buffer(app);

    // If a notebook is open, tear it down cleanly before loading a plain file.
    let had_notebook = app.notebook.is_some();
    if had_notebook {
        // Preserve the .ipynb path in the buffer list before stashing.
        if let Some((ref nb, _)) = app.notebook {
            let nb_path = nb.path.clone();
            super::register_buffer(&mut app.open_buffers, &nb_path);
        }
        // Stash notebook state so edits survive if the user comes back.
        super::notebook::stash_current_notebook(app);
        app.mode = crate::mode::Mode::Normal;
    }

    if let (Some(ref lang), Some(ref old_path)) = (
        app.lsp_language.clone(),
        app.buffer.path.clone(),
    ) {
        app.lsp.did_close(lang, old_path);
    }

    // Keep the outgoing plain buffer's unsaved edits (and undo history) in
    // memory.  After a notebook teardown `app.buffer` holds stale cell text
    // under a virtual path — never stash that.
    if !had_notebook {
        super::stash_current_file_buffer(app);
    }

    // Restore the target from its stash when we've visited it before;
    // otherwise load from disk.
    let stashed = super::take_stashed_file_buffer(app, path);
    let from_stash = stashed.is_some();
    let new_buffer = stashed.unwrap_or_else(|| {
        Buffer::from_path(path_str).unwrap_or_else(|_| {
            let mut b = Buffer::new_empty();
            b.path = Some(path.to_path_buf());
            b
        })
    });

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
            prewarm_current(app, lang);
        }
    }

    let char_idx = lsp_pos_to_char(&app.buffer.rope, line, character);
    app.selection = Selection::point(char_idx);
    super::update_scroll(app);

    super::register_buffer(&mut app.open_buffers, path);

    super::refresh_git(app);
    super::rebuild_diag_cache(app);

    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
    app.messages.show(format!("Opened {} (line {})", name, line + 1));

    // If a recovery file from a previous crash exists for this path, offer it.
    // Skipped when restoring from the in-session stash: the stash already
    // carries the unsaved edits the recovery file mirrors.
    if !from_stash {
        crate::recovery::offer_on_open(app, path);
    }
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
    text_edits.sort_by_key(|e| std::cmp::Reverse(e.0));

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

/// Perform a plain buffer save (no format step) — used by the format-on-save path.
fn do_save(app: &mut App) {
    match app.buffer.save(None, false) {
        Ok(()) => {
            app.messages.show(format!("Saved {}", app.buffer.display_name()));
            super::refresh_git(app);
        }
        Err(e) => app.messages.show(format!("Error: {e}")),
    }
}

fn word_prefix_at_cursor(app: &crate::app::App) -> String {
    crate::motion::word_prefix_at(&app.buffer.rope, app.selection.head)
}

/// Open a completion popup with `items` and `prefix` as the initial filter.
fn open_completion_popup(app: &mut crate::app::App, items: Vec<crate::popup::ListItem>, prefix: String) {
    let mut popup = crate::popup::Popup::completion(items);
    if let crate::popup::PopupContent::List(ref mut list) = popup.content {
        list.filter = prefix.clone();
        // Don't flash an empty popup — if nothing matches the current prefix,
        // bail out rather than opening then immediately dismissing it.
        //
        // We deliberately do NOT set `suppressed_prefix` here. This response is
        // one async snapshot: the server may have returned incomplete/empty
        // results that the very next keystroke's request fills in (a common
        // cause of "completions sometimes don't show"). Letting later keystrokes
        // re-request keeps autocomplete reliable. Persistent suppression is set
        // only from the user-driven filter path (`sync_completion_filter`), where
        // we have the server's full result set and know the prefix truly matches
        // nothing.
        if !prefix.is_empty() && list.filtered_indices().is_empty() {
            return;
        }
    }
    app.completion.pending_resolve = None;
    app.popup = Some(popup);
}

/// Refresh the `K` documentation side panel of the focused completion popup to
/// match the current selection.  A no-op unless a completion popup with an open
/// doc panel is showing.  Fires a `completionItem/resolve` request (at most one
/// in flight) when the selected item has no inline documentation yet.
pub fn refresh_completion_doc(app: &mut App) {
    // Phase 1 — read the selected item under an immutable borrow.
    let info = match app.popup.as_ref() {
        Some(p) if p.on_confirm == crate::popup::PopupTarget::InsertText => {
            if let crate::popup::PopupContent::List(list) = &p.content {
                if list.doc.is_some() {
                    list.selected_index().map(|idx| {
                        let it = &list.items[idx];
                        (
                            idx,
                            it.documentation.clone(),
                            it.detail.clone(),
                            it.resolve_data.clone(),
                        )
                    })
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    };
    let Some((idx, documentation, detail, resolve_json)) = info else {
        return;
    };

    // Phase 2 — decide content and whether a resolve request is warranted.
    let lang = app.current_language().unwrap_or("").to_owned();
    let can_resolve = documentation.is_none()
        && resolve_json.is_some()
        && !lang.is_empty()
        && app.lsp.completion_resolve_supported(&lang);
    let (lines, loading) = build_doc_content(&documentation, &detail, can_resolve);

    // Phase 3 — write the panel back.
    if let Some(p) = app.popup.as_mut() {
        if let crate::popup::PopupContent::List(ref mut list) = p.content {
            if list.doc.is_some() {
                list.doc = Some(crate::popup::DocPanel { lines, loading });
            }
        }
    }

    // Phase 4 — fire the resolve request (only one outstanding at a time).
    if can_resolve && app.completion.pending_resolve.is_none() {
        if let Some(json) = resolve_json {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json) {
                if app.lsp.request_completion_resolve(&lang, val) {
                    app.completion.pending_resolve = Some(idx);
                }
            }
        }
    }
}

/// Build the doc-panel lines for a completion item: its signature (`detail`)
/// followed by documentation, or a loading/placeholder line.
fn build_doc_content(
    documentation: &Option<String>,
    detail: &Option<String>,
    can_resolve: bool,
) -> (Vec<String>, bool) {
    let mut lines: Vec<String> = Vec::new();
    if let Some(sig) = detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        lines.extend(sig.lines().map(str::to_owned));
    }

    match documentation {
        Some(doc) => {
            let doc = doc.trim();
            if !doc.is_empty() {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.extend(doc.lines().map(str::to_owned));
            }
            if lines.is_empty() {
                lines.push("No documentation available.".into());
            }
            (lines, false)
        }
        None if can_resolve => {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push("Loading documentation…".into());
            (lines, true)
        }
        None => {
            if lines.is_empty() {
                lines.push("No documentation available.".into());
            }
            (lines, false)
        }
    }
}

/// Return the kind badge string for a tree-sitter symbol, using the configured icons map.
fn symbol_kind_badge(kind: &str, icons: &std::collections::HashMap<String, String>) -> String {
    icons.get(kind).cloned().unwrap_or_else(|| kind.to_owned())
}

#[cfg(test)]
mod doc_tests {
    use super::build_doc_content;

    #[test]
    fn shows_signature_and_documentation() {
        let (lines, loading) = build_doc_content(
            &Some("Returns the answer.".into()),
            &Some("def f() -> int".into()),
            false,
        );
        assert!(!loading);
        assert_eq!(lines[0], "def f() -> int");
        assert!(lines.iter().any(|l| l.contains("Returns the answer.")));
    }

    #[test]
    fn loading_state_when_resolvable() {
        let (lines, loading) = build_doc_content(&None, &None, true);
        assert!(loading);
        assert!(lines.iter().any(|l| l.contains("Loading")));
    }

    #[test]
    fn detail_only_when_not_resolvable() {
        let (lines, loading) = build_doc_content(&None, &Some("x: int".into()), false);
        assert!(!loading);
        assert_eq!(lines, vec!["x: int".to_string()]);
    }

    #[test]
    fn placeholder_when_nothing_available() {
        let (lines, loading) = build_doc_content(&None, &None, false);
        assert!(!loading);
        assert_eq!(lines, vec!["No documentation available.".to_string()]);
    }

    #[test]
    fn resolved_empty_documentation_is_not_loading() {
        // An item resolved to empty docs (Some("")) must not show as loading,
        // otherwise the panel would re-request forever.
        let (lines, loading) = build_doc_content(&Some(String::new()), &None, false);
        assert!(!loading);
        assert_eq!(lines, vec!["No documentation available.".to_string()]);
    }
}
