mod lsp;
pub(crate) mod notebook;
mod pickers;
mod search;
mod text;

pub use lsp::{
    apply_code_action, jump_to_location, lsp_did_change, lsp_signature_help,
    process_lsp_events, refresh_completion_doc,
};
pub use search::{search_compute_matches, search_jump};

use ropey::Rope;

use crate::{
    app::App,
    command::Command,
    jump,
    lsp_manager::LspRequestKind,
    mode::{FindDir, Mode},
    motion,
    selection::Selection,
};

// ---------------------------------------------------------------------------
// Special buffers
// ---------------------------------------------------------------------------

pub(crate) const SCRATCH_INTRO: &str = "\
;; This buffer is for notes you don't want to save.\n\
;; Use it for scratch text.\n";

/// Returns true for virtual buffer names that don't correspond to real files.
pub fn is_special_path(path: &std::path::Path) -> bool {
    matches!(path.to_str(), Some("*scratch*") | Some("*Messages*"))
}

/// Save the scratch buffer rope when leaving it (so edits survive switches).
pub(super) fn save_current_special_buffer(app: &mut App) {
    if let Some(ref path) = app.buffer.path.clone() {
        if path.to_str() == Some("*scratch*") {
            app.special_buffer_ropes
                .insert("*scratch*".to_string(), app.buffer.rope.clone());
        }
    }
}

/// Switch the editor to a named special buffer (`*scratch*` or `*Messages*`).
pub fn switch_to_special_buffer(app: &mut App, name: &str) {
    save_current_special_buffer(app);

    if app.notebook.is_some() {
        // Stash the open notebook so edits are preserved if the user comes back.
        // (After this, `app.buffer` holds stale cell text — do NOT stash it.)
        notebook::stash_current_notebook(app);
    } else {
        // Plain buffer: close it with the LSP and keep its unsaved edits in memory.
        if let (Some(ref lang), Some(ref old_path)) =
            (app.lsp_language.clone(), app.buffer.path.clone())
        {
            if !is_special_path(old_path) {
                app.lsp.did_close(lang, old_path);
            }
        }
        stash_current_file_buffer(app);
    }

    let rope = if name == "*scratch*" {
        app.special_buffer_ropes
            .get("*scratch*")
            .cloned()
            .unwrap_or_else(|| Rope::from_str(SCRATCH_INTRO))
    } else {
        // *Messages*: rebuild from the accumulated log.
        let content = if app.messages_log.is_empty() {
            String::new()
        } else {
            let mut s = app.messages_log.join("\n");
            s.push('\n');
            s
        };
        Rope::from_str(&content)
    };

    let mut buf = crate::buffer::Buffer::new_empty();
    buf.rope = rope;
    buf.path = Some(std::path::PathBuf::from(name));

    app.buffer = buf;
    app.selection = Selection::point(0);
    app.scroll_row = 0;
    app.scroll_col = 0;
    app.insert_session_active = false;
    app.lsp_language = None;
    app.highlighter = crate::highlight::Highlighter::new(None);
    recompute_highlights(app);
    app.mode = Mode::Normal;
    app.git_diff.clear();
    rebuild_diag_cache(app);
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Execute a single command against the application state.
pub fn execute(app: &mut App, cmd: &Command) {
    let extend = app.mode == Mode::Select;

    // Capture cursor line before the command so we can detect movement direction.
    let pre_exec_line: usize = {
        let rope = &app.buffer.rope;
        if rope.len_chars() == 0 {
            0
        } else {
            let pos = app.selection.head.min(rope.len_chars());
            rope.char_to_line(pos)
        }
    };

    match cmd {
        // --- Motions ---
        Command::MoveLeft         => app.selection = motion::move_left(&app.buffer.rope, app.selection, extend),
        Command::MoveRight        => app.selection = motion::move_right(&app.buffer.rope, app.selection, extend),
        Command::MoveUp => {
            // In a notebook, `k` on the first line of a cell crosses into the
            // previous cell, landing on its last line (column preserved).
            if app.notebook.is_some() && !app.notebook_focused_edit() && !extend {
                let rope = &app.buffer.rope;
                let pos = app.selection.head.min(rope.len_chars());
                let on_first_line = rope.len_chars() == 0 || rope.char_to_line(pos) == 0;
                let focused = app.notebook.as_ref().map(|(_, s)| s.focused_cell).unwrap_or(0);
                if on_first_line && focused > 0 {
                    let col = motion::col_of(rope, pos);
                    switch_focused_cell(app, focused - 1);
                    let last_line = app.buffer.rope.len_lines().saturating_sub(1);
                    place_cursor_at_line(app, last_line, col);
                    update_scroll(app);
                    return;
                }
            }
            app.selection = motion::move_up(&app.buffer.rope, app.selection, extend);
        }
        Command::MoveDown => {
            // In a notebook, `j` on the last line of a cell crosses into the
            // next cell, landing on its first line (column preserved).
            if app.notebook.is_some() && !app.notebook_focused_edit() && !extend {
                let rope = &app.buffer.rope;
                let pos = app.selection.head.min(rope.len_chars());
                let on_last_line =
                    rope.len_chars() == 0 || rope.char_to_line(pos) + 1 >= rope.len_lines();
                let (focused, count) = app.notebook.as_ref()
                    .map(|(nb, s)| (s.focused_cell, nb.cells.len()))
                    .unwrap_or((0, 0));
                if on_last_line && focused + 1 < count {
                    let col = motion::col_of(rope, pos);
                    switch_focused_cell(app, focused + 1);
                    place_cursor_at_line(app, 0, col);
                    update_scroll(app);
                    return;
                }
            }
            app.selection = motion::move_down(&app.buffer.rope, app.selection, extend);
        }
        Command::MoveWordForward  => app.selection = motion::move_word_forward(&app.buffer.rope, app.selection, extend),
        Command::MoveWordBackward => app.selection = motion::move_word_backward(&app.buffer.rope, app.selection, extend),
        Command::MoveWordEnd      => app.selection = motion::move_word_end(&app.buffer.rope, app.selection, extend),
        Command::MoveBigWordForward  => app.selection = motion::move_big_word_forward(&app.buffer.rope, app.selection, extend),
        Command::MoveBigWordBackward => app.selection = motion::move_big_word_backward(&app.buffer.rope, app.selection, extend),
        Command::MoveBigWordEnd      => app.selection = motion::move_big_word_end(&app.buffer.rope, app.selection, extend),
        Command::MoveLineStart       => app.selection = motion::move_line_start(&app.buffer.rope, app.selection, extend),
        Command::MoveLineFirstNonWs  => app.selection = motion::move_line_first_non_ws(&app.buffer.rope, app.selection, extend),
        Command::MoveLineEnd         => app.selection = motion::move_line_end(&app.buffer.rope, app.selection, extend),
        Command::GotoFileStart       => app.selection = motion::goto_file_start(&app.buffer.rope, app.selection, extend),
        Command::GotoFileEnd         => app.selection = motion::goto_file_end(&app.buffer.rope, app.selection, extend),
        Command::GotoLine(n)  => app.selection = motion::goto_line(&app.buffer.rope, app.selection, *n, extend),
        Command::SelectLine   => app.selection = motion::select_line(&app.buffer.rope, app.selection),
        Command::SelectAll    => app.selection = motion::select_all(&app.buffer.rope),

        // --- Popup / UI ---
        Command::OpenCommandPalette  => { pickers::command_palette(app);  return; }
        Command::GrepBuffer          => { pickers::grep_buffer(app);      return; }
        Command::GrepProject         => { pickers::grep_project(app);     return; }
        Command::OpenBufferPicker    => { pickers::buffer_picker(app);    return; }
        Command::OpenSymbolPicker    => { pickers::symbol_picker(app);    return; }
        Command::OpenFilePicker      => { pickers::file_picker(app);      return; }
        Command::OpenDiagnosticPicker => { pickers::diagnostic_picker(app); return; }

        // --- Sub-mode entries ---
        Command::EnterGotoMode => {
            let extend = app.mode == Mode::Select;
            app.mode = Mode::Goto { extend };
            let lsp_active = app.current_language()
                .map(|l| app.lsp.is_ready(l))
                .unwrap_or(false);
            let mut hints = vec![
                ("g".into(), "go to file start".into()),
                ("e".into(), "go to file end".into()),
                ("h".into(), "go to line first non-whitespace".into()),
                ("l".into(), "go to line end".into()),
                ("z".into(), "scroll cursor to centre".into()),
                ("w".into(), "jump to label in view".into()),
                ("b".into(), "buffer picker".into()),
                ("s".into(), "symbol picker".into()),
                ("c".into(), "comment/uncomment selection".into()),
                ("D".into(), "diagnostic picker".into()),
            ];
            if lsp_active {
                hints.push(("a".into(), "code actions  [LSP]".into()));
                hints.push(("k".into(), "show documentation  [LSP]".into()));
                hints.push(("d".into(), "go to definition  [LSP]".into()));
                hints.push(("r".into(), "go to references  [LSP]".into()));
                hints.push(("y".into(), "go to type definition  [LSP]".into()));
                hints.push(("i".into(), "go to implementation  [LSP]".into()));
            }
            app.popup = Some(crate::popup::Popup::which_key("g", hints));
            return;
        }
        Command::EnterJumpMode => {
            let extend = app.mode == Mode::Select;
            let positions =
                jump::visible_word_starts(&app.buffer.rope, app.scroll_row, app.viewport_height);
            let jump_keys: Vec<char> = app.config.ui.jump_keys.chars().collect();
            app.jump.labels = jump::generate_labels(&positions, &jump_keys);
            app.jump.typed = String::new();
            app.popup = None;
            app.mode = Mode::Jump { extend };
            return;
        }
        Command::FindCharForward => {
            app.mode = Mode::FindChar { dir: FindDir::Forward, till: false };
            app.popup = Some(crate::popup::Popup::which_key(
                "f",
                vec![("any char".into(), "move cursor to next occurrence".into())],
            ));
            return;
        }
        Command::TillCharForward => {
            app.mode = Mode::FindChar { dir: FindDir::Forward, till: true };
            app.popup = Some(crate::popup::Popup::which_key(
                "t",
                vec![("any char".into(), "move cursor till next occurrence".into())],
            ));
            return;
        }
        Command::FindCharBackward => {
            app.mode = Mode::FindChar { dir: FindDir::Backward, till: false };
            app.popup = Some(crate::popup::Popup::which_key(
                "F",
                vec![("any char".into(), "move cursor to previous occurrence".into())],
            ));
            return;
        }
        Command::TillCharBackward => {
            app.mode = Mode::FindChar { dir: FindDir::Backward, till: true };
            app.popup = Some(crate::popup::Popup::which_key(
                "T",
                vec![("any char".into(), "move cursor till previous occurrence".into())],
            ));
            return;
        }

        // --- Editing ---
        Command::DeleteSelection => {
            text::delete_selection(app);
            if app.mode == Mode::Select {
                app.mode = Mode::Normal;
            }
        }
        Command::ChangeSelection => {
            text::delete_selection(app);
            app.mode = Mode::Insert;
        }
        Command::YankSelection => {
            text::yank_selection(app);
            if app.mode == Mode::Select {
                app.mode = Mode::Normal;
            }
        }
        Command::PasteAfter  => text::paste_after(app),
        Command::PasteBefore => text::paste_before(app),
        Command::Undo => {
            if app.buffer.undo() {
                text::clamp_selection(app);
                recompute_highlights(app);
            }
        }
        Command::Redo => {
            if app.buffer.redo() {
                text::clamp_selection(app);
                recompute_highlights(app);
            }
        }
        Command::OpenLineBelow => {
            text::open_line_below(app);
            return;
        }
        Command::OpenLineAbove => {
            text::open_line_above(app);
            return;
        }

        // --- Mode transitions ---
        Command::EnterInsert => {
            app.mode = Mode::Insert;
            return;
        }
        Command::EnterInsertAfter => {
            let len = app.buffer.rope.len_chars();
            if len > 0 {
                let pos = (app.selection.head + 1).min(len);
                app.selection = Selection::point(pos);
            }
            app.mode = Mode::Insert;
            return;
        }
        Command::EnterInsertAtLineStart => {
            app.selection = motion::move_line_start(&app.buffer.rope, app.selection, false);
            app.mode = Mode::Insert;
            return;
        }
        Command::EnterInsertAtLineEnd => {
            let le = motion::move_line_end(&app.buffer.rope, app.selection, false);
            let len = app.buffer.rope.len_chars();
            if len > 0 {
                let pos = (le.head + 1).min(len);
                app.selection = Selection::point(pos);
            } else {
                app.selection = le;
            }
            app.mode = Mode::Insert;
            return;
        }
        Command::EnterNormal => {
            if app.mode == Mode::Insert {
                app.insert_session_active = false;
                let rope = &app.buffer.rope;
                let pos = app.selection.head;
                let ls = if rope.len_chars() > 0 {
                    let li = rope.char_to_line(pos.min(rope.len_chars()));
                    rope.line_to_char(li)
                } else {
                    0
                };
                app.selection = Selection::point(if pos > ls { pos - 1 } else { pos });
            } else {
                app.selection = Selection::point(app.selection.head);
            }
            app.mode = Mode::Normal;
            // The call-signature hint only makes sense while typing arguments.
            app.signature_help = None;
            return;
        }
        Command::EnterSelect => {
            app.mode = Mode::Select;
            return;
        }
        Command::EnterCommandMode => {
            app.mode = Mode::Command;
            app.command_buf.clear();
            return;
        }

        Command::ToggleGitGutter => {
            app.config.editor.git_gutter = !app.config.editor.git_gutter;
            if app.config.editor.git_gutter {
                refresh_git(app);
            } else {
                app.git_diff.clear();
            }
            return;
        }

        // --- Code folding ---
        Command::EnterFoldMode => {
            app.mode = crate::mode::Mode::Fold;
            app.popup = Some(crate::popup::Popup::which_key(
                "z",
                vec![
                    ("a".into(), "toggle fold at cursor".into()),
                    ("A".into(), "toggle all folds".into()),
                ],
            ));
            return;
        }
        Command::FoldToggle => {
            let cursor_line = {
                let rope = &app.buffer.rope;
                let pos = app.selection.head.min(rope.len_chars());
                if rope.len_chars() == 0 { 0 } else { rope.char_to_line(pos) }
            };
            app.fold.toggle_at_line(cursor_line);
            normalize_cursor_folds(app);
            return;
        }
        Command::FoldToggleAll => {
            if app.fold.folded.is_empty() {
                app.fold.close_all();
                normalize_cursor_folds(app);
            } else {
                app.fold.open_all();
            }
            return;
        }

        // --- File / application ---
        Command::Write | Command::WriteForce => {
            let force = matches!(cmd, Command::WriteForce);
            if app.buffer.path.as_deref().map(is_special_path).unwrap_or(false) {
                app.message = Some("Special buffer — nothing to save".into());
                return;
            }
            // format_on_save: try shell formatter first, then LSP.
            if app.notebook.is_none() && app.config.editor.format_on_save {
                if run_shell_formatter(app) {
                    // Shell formatter saved+formatted the file; show result and return.
                    if app.message.is_none() {
                        app.message = Some(format!("Saved {}", app.buffer.display_name()));
                    }
                    return;
                }
                // No shell formatter; try LSP-based format-then-save.
                let lang = app.current_language().map(|l| l.to_owned());
                let path = app.buffer.path.clone();
                if let (Some(lang), Some(path)) = (lang, path) {
                    if !is_special_path(&path) && app.lsp.is_ready(&lang) {
                        let tab_size = app.config.editor.tab_width;
                        app.pending_format_save = true;
                        if app.lsp.format_document(&lang, &path, tab_size, true) {
                            return; // save happens when FormattingResult arrives
                        }
                        app.pending_format_save = false; // server doesn't support formatting
                    }
                }
            }
            if app.notebook.is_some() {
                // Flush any in-progress cell edits into nb.cells before serialising.
                notebook::save_focused_cell(app);
                if let Some((ref mut nb, _)) = app.notebook {
                    match nb.save() {
                        Ok(()) => {
                            let name = nb.path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("notebook.ipynb")
                                .to_string();
                            app.message = Some(format!("Saved {name}"));
                        }
                        Err(e) => app.message = Some(format!("Error: {e}")),
                    }
                }
            } else {
                match app.buffer.save(None, force) {
                    Ok(()) => {
                        app.message = Some(format!("Saved {}", app.buffer.display_name()));
                        refresh_git(app);
                    }
                    Err(e) => app.message = Some(format!("Error: {e}")),
                }
            }
            return;
        }
        Command::WriteAs(_) if app.buffer.path.as_deref().map(is_special_path).unwrap_or(false) => {
            app.message = Some("Special buffer — nothing to save".into());
            return;
        }
        Command::WriteAs(path) => {
            let path = path.clone();
            match app.buffer.save(Some(&path), false) {
                Ok(()) => {
                    app.message = Some(format!("Saved {path}"));
                    refresh_git(app);
                }
                Err(e) => app.message = Some(format!("Error: {e}")),
            }
            return;
        }
        Command::NewFile => {
            app.command_buf.clear();
            app.mode = Mode::Prompt { kind: crate::mode::PromptKind::NewFile };
            return;
        }
        Command::NewNotebook => {
            app.command_buf.clear();
            app.mode = Mode::Prompt { kind: crate::mode::PromptKind::NewNotebook };
            return;
        }
        Command::Quit => {
            // Sweep EVERY buffer in the session, not just the active one — a
            // modified notebook or file stashed by a buffer switch would
            // otherwise be silently discarded (and its recovery file deleted
            // by the clean-exit cleanup).
            let unsaved = unsaved_buffer_names(app);
            if unsaved.is_empty() {
                app.should_quit = true;
            } else {
                app.message = Some(format!(
                    "Unsaved changes in {} — :w to write, :q! to force quit",
                    unsaved.join(", ")
                ));
            }
            return;
        }
        Command::ForceQuit => {
            app.should_quit = true;
            return;
        }
        Command::WriteQuit => {
            // Save the active buffer, then quit only if nothing else in the
            // session still holds unsaved changes (stashed notebooks/files).
            let saved = if app.buffer.path.as_deref().map(is_special_path).unwrap_or(false) {
                true
            } else if app.notebook.is_some() {
                notebook::save_focused_cell(app);
                if let Some((ref mut nb, _)) = app.notebook {
                    match nb.save() {
                        Ok(()) => true,
                        Err(e) => {
                            app.message = Some(format!("Error: {e}"));
                            false
                        }
                    }
                } else {
                    false
                }
            } else {
                match app.buffer.save(None, false) {
                    Ok(()) => true,
                    Err(e) => {
                        app.message = Some(format!("Error: {e}"));
                        false
                    }
                }
            };
            if saved {
                let unsaved = unsaved_buffer_names(app);
                if unsaved.is_empty() {
                    app.should_quit = true;
                } else {
                    app.message = Some(format!(
                        "Saved — but unsaved changes remain in {} (:q! to discard)",
                        unsaved.join(", ")
                    ));
                }
            }
            return;
        }

        Command::BufferClose | Command::BufferForceClose => {
            let force = matches!(cmd, Command::BufferForceClose);

            // Special buffers cannot be closed.
            let is_special = app.buffer.path.as_deref()
                .map(is_special_path)
                .unwrap_or(false);
            if is_special {
                let name = app.buffer.path.as_ref()
                    .and_then(|p| p.to_str())
                    .unwrap_or("this buffer");
                app.message = Some(format!("Cannot close special buffer {name}"));
                return;
            }

            // Check for unsaved changes.
            let is_modified = if let Some((ref nb, _)) = app.notebook {
                nb.modified
            } else {
                app.buffer.modified
            };
            if is_modified && !force {
                app.message = Some(
                    "Buffer modified — save with :w or use :bd! to force close".into(),
                );
                return;
            }

            // Determine the path to remove (notebook path, not virtual cell path).
            let path_to_remove: Option<std::path::PathBuf> =
                if let Some((ref nb, _)) = app.notebook {
                    Some(nb.path.clone())
                } else {
                    app.buffer.path.clone()
                };

            // Tear down notebook/LSP for the current buffer.
            if app.notebook.is_some() {
                notebook::save_focused_cell(app);
                notebook::notebook_lsp_close(app);
                app.notebook = None;
                app.cell_focused_edit = false;
            } else if let (Some(ref lang), Some(ref old_path)) =
                (app.lsp_language.clone(), app.buffer.path.clone())
            {
                app.lsp.did_close(lang, old_path);
            }

            // Remove the closed buffer from the buffer list and any stash.
            if let Some(ref p) = path_to_remove {
                let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
                app.open_buffers.retain(|stored| {
                    let sc = stored.canonicalize().unwrap_or_else(|_| stored.clone());
                    sc != canon && stored != p
                });
                app.notebook_buffers.remove(&canon);
                app.notebook_buffers.remove(p);
                app.file_buffers.remove(&canon);
                app.file_buffers.remove(p);
            }

            // Drop the closed buffer's contents now: the buffer-switch below
            // stashes whatever is in `app.buffer`, and the buffer we just
            // closed must not be resurrected into the stash.
            app.buffer = crate::buffer::Buffer::new_empty();

            // Pick the next buffer: prefer real files over *Messages*, fall back to *scratch*.
            let next = app.open_buffers.iter()
                .find(|p| p.to_str() != Some("*Messages*"))
                .cloned()
                .unwrap_or_else(|| std::path::PathBuf::from("*scratch*"));

            if is_special_path(&next) {
                switch_to_special_buffer(app, next.to_str().unwrap_or("*scratch*"));
            } else if next.extension().and_then(|e| e.to_str()) == Some("ipynb") {
                open_as_notebook(app, &next);
            } else {
                lsp::open_file_at(app, &next, 0, 0);
            }

            app.message = Some("Buffer closed".into());
            return;
        }

        Command::BufferNext => {
            navigate_buffer(app, 1);
            return;
        }
        Command::BufferPrev => {
            navigate_buffer(app, -1);
            return;
        }
        Command::SwitchToScratch => {
            switch_to_special_buffer(app, "*scratch*");
            return;
        }
        Command::SwitchToMessages => {
            switch_to_special_buffer(app, "*Messages*");
            return;
        }

        Command::ToggleLineNumbers => {
            app.config.editor.line_numbers = !app.config.editor.line_numbers;
            return;
        }
        Command::ToggleRelativeLineNumbers => {
            app.config.editor.relative_line_numbers = !app.config.editor.relative_line_numbers;
            return;
        }

        // --- Scripting ---
        Command::Shell(cmd_str) => {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd_str)
                .output();
            match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let msg = if !stdout.is_empty() {
                        stdout.chars().take(200).collect::<String>()
                    } else if !stderr.is_empty() {
                        stderr.chars().take(200).collect::<String>()
                    } else {
                        format!("exit code: {}", out.status)
                    };
                    app.message = Some(msg);
                }
                Err(e) => app.message = Some(format!("shell error: {e}")),
            }
            return;
        }
        Command::Sequence(cmds) => {
            let cmds = cmds.clone();
            for c in &cmds {
                execute(app, c);
            }
            return;
        }

        // --- Notebook commands ---
        Command::NotebookNextCell => {
            let target = app.notebook.as_ref()
                .map(|(_, s)| s.focused_cell + 1)
                .unwrap_or(0);
            switch_focused_cell(app, target);
            return;
        }
        Command::NotebookPrevCell => {
            let target = app.notebook.as_ref()
                .map(|(_, s)| s.focused_cell.saturating_sub(1))
                .unwrap_or(0);
            switch_focused_cell(app, target);
            return;
        }
        Command::NotebookScrollDown => {
            if let Some((ref nb, ref mut state)) = app.notebook {
                let last = nb.cells.len().saturating_sub(1);
                state.scroll_cell = (state.scroll_cell + 1).min(last);
            }
            return;
        }
        Command::NotebookScrollUp => {
            if let Some((_, ref mut state)) = app.notebook {
                state.scroll_cell = state.scroll_cell.saturating_sub(1);
            }
            return;
        }
        Command::NotebookExecuteCell => {
            notebook::execute_focused_cell(app);
            return;
        }
        Command::NotebookRestartKernel => {
            notebook::restart_kernel(app);
            return;
        }
        Command::NotebookInterruptKernel => {
            notebook::interrupt_kernel(app);
            return;
        }
        Command::NotebookExecuteAndAdvance => {
            execute(app, &Command::NotebookExecuteCell);
            execute(app, &Command::NotebookNextCell);
            return;
        }
        Command::NotebookUndoStructural => {
            let snap = {
                let current = app.notebook.as_ref()
                    .map(|(nb, state)| (state.focused_cell, nb.cells.clone()));
                if let Some((focused, cells)) = current {
                    if let Some((_, ref mut state)) = app.notebook {
                        state.pop_snapshot_undo(focused, &cells)
                    } else { None }
                } else { None }
            };
            if let Some((focused, cells)) = snap {
                if let Some((ref mut nb, ref mut state)) = app.notebook {
                    nb.cells = cells;
                    nb.modified = true;
                    state.focused_cell = focused.min(nb.cells.len().saturating_sub(1));
                }
                notebook::after_structural_edit(app);
            } else {
                app.message = Some("Nothing to undo".into());
            }
            return;
        }
        Command::NotebookRedoStructural => {
            let snap = {
                let current = app.notebook.as_ref()
                    .map(|(nb, state)| (state.focused_cell, nb.cells.clone()));
                if let Some((focused, cells)) = current {
                    if let Some((_, ref mut state)) = app.notebook {
                        state.pop_snapshot_redo(focused, &cells)
                    } else { None }
                } else { None }
            };
            if let Some((focused, cells)) = snap {
                if let Some((ref mut nb, ref mut state)) = app.notebook {
                    nb.cells = cells;
                    nb.modified = true;
                    state.focused_cell = focused.min(nb.cells.len().saturating_sub(1));
                }
                notebook::after_structural_edit(app);
            } else {
                app.message = Some("Nothing to redo".into());
            }
            return;
        }
        Command::NotebookNewCellBelow => {
            notebook::insert_new_cell(app, false);
            return;
        }
        Command::NotebookNewCellAbove => {
            notebook::insert_new_cell(app, true);
            return;
        }
        Command::NotebookDeleteCell => {
            notebook::delete_cell(app);
            return;
        }
        Command::NotebookClearOutputs => {
            notebook::clear_outputs(app);
            return;
        }
        Command::NotebookCellToMarkdown | Command::NotebookCellToCode => {
            notebook::convert_cell(app, matches!(cmd, Command::NotebookCellToMarkdown));
            return;
        }

        // --- Notebook cell folding ---
        Command::NotebookToggleFoldCell => {
            if let Some((_, ref mut state)) = app.notebook {
                let idx = state.focused_cell;
                state.toggle_cell_fold(idx);
            }
            return;
        }
        Command::NotebookToggleAllFolds => {
            if let Some((ref nb, ref mut state)) = app.notebook {
                let count = nb.cells.len();
                // If any non-focused cell is unfolded, fold all; otherwise unfold all.
                let any_unfolded = (0..count)
                    .any(|i| i != state.focused_cell && !state.folded_cells.contains(&i));
                if any_unfolded {
                    state.fold_all_cells(count);
                } else {
                    state.unfold_all_cells();
                }
            }
            return;
        }

        // --- Cell edit overlay ---
        Command::NotebookOpenCellEdit => {
            app.cell_focused_edit = true;
            app.mode = Mode::Normal;
            return;
        }
        Command::NotebookCloseCellEdit => {
            app.cell_focused_edit = false;
            app.mode = Mode::Normal;
            // Flush the edited cell to the LSP servers (notebook-sync or
            // per-cell plain doc, chosen per server by the manager).
            let nb_info = app.notebook.as_ref()
                .map(|(nb, _)| (nb.metadata.kernel_language.clone(), nb.path.clone()));
            if let (Some((lang, nb_path)), Some(path)) = (nb_info, app.buffer.path.clone()) {
                let notebook_uri = crate::lsp::path_to_uri(&nb_path);
                let cell_uri = crate::lsp::path_to_uri(&path);
                let text = app.buffer.rope.to_string();
                app.lsp.notebook_did_change_cell(&lang, &notebook_uri, &cell_uri, &text);
            }
            return;
        }

        // --- Notebook ---
        // Open the current `.ipynb` buffer as a notebook. A no-op when one is
        // already open (there's no separate notebook navigation mode anymore —
        // cell navigation is J/K within Normal mode).
        Command::EnterNotebook => {
            if app.notebook.is_none()
                && app.buffer.path.as_ref()
                    .and_then(|p| p.extension())
                    .and_then(|e| e.to_str()) == Some("ipynb")
            {
                if let Some(path) = app.buffer.path.clone() {
                    open_as_notebook(app, &path);
                }
            }
            return;
        }

        // --- Search ---
        Command::SearchForward => {
            app.mode = Mode::Search { forward: true };
            app.search.just_opened = true;
            app.search.active = false;
            search_compute_matches(app);
            return;
        }
        Command::SearchBackward => {
            app.mode = Mode::Search { forward: false };
            app.search.just_opened = true;
            app.search.active = false;
            search_compute_matches(app);
            return;
        }
        Command::SearchNext => {
            search_jump(app, false);
            return;
        }
        Command::SearchPrev => {
            search_jump(app, true);
            return;
        }

        // --- Page scroll ---
        Command::PageDown => {
            let half = (app.viewport_height / 2).max(1);
            for _ in 0..half {
                app.selection = motion::move_down(&app.buffer.rope, app.selection, false);
            }
        }
        Command::PageUp => {
            let half = (app.viewport_height / 2).max(1);
            for _ in 0..half {
                app.selection = motion::move_up(&app.buffer.rope, app.selection, false);
            }
        }

        // --- LSP ---
        Command::LspShowDocumentation => { lsp::lsp_request(app, LspRequestKind::Hover);          return; }
        Command::LspGotoDefinition   => { lsp::lsp_request(app, LspRequestKind::Definition);      return; }
        Command::LspGotoReferences   => { lsp::lsp_request(app, LspRequestKind::References);      return; }
        Command::LspGotoTypeDefinition => { lsp::lsp_request(app, LspRequestKind::TypeDefinition); return; }
        Command::LspGotoImplementation => { lsp::lsp_request(app, LspRequestKind::Implementation); return; }
        Command::LspRequestCompletion => { lsp::lsp_request(app, LspRequestKind::Completion);     return; }
        Command::LspCodeActions      => { lsp::lsp_code_actions_request(app);                     return; }
        Command::FormatDocument => {
            if run_shell_formatter(app) {
                return; // handled (success or failure message already set)
            }
            // No shell formatter configured — fall back to LSP.
            let lang = match app.current_language() {
                Some(l) => l.to_owned(),
                None => {
                    app.message = Some("No formatter configured for this file type".into());
                    return;
                }
            };
            let path = match app.buffer.path.clone() {
                Some(p) => p,
                None => {
                    app.message = Some("Save the file before formatting".into());
                    return;
                }
            };
            if !app.lsp.is_ready(&lang) {
                app.message = Some("No formatter configured (add [formatters.python] to config, or wait for LSP)".into());
                return;
            }
            let tab_size = app.config.editor.tab_width;
            if !app.lsp.format_document(&lang, &path, tab_size, true) {
                app.message = Some("No formatter configured — add [formatters.<lang>] to your config".into());
            }
            return;
        }
        Command::OpenConfig => {
            let path = match crate::config::config_file_path() {
                Some(p) => p,
                None => {
                    app.message = Some("Could not determine config file path".into());
                    return;
                }
            };
            if !path.exists() {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&path, "");
            }
            lsp::open_file_at(app, &path, 0, 0);
            return;
        }
        Command::ReloadConfig => {
            let config = crate::config::Config::load();
            let mut keymap = crate::keymap::Keymap::default_bindings();
            keymap.apply_custom_bindings(&config.keys);
            app.config = config;
            app.keymap = keymap;
            app.message = Some("Config reloaded".into());
            return;
        }

        // --- Editing (continued) ---
        Command::CommentRegion => {
            text::comment_region(app);
            if app.mode == Mode::Select {
                app.mode = Mode::Normal;
            }
        }
        Command::IndentRegion => text::indent_region(app),
        Command::DedentRegion => text::dedent_region(app),

        Command::KillToEndOfLine => {
            let pos = app.selection.head;
            if app.buffer.rope.len_chars() > 0 {
                let eol = motion::move_line_end(&app.buffer.rope, Selection::point(pos), false).head;
                if pos <= eol {
                    app.selection = Selection::new(pos, eol);
                    text::delete_selection(app);
                }
            }
            if app.mode == Mode::Select {
                app.mode = Mode::Normal;
            }
            return;
        }

        Command::ScrollCursorCenter => {
            let rope = &app.buffer.rope;
            let cursor_line = if rope.len_chars() > 0 {
                rope.char_to_line(app.selection.head.min(rope.len_chars()))
            } else {
                0
            };
            let half = (app.viewport_height / 2).max(1);
            app.scroll_row = cursor_line.saturating_sub(half);
            app.scroll_row = app.fold.normalize_scroll_row(app.scroll_row);
            return;
        }

        Command::ToggleWordWrap => {
            app.config.editor.word_wrap = !app.config.editor.word_wrap;
            // Disable horizontal scroll when wrapping.
            if app.config.editor.word_wrap {
                app.scroll_col = 0;
            }
            return;
        }
        Command::ShowDashboard => {
            app.show_splash = true;
            return;
        }
    }

    // If a motion landed the cursor inside a hidden fold, snap out direction-aware.
    normalize_cursor_folds_directional(app, pre_exec_line);
    update_scroll(app);
}

/// If the cursor is inside a hidden fold region, move it to the fold's start line.
pub fn normalize_cursor_folds(app: &mut App) {
    if app.fold.ranges.is_empty() { return; }
    let rope = &app.buffer.rope;
    if rope.len_chars() == 0 { return; }
    let pos = app.selection.head.min(rope.len_chars());
    let line_idx = rope.char_to_line(pos);
    if app.fold.is_hidden(line_idx) {
        let vis_line = app.fold.normalize_line(line_idx);
        let new_pos = rope.line_to_char(vis_line);
        app.selection = Selection::point(new_pos);
    }
}

/// Direction-aware version: if cursor landed inside a hidden fold, snap to
/// fold_start when moving backward/up or to fold_end+1 when moving forward/down.
fn normalize_cursor_folds_directional(app: &mut App, pre_exec_line: usize) {
    if app.fold.ranges.is_empty() { return; }
    let rope = &app.buffer.rope;
    if rope.len_chars() == 0 { return; }
    let pos = app.selection.head.min(rope.len_chars());
    let line_idx = rope.char_to_line(pos);
    if !app.fold.is_hidden(line_idx) { return; }

    let moved_forward = line_idx > pre_exec_line;

    if moved_forward {
        // Moving down/forward: jump past the fold to the first line after it.
        // Find the fold that contains this hidden line.
        let snap_line = app.fold.folded.iter()
            .filter_map(|&start| app.fold.range_starting_at(start))
            .find(|&(s, e)| line_idx > s && line_idx <= e)
            .map(|(_, e)| (e + 1).min(rope.len_lines().saturating_sub(1)))
            .unwrap_or_else(|| app.fold.normalize_line(line_idx));
        let new_pos = rope.line_to_char(snap_line);
        app.selection = Selection::point(new_pos);
    } else {
        // Moving up/backward: snap to the fold start line.
        let vis_line = app.fold.normalize_line(line_idx);
        let new_pos = rope.line_to_char(vis_line);
        app.selection = Selection::point(new_pos);
    }
}

/// Switch the focused notebook cell to `new_idx` (clamped to the valid range),
/// flushing the current cell to the LSP and notebook model first and loading the
/// target cell into `app.buffer`. The cursor lands at the start of the new cell;
/// callers wanting a specific position set the selection afterwards. No-op when
/// no notebook is open.
fn switch_focused_cell(app: &mut App, new_idx: usize) {
    if app.notebook.is_none() {
        return;
    }
    lsp_did_change(app);
    notebook::save_focused_cell(app);
    if let Some((ref nb, ref mut state)) = app.notebook {
        let last = nb.cells.len().saturating_sub(1);
        state.focused_cell = new_idx.min(last);
    }
    notebook::ensure_focused_visible(app);
    notebook::load_focused_cell(app);
}

/// Place a point selection on `line_idx` (clamped) at column `col` (clamped to
/// the line's content), using the same column discipline as vertical motion.
fn place_cursor_at_line(app: &mut App, line_idx: usize, col: usize) {
    let rope = &app.buffer.rope;
    if rope.len_chars() == 0 {
        app.selection = Selection::point(0);
        return;
    }
    let line_idx = line_idx.min(rope.len_lines().saturating_sub(1));
    let line_start = rope.line_to_char(line_idx);
    let line = rope.line(line_idx);
    let nl = line.len_chars();
    let content_len = if nl > 0 && (line.char(nl - 1) == '\n' || line.char(nl - 1) == '\r') {
        nl - 1
    } else {
        nl
    };
    let head = if content_len == 0 {
        line_start
    } else {
        line_start + col.min(content_len - 1)
    };
    app.selection = Selection::point(head);
}

/// Cycle through `open_buffers` by `delta` (+1 = next, -1 = prev).
fn navigate_buffer(app: &mut App, delta: i32) {
    let n = app.open_buffers.len();
    if n <= 1 {
        return;
    }

    let current_canon = if let Some((ref nb, _)) = app.notebook {
        nb.path.canonicalize().unwrap_or_else(|_| nb.path.clone())
    } else if let Some(ref p) = app.buffer.path {
        p.canonicalize().unwrap_or_else(|_| p.clone())
    } else {
        return;
    };

    let current_idx = app.open_buffers.iter().position(|p| {
        p.canonicalize().unwrap_or_else(|_| p.clone()) == current_canon
    });

    let idx = match current_idx {
        Some(i) => ((i as i32 + delta).rem_euclid(n as i32)) as usize,
        None => 0,
    };

    let target = app.open_buffers[idx].clone();
    if is_special_path(&target) {
        switch_to_special_buffer(app, target.to_str().unwrap_or("*scratch*"));
    } else if target.extension().and_then(|e| e.to_str()) == Some("ipynb") {
        open_as_notebook(app, &target);
    } else {
        lsp::open_file_at(app, &target, 0, 0);
    }
}

/// Open a `.ipynb` file as a notebook, replacing whatever is currently open.
/// Called when the user selects a notebook from the buffer picker.
pub fn open_as_notebook(app: &mut App, path: &std::path::Path) {
    use crate::{notebook::Notebook, notebook_state::NotebookState};

    // Save scratch content when leaving it.
    save_current_special_buffer(app);

    // Stash or close whatever is currently open.
    if app.notebook.is_some() {
        notebook::stash_current_notebook(app);
    } else {
        if let (Some(ref lang), Some(ref old_path)) =
            (app.lsp_language.clone(), app.buffer.path.clone())
        {
            if !is_special_path(old_path) {
                app.lsp.did_close(lang, old_path);
            }
        }
        stash_current_file_buffer(app);
    }

    // Restore from stash if we've visited this notebook before (preserves unsaved edits).
    if notebook::restore_stashed_notebook(app, path) {
        register_buffer(&mut app.open_buffers, path);
        app.message = Some(format!(
            "Opened {}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        ));
        return;
    }

    let nb = match Notebook::from_path(path) {
        Ok(n) => n,
        Err(e) => {
            app.message = Some(format!("Failed to open notebook: {e}"));
            return;
        }
    };

    let lang = nb.metadata.kernel_language.clone();
    app.notebook = Some((nb, NotebookState::new()));
    app.cell_focused_edit = false;
    app.mode = Mode::Normal;
    app.lsp_language = Some(lang);
    // Load cell 0 into the buffer — this sets the buffer/path/highlighter,
    // resets the selection + scroll, and opens the cell with the LSP.
    notebook::load_focused_cell(app);
    // Register the whole notebook with a notebook-aware server. When the server
    // is still initializing this is a no-op; the Initialized event re-runs it.
    notebook::notebook_lsp_open(app);

    register_buffer(&mut app.open_buffers, path);

    app.message = Some(format!(
        "Opened {}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
    ));

    // Offer to restore unsaved cells from a previous crash, if any.
    crate::recovery::offer_on_open(app, path);
}

/// Execute a slice of commands in order.
pub fn run_many(app: &mut App, cmds: &[Command]) {
    for cmd in cmds {
        execute(app, cmd);
    }
}

/// Mark highlight spans as stale.  The render loop recomputes them once per
/// frame, so callers don't pay the tree-sitter cost on every keystroke.
pub fn recompute_highlights(app: &mut App) {
    app.highlights_dirty = true;
}

/// Kick off a background git refresh (branch + per-line diff marks for the
/// current buffer).  The result is applied by the run loop when it arrives —
/// a slow or absent git can never block the UI.
pub fn refresh_git(app: &mut App) {
    let path = if app.notebook.is_some() {
        None // notebook buffers have virtual paths; no per-line diff applies
    } else {
        app.buffer.path.clone().filter(|p| !is_special_path(p))
    };
    app.git_pending = Some(crate::git::refresh(path));
}

/// Apply a finished background git refresh, if one is ready.  Returns true
/// when state changed (the caller should redraw).
pub fn poll_git(app: &mut App) -> bool {
    let Some(pending) = &app.git_pending else { return false };
    let Some(info) = pending.poll() else { return false };
    app.git_pending = None;
    app.git_branch = info.branch;
    app.git_diff = if app.config.editor.git_gutter {
        info.diff
    } else {
        Default::default()
    };
    true
}

/// Drain streamed output from the running kernel and apply it to the executing
/// cell. Called once per frame so outputs (incl. live progress bars) appear as
/// they are produced rather than only when the cell finishes.
/// Returns true when any output was applied (the caller should redraw).
pub fn process_kernel_events(app: &mut App) -> bool {
    use crate::notebook::{append_stream, push_error_output, KernelMessage, KernelStatus, MimeData, Output};

    let mut refresh_images = false;
    let mut applied = false;
    if let Some((ref mut nb, ref mut state)) = app.notebook {
        let Some(idx) = state.executing_cell else { return false };
        let msgs = match nb.kernel.as_mut() {
            Some(k) => k.poll(),
            None => {
                state.executing_cell = None;
                return true;
            }
        };
        if msgs.is_empty() {
            return false;
        }
        applied = true;
        for msg in msgs {
            if idx >= nb.cells.len() {
                break;
            }
            match msg {
                KernelMessage::Stream { name, text } => {
                    append_stream(&mut nb.cells[idx].outputs, &name, &text);
                }
                KernelMessage::Image { png } => {
                    nb.cells[idx].outputs.push(Output::DisplayData {
                        data: MimeData { text_plain: None, image_png: Some(std::sync::Arc::new(png)) },
                    });
                    refresh_images = true;
                }
                KernelMessage::Error { traceback } => {
                    push_error_output(&mut nb.cells[idx].outputs, &traceback);
                }
                KernelMessage::Done => {
                    if let Some(ref mut k) = nb.kernel {
                        k.execution_count += 1;
                        k.status = KernelStatus::Idle;
                        nb.cells[idx].execution_count = Some(k.execution_count);
                    }
                    state.executing_cell = None;
                    nb.modified = true;
                    refresh_images = true;
                }
                KernelMessage::Dead => {
                    if let Some(ref mut k) = nb.kernel {
                        k.status = KernelStatus::Dead;
                    }
                    state.executing_cell = None;
                    refresh_images = true;
                }
            }
        }
    }
    if refresh_images {
        app.graphics.image_ids.clear();
    }
    applied
}

/// Rebuild the per-line diagnostic cache for the current buffer.
/// Call this after diagnostics change or after switching files.
pub fn rebuild_diag_cache(app: &mut App) {
    app.diag_by_line.clear();
    if let Some(ref path) = app.buffer.path {
        let key = crate::lsp::diagnostic_key(path);
        if let Some(diags) = app.lsp.diagnostics.get(&key) {
            for d in diags {
                app.diag_by_line
                    .entry(d.line)
                    .or_default()
                    .push((d.col_start, d.col_end, d.severity.clone()));
            }
        }
    }
}

/// Update scroll_row / scroll_col so the cursor is visible.
///
/// Uses the stored viewport dimensions (`app.viewport_height` / `app.viewport_width`)
/// which are refreshed at the top of every render frame.  This is the single
/// authoritative scroll function.
pub fn update_scroll(app: &mut App) {
    let visible_rows = app.viewport_height;
    let git_col = if app.config.editor.git_gutter && app.notebook.is_none() { 1usize } else { 0 };
    let gutter_width = if app.config.editor.line_numbers { 5 + git_col } else { git_col };
    let visible_cols = app.viewport_width.saturating_sub(gutter_width);
    let word_wrap = app.config.editor.word_wrap;

    let rope = &app.buffer.rope;
    if rope.len_chars() == 0 {
        app.scroll_row = 0;
        app.scroll_col = 0;
        return;
    }
    if visible_rows == 0 || visible_cols == 0 {
        return;
    }

    let pos = app.selection.head.min(rope.len_chars());
    let line_idx = rope.char_to_line(pos);
    let total_lines = rope.len_lines();
    let scroll_off = app.config.editor.scroll_off;
    let tab_width = app.config.editor.tab_width;

    if app.notebook.is_some() && !app.notebook_focused_edit() {
        // Editing within the focused cell.  The cell is in `app.buffer`, but it
        // does not fill the viewport: cells above it (and its own top border)
        // push its content downward.  We first keep the focused cell visible at
        // the cell granularity, then scroll *within* the cell so the cursor
        // stays inside the rows actually available below that offset — the same
        // text-buffer behaviour you'd expect, rather than letting the cursor
        // slide off the bottom of the screen.
        let image_rows = app.config.notebook.image_rows;
        let cell_px = app.graphics.cell_pixel_size;
        let avail_cols = app.viewport_width.saturating_sub(2) as u16;
        let mut new_scroll_row = app.scroll_row;
        if let Some((nb, state)) = app.notebook.as_mut() {
            state.ensure_focused_visible(
                &nb.cells, visible_rows, rope, image_rows, cell_px, avail_cols,
            );
            let focused = state.focused_cell.min(nb.cells.len().saturating_sub(1));
            // Rows consumed above the focused cell's content: each fully-shown
            // preceding cell plus the 1-row inter-cell gap, then the focused
            // cell's own top border.
            let mut content_top = 0usize;
            for idx in state.scroll_cell..focused {
                let h = if state.is_cell_folded(idx) {
                    3
                } else {
                    crate::notebook_ui::cell_display_height(
                        &nb.cells[idx].source, &nb.cells[idx], image_rows, cell_px, avail_cols,
                    ) as usize
                };
                content_top += h + 1;
            }
            content_top += 1; // top border of the focused cell

            let avail = visible_rows.saturating_sub(content_top).max(1);
            let so = scroll_off.min(avail.saturating_sub(1) / 2);
            if line_idx < new_scroll_row + so || new_scroll_row > line_idx {
                new_scroll_row = line_idx.saturating_sub(so);
            } else if line_idx + so + 1 > new_scroll_row + avail {
                new_scroll_row = (line_idx + so + 1).saturating_sub(avail);
            }
            let max_scroll = total_lines.saturating_sub(1);
            if new_scroll_row > max_scroll {
                new_scroll_row = max_scroll;
            }
        }
        app.scroll_row = new_scroll_row;
    } else {
        // Normalize scroll_row so it never points inside a hidden fold region.
        app.scroll_row = app.fold.normalize_scroll_row(app.scroll_row);

        // Vertical — fold+wrap-aware row count from scroll_row to cursor line.
        let vdist = if word_wrap {
            wrap_visible_row_count(app, app.scroll_row, line_idx, visible_cols, tab_width)
        } else {
            app.fold.visible_row_count(app.scroll_row, line_idx, total_lines)
        };

        if vdist < scroll_off || app.scroll_row > line_idx {
            // Cursor too close to top (or above scroll area): scroll up.
            let desired = scroll_off.min(line_idx);
            app.scroll_row = if word_wrap {
                wrap_scroll_row_for_cursor(&app.fold, rope, line_idx, desired, visible_cols, tab_width)
            } else {
                app.fold.scroll_row_for_cursor(line_idx, desired)
            };
        } else if vdist + scroll_off >= visible_rows {
            // Cursor too close to bottom: scroll down.
            let desired = visible_rows.saturating_sub(scroll_off + 1);
            app.scroll_row = if word_wrap {
                wrap_scroll_row_for_cursor(&app.fold, rope, line_idx, desired, visible_cols, tab_width)
            } else {
                app.fold.scroll_row_for_cursor(line_idx, desired)
            };
        }
    }

    if word_wrap {
        // No horizontal scrolling when wrapping.
        app.scroll_col = 0;
        return;
    }

    // Horizontal — accurate display-column calculation (handles tabs)
    let line_start = rope.line_to_char(line_idx);
    let line_str = rope.line(line_idx);
    let cursor_off = pos - line_start;
    let mut display_col: usize = 0;
    for i in 0..cursor_off {
        display_col +=
            crate::render_util::char_display_width(line_str.char(i), display_col, tab_width);
    }

    if display_col < app.scroll_col {
        app.scroll_col = display_col;
    }
    if display_col >= app.scroll_col + visible_cols {
        app.scroll_col = display_col.saturating_sub(visible_cols) + 1;
    }
}

/// Count visual rows from `from` (inclusive) to `to` (exclusive), accounting
/// for folds and word-wrap.  `text_width` is the number of display columns
/// available for text (viewport minus gutter).
fn wrap_visible_row_count(
    app: &App,
    from: usize,
    to: usize,
    text_width: usize,
    tab_width: usize,
) -> usize {
    let rope = &app.buffer.rope;
    let total_lines = rope.len_lines();
    let mut count = 0;
    let mut line = from;
    while line < to && line < total_lines {
        if app.fold.is_hidden(line) {
            line += 1;
            continue;
        }
        if let Some(end) = app.fold.fold_end_at(line) {
            count += 1;
            line = end + 1;
        } else {
            count += crate::ui::visual_line_height(rope, line, text_width, tab_width);
            line += 1;
        }
    }
    count
}

/// Walk backward from `cursor_line` by `desired_vrows` visual rows (fold+wrap
/// aware) and return the resulting scroll_row.
fn wrap_scroll_row_for_cursor(
    fold: &crate::fold::FoldState,
    rope: &ropey::Rope,
    cursor_line: usize,
    desired_vrows: usize,
    text_width: usize,
    tab_width: usize,
) -> usize {
    let mut line = cursor_line;
    let mut remaining = desired_vrows;

    while remaining > 0 && line > 0 {
        line -= 1;
        if let Some(start) = fold.fold_start_hiding(line) {
            line = start;
        }
        let height = if fold.is_hidden(line) {
            0
        } else if fold.fold_end_at(line).is_some() {
            1
        } else {
            crate::ui::visual_line_height(rope, line, text_width, tab_width)
        };
        remaining = remaining.saturating_sub(height);
    }
    line
}

// ---------------------------------------------------------------------------
// Shell formatter
// ---------------------------------------------------------------------------

/// Run the configured shell formatter for the current buffer's language.
///
/// Flow: save buffer → run `command args... <file>` → reload formatted content.
///
/// Returns `true` if a formatter was configured and was attempted (the caller
/// should not try anything else for this save/format cycle).
/// Returns `false` if no formatter is configured for this language (caller
/// should fall back to LSP or a plain save).
fn run_shell_formatter(app: &mut App) -> bool {
    let path = match app.buffer.path.clone() {
        Some(p) => p,
        None => return false,
    };
    if is_special_path(&path) {
        return false;
    }
    let lang = match app.current_language() {
        Some(l) => l.to_owned(),
        None => return false,
    };
    let fmt = match app.config.formatters.get(&lang).cloned() {
        Some(f) => f,
        None => return false,
    };

    // Save current buffer content to disk first so the formatter sees it.
    if let Err(e) = app.buffer.save(None, false) {
        app.message = Some(format!("Could not save before formatting: {e}"));
        return true;
    }

    let result = std::process::Command::new(&fmt.command)
        .args(&fmt.args)
        .arg(&path)
        .output();

    match result {
        Ok(out) if out.status.success() => {
            // Reload the formatter's output back into the buffer.
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    app.buffer.rope = ropey::Rope::from_str(&content);
                    app.buffer.modified = false;
                    // The formatter rewrote the file; re-stat so the next save's
                    // external-modification check doesn't false-positive.
                    app.buffer.refresh_disk_mtime();
                    recompute_highlights(app);
                    lsp::lsp_did_change(app);
                    refresh_git(app);
                }
                Err(e) => {
                    app.message = Some(format!("Could not reload after format: {e}"));
                }
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.trim();
            app.message = Some(if msg.is_empty() {
                format!("Formatter exited with code {}", out.status.code().unwrap_or(-1))
            } else {
                msg.chars().take(200).collect()
            });
        }
        Err(e) => {
            app.message = Some(format!("Formatter '{}': {e}", fmt.command));
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Buffer list helpers
// ---------------------------------------------------------------------------

/// Resolve the directory new files should be created in: the directory of the
/// open notebook or current buffer, falling back to the working directory for
/// special buffers (scratch / messages / dashboard) or unnamed buffers.
fn current_buffer_dir(app: &App) -> std::path::PathBuf {
    if let Some((ref nb, _)) = app.notebook {
        return crate::notebook::notebook_dir(&nb.path);
    }
    app.buffer.path.as_deref()
        .filter(|p| !is_special_path(p))
        .and_then(|p| p.parent())
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
}

/// Resolve `name` against the current buffer's directory (absolute names are
/// used verbatim).
fn resolve_new_path(app: &App, name: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(name);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        current_buffer_dir(app).join(p)
    }
}

/// Create an empty file in the current buffer's directory and open it.
/// If it already exists, just open it instead of clobbering.
/// Called from the minibuffer `Prompt` handler once a name has been entered.
pub(crate) fn create_new_file(app: &mut App, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        app.message = Some("Usage: :new-file <name>".into());
        return;
    }
    let path = resolve_new_path(app, name);
    if path.exists() {
        app.message = Some(format!("{name} already exists — opening"));
        lsp::open_file_at(app, &path, 0, 0);
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            app.message = Some(format!("Could not create directory: {e}"));
            return;
        }
    }
    if let Err(e) = std::fs::write(&path, "") {
        app.message = Some(format!("Could not create file: {e}"));
        return;
    }
    lsp::open_file_at(app, &path, 0, 0);
    app.message = Some(format!("Created {name}"));
}

/// Create a valid empty `.ipynb` notebook in the current buffer's directory and
/// open it in the notebook interface.  If it already exists, just open it.
/// Called from the minibuffer `Prompt` handler once a name has been entered.
pub(crate) fn create_new_notebook(app: &mut App, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        app.message = Some("Usage: :new-notebook <name>".into());
        return;
    }
    // Ensure the file carries the .ipynb extension so it opens as a notebook.
    let mut name = name.to_string();
    if std::path::Path::new(&name).extension().and_then(|e| e.to_str()) != Some("ipynb") {
        name.push_str(".ipynb");
    }
    let path = resolve_new_path(app, &name);
    if path.exists() {
        app.message = Some(format!("{name} already exists — opening"));
        open_as_notebook(app, &path);
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            app.message = Some(format!("Could not create directory: {e}"));
            return;
        }
    }
    if let Err(e) = std::fs::write(&path, crate::notebook::empty_notebook_json()) {
        app.message = Some(format!("Could not create notebook: {e}"));
        return;
    }
    open_as_notebook(app, &path);
    app.message = Some(format!("Created {name}"));
}

/// Stash the current plain-file buffer so unsaved edits and undo history
/// survive switching away (the buffer is otherwise reloaded from disk when the
/// user comes back).  No-op for notebooks (stashed separately via
/// `notebook::stash_current_notebook`), special buffers, and path-less buffers.
/// Leaves `app.buffer` empty — every caller immediately replaces it.
pub(crate) fn stash_current_file_buffer(app: &mut App) {
    if app.notebook.is_some() {
        return;
    }
    let Some(path) = app.buffer.path.clone() else { return };
    if is_special_path(&path) {
        return;
    }
    let key = path.canonicalize().unwrap_or(path);
    let buf = std::mem::replace(&mut app.buffer, crate::buffer::Buffer::new_empty());
    app.file_buffers.insert(key, buf);
}

/// Remove and return the stashed buffer for `path`, if one exists.
pub(crate) fn take_stashed_file_buffer(
    app: &mut App,
    path: &std::path::Path,
) -> Option<crate::buffer::Buffer> {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    app.file_buffers.remove(&key)
}

/// Names of every buffer holding unsaved changes, anywhere in the session:
/// the active buffer/notebook, stashed notebooks, and stashed plain files.
/// Special buffers (scratch/messages) are excluded — they are throwaway by
/// design and covered by crash recovery.
pub(crate) fn unsaved_buffer_names(app: &App) -> Vec<String> {
    fn short(p: &std::path::Path) -> String {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string_lossy().into_owned())
    }
    let mut names = Vec::new();
    if let Some((nb, _)) = &app.notebook {
        if nb.modified {
            names.push(short(&nb.path));
        }
    } else if app.buffer.modified {
        if let Some(p) = app.buffer.path.as_deref().filter(|p| !is_special_path(p)) {
            names.push(short(p));
        }
    }
    for (path, (nb, _)) in &app.notebook_buffers {
        if nb.modified {
            names.push(short(path));
        }
    }
    for (path, buf) in &app.file_buffers {
        if buf.modified {
            names.push(short(path));
        }
    }
    names
}

/// Append `path` to `open_buffers` if it is not already present (by canonical path).
fn register_buffer(open_buffers: &mut Vec<std::path::PathBuf>, path: &std::path::Path) {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !open_buffers.iter().any(|stored| {
        stored.canonicalize().unwrap_or_else(|_| stored.clone()) == canon
    }) {
        open_buffers.push(canon);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use ropey::Rope;

    #[test]
    fn test_exec_clamping_behavior() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();

        app.buffer.rope = Rope::from_str("hello\nworld\n");
        let len = app.buffer.rope.len_chars();
        assert_eq!(len, 12);

        app.selection = Selection::point(20);
        text::clamp_selection(&mut app);
        assert_eq!(app.selection.head, 12);
        assert_eq!(app.selection.anchor, 12);

        app.selection = Selection::point(12);
        update_scroll(&mut app);
        assert_eq!(app.buffer.rope.char_to_line(12), 2);
    }

    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sv-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_new_file_creates_and_opens() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();

        let dir = unique_tmp_dir("newfile");
        let target = dir.join("scratch_test.txt");
        let _ = std::fs::remove_file(&target);
        // Anchor the "current directory" by giving the buffer a path in `dir`.
        app.buffer.path = Some(dir.join("anchor.txt"));

        create_new_file(&mut app, "scratch_test.txt");

        assert!(target.exists(), "new-file should create the file on disk");
        assert_eq!(
            app.buffer.path.as_deref().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("scratch_test.txt")),
            "editor should switch to the new file's buffer"
        );
        let _ = std::fs::remove_file(&target);
    }

    #[test]
    fn test_new_notebook_creates_valid_ipynb() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();

        let dir = unique_tmp_dir("newnb");
        let target = dir.join("analysis.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));

        // Name given without extension — `.ipynb` should be appended.
        create_new_notebook(&mut app, "analysis");

        assert!(target.exists(), "new-notebook should create the .ipynb on disk");
        assert!(app.notebook.is_some(), "editor should open the notebook view");
        // The file must round-trip back through the notebook parser.
        let reparsed = crate::notebook::Notebook::from_path(&target);
        assert!(reparsed.is_ok(), "created notebook must be valid nbformat");
        let _ = std::fs::remove_file(&target);
    }

    #[test]
    fn test_delete_selection_clamping() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.buffer.rope = Rope::from_str("abc");
        app.selection = Selection::new(0, 2);
        text::delete_selection(&mut app);
        assert_eq!(app.buffer.rope.len_chars(), 0);
        assert_eq!(app.selection.head, 0);
    }

    #[test]
    fn buffer_switch_preserves_unsaved_edits() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();

        let dir = unique_tmp_dir("stash");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, "alpha\n").unwrap();
        std::fs::write(&b, "beta\n").unwrap();

        lsp::open_file_at(&mut app, &a, 0, 0);
        // Make an unsaved edit to a.txt.
        app.buffer.insert(0, "EDIT ");
        assert!(app.buffer.modified);

        // Switch to b.txt and back — the edit must survive in memory.
        lsp::open_file_at(&mut app, &b, 0, 0);
        assert_eq!(app.buffer.rope.to_string(), "beta\n");
        lsp::open_file_at(&mut app, &a, 0, 0);
        assert_eq!(app.buffer.rope.to_string(), "EDIT alpha\n");
        assert!(app.buffer.modified, "modified flag must survive the round trip");

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn quit_blocks_on_stashed_unsaved_buffer() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();

        let dir = unique_tmp_dir("quitsweep");
        let a = dir.join("dirty.txt");
        let b = dir.join("clean.txt");
        std::fs::write(&a, "x\n").unwrap();
        std::fs::write(&b, "y\n").unwrap();

        lsp::open_file_at(&mut app, &a, 0, 0);
        app.buffer.insert(0, "unsaved ");
        // Stash the dirty buffer by switching away.
        lsp::open_file_at(&mut app, &b, 0, 0);
        assert!(!app.buffer.modified, "active buffer is clean");

        // :q must refuse — the *stashed* buffer has unsaved changes.
        execute(&mut app, &Command::Quit);
        assert!(!app.should_quit, "quit must be blocked by stashed dirty buffer");
        assert!(
            app.message.as_deref().unwrap_or("").contains("dirty.txt"),
            "message should name the dirty buffer: {:?}",
            app.message
        );

        // :q! still force-quits.
        execute(&mut app, &Command::ForceQuit);
        assert!(app.should_quit);

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn save_refuses_external_modification_unless_forced() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();

        let dir = unique_tmp_dir("mtime");
        let f = dir.join("conflict.txt");
        std::fs::write(&f, "original\n").unwrap();

        lsp::open_file_at(&mut app, &f, 0, 0);
        app.buffer.insert(0, "mine ");

        // Simulate an external edit (ensure a different mtime).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, "theirs\n").unwrap();
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let _ = std::fs::File::open(&f).and_then(|h| h.set_modified(bumped));

        execute(&mut app, &Command::Write);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "theirs\n",
            ":w must not clobber an externally-modified file"
        );
        assert!(app.message.as_deref().unwrap_or("").contains("changed on disk"));

        execute(&mut app, &Command::WriteForce);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "mine original\n",
            ":w! must overwrite"
        );

        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn test_notebook_cross_cell_motion() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();

        // Start from a real on-disk notebook (one empty cell), then give the
        // first cell content and append a second cell.
        let dir = unique_tmp_dir("xcell");
        let target = dir.join("xcell.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "xcell");

        if let Some((ref mut nb, ref mut state)) = app.notebook {
            nb.cells[0].source = Rope::from_str("a\nb");
            let mut second = nb.cells[0].clone();
            second.id = crate::notebook::new_cell_id();
            second.source = Rope::from_str("c\nd");
            nb.cells.push(second);
            state.focused_cell = 0;
        }
        notebook::load_focused_cell(&mut app);
        assert_eq!(app.buffer.rope.to_string(), "a\nb");

        // `j` on the last line of cell 0 crosses into cell 1, first line.
        app.selection = Selection::point(2); // the 'b'
        execute(&mut app, &Command::MoveDown);
        assert_eq!(app.notebook.as_ref().unwrap().1.focused_cell, 1);
        assert_eq!(app.buffer.rope.to_string(), "c\nd");
        assert_eq!(app.selection.head, 0); // first line, column preserved

        // `k` on the first line of cell 1 crosses back into cell 0, last line.
        execute(&mut app, &Command::MoveUp);
        assert_eq!(app.notebook.as_ref().unwrap().1.focused_cell, 0);
        assert_eq!(app.buffer.rope.to_string(), "a\nb");
        assert_eq!(app.buffer.rope.char_to_line(app.selection.head), 1); // last line

        // `k` at the top cell stays put (no previous cell to cross into).
        app.selection = Selection::point(0);
        execute(&mut app, &Command::MoveUp);
        assert_eq!(app.notebook.as_ref().unwrap().1.focused_cell, 0);

        let _ = std::fs::remove_file(&target);
    }
}
