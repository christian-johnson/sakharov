mod lsp;
mod notebook;
mod search;
mod text;

pub use lsp::{apply_code_action, jump_to_location, lsp_did_change, process_lsp_events};
pub use search::{search_compute_matches, search_jump};

use ropey::Rope;

use crate::{
    app::App,
    command::Command,
    lsp_manager::LspRequestKind,
    mode::{FindDir, Mode},
    motion,
    notebook::{Cell, CellType},
    selection::Selection,
    symbols,
};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Execute a single command against the application state.
pub fn execute(app: &mut App, cmd: &Command) {
    let extend = app.mode == Mode::Select;

    match cmd {
        // --- Motions ---
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
                    let label = line.to_string()
                        .trim_end_matches(&['\r', '\n'][..])
                        .to_owned();
                    crate::popup::ListItem::navigate(
                        label,
                        format!("Line {}", line_idx + 1),
                        &path,
                        line_idx,
                        0,
                    )
                })
                .collect();
            app.popup = Some(crate::popup::Popup::grep(
                "grep buffer",
                items,
                app.search.query.clone(),
            ));
            return;
        }
        Command::GrepProject => {
            let root = app.buffer.path.as_deref()
                .and_then(|p| p.parent())
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

            // Try ripgrep first, fall back to grep.
            let rg_available = std::process::Command::new("rg")
                .arg("--version")
                .output()
                .is_ok();

            let output = if rg_available {
                std::process::Command::new("rg")
                    .args(["--line-number", "--no-heading", "--color=never", "--with-filename", "."])
                    .current_dir(&root)
                    .output()
            } else {
                std::process::Command::new("grep")
                    .args(["-rn", "-I", "."])
                    .arg(".")
                    .current_dir(&root)
                    .output()
            };

            let items: Vec<crate::popup::ListItem> = match output {
                Ok(out) => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    text.lines()
                        .filter_map(|line| {
                            // format: file:lineno:content
                            let mut parts = line.splitn(3, ':');
                            let file = parts.next()?;
                            let lineno_str = parts.next()?;
                            let content = parts.next().unwrap_or("").trim_end_matches(&['\r', '\n'][..]);
                            let lineno: usize = lineno_str.parse().ok()?;
                            let path = root.join(file);
                            Some(crate::popup::ListItem::navigate(
                                content.to_owned(),
                                format!("{}:{}", file, lineno),
                                &path,
                                lineno.saturating_sub(1),
                                0,
                            ))
                        })
                        .collect()
                }
                Err(_) => {
                    app.message = Some("grep not available".into());
                    return;
                }
            };

            app.popup = Some(crate::popup::Popup::grep(
                "grep project",
                items,
                app.search.query.clone(),
            ));
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
                    .map(|s| crate::popup::ListItem::navigate(
                        format!("{} {}", s.kind, s.name),
                        format!("line {}", s.line + 1),
                        &path,
                        s.line,
                        s.col,
                    ))
                    .collect();
                app.popup = Some(crate::popup::Popup::navigate("symbols", items));
            }
            return;
        }
        Command::OpenDiagnosticPicker => {
            let mut items: Vec<crate::popup::ListItem> = Vec::new();
            for (path_str, diags) in &app.lsp.diagnostics {
                let path = std::path::PathBuf::from(path_str);
                let file = path.file_name()
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

        // --- Sub-mode entries ---
        Command::EnterGotoMode => {
            app.mode = Mode::Goto;
            let lsp_active = app.current_language()
                .map(|l| app.lsp.is_ready(l))
                .unwrap_or(false);
            let mut hints = vec![
                ("g".into(), "go to file start".into()),
                ("e".into(), "go to file end".into()),
                ("h".into(), "go to line first non-whitespace".into()),
                ("l".into(), "go to line end".into()),
                ("b".into(), "buffer picker".into()),
                ("s".into(), "symbol picker".into()),
                ("c".into(), "comment/uncomment selection".into()),
                ("D".into(), "diagnostic picker".into()),
            ];
            if lsp_active {
                hints.push(("a".into(), "code actions  [LSP]".into()));
                hints.push(("d".into(), "go to definition  [LSP]".into()));
                hints.push(("r".into(), "go to references  [LSP]".into()));
                hints.push(("y".into(), "go to type definition  [LSP]".into()));
                hints.push(("i".into(), "go to implementation  [LSP]".into()));
            }
            app.popup = Some(crate::popup::Popup::which_key("g", hints));
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
                    Err(e) => app.message = Some(format!("Error: {e}")),
                }
            } else {
                match app.buffer.save(None) {
                    Ok(()) => {
                        app.message = Some(format!("Saved {}", app.buffer.display_name()));
                        if let Some(ref path) = app.buffer.path.clone() {
                            app.git_diff = crate::git::diff_marks(path);
                        }
                    }
                    Err(e) => app.message = Some(format!("Error: {e}")),
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
                Err(e) => app.message = Some(format!("Error: {e}")),
            }
            return;
        }
        Command::Quit => {
            if app.notebook_cell_edit.is_some() && app.notebook.is_some() {
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
                app.message = Some("Unsaved changes — use :q! to force quit".to_string());
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
                Ok(()) => app.should_quit = true,
                Err(e) => app.message = Some(format!("Error: {e}")),
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
            lsp_did_change(app);
            notebook::save_focused_cell(app);
            if let Some((ref nb, ref mut state)) = app.notebook {
                let last = nb.cells.len().saturating_sub(1);
                state.focused_cell = (state.focused_cell + 1).min(last);
                state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
            }
            notebook::load_focused_cell(app);
            app.mode = Mode::Notebook;
            return;
        }
        Command::NotebookPrevCell => {
            lsp_did_change(app);
            notebook::save_focused_cell(app);
            if let Some((ref nb, ref mut state)) = app.notebook {
                state.focused_cell = state.focused_cell.saturating_sub(1);
                state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
            }
            notebook::load_focused_cell(app);
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
            return;
        }
        Command::NotebookExecuteCell => {
            notebook::save_focused_cell(app);
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                let nb_dir = notebook::notebook_dir(&nb.path.clone());
                if nb.kernel.is_none()
                    || !nb.kernel.as_mut().map(|k| k.is_alive()).unwrap_or(false)
                {
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
                                app.message =
                                    Some(format!("Cell [{}] done  In [{}]", idx + 1, count));
                            }
                            Err(e) => {
                                app.message = Some(format!("Kernel error: {e}"));
                                nb.kernel = None;
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
                nb.kernel = None;
                let nb_dir = notebook::notebook_dir(&nb.path.clone());
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
                    } else { None }
                } else { None }
            };
            if let Some((focused, cells)) = snap {
                if let Some((ref mut nb, ref mut state)) = app.notebook {
                    nb.cells = cells;
                    nb.modified = true;
                    state.focused_cell = focused.min(nb.cells.len().saturating_sub(1));
                    state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
                }
                notebook::load_focused_cell(app);
                notebook::notebook_lsp_reopen(app);
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
                    } else { None }
                } else { None }
            };
            if let Some((focused, cells)) = snap {
                if let Some((ref mut nb, ref mut state)) = app.notebook {
                    nb.cells = cells;
                    nb.modified = true;
                    state.focused_cell = focused.min(nb.cells.len().saturating_sub(1));
                    state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
                }
                notebook::load_focused_cell(app);
                notebook::notebook_lsp_reopen(app);
                app.mode = Mode::Notebook;
            } else {
                app.message = Some("Nothing to redo".into());
            }
            return;
        }
        Command::NotebookNewCellBelow => {
            notebook::save_focused_cell(app);
            notebook::push_cell_snapshot(app);
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                let new_idx = state.focused_cell + 1;
                nb.cells.insert(new_idx, Cell {
                    id: notebook::new_cell_id(),
                    cell_type: CellType::Code,
                    source: Rope::new(),
                    outputs: vec![],
                    execution_count: None,
                });
                state.focused_cell = new_idx;
                nb.modified = true;
                state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
            }
            notebook::load_focused_cell(app);
            notebook::notebook_lsp_reopen(app);
            app.mode = Mode::Notebook;
            return;
        }
        Command::NotebookNewCellAbove => {
            notebook::save_focused_cell(app);
            notebook::push_cell_snapshot(app);
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                let new_idx = state.focused_cell;
                nb.cells.insert(new_idx, Cell {
                    id: notebook::new_cell_id(),
                    cell_type: CellType::Code,
                    source: Rope::new(),
                    outputs: vec![],
                    execution_count: None,
                });
                nb.modified = true;
                state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
            }
            notebook::load_focused_cell(app);
            notebook::notebook_lsp_reopen(app);
            app.mode = Mode::Notebook;
            return;
        }
        Command::NotebookDeleteCell => {
            notebook::save_focused_cell(app);
            notebook::push_cell_snapshot(app);
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                if !nb.cells.is_empty() {
                    nb.cells.remove(state.focused_cell);
                    nb.modified = true;
                    state.focused_cell =
                        state.focused_cell.min(nb.cells.len().saturating_sub(1));
                    state.ensure_focused_visible(&nb.cells, app.viewport_height, &app.buffer.rope);
                }
            }
            notebook::load_focused_cell(app);
            notebook::notebook_lsp_reopen(app);
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

        // --- Cell edit overlay ---
        Command::NotebookOpenCellEdit => {
            if let Some(ref mut session) = app.notebook_cell_edit {
                session.focused_edit = true;
            }
            app.mode = Mode::Normal;
            return;
        }
        Command::NotebookCloseCellEdit | Command::NotebookDiscardCellEdit => {
            if let Some(ref mut session) = app.notebook_cell_edit {
                session.focused_edit = false;
            }
            app.mode = Mode::Notebook;
            if let Some(ref session) = app.notebook_cell_edit {
                if let Some(path) = app.buffer.path.clone() {
                    if app.lsp.notebook_sync_supported(&session.language) {
                        let notebook_uri = crate::lsp::path_to_uri(&session.notebook_path);
                        let cell_uri = crate::lsp::path_to_uri(&path);
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
        Command::LspHover            => { lsp::lsp_request(app, LspRequestKind::Hover);           return; }
        Command::LspGotoDefinition   => { lsp::lsp_request(app, LspRequestKind::Definition);      return; }
        Command::LspGotoReferences   => { lsp::lsp_request(app, LspRequestKind::References);      return; }
        Command::LspGotoTypeDefinition => { lsp::lsp_request(app, LspRequestKind::TypeDefinition); return; }
        Command::LspGotoImplementation => { lsp::lsp_request(app, LspRequestKind::Implementation); return; }
        Command::LspRequestCompletion => { lsp::lsp_request(app, LspRequestKind::Completion);     return; }
        Command::LspCodeActions      => { lsp::lsp_code_actions_request(app);                     return; }

        // --- Editing (continued) ---
        Command::CommentRegion => {
            text::comment_region(app);
            if app.mode == Mode::Select {
                app.mode = Mode::Normal;
            }
        }
    }

    update_scroll(app);
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
///
/// Uses the stored viewport dimensions (`app.viewport_height` / `app.viewport_width`)
/// which are refreshed at the top of every render frame.  This is the single
/// authoritative scroll function; the previous `update_scroll_to_fit` in
/// app.rs is gone.
pub fn update_scroll(app: &mut App) {
    let visible_rows = app.viewport_height;
    let git_col = if app.config.editor.git_gutter && app.notebook.is_none() { 1usize } else { 0 };
    let gutter_width = if app.config.editor.line_numbers { 5 + git_col } else { git_col };
    let visible_cols = app.viewport_width.saturating_sub(gutter_width);

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
    let scroll_off = app.config.editor.scroll_off;

    // Vertical
    let top_bound = line_idx.saturating_sub(scroll_off);
    if app.scroll_row > top_bound {
        app.scroll_row = top_bound;
    }
    let bottom_bound = line_idx + scroll_off;
    if bottom_bound >= app.scroll_row + visible_rows {
        app.scroll_row = bottom_bound.saturating_sub(visible_rows) + 1;
    }

    // Horizontal — accurate display-column calculation (handles tabs)
    let line_start = rope.line_to_char(line_idx);
    let line_str = rope.line(line_idx);
    let cursor_off = pos - line_start;
    let tab_width = app.config.editor.tab_width;
    let mut display_col: usize = 0;
    for i in 0..cursor_off {
        let c = line_str.char(i);
        display_col += if c == '\t' {
            tab_width - (display_col % tab_width)
        } else {
            unicode_display_width(c)
        };
    }

    if display_col < app.scroll_col {
        app.scroll_col = display_col;
    }
    if display_col >= app.scroll_col + visible_cols {
        app.scroll_col = display_col.saturating_sub(visible_cols) + 1;
    }
}

fn unicode_display_width(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(1)
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
        let config = Config::load().expect("failed to load config");
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

    #[test]
    fn test_delete_selection_clamping() {
        let config = Config::load().expect("failed to load config");
        let mut app = App::new(None, config).unwrap();
        app.buffer.rope = Rope::from_str("abc");
        app.selection = Selection::new(0, 2);
        text::delete_selection(&mut app);
        assert_eq!(app.buffer.rope.len_chars(), 0);
        assert_eq!(app.selection.head, 0);
    }
}
