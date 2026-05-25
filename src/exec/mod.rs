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
    jump,
    lsp_manager::LspRequestKind,
    mode::{FindDir, Mode},
    motion,
    notebook::{Cell, CellType},
    selection::Selection,
    symbols,
};

// ---------------------------------------------------------------------------
// Special buffers
// ---------------------------------------------------------------------------

const SCRATCH_INTRO: &str = "\
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

    // Tear down any open notebook.
    if app.notebook.is_some() {
        notebook::notebook_lsp_close(app);
        app.notebook = None;
        app.notebook_cell_edit = None;
    }

    // Close LSP for the current plain-text buffer.
    if let (Some(ref lang), Some(ref old_path)) =
        (app.lsp_language.clone(), app.buffer.path.clone())
    {
        if !is_special_path(old_path) {
            app.lsp.did_close(lang, old_path);
        }
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

            // Try ripgrep first, fall back to grep.  Cache the availability
            // check so we don't spawn a process on every invocation.
            let rg_available = rg_is_available();

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
        Command::OpenFilePicker => {
            let picker_cmd = app.config.editor.file_picker.clone();
            if let Some(cmd) = picker_cmd {
                open_file_external_picker(app, &cmd);
            } else {
                open_file_picker_popup(app);
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
            let positions =
                jump::visible_word_starts(&app.buffer.rope, app.scroll_row, app.viewport_height);
            app.jump_labels = jump::generate_labels(&positions);
            app.jump_typed = String::new();
            app.popup = None;
            app.mode = Mode::Jump;
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
        Command::Write => {
            if app.buffer.path.as_deref().map(is_special_path).unwrap_or(false) {
                app.message = Some("Special buffer — nothing to save".into());
                return;
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
                match app.buffer.save(None) {
                    Ok(()) => {
                        app.message = Some(format!("Saved {}", app.buffer.display_name()));
                        if let Some(ref path) = app.buffer.path.clone() {
                            app.git_diff = crate::git::diff_marks(path);
                            app.git_branch = crate::git::current_branch();
                        }
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
            match app.buffer.save(Some(&path)) {
                Ok(()) => {
                    app.message = Some(format!("Saved {path}"));
                    if let Some(ref p) = app.buffer.path {
                        app.git_diff = crate::git::diff_marks(p);
                        app.git_branch = crate::git::current_branch();
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
                app.message = Some("Unsaved changes — use :w to write, :q! to force quit".to_string());
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
            if app.buffer.path.as_deref().map(is_special_path).unwrap_or(false) {
                app.should_quit = true;
                return;
            }
            if app.notebook.is_some() {
                notebook::save_focused_cell(app);
                if let Some((ref mut nb, _)) = app.notebook {
                    match nb.save() {
                        Ok(()) => app.should_quit = true,
                        Err(e) => app.message = Some(format!("Error: {e}")),
                    }
                }
            } else {
                match app.buffer.save(None) {
                    Ok(()) => app.should_quit = true,
                    Err(e) => app.message = Some(format!("Error: {e}")),
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
                app.notebook_cell_edit = None;
            } else if let (Some(ref lang), Some(ref old_path)) =
                (app.lsp_language.clone(), app.buffer.path.clone())
            {
                app.lsp.did_close(lang, old_path);
            }

            // Remove the closed buffer from the buffer list.
            if let Some(ref p) = path_to_remove {
                let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
                app.open_buffers.retain(|stored| {
                    let sc = stored.canonicalize().unwrap_or_else(|_| stored.clone());
                    sc != canon && stored != p
                });
            }

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
                        Ok(found_venv) => {
                            if !found_venv {
                                app.message = Some(
                                    "Kernel started (no venv found — using system python3)".into(),
                                );
                            }
                        }
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
                    Ok(found_venv) => {
                        app.message = Some(if found_venv {
                            "Kernel restarted".into()
                        } else {
                            "Kernel restarted (no venv found — using system python3)".into()
                        });
                    }
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
                let new_idx = (state.focused_cell + 1).min(nb.cells.len());
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
        Command::LspShowDocumentation => { lsp::lsp_request(app, LspRequestKind::Hover);          return; }
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
    use crate::{
        app::CellEditSession,
        buffer::Buffer,
        highlight::Highlighter,
        lang::lang_to_ext,
        notebook::Notebook,
        notebook_state::NotebookState,
    };

    // Save scratch content when leaving it.
    save_current_special_buffer(app);

    // Tear down any existing notebook state.
    if app.notebook.is_some() {
        notebook::notebook_lsp_close(app);
        app.notebook = None;
        app.notebook_cell_edit = None;
    }
    // Close the currently-open plain file from LSP perspective.
    if let (Some(ref lang), Some(ref old_path)) =
        (app.lsp_language.clone(), app.buffer.path.clone())
    {
        app.lsp.did_close(lang, old_path);
    }

    let nb = match Notebook::from_path(path) {
        Ok(n) => n,
        Err(e) => {
            app.message = Some(format!("Failed to open notebook: {e}"));
            return;
        }
    };

    let lang = nb.metadata.kernel_language.clone();
    let ext = lang_to_ext(&lang);
    let stem = nb.path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "notebook".into());
    let dir = nb.path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let vpath = dir.join(format!("{stem}__cell0.{ext}"));

    let mut buf = Buffer::new_empty();
    if let Some(cell) = nb.cells.first() {
        buf.rope = cell.source.clone();
    }
    buf.path = Some(vpath.clone());

    let session = nb.cells.first().map(|_| CellEditSession {
        cell_index: 0,
        language: lang.clone(),
        notebook_path: nb.path.clone(),
        focused_edit: false,
    });

    let nb_path_canon = nb.path.canonicalize().unwrap_or_else(|_| nb.path.clone());

    app.buffer = buf;
    app.notebook = Some((nb, NotebookState::new()));
    app.notebook_cell_edit = session;
    app.selection = Selection::point(0);
    app.scroll_row = 0;
    app.scroll_col = 0;
    app.mode = Mode::Notebook;
    app.lsp_language = Some(lang.clone());
    app.highlighter = Highlighter::new(Some(&vpath));
    recompute_highlights(app);

    if !app.open_buffers.iter().any(|p| {
        p.canonicalize().unwrap_or_else(|_| p.clone()) == nb_path_canon
    }) {
        app.open_buffers.push(nb_path_canon);
    }

    app.message = Some(format!(
        "Opened {}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
    ));
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

/// Rebuild the per-line diagnostic cache for the current buffer.
/// Call this after diagnostics change or after switching files.
pub fn rebuild_diag_cache(app: &mut App) {
    app.diag_by_line.clear();
    if let Some(ref path) = app.buffer.path {
        let key = path.to_string_lossy().to_string();
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

    // Normalize scroll_row so it never points inside a hidden fold region.
    app.scroll_row = app.fold.normalize_scroll_row(app.scroll_row);

    // Vertical — fold-aware.
    // Count visible rows from scroll_row to cursor.
    let vdist = app.fold.visible_row_count(app.scroll_row, line_idx, total_lines);

    if vdist < scroll_off || app.scroll_row > line_idx {
        // Cursor too close to top (or above scroll area): scroll up.
        let desired = scroll_off.min(line_idx);
        app.scroll_row = app.fold.scroll_row_for_cursor(line_idx, desired);
    } else if vdist + scroll_off >= visible_rows {
        // Cursor too close to bottom: scroll down.
        let desired = visible_rows.saturating_sub(scroll_off + 1);
        app.scroll_row = app.fold.scroll_row_for_cursor(line_idx, desired);
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
// File picker helpers
// ---------------------------------------------------------------------------

fn rg_is_available() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_ok()
    })
}

/// Open a fuzzy-filterable file list popup from the project root.
fn open_file_picker_popup(app: &mut App) {
    let root = std::env::current_dir()
        .unwrap_or_else(|_| {
            app.buffer.path.as_deref()
                .and_then(|p| p.parent())
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_default()
        });

    let mut items: Vec<crate::popup::ListItem> = Vec::new();
    collect_files(&root, &root, &mut items, 0);
    items.sort_by(|a, b| a.label.cmp(&b.label));

    if items.is_empty() {
        app.message = Some("No files found".into());
        return;
    }

    app.popup = Some(crate::popup::Popup::navigate("open file", items));
}

/// Recursively collect files under `dir` relative to `base`, skipping noise.
fn collect_files(
    base: &std::path::Path,
    dir: &std::path::Path,
    items: &mut Vec<crate::popup::ListItem>,
    depth: usize,
) {
    const MAX_DEPTH: usize = 10;
    const MAX_FILES: usize = 2000;
    if depth > MAX_DEPTH || items.len() >= MAX_FILES {
        return;
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else { return };

    let mut entries: Vec<_> = read_dir.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        if items.len() >= MAX_FILES {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            if matches!(
                name_str.as_ref(),
                "target" | "node_modules" | "__pycache__" | "dist" | "build" | "out"
            ) {
                continue;
            }
            collect_files(base, &path, items, depth + 1);
        } else {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let label = rel.to_string_lossy().into_owned();
            let abs = path.canonicalize().unwrap_or_else(|_| path.clone());
            items.push(crate::popup::ListItem::navigate(label, abs.to_string_lossy(), &abs, 0, 0));
        }
    }
}

/// Suspend the TUI, run an external picker command, then resume.
///
/// The command receives:
///   MJ_PICKER_FILE  — path to a temp file; write the chosen file path there
///                     (preferred for TUI pickers like yazi that own the screen)
///   MJ_CURRENT_DIR  — directory of the currently open buffer
///
/// If MJ_PICKER_FILE is non-empty after the command exits, that path is used.
/// Otherwise the command's stdout is used (works well with fzf).
fn open_file_external_picker(app: &mut App, cmd: &str) {
    use crossterm::{execute, terminal};
    use std::io::{self, Write};

    let tmp_path = std::env::temp_dir()
        .join(format!("mj-picker-{}.txt", std::process::id()));

    let current_dir = app.buffer.path.as_deref()
        .and_then(|p| p.parent())
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // Suspend TUI so the external picker has the full terminal.
    let _ = terminal::disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, crossterm::terminal::LeaveAlternateScreen);
    let _ = stdout.flush();

    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("MJ_PICKER_FILE", &tmp_path)
        .env("MJ_CURRENT_DIR", &current_dir)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output();

    // Resume TUI.
    let _ = execute!(stdout, crossterm::terminal::EnterAlternateScreen);
    let _ = terminal::enable_raw_mode();
    crate::theme::initialize_color_cache();

    // Determine chosen path: temp file wins over stdout.
    let chosen = if tmp_path.exists() {
        let content = std::fs::read_to_string(&tmp_path).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp_path);
        content.trim().to_owned()
    } else {
        match output {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
            Err(e) => {
                app.message = Some(format!("File picker error: {e}"));
                return;
            }
        }
    };

    if !chosen.is_empty() {
        let path = std::path::PathBuf::from(&chosen);
        lsp::open_file_at(app, &path, 0, 0);
    }

    app.needs_clear = true;
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
