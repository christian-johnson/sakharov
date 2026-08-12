use ratatui::{
    buffer::Buffer as RatBuffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
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
    mode::{Mode, PromptKind},
    render_util::{
        apply_diag_underline, char_display_width, diagnostic_at, for_each_jump_label_char,
        SingleLineWidget,
    },
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

/// Number of visual rows a logical line occupies when soft-wrapped to `text_width` columns.
/// Returns 1 when `text_width == 0` (degenerate) or wrapping is not active.
pub(crate) fn visual_line_height(
    rope: &ropey::Rope,
    line_idx: usize,
    text_width: usize,
    tab_width: usize,
) -> usize {
    if text_width == 0 || line_idx >= rope.len_lines() {
        return 1;
    }
    // Counted through the shared wrap rule, so the height the scroll math uses,
    // the rows this renderer draws and the rows `j`/`k` step through are all the
    // same segmentation (see `render_util::scan_wrap_rows`).
    let mut rows = 0usize;
    crate::render_util::scan_wrap_rows(rope.line(line_idx), text_width, tab_width, |_| rows += 1);
    rows
}

/// A single visible screen row: either a normal line sub-row or a fold indicator.
struct VisRow {
    line_idx: usize,
    /// `Some(end)` → this row is a fold indicator hiding lines `line_idx+1..=end`.
    fold_end: Option<usize>,
    /// Which wrapped sub-row of `line_idx` this is (0 = first, always 0 without wrapping).
    sub_row: usize,
}

/// Build the list of visible screen rows, honouring folds and optional word-wrap.
fn build_vis_rows(
    fold: &crate::fold::FoldState,
    rope: &ropey::Rope,
    scroll_row: usize,
    visible_rows: usize,
    word_wrap: bool,
    text_width: usize,
    tab_width: usize,
) -> Vec<VisRow> {
    let total_lines = rope.len_lines();
    let mut rows: Vec<VisRow> = Vec::with_capacity(visible_rows);
    let mut line = scroll_row;

    while rows.len() < visible_rows && line < total_lines {
        if fold.is_hidden(line) {
            line += 1;
            continue;
        }
        if let Some(fold_end) = fold.fold_end_at(line) {
            rows.push(VisRow { line_idx: line, fold_end: Some(fold_end), sub_row: 0 });
            line = fold_end + 1;
        } else {
            let height = if word_wrap && text_width > 0 {
                visual_line_height(rope, line, text_width, tab_width)
            } else {
                1
            };
            for sub in 0..height {
                if rows.len() >= visible_rows {
                    break;
                }
                rows.push(VisRow { line_idx: line, fold_end: None, sub_row: sub });
            }
            line += 1;
        }
    }
    rows
}

/// Gutter/text-column geometry for the plain-editor line area, computed from
/// `area_width` — shared by `render_lines` and `cursor_screen_pos` so a
/// gutter change can never desync cursor placement from what's drawn.
struct LineLayout {
    /// 1 when the git-diff mark column is shown, else 0.
    git_col: u16,
    /// 5 when line numbers are shown, else 0.
    line_num_width: u16,
    /// `git_col + line_num_width`.
    gutter_width: u16,
    /// 1 when the right diagnostic gutter is shown (there are diagnostics), else 0.
    right_gutter: u16,
    /// Display columns left for text after the gutter and the right diagnostic gutter.
    text_width: usize,
}

fn line_layout(app: &App, area_width: u16) -> LineLayout {
    // Git gutter is 1 char wide (only for regular files, not notebooks).
    let git_col: u16 = if app.config.editor.git_gutter && app.notebook.is_none() { 1 } else { 0 };
    let line_num_width: u16 = if app.config.editor.line_numbers { 5 } else { 0 };
    let gutter_width: u16 = git_col + line_num_width;
    // 1-column right diagnostic gutter (only when there are diagnostics).
    let has_diags = !app.diag_by_line.is_empty();
    let right_gutter: u16 = if has_diags { 1 } else { 0 };
    let text_width = area_width.saturating_sub(gutter_width + right_gutter) as usize;
    LineLayout { git_col, line_num_width, gutter_width, right_gutter, text_width }
}

/// Display columns available to text in the plain editor at the current
/// terminal width — the width lines are soft-wrapped to.
///
/// The scroll math and visual (`j`/`k`) motion both ask for it here rather than
/// recomputing the gutters, so a gutter change can't leave them wrapping at a
/// different width than the renderer draws.
pub(crate) fn text_width(app: &App) -> usize {
    line_layout(app, app.viewport_width as u16).text_width
}

fn render_lines(frame: &mut Frame, app: &App, area: Rect) {
    let th = theme::active();
    let LineLayout { git_col, line_num_width, gutter_width, right_gutter, text_width } =
        line_layout(app, area.width);
    let visible_rows = area.height as usize;
    let word_wrap = app.config.editor.word_wrap;

    let diag_by_line = &app.diag_by_line;

    let rope = &app.buffer.rope;
    let total_lines = rope.len_lines();
    let scroll_row = app.scroll_row;
    let tab_width = app.config.editor.tab_width;

    let spans = &app.highlight_spans;

    let sel_start = app.selection.start();
    let sel_end = app.selection.end();
    let cursor_pos = app.selection.head;

    // Build the fold+wrap-aware list of visible rows.
    let vis_rows = build_vis_rows(
        &app.fold, rope, scroll_row, visible_rows,
        word_wrap, text_width, tab_width,
    );

    let mut cells: Vec<(char, Style)> = Vec::with_capacity(text_width + 8);

    for (row, vis) in vis_rows.iter().enumerate() {
        let line_idx = vis.line_idx;
        let fold_end_opt = vis.fold_end;
        let sub_row = vis.sub_row;
        let is_continuation = sub_row > 0;

        let y = area.top() + row as u16;

        // --- Gutter ---
        if gutter_width > 0 {
            let mut gx = area.left();
            let buf = frame.buffer_mut();

            // Git mark column (1 char).
            if git_col > 0 {
                let arrow_style = Style::default().fg(th.accent);
                if fold_end_opt.is_some() {
                    buf[(gx, y)].set_char('▶').set_style(arrow_style);
                } else if is_continuation {
                    // Wrap continuation: blank git column.
                    buf[(gx, y)].set_char(' ').set_style(Style::default().fg(th.line_numbers));
                } else {
                    let (mark_ch, mark_color) = match app.git_diff.get(&line_idx) {
                        Some(GutterMark::Added)    => ('+', th.git_added),
                        Some(GutterMark::Modified) => ('~', th.git_modified),
                        None                       => (' ', th.line_numbers),
                    };
                    buf[(gx, y)]
                        .set_char(mark_ch)
                        .set_style(Style::default().fg(mark_color));
                }
                gx += 1;
            }

            // Line number column (5 chars).
            if line_num_width > 0 && line_idx < total_lines {
                let num_style = Style::default().fg(th.line_numbers);
                if is_continuation {
                    // Wrap continuation: blank line number area.
                    for _ in 0..line_num_width {
                        if gx >= area.left() + gutter_width { break; }
                        buf[(gx, y)].set_char(' ').set_style(num_style);
                        gx += 1;
                    }
                } else {
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

                    if fold_end_opt.is_some() && git_col == 0 && line_num_width >= 2 {
                        let arrow_style = Style::default().fg(th.accent);
                        buf[(gx, y)].set_char('▶').set_style(arrow_style);
                        gx += 1;
                        for c in line_num_str.chars().skip(1) {
                            if gx >= area.left() + gutter_width { break; }
                            buf[(gx, y)].set_char(c).set_style(num_style);
                            gx += 1;
                        }
                    } else {
                        for c in line_num_str.chars() {
                            if gx >= area.left() + gutter_width { break; }
                            buf[(gx, y)].set_char(c).set_style(num_style);
                            gx += 1;
                        }
                    }
                }
            } else if line_num_width > 0 {
                let num_style = Style::default().fg(th.line_numbers);
                for _ in 0..line_num_width {
                    if gx >= area.left() + gutter_width { break; }
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
            frame.render_widget(EmptyLineWidget, text_area);
            continue;
        }

        let line_start_char = rope.line_to_char(line_idx);
        let line_str = rope.line(line_idx);
        let line_len = line_str.len_chars();

        // When this row is a fold indicator, reserve space for the fold badge.
        let fold_badge: Option<String> = fold_end_opt.map(|end| {
            let hidden = end - line_idx;
            format!("  ▶ {} lines", hidden)
        });
        let badge_len = fold_badge.as_ref().map(|b| b.chars().count()).unwrap_or(0);
        let content_width = text_width.saturating_sub(badge_len);

        // `effective_skip`: the first display column that is visible on this screen row.
        //   - No wrap: app.scroll_col (horizontal scroll offset)
        //   - Wrap sub-row N: N * text_width
        let effective_skip = if word_wrap { sub_row * text_width } else { app.scroll_col };
        // The visible display-column window is [effective_skip, effective_skip + content_width).

        cells.clear();
        let mut col_offset = 0usize; // display col from start of line

        for char_off in 0..line_len {
            let char_idx = line_start_char + char_off;
            let c = line_str.char(char_off);

            if c == '\n' || c == '\r' {
                if char_idx == cursor_pos && col_offset >= effective_skip && col_offset < effective_skip + content_width {
                    cells.push((' ', theme::cursor_style(&app.mode)));
                }
                break;
            }

            let w = char_display_width(c, col_offset, tab_width);

            // Skip characters that end entirely before the visible window.
            if col_offset + w <= effective_skip {
                col_offset += w;
                continue;
            }
            // Stop characters that start at or past the visible window's right edge.
            if col_offset >= effective_skip + content_width {
                break;
            }

            // Determine base style from highlights
            let base_style = highlight::style_at(spans, char_idx);

            let style = if char_idx == cursor_pos {
                theme::cursor_style(&app.mode)
            } else if char_idx >= sel_start && char_idx <= sel_end && sel_start != sel_end {
                theme::selection_style()
            } else {
                base_style
            };

            // Diagnostic underline.
            let char_off_diag = char_idx - line_start_char;
            let style = if let Some(line_diags) = diag_by_line.get(&line_idx) {
                apply_diag_underline(
                    style,
                    line_diags
                        .iter()
                        .filter(|(cs, ce, _)| char_off_diag >= *cs && char_off_diag < *ce)
                        .map(|(_, _, sev)| sev),
                )
            } else {
                style
            };

            if c == '\t' {
                // Render only the portion of the tab that falls in the visible window.
                let tab_end = col_offset + w;
                let render_start = col_offset.max(effective_skip);
                let render_end = tab_end.min(effective_skip + content_width);
                for _ in render_start..render_end {
                    cells.push((' ', style));
                }
                col_offset = tab_end;
            } else {
                cells.push((c, style));
                col_offset += w;
            }
        }

        // Append fold badge when this row is a fold indicator.
        if let Some(ref badge) = fold_badge {
            let arrow_style = Style::default().fg(th.accent);
            let count_style = Style::default().fg(th.dim);
            for (i, c) in badge.chars().enumerate() {
                let style = if i < 4 { arrow_style } else { count_style };
                cells.push((c, style));
            }
        } else if !is_continuation
            && matches!(app.mode, crate::mode::Mode::Jump { .. })
        {
            // Jump label overlay — only on first sub-row (non-fold) lines.
            // Map char offsets to display columns (tab-aware) before painting.
            let display_col_of = |char_off: usize| {
                let mut col = 0usize;
                for i in 0..char_off {
                    col += char_display_width(line_str.char(i), col, tab_width);
                }
                col
            };
            for_each_jump_label_char(
                &app.jump.labels,
                &app.jump.typed,
                line_start_char,
                line_len,
                |char_off, lc, style| {
                    let display_col = display_col_of(char_off);
                    if display_col < effective_skip {
                        return;
                    }
                    let idx = display_col - effective_skip;
                    if idx < cells.len() {
                        cells[idx] = (lc, style);
                    }
                },
            );
        }

        let line_widget = LineWidget { cells: &cells };
        frame.render_widget(line_widget, text_area);

        // Right diagnostic gutter marker (only on non-fold, non-continuation rows).
        if right_gutter > 0 && line_idx < total_lines && fold_end_opt.is_none() && !is_continuation {
            let rx = area.right() - 1;
            let buf = frame.buffer_mut();
            if let Some(line_diags) = diag_by_line.get(&line_idx) {
                let has_error = line_diags.iter().any(|(_, _, s)| *s == DiagnosticSeverity::Error);
                let has_warn  = line_diags.iter().any(|(_, _, s)| *s == DiagnosticSeverity::Warning);
                let (ch, color) = if has_error {
                    ('●', th.error)
                } else if has_warn {
                    ('◆', th.warning)
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
/// Returns None if the cursor is scrolled off screen or inside a hidden fold.
pub fn cursor_screen_pos(app: &App, lines_area: Rect) -> Option<(u16, u16)> {
    let LineLayout { gutter_width, text_width, .. } = line_layout(app, lines_area.width);

    let rope = &app.buffer.rope;
    if rope.len_chars() == 0 {
        return Some((lines_area.left() + gutter_width, lines_area.top()));
    }

    let head = app.selection.head.min(rope.len_chars());
    let line_idx = rope.char_to_line(head);

    // Cursor must not be inside a hidden fold region.
    if app.fold.is_hidden(line_idx) {
        return None;
    }

    let total_lines = rope.len_lines();
    let word_wrap = app.config.editor.word_wrap;
    let tab_width = app.config.editor.tab_width;

    let line_start = rope.line_to_char(line_idx);
    let line_str = rope.line(line_idx);
    let cursor_off = head - line_start;

    // Compute total display column from the start of this logical line.
    let mut total_col: usize = 0;
    for i in 0..cursor_off {
        total_col += char_display_width(line_str.char(i), total_col, tab_width);
    }

    if word_wrap && text_width > 0 {
        // With wrapping: find the screen row by walking the visible entry list.
        let vis_rows = build_vis_rows(
            &app.fold, rope, app.scroll_row, lines_area.height as usize,
            true, text_width, tab_width,
        );
        // Which wrapped row the cursor is on comes from the shared wrap rule,
        // not from dividing the display column: `j`/`k` step through the rows
        // that rule produces, so anything else could draw the cursor on a row
        // it cannot be moved to.
        let starts =
            crate::render_util::wrap_row_starts(line_str, text_width, tab_width);
        let cursor_sub_row = starts.iter().rposition(|&s| cursor_off >= s).unwrap_or(0);
        let mut col_in_sub_row = 0usize;
        for i in starts[cursor_sub_row]..cursor_off {
            col_in_sub_row +=
                char_display_width(line_str.char(i), col_in_sub_row, tab_width);
        }

        let screen_row = vis_rows.iter().position(|v| {
            v.line_idx == line_idx && v.fold_end.is_none() && v.sub_row == cursor_sub_row
        })?;

        let screen_x = lines_area.left() + gutter_width + col_in_sub_row as u16;
        let screen_y = lines_area.top() + screen_row as u16;
        Some((screen_x, screen_y))
    } else {
        // Without wrapping: use fold-aware entry list.
        let entries = app.fold.visible_entries(app.scroll_row, lines_area.height as usize, total_lines);
        let screen_row = entries.iter().position(|&(l, _)| l == line_idx)?;

        let text_col = total_col.saturating_sub(app.scroll_col);
        if text_col >= text_width {
            return None;
        }

        let screen_x = lines_area.left() + gutter_width + text_col as u16;
        let screen_y = lines_area.top() + screen_row as u16;
        Some((screen_x, screen_y))
    }
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

/// 0-based (line, char-column) of the cursor in `app.buffer.rope` — in the
/// notebook view this is the focused cell's text, same as everywhere else in
/// this module that reads cursor position off `app.buffer`.
fn cursor_line_col(app: &App) -> (usize, usize) {
    let rope = &app.buffer.rope;
    let cursor_pos = app.selection.head.min(rope.len_chars());
    let line_idx = if rope.len_chars() == 0 { 0 } else { rope.char_to_line(cursor_pos) };
    let line_start = if rope.len_chars() == 0 { 0 } else { rope.line_to_char(line_idx) };
    (line_idx, cursor_pos.saturating_sub(line_start))
}

/// The diagnostic (if any) covering the cursor position in the document
/// currently being edited — the plain buffer, or the focused notebook cell's
/// source. `None` while browsing notebook output (the cursor there isn't over
/// source text) or when nothing at the cursor has a diagnostic.
fn diagnostic_at_cursor(app: &App) -> Option<&crate::lsp_manager::Diagnostic> {
    let key = if let Some((nb, state)) = app.notebook.as_ref() {
        if state.output_row.is_some() {
            return None;
        }
        let vpath = crate::notebook::cell_virtual_path(
            &nb.path,
            &nb.metadata.kernel_language,
            state.focused_cell,
        );
        crate::lsp::diagnostic_key(&vpath)
    } else {
        crate::lsp::diagnostic_key(app.buffer.path.as_ref()?)
    };
    let diags = app.lsp.diagnostics.get(&key)?;
    let (line, col) = cursor_line_col(app);
    diagnostic_at(diags, line, col)
}

/// Build the status-line [`Ctx`](crate::statusline::Ctx) from the current app
/// state. Shared by the plain editor / focused-cell overlay (this module) and
/// the multi-cell notebook view (`app::run_loop`); only the chosen module
/// *layout* differs between those call sites, not the data.
///
/// Position fields are always read from `app.buffer.rope` — in the notebook view
/// the focused cell's text lives there too. When a notebook is open, the
/// filename gains cell context and diagnostics are summed across every cell.
pub fn status_ctx(app: &App) -> crate::statusline::Ctx {
    let rope = &app.buffer.rope;
    let (line_idx, col0) = cursor_line_col(app);
    let col = col0 + 1;
    let total_lines = rope.len_lines().max(1);
    let scroll_pct = (line_idx * 100) / total_lines;

    let count_diags = |key: &str, e: &mut usize, w: &mut usize| {
        if let Some(diags) = app.lsp.diagnostics.get(key) {
            *e += diags.iter().filter(|d| d.severity == DiagnosticSeverity::Error).count();
            *w += diags.iter().filter(|d| d.severity == DiagnosticSeverity::Warning).count();
        }
    };

    let (mut diag_errors, mut diag_warnings) = (0usize, 0usize);
    let (filename, modified, cell, kernel) = if let Some(session) = app.table.as_ref() {
        // The table view's buffer is detached and has no path, so the name comes
        // from the session.  Read-only, hence never modified.
        (session.display_name(), false, None, None)
    } else if let Some(name) = app.table_load_name() {
        // A table is still parsing on the background thread.  The buffer is
        // already detached, so without this the status line reads "[No Name]"
        // over an empty screen — name the file being loaded instead (the
        // `spinner` module is animating alongside it).
        (format!("{name}  ·  loading"), false, None, None)
    } else if let Some((nb, state)) = app.notebook.as_ref() {
        let nb_name = nb.path.file_stem().and_then(|s| s.to_str()).unwrap_or("notebook");
        let ext = lang_to_ext(&nb.metadata.kernel_language);
        let filename = format!("{nb_name}  ·  cell [{}].{ext}", state.focused_cell + 1);
        // Diagnostics are reported per virtual cell path; sum across them all.
        for idx in 0..nb.cells.len() {
            let vpath = crate::notebook::cell_virtual_path(&nb.path, &nb.metadata.kernel_language, idx);
            count_diags(&crate::lsp::diagnostic_key(&vpath), &mut diag_errors, &mut diag_warnings);
        }
        let kernel = Some(match nb.kernel.as_ref().map(|k| &k.status) {
            Some(crate::notebook::KernelStatus::Starting) => crate::statusline::KernelView::Starting,
            Some(crate::notebook::KernelStatus::Idle) => crate::statusline::KernelView::Idle,
            Some(crate::notebook::KernelStatus::Busy) => crate::statusline::KernelView::Busy,
            Some(crate::notebook::KernelStatus::Dead) => crate::statusline::KernelView::Dead,
            None => crate::statusline::KernelView::None,
        });
        (filename, nb.modified, Some((state.focused_cell + 1, nb.cells.len().max(1))), kernel)
    } else {
        if let Some(ref path) = app.buffer.path {
            count_diags(&crate::lsp::diagnostic_key(path), &mut diag_errors, &mut diag_warnings);
        }
        (app.buffer.display_name(), app.buffer.modified, None, None)
    };

    crate::statusline::Ctx {
        mode_label: app.mode.label().to_string(),
        mode_color: theme::mode_color(&app.mode),
        filename,
        modified,
        branch: app.git_branch.clone(),
        diag_errors,
        diag_warnings,
        line: line_idx + 1,
        col,
        scroll_pct,
        spinner: app.spinner.glyph(),
        cell,
        kernel,
        table: app.table.as_ref().map(|s| {
            let st = &s.state;
            let cols = s.source.columns();
            crate::statusline::TableView {
                row: (st.cursor_row + 1, s.source.loaded_rows()),
                col: (st.cursor_col + 1, cols.len()),
                col_name: cols
                    .get(st.cursor_col)
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
            }
        }),
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let ctx = status_ctx(app);
    crate::statusline::render(
        frame,
        area,
        &ctx,
        &app.config.statusline.left,
        &app.config.statusline.right,
        &app.config.statusline.separator,
        &app.config.statusline.styles,
    );
}

// ---------------------------------------------------------------------------
// Command/message line
// ---------------------------------------------------------------------------

/// Render the command/message/prompt line (bottom row).  Shared by the plain
/// editor, the notebook view, and the splash screen.
pub fn render_command(frame: &mut Frame, app: &App, area: Rect) {
    let (text, style) = match &app.mode {
        Mode::Jump { .. } => (
            if app.jump.typed.is_empty() {
                "Jump: type label chars...".to_string()
            } else {
                format!("Jump: {}_", app.jump.typed)
            },
            Style::default(),
        ),
        Mode::Command => (format!(":{}", app.command_buf), Style::default()),
        Mode::Prompt { kind } => {
            let label = match kind {
                PromptKind::NewFile => "New file",
                PromptKind::NewNotebook => "New notebook",
            };
            (format!("{label}: {}_", app.command_buf), Style::default())
        }
        Mode::Search { forward } => {
            let prefix = if *forward { '/' } else { '?' };
            let count = app.search.matches.len();
            let text = if app.search.query.is_empty() {
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
            };
            (text, Style::default())
        }
        // A transient message wins; then the diagnostic under the cursor (colored
        // by severity), skipped in Insert mode since signature help owns that
        // slot there; otherwise the active call signature.
        _ => {
            if let Some(msg) = app.messages.current() {
                (msg.to_owned(), Style::default())
            } else if !matches!(app.mode, Mode::Insert) {
                match diagnostic_at_cursor(app) {
                    Some(diag) => {
                        let th = theme::active();
                        let color = match diag.severity {
                            DiagnosticSeverity::Error => th.error,
                            DiagnosticSeverity::Warning => th.warning,
                            _ => th.info,
                        };
                        (diag.message.clone(), Style::default().fg(color))
                    }
                    None => (
                        app.signature_help.clone().unwrap_or_default(),
                        Style::default(),
                    ),
                }
            } else {
                (
                    app.signature_help.clone().unwrap_or_default(),
                    Style::default(),
                )
            }
        }
    };
    let cmd_widget = SingleLineWidget { text, style };
    frame.render_widget(cmd_widget, area);
}

// ---------------------------------------------------------------------------
// Custom widgets
// ---------------------------------------------------------------------------


struct EmptyLineWidget;

impl Widget for EmptyLineWidget {
    fn render(self, _area: Rect, _buf: &mut RatBuffer) {}
}

struct LineWidget<'a> {
    cells: &'a [(char, Style)],
}

impl Widget for LineWidget<'_> {
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
            buf[(x, area.top())].set_char(*c).set_style(*style);
            x += w;
        }
    }
}

