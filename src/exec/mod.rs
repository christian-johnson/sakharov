mod buffers;
mod export;
mod format;
mod lsp;
pub(crate) mod notebook;
mod pickers;
mod scroll;
mod search;
pub(crate) mod table;
mod text;

pub use buffers::{is_special_path, open_as_notebook, open_path, switch_to_special_buffer};
pub use export::{poll_export, ExportJob};
pub(crate) use buffers::{create_new_file, create_new_notebook, SCRATCH_INTRO};
pub use lsp::{
    apply_code_action, jump_to_location, lsp_did_change, lsp_did_change_insert,
    lsp_did_change_remove, lsp_signature_help, process_lsp_events, pump_signature_help,
    refresh_completion_doc,
};
pub use scroll::{normalize_cursor_folds, update_scroll};
pub use table::{is_table_path, open_as_table, poll_table_load};
pub use search::{search_compute_matches, search_jump};

// Names used by `execute()` and by sibling submodules via `super::…`.
use buffers::{
    canon, navigate_buffer, register_buffer, save_current_special_buffer,
    take_stashed_file_buffer, teardown_current_buffer, unsaved_buffer_names,
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

/// Live-preview the theme currently selected in the theme picker, without
/// committing it (no `config.theme.name` update, no message).  Called after
/// every handled key while the picker is open, so scrolling the list restyles
/// the editor in real time.  Errors (e.g. an unparsable user theme file) are
/// ignored — the previous theme simply stays active.
pub fn preview_selected_theme(app: &mut App) {
    let Some(popup) = app.popup.as_ref() else { return };
    if popup.on_confirm != crate::popup::PopupTarget::SwitchTheme {
        return;
    }
    let crate::popup::PopupContent::List(ref state) = popup.content else { return };
    let Some(name) = state.selected_item().map(|it| it.label.clone()) else { return };
    let _ = crate::theme::load_and_set(&name, &app.config.theme.overrides);
}

/// Restore the committed theme (`config.theme.name`) after the theme picker
/// is dismissed without confirming a selection.
pub fn revert_theme_preview(app: &mut App) {
    let name = app.config.theme.name.clone();
    if let Err(e) = crate::theme::load_and_set(&name, &app.config.theme.overrides) {
        // The committed theme should always load (it loaded before the
        // preview); surface the anomaly rather than dying.
        app.messages.show(format!("Theme error: {e}"));
    }
}

/// Execute a single command against the application state.
pub fn execute(app: &mut App, cmd: &Command) {
    let extend = app.mode == Mode::Select;

    // The table view owns the whole screen and has no text buffer behind it, so
    // it intercepts the commands it implements (motions become cell movement)
    // and refuses the ones that would edit.  Everything else — `:q`, the
    // palette, `:theme`, buffer switching — falls through unchanged.
    if app.view() == crate::app::View::Table && table::handle(app, cmd) {
        return;
    }

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

    // While browsing a cell's output block (`output_row`), horizontal/word/
    // line motions move a read-only text cursor over the output itself (see
    // `output_motion`) instead of the hidden source — the output has no
    // horizontal *scroll*, but the cursor and an optional selection (Select
    // mode) still move within it, exactly like the plain buffer. j/k/paging
    // keep navigating the block below (handled further down).
    let browsing_output = app.notebook.as_ref().map(|(_, s)| s.output_row.is_some()).unwrap_or(false);
    if browsing_output {
        let handled = match cmd {
            Command::MoveLeft => Some(output_motion(app, extend, motion::move_left)),
            Command::MoveRight => Some(output_motion(app, extend, motion::move_right)),
            Command::MoveWordForward => Some(output_motion(app, extend, motion::move_word_forward)),
            Command::MoveWordBackward => Some(output_motion(app, extend, motion::move_word_backward)),
            Command::MoveWordEnd => Some(output_motion(app, extend, motion::move_word_end)),
            Command::MoveBigWordForward => Some(output_motion(app, extend, motion::move_big_word_forward)),
            Command::MoveBigWordBackward => Some(output_motion(app, extend, motion::move_big_word_backward)),
            Command::MoveBigWordEnd => Some(output_motion(app, extend, motion::move_big_word_end)),
            Command::MoveLineStart => Some(output_motion(app, extend, motion::move_line_start)),
            Command::MoveLineFirstNonWs => Some(output_motion(app, extend, motion::move_line_first_non_ws)),
            Command::MoveLineEnd => Some(output_motion(app, extend, motion::move_line_end)),
            _ => None,
        };
        if let Some(moved) = handled {
            if moved {
                update_scroll(app);
            }
            return;
        }
    }

    // Browsing a cell's output block (`output_row`) is a transient state owned
    // by continued vertical motion — including paging, which is just a run of
    // single steps — plus the commands above (handled and returned already)
    // and mode/yank transitions that operate on the output selection itself;
    // any other command snaps the cursor back to the cell source.
    if !matches!(
        cmd,
        Command::MoveUp | Command::MoveDown | Command::PageUp | Command::PageDown
            // Reads output_row to resolve which traceback frame the cursor is on.
            | Command::NotebookFollowError
            | Command::EnterSelect | Command::EnterNormal | Command::YankSelection
    ) {
        if let Some((_, state)) = app.notebook.as_mut() {
            state.clear_output_browsing();
        }
    }

    match cmd {
        // --- Motions ---
        Command::MoveLeft         => app.selection = motion::move_left(&app.buffer.rope, app.selection, extend),
        Command::MoveRight        => app.selection = motion::move_right(&app.buffer.rope, app.selection, extend),
        Command::MoveUp => {
            // In a notebook, vertical motion flows continuously: through the
            // focused cell's source, up through the previous cell's output
            // block, and into its source — see `notebook_move_up`. In Select
            // mode this only continues an *already active* output-text
            // selection (extending it row by row); it never starts one, so
            // Select-mode motion over the source is unaffected.
            let extending_output = extend && browsing_output;
            if (!extend || extending_output) && notebook_vertical(app) && notebook_move_up(app, extend) {
                return;
            }
            app.selection = motion::move_up(&app.buffer.rope, app.selection, extend);
        }
        Command::MoveDown => {
            // In a notebook, `j` past the last source line descends into the
            // cell's output block (so long errors/streams scroll into view),
            // then crosses into the next cell — see `notebook_move_down`. See
            // `MoveUp` above for the Select-mode caveat.
            let extending_output = extend && browsing_output;
            if (!extend || extending_output) && notebook_vertical(app) && notebook_move_down(app, extend) {
                return;
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
            app.popup = Some(crate::popup::Popup::which_key("g", goto_hints(app)));
            return;
        }
        Command::EnterJumpMode => {
            let extend = app.mode == Mode::Select;
            // Label only what is actually on screen.  In a notebook the
            // buffer is the focused *cell*, so its visible line range comes
            // from the cell-stack scroll anchor — `app.scroll_row` belongs to
            // the plain editor and would label from the top of the cell,
            // leaving a scrolled long cell with no visible labels at all.
            let (first_line, rows) = if app.in_notebook_nav() {
                scroll::notebook_visible_source_lines(app).unwrap_or((0, 0))
            } else {
                (app.scroll_row, app.viewport_height)
            };
            let positions = jump::visible_word_starts(&app.buffer.rope, first_line, rows);
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
            if browsing_output {
                if let Some(text) = yank_output_selection(app) {
                    let n = text.chars().count();
                    app.clipboard = text.clone();
                    crate::clipboard::write(&text);
                    app.messages.show(format!("Yanked {n} chars from output"));
                    if let Some((_, s)) = app.notebook.as_mut() {
                        s.output_anchor = None;
                    }
                    if app.mode == Mode::Select {
                        app.mode = Mode::Normal;
                    }
                    return;
                }
            }
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
            // Collapse (don't end) an active output-text selection: stay on
            // the same output row/col, just drop the anchor — mirrors how
            // Esc collapses `app.selection` above instead of leaving the cell.
            if let Some((_, s)) = app.notebook.as_mut() {
                s.output_anchor = None;
            }
            return;
        }
        Command::EnterSelect => {
            app.mode = Mode::Select;
            // Seed the output-selection anchor at the current position so a
            // `y` right after entering Select mode (before any motion) still
            // yanks — see `Command::YankSelection`.
            if let Some((_, s)) = app.notebook.as_mut() {
                if let Some(row) = s.output_row {
                    s.output_anchor = Some((row, s.output_col));
                }
            }
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
            let mut hints = vec![
                ("a".into(), "toggle fold at cursor".into()),
                ("A".into(), "toggle all folds".into()),
            ];
            if app.notebook.is_some() {
                hints.push(("o".into(), "expand/collapse full cell output".into()));
            }
            app.popup = Some(crate::popup::Popup::which_key("z", hints));
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
                // Flushes any in-progress cell edits into nb.cells before serialising.
                let result = notebook::save_notebook(app);
                report_save(app, result, |app| {
                    let name = app.notebook.as_ref()
                        .and_then(|(nb, _)| nb.path.file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("notebook.ipynb")
                        .to_string();
                    app.messages.show(format!("Saved {name}"));
                });
            } else {
                let result = app.buffer.save(None, force);
                report_save(app, result, |app| {
                    app.messages.show(format!("Saved {}", app.buffer.display_name()));
                    refresh_git(app);
                });
            }
            return;
        }
        Command::WriteAs(_) if app.buffer.path.as_deref().map(is_special_path).unwrap_or(false) => {
            app.messages.show("Special buffer — nothing to save");
            return;
        }
        Command::WriteAs(path) => {
            let path = path.clone();
            let result = app.buffer.save(Some(&path), false);
            report_save(app, result, |app| {
                app.messages.show(format!("Saved {path}"));
                refresh_git(app);
            });
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
                let result = notebook::save_notebook(app);
                report_save(app, result, |_| {})
            } else {
                let result = app.buffer.save(None, false);
                report_save(app, result, |_| {})
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

            // A `*cell …*` buffer is closed by going back to the table it was
            // read out of — the only place it makes sense to return to.
            if table::close_cell_buffer(app) {
                return;
            }

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
                let key = canon(p);
                app.open_buffers.retain(|stored| canon(stored) != key && stored != p);
                app.notebook_buffers.remove(&key);
                app.notebook_buffers.remove(p);
                app.file_buffers.remove(&key);
                app.file_buffers.remove(p);
                app.table_buffers.remove(&key);
                app.table_buffers.remove(p);
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

            buffers::open_path(app, &next);

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
        Command::NotebookGotoError => {
            goto_focused_cell_error(app);
            return;
        }
        Command::NotebookFollowError => {
            follow_output_error_link(app);
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
        Command::NotebookToggleOutputExpand => {
            let Some((nb, state)) = app.notebook.as_mut() else {
                app.messages.show("Not a notebook");
                return;
            };
            let idx = state.focused_cell;
            let has_output = nb.cells.get(idx).map(|c| !c.outputs.is_empty()).unwrap_or(false);
            if !has_output {
                app.messages.show("Cell has no output");
                return;
            }
            // The cursor may be parked deep in the output block; collapsing
            // shrinks it under them, so drop back to the source.
            state.output_row = None;
            let expanded = state.toggle_output_expand(idx);
            app.messages.show(if expanded {
                "Output expanded — j/k scrolls the whole block"
            } else {
                "Output collapsed"
            });
            update_scroll(app);
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
        // A page is just N vertical steps, so in a notebook it flows across
        // cells and through output blocks exactly like `j`/`k` — otherwise it
        // would stall at the edges of the focused cell. In Select mode it
        // extends the selection to the landing line, same as `j`/`k` (and, as
        // with them, stays inside the focused cell rather than crossing into
        // another cell's source, which the selection can't span).
        Command::PageDown => {
            let half = (app.viewport_height / 2).max(1);
            for _ in 0..half {
                let extending_output = extend && browsing_output;
                if (!extend || extending_output) && notebook_vertical(app) && notebook_move_down(app, extend) {
                    continue;
                }
                app.selection = motion::move_down(&app.buffer.rope, app.selection, extend);
            }
        }
        Command::PageUp => {
            let half = (app.viewport_height / 2).max(1);
            for _ in 0..half {
                let extending_output = extend && browsing_output;
                if (!extend || extending_output) && notebook_vertical(app) && notebook_move_up(app, extend) {
                    continue;
                }
                app.selection = motion::move_up(&app.buffer.rope, app.selection, extend);
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
            let Some((lang, path)) = lsp::require_lang_path(
                app,
                "No formatter configured for this file type",
                "Save the file before formatting",
            ) else {
                return;
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

        // --- Tabular data view ---
        // (In the table view these are handled by `table::handle` above.)
        Command::OpenAsTable => {
            table::open_current_as_table(app);
            return;
        }
        Command::TableClose => {
            app.messages.show("No table open");
            return;
        }
        Command::TableCloseCell => {
            if !table::close_cell_buffer(app) {
                app.messages.show("Not a table cell buffer");
            }
            return;
        }
        Command::TableOpenCell
        | Command::TablePeekCell
        | Command::TableYankCell
        | Command::TableYankRow => {
            app.messages.show("No table open");
            return;
        }
    }

    // If a motion landed the cursor inside a hidden fold, snap out direction-aware.
    normalize_cursor_folds_directional(app, pre_exec_line);
    update_scroll(app);
}

/// The which-key entries for the `g` sub-mode.
///
/// Every key listed here must be one `input::goto_command` dispatches (pinned
/// by `goto_hints_only_advertise_real_bindings`), and the labels describe what
/// the command does **in the current view** — the same `g h` that goes to the
/// first non-whitespace character in text goes to the first column in the grid,
/// and `g k` asks the LSP in a buffer but peeks the cell in a table.  A key that
/// would do nothing here is left out rather than advertised.
fn goto_hints(app: &App) -> Vec<(String, String)> {
    let hint = |k: &str, d: &str| (k.to_string(), d.to_string());

    if app.view() == crate::app::View::Table {
        return vec![
            hint("g", "first row"),
            hint("e", "last row"),
            hint("h", "first column"),
            hint("l", "last column"),
            hint("k", "peek cell text"),
            hint("b", "buffer picker"),
        ];
    }

    let mut hints = vec![
        hint("g", "go to file start"),
        hint("e", "go to file end"),
        hint("h", "go to line first non-whitespace"),
        hint("l", "go to line end"),
        hint("z", "scroll cursor to centre"),
        hint("w", "jump to label in view"),
        hint("b", "buffer picker"),
        hint("s", "symbol picker"),
        hint("c", "comment/uncomment selection"),
        hint("D", "diagnostic picker"),
    ];
    let lsp_active = app
        .current_language()
        .map(|l| app.lsp.is_ready(l))
        .unwrap_or(false);
    if lsp_active {
        hints.push(hint("a", "code actions  [LSP]"));
        hints.push(hint("k", "show documentation  [LSP]"));
        hints.push(hint("d", "go to definition  [LSP]"));
        hints.push(hint("r", "go to references  [LSP]"));
        hints.push(hint("y", "go to type definition  [LSP]"));
        hints.push(hint("i", "go to implementation  [LSP]"));
    }
    hints
}

/// True when vertical motion should flow through the notebook cell stack
/// rather than staying inside the buffer (i.e. a notebook is open and we're
/// not in the full-screen single-cell overlay).
fn notebook_vertical(app: &App) -> bool {
    app.in_notebook_nav()
}

/// Visual rows in cell `cell_idx`'s output block (0 for none), sized exactly
/// as the renderer draws it — including the cell's expand/collapse state, so
/// `j`/`k` reach every row of an expanded block.  Used by the output-block
/// navigation below.
fn nb_output_rows(app: &App, cell_idx: usize) -> usize {
    let cell_px = app.graphics.cell_pixel_size;
    let avail_cols = app.viewport_width.saturating_sub(2) as u16;
    let Some((nb, state)) = app.notebook.as_ref() else { return 0 };
    let limits = crate::notebook_ui::OutputLimits::new(
        &app.config.notebook, state.is_output_expanded(cell_idx),
    );
    nb.cells
        .get(cell_idx)
        .map(|cell| crate::notebook_ui::cell_output_rows(cell, limits, cell_px, avail_cols))
        .unwrap_or(0)
}

/// `j` inside a notebook: continue past the last source line into the cell's
/// output block, then into the next cell.  Returns true when it handled the
/// motion (the caller must not run the ordinary source `move_down`).
///
/// `extend` (Select mode) is only ever passed `true` when already browsing
/// output (see the `MoveDown` handler) — it extends the row-selection one
/// output row further but never crosses into the next cell, so a selection
/// stays within a single cell's output block.
fn notebook_move_down(app: &mut App, extend: bool) -> bool {
    let (focused, count) = match app.notebook.as_ref() {
        Some((nb, s)) => (s.focused_cell, nb.cells.len()),
        None => return false,
    };
    let output_row = app.notebook.as_ref().and_then(|(_, s)| s.output_row);

    if let Some(r) = output_row {
        let out_rows = nb_output_rows(app, focused);
        if r + 1 < out_rows {
            if let Some((_, s)) = app.notebook.as_mut() {
                s.output_row = Some(r + 1);
                s.output_col = 0;
                if !extend {
                    s.output_anchor = None;
                }
            }
            update_scroll(app);
            return true;
        }
        if extend {
            return true; // pinned at the last output row while selecting
        }
        // Past the last output row → next cell's first source line.
        if focused + 1 < count {
            if let Some((_, s)) = app.notebook.as_mut() {
                s.clear_output_browsing();
            }
            switch_focused_cell(app, focused + 1);
            place_cursor_at_line(app, 0, 0);
            update_scroll(app);
        }
        return true; // consumed even when pinned at the very bottom
    }

    // On the source: only the last line descends into the output block.
    let rope = &app.buffer.rope;
    let pos = app.selection.head.min(rope.len_chars());
    let on_last_line = rope.len_chars() == 0 || rope.char_to_line(pos) + 1 >= rope.len_lines();
    if !on_last_line {
        return false;
    }
    if nb_output_rows(app, focused) > 0 {
        if let Some((_, s)) = app.notebook.as_mut() {
            s.output_row = Some(0);
            s.output_col = 0;
            s.output_anchor = None;
        }
        update_scroll(app);
        return true;
    }
    // No outputs: cross straight into the next cell (column preserved).
    if focused + 1 < count {
        let col = motion::col_of(rope, pos);
        switch_focused_cell(app, focused + 1);
        place_cursor_at_line(app, 0, col);
        update_scroll(app);
        return true;
    }
    false
}

/// `k` inside a notebook: the inverse of [`notebook_move_down`] — climb the
/// output block back to the source, then up into the previous cell (landing on
/// its last output row when it has outputs, else its last source line). See
/// [`notebook_move_down`] for the `extend` (Select mode) semantics.
fn notebook_move_up(app: &mut App, extend: bool) -> bool {
    let focused = match app.notebook.as_ref() {
        Some((_, s)) => s.focused_cell,
        None => return false,
    };
    let output_row = app.notebook.as_ref().and_then(|(_, s)| s.output_row);

    if let Some(r) = output_row {
        if r > 0 {
            if let Some((_, s)) = app.notebook.as_mut() {
                s.output_row = Some(r - 1);
                s.output_col = 0;
                if !extend {
                    s.output_anchor = None;
                }
            }
            update_scroll(app);
            return true;
        }
        if extend {
            return true; // pinned at the first output row while selecting
        }
        // r == 0 climbs back onto the source (cursor already sits on the
        // last source line it descended from).
        if let Some((_, s)) = app.notebook.as_mut() {
            s.clear_output_browsing();
        }
        update_scroll(app);
        return true;
    }

    // On the source: only the first line crosses into the previous cell.
    let rope = &app.buffer.rope;
    let pos = app.selection.head.min(rope.len_chars());
    let on_first_line = rope.len_chars() == 0 || rope.char_to_line(pos) == 0;
    if !on_first_line || focused == 0 {
        return false;
    }
    let col = motion::col_of(rope, pos);
    switch_focused_cell(app, focused - 1);
    let last_line = app.buffer.rope.len_lines().saturating_sub(1);
    place_cursor_at_line(app, last_line, col);
    // Land in the previous cell's output block when it has one.
    let prev_out = nb_output_rows(app, focused - 1);
    if prev_out > 0 {
        if let Some((_, s)) = app.notebook.as_mut() {
            s.output_row = Some(prev_out - 1);
            s.output_col = 0;
            s.output_anchor = None;
        }
    }
    update_scroll(app);
    true
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

// ---------------------------------------------------------------------------
// Output-text navigation, selection, and yank — a read-only cursor over a
// cell's rendered output block (streams, results, tracebacks), addressed
// exactly like the plain buffer but backed by a virtual rope over the
// output text instead of `app.buffer.rope` (see `notebook_ui::output_rows_content`
// / `output_virtual_rope`). Reusing `motion::*` against that rope means
// h/l/w/b/e/0/^/$ and Select-mode extension all behave identically to the
// source buffer without re-implementing char/word boundaries a second time.
// ---------------------------------------------------------------------------

/// A line's content length excluding a trailing `\n`/`\r` — the same
/// trimming `motion.rs`'s internal `line_end_char` applies, so a column
/// derived here never spills past this row into the next one when added to
/// `line_to_char`.
fn rope_line_content_len(rope: &ropey::Rope, line_idx: usize) -> usize {
    let line = rope.line(line_idx);
    let n = line.len_chars();
    if n > 0 && matches!(line.char(n - 1), '\n' | '\r') { n - 1 } else { n }
}

/// Build the focused cell's output virtual rope (see
/// `notebook_ui::output_virtual_rope`) using exactly the geometry the
/// renderer used to lay it out, so char addresses agree with what's on
/// screen. `None` when no notebook is open or there's no focused cell.
fn focused_output_rope(app: &App) -> Option<ropey::Rope> {
    let (nb, state) = app.notebook.as_ref()?;
    let cell = nb.cells.get(state.focused_cell)?;
    let limits = crate::notebook_ui::OutputLimits::new(
        &app.config.notebook, state.is_output_expanded(state.focused_cell),
    );
    let cell_px = app.graphics.cell_pixel_size;
    let avail_cols = app.viewport_width.saturating_sub(2) as u16;
    Some(crate::notebook_ui::output_virtual_rope(cell, limits, cell_px, avail_cols))
}

/// Apply a `motion::*` function to the read-only output-text cursor exactly
/// as it would apply to `app.selection`: map `(output_row, output_col)` to a
/// char index into the focused cell's output virtual rope, run the motion,
/// map the result back. `extend` (Select mode) moves only the head,
/// establishing `output_anchor` as the fixed point (or keeping it already
/// set) — the same anchor/head contract `Selection` has, just addressed in
/// `(row, col)` instead of a flat char index. Returns whether it moved
/// anything (false only if output browsing ended between the caller's check
/// and here, which shouldn't happen in practice).
fn output_motion(
    app: &mut App,
    extend: bool,
    f: fn(&ropey::Rope, Selection, bool) -> Selection,
) -> bool {
    let Some(rope) = focused_output_rope(app) else { return false };
    let Some(row) = app.notebook.as_ref().and_then(|(_, s)| s.output_row) else { return false };
    let output_col = app.notebook.as_ref().map(|(_, s)| s.output_col).unwrap_or(0);
    let output_anchor = app.notebook.as_ref().and_then(|(_, s)| s.output_anchor);

    let last_line = rope.len_lines().saturating_sub(1);
    let to_char = |r: usize, c: usize| {
        let r = r.min(last_line);
        rope.line_to_char(r) + c.min(rope_line_content_len(&rope, r))
    };
    let head = to_char(row, output_col);
    let anchor = output_anchor.map(|(r, c)| to_char(r, c)).unwrap_or(head);

    let new_sel = f(&rope, Selection::new(anchor, head), extend);
    let to_rowcol = |pos: usize| {
        let pos = pos.min(rope.len_chars());
        let li = rope.char_to_line(pos);
        (li, pos - rope.line_to_char(li))
    };
    let (nr, nc) = to_rowcol(new_sel.head);
    if let Some((_, state)) = app.notebook.as_mut() {
        state.output_row = Some(nr);
        state.output_col = nc;
        state.output_anchor = if extend { Some(to_rowcol(new_sel.anchor)) } else { None };
    }
    true
}

/// `y` while browsing an active output-text selection: the text spanning
/// `output_anchor` to the current `(output_row, output_col)`, inclusive of
/// the char under the head/anchor extreme (matching `text::yank_selection`'s
/// `end()+1`, so even a zero-width "selection" yanks the one char under the
/// cursor). Returns `None` when not browsing output at all, so the caller
/// can fall back to the ordinary buffer yank.
fn yank_output_selection(app: &App) -> Option<String> {
    let rope = focused_output_rope(app)?;
    let (_, state) = app.notebook.as_ref()?;
    let row = state.output_row?;
    let last_line = rope.len_lines().saturating_sub(1);
    let to_char = |r: usize, c: usize| {
        let r = r.min(last_line);
        rope.line_to_char(r) + c.min(rope_line_content_len(&rope, r))
    };
    let head = to_char(row, state.output_col);
    let anchor = state.output_anchor.map(|(r, c)| to_char(r, c)).unwrap_or(head);
    let lo = anchor.min(head);
    let hi = (anchor.max(head) + 1).min(rope.len_chars());
    Some(rope.slice(lo..hi).to_string())
}

// ---------------------------------------------------------------------------
// Error-traceback navigation ("jump to the line that raised")
// ---------------------------------------------------------------------------

/// Resolve an [`ErrorFrame`] to a concrete `(cell index, 0-based line)` in the
/// currently-open notebook, preferring the stable cell id and falling back to
/// the 1-based cell number printed in the label.
fn resolve_error_frame(
    nb: &crate::notebook::Notebook,
    frame: &crate::notebook::ErrorFrame,
) -> Option<(usize, usize)> {
    let idx = match &frame.cell_id {
        Some(id) => nb.cells.iter().position(|c| &c.id == id)?,
        None => frame.cell_number.checked_sub(1)?,
    };
    (idx < nb.cells.len()).then_some((idx, frame.line))
}

/// Move the cursor to `(cell_idx, line)` in the open notebook: focus that cell,
/// leave the output block, land on the line's first non-whitespace column, and
/// re-anchor the scroll. Shared by `:goto-error` and Enter-on-a-frame.
fn jump_to_notebook_cell_line(app: &mut App, cell_idx: usize, line: usize) {
    if app.notebook.as_ref().map(|(_, s)| s.focused_cell) != Some(cell_idx) {
        switch_focused_cell(app, cell_idx);
    }
    if let Some((_, s)) = app.notebook.as_mut() {
        s.output_row = None;
    }
    let rope = &app.buffer.rope;
    let li = line.min(rope.len_lines().saturating_sub(1));
    let start = rope.line_to_char(li);
    app.selection = motion::move_line_first_non_ws(rope, Selection::point(start), false);
    update_scroll(app);
}

/// `:goto-error` — jump to the source line of the focused cell's error. Targets
/// the *innermost* frame (the last `File` line — where the exception actually
/// raised), which may be another cell when the culprit is a function defined
/// elsewhere.
fn goto_focused_cell_error(app: &mut App) {
    let target = app.notebook.as_ref().and_then(|(nb, state)| {
        let cell = nb.cells.get(state.focused_cell)?;
        cell.outputs.iter().rev().find_map(|o| match o {
            crate::notebook::Output::Error { frames, .. } => {
                resolve_error_frame(nb, frames.last()?)
            }
            _ => None,
        })
    });
    match target {
        Some((idx, line)) => {
            jump_to_notebook_cell_line(app, idx, line);
            app.messages.show(format!("Jumped to cell [{}] line {}", idx + 1, line + 1));
        }
        None => app.messages.show("No navigable error in this cell"),
    }
}

/// Enter on a traceback frame line (while browsing the output block with `j`/`k`)
/// — jump to the exact source line that frame names. A no-op when the output
/// cursor isn't on a navigable frame.
fn follow_output_error_link(app: &mut App) {
    let cell_px = app.graphics.cell_pixel_size;
    let avail_cols = app.viewport_width.saturating_sub(2) as u16;
    let target = app.notebook.as_ref().and_then(|(nb, state)| {
        let orow = state.output_row?;
        let idx = state.focused_cell;
        let cell = nb.cells.get(idx)?;
        let limits = crate::notebook_ui::OutputLimits::new(
            &app.config.notebook, state.is_output_expanded(idx),
        );
        let frame = crate::notebook_ui::error_frame_at_output_row(
            cell, orow, limits, cell_px, avail_cols,
        )?;
        resolve_error_frame(nb, frame)
    });
    if let Some((idx, line)) = target {
        jump_to_notebook_cell_line(app, idx, line);
        app.messages.show(format!("Jumped to cell [{}] line {}", idx + 1, line + 1));
    }
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

/// Report the outcome of a save: on success runs `on_ok` (typically a
/// "Saved …" message, possibly plus a git refresh) and returns `true`; on
/// failure shows "Error: {e}" and returns `false`.
pub(crate) fn report_save<E: std::fmt::Display>(
    app: &mut App,
    result: Result<(), E>,
    on_ok: impl FnOnce(&mut App),
) -> bool {
    match result {
        Ok(()) => {
            on_ok(app);
            true
        }
        Err(e) => {
            app.messages.show(format!("Error: {e}"));
            false
        }
    }
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
    use crate::notebook::{append_stream, KernelMessage, KernelStatus, MimeData, Output};

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
                        // Build against the whole cell list so `File "<id>"`
                        // frames resolve to jump targets; then push.
                        let out = crate::notebook::build_error_output(&traceback, &nb.cells);
                        nb.cells[idx].outputs.push(out);
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
                        // On failure, point at the jump-to-line affordances.
                        let hint = if failed { " — :goto-error (or ↵ on a File line) to jump" } else { "" };
                        announce.push(match elapsed {
                            Some(t) => format!("Cell [{}] {verb} in {t}{hint}", idx + 1),
                            None => format!("Cell [{}] {verb}{hint}", idx + 1),
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

    /// The which-key popup is a promise about what the next keypress does, so
    /// every key it advertises must be one the `g` sub-mode actually
    /// dispatches — in every view, since the table view has its own list.
    #[test]
    fn goto_hints_only_advertise_real_bindings() {
        let mut app = App::new(None, Config::load()).unwrap();
        for hints in [goto_hints(&app), {
            app.table = Some(crate::exec::table::Session {
                source: Box::new(
                    crate::table::csv::CsvSource::from_reader(
                        "a,b\n1,2\n".as_bytes(),
                        b',',
                        &crate::config::TableConfig::default(),
                    )
                    .unwrap(),
                ),
                state: crate::table::TableState::new(),
                path: std::path::PathBuf::from("t.csv"),
            });
            goto_hints(&app)
        }] {
            assert!(!hints.is_empty());
            for (key, label) in hints {
                let c = key.chars().next().expect("hint key is a char");
                assert!(
                    crate::input::goto_command(c).is_some(),
                    "g{key} is advertised as {label:?} but dispatches nothing"
                );
            }
        }
    }

    /// `gk` peeks the cell in a table, so it must be listed there — the hint
    /// list used to gate `k` on an active LSP, which a table never has.
    #[test]
    fn the_table_views_goto_hints_include_the_cell_peek() {
        let mut app = App::new(None, Config::load()).unwrap();
        app.table = Some(crate::exec::table::Session {
            source: Box::new(
                crate::table::csv::CsvSource::from_reader(
                    "a,b\n1,2\n".as_bytes(),
                    b',',
                    &crate::config::TableConfig::default(),
                )
                .unwrap(),
            ),
            state: crate::table::TableState::new(),
            path: std::path::PathBuf::from("t.csv"),
        });
        let hints = goto_hints(&app);
        assert!(hints.iter().any(|(k, d)| k == "k" && d.contains("peek")));
        // And nothing that would do nothing here.
        assert!(!hints.iter().any(|(k, _)| k == "s" || k == "w"));
    }

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

    /// `gc` anchors the comment markers to the shallowest indent in the region,
    /// so commenting a function body keeps the block indented (and round-trips).
    #[test]
    fn comment_region_is_indent_aware() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.buffer.path = Some(std::path::PathBuf::from("indent_test.py"));
        app.lsp_language = Some("python".to_string());
        let src = "def f():\n    a = 1\n\n    if a:\n        b = 2\n";
        app.buffer.rope = Rope::from_str(src);

        // Select the body (lines 1..=4), leaving the `def` line alone.
        let start = app.buffer.rope.line_to_char(1);
        let end = app.buffer.rope.line_to_char(5) - 1;
        app.selection = Selection::new(start, end);
        execute(&mut app, &Command::CommentRegion);

        assert_eq!(
            app.buffer.rope.to_string(),
            "def f():\n    # a = 1\n\n    # if a:\n    #     b = 2\n",
            "markers sit at the region's shallowest indent, relative indent preserved"
        );

        // Uncommenting restores the original exactly.
        execute(&mut app, &Command::CommentRegion);
        assert_eq!(app.buffer.rope.to_string(), src, "comment/uncomment round-trips");
    }

    /// A bracketed paste is inserted verbatim — no auto-indent on the embedded
    /// newlines, which is what used to staircase a pasted block to the right.
    #[test]
    fn bracketed_paste_inserts_verbatim() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.buffer.path = Some(std::path::PathBuf::from("paste_test.py"));
        app.buffer.rope = Rope::from_str("def f():\n    ");
        app.selection = Selection::point(app.buffer.rope.len_chars());
        app.mode = Mode::Insert;

        crate::input::handle_paste(&mut app, "if a:\r\n    b = 2\r\n");
        assert_eq!(
            app.buffer.rope.to_string(),
            "def f():\n    if a:\n    b = 2\n",
            "pasted lines keep their own indentation and gain none"
        );
        assert_eq!(app.selection.head, app.buffer.rope.len_chars());

        // Outside Insert, a paste replaces the selection (like `P`).
        app.mode = Mode::Normal;
        app.buffer.rope = Rope::from_str("abcd");
        app.selection = Selection::new(1, 2);
        crate::input::handle_paste(&mut app, "XY");
        assert_eq!(app.buffer.rope.to_string(), "aXYd");
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

    /// A notebook cell's `app.buffer.path` is a virtual location
    /// (`{notebook}__cellN.py`) inside the notebook's own directory — it does
    /// not exist on disk. The shell-formatter path used to `Buffer::save` to
    /// that path unconditionally, littering the notebook's directory with a
    /// real leftover file on every `:fmt`. It must format through a scratch
    /// file instead and leave the notebook's directory untouched.
    #[test]
    fn shell_formatter_does_not_leave_stray_file_in_notebook_dir() {
        let mut config = Config::load();
        config.formatters.insert(
            "python".to_string(),
            crate::config::FormatterConfig {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "tr 'a-z' 'A-Z' < \"$0\" > \"$0.up\" && mv \"$0.up\" \"$0\"".to_string(),
                ],
            },
        );
        let mut app = App::new(None, config).unwrap();

        let dir = unique_tmp_dir("fmt-notebook");
        let nb_path = dir.join("analysis.ipynb");
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "analysis");
        assert!(app.notebook.is_some(), "setup: notebook should be open");

        app.buffer.rope = Rope::from_str("hello world");

        let handled = run_shell_formatter(&mut app);
        assert!(handled, "a configured formatter must be attempted");
        assert_eq!(
            app.buffer.rope.to_string(),
            "HELLO WORLD",
            "formatted content should be reloaded into the buffer"
        );

        let stray_entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| n != nb_path.file_name().unwrap() && n != "anchor.txt")
            .collect();
        assert!(
            stray_entries.is_empty(),
            "formatting a notebook cell must not leave files behind in its directory, found: {stray_entries:?}"
        );

        let _ = std::fs::remove_file(&nb_path);
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

    /// End-to-end: a cell that raises produces an error whose traceback frame
    /// resolves to the exact cell + line, `:goto-error` jumps there, and the
    /// runner's linecache registration surfaces the offending source line.
    #[test]
    fn kernel_error_frame_resolves_and_goto_error_jumps() {
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 not available — skipping kernel error-nav test");
            return;
        }
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.viewport_height = 20;
        app.viewport_width = 80;

        let dir = unique_tmp_dir("errnav");
        let target = dir.join("err.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "err");

        // The IndexError is raised on line 3 (0-based line 2) of the cell.
        if let Some((ref mut nb, _)) = app.notebook {
            nb.cells[0].source =
                Rope::from_str("data = [1, 2, 3]\nmid = len(data) // 2\nprint(data[99])");
        }
        notebook::load_focused_cell(&mut app);

        execute(&mut app, &Command::NotebookExecuteCell);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            process_kernel_events(&mut app);
            let (nb, state) = app.notebook.as_ref().unwrap();
            assert!(
                nb.kernel.as_ref().map(|k| k.status != crate::notebook::KernelStatus::Dead).unwrap_or(true),
                "kernel died during the test",
            );
            if state.exec_queue.is_empty()
                && state.executing_cell.is_none()
                && nb.cells[0].execution_count.is_some()
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "kernel error test timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // The error output carries a navigable frame → cell 0, line 2.
        {
            let (nb, _) = app.notebook.as_ref().unwrap();
            let frames = nb.cells[0].outputs.iter().find_map(|o| match o {
                crate::notebook::Output::Error { frames, traceback, .. } => {
                    // linecache made the offending source line show inline.
                    assert!(
                        traceback.iter().any(|l| l.contains("data[99]")),
                        "traceback should include the offending source line",
                    );
                    // The ugly compile id was relabelled to a friendly cell number.
                    assert!(traceback.iter().any(|l| l.contains("Cell [1]")));
                    Some(frames.clone())
                }
                _ => None,
            }).expect("cell must have an error output");
            let inner = frames.last().expect("a navigable frame");
            assert_eq!(resolve_error_frame(nb, inner), Some((0, 2)));
        }

        // Move the cursor off the culprit line, then `:goto-error` returns to it.
        app.selection = Selection::point(0);
        execute(&mut app, &Command::NotebookGotoError);
        let line = {
            let rope = &app.buffer.rope;
            rope.char_to_line(app.selection.head)
        };
        assert_eq!(line, 2, ":goto-error must land on the raising line");

        let _ = std::fs::remove_file(&target);
    }

    /// `j`/`k` traverse a cell's output block (so long errors scroll into
    /// view) and cross cleanly into neighbouring cells, and the row-granular
    /// scroll keeps the browsed row on screen.
    #[test]
    fn notebook_output_block_navigation() {
        use crate::notebook::Output;
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.viewport_height = 12;
        app.viewport_width = 80;

        let dir = unique_tmp_dir("outnav");
        let target = dir.join("outnav.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "outnav");

        if let Some((ref mut nb, ref mut state)) = app.notebook {
            nb.cells[0].source = Rope::from_str("a\nb");
            // Error output: 1 headline row + 3 traceback rows = 4 output rows.
            nb.cells[0].outputs = vec![Output::Error {
                frames: vec![],
                ename: "ValueError".into(),
                evalue: "boom".into(),
                traceback: vec!["tb1".into(), "tb2".into(), "tb3".into()],
            }];
            let mut second = nb.cells[0].clone();
            second.id = crate::notebook::new_cell_id();
            second.source = Rope::from_str("c\nd");
            second.outputs = vec![];
            nb.cells.push(second);
            state.focused_cell = 0;
        }
        notebook::load_focused_cell(&mut app);

        // Cursor on the last source line, then `j` descends into the outputs.
        app.selection = Selection::point(2); // the 'b'
        for expected in 0..4 {
            execute(&mut app, &Command::MoveDown);
            assert_eq!(
                app.notebook.as_ref().unwrap().1.output_row,
                Some(expected),
                "j should step through output row {expected}"
            );
            assert_eq!(app.notebook.as_ref().unwrap().1.focused_cell, 0);
        }
        // One more `j` crosses into cell 1's source (output browsing ends).
        execute(&mut app, &Command::MoveDown);
        assert_eq!(app.notebook.as_ref().unwrap().1.focused_cell, 1);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_row, None);

        // `k` climbs back into cell 0's output block at its last row…
        execute(&mut app, &Command::MoveUp);
        assert_eq!(app.notebook.as_ref().unwrap().1.focused_cell, 0);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_row, Some(3));
        // …then up through the outputs and back onto the source.
        for expected in [2usize, 1, 0] {
            execute(&mut app, &Command::MoveUp);
            assert_eq!(app.notebook.as_ref().unwrap().1.output_row, Some(expected));
        }
        execute(&mut app, &Command::MoveUp);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_row, None,
            "k off the top of the output block returns to the source");

        // A horizontal motion is swallowed while browsing output — the output
        // is read-only and doesn't scroll sideways, so h/l/w/0/$ keep the
        // cursor in the block instead of snapping back to the (hidden) source.
        app.selection = Selection::point(2);
        execute(&mut app, &Command::MoveDown); // into output_row 0
        assert_eq!(app.notebook.as_ref().unwrap().1.output_row, Some(0));
        let sel_before = app.selection;
        execute(&mut app, &Command::MoveLineStart);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_row, Some(0),
            "a horizontal motion stays in the output block");
        execute(&mut app, &Command::MoveRight);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_row, Some(0));
        assert_eq!(app.selection, sel_before, "the source cursor must not move");
        // A genuinely different command still snaps back to the source.
        execute(&mut app, &Command::SelectLine);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_row, None,
            "a non-motion command resets output browsing");

        let _ = std::fs::remove_file(&target);
    }

    /// Character/word/line motions over output text move a real column
    /// within the row — `w`/`$`/`0`/`h`/`l` address the output row's own
    /// content exactly like the plain buffer addresses a source line,
    /// rather than being pure no-ops.
    #[test]
    fn output_text_supports_char_and_word_motion() {
        use crate::notebook::Output;
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.viewport_height = 12;
        app.viewport_width = 80;

        let dir = unique_tmp_dir("outmotion");
        let target = dir.join("outmotion.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "outmotion");

        if let Some((ref mut nb, ref mut state)) = app.notebook {
            nb.cells[0].source = Rope::from_str("a");
            nb.cells[0].outputs = vec![Output::Stream {
                name: "stdout".into(),
                text: "hello world\n".into(),
            }];
            state.focused_cell = 0;
        }
        notebook::load_focused_cell(&mut app);

        app.selection = Selection::point(0);
        execute(&mut app, &Command::MoveDown); // into output row 0
        assert_eq!(app.notebook.as_ref().unwrap().1.output_row, Some(0));
        assert_eq!(app.notebook.as_ref().unwrap().1.output_col, 0);

        execute(&mut app, &Command::MoveWordForward);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_col, 6, "w lands on 'world'");

        execute(&mut app, &Command::MoveLineEnd);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_col, 10, "$ lands on the last char");

        execute(&mut app, &Command::MoveLineStart);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_col, 0);

        execute(&mut app, &Command::MoveRight);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_col, 1);
        execute(&mut app, &Command::MoveLeft);
        assert_eq!(app.notebook.as_ref().unwrap().1.output_col, 0);

        // Still anchored in the output block throughout — none of this
        // touched the (hidden) source cursor's cell.
        assert_eq!(app.notebook.as_ref().unwrap().1.output_row, Some(0));
        assert_eq!(app.notebook.as_ref().unwrap().1.focused_cell, 0);

        let _ = std::fs::remove_file(&target);
    }

    /// Selecting inside output text (Select mode entered while browsing
    /// output) and yanking copies exactly the selected span to the
    /// clipboard, distinct from the buffer's own yank path, and leaves the
    /// cursor exactly where it was in the output block.
    #[test]
    fn output_text_selection_yanks_to_clipboard() {
        use crate::notebook::Output;
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.viewport_height = 12;
        app.viewport_width = 80;

        let dir = unique_tmp_dir("outyank");
        let target = dir.join("outyank.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "outyank");

        if let Some((ref mut nb, ref mut state)) = app.notebook {
            nb.cells[0].source = Rope::from_str("a");
            nb.cells[0].outputs = vec![Output::Stream {
                name: "stdout".into(),
                text: "hello world\n".into(),
            }];
            state.focused_cell = 0;
        }
        notebook::load_focused_cell(&mut app);

        app.selection = Selection::point(0);
        execute(&mut app, &Command::MoveDown); // into output row 0, col 0
        execute(&mut app, &Command::EnterSelect);
        assert_eq!(app.mode, Mode::Select);
        execute(&mut app, &Command::MoveWordEnd); // extend to the end of "hello"
        execute(&mut app, &Command::YankSelection);

        assert_eq!(app.clipboard, "hello");
        assert_eq!(app.mode, Mode::Normal, "yank returns to Normal mode");
        assert_eq!(
            app.notebook.as_ref().unwrap().1.output_row, Some(0),
            "yanking output text stays in the output block"
        );
        assert!(app.notebook.as_ref().unwrap().1.output_anchor.is_none(),
            "yank collapses the selection");

        let _ = std::fs::remove_file(&target);
    }

    /// `gw` labels must cover what is *on screen*.  In a cell taller than the
    /// viewport the top of the cell is scrolled off, and labelling from line 0
    /// put every label above the viewport — the user saw no labels at all.
    #[test]
    fn jump_labels_follow_the_visible_slice_of_a_scrolled_cell() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.viewport_height = 10;
        app.viewport_width = 80;
        app.config.editor.scroll_off = 2;

        let dir = unique_tmp_dir("nbjump");
        let target = dir.join("nbjump.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "nbjump");

        // One cell far taller than the viewport, one labellable word per line.
        let src: String = (0..60).map(|i| format!("word{i}")).collect::<Vec<_>>().join("\n");
        if let Some((ref mut nb, ref mut state)) = app.notebook {
            nb.cells[0].source = Rope::from_str(&src);
            state.focused_cell = 0;
        }
        notebook::load_focused_cell(&mut app);

        // Scroll deep into the cell.
        app.selection = Selection::point(app.buffer.rope.line_to_char(50));
        update_scroll(&mut app);
        assert!(app.notebook.as_ref().unwrap().1.scroll_offset > 0, "cell must be scrolled");

        execute(&mut app, &Command::EnterJumpMode);
        assert!(!app.jump.labels.is_empty(), "a scrolled cell must still get labels");

        // Every label lies inside the on-screen slice, and never more labels
        // than the viewport has rows.
        let (first, count) = scroll::notebook_visible_source_lines(&app).unwrap();
        assert!(count > 0);
        for (pos, _) in &app.jump.labels {
            let line = app.buffer.rope.char_to_line(*pos);
            assert!(
                (first..first + count).contains(&line),
                "label on line {line} is outside the visible range {first}..{}",
                first + count
            );
        }
        // The cursor's own line is one of them (it is on screen by definition).
        assert!(app.jump.labels.iter().any(|(p, _)| app.buffer.rope.char_to_line(*p) == 50));

        let _ = std::fs::remove_file(&target);
    }

    /// Long output is capped by `max_output_lines`, but expanding a cell makes
    /// every line a real, navigable row so `j` can scroll through the lot.
    #[test]
    fn expanded_output_exposes_every_row_to_navigation() {
        use crate::notebook::Output;
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.viewport_height = 12;
        app.viewport_width = 80;
        let cap = app.config.notebook.max_output_lines;

        let dir = unique_tmp_dir("nbexpand");
        let target = dir.join("nbexpand.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "nbexpand");

        let total = cap * 3;
        if let Some((ref mut nb, ref mut state)) = app.notebook {
            nb.cells[0].source = Rope::from_str("a");
            nb.cells[0].outputs = vec![Output::Stream {
                name: "stdout".into(),
                text: (0..total).map(|i| format!("out{i}\n")).collect(),
            }];
            state.focused_cell = 0;
        }
        notebook::load_focused_cell(&mut app);

        // Collapsed: the cap plus one "… N more lines" indicator row.
        assert_eq!(nb_output_rows(&app, 0), cap + 1);

        execute(&mut app, &Command::NotebookToggleOutputExpand);
        assert!(app.notebook.as_ref().unwrap().1.is_output_expanded(0));
        assert_eq!(nb_output_rows(&app, 0), total, "expanding reveals every line");

        // `j` walks from the source through all of them.
        app.selection = Selection::point(0);
        for expected in 0..total {
            execute(&mut app, &Command::MoveDown);
            assert_eq!(
                app.notebook.as_ref().unwrap().1.output_row,
                Some(expected),
                "j should reach output row {expected} of an expanded block"
            );
        }

        // Collapsing again returns the cursor to the source (its row is gone).
        execute(&mut app, &Command::NotebookToggleOutputExpand);
        assert!(!app.notebook.as_ref().unwrap().1.is_output_expanded(0));
        assert_eq!(app.notebook.as_ref().unwrap().1.output_row, None);

        let _ = std::fs::remove_file(&target);
    }

    /// Paging in a notebook is a run of vertical steps, so it flows across
    /// cells and through output blocks instead of stalling at the cell edges.
    #[test]
    fn page_down_crosses_notebook_cells() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.viewport_height = 12; // half page = 6 steps
        app.viewport_width = 80;

        let dir = unique_tmp_dir("nbpage");
        let target = dir.join("nbpage.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "nbpage");

        if let Some((ref mut nb, ref mut state)) = app.notebook {
            nb.cells[0].source = Rope::from_str("a\nb\nc");
            let mut second = nb.cells[0].clone();
            second.id = crate::notebook::new_cell_id();
            second.source = Rope::from_str("d\ne\nf");
            nb.cells.push(second);
            state.focused_cell = 0;
        }
        notebook::load_focused_cell(&mut app);

        app.selection = Selection::point(0);
        execute(&mut app, &Command::PageDown);
        assert_eq!(app.notebook.as_ref().unwrap().1.focused_cell, 1,
            "a page down must carry past the end of the first cell");

        execute(&mut app, &Command::PageUp);
        assert_eq!(app.notebook.as_ref().unwrap().1.focused_cell, 0,
            "a page up must carry back into the previous cell");

        let _ = std::fs::remove_file(&target);
    }

    /// Paging in Select mode extends the selection to the landing line — it is
    /// `j`/`k` by the page, so it must not collapse the selection.
    #[test]
    fn page_scroll_extends_selection_in_select_mode() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.viewport_height = 8; // half page = 4 steps
        app.viewport_width = 80;
        app.buffer.rope = Rope::from_str("l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\n");

        app.selection = Selection::point(0);
        app.mode = Mode::Select;
        execute(&mut app, &Command::PageDown);
        assert_eq!(app.selection.anchor, 0, "the anchor must stay put");
        assert_eq!(app.buffer.rope.char_to_line(app.selection.head), 4);

        execute(&mut app, &Command::PageUp);
        assert_eq!(app.selection.anchor, 0);
        assert_eq!(app.buffer.rope.char_to_line(app.selection.head), 0);

        // In Normal mode a page is still a plain cursor move (selection collapses).
        app.mode = Mode::Normal;
        app.selection = Selection::point(0);
        execute(&mut app, &Command::PageDown);
        assert_eq!(app.selection.anchor, app.selection.head,
            "a page in Normal mode leaves no selection behind");
    }

    /// Saving a notebook clears the focused cell's buffer-modified flag too —
    /// otherwise the next keystroke propagates it back onto the notebook and
    /// the status line shows `[+]` on a file that is clean on disk.
    #[test]
    fn notebook_save_clears_the_modified_indicator() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.viewport_height = 24;
        app.viewport_width = 80;

        let dir = unique_tmp_dir("nbsave");
        let target = dir.join("nbsave.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "nbsave");

        app.buffer.insert(0, "x = 1");
        assert!(app.buffer.modified);

        execute(&mut app, &Command::Write);
        assert!(!app.notebook.as_ref().unwrap().0.modified, "notebook is clean after :w");
        assert!(!app.buffer.modified, "the focused cell's buffer is clean after :w");

        // Every keystroke flushes the buffer back into the cell (input.rs does
        // this via `sync_buffer_to_notebook`); that must not resurrect the flag.
        execute(&mut app, &Command::MoveRight);
        notebook::save_focused_cell(&mut app);
        assert!(!app.notebook.as_ref().unwrap().0.modified,
            "moving the cursor must not mark a saved notebook modified");

        let _ = std::fs::remove_file(&target);
    }

    /// The notebook scroll anchor is row-granular: paging down a tall cell
    /// advances `scroll_offset` one row at a time rather than jumping cells.
    #[test]
    fn notebook_scroll_is_row_granular() {
        let config = Config::load();
        let mut app = App::new(None, config).unwrap();
        app.viewport_height = 8;
        app.viewport_width = 80;
        app.config.editor.scroll_off = 2;

        let dir = unique_tmp_dir("rowscroll");
        let target = dir.join("rowscroll.ipynb");
        let _ = std::fs::remove_file(&target);
        app.buffer.path = Some(dir.join("anchor.txt"));
        create_new_notebook(&mut app, "rowscroll");

        // One tall cell: 30 source lines, taller than the 8-row viewport.
        let src: String = (0..30).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        if let Some((ref mut nb, ref mut state)) = app.notebook {
            nb.cells[0].source = Rope::from_str(&src);
            state.focused_cell = 0;
        }
        notebook::load_focused_cell(&mut app);
        update_scroll(&mut app);
        assert_eq!(app.notebook.as_ref().unwrap().1.scroll_offset, 0);

        // Walk the cursor down; scroll_offset must climb gradually (never in a
        // single whole-cell jump) and stay bounded by the cursor position.
        let mut last = 0usize;
        for _ in 0..25 {
            execute(&mut app, &Command::MoveDown);
            let off = app.notebook.as_ref().unwrap().1.scroll_offset;
            assert!(off >= last, "scroll must not jump backwards while moving down");
            assert!(off <= last + 1, "scroll must advance one row at a time (was {last}, now {off})");
            last = off;
        }
        assert!(last > 0, "scrolling a tall cell must move the anchor");

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
