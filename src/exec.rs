use ropey::Rope;

use crate::{
    app::App,
    command::Command,
    mode::{FindDir, Mode},
    motion,
    notebook::{Cell, CellType},
    notebook_state::NotebookEditMode,
    selection::Selection,
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
        // --- Motions ---
        Command::MoveLeft => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_left(rope, app.selection, extend);
        }
        Command::MoveRight => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_right(rope, app.selection, extend);
        }
        Command::MoveUp => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_up(rope, app.selection, extend);
        }
        Command::MoveDown => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_down(rope, app.selection, extend);
        }
        Command::MoveWordForward => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_word_forward(rope, app.selection, extend);
        }
        Command::MoveWordBackward => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_word_backward(rope, app.selection, extend);
        }
        Command::MoveWordEnd => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_word_end(rope, app.selection, extend);
        }
        Command::MoveBigWordForward => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_big_word_forward(rope, app.selection, extend);
        }
        Command::MoveBigWordBackward => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_big_word_backward(rope, app.selection, extend);
        }
        Command::MoveBigWordEnd => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_big_word_end(rope, app.selection, extend);
        }
        Command::MoveLineStart => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_line_start(rope, app.selection, extend);
        }
        Command::MoveLineFirstNonWs => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_line_first_non_ws(rope, app.selection, extend);
        }
        Command::MoveLineEnd => {
            let rope = &app.buffer.rope;
            app.selection = motion::move_line_end(rope, app.selection, extend);
        }
        Command::GotoFileStart => {
            let rope = &app.buffer.rope;
            app.selection = motion::goto_file_start(rope, app.selection, extend);
        }
        Command::GotoFileEnd => {
            let rope = &app.buffer.rope;
            app.selection = motion::goto_file_end(rope, app.selection, extend);
        }
        Command::GotoLine(n) => {
            let rope = &app.buffer.rope;
            app.selection = motion::goto_line(rope, app.selection, *n, extend);
        }
        Command::SelectLine => {
            let rope = &app.buffer.rope;
            app.selection = motion::select_line(rope, app.selection);
        }
        Command::SelectAll => {
            let rope = &app.buffer.rope;
            app.selection = motion::select_all(rope);
        }

        // --- Popup / UI ---
        Command::OpenCommandPalette => {
            app.popup = Some(crate::popup::Popup::command_palette(
                crate::popup::command_palette_items(),
            ));
            return;
        }

        // --- Sub-mode entries (return early — no scroll update) ---
        Command::EnterGotoMode => {
            app.mode = Mode::Goto;
            app.popup = Some(crate::popup::Popup::which_key(
                "g",
                vec![("g".into(), "go to file start".into())],
            ));
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
                    let li = rope.char_to_line(pos.min(rope.len_chars().saturating_sub(1)));
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
            if let Some((ref nb, ref mut state)) = app.notebook {
                let last = nb.cells.len().saturating_sub(1);
                state.focused_cell = (state.focused_cell + 1).min(last);
                state.cursor_pos = 0;
                state.ensure_focused_visible();
            }
            return;
        }
        Command::NotebookPrevCell => {
            if let Some((_, ref mut state)) = app.notebook {
                state.focused_cell = state.focused_cell.saturating_sub(1);
                state.cursor_pos = 0;
                state.ensure_focused_visible();
            }
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
        Command::NotebookEnterEdit => {
            if let Some((_, ref mut state)) = app.notebook {
                state.mode = NotebookEditMode::Edit;
                state.insert_session_active = false;
            }
            return;
        }
        Command::NotebookExitEdit => {
            if let Some((_, ref mut state)) = app.notebook {
                state.mode = NotebookEditMode::Navigate;
                state.insert_session_active = false;
            }
            return;
        }
        Command::NotebookExecuteCell => {
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
        Command::NotebookNewCellBelow => {
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
                state.cursor_pos = 0;
                nb.modified = true;
                state.ensure_focused_visible();
            }
            return;
        }
        Command::NotebookNewCellAbove => {
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                let new_idx = state.focused_cell;
                nb.cells.insert(new_idx, Cell {
                    id: new_cell_id(),
                    cell_type: CellType::Code,
                    source: Rope::new(),
                    outputs: vec![],
                    execution_count: None,
                });
                state.cursor_pos = 0;
                nb.modified = true;
                state.ensure_focused_visible();
            }
            return;
        }
        Command::NotebookDeleteCell => {
            if let Some((ref mut nb, ref mut state)) = app.notebook {
                if !nb.cells.is_empty() {
                    nb.cells.remove(state.focused_cell);
                    nb.modified = true;
                    let last = nb.cells.len().saturating_sub(1);
                    state.focused_cell = state.focused_cell.min(last);
                    state.cursor_pos = 0;
                }
            }
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
            if let Some((ref nb, ref state)) = app.notebook {
                let idx = state.focused_cell;
                if idx >= nb.cells.len() { return; }

                let cell = &nb.cells[idx];
                let language = nb.metadata.kernel_language.clone();
                let cell_id  = cell.id.clone();
                let notebook_path = nb.path.clone();

                // Virtual path gives tree-sitter the right extension without
                // touching the filesystem. This path also becomes the LSP
                // textDocument URI when LSP is wired up.
                let ext = lang_ext(&language);
                let stem = notebook_path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "notebook".into());
                let dir = notebook_path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                    });
                let virtual_path = dir.join(format!("{stem}__cell{idx}.{ext}"));

                // Check out cell source into app.buffer.
                app.buffer = crate::buffer::Buffer::new_empty();
                app.buffer.rope = cell.source.clone();
                app.buffer.path = Some(virtual_path.clone());
                app.selection = crate::selection::Selection::point(0);
                app.mode = crate::mode::Mode::Normal;
                app.insert_session_active = false;
                app.scroll_row = 0;
                app.scroll_col = 0;

                app.notebook_cell_edit = Some(crate::app::CellEditSession {
                    cell_index: idx,
                    cell_id,
                    language,
                    notebook_path,
                });

                // Update highlighter for the cell's language.
                app.highlighter = crate::highlight::Highlighter::new(Some(&virtual_path));
                recompute_highlights(app);
            }
            return;
        }

        Command::NotebookCloseCellEdit => {
            if let Some(session) = app.notebook_cell_edit.take() {
                // Write buffer back to the notebook cell.
                if let Some((ref mut nb, _)) = app.notebook {
                    let idx = session.cell_index;
                    if idx < nb.cells.len() {
                        nb.cells[idx].source = app.buffer.rope.clone();
                        nb.modified = true;
                    }
                }
                reset_after_cell_edit(app);
            }
            return;
        }

        Command::NotebookDiscardCellEdit => {
            app.notebook_cell_edit = None;
            reset_after_cell_edit(app);
            return;
        }
    }

    update_scroll(app);
}

/// Clear buffer and restore editor state after closing a cell-edit overlay.
fn reset_after_cell_edit(app: &mut App) {
    app.buffer = crate::buffer::Buffer::new_empty();
    app.selection = crate::selection::Selection::point(0);
    app.mode = crate::mode::Mode::Normal;
    app.insert_session_active = false;
    app.scroll_row = 0;
    app.scroll_col = 0;
    app.highlighter = crate::highlight::Highlighter::new(None);
    app.highlight_spans = Vec::new();
}

fn lang_ext(lang: &str) -> &str {
    match lang {
        "python" | "python3" => "py",
        "javascript" | "js" => "js",
        "rust" => "rs",
        _ => "txt",
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

    let pos = app.selection.head.min(rope.len_chars().saturating_sub(1));
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
    let new_pos = start.min(app.buffer.rope.len_chars().saturating_sub(1));
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
        let line_idx = rope.char_to_line(pos.min(rope.len_chars().saturating_sub(1)));
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
        let line_idx = rope.char_to_line(pos.min(rope.len_chars().saturating_sub(1)));
        rope.line_to_char(line_idx)
    };
    app.buffer.insert(ls, "\n");
    app.selection = Selection::point(ls);
    app.mode = Mode::Insert;
    recompute_highlights(app);
    update_scroll(app);
}

fn clamp_selection(app: &mut App) {
    let len = app.buffer.rope.len_chars();
    let head = app.selection.head.min(len.saturating_sub(1));
    let anchor = app.selection.anchor.min(len.saturating_sub(1));
    app.selection = Selection::new(anchor, head);
}
