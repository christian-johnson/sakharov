mod buffers;
mod export;
mod format;
mod lsp;
pub(crate) mod notebook;
mod pickers;
mod scroll;
mod search;
mod text;

pub use buffers::{is_special_path, open_as_notebook, switch_to_special_buffer};
pub use export::{poll_export, ExportJob};
pub(crate) use buffers::{create_new_file, create_new_notebook, SCRATCH_INTRO};
pub use lsp::{
    apply_code_action, jump_to_location, lsp_did_change, lsp_did_change_insert,
    lsp_did_change_remove, lsp_signature_help, process_lsp_events, pump_signature_help,
    refresh_completion_doc,
};
pub use scroll::{normalize_cursor_folds, update_scroll};
pub use search::{search_compute_matches, search_jump};

// Names used by `execute()` and by sibling submodules via `super::…`.
use buffers::{
    navigate_buffer, register_buffer, save_current_special_buffer,
    stash_current_file_buffer, take_stashed_file_buffer, unsaved_buffer_names,
};
use format::run_shell_formatter;
use scroll::normalize_cursor_folds_directional;

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
// Public API
// ---------------------------------------------------------------------------

/// Switch to the named theme (built-in or user theme file), keeping the
/// `[theme]` config overrides applied on top.  The choice lasts for the
/// session; the message points at the config key that persists it.
pub fn apply_theme(app: &mut App, name: &str) {
    match crate::theme::load_and_set(name, &app.config.theme.overrides) {
        Ok(display) => {
            app.config.theme.name = name.to_string();
            app.messages.show(format!(
                "Theme: {display}  (persist with `name = \"{name}\"` under [theme] in :config)"
            ));
        }
        Err(e) => app.messages.show(format!("Theme error: {e}")),
    }
}

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
        Command::OpenThemePicker     => { pickers::theme_picker(app);      return; }
        Command::SwitchTheme(name)   => { apply_theme(app, name);          return; }

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
            app.messages.show(if app.config.editor.git_gutter {
                "Git gutter on"
            } else {
                "Git gutter off"
            });
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
                app.messages.show("Special buffer — nothing to save");
                return;
            }
            // format_on_save: try shell formatter first, then LSP.
            if app.notebook.is_none() && app.config.editor.format_on_save {
                if run_shell_formatter(app) {
                    // Shell formatter saved+formatted the file; show result and return.
                    if app.messages.current().is_none() {
                        app.messages.show(format!("Saved {}", app.buffer.display_name()));
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
                            app.messages.show(format!("Saved {name}"));
                        }
                        Err(e) => app.messages.show(format!("Error: {e}")),
                    }
                }
            } else {
                match app.buffer.save(None, force) {
                    Ok(()) => {
                        app.messages.show(format!("Saved {}", app.buffer.display_name()));
                        refresh_git(app);
                    }
                    Err(e) => app.messages.show(format!("Error: {e}")),
                }
            }
            return;
        }
        Command::WriteAs(_) if app.buffer.path.as_deref().map(is_special_path).unwrap_or(false) => {
            app.messages.show("Special buffer — nothing to save");
            return;
        }
        Command::WriteAs(path) => {
            let path = path.clone();
            match app.buffer.save(Some(&path), false) {
                Ok(()) => {
                    app.messages.show(format!("Saved {path}"));
                    refresh_git(app);
                }
                Err(e) => app.messages.show(format!("Error: {e}")),
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
                app.messages.show(format!(
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
                            app.messages.show(format!("Error: {e}"));
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
                        app.messages.show(format!("Error: {e}"));
                        false
                    }
                }
            };
            if saved {
                let unsaved = unsaved_buffer_names(app);
                if unsaved.is_empty() {
                    app.should_quit = true;
                } else {
                    app.messages.show(format!(
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
                app.messages.show(format!("Cannot close special buffer {name}"));
                return;
            }

            // Check for unsaved changes.
            let is_modified = if let Some((ref nb, _)) = app.notebook {
                nb.modified
            } else {
                app.buffer.modified
            };
            if is_modified && !force {
                app.messages.show(
                    "Buffer modified — save with :w or use :bd! to force close",
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

            app.messages.show("Buffer closed");
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
            app.messages.show(if app.config.editor.line_numbers {
                "Line numbers on"
            } else {
                "Line numbers off"
            });
            return;
        }
        Command::ToggleRelativeLineNumbers => {
            app.config.editor.relative_line_numbers = !app.config.editor.relative_line_numbers;
            app.messages.show(if app.config.editor.relative_line_numbers {
                "Relative line numbers on"
            } else {
                "Relative line numbers off"
            });
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
                    app.messages.show(msg);
                }
                Err(e) => app.messages.show(format!("shell error: {e}")),
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
        Command::NotebookExecuteAllCells => {
            notebook::execute_all_cells(app, false);
            return;
        }
        Command::NotebookExecuteCellsBelow => {
            notebook::execute_all_cells(app, true);
            return;
        }
        Command::ExportDocument(fmt) => {
            export::start_export(app, fmt);
            return;
        }
        Command::NotebookUndoStructural | Command::NotebookRedoStructural => {
            let redo = matches!(cmd, Command::NotebookRedoStructural);
            notebook::structural_history_step(app, redo);
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
                    app.messages.show("No formatter configured for this file type");
                    return;
                }
            };
            let path = match app.buffer.path.clone() {
                Some(p) => p,
                None => {
                    app.messages.show("Save the file before formatting");
                    return;
                }
            };
            if !app.lsp.is_ready(&lang) {
                app.messages.show("No formatter configured (add [formatters.python] to config, or wait for LSP)");
                return;
            }
            let tab_size = app.config.editor.tab_width;
            if !app.lsp.format_document(&lang, &path, tab_size, true) {
                app.messages.show("No formatter configured — add [formatters.<lang>] to your config");
            }
            return;
        }
        Command::OpenConfig => {
            let path = match crate::config::config_file_path() {
                Some(p) => p,
                None => {
                    app.messages.show("Could not determine config file path");
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
            crate::theme::init_from_config(&config);
            app.config = config;
            app.keymap = keymap;
            app.messages.show("Config reloaded");
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
            app.messages.show(if app.config.editor.word_wrap {
                "Word wrap on"
            } else {
                "Word wrap off"
            });
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
/// they are produced rather than only when the cell finishes. Also handles the
/// kernel-ready handshake and starts the next queued cell when the kernel
/// becomes idle. Returns true when state changed (the caller should redraw).
pub fn process_kernel_events(app: &mut App) -> bool {
    use crate::notebook::{append_stream, push_error_output, KernelMessage, KernelStatus, MimeData, Output};

    let mut refresh_images = false;
    let mut applied = false;
    // Status changes worth logging are collected and shown after the notebook
    // borrow ends; when several arrive in one frame the last (most recent)
    // wins the minibuffer and the log keeps them all.
    let mut announce: Vec<String> = Vec::new();
    if let Some((ref mut nb, ref mut state)) = app.notebook {
        if state.executing_cell.is_some() && nb.kernel.is_none() {
            state.executing_cell = None;
            state.executing_since = None;
            applied = true;
        }
        let msgs = match nb.kernel.as_mut() {
            Some(k) => k.poll(),
            None => Vec::new(),
        };
        applied |= !msgs.is_empty();
        for msg in msgs {
            // The executing cell, revalidated per message (Done/Dead clear it).
            let idx = state.executing_cell.filter(|&i| i < nb.cells.len());
            match msg {
                KernelMessage::Ready => {
                    if let Some(ref mut k) = nb.kernel {
                        if k.status == KernelStatus::Starting {
                            k.status = KernelStatus::Idle;
                        }
                        announce.push(format!("Kernel ready ({})", k.python));
                    }
                }
                KernelMessage::Stream { name, text } => {
                    if let Some(idx) = idx {
                        append_stream(&mut nb.cells[idx].outputs, &name, &text);
                    }
                }
                KernelMessage::Image { png } => {
                    if let Some(idx) = idx {
                        nb.cells[idx].outputs.push(Output::DisplayData {
                            data: MimeData { text_plain: None, image_png: Some(std::sync::Arc::new(png)) },
                        });
                        refresh_images = true;
                    }
                }
                KernelMessage::Error { traceback } => {
                    if let Some(idx) = idx {
                        push_error_output(&mut nb.cells[idx].outputs, &traceback);
                    }
                }
                KernelMessage::Done => {
                    if let Some(ref mut k) = nb.kernel {
                        k.execution_count += 1;
                        k.status = KernelStatus::Idle;
                        if let Some(idx) = idx {
                            nb.cells[idx].execution_count = Some(k.execution_count);
                        }
                    }
                    let elapsed = state.executing_since.take().map(|t| format_duration(t.elapsed()));
                    if let Some(idx) = idx {
                        let failed = nb.cells[idx].outputs.iter()
                            .any(|o| matches!(o, Output::Error { .. }));
                        let verb = if failed { "failed" } else { "finished" };
                        announce.push(match elapsed {
                            Some(t) => format!("Cell [{}] {verb} in {t}", idx + 1),
                            None => format!("Cell [{}] {verb}", idx + 1),
                        });
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
                    state.executing_since = None;
                    let dropped = state.exec_queue.len();
                    state.exec_queue.clear();
                    announce.push(if dropped > 0 {
                        format!("Kernel died — {dropped} queued cell(s) dropped (:restart-kernel)")
                    } else {
                        "Kernel died (:restart-kernel to restart)".to_string()
                    });
                    refresh_images = true;
                }
            }
        }
    }
    for msg in announce {
        app.messages.show(msg);
    }
    if refresh_images {
        app.graphics.image_ids.clear();
    }
    // The kernel may have just become idle (Ready/Done) — start the next
    // queued cell. Its "Running cell [N]…" takes over the minibuffer.
    applied |= notebook::pump_execution_queue(app);
    applied
}

/// Human-readable duration for the cell-completion log message.
fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
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
            app.messages.current().unwrap_or("").contains("dirty.txt"),
            "message should name the dirty buffer: {:?}",
            app.messages.current()
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
        assert!(app.messages.current().unwrap_or("").contains("changed on disk"));

        execute(&mut app, &Command::WriteForce);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "mine original\n",
            ":w! must overwrite"
        );

        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn notebook_stash_round_trip_preserves_cell_edits() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();

        let dir = unique_tmp_dir("nbstash");
        let nb_path = dir.join("roundtrip.ipynb");
        let txt = dir.join("side.txt");
        let _ = std::fs::remove_file(&nb_path);
        std::fs::write(&txt, "side\n").unwrap();
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "roundtrip");
        assert!(app.notebook.is_some());

        // Type into the focused cell (buffer mirrors the cell).
        app.buffer.insert(0, "x = 42");
        // Leave for a plain file (stashes the notebook), then come back.
        lsp::open_file_at(&mut app, &txt, 0, 0);
        assert!(app.notebook.is_none());
        open_as_notebook(&mut app, &nb_path);

        let (nb, _) = app.notebook.as_ref().unwrap();
        assert_eq!(nb.cells[0].source.to_string(), "x = 42");
        assert!(nb.modified, "unsaved notebook edit must survive the round trip");
        // …and the unsaved notebook must block :q from anywhere.
        lsp::open_file_at(&mut app, &txt, 0, 0);
        execute(&mut app, &Command::Quit);
        assert!(!app.should_quit);

        let _ = std::fs::remove_file(&nb_path);
        let _ = std::fs::remove_file(&txt);
    }

    #[test]
    fn force_closed_buffer_is_not_resurrected_into_stash() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();

        let dir = unique_tmp_dir("bdstash");
        let a = dir.join("doomed.txt");
        std::fs::write(&a, "x\n").unwrap();

        lsp::open_file_at(&mut app, &a, 0, 0);
        app.buffer.insert(0, "unsaved ");
        execute(&mut app, &Command::BufferForceClose);

        // The closed buffer must be gone from every stash; quit is clean.
        assert!(app.file_buffers.is_empty(), "closed buffer must not linger in stash");
        execute(&mut app, &Command::Quit);
        assert!(app.should_quit, "no unsaved buffers should remain after :bd!");

        let _ = std::fs::remove_file(&a);
    }

    /// End-to-end async execution: `:run-all` queues both cells, the kernel
    /// boots in the background, and `process_kernel_events` (the run-loop
    /// pump) runs them in order with a shared namespace, logging progress.
    #[test]
    fn async_kernel_executes_queued_cells_in_order() {
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 not available — skipping kernel integration test");
            return;
        }
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();

        let dir = unique_tmp_dir("kernelq");
        let target = dir.join("queue.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "queue");

        if let Some((ref mut nb, _)) = app.notebook {
            nb.cells[0].source = Rope::from_str("x = 1\nprint('first', x)");
            let mut second = nb.cells[0].clone();
            second.id = crate::notebook::new_cell_id();
            second.source = Rope::from_str("print('second', x + 1)");
            nb.cells.push(second);
        }
        notebook::load_focused_cell(&mut app);

        execute(&mut app, &Command::NotebookExecuteAllCells);
        // The kernel boots asynchronously — nothing has finished yet, but the
        // work must be queued (or already started) without blocking.
        {
            let (_, state) = app.notebook.as_ref().unwrap();
            assert!(
                state.executing_cell.is_some() || !state.exec_queue.is_empty(),
                "run-all must queue the code cells"
            );
        }

        // Drive the run-loop pump until both cells complete (or time out).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            process_kernel_events(&mut app);
            let (nb, state) = app.notebook.as_ref().unwrap();
            let kernel_dead = nb.kernel.as_ref()
                .map(|k| k.status == crate::notebook::KernelStatus::Dead)
                .unwrap_or(false);
            assert!(!kernel_dead, "kernel died during the test");
            let done = state.exec_queue.is_empty()
                && state.executing_cell.is_none()
                && nb.cells.iter().all(|c| c.execution_count.is_some());
            if done {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "kernel execution timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let stream_text = |cell: &crate::notebook::Cell| -> String {
            cell.outputs.iter().filter_map(|o| match o {
                crate::notebook::Output::Stream { text, .. } => Some(text.as_str()),
                _ => None,
            }).collect()
        };
        let (nb, _) = app.notebook.as_ref().unwrap();
        // Ran in order with a shared namespace: cell 1 saw cell 0's `x`.
        assert_eq!(nb.cells[0].execution_count, Some(1));
        assert_eq!(nb.cells[1].execution_count, Some(2));
        assert!(stream_text(&nb.cells[0]).contains("first 1"));
        assert!(stream_text(&nb.cells[1]).contains("second 2"));
        // The message log recorded the kernel lifecycle and cell completions.
        assert!(app.messages.log.iter().any(|m| m.starts_with("Kernel ready")));
        assert!(app.messages.log.iter().any(|m| m.contains("Cell [1] finished")));
        assert!(app.messages.log.iter().any(|m| m.contains("Cell [2] finished")));

        let _ = std::fs::remove_file(&target);
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
