use ratatui::{
    buffer::Buffer as RatBuffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, Widget},
    Frame,
};

use crate::{
    config::Config,
    highlight::Highlighter,
    notebook::{Cell, CellType, KernelStatus, MimeData, Notebook, Output},
    notebook_state::{NotebookEditMode, NotebookState},
};

/// Number of terminal cell rows reserved for each image output.
const IMAGE_ROWS: u16 = 12;

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
pub fn render(
    frame: &mut Frame,
    state: &NotebookState,
    nb: &Notebook,
    config: &Config,
) -> Vec<ImageRequest> {
    let size = frame.area();
    if size.height < 3 {
        return vec![];
    }

    // Content area — leave last 2 rows for status bar + command line.
    let content_area = Rect {
        x: size.x,
        y: size.y,
        width: size.width,
        height: size.height.saturating_sub(2),
    };

    render_cells(frame, state, nb, config, content_area)
}

// ---------------------------------------------------------------------------
// Cell rendering
// ---------------------------------------------------------------------------

fn render_cells(
    frame: &mut Frame,
    state: &NotebookState,
    nb: &Notebook,
    config: &Config,
    area: Rect,
) -> Vec<ImageRequest> {
    let mut image_requests = Vec::new();
    let mut current_row = area.top();

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
        let cell_height = cell_display_height(cell).min(remaining);

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
            render_cell_content(
                frame, state, nb, cell, is_focused, inner, config, &mut image_requests,
            );
        }

        current_row += cell_height + 1; // +1 blank gap between cells
    }

    image_requests
}

/// Render source lines and outputs inside a cell's bordered inner area.
fn render_cell_content(
    frame: &mut Frame,
    state: &NotebookState,
    nb: &Notebook,
    cell: &Cell,
    is_focused: bool,
    area: Rect,
    config: &Config,
    image_requests: &mut Vec<ImageRequest>,
) {
    let source_text = cell.source.to_string();
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
        hl.highlight(&cell.source).unwrap_or_default()
    } else {
        vec![]
    };

    let cursor_pos = if is_focused { Some(state.cursor_pos) } else { None };
    let mut current_row = area.top();

    for (line_no, line) in source_lines.iter().enumerate() {
        if current_row >= area.bottom() {
            break;
        }
        let line_start_char: usize = source_lines[..line_no]
            .iter()
            .map(|l| l.chars().count() + 1)
            .sum();
        render_source_line(
            frame,
            single_row(area, current_row),
            line,
            cell,
            line_start_char,
            cursor_pos,
            &highlight_spans,
            config,
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

fn lang_to_ext(lang: &str) -> &str {
    match lang {
        "python" | "python3" => "py",
        "javascript" | "js" => "js",
        "rust" => "rs",
        _ => "txt",
    }
}

#[allow(clippy::too_many_arguments)]
fn render_source_line(
    frame: &mut Frame,
    area: Rect,
    line: &str,
    cell: &Cell,
    line_start_char: usize,
    cursor_pos: Option<usize>,
    highlight_spans: &[(usize, usize, usize)],
    _config: &Config,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let padding = "  ";
    let pad_len = padding.chars().count() as u16;

    // Left-margin padding.
    if area.width > 0 {
        let pad_area = Rect {
            x: area.x,
            y: area.y,
            width: pad_len.min(area.width),
            height: 1,
        };
        frame.render_widget(
            SingleLineWidget {
                text: padding.to_string(),
                style: Style::default(),
            },
            pad_area,
        );
    }

    let content_x = area.x + pad_len;
    let content_width = area.width.saturating_sub(pad_len);
    if content_width == 0 {
        return;
    }
    let content_area = Rect {
        x: content_x,
        y: area.y,
        width: content_width,
        height: 1,
    };

    let is_code = cell.cell_type == CellType::Code;

    let mut x = content_area.x;
    let buf = frame.buffer_mut();

    for (char_off, c) in line.chars().enumerate() {
        if x >= content_area.right() {
            break;
        }
        let char_idx = line_start_char + char_off;

        // Determine base style.
        let base_style = if is_code {
            get_highlight_style(highlight_spans, char_idx)
        } else {
            Style::default().fg(Color::Gray)
        };

        let style = if cursor_pos == Some(char_idx) {
            Style::default().bg(Color::White).fg(Color::Black)
        } else {
            base_style
        };

        buf[(x, area.y)].set_char(c).set_style(style);
        x += 1;
    }

    // Render cursor at end-of-line if focused and cursor is past all chars.
    if let Some(cp) = cursor_pos {
        let line_len = line.chars().count();
        if cp == line_start_char + line_len && x < content_area.right() {
            let buf = frame.buffer_mut();
            buf[(x, area.y)]
                .set_char(' ')
                .set_style(Style::default().bg(Color::White).fg(Color::Black));
        }
    }
}

fn get_highlight_style(spans: &[(usize, usize, usize)], char_idx: usize) -> Style {
    let mut result = Style::default();
    for &(start, end, hl) in spans {
        if start <= char_idx && char_idx < end {
            result = crate::theme::style_for_highlight(hl);
        }
    }
    result
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
) {
    let mode_label = match state.mode {
        NotebookEditMode::Navigate => "NAV",
        NotebookEditMode::Edit => "EDT",
    };
    let mode_style = match state.mode {
        NotebookEditMode::Navigate => {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD)
        }
        NotebookEditMode::Edit => {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        }
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
