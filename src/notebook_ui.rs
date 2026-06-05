use ratatui::{
    buffer::Buffer as RatBuffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, Widget},
    Frame,
};

use crate::{
    highlight::{self, Highlighter},
    lang::lang_to_ext,
    lsp_manager::{Diagnostic, DiagnosticSeverity},
    mode::Mode,
    notebook::{Cell, CellType, MimeData, Notebook, Output},
    notebook_state::NotebookState,
};


/// Info about the focused cell that comes from `app.buffer`/`app.selection`.
pub struct ActiveCellView<'a> {
    /// The rope backing the focused cell (= `app.buffer.rope`).
    pub rope: &'a ropey::Rope,
    /// Cursor char-index within that rope (= `app.selection.head`).
    pub cursor: usize,
    /// Selection anchor (= `app.selection.anchor`). Equal to cursor when no selection.
    pub sel_anchor: usize,
    /// First visible line inside the cell (= `app.scroll_row`).
    pub scroll_row: usize,
    /// Current editor mode — determines cursor highlight style.
    pub mode: &'a Mode,
    /// Per-mode color overrides from the loaded config.
    pub mode_colors: &'a crate::config::ModeColorsConfig,
    /// Jump-mode labels to overlay on the cell source (`app.jump_labels`).
    /// Empty slice when not in Jump mode.
    pub jump_labels: &'a [(usize, String)],
    /// Characters typed so far in Jump mode (`app.jump_typed`).
    pub jump_typed: &'a str,
}

/// A request to render a PNG image via the Kitty graphics protocol.
pub struct ImageRequest {
    pub col: u16,
    pub row: u16,
    pub rows: u16,
    /// Explicit column width passed as `c=` in the protocol.  Required for
    /// WezTerm, which doesn't auto-compute width from aspect ratio like Kitty.
    pub cols: u16,
    /// Shared reference to the raw PNG bytes — cloning this is O(1).
    pub png_data: std::sync::Arc<Vec<u8>>,
}

/// Render the notebook view into the frame.
///
/// Returns a list of images to draw via Kitty after `terminal.draw()`.
/// Render the notebook view.
///
/// Returns `(image_requests, cursor_screen_pos)`.  The cursor position is the
/// terminal (col, row) of the insertion point inside the focused cell — pass
/// it to `popup_ui::render` so completion popups anchor to the right spot.
pub fn render(
    frame: &mut Frame,
    state: &NotebookState,
    nb: &Notebook,
    active: &ActiveCellView<'_>,
    lsp_diagnostics: &std::collections::HashMap<String, Vec<Diagnostic>>,
    nb_config: &crate::config::NotebookConfig,
    cell_pixel_size: Option<(u16, u16)>,
) -> (Vec<ImageRequest>, Option<(u16, u16)>) {
    let size = frame.area();
    if size.height < 3 {
        return (vec![], None);
    }

    // Content area — leave last 2 rows for status bar + command line.
    let content_area = Rect {
        x: size.x,
        y: size.y,
        width: size.width,
        height: size.height.saturating_sub(2),
    };

    render_cells(frame, state, nb, active, lsp_diagnostics, content_area, nb_config, cell_pixel_size)
}

// ---------------------------------------------------------------------------
// Cell rendering
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_cells(
    frame: &mut Frame,
    state: &NotebookState,
    nb: &Notebook,
    active: &ActiveCellView<'_>,
    lsp_diagnostics: &std::collections::HashMap<String, Vec<Diagnostic>>,
    area: Rect,
    nb_config: &crate::config::NotebookConfig,
    cell_pixel_size: Option<(u16, u16)>,
) -> (Vec<ImageRequest>, Option<(u16, u16)>) {
    let mut image_requests = Vec::new();
    let mut current_row = area.top();
    let mut focused_cell_screen_pos: Option<(u16, u16)> = None;

    // One Highlighter shared across all cells — avoids rebuilding the
    // tree-sitter HighlightConfiguration (grammar query parse) per cell.
    let lang_ext = format!("_.{}", lang_to_ext(&nb.metadata.kernel_language));
    let mut shared_hl = Highlighter::new(Some(std::path::Path::new(&lang_ext)));

    for (cell_idx, cell) in nb.cells.iter().enumerate() {
        if cell_idx < state.scroll_cell {
            continue;
        }
        if current_row >= area.bottom() {
            break;
        }

        let remaining = area.bottom().saturating_sub(current_row);
        if remaining < 3 {
            break; // need at least border-top + 1 content row + border-bottom
        }

        let is_focused = cell_idx == state.focused_cell;
        let is_folded = state.is_cell_folded(cell_idx);
        // Inner column width available for cell content (subtract left+right borders).
        let inner_cols = area.width.saturating_sub(2).max(4);
        // Folded cells always get the compact height regardless of focus.
        // For the focused non-folded cell, use the live buffer rope height.
        let cell_height = if is_folded {
            3u16.min(remaining) // border-top + 1 summary line + border-bottom
        } else if is_focused {
            focused_cell_display_height(active.rope, cell, nb_config.image_rows, cell_pixel_size, inner_cols).min(remaining)
        } else {
            cell_display_height(cell, nb_config.image_rows, cell_pixel_size, inner_cols).min(remaining)
        };

        let cell_rect = Rect {
            x: area.x,
            y: current_row,
            width: area.width,
            height: cell_height,
        };

        // Border colour encodes cell execution state
        let border_color = cell_border_color(cell, state.executing_cell, cell_idx);

        // Cell title sits inside the top border line
        let count_str = cell.execution_count
            .map(|n| format!("[{n}]"))
            .unwrap_or_else(|| "[ ]".to_string());
        let type_label = cell_type_label(cell, &nb.metadata.kernel_language);
        let title = format!(" {count_str} {type_label} ");

        let title_style = if is_focused {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let block = Block::default()
            .title(ratatui::text::Span::styled(title, title_style))
            .borders(Borders::ALL)
            .border_type(if is_focused { BorderType::Thick } else { BorderType::Rounded })
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(Color::Rgb(20, 20, 30)));

        let inner = block.inner(cell_rect);
        frame.render_widget(block, cell_rect);

        if inner.height > 0 {
            if is_folded {
                // For the focused cell, use the live rope so unsaved edits are shown.
                let rope_for_summary = if is_focused { active.rope } else { &cell.source };
                render_folded_cell_summary_rope(frame, rope_for_summary, &cell.outputs, inner);
            } else {
                let cursor_screen = render_cell_content(
                    frame, nb, cell, cell_idx, is_focused, inner, active,
                    lsp_diagnostics, &mut image_requests, &mut shared_hl, nb_config,
                    cell_pixel_size,
                );
                if is_focused {
                    focused_cell_screen_pos = cursor_screen;
                }
            }
        }

        current_row += cell_height + 1; // +1 blank gap between cells
    }

    // Position the hardware cursor inside the focused cell.
    if let Some((cx, cy)) = focused_cell_screen_pos {
        frame.set_cursor_position((cx, cy));
    }

    (image_requests, focused_cell_screen_pos)
}

/// Render source lines and outputs inside a cell's bordered inner area.
/// Returns the screen (col, row) of the cursor when `is_focused` is true.
#[allow(clippy::too_many_arguments)]
fn render_cell_content(
    frame: &mut Frame,
    nb: &Notebook,
    cell: &Cell,
    cell_idx: usize,
    is_focused: bool,
    area: Rect,
    active: &ActiveCellView<'_>,
    lsp_diagnostics: &std::collections::HashMap<String, Vec<Diagnostic>>,
    image_requests: &mut Vec<ImageRequest>,
    highlighter: &mut Highlighter,
    nb_config: &crate::config::NotebookConfig,
    cell_pixel_size: Option<(u16, u16)>,
) -> Option<(u16, u16)> {
    // For the focused cell, use the live buffer rope; otherwise use stored source.
    let rope: &ropey::Rope = if is_focused { active.rope } else { &cell.source };

    // A Markdown cell shows its formatted (highlighted) view when `rendered`,
    // except while it's the focused cell being actively edited (i.e. we've
    // dropped out of Notebook navigation into an edit sub-mode) — then we show
    // the raw source so the markup is editable.
    let editing_this = is_focused && !matches!(active.mode, Mode::Notebook);
    let show_markdown = cell.cell_type == CellType::Markdown && cell.rendered && !editing_this;

    // Suppress the cursor/selection in the rendered markdown view.
    let (cursor_char_idx, sel_range, scroll_row) = if is_focused && !show_markdown {
        let lo = active.cursor.min(active.sel_anchor);
        let hi = active.cursor.max(active.sel_anchor);
        (Some(active.cursor), (lo, hi), active.scroll_row)
    } else {
        (None, (0usize, 0usize), if is_focused { active.scroll_row } else { 0 })
    };

    let source_text = rope.to_string();
    let source_lines: Vec<&str> = if source_text.is_empty() {
        vec![""]
    } else {
        source_text.split('\n').collect()
    };

    let highlight_spans = if cell.cell_type == CellType::Code {
        highlighter.highlight(rope).unwrap_or_default()
    } else if show_markdown {
        crate::markdown::highlight(rope)
    } else {
        vec![]
    };

    // Collect diagnostics for this cell's virtual path (e.g. notebook__cell0.py).
    // Format: (line_within_cell, col_start, col_end, severity).
    let cell_diag_ranges: Vec<(usize, usize, usize, DiagnosticSeverity)> = {
        let vpath = crate::notebook::cell_virtual_path(
            &nb.path, &nb.metadata.kernel_language, cell_idx,
        );
        let key = crate::lsp::diagnostic_key(&vpath);
        lsp_diagnostics
            .get(&key)
            .map(|diags| {
                diags.iter()
                    .map(|d| (d.line, d.col_start, d.col_end, d.severity.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };

    let line_ctx = SourceLineCtx {
        cursor_pos: cursor_char_idx,
        sel_range,
        mode: active.mode,
        mode_colors: active.mode_colors,
        highlight_spans: &highlight_spans,
        use_highlight: cell.cell_type == CellType::Code || show_markdown,
        diag_ranges: &cell_diag_ranges,
        // Only overlay jump labels on the focused cell.
        jump_labels: if is_focused { active.jump_labels } else { &[] },
        jump_typed: if is_focused { active.jump_typed } else { "" },
    };

    let mut current_row = area.top();
    let mut cursor_screen: Option<(u16, u16)> = None;
    let pad_len = 2u16; // leading spaces

    for (line_no, line) in source_lines.iter().enumerate() {
        // Honour intra-cell scroll offset.
        if line_no < scroll_row {
            continue;
        }
        if current_row >= area.bottom() {
            break;
        }

        // Compute the char index at the start of this line.
        let line_start_char: usize = source_lines[..line_no]
            .iter()
            .map(|l| l.chars().count() + 1)
            .sum();

        // Compute cursor screen position if the cursor is on this line.
        if let Some(ci) = cursor_char_idx {
            let line_len = line.chars().count();
            if ci >= line_start_char && ci <= line_start_char + line_len {
                let col_in_line = ci - line_start_char;
                let screen_x = area.x + pad_len + col_in_line as u16;
                cursor_screen = Some(if screen_x < area.right() {
                    (screen_x, current_row)
                } else {
                    (area.right().saturating_sub(1), current_row)
                });
            }
        }

        render_source_line(
            frame,
            single_row(area, current_row),
            line,
            line_no,
            line_start_char,
            &line_ctx,
        );
        current_row += 1;
    }

    if cell.cell_type == CellType::Code && !cell.outputs.is_empty() {
        if current_row < area.bottom() {
            frame.render_widget(
                SingleLineWidget {
                    text: " \u{2500}\u{2500} output \u{2500}\u{2500}".to_string(),
                    style: Style::default().fg(Color::DarkGray),
                },
                single_row(area, current_row),
            );
            current_row += 1;
        }
        for output in &cell.outputs {
            if current_row >= area.bottom() {
                break;
            }
            render_output(frame, output, area, &mut current_row, image_requests, nb_config, cell_pixel_size);
        }
    }

    cursor_screen
}

/// Render a single summary line for a folded (collapsed) cell.
/// Uses the provided rope (may be the live editor rope for the focused cell).
fn render_folded_cell_summary_rope(
    frame: &mut Frame,
    source: &ropey::Rope,
    outputs: &[Output],
    area: Rect,
) {
    if area.height == 0 {
        return;
    }
    let row = single_row(area, area.y);

    let total_lines = source.len_lines().max(1) as usize;
    let hidden_lines = total_lines.saturating_sub(1);
    let output_count = outputs.len();

    let source_str = source.to_string();
    let first_line = source_str.lines().next().unwrap_or("").trim_end();
    let max_content = (area.width as usize).saturating_sub(30);
    let content: String = first_line.chars().take(max_content).collect();

    let suffix = if output_count > 0 {
        format!("  ▶ {} lines · {} outputs", hidden_lines, output_count)
    } else {
        format!("  ▶ {} lines", hidden_lines)
    };

    let buf = frame.buffer_mut();
    let y = row.y;
    let mut x = row.x;

    let content_style = Style::default().fg(Color::Rgb(120, 120, 150));
    let arrow_style = Style::default().fg(Color::Rgb(255, 160, 50));
    let count_style = Style::default().fg(Color::DarkGray);

    for c in format!("  {content}").chars() {
        if x >= row.right() { break; }
        buf[(x, y)].set_char(c).set_style(content_style);
        x += 1;
    }
    for c in "  ▶ ".chars() {
        if x >= row.right() { break; }
        let style = if c == '▶' { arrow_style } else { count_style };
        buf[(x, y)].set_char(c).set_style(style);
        x += 1;
    }
    let count_part: String = suffix.chars().skip(4).collect();
    for c in count_part.chars() {
        if x >= row.right() { break; }
        buf[(x, y)].set_char(c).set_style(count_style);
        x += 1;
    }
}

// ---------------------------------------------------------------------------
// Cell height / colour helpers
// ---------------------------------------------------------------------------

/// Compute how many terminal rows an image should occupy.
///
/// The image's *natural* terminal size is `png_w / cell_w` cols × `png_h / cell_h` rows —
/// a 1:1 mapping of PNG pixels to terminal pixels.  If the image fits within
/// `available_cols`, it is displayed at that natural (smaller) size.  If it is
/// wider than `available_cols`, it is scaled down to fill the available width,
/// preserving aspect ratio.  The result is always capped at `max_image_rows`.
///
/// This means small figures (small figsize) show small, while large figures
/// scale down to fill the available width — `available_cols` is a ceiling, not
/// a target.
pub fn compute_image_rows(
    png_w: u32,
    png_h: u32,
    available_cols: u16,
    cell_pixel_size: Option<(u16, u16)>,
    max_image_rows: u16,
) -> u16 {
    let (cell_h, cell_w) = cell_pixel_size.unwrap_or((18, 9));

    // Natural terminal dimensions at 1:1 PNG-pixel-to-terminal-pixel mapping.
    let natural_cols = png_w / cell_w as u32;
    let natural_rows = png_h / cell_h as u32;

    let rows: u64 = if natural_cols <= available_cols as u32 {
        // Image fits within the available width — use its natural height.
        natural_rows as u64
    } else {
        // Image is wider than available — scale down to fit, preserving aspect ratio.
        // rows = available_cols × cell_w_px × png_h / (png_w × cell_h_px)
        (available_cols as u64 * cell_w as u64 * png_h as u64)
            / (png_w as u64 * cell_h as u64)
    };

    (rows as u16).max(2).min(max_image_rows)
}

pub fn cell_display_height(
    cell: &Cell,
    max_image_rows: u16,
    cell_pixel_size: Option<(u16, u16)>,
    available_cols: u16,
) -> u16 {
    // len_lines() is O(1) on a Rope; avoids the O(n) to_string() conversion.
    let source_lines = cell.source.len_lines().max(1) as u16;
    let output_h: u16 = if cell.cell_type == CellType::Code && !cell.outputs.is_empty() {
        1 + cell.outputs.iter()
            .map(|o| single_output_height_count(o, max_image_rows, cell_pixel_size, available_cols))
            .sum::<u16>()
    } else {
        0
    };
    2 + source_lines + output_h // 2 = top border + bottom border
}

pub fn focused_cell_display_height(
    rope: &ropey::Rope,
    cell: &Cell,
    max_image_rows: u16,
    cell_pixel_size: Option<(u16, u16)>,
    available_cols: u16,
) -> u16 {
    let source_lines = rope.len_lines().max(1) as u16;
    let output_h: u16 = if cell.cell_type == CellType::Code && !cell.outputs.is_empty() {
        1 + cell.outputs.iter()
            .map(|o| single_output_height_count(o, max_image_rows, cell_pixel_size, available_cols))
            .sum::<u16>()
    } else {
        0
    };
    2 + source_lines + output_h
}

fn single_output_height_count(
    output: &Output,
    max_image_rows: u16,
    cell_pixel_size: Option<(u16, u16)>,
    available_cols: u16,
) -> u16 {
    match output {
        Output::Stream { text, .. } => {
            let n = text.lines().count();
            let shown = n.min(20);
            let extra = if n > 20 { 1 } else { 0 };
            (shown + extra).max(1) as u16
        }
        Output::DisplayData { data } | Output::ExecuteResult { data, .. } => {
            if let Some(png) = &data.image_png {
                if let Some((pw, ph)) = png_pixel_size(png) {
                    compute_image_rows(pw, ph, available_cols, cell_pixel_size, max_image_rows)
                } else {
                    max_image_rows
                }
            } else {
                data.text_plain
                    .as_deref()
                    .map(|t| t.lines().count().min(20).max(1))
                    .unwrap_or(0) as u16
            }
        }
        Output::Error { traceback, .. } => 1 + traceback.len().min(5) as u16,
    }
}

/// Returns the border colour reflecting the cell's execution state.
/// Dark blue = not yet run, bright blue = running, Green = success, Red = errored.
fn cell_border_color(cell: &Cell, executing_cell: Option<usize>, cell_idx: usize) -> Color {
    if executing_cell == Some(cell_idx) {
        // Bright blue while the cell streams output, distinct from the dim blue
        // of an un-run cell.
        return Color::LightBlue;
    }
    if cell.outputs.iter().any(|o| matches!(o, Output::Error { .. })) {
        return Color::Red;
    }
    if cell.execution_count.is_some() {
        return Color::Green;
    }
    Color::Blue
}

fn cell_type_label(cell: &Cell, kernel_language: &str) -> String {
    match cell.cell_type {
        CellType::Code => format!("CODE ({})", kernel_language),
        CellType::Markdown => "MARKDOWN".to_string(),
        CellType::Raw => "RAW".to_string(),
    }
}

/// Per-line rendering context shared across all source lines in a cell.
struct SourceLineCtx<'a> {
    cursor_pos: Option<usize>,
    sel_range: (usize, usize),
    mode: &'a Mode,
    mode_colors: &'a crate::config::ModeColorsConfig,
    highlight_spans: &'a [(usize, usize, usize)],
    /// When true, render characters with their highlight spans (code cells, and
    /// rendered markdown cells); when false, render as plain gray source text.
    use_highlight: bool,
    /// Diagnostic ranges for this cell: (line_within_cell, col_start, col_end, severity).
    diag_ranges: &'a [(usize, usize, usize, DiagnosticSeverity)],
    /// Jump-mode labels to overlay on the focused cell's source lines.
    jump_labels: &'a [(usize, String)],
    jump_typed: &'a str,
}

fn render_source_line(
    frame: &mut Frame,
    area: Rect,
    line: &str,
    line_no: usize,
    line_start_char: usize,
    ctx: &SourceLineCtx<'_>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let padding = "  ";
    let pad_len = padding.chars().count() as u16;

    if area.width > 0 {
        let pad_area = Rect { x: area.x, y: area.y, width: pad_len.min(area.width), height: 1 };
        frame.render_widget(
            SingleLineWidget { text: padding.to_string(), style: Style::default() },
            pad_area,
        );
    }

    let content_x = area.x + pad_len;
    let content_width = area.width.saturating_sub(pad_len);
    if content_width == 0 {
        return;
    }
    let content_area = Rect { x: content_x, y: area.y, width: content_width, height: 1 };
    let cursor_style = crate::theme::cursor_style(ctx.mode, ctx.mode_colors);
    let selection_style = Style::default().bg(Color::Rgb(60, 80, 120)).fg(Color::White);
    let (sel_lo, sel_hi) = ctx.sel_range;
    let has_selection = sel_lo != sel_hi;

    let mut x = content_area.x;
    let line_len = line.chars().count();
    let buf = frame.buffer_mut();

    for (char_off, c) in line.chars().enumerate() {
        if x >= content_area.right() {
            break;
        }
        let char_idx = line_start_char + char_off;
        let base_style = if ctx.use_highlight {
            highlight::style_at(ctx.highlight_spans, char_idx)
        } else {
            Style::default().fg(Color::Gray)
        };
        let style = if ctx.cursor_pos == Some(char_idx) {
            cursor_style
        } else if has_selection && char_idx >= sel_lo && char_idx < sel_hi {
            selection_style
        } else {
            base_style
        };
        // Diagnostic underline (does not override cursor/selection colours).
        let style = {
            let worst = ctx
                .diag_ranges
                .iter()
                .filter(|(dl, cs, ce, _)| *dl == line_no && char_off >= *cs && char_off < *ce)
                .fold(None::<&DiagnosticSeverity>, |acc, (_, _, _, sev)| {
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
        };
        buf[(x, area.y)].set_char(c).set_style(style);
        x += 1;
    }

    // Cursor past end-of-line (empty line or cursor at newline position).
    if let Some(cp) = ctx.cursor_pos {
        if cp == line_start_char + line_len && x < content_area.right() {
            buf[(x, area.y)].set_char(' ').set_style(cursor_style);
        }
    }

    // Jump label overlay — paint over already-rendered characters.
    if !ctx.jump_labels.is_empty() {
        let typed_len = ctx.jump_typed.len();
        let jump_pending = Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(255, 160, 0))
            .add_modifier(Modifier::BOLD);
        let jump_confirmed = Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD);
        for (pos, label) in ctx.jump_labels {
            if !label.starts_with(ctx.jump_typed) {
                continue;
            }
            if *pos < line_start_char {
                continue;
            }
            let char_off = pos - line_start_char;
            if char_off >= line_len {
                continue;
            }
            for (i, lc) in label.chars().enumerate() {
                let col = content_x + (char_off + i) as u16;
                if col >= content_area.right() {
                    break;
                }
                let style = if i < typed_len { jump_confirmed } else { jump_pending };
                buf[(col, area.y)].set_char(lc).set_style(style);
            }
        }
    }
}

fn render_output(
    frame: &mut Frame,
    output: &Output,
    area: Rect,
    current_row: &mut u16,
    image_requests: &mut Vec<ImageRequest>,
    nb_config: &crate::config::NotebookConfig,
    cell_pixel_size: Option<(u16, u16)>,
) {
    match output {
        Output::Stream { name, text } => {
            let style = if name == "stderr" {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            let lines: Vec<&str> = text.lines().collect();
            let max_lines = nb_config.max_output_lines;
            let to_show = lines.len().min(max_lines);
            for line in &lines[..to_show] {
                if *current_row >= area.bottom() {
                    break;
                }
                let row_area = single_row(area, *current_row);
                frame.render_widget(
                    SingleLineWidget {
                        text: format!("  {line}"),
                        style,
                    },
                    row_area,
                );
                *current_row += 1;
            }
            if lines.len() > max_lines && *current_row < area.bottom() {
                let extra = lines.len() - max_lines;
                let row_area = single_row(area, *current_row);
                frame.render_widget(
                    SingleLineWidget {
                        text: format!("  ... ({extra} more lines)"),
                        style: Style::default().fg(Color::DarkGray),
                    },
                    row_area,
                );
                *current_row += 1;
            }
        }

        Output::DisplayData { data } | Output::ExecuteResult { data, .. } => {
            render_mime_data(frame, data, area, current_row, image_requests, nb_config.image_rows, cell_pixel_size);
        }

        Output::Error { ename, evalue, traceback } => {
            if *current_row < area.bottom() {
                let row_area = single_row(area, *current_row);
                frame.render_widget(
                    SingleLineWidget {
                        text: format!("  {ename}: {evalue}"),
                        style: Style::default().fg(Color::Red),
                    },
                    row_area,
                );
                *current_row += 1;
            }
            for tb_line in traceback.iter().take(nb_config.max_traceback_lines) {
                if *current_row >= area.bottom() {
                    break;
                }
                let row_area = single_row(area, *current_row);
                frame.render_widget(
                    SingleLineWidget {
                        text: format!("  {tb_line}"),
                        style: Style::default().fg(Color::DarkGray),
                    },
                    row_area,
                );
                *current_row += 1;
            }
        }
    }
}

/// Read pixel dimensions from a PNG header (bytes 16-23 of the file).
/// Returns None if the slice is too short or reports zero dimensions.
fn png_pixel_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 {
        return None;
    }
    let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    if w > 0 && h > 0 { Some((w, h)) } else { None }
}

/// Compute how many terminal columns a `rows`-tall image will occupy.
///
/// Kitty scales the image to exactly `rows` terminal rows in height, then
/// determines the width from the image's aspect ratio and the actual terminal
/// cell pixel dimensions.  We replicate that calculation so the dark placeholder
/// drawn by ratatui matches the image footprint exactly.
///
/// Formula: cols = rows × cell_h_px × png_w / (png_h × cell_w_px)
///
/// Falls back to a 2:1 cell ratio when actual pixel dimensions are unavailable.
fn estimated_image_cols(png_w: u32, png_h: u32, rows: u16, cell_pixel_size: Option<(u16, u16)>) -> u16 {
    let (cell_h, cell_w) = cell_pixel_size.unwrap_or((18, 9));
    let cols = (rows as u64) * (cell_h as u64) * (png_w as u64)
        / ((png_h as u64) * (cell_w as u64));
    cols.clamp(4, 512) as u16
}

fn render_mime_data(
    frame: &mut Frame,
    data: &MimeData,
    area: Rect,
    current_row: &mut u16,
    image_requests: &mut Vec<ImageRequest>,
    image_rows: u16,
    cell_pixel_size: Option<(u16, u16)>,
) {
    if let Some(png) = &data.image_png {
        let available = area.bottom().saturating_sub(*current_row);
        // Compute rows from image aspect ratio so the display height scales with
        // figsize.  image_rows acts as a cap, not a fixed height.
        let natural_rows = if let Some((pw, ph)) = png_pixel_size(png) {
            compute_image_rows(pw, ph, area.width, cell_pixel_size, image_rows)
        } else {
            image_rows
        };
        let reserved = natural_rows.min(available);
        if reserved > 0 {
            let image_top = *current_row;

            // Placeholder width = the same column count Kitty will use so the
            // dark background matches the rendered image footprint exactly.
            let placeholder_cols = if let Some((pw, ph)) = png_pixel_size(png) {
                estimated_image_cols(pw, ph, reserved, cell_pixel_size).min(area.width)
            } else {
                area.width
            };

            // Draw a dark placeholder block; Kitty will paint over it.
            for r in 0..reserved {
                let row_area = Rect {
                    x: area.x,
                    y: image_top + r,
                    width: placeholder_cols,
                    height: 1,
                };
                let label = if r == 0 { "  ▸ image ".to_string() } else { String::new() };
                frame.render_widget(
                    SingleLineWidget {
                        text: label,
                        style: Style::default()
                            .bg(Color::Rgb(10, 10, 20))
                            .fg(Color::DarkGray),
                    },
                    row_area,
                );
            }
            image_requests.push(ImageRequest {
                col: area.x,
                row: image_top,
                rows: reserved,
                cols: placeholder_cols,
                png_data: png.clone(),
            });
            *current_row += reserved;
        }
    } else if let Some(text) = &data.text_plain {
        for line in text.lines() {
            if *current_row >= area.bottom() {
                break;
            }
            let row_area = single_row(area, *current_row);
            frame.render_widget(
                SingleLineWidget {
                    text: format!("  {line}"),
                    style: Style::default().fg(Color::Cyan),
                },
                row_area,
            );
            *current_row += 1;
        }
    }
}

fn single_row(area: Rect, row: u16) -> Rect {
    Rect {
        x: area.x,
        y: row,
        width: area.width,
        height: 1,
    }
}

// ---------------------------------------------------------------------------
// Custom widgets
// ---------------------------------------------------------------------------

struct SingleLineWidget {
    text: String,
    style: Style,
}

impl Widget for SingleLineWidget {
    fn render(self, area: Rect, buf: &mut RatBuffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let y = area.top();
        let mut x = area.left();
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
