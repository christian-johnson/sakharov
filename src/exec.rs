use crate::{
    app::App,
    command::Command,
    mode::{FindDir, Mode},
    motion,
    selection::Selection,
};

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

        // --- Sub-mode entries (return early — no scroll update) ---
        Command::EnterGotoMode => {
            app.mode = Mode::Goto;
            return;
        }
        Command::FindCharForward => {
            app.mode = Mode::FindChar {
                dir: FindDir::Forward,
                till: false,
            };
            return;
        }
        Command::TillCharForward => {
            app.mode = Mode::FindChar {
                dir: FindDir::Forward,
                till: true,
            };
            return;
        }
        Command::FindCharBackward => {
            app.mode = Mode::FindChar {
                dir: FindDir::Backward,
                till: false,
            };
            return;
        }
        Command::TillCharBackward => {
            app.mode = Mode::FindChar {
                dir: FindDir::Backward,
                till: true,
            };
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
            match app.buffer.save(None) {
                Ok(()) => {
                    app.message = Some(format!("Saved {}", app.buffer.display_name()));
                }
                Err(e) => {
                    app.message = Some(format!("Error: {e}"));
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
            if app.buffer.modified {
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
