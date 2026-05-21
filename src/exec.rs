use ropey::Rope;

use crate::{
    app::{language_for_path, App},
    buffer::Buffer,
    command::Command,
    highlight::Highlighter,
    lang::lang_to_ext,
    lsp::{lsp_pos_to_char, path_to_uri, NotebookCell},
    lsp_manager::{LspEvent, LspLocation, LspRequestKind},
    mode::{FindDir, Mode},
    motion,
    notebook::{Cell, CellType},
    selection::Selection,
    symbols,
};

// Helper: resolve the notebook's parent directory, falling back to cwd.
fn notebook_dir(path: &std::path::Path) -> std::path::PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
}

/// Execute a single command against the application state.
pub fn execute(app: &mut App, cmd: &Command) {
    let extend = app.mode == Mode::Select;

    match cmd {
        // --- Motions (extend = true in Select mode) ---
        Command::MoveLeft         => app.selection = motion::move_left(&app.buffer.rope, app.selection, extend),
        Command::MoveRight        => app.selection = motion::move_right(&app.buffer.rope, app.selection, extend),
        Command::MoveUp           => app.selection = motion::move_up(&app.buffer.rope, app.selection, extend),
        Command::MoveDown         => app.selection = motion::move_down(&app.buffer.rope, app.selection, extend),
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
        Command::OpenCommandPalette => {
            app.popup = Some(crate::popup::Popup::command_palette(
                crate::popup::command_palette_items(),
            ));
            return;
        }
        Command::GrepBuffer => {
            let rope = &app.buffer.rope;
            let path = app.buffer.path.clone().unwrap_or_default();
            let items: Vec<crate::popup::ListItem> = rope
                .lines()
                .enumerate()
                .map(|(line_idx, line)| {
                    let line_str = line.to_string();
                    let label = line_str.trim_end_matches(&['\r', '\n'][..]).to_owned();
                    crate::popup::ListItem::navigate(
                        label,
                        format!("Line {}", line_idx + 1),
                        &path,
                        line_idx,
                        0,
                    )
                })
                .collect();

            let popup = crate::popup::Popup::grep_buffer(
                "grep-buffer",
                items,
                app.search_query.clone(),
            );
            app.popup = Some(popup);
            return;
        }
        Command::OpenBufferPicker => {
            let current = app.buffer.path.clone();
            let items: Vec<crate::popup::ListItem> = app
                .open_buffers
                .iter()
                .filter_map(|p| {
                    let name = p.file_name()?.to_string_lossy().into_owned();
                    let detail = p.to_string_lossy().into_owned();
                    Some(crate::popup::ListItem::navigate(name, detail, p, 0, 0))
                })
                .collect();
            if items.is_empty() {
                app.message = Some("No open buffers".into());
            } else {
                // Pre-select the current file if it's in the list.
                let mut popup = crate::popup::Popup::navigate("buffers", items);
                if let crate::popup::PopupContent::List(ref mut state) = popup.content {
                    if let Some(cur) = &current {
                        let cur_str = cur.to_string_lossy();
                        if let Some(idx) = state
                            .items
                            .iter()
                            .position(|it| it.detail.as_deref() == Some(cur_str.as_ref()))
                        {
                            state.selected = idx;
                        }
                    }
                }
                app.popup = Some(popup);
            }
            return;
        }
        Command::OpenSymbolPicker => {
            let lang = app.current_language().unwrap_or("").to_owned();
            let path = app.buffer.path.clone().unwrap_or_else(|| {
                std::path::PathBuf::from(format!("untitled.{}", crate::lang::lang_to_ext(&lang)))
            });
            let syms = symbols::extract_symbols(&app.buffer.rope, &lang);
            if syms.is_empty() {
                app.message = Some("No symbols found".into());
            } else {
                let items: Vec<crate::popup::ListItem> = syms
                    .iter()
                    .map(|s| {
                        crate::popup::ListItem::navigate(
                            format!("{} {}", s.kind, s.name),
                            format!("line {}", s.line + 1),
                            &path,
                            s.line,
                            s.col,
                        )
                    })
                    .collect();
                app.popup = Some(crate::popup::Popup::navigate("symbols", items));
            }
            return;
        }
        Command::OpenDiagnosticPicker => {
            // Collect all diagnostics across every tracked file.
            let mut items: Vec<crate::popup::ListItem> = Vec::new();
            for (path_str, diags) in &app.lsp.diagnostics {
                let path = std::path::PathBuf::from(path_str);
                let file = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path_str.clone());
                for d in diags {
                    let sev = match d.severity {
                        crate::lsp_manager::DiagnosticSeverity::Error => "error",
                        crate::lsp_manager::DiagnosticSeverity::Warning => "warning",
                        crate::lsp_manager::DiagnosticSeverity::Information => "info",
                        crate::lsp_manager::DiagnosticSeverity::Hint => "hint",
                    };
                    items.push(crate::popup::ListItem::navigate(
                        d.message.clone(),
                        format!("{file}:{} [{sev}]", d.line + 1),
                        &path,
                        d.line,
                        d.col_start,
                    ));
                }
            }
            if items.is_empty() {
                app.message = Some("No diagnostics".into());
            } else {
                // Sort: errors first, then warnings, then by file+line.
                items.sort_by(|a, b| {
                    let sev_rank = |detail: &Option<String>| -> u8 {
                        match detail.as_deref().and_then(|d| d.split('[').nth(1)) {
                            Some(s) if s.starts_with("error") => 0,
                            Some(s) if s.starts_with("warning") => 1,
                            _ => 2,
                        }
                    };
                    sev_rank(&a.detail).cmp(&sev_rank(&b.detail))
                        .then_with(|| a.detail.cmp(&b.detail))
                });
                app.popup = Some(crate::popup::Popup::navigate("diagnostics", items));
            }
            return;
        }

        // --- Sub-mode entries (return early — no scroll update) ---
        Command::EnterGotoMode => {
            app.mode = Mode::Goto;
            let lsp_active = app
                .current_language()
                .map(|l| app.lsp.is_ready(l))
                .unwrap_or(false);
            let mut hints = vec![
                ("g".into(), "go to file start".into()),
                ("e".into(), "go to file end".into()),
                ("h".into(), "go to line first non-whitespace".into()),
                ("l".into(), "go to line end".into()),
                ("b".into(), "buffer picker".into()),
                ("s".into(), "symbol picker".into()),
                ("D".into(), "diagnostic picker  [LSP]".into()),
            ];
            if lsp_active {
                hints.push(("d".into(), "go to definition  [LSP]".into()));
                hints.push(("r".into(), "go to references  [LSP]".into()));
                hints.push(("y".into(), "go to type definition  [LSP]".into()));
                hints.push(("i".into(), "go to implementation  [LSP]".into()));
            }
            app.popup = Some(crate::popup::Popup::which_key("g", hints));
            return;
        }
        Command::FindCharForward => {
            app.mode = Mode::FindChar {
                dir: FindDir::Forward,
                till: false,
            };
            app.popup = Some(crate::popup::Popup::which_key(
                "f",
                vec![("any char".into(), "move cursor to next occurrence".into())],
            ));
            return;
        }
        Command::TillCharForward => {
            app.mode = Mode::FindChar {
                dir: FindDir::Forward,
                till: true,
            };
            app.popup = Some(crate::popup::Popup::which_key(
                "t",
                vec![("any char".into(), "move cursor till next occurrence".into())],
            ));
            return;
        }
        Command::FindCharBackward => {
            app.mode = Mode::FindChar {
                dir: FindDir::Backward,
                till: false,
            };
            app.popup = Some(crate::popup::Popup::which_key(
                "F",
                vec![("any char".into(), "move cursor to previous occurrence".into())],
            ));
            return;
        }
        Command::TillCharBackward => {
            app.mode = Mode::FindChar {
                dir: FindDir::Backward,
                till: true,
            };
            app.popup = Some(crate::popup::Popup::which_key(
                "T",
                vec![("any char".into(), "move cursor till previous occurrence".into())],
            ));
            return;
        }

        // --- Editing ---
        Command::DeleteSelection => {
            delete_selection(app);
            if app.mode == Mode::Select {
                app.mode = Mode::Normal;
            }
        }
        Command::ChangeSelection => {
            delete_selection(app);
            app.mode = Mode::Insert;
        }
        Command::YankSelection => {
            yank_selection(app);
            if app.mode == Mode::Select {
                app.mode = Mode::Normal;
            }
        }
        Command::PasteAfter => {
            paste_after(app);
        }
        Command::PasteBefore => {
            paste_before(app);
        }
        Command::Undo => {
            if app.buffer.undo() {
                clamp_selection(app);
                recompute_highlights(app);
            }
        }
        Command::Redo => {
            if app.buffer.redo() {
                clamp_selection(app);
                recompute_highlights(app);
            }
        }
        Command::OpenLineBelow => {
            open_line_below(app);
            return; // open_line_below handles scroll internally and sets Insert mode
        }
        Command::OpenLineAbove => {
            open_line_above(app);
            return; // open_line_above handles scroll internally and sets Insert mode
        }

        // --- Mode transitions (return early) ---
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
            let rope = &app.buffer.rope;
            app.selection = motion::move_line_start(rope, app.selection, false);
            app.mode = Mode::Insert;
            return;
        }
        Command::EnterInsertAtLineEnd => {
            let rope = &app.buffer.rope;
            let le = motion::move_line_end(rope, app.selection, false);
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
                // Close the insert undo session so the next insert starts fresh.
                app.insert_session_active = false;
                // Move cursor left one on leaving Insert (but not past line start).
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
                if let Some(path) = &app.buffer.path {
                    app.git_diff = crate::git::diff_marks(path);
                }
            } else {
                app.git_diff.clear();
            }
            return;
        }

        // --- File / application ---
        Command::Save => {
            if let Some((ref mut nb, _)) = app.notebook {
                match nb.save() {
                    Ok(()) => {
                        let name = nb.path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("notebook.ipynb")
                            .to_string();
                        app.message = Some(format!("Saved {name}"));
                    }
                    Err(e) => {
                        app.message = Some(format!("Error: {e}"));
                    }
                }
            } else {
                match app.buffer.save(None) {
                    Ok(()) => {
                        app.message = Some(format!("Saved {}", app.buffer.display_name()));
                        if let Some(ref path) = app.buffer.path.clone() {
                            app.git_diff = crate::git::diff_marks(path);
                        }
                    }
                    Err(e) => {
                        app.message = Some(format!("Error: {e}"));
                    }
                }
            }
            return;
        }
        Command::SaveAs(path) => {
            let path = path.clone();
            match app.buffer.save(Some(&path)) {
                Ok(()) => {
                    app.message = Some(format!("Saved {path}"));
                    if let Some(ref p) = app.buffer.path {
                        app.git_diff = crate::git::diff_marks(p);
                    }
                }
                Err(e) => {
                    app.message = Some(format!("Error: {e}"));
                }
            }
            return;
        }
        Command::Quit => {
            // Guard: quitting while in a cell edit overlay would lose work.
            if app.notebook_cell_edit.is_some() {
                app.message = Some(
                    "Editing a cell — Ctrl+Enter or :close-cell to return, :discard-cell to abandon"
                        .into(),
                );
                return;
            }
            let modified = if let Some((ref nb, _)) = app.notebook {
                nb.modified
            } else {
                app.buffer.modified
            };
            if modified {
                app.message =
                    Some("Unsaved changes — use :q! to force quit".to_string());
            } else {
                app.should_quit = true;
            }
            return;
        }
        Command::ForceQuit => {
            app.should_quit = true;
            return;
        }
        Command::WriteQuit => {
            match app.buffer.save(None) {
                Ok(()) => {
                    app.should_quit = true;
                }
                Err(e) => {
                    app.message = Some(format!("Error: {e}"));
                }
            }
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
                Err(e) => {
                    app.message = Some(format!("shell error: {e}"));
                }
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
            // Sync current cell to LSP before leaving it.
            lsp_did_change(app);
            save_focused_cell(app);
            {
                if let Some((ref nb, ref mut state)) = app.notebook {
                    let last = nb.cells.len().saturating_sub(1);
                    state.focused_cell = (state.focused_cell + 1).min(last);
                    state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
                }
            }
            load_focused_cell(app);
            app.mode = Mode::Notebook;
            return;
        }
        Command::NotebookPrevCell => {
            // Sync current cell to LSP before leaving it.
            lsp_did_change(app);
            save_focused_cell(app);
            {
                if let Some((ref nb, ref mut state)) = app.notebook {
                    state.focused_cell = state.focused_cell.saturating_sub(1);
                    state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
                }
            }
            load_focused_cell(app);
            app.mode = Mode::Notebook;
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
        Command::NotebookEnterEdit | Command::NotebookExitEdit => {
            // No-op: notebook editing is now always in-place via app.buffer.
            return;
        }
        Command::NotebookExecuteCell => {
            // Flush the live buffer into the cell before executing so the
            // kernel always runs whatever is currently visible on screen.
            save_focused_cell(app);

            if let Some((ref mut nb, ref mut state)) = app.notebook {
                let nb_dir = notebook_dir(&nb.path.clone());

                // Lazily start the kernel on first execution, or restart if dead.
                if nb.kernel.is_none() || !nb.kernel.as_mut().map(|k| k.is_alive()).unwrap_or(false) {
                    match nb.start_kernel(&nb_dir) {
                        Ok(()) => {}
                        Err(e) => {
                            app.message = Some(format!("Kernel start failed: {e}"));
                            return;
                        }
                    }
                }

                let idx = state.focused_cell;
                state.executing_cell = Some(idx);

                if let Some(ref mut session) = nb.kernel {
                    if idx < nb.cells.len() {
                        match nb.cells[idx].execute(session) {
                            Ok(()) => {
                                let count = nb.cells[idx].execution_count.unwrap_or(0);
                                app.message = Some(format!(
                                    "Cell [{}] done  In [{}]",
                                    idx + 1,
                                    count
                                ));
                            }
                            Err(e) => {
                                app.message = Some(format!("Kernel error: {e}"));
                                nb.kernel = None; // session died; restart on next execute
                            }
                        }
                        nb.modified = true;
                    }
                }

                state.executing_cell = None;
            }
            return;
        }
        Command::NotebookRestartKernel => {
            if let Some((ref mut nb, _)) = app.notebook {
                nb.kernel = None; // Drop kills the old process
                let nb_dir = notebook_dir(&nb.path.clone());
                match nb.start_kernel(&nb_dir) {
                    Ok(()) => app.message = Some("Kernel restarted".into()),
                    Err(e) => app.message = Some(format!("Kernel restart failed: {e}")),
                }
            }
            return;
        }
        Command::NotebookInterruptKernel => {
            if let Some((ref nb, _)) = app.notebook {
                if let Some(ref session) = nb.kernel {
                    session.interrupt();
                    app.message = Some("Kernel interrupted".into());
                } else {
                    app.message = Some("No kernel running".into());
                }
            }
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
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((focused, cells)) = snap {
                if let Some((ref mut nb, ref mut state)) = app.notebook {
                    nb.cells = cells;
                    nb.modified = true;
                    state.focused_cell = focused.min(nb.cells.len().saturating_sub(1));
                    state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
                }
                load_focused_cell(app);
                notebook_lsp_reopen(app);
                app.mode = Mode::Notebook;
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
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((focused, cells)) = snap {
                if let Some((ref mut nb, ref mut state)) = app.notebook {
                    nb.cells = cells;
                    nb.modified = true;
                    state.focused_cell = focused.min(nb.cells.len().saturating_sub(1));
                    state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
                }
                load_focused_cell(app);
                notebook_lsp_reopen(app);
                app.mode = Mode::Notebook;
            } else {
                app.message = Some("Nothing to redo".into());
            }
            return;
        }
        Command::NotebookNewCellBelow => {
            save_focused_cell(app);
            push_cell_snapshot(app);
            {
                if let Some((ref mut nb, ref mut state)) = app.notebook {
                    let new_idx = state.focused_cell + 1;
                    nb.cells.insert(new_idx, Cell {
                        id: new_cell_id(),
                        cell_type: CellType::Code,
                        source: Rope::new(),
                        outputs: vec![],
                        execution_count: None,
                    });
                    state.focused_cell = new_idx;
                    nb.modified = true;
                    state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
                }
            }
            load_focused_cell(app);
            notebook_lsp_reopen(app);
            app.mode = Mode::Notebook;
            return;
        }
        Command::NotebookNewCellAbove => {
            save_focused_cell(app);
            push_cell_snapshot(app);
            {
                if let Some((ref mut nb, ref mut state)) = app.notebook {
                    let new_idx = state.focused_cell;
                    nb.cells.insert(new_idx, Cell {
                        id: new_cell_id(),
                        cell_type: CellType::Code,
                        source: Rope::new(),
                        outputs: vec![],
                        execution_count: None,
                    });
                    // focused_cell already points at the new empty cell (same index).
                    nb.modified = true;
                    state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
                }
            }
            load_focused_cell(app);
            notebook_lsp_reopen(app);
            app.mode = Mode::Notebook;
            return;
        }
        Command::NotebookDeleteCell => {
            save_focused_cell(app);
            push_cell_snapshot(app);
            {
                if let Some((ref mut nb, ref mut state)) = app.notebook {
                    if !nb.cells.is_empty() {
                        nb.cells.remove(state.focused_cell);
                        nb.modified = true;
                        state.focused_cell =
                            state.focused_cell.min(nb.cells.len().saturating_sub(1));
                        state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
                    }
                }
            }
            load_focused_cell(app);
            notebook_lsp_reopen(app);
            app.mode = Mode::Notebook;
            return;
        }
        Command::NotebookClearOutputs => {
            if let Some((ref mut nb, ref state)) = app.notebook {
                let idx = state.focused_cell;
                if idx < nb.cells.len() {
                    nb.cells[idx].outputs.clear();
                    nb.modified = true;
                }
            }
            return;
        }

        // ── Cell edit overlay ────────────────────────────────────────────────

        Command::NotebookOpenCellEdit => {
            // The focused cell is already in app.buffer; just switch to the
            // full-screen rendering so the user gets an uncluttered edit view.
            app.notebook_focused_edit = true;
            app.mode = Mode::Normal;
            return;
        }

        Command::NotebookCloseCellEdit | Command::NotebookDiscardCellEdit => {
            // Return to the multi-cell view and re-enter Notebook mode.
            app.notebook_focused_edit = false;
            app.mode = Mode::Notebook;
            // Sync final cell source in case edits happened in the overlay.
            if let Some(ref session) = app.notebook_cell_edit {
                if let Some(path) = app.buffer.path.clone() {
                    if app.lsp.notebook_sync_supported(&session.language) {
                        let notebook_uri = path_to_uri(&session.notebook_path);
                        let cell_uri = path_to_uri(&path);
                        let text = app.buffer.rope.to_string();
                        app.lsp.notebook_did_change_cell(
                            &session.language, &notebook_uri, &cell_uri, &text,
                        );
                    }
                }
            }
            return;
        }

        // --- Notebook mode ---
        Command::EnterNotebook => {
            if app.notebook.is_some() {
                app.mode = Mode::Notebook;
            }
            return;
        }

        // --- Search ---
        Command::SearchForward => {
            app.mode = Mode::Search { forward: true };
            app.search_query_just_opened = true;
            app.search_active = false;
            search_compute_matches(app);
            return;
        }
        Command::SearchBackward => {
            app.mode = Mode::Search { forward: false };
            app.search_query_just_opened = true;
            app.search_active = false;
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
                let rope = &app.buffer.rope;
                app.selection = motion::move_down(rope, app.selection, false);
            }
        }
        Command::PageUp => {
            let half = (app.viewport_height / 2).max(1);
            for _ in 0..half {
                let rope = &app.buffer.rope;
                app.selection = motion::move_up(rope, app.selection, false);
            }
        }

        // --- LSP ---

        Command::LspHover => {
            lsp_request(app, LspRequestKind::Hover);
            return;
        }
        Command::LspGotoDefinition => {
            lsp_request(app, LspRequestKind::Definition);
            return;
        }
        Command::LspGotoReferences => {
            lsp_request(app, LspRequestKind::References);
            return;
        }
        Command::LspGotoTypeDefinition => {
            lsp_request(app, LspRequestKind::TypeDefinition);
            return;
        }
        Command::LspGotoImplementation => {
            lsp_request(app, LspRequestKind::Implementation);
            return;
        }
        Command::LspRequestCompletion => {
            lsp_request(app, LspRequestKind::Completion);
            return;
        }
    }

    update_scroll(app);
}

/// Snapshot the full cell list before a structural mutation (undo support).
fn push_cell_snapshot(app: &mut App) {
    let snapshot = app.notebook.as_ref()
        .map(|(nb, state)| (state.focused_cell, nb.cells.clone()));
    if let Some((focused, cells)) = snapshot {
        if let Some((_, ref mut state)) = app.notebook {
            state.push_snapshot(focused, &cells);
        }
    }
}

/// Write app.buffer.rope back to the currently focused notebook cell.
fn save_focused_cell(app: &mut App) {
    if let Some((ref mut nb, ref state)) = app.notebook {
        let idx = state.focused_cell;
        if idx < nb.cells.len() {
            nb.cells[idx].source = app.buffer.rope.clone();
        }
    }
}

/// Load the focused notebook cell into app.buffer, updating all dependent state.
pub fn load_focused_cell(app: &mut App) {
    if let Some((ref nb, ref state)) = app.notebook {
        let idx = state.focused_cell;
        if idx >= nb.cells.len() {
            return;
        }
        let cell = &nb.cells[idx];
        let language = nb.metadata.kernel_language.clone();
        let notebook_path = nb.path.clone();
        let source = cell.source.clone();

        let ext = lang_to_ext(&language);
        let stem = notebook_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "notebook".into());
        let dir = notebook_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let virtual_path = dir.join(format!("{stem}__cell{idx}.{ext}"));

        app.buffer = Buffer::new_empty();
        app.buffer.rope = source;
        app.buffer.path = Some(virtual_path.clone());
        app.selection = Selection::point(0);
        app.scroll_row = 0;
        app.scroll_col = 0;
        app.insert_session_active = false;

        app.notebook_cell_edit = Some(crate::app::CellEditSession {
            cell_index: idx,
            language: language.clone(),
            notebook_path,
        });

        app.highlighter = Highlighter::new(Some(&virtual_path));
        recompute_highlights(app);

        // Ensure the LSP server is running.
        if let Some(server_config) = app.config.language_servers.get(&language).cloned() {
            let nb_dir = app.notebook.as_ref()
                .and_then(|(nb, _)| nb.path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| p.to_path_buf()));
            app.lsp.ensure_server(&language, &server_config, nb_dir.as_deref());
        }

        // For servers using the notebookDocument protocol, all cells are already
        // tracked — no per-cell textDocument notification needed.
        // For plain textDocument servers, open the cell if this is the first
        // visit, or send a change if we've been here before (no duplicate opens).
        if let Some(ref session) = app.notebook_cell_edit {
            if !app.lsp.notebook_sync_supported(&session.language) {
                let text = app.buffer.rope.to_string();
                if app.lsp.is_doc_open(&session.language, &virtual_path) {
                    app.lsp.did_change(&session.language, &virtual_path, &text);
                } else {
                    app.lsp.did_open(&session.language, &virtual_path, &text);
                }
            }
        }
    }
}

/// Generate a simple unique cell ID without an external crate.
fn new_cell_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{t:016x}{n:016x}")
}

/// Execute a slice of commands in order.
pub fn run_many(app: &mut App, cmds: &[Command]) {
    for cmd in cmds {
        execute(app, cmd);
    }
}

/// Recompute syntax highlight spans from the current buffer.
pub fn recompute_highlights(app: &mut App) {
    app.highlight_spans = app
        .highlighter
        .highlight(&app.buffer.rope)
        .unwrap_or_default();
}

/// Update scroll_row / scroll_col so the cursor is visible.
pub fn update_scroll(app: &mut App) {
    let rope = &app.buffer.rope;
    if rope.len_chars() == 0 {
        app.scroll_row = 0;
        app.scroll_col = 0;
        return;
    }

    let pos = app.selection.head.min(rope.len_chars());
    let line_idx = rope.char_to_line(pos);
    let line_start = rope.line_to_char(line_idx);
    let col = pos - line_start;
    let scroll_off = app.config.editor.scroll_off;

    if line_idx < app.scroll_row + scroll_off {
        app.scroll_row = line_idx.saturating_sub(scroll_off);
    }
    if app.scroll_row + scroll_off > line_idx {
        app.scroll_row = line_idx.saturating_sub(scroll_off);
    }

    // Horizontal scroll
    if col < app.scroll_col {
        app.scroll_col = col;
    }
}

// ---------------------------------------------------------------------------
// Private edit helpers
// ---------------------------------------------------------------------------

fn delete_selection(app: &mut App) {
    let start = app.selection.start();
    let end = app.selection.end();
    let del_end = (end + 1).min(app.buffer.rope.len_chars());
    app.buffer.remove(start, del_end);
    let new_pos = start.min(app.buffer.rope.len_chars());
    app.selection = Selection::point(new_pos);
    recompute_highlights(app);
    update_scroll(app);
}

fn yank_selection(app: &mut App) {
    let start = app.selection.start();
    let end = (app.selection.end() + 1).min(app.buffer.rope.len_chars());
    app.clipboard = app.buffer.rope.slice(start..end).to_string();
    app.message = Some(format!("Yanked {} chars", end - start));
}

fn paste_after(app: &mut App) {
    let text = app.clipboard.clone();
    if text.is_empty() {
        return;
    }
    let pos = app.selection.head;
    let len = app.buffer.rope.len_chars();
    let insert_pos = if len > 0 { (pos + 1).min(len) } else { 0 };
    app.buffer.insert(insert_pos, &text);
    app.selection = Selection::point(insert_pos);
    recompute_highlights(app);
    update_scroll(app);
}

fn paste_before(app: &mut App) {
    let text = app.clipboard.clone();
    if text.is_empty() {
        return;
    }
    let pos = app.selection.head;
    app.buffer.insert(pos, &text);
    app.selection = Selection::point(pos);
    recompute_highlights(app);
    update_scroll(app);
}

fn open_line_below(app: &mut App) {
    let rope = &app.buffer.rope;
    let pos = app.selection.head;
    let le = if rope.len_chars() == 0 {
        0
    } else {
        let line_idx = rope.char_to_line(pos.min(rope.len_chars()));
        let line_str = rope.line(line_idx);
        let line_len = line_str.len_chars();
        let content_len = if line_len > 0
            && (line_str.char(line_len - 1) == '\n'
                || line_str.char(line_len - 1) == '\r')
        {
            rope.line_to_char(line_idx) + line_len - 1
        } else {
            rope.line_to_char(line_idx) + line_len
        };
        content_len
    };
    app.buffer.insert(le, "\n");
    app.selection = Selection::point(le + 1);
    app.mode = Mode::Insert;
    recompute_highlights(app);
    update_scroll(app);
}

fn open_line_above(app: &mut App) {
    let rope = &app.buffer.rope;
    let pos = app.selection.head;
    let ls = if rope.len_chars() == 0 {
        0
    } else {
        let line_idx = rope.char_to_line(pos.min(rope.len_chars()));
        rope.line_to_char(line_idx)
    };
    app.buffer.insert(ls, "\n");
    app.selection = Selection::point(ls);
    app.mode = Mode::Insert;
    recompute_highlights(app);
    update_scroll(app);
}

// ---------------------------------------------------------------------------
// Notebook LSP helpers
// ---------------------------------------------------------------------------

/// Build the full cell list for `notebookDocument/didOpen` or a reopen.
fn build_notebook_cells(nb: &crate::notebook::Notebook) -> Vec<NotebookCell> {
    let lang = &nb.metadata.kernel_language;
    let ext = lang_to_ext(lang);
    let stem = nb.path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "notebook".into());
    let dir = nb.path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    nb.cells
        .iter()
        .enumerate()
        .map(|(idx, cell)| {
            let kind = match cell.cell_type {
                CellType::Code => 2,
                _ => 1,
            };
            let cell_path = dir.join(format!("{stem}__cell{idx}.{ext}"));
            let language_id = match cell.cell_type {
                CellType::Code => lang.clone(),
                CellType::Markdown => "markdown".into(),
                _ => "plaintext".into(),
            };
            NotebookCell {
                kind,
                uri: path_to_uri(&cell_path),
                language_id,
                text: cell.source.to_string(),
            }
        })
        .collect()
}

/// Send `notebookDocument/didOpen` for the currently-loaded notebook.
pub fn notebook_lsp_open(app: &mut App) {
    if let Some((ref nb, _)) = app.notebook {
        let lang = nb.metadata.kernel_language.clone();
        if !app.lsp.is_ready(&lang) || !app.lsp.notebook_sync_supported(&lang) {
            return;
        }
        let notebook_uri = path_to_uri(&nb.path);
        let cells = build_notebook_cells(nb);
        app.lsp.notebook_did_open(&lang, &notebook_uri, &cells);
    }
}

/// Close the notebook in LSP (does nothing if not currently open there).
fn notebook_lsp_close(app: &mut App) {
    if let Some((ref nb, _)) = app.notebook {
        let lang = nb.metadata.kernel_language.clone();
        let notebook_uri = path_to_uri(&nb.path);
        app.lsp.notebook_did_close(&lang, &notebook_uri);
    }
}

/// Close and immediately reopen the notebook in LSP after a structural change.
fn notebook_lsp_reopen(app: &mut App) {
    notebook_lsp_close(app);
    notebook_lsp_open(app);
}

// ---------------------------------------------------------------------------
// Search helpers
// ---------------------------------------------------------------------------

/// Recompute search_matches for the current query across the whole buffer.
pub fn search_compute_matches(app: &mut App) {
    app.search_matches.clear();
    app.search_current = 0;
    if app.search_query.is_empty() {
        return;
    }
    let text = app.buffer.rope.to_string();
    let query = &app.search_query;
    let mut start = 0;
    while start < text.len() {
        if let Some(rel) = text[start..].find(query.as_str()) {
            let byte_pos = start + rel;
            // Convert byte index to char index.
            let char_idx = text[..byte_pos].chars().count();
            app.search_matches.push(char_idx);
            // Advance past this match (at least 1 byte to avoid infinite loop).
            start = byte_pos + query.len().max(1);
        } else {
            break;
        }
    }
}

/// Jump to the next (or previous if `reverse`) search match relative to cursor.
pub fn search_jump(app: &mut App, reverse: bool) {
    if app.search_matches.is_empty() {
        if !app.search_query.is_empty() {
            app.message = Some(format!("No matches for \"{}\"", app.search_query));
        }
        return;
    }
    let cursor = app.selection.head;
    let count = app.search_matches.len();
    if reverse {
        // Find the last match strictly before the cursor, wrap around.
        let idx = app.search_matches.iter().rposition(|&m| m < cursor)
            .unwrap_or(count - 1);
        app.search_current = idx;
    } else {
        // Find the first match strictly after the cursor, wrap around.
        let idx = app.search_matches.iter().position(|&m| m > cursor)
            .unwrap_or(0);
        app.search_current = idx;
    }
    app.selection = Selection::point(app.search_matches[app.search_current]);
    update_scroll(app);
    app.message = Some(format!(
        "Match {}/{} for \"{}\"",
        app.search_current + 1,
        count,
        app.search_query
    ));
}

fn clamp_selection(app: &mut App) {
    let len = app.buffer.rope.len_chars();
    let head = app.selection.head.min(len);
    let anchor = app.selection.anchor.min(len);
    app.selection = Selection::new(anchor, head);
}

// ---------------------------------------------------------------------------
// LSP helpers
// ---------------------------------------------------------------------------

fn lsp_request(app: &mut App, kind: LspRequestKind) {
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

    // In a cell-edit overlay with notebookDocument support, route the change
    // through the notebook protocol so the server has full cross-cell context.
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
            // If a notebook is open (and we're not inside a cell-edit overlay),
            // prefer the notebookDocument protocol.
            let notebook_lang = app.notebook.as_ref()
                .map(|(nb, _)| nb.metadata.kernel_language.clone());

            if notebook_lang.as_deref() == Some(&language)
                && !app.notebook_focused_edit
            {
                if app.lsp.notebook_sync_supported(&language) {
                    notebook_lsp_open(app);
                }
                // Fallback: open the current cell as a standalone textDocument.
                if !app.lsp.notebook_sync_supported(&language) {
                    if let Some(path) = app.buffer.path.clone() {
                        let text = app.buffer.rope.to_string();
                        app.lsp.did_open(&language, &path, &text);
                    }
                }
                return;
            }

            // Regular file (or cell-edit overlay that opened before Initialized).
            if app.current_language() == Some(&language) {
                if let Some(path) = app.buffer.path.clone() {
                    let text = app.buffer.rope.to_string();
                    app.lsp.did_open(&language, &path, &text);
                }
            }
        }
        LspEvent::Diagnostics { path: _, ref items } => {
            // Diagnostic counts are shown permanently in the status bar; no flash needed.
            let _ = items;
        }
        LspEvent::CompletionResult { items } => {
            // Only show if still in Insert mode.
            if app.mode == Mode::Insert && !items.is_empty() {
                let popup_items: Vec<crate::popup::ListItem> = items
                    .iter()
                    .map(|item| crate::popup::ListItem {
                        label: item
                            .insert_text
                            .clone()
                            .unwrap_or_else(|| item.label.clone()),
                        detail: item.detail.clone(),
                        kind: item.kind.clone(),
                        payload: None,
                    })
                    .collect();
                app.popup = Some(crate::popup::Popup::completion(popup_items));
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
                // Show a filterable list of all reference locations.
                let items: Vec<crate::popup::ListItem> = locations
                    .iter()
                    .map(|loc| {
                        let file = loc
                            .path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("?");
                        crate::popup::ListItem {
                            label: format!("{}:{}", file, loc.line + 1),
                            detail: Some(loc.path.to_string_lossy().to_string()),
                            kind: None,
                            payload: None,
                        }
                    })
                    .collect();
                // Jump to first; a full location-list popup is Phase 4.
                jump_to_location(app, &locations[0]);
                let _ = items;
            }
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
        update_scroll(app);
    } else {
        open_file_at(app, &target, loc.line, loc.character);
    }
}

/// Load `path` into the editor buffer and place the cursor at (line, character).
pub fn open_file_at(app: &mut App, path: &std::path::Path, line: usize, character: usize) {
    // Close the old virtual document in LSP if applicable.
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
    recompute_highlights(app);

    // Ensure LSP server for the new file's language.
    let file_dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(ref lang) = new_lang {
        if let Some(server_config) = app.config.language_servers.get(lang).cloned() {
            app.lsp.ensure_server(lang, &server_config, file_dir);
        }
        if app.lsp.is_ready(lang) {
            let text = app.buffer.rope.to_string();
            app.lsp.did_open(lang, path, &text);
        }
        // If not yet ready, the Initialized event handler will send did_open.
    }

    // Jump to the target position.
    let char_idx = lsp_pos_to_char(&app.buffer.rope, line, character);
    app.selection = Selection::point(char_idx);
    update_scroll(app);

    // Track this path in the session buffer list (deduplicated).
    if !app.open_buffers.iter().any(|p| p.as_path() == path) {
        app.open_buffers.push(path.to_path_buf());
    }

    // Refresh git diff marks for the newly opened file.
    app.git_diff = crate::git::diff_marks(path);

    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
    app.message = Some(format!("Opened {} (line {})", name, line + 1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn test_exec_clamping_behavior() {
        let config = Config::load().expect("failed to load config");
        let mut app = App::new(None, config).unwrap();
        
        // 1. Setup buffer with some text ending in newline.
        app.buffer.rope = Rope::from_str("hello\nworld\n");
        let len = app.buffer.rope.len_chars();
        assert_eq!(len, 12); // "hello\n" (6) + "world\n" (6)

        // 2. Test clamp_selection: setting cursor head beyond len should clamp to len, not len - 1
        app.selection = Selection::point(20);
        clamp_selection(&mut app);
        assert_eq!(app.selection.head, 12);
        assert_eq!(app.selection.anchor, 12);

        // 3. Test update_scroll: cursor at len (12) should place pos on line index 2.
        app.selection = Selection::point(12);
        update_scroll(&mut app);
        assert_eq!(app.buffer.rope.char_to_line(12), 2);
    }

    #[test]
    fn test_delete_selection_clamping() {
        let config = Config::load().expect("failed to load config");
        let mut app = App::new(None, config).unwrap();
        app.buffer.rope = Rope::from_str("abc");
        app.selection = Selection::new(0, 2); // selects 'abc' (char indexes 0, 1, 2)
        delete_selection(&mut app);
        assert_eq!(app.buffer.rope.len_chars(), 0);
        assert_eq!(app.selection.head, 0);
    }
}
