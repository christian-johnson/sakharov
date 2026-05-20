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
    notebook::{Cell, CellType, KernelStatus, MimeData, Notebook, Output},
    notebook_state::NotebookState,
};

/// Number of terminal cell rows reserved for each image output.
const IMAGE_ROWS: u16 = 12;

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
}

/// A request to render a PNG image via the Kitty graphics protocol.
pub struct ImageRequest {
    pub col: u16,
    pub row: u16,
    pub rows: u16,
    pub png_data: Vec<u8>,
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

    render_cells(frame, state, nb, active, lsp_diagnostics, content_area)
}

// ---------------------------------------------------------------------------
// Cell rendering
// ---------------------------------------------------------------------------

fn render_cells(
    frame: &mut Frame,
    state: &NotebookState,
    nb: &Notebook,
    active: &ActiveCellView<'_>,
    lsp_diagnostics: &std::collections::HashMap<String, Vec<Diagnostic>>,
    area: Rect,
) -> (Vec<ImageRequest>, Option<(u16, u16)>) {
    let mut image_requests = Vec::new();
    let mut current_row = area.top();
    let mut focused_cell_screen_pos: Option<(u16, u16)> = None;

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
        // Use the live buffer rope for the focused cell's height so it reflects
        // any edits made since the last save.
        let cell_height = if is_focused {
            focused_cell_display_height(active.rope, cell).min(remaining)
        } else {
            cell_display_height(cell).min(remaining)
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
            let cursor_screen = render_cell_content(
                frame, nb, cell, cell_idx, is_focused, inner, active,
                lsp_diagnostics, &mut image_requests,
            );
            if is_focused {
                focused_cell_screen_pos = cursor_screen;
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
) -> Option<(u16, u16)> {
    // For the focused cell, use the live buffer rope; otherwise use stored source.
    let rope_storage;
    let (rope, cursor_char_idx, sel_range, scroll_row) = if is_focused {
        let lo = active.cursor.min(active.sel_anchor);
        let hi = active.cursor.max(active.sel_anchor);
        (active.rope, Some(active.cursor), (lo, hi), active.scroll_row)
    } else {
        rope_storage = cell.source.clone();
        (&rope_storage, None, (0usize, 0usize), 0usize)
    };

    let source_text = rope.to_string();
    let source_lines: Vec<&str> = if source_text.is_empty() {
        vec![""]
    } else {
        source_text.split('\n').collect()
    };

    let highlight_spans = if cell.cell_type == CellType::Code {
        let hl = Highlighter::new(Some(std::path::Path::new(&format!(
            "_.{}",
            lang_to_ext(&nb.metadata.kernel_language)
        ))));
        hl.highlight(rope).unwrap_or_default()
    } else {
        vec![]
    };

    // Collect diagnostics for this cell's virtual path (e.g. notebook__cell0.py).
    // Format: (line_within_cell, col_start, col_end, severity).
    let cell_diag_ranges: Vec<(usize, usize, usize, DiagnosticSeverity)> = {
        let lang = &nb.metadata.kernel_language;
        let ext = lang_to_ext(lang);
        let stem = nb.path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "notebook".into());
        let dir = nb.path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let vpath = dir.join(format!("{stem}__cell{cell_idx}.{ext}"));
        let key = vpath.to_string_lossy().to_string();
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
        highlight_spans: &highlight_spans,
        is_code: cell.cell_type == CellType::Code,
        diag_ranges: &cell_diag_ranges,
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
            render_output(frame, output, area, &mut current_row, image_requests);
        }
    }

    cursor_screen
}

// ---------------------------------------------------------------------------
// Cell height / colour helpers
// ---------------------------------------------------------------------------

fn cell_display_height(cell: &Cell) -> u16 {
    let source_text = cell.source.to_string();
    let source_lines = source_text.lines().count().max(1) as u16;
    let output_h: u16 = if cell.cell_type == CellType::Code && !cell.outputs.is_empty() {
        1 + cell.outputs.iter().map(single_output_height_count).sum::<u16>()
    } else {
        0
    };
    2 + source_lines + output_h // 2 = top border + bottom border
}

fn focused_cell_display_height(rope: &ropey::Rope, cell: &Cell) -> u16 {
    let source_lines = rope.len_lines().max(1) as u16;
    let output_h: u16 = if cell.cell_type == CellType::Code && !cell.outputs.is_empty() {
        1 + cell.outputs.iter().map(single_output_height_count).sum::<u16>()
    } else {
        0
    };
    2 + source_lines + output_h
}

fn single_output_height_count(output: &Output) -> u16 {
    match output {
        Output::Stream { text, .. } => {
            let n = text.lines().count();
            let shown = n.min(20);
            let extra = if n > 20 { 1 } else { 0 };
            (shown + extra).max(1) as u16
        }
        Output::DisplayData { data } | Output::ExecuteResult { data, .. } => {
            if data.image_png.is_some() {
                IMAGE_ROWS
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
/// Blue = not yet run, Green = success, Red = errored, Yellow = running.
fn cell_border_color(cell: &Cell, executing_cell: Option<usize>, cell_idx: usize) -> Color {
    if executing_cell == Some(cell_idx) {
        return Color::Yellow;
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
    highlight_spans: &'a [(usize, usize, usize)],
    is_code: bool,
    /// Diagnostic ranges for this cell: (line_within_cell, col_start, col_end, severity).
    diag_ranges: &'a [(usize, usize, usize, DiagnosticSeverity)],
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
    let cursor_style = match ctx.mode {
        Mode::Insert => Style::default().bg(Color::Green).fg(Color::Black),
        _ => Style::default().bg(Color::White).fg(Color::Black),
    };
    let selection_style = Style::default().bg(Color::Rgb(60, 80, 120)).fg(Color::White);
    let (sel_lo, sel_hi) = ctx.sel_range;
    let has_selection = sel_lo != sel_hi;

    let mut x = content_area.x;
    let buf = frame.buffer_mut();

    for (char_off, c) in line.chars().enumerate() {
        if x >= content_area.right() {
            break;
        }
        let char_idx = line_start_char + char_off;
        let base_style = if ctx.is_code {
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
        let line_len = line.chars().count();
        if cp == line_start_char + line_len && x < content_area.right() {
            frame.buffer_mut()[(x, area.y)]
                .set_char(' ')
                .set_style(cursor_style);
        }
    }
}

fn render_output(
    frame: &mut Frame,
    output: &Output,
    area: Rect,
    current_row: &mut u16,
    image_requests: &mut Vec<ImageRequest>,
) {
    match output {
        Output::Stream { name, text } => {
            let style = if name == "stderr" {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            let lines: Vec<&str> = text.lines().collect();
            let max_lines = 20usize;
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
            render_mime_data(frame, data, area, current_row, image_requests);
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
            for tb_line in traceback.iter().take(5) {
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

fn render_mime_data(
    frame: &mut Frame,
    data: &MimeData,
    area: Rect,
    current_row: &mut u16,
    image_requests: &mut Vec<ImageRequest>,
) {
    if let Some(png) = &data.image_png {
        // Reserve IMAGE_ROWS rows so the Kitty render can't overlap the next cell.
        let available = area.bottom().saturating_sub(*current_row);
        let reserved = IMAGE_ROWS.min(available);
        if reserved > 0 {
            let image_top = *current_row;
            // Draw a dark placeholder block; Kitty will paint over it.
            for r in 0..reserved {
                let row_area = single_row(area, image_top + r);
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
// Notebook status bar / command line (called from ui.rs helpers)
// ---------------------------------------------------------------------------

/// Render the notebook status bar.
pub fn render_notebook_status(
    frame: &mut Frame,
    nb: &Notebook,
    state: &NotebookState,
    kernel_status: Option<&KernelStatus>,
    area: Rect,
    mode_label: &str,
) {
    let mode_style = match mode_label {
        "INS" => Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD),
        "SEL" => Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
        "NOR" => Style::default().fg(Color::Black).bg(Color::Blue).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
    };

    let filename = nb
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("notebook.ipynb");
    let modified = if nb.modified { " [+]" } else { "" };
    let cell_pos = format!(
        "{}/{}",
        state.focused_cell + 1,
        nb.cells.len().max(1)
    );
    let kernel_indicator = match kernel_status {
        Some(KernelStatus::Idle) => " [idle]",
        Some(KernelStatus::Busy) => " [busy]",
        Some(KernelStatus::Dead) => " [dead]",
        None => " [no kernel]",
    };
    let right = format!("{cell_pos}{kernel_indicator}");

    frame.render_widget(
        NotebookStatusWidget {
            mode_label: format!(" {mode_label} "),
            mode_style,
            filename: format!("  {filename}{modified}  "),
            right,
        },
        area,
    );
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

struct NotebookStatusWidget {
    mode_label: String,
    mode_style: Style,
    filename: String,
    right: String,
}

impl Widget for NotebookStatusWidget {
    fn render(self, area: Rect, buf: &mut RatBuffer) {
        if area.height == 0 {
            return;
        }
        let y = area.top();
        let bg_style = Style::default().bg(Color::DarkGray).fg(Color::White);

        // Fill background.
        for col in area.left()..area.right() {
            buf[(col, y)].set_char(' ').set_style(bg_style);
        }

        let mut x = area.left();

        // Mode label.
        for c in self.mode_label.chars() {
            if x >= area.right() {
                break;
            }
            buf[(x, y)].set_char(c).set_style(self.mode_style);
            x += 1;
        }

        // Filename.
        for c in self.filename.chars() {
            if x >= area.right() {
                break;
            }
            buf[(x, y)].set_char(c).set_style(bg_style);
            x += 1;
        }

        // Right-aligned cell position.
        let right_width = self.right.len() as u16;
        if area.right() >= right_width {
            let rx = area.right() - right_width;
            let mut rx2 = rx;
            for c in self.right.chars() {
                if rx2 >= area.right() {
                    break;
                }
                buf[(rx2, y)].set_char(c).set_style(bg_style);
                rx2 += 1;
            }
        }
    }
}
