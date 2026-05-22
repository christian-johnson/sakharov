use ratatui::{
    buffer::Buffer as RatBuffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::Widget,
    Frame,
};
use unicode_width::UnicodeWidthChar;

use crate::{
    app::App,
    git::GutterMark,
    highlight,
    lang::lang_to_ext,
    lsp_manager::DiagnosticSeverity,
    mode::Mode,
    theme,
};

/// Render the full UI into the frame.
pub fn render(frame: &mut Frame, app: &App) {
    let size = frame.area();
    if size.height < 3 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(size);

    let lines_area = chunks[0];
    let status_area = chunks[1];
    let cmd_area = chunks[2];

    render_lines(frame, app, lines_area);
    render_status(frame, app, status_area);
    render_command(frame, app, cmd_area);

    // Position the hardware cursor so the terminal blinks at the right spot.
    if let Some((cx, cy)) = cursor_screen_pos(app, lines_area) {
        frame.set_cursor_position((cx, cy));
    }
}

// ---------------------------------------------------------------------------
// Lines area
// ---------------------------------------------------------------------------

fn render_lines(frame: &mut Frame, app: &App, area: Rect) {
    // Git gutter is 1 char wide (only for regular files, not notebooks).
    let git_col: u16 = if app.config.editor.git_gutter && app.notebook.is_none() { 1 } else { 0 };
    let line_num_width: u16 = if app.config.editor.line_numbers { 5 } else { 0 };
    let gutter_width: u16 = git_col + line_num_width;
    // 1-column right diagnostic gutter (only when there are diagnostics).
    let has_diags = app.buffer.path.as_ref()
        .map(|p| app.lsp.diagnostics.contains_key(&p.to_string_lossy().to_string()))
        .unwrap_or(false);
    let right_gutter: u16 = if has_diags { 1 } else { 0 };
    let text_width = area.width.saturating_sub(gutter_width + right_gutter) as usize;
    let visible_rows = area.height as usize;

    // Pre-compute per-line diagnostic ranges for the current file.
    let diag_by_line: std::collections::HashMap<usize, Vec<(usize, usize, DiagnosticSeverity)>> = {
        let mut map = std::collections::HashMap::new();
        if let Some(ref path) = app.buffer.path {
            let key = path.to_string_lossy().to_string();
            if let Some(diags) = app.lsp.diagnostics.get(&key) {
                for d in diags {
                    map.entry(d.line)
                        .or_insert_with(Vec::new)
                        .push((d.col_start, d.col_end, d.severity.clone()));
                }
            }
        }
        map
    };

    let rope = &app.buffer.rope;
    let total_lines = rope.len_lines();
    let scroll_row = app.scroll_row;

    // Build highlight map: char_index -> style
    // We store spans sorted; during rendering we pick the active style per char.
    let spans = &app.highlight_spans;

    let sel_start = app.selection.start();
    let sel_end = app.selection.end();
    let cursor_pos = app.selection.head;

    for row in 0..visible_rows {
        let line_idx = scroll_row + row;
        let y = area.top() + row as u16;

        // --- Gutter ---
        if gutter_width > 0 {
            let mut gx = area.left();
            let buf = frame.buffer_mut();

            // Git mark column (1 char).
            if git_col > 0 {
                let (mark_ch, mark_color) = match app.git_diff.get(&line_idx) {
                    Some(GutterMark::Added)    => ('+', Color::Green),
                    Some(GutterMark::Modified) => ('~', Color::Yellow),
                    None                       => (' ', Color::DarkGray),
                };
                buf[(gx, y)]
                    .set_char(mark_ch)
                    .set_style(Style::default().fg(mark_color));
                gx += 1;
            }

            // Line number column (5 chars).
            if line_num_width > 0 && line_idx < total_lines {
                let line_num_str = if app.config.editor.relative_line_numbers {
                    let cursor_line = rope
                        .char_to_line(app.selection.head.min(rope.len_chars()));
                    if line_idx == cursor_line {
                        format!("{:4} ", line_idx + 1)
                    } else {
                        let dist = (line_idx as isize - cursor_line as isize).unsigned_abs();
                        format!("{:4} ", dist)
                    }
                } else {
                    format!("{:4} ", line_idx + 1)
                };
                let num_style = Style::default().fg(Color::DarkGray);
                for c in line_num_str.chars() {
                    if gx >= area.left() + gutter_width {
                        break;
                    }
                    buf[(gx, y)].set_char(c).set_style(num_style);
                    gx += 1;
                }
            } else if line_num_width > 0 {
                // Past end of file — blank line number.
                let num_style = Style::default().fg(Color::DarkGray);
                for _ in 0..line_num_width {
                    if gx >= area.left() + gutter_width {
                        break;
                    }
                    buf[(gx, y)].set_char(' ').set_style(num_style);
                    gx += 1;
                }
            }
        }

        // --- Text content ---
        let text_area = Rect {
            x: area.left() + gutter_width,
            y,
            width: area.width.saturating_sub(gutter_width),
            height: 1,
        };

        if line_idx >= total_lines {
            // Past end of file
            let empty = EmptyLineWidget;
            frame.render_widget(empty, text_area);
            continue;
        }

        let line_start_char = rope.line_to_char(line_idx);
        let line_str = rope.line(line_idx);
        let line_len = line_str.len_chars();

        // Build character cells for this line
        let mut cells: Vec<(char, Style)> = Vec::new();
        let mut col_offset = 0usize;
        let tab_width = app.config.editor.tab_width;

        for char_off in 0..line_len {
            let char_idx = line_start_char + char_off;
            let c = line_str.char(char_off);

            // Skip if before scroll col
            if col_offset < app.scroll_col {
                let w = char_display_width(c, col_offset, tab_width);
                col_offset += w;
                continue;
            }

            // Past visible width
            if col_offset - app.scroll_col >= text_width {
                break;
            }

            // Determine base style from highlights
            let base_style = highlight::style_at(spans, char_idx);

            // Apply selection or cursor overlay.
            let style = if char_idx == cursor_pos {
                theme::cursor_style(&app.mode)
            } else if char_idx >= sel_start && char_idx <= sel_end && sel_start != sel_end {
                theme::selection_style()
            } else {
                base_style
            };

            // Diagnostic underline (does not override cursor/selection colours).
            let char_off = char_idx - line_start_char;
            let style = if let Some(line_diags) = diag_by_line.get(&line_idx) {
                let worst = line_diags
                    .iter()
                    .filter(|(cs, ce, _)| char_off >= *cs && char_off < *ce)
                    .fold(None::<&DiagnosticSeverity>, |acc, (_, _, sev)| {
                        Some(match acc {
                            Some(DiagnosticSeverity::Error) => &DiagnosticSeverity::Error,
                            _ => sev,
                        })
                    });
                match worst {
                    Some(DiagnosticSeverity::Error) => style
                        .add_modifier(ratatui::style::Modifier::UNDERLINED)
                        .underline_color(Color::Red),
                    Some(_) => style
                        .add_modifier(ratatui::style::Modifier::UNDERLINED)
                        .underline_color(Color::Yellow),
                    None => style,
                }
            } else {
                style
            };

            if c == '\n' || c == '\r' {
                if char_idx == cursor_pos {
                    let cs = theme::cursor_style(&app.mode);
                    cells.push((' ', cs));
                }
                break;
            } else if c == '\t' {
                let w = tab_stop(col_offset, tab_width);
                for i in 0..w {
                    if col_offset - app.scroll_col + i < text_width {
                        cells.push((' ', style));
                    }
                }
                col_offset += w;
            } else {
                let w = c.width().unwrap_or(1);
                cells.push((c, style));
                col_offset += w;
            }
        }

        // Jump label overlay — patch cells in-place before rendering.
        if app.mode == crate::mode::Mode::Jump && !app.jump_labels.is_empty() {
            let typed_len = app.jump_typed.len();
            let jump_pending = Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(255, 160, 0))
                .add_modifier(Modifier::BOLD);
            let jump_confirmed = Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD);

            for &(pos, ref label) in &app.jump_labels {
                // Skip labels that don't match the typed prefix.
                if !label.starts_with(app.jump_typed.as_str()) {
                    continue;
                }
                if pos < line_start_char {
                    continue;
                }
                let char_off = pos - line_start_char;
                if char_off >= line_len {
                    continue;
                }

                // Compute display column for this char offset.
                let display_col = {
                    let mut col = 0usize;
                    for i in 0..char_off {
                        let c = line_str.char(i);
                        col += char_display_width(c, col, tab_width);
                    }
                    col
                };

                if display_col < app.scroll_col {
                    continue;
                }
                let cell_idx = display_col - app.scroll_col;

                for (j, lc) in label.chars().enumerate() {
                    let idx = cell_idx + j;
                    if idx >= cells.len() {
                        break;
                    }
                    let style = if j < typed_len { jump_confirmed } else { jump_pending };
                    cells[idx] = (lc, style);
                }
            }
        }

        let line_widget = LineWidget { cells };
        frame.render_widget(line_widget, text_area);

        // Right diagnostic gutter marker.
        if right_gutter > 0 && line_idx < total_lines {
            let rx = area.right() - 1;
            let buf = frame.buffer_mut();
            if let Some(line_diags) = diag_by_line.get(&line_idx) {
                let has_error = line_diags.iter().any(|(_, _, s)| *s == DiagnosticSeverity::Error);
                let has_warn  = line_diags.iter().any(|(_, _, s)| *s == DiagnosticSeverity::Warning);
                let (ch, color) = if has_error {
                    ('●', Color::Red)
                } else if has_warn {
                    ('◆', Color::Yellow)
                } else {
                    (' ', Color::Reset)
                };
                buf[(rx, y)].set_char(ch).set_style(Style::default().fg(color));
            } else {
                buf[(rx, y)].set_char(' ').set_style(Style::default());
            }
        }
    }
}

/// Compute the terminal (col, row) of the cursor for `frame.set_cursor_position`.
/// Returns None if the cursor is scrolled off screen.
pub fn cursor_screen_pos(app: &App, lines_area: Rect) -> Option<(u16, u16)> {
    let git_col: u16 = if app.config.editor.git_gutter && app.notebook.is_none() { 1 } else { 0 };
    let line_num_width: u16 = if app.config.editor.line_numbers { 5 } else { 0 };
    let gutter_width = git_col + line_num_width;

    let rope = &app.buffer.rope;
    if rope.len_chars() == 0 {
        return Some((lines_area.left() + gutter_width, lines_area.top()));
    }

    let head = app.selection.head.min(rope.len_chars());
    let line_idx = rope.char_to_line(head);

    if line_idx < app.scroll_row {
        return None;
    }
    let screen_row = line_idx - app.scroll_row;
    if screen_row >= lines_area.height as usize {
        return None;
    }

    let line_start = rope.line_to_char(line_idx);
    let line_str = rope.line(line_idx);
    let cursor_off = head - line_start;
    let tab_width = app.config.editor.tab_width;

    let mut col: usize = 0;
    for i in 0..cursor_off {
        let c = line_str.char(i);
        col += if c == '\t' { tab_stop(col, tab_width) } else { c.width().unwrap_or(1) };
    }

    let text_col = col.saturating_sub(app.scroll_col);
    let text_width = lines_area.width.saturating_sub(gutter_width) as usize;
    if text_col >= text_width {
        return None;
    }

    let screen_x = lines_area.left() + gutter_width + text_col as u16;
    let screen_y = lines_area.top() + screen_row as u16;
    Some((screen_x, screen_y))
}

fn char_display_width(c: char, col: usize, tab_width: usize) -> usize {
    if c == '\t' {
        tab_stop(col, tab_width)
    } else {
        c.width().unwrap_or(1)
    }
}

fn tab_stop(col: usize, tab_width: usize) -> usize {
    tab_width - (col % tab_width)
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let mode_label = app.mode.label();
    let mode_style = Style::default()
        .fg(Color::Black)
        .bg(theme::mode_color(&app.mode))
        .add_modifier(Modifier::BOLD);

    // When in a cell-edit overlay, show the notebook + cell context instead of
    // the virtual buffer path. Ctrl+Enter hint keeps the affordance visible.
    let (filename, modified) = if let Some(ref session) = app.notebook_cell_edit {
        let nb_name = session.notebook_path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("notebook");
        let ext = lang_to_ext(&session.language);
        let m = if app.buffer.modified { " [+]" } else { "" };
        (
            format!("{nb_name}  ·  cell [{}].{ext}", session.cell_index + 1),
            m.to_string(),
        )
    } else {
        (app.buffer.display_name(), if app.buffer.modified { " [+]".into() } else { String::new() })
    };

    let rope = &app.buffer.rope;
    let cursor_pos = app.selection.head.min(rope.len_chars());
    let line_idx = if rope.len_chars() == 0 {
        0
    } else {
        rope.char_to_line(cursor_pos)
    };
    let line_start = if rope.len_chars() == 0 {
        0
    } else {
        rope.line_to_char(line_idx)
    };
    let col = cursor_pos.saturating_sub(line_start) + 1;
    let line_num = line_idx + 1;

    let total_lines = rope.len_lines().max(1);
    let scroll_pct = (line_idx * 100) / total_lines;

    // Diagnostic counts for the current file (split by severity for coloring).
    let (diag_errors, diag_warnings) = if let Some(ref path) = app.buffer.path {
        let path_str = path.to_string_lossy().to_string();
        if let Some(diags) = app.lsp.diagnostics.get(&path_str) {
            let e = diags.iter().filter(|d| d.severity == DiagnosticSeverity::Error).count();
            let w = diags.iter().filter(|d| d.severity == DiagnosticSeverity::Warning).count();
            (e, w)
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };

    let position = format!("  {}:{}  {}%", line_num, col, scroll_pct);

    let status_widget = StatusWidget {
        mode_label: format!(" {mode_label} "),
        mode_style,
        branch: app.git_branch.clone(),
        filename: format!(" {filename}{modified} "),
        diag_errors,
        diag_warnings,
        position,
    };
    frame.render_widget(status_widget, area);
}

// ---------------------------------------------------------------------------
// Command/message line — notebook variant (public)
// ---------------------------------------------------------------------------

/// Render the command/message line for notebook mode.
pub fn render_command_nb(frame: &mut Frame, app: &App, area: Rect) {
    render_command(frame, app, area);
}

// ---------------------------------------------------------------------------
// Command/message line
// ---------------------------------------------------------------------------

fn render_command(frame: &mut Frame, app: &App, area: Rect) {
    let text = match &app.mode {
        Mode::Jump => {
            if app.jump_typed.is_empty() {
                "Jump: type label chars...".to_string()
            } else {
                format!("Jump: {}_", app.jump_typed)
            }
        }
        Mode::Command => format!(":{}", app.command_buf),
        Mode::Search { forward } => {
            let prefix = if *forward { '/' } else { '?' };
            let count = app.search.matches.len();
            if app.search.query.is_empty() {
                format!("{prefix}")
            } else if count == 0 {
                format!("{prefix}{} [no matches]", app.search.query)
            } else {
                format!(
                    "{prefix}{} [{}/{}]",
                    app.search.query,
                    app.search.current + 1,
                    count,
                )
            }
        }
        _ => app.message.clone().unwrap_or_default(),
    };
    let cmd_widget = SingleLineWidget {
        text,
        style: Style::default(),
    };
    frame.render_widget(cmd_widget, area);
}

// ---------------------------------------------------------------------------
// Custom widgets
// ---------------------------------------------------------------------------


struct EmptyLineWidget;

impl Widget for EmptyLineWidget {
    fn render(self, _area: Rect, _buf: &mut RatBuffer) {}
}

struct LineWidget {
    cells: Vec<(char, Style)>,
}

impl Widget for LineWidget {
    fn render(self, area: Rect, buf: &mut RatBuffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut x = area.left();
        for (c, style) in self.cells {
            if x >= area.right() {
                break;
            }
            let w = c.width().unwrap_or(1) as u16;
            buf[(x, area.top())].set_char(c).set_style(style);
            x += w;
        }
    }
}

struct StatusWidget {
    mode_label: String,
    mode_style: Style,
    /// Git branch name (e.g. "main").
    branch: Option<String>,
    filename: String,
    diag_errors: usize,
    diag_warnings: usize,
    /// Right-aligned position text e.g. "  42:10  23%"
    position: String,
}

impl Widget for StatusWidget {
    fn render(self, area: Rect, buf: &mut RatBuffer) {
        if area.height == 0 {
            return;
        }
        let y = area.top();
        let bg = Style::default().bg(Color::DarkGray).fg(Color::White);

        // Fill background.
        for col in area.left()..area.right() {
            buf[(col, y)].set_char(' ').set_style(bg);
        }

        // Left side: mode label.
        let mut x = area.left();
        for c in self.mode_label.chars() {
            if x >= area.right() { break; }
            buf[(x, y)].set_char(c).set_style(self.mode_style);
            x += 1;
        }

        // Branch name (dimmed, with  prefix).
        if let Some(ref branch) = self.branch {
            let branch_str = format!("  \u{e0a0} {branch}"); // nerd-font branch icon
            let branch_style = Style::default().bg(Color::DarkGray).fg(Color::Rgb(170, 170, 170));
            for c in branch_str.chars() {
                if x >= area.right() { break; }
                buf[(x, y)].set_char(c).set_style(branch_style);
                x += 1;
            }
        }

        // Filename.
        for c in self.filename.chars() {
            if x >= area.right() { break; }
            buf[(x, y)].set_char(c).set_style(bg);
            x += 1;
        }

        // Right side: build colored segments and measure total width.
        // Segments are rendered right-to-left (position → warnings → errors).
        let mut segments: Vec<(String, Style)> = Vec::new();
        segments.push((self.position.clone(), bg));
        if self.diag_warnings > 0 {
            let s = format!("  ◆{}", self.diag_warnings);
            segments.push((s, Style::default().bg(Color::DarkGray).fg(Color::Yellow)));
        }
        if self.diag_errors > 0 {
            let s = format!("  ●{}", self.diag_errors);
            segments.push((s, Style::default().bg(Color::DarkGray).fg(Color::Red)));
        }

        let total_right: u16 = segments.iter()
            .map(|(s, _)| s.chars().count() as u16)
            .sum();

        if area.right() >= total_right {
            let mut rx = area.right() - total_right;
            for (text, style) in segments {
                for c in text.chars() {
                    if rx >= area.right() { break; }
                    buf[(rx, y)].set_char(c).set_style(style);
                    rx += 1;
                }
            }
        }
    }
}

struct SingleLineWidget {
    text: String,
    style: Style,
}

impl Widget for SingleLineWidget {
    fn render(self, area: Rect, buf: &mut RatBuffer) {
        if area.height == 0 {
            return;
        }
        let y = area.top();
        let mut x = area.left();

        // Fill with spaces first
        for col in area.left()..area.right() {
            buf[(col, y)].set_char(' ').set_style(self.style);
        }

        for c in self.text.chars() {
            if x >= area.right() {
                break;
            }
            buf[(x, y)].set_char(c).set_style(self.style);
            x += 1;
        }
    }
}
