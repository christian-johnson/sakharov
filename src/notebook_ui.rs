use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders},
    Frame,
};

use crate::{
    highlight::{self, Highlighter},
    lang::lang_to_ext,
    lsp_manager::{Diagnostic, DiagnosticSeverity},
    mode::Mode,
    notebook::{Cell, CellType, MimeData, Notebook, Output},
    notebook_state::NotebookState,
    render_util::{apply_diag_underline, for_each_jump_label_char, SingleLineWidget},
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
    /// Jump-mode labels to overlay on the cell source (`app.jump.labels`).
    /// Empty slice when not in Jump mode.
    pub jump_labels: &'a [(usize, String)],
    /// Characters typed so far in Jump mode (`app.jump.typed`).
    pub jump_typed: &'a str,
    /// The `editor.word_wrap` toggle — non-markdown cells wrap when set
    /// (markdown cells always wrap; see [`cell_wraps`]).
    pub word_wrap: bool,
}

/// Cache of per-cell highlight spans plus the shared tree-sitter highlighter
/// for the notebook's kernel language.
///
/// Building a `HighlightConfiguration` parses the grammar's highlight query —
/// far too expensive to repeat per frame — and re-highlighting unchanged cells
/// is wasted tree-sitter work.  Entries are keyed by cell index and validated
/// by a content fingerprint, so both costs are paid only when a cell's text
/// (or render kind) actually changes.  No invalidation plumbing is needed:
/// structural edits shift indices but the fingerprint check makes a stale
/// entry recompute rather than mis-render.
#[derive(Default)]
pub struct CellHighlightCache {
    lang_ext: String,
    highlighter: Option<Highlighter>,
    spans: std::collections::HashMap<usize, (u64, Vec<highlight::Span>)>,
}

/// How a cell's content is highlighted.
#[derive(Clone, Copy, PartialEq)]
enum CellKind {
    /// No highlighting (raw cells, markdown source view).
    Plain,
    /// Tree-sitter highlighting in the kernel language.
    Code,
    /// Rendered-markdown highlighting.
    Markdown,
}

fn cell_fingerprint(rope: &ropey::Rope, kind: CellKind) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write_u8(match kind {
        CellKind::Plain => 0,
        CellKind::Code => 1,
        CellKind::Markdown => 2,
    });
    for chunk in rope.chunks() {
        h.write(chunk.as_bytes());
    }
    h.finish()
}

impl CellHighlightCache {
    /// The shared highlighter for `lang`, (re)built only on language change.
    fn highlighter_for(&mut self, lang: &str) -> &mut Highlighter {
        let ext = lang_to_ext(lang);
        if self.highlighter.is_none() || self.lang_ext != ext {
            self.lang_ext = ext.to_owned();
            let fake = format!("_.{ext}");
            self.highlighter = Some(Highlighter::new(Some(std::path::Path::new(&fake))));
            self.spans.clear();
        }
        self.highlighter.as_mut().expect("just ensured")
    }

    /// Highlight spans for cell `idx` with content `rope`, recomputed only
    /// when the content fingerprint changes.
    fn spans_for(&mut self, lang: &str, idx: usize, rope: &ropey::Rope, kind: CellKind) -> &[highlight::Span] {
        if kind == CellKind::Plain {
            return &[];
        }
        let fp = cell_fingerprint(rope, kind);
        let stale = self.spans.get(&idx).map(|(h, _)| *h != fp).unwrap_or(true);
        if stale {
            let spans = match kind {
                CellKind::Code => self.highlighter_for(lang).highlight(rope).unwrap_or_default(),
                _ => crate::markdown::highlight(rope),
            };
            self.spans.insert(idx, (fp, spans));
        }
        &self.spans[&idx].1
    }
}

/// True when a cell's content word-wraps to the cell width.
///
/// Markdown cells always wrap — prose, in both the rendered view and the
/// editable source view. Other cells follow the `editor.word_wrap` toggle.
/// This is the single predicate deciding wrapping, used by the renderer,
/// [`cell_display_height`], and the in-cell scroll math (`exec::update_scroll`)
/// — they must agree or cell heights drift from what is actually drawn.
/// Notebook cells have no horizontal scroll, so a non-wrapped long line clips
/// at the cell border.
pub(crate) fn cell_wraps(cell: &Cell, word_wrap: bool) -> bool {
    cell.cell_type == CellType::Markdown || word_wrap
}

/// Word-wrap a logical line into visual-row segments of at most `width` chars.
///
/// Breaks at the last space within the window when possible (the space is
/// consumed by the break); a single word longer than `width` is hard-broken.
/// Returns `(char_offset_within_line, segment)` pairs — always at least one,
/// so an empty line still occupies one row. Char-based, like the rest of the
/// cell renderer (the width-1-chars assumption is a known rough edge).
fn wrap_segments(line: &str, width: usize) -> Vec<(usize, &str)> {
    let width = width.max(1);
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let n = chars.len();
    if n <= width {
        return vec![(0, line)];
    }
    let byte_at = |ci: usize| if ci < n { chars[ci].0 } else { line.len() };
    let mut segs = Vec::new();
    let mut start = 0usize; // char index of the current segment's first char
    while n - start > width {
        let limit = start + width; // exclusive end of a full-width segment
        // A space at `limit` itself is the ideal break: the segment is exactly
        // full and the space dies at the boundary.
        let brk = (start + 1..=limit).rev().find(|&i| chars[i].1 == ' ');
        let (end, next) = match brk {
            Some(i) => (i, i + 1),
            None => (limit, limit),
        };
        segs.push((start, &line[byte_at(start)..byte_at(end)]));
        start = next;
    }
    segs.push((start, &line[byte_at(start)..]));
    segs
}

/// Total visual rows of a source rope when word-wrapped to `width` chars.
/// Must mirror the renderer exactly: same line split, same segmentation.
fn wrapped_source_rows(source: &ropey::Rope, width: usize) -> u16 {
    let text = source.to_string();
    let lines: Vec<&str> = if text.is_empty() { vec![""] } else { text.split('\n').collect() };
    lines.iter().map(|l| wrap_segments(l, width).len()).sum::<usize>() as u16
}

/// Columns available for cell text given the inner (within-borders) width:
/// the renderer indents every source line by a 2-char pad.
pub(crate) fn cell_text_width(inner_cols: u16) -> usize {
    inner_cols.saturating_sub(2).max(1) as usize
}

/// The wrapped sub-row of `line` that owns column `col` (0 when not wrapping).
/// Ownership matches the renderer: a break-consumed space belongs to the row
/// it ends; the char right after a hard break starts the next row.
fn cursor_sub_row(line: &str, width: usize, col: usize) -> usize {
    wrap_segments(line, width)
        .iter()
        .rposition(|&(off, _)| off <= col)
        .unwrap_or(0)
}

/// The cursor's visual row within a (possibly wrapped) cell: wrapped rows of
/// every line above it, plus its sub-row within its own line.  `width = None`
/// means no wrapping (visual row == logical line).  Used by the in-cell
/// scroll in `exec::update_scroll`; must mirror the renderer's segmentation.
pub(crate) fn cell_cursor_visual_row(rope: &ropey::Rope, cursor: usize, width: Option<usize>) -> usize {
    let pos = cursor.min(rope.len_chars());
    let line_idx = if rope.len_chars() == 0 { 0 } else { rope.char_to_line(pos) };
    let Some(width) = width else { return line_idx };
    let text = rope.to_string();
    let lines: Vec<&str> = if text.is_empty() { vec![""] } else { text.split('\n').collect() };
    let mut vrow = 0usize;
    for line in lines.iter().take(line_idx) {
        vrow += wrap_segments(line, width).len();
    }
    let col = pos - rope.line_to_char(line_idx.min(rope.len_lines().saturating_sub(1)));
    vrow + lines.get(line_idx).map(|l| cursor_sub_row(l, width, col)).unwrap_or(0)
}

/// Total visual rows of a cell's source (`width = None` → logical line count).
pub(crate) fn cell_visual_rows(rope: &ropey::Rope, width: Option<usize>) -> usize {
    match width {
        Some(w) => wrapped_source_rows(rope, w) as usize,
        None => rope.len_lines().max(1),
    }
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
#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    state: &NotebookState,
    nb: &Notebook,
    active: &ActiveCellView<'_>,
    lsp_diagnostics: &std::collections::HashMap<String, Vec<Diagnostic>>,
    nb_config: &crate::config::NotebookConfig,
    cell_pixel_size: Option<(u16, u16)>,
    cache: &mut CellHighlightCache,
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

    render_cells(frame, state, nb, active, lsp_diagnostics, content_area, nb_config, cell_pixel_size, cache)
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
    cache: &mut CellHighlightCache,
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
        let is_folded = state.is_cell_folded(cell_idx);
        // Inner column width available for cell content (subtract left+right borders).
        let inner_cols = area.width.saturating_sub(2).max(4);
        // Folded cells always get the compact height regardless of focus.
        // For the focused non-folded cell, use the live buffer rope height.
        let cell_height = if is_folded {
            3u16.min(remaining) // border-top + 1 summary line + border-bottom
        } else {
            let source = if is_focused { active.rope } else { &cell.source };
            cell_display_height(
                source, cell, nb_config.image_rows, cell_pixel_size, inner_cols, active.word_wrap,
            )
            .min(remaining)
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

        let th = crate::theme::active();
        let title_style = if is_focused {
            Style::default().fg(th.fg()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.dim)
        };

        let block = Block::default()
            .title(ratatui::text::Span::styled(title, title_style))
            .borders(Borders::ALL)
            .border_type(if is_focused { BorderType::Thick } else { BorderType::Rounded })
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(th.cell_bg));

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
                    lsp_diagnostics, &mut image_requests, cache, nb_config,
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
    cache: &mut CellHighlightCache,
    nb_config: &crate::config::NotebookConfig,
    cell_pixel_size: Option<(u16, u16)>,
) -> Option<(u16, u16)> {
    // For the focused cell, use the live buffer rope; otherwise use stored source.
    let rope: &ropey::Rope = if is_focused { active.rope } else { &cell.source };

    // A Markdown cell shows its formatted (highlighted) view when `rendered`,
    // except while it's the focused cell being actively edited (Insert/Select) —
    // then we show the raw source so the markup is editable. (Entering Insert
    // also flips `rendered` off, so navigating over it in Normal keeps it
    // rendered until you start editing or convert/re-render it.)
    let editing_this = is_focused && matches!(active.mode, Mode::Insert | Mode::Select);
    let show_markdown = cell.cell_type == CellType::Markdown && cell.rendered && !editing_this;

    // The cursor stays visible in the rendered markdown view too: rendering
    // only restyles the source text (header colours, bold, …) — it never
    // transforms it — so char indices map 1:1 to displayed characters and
    // `j`/`k` passing through the cell keeps a visible cursor.
    let (cursor_char_idx, sel_range, scroll_row) = if is_focused {
        let lo = active.cursor.min(active.sel_anchor);
        let hi = active.cursor.max(active.sel_anchor);
        (Some(active.cursor), (lo, hi), active.scroll_row)
    } else {
        (None, (0usize, 0usize), 0)
    };

    let source_text = rope.to_string();
    let source_lines: Vec<&str> = if source_text.is_empty() {
        vec![""]
    } else {
        source_text.split('\n').collect()
    };

    let kind = if cell.cell_type == CellType::Code {
        CellKind::Code
    } else if show_markdown {
        CellKind::Markdown
    } else {
        CellKind::Plain
    };
    let highlight_spans =
        cache.spans_for(&nb.metadata.kernel_language, cell_idx, rope, kind);

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
        highlight_spans,
        use_highlight: kind != CellKind::Plain,
        diag_ranges: &cell_diag_ranges,
        // Only overlay jump labels on the focused cell.
        jump_labels: if is_focused { active.jump_labels } else { &[] },
        jump_typed: if is_focused { active.jump_typed } else { "" },
    };

    let mut current_row = area.top();
    let mut cursor_screen: Option<(u16, u16)> = None;
    let pad_len = 2u16; // leading spaces

    // Word-wrap the cell content to its text width (markdown always; other
    // cells per the word_wrap toggle). The wrap width must match
    // `cell_display_height` (via `cell_text_width`) or the cell border won't
    // enclose the wrapped content.
    let wrap_width = cell_wraps(cell, active.word_wrap).then(|| cell_text_width(area.width));

    // `scroll_row` counts *visual* rows within the cell (== logical lines
    // when not wrapping); `exec::update_scroll` maintains it in the same
    // units via `cell_cursor_visual_row`.
    let mut skip_rows = scroll_row;
    // Running char offset of the current line's start (O(L) total, not O(L²)).
    let mut next_line_start: usize = 0;
    'lines: for (line_no, line) in source_lines.iter().enumerate() {
        let line_start_char = next_line_start;
        let line_len = line.chars().count();
        next_line_start += line_len + 1;

        if current_row >= area.bottom() {
            break;
        }

        let segments: Vec<(usize, &str)> = match wrap_width {
            Some(w) => wrap_segments(line, w),
            None => vec![(0, *line)],
        };
        let n_segs = segments.len();
        for (k, &(seg_off, seg)) in segments.iter().enumerate() {
            // Honour intra-cell scroll offset (visual rows).
            if skip_rows > 0 {
                skip_rows -= 1;
                continue;
            }
            if current_row >= area.bottom() {
                break 'lines;
            }
            let is_last_seg = k + 1 == n_segs;
            let seg_len = seg.chars().count();
            // Char range this row owns: up to the next segment's start (so a
            // break-consumed space belongs to the row it ends), or through the
            // end-of-line cursor position for the final row.
            let owned_end = if is_last_seg { line_len + 1 } else { segments[k + 1].0 };

            // Cursor screen position when the cursor sits on this row.
            if let Some(ci) = cursor_char_idx {
                if ci >= line_start_char + seg_off && ci < line_start_char + owned_end {
                    // Clamp a cursor on a break-consumed space to the row end.
                    let col = (ci - line_start_char - seg_off).min(seg_len);
                    let screen_x = area.x + pad_len + col as u16;
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
                seg,
                line_no,
                line_start_char + seg_off,
                seg_off,
                // The end-of-row cursor cell: on the final row it marks the
                // end-of-line position; on a word-break row it marks the
                // consumed space. After a hard break that position belongs to
                // the next row's first char instead — don't double-draw.
                is_last_seg || owned_end > seg_off + seg_len,
                &line_ctx,
            );
            current_row += 1;
        }
    }

    if cell.cell_type == CellType::Code && !cell.outputs.is_empty() {
        if current_row < area.bottom() {
            frame.render_widget(
                SingleLineWidget {
                    text: " \u{2500}\u{2500} output \u{2500}\u{2500}".to_string(),
                    style: Style::default().fg(crate::theme::active().dim),
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

    let total_lines = source.len_lines().max(1);
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

    let th = crate::theme::active();
    let content_style = Style::default().fg(th.dim);
    let arrow_style = Style::default().fg(th.accent);
    let count_style = Style::default().fg(th.dim);

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

/// Display height of a cell in terminal rows: borders + source lines + outputs.
///
/// `source` is the rope whose line count to use — `&cell.source` normally, or
/// the live editor rope for the focused cell (whose unsaved edits are in
/// `app.buffer`, ahead of the stored source).  `len_lines()` is O(1) on a Rope;
/// wrapping cells (see [`cell_wraps`]) instead count word-wrapped rows
/// (O(len)) so the height matches what the renderer draws.
pub fn cell_display_height(
    source: &ropey::Rope,
    cell: &Cell,
    max_image_rows: u16,
    cell_pixel_size: Option<(u16, u16)>,
    available_cols: u16,
    word_wrap: bool,
) -> u16 {
    let source_lines = if cell_wraps(cell, word_wrap) {
        wrapped_source_rows(source, cell_text_width(available_cols)).max(1)
    } else {
        source.len_lines().max(1) as u16
    };
    let output_h: u16 = if cell.cell_type == CellType::Code && !cell.outputs.is_empty() {
        1 + cell.outputs.iter()
            .map(|o| single_output_height_count(o, max_image_rows, cell_pixel_size, available_cols))
            .sum::<u16>()
    } else {
        0
    };
    2 + source_lines + output_h // 2 = top border + bottom border
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
                    .map(|t| t.lines().count().clamp(1, 20))
                    .unwrap_or(0) as u16
            }
        }
        Output::Error { traceback, .. } => 1 + traceback.len().min(5) as u16,
    }
}

/// Returns the border colour reflecting the cell's execution state
/// (theme `[notebook]` colors): not yet run, running, success, errored.
fn cell_border_color(cell: &Cell, executing_cell: Option<usize>, cell_idx: usize) -> Color {
    let th = crate::theme::active();
    if executing_cell == Some(cell_idx) {
        // Brighter while the cell streams output, distinct from the dim
        // border of an un-run cell.
        return th.nb_border_running;
    }
    if cell.outputs.iter().any(|o| matches!(o, Output::Error { .. })) {
        return th.nb_border_error;
    }
    if cell.execution_count.is_some() {
        return th.nb_border_ok;
    }
    th.nb_border
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
    /// When true, render characters with their highlight spans (code cells, and
    /// rendered markdown cells); when false, render as plain gray source text.
    use_highlight: bool,
    /// Diagnostic ranges for this cell: (line_within_cell, col_start, col_end, severity).
    diag_ranges: &'a [(usize, usize, usize, DiagnosticSeverity)],
    /// Jump-mode labels to overlay on the focused cell's source lines.
    jump_labels: &'a [(usize, String)],
    jump_typed: &'a str,
}

/// Render one visual row of cell source: a whole logical line, or one
/// word-wrapped segment of it. `line_start_char` is the segment's absolute
/// char index in the cell; `col_offset` its char offset within the logical
/// line (for diagnostic column matching). `cursor_eol_cell` enables the
/// styled cursor cell one past the segment's last char (end of line, or a
/// break-consumed space) — false after a hard break, where that position is
/// the next row's first char.
#[allow(clippy::too_many_arguments)]
fn render_source_line(
    frame: &mut Frame,
    area: Rect,
    line: &str,
    line_no: usize,
    line_start_char: usize,
    col_offset: usize,
    cursor_eol_cell: bool,
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
    let th = crate::theme::active();
    let cursor_style = crate::theme::cursor_style(ctx.mode);
    let selection_style = Style::default()
        .bg(th.cell_selection_bg)
        .fg(th.selection_fg.unwrap_or_else(|| th.fg()));
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
            // Plain (raw) cell text: slightly de-emphasized.
            Style::default().fg(match th.foreground {
                Some(_) => th.dim,
                None => Color::Gray,
            })
        };
        let style = if ctx.cursor_pos == Some(char_idx) {
            cursor_style
        } else if has_selection && char_idx >= sel_lo && char_idx < sel_hi {
            selection_style
        } else {
            base_style
        };
        // Diagnostic underline (does not override cursor/selection colours).
        // Diagnostic columns are logical-line-relative; offset by the
        // segment's position within its line.
        let col_in_line = col_offset + char_off;
        let style = apply_diag_underline(
            style,
            ctx.diag_ranges
                .iter()
                .filter(|(dl, cs, ce, _)| *dl == line_no && col_in_line >= *cs && col_in_line < *ce)
                .map(|(_, _, _, sev)| sev),
        );
        buf[(x, area.y)].set_char(c).set_style(style);
        x += 1;
    }

    // Cursor one past the segment's last char (end of line, empty line, or a
    // word-break-consumed space).
    if let Some(cp) = ctx.cursor_pos {
        if cursor_eol_cell && cp == line_start_char + line_len && x < content_area.right() {
            buf[(x, area.y)].set_char(' ').set_style(cursor_style);
        }
    }

    // Jump label overlay — paint over already-rendered characters.
    for_each_jump_label_char(
        ctx.jump_labels,
        ctx.jump_typed,
        line_start_char,
        line_len,
        |char_off, lc, style| {
            let col = content_x + char_off as u16;
            if col < content_area.right() {
                buf[(col, area.y)].set_char(lc).set_style(style);
            }
        },
    );
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
    let th = crate::theme::active();
    match output {
        Output::Stream { name, text } => {
            let style = if name == "stderr" {
                Style::default().fg(th.warning)
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
                        style: Style::default().fg(th.dim),
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
                        style: Style::default().fg(th.error),
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
                        style: Style::default().fg(th.dim),
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
                let th = crate::theme::active();
                frame.render_widget(
                    SingleLineWidget {
                        text: label,
                        style: Style::default()
                            .bg(th.output_bg)
                            .fg(th.dim),
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
                    style: Style::default().fg(crate::theme::active().info),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    #[test]
    fn wrap_segments_breaks_at_word_boundaries() {
        // Width 10: "hello brave world" → "hello" / "brave" / "world"
        let segs = wrap_segments("hello brave world", 10);
        let texts: Vec<&str> = segs.iter().map(|&(_, s)| s).collect();
        assert_eq!(texts, vec!["hello", "brave", "world"]);
        // Offsets address the original line (for highlight-span lookup).
        assert_eq!(segs[1].0, 6);
        assert_eq!(segs[2].0, 12);
        // Every segment fits the width.
        assert!(segs.iter().all(|&(_, s)| s.chars().count() <= 10));
    }

    #[test]
    fn wrap_segments_hard_breaks_long_words_and_keeps_short_lines() {
        let segs = wrap_segments("abcdefghij", 4);
        let texts: Vec<&str> = segs.iter().map(|&(_, s)| s).collect();
        assert_eq!(texts, vec!["abcd", "efgh", "ij"]);
        // Short and empty lines occupy exactly one row.
        assert_eq!(wrap_segments("short", 80).len(), 1);
        assert_eq!(wrap_segments("", 80).len(), 1);
    }

    #[test]
    fn markdown_height_counts_wrapped_rows() {
        let long = "word ".repeat(30); // 150 chars of prose
        let make = |cell_type, rendered| Cell {
            id: "t".into(),
            cell_type,
            source: Rope::from_str(&long),
            outputs: vec![],
            execution_count: None,
            rendered,
        };

        let inner_cols = 42u16; // text width 40 → 150 chars ≈ 4 rows
        let expected_rows = wrapped_source_rows(&Rope::from_str(&long), cell_text_width(inner_cols));
        assert!(expected_rows > 1, "long prose must wrap to several rows");

        // Markdown wraps in both the rendered view and the source view —
        // word_wrap toggle irrelevant.
        let md = make(CellType::Markdown, true);
        assert_eq!(cell_display_height(&md.source, &md, 40, None, inner_cols, false), 2 + expected_rows);
        let md_src = make(CellType::Markdown, false);
        assert_eq!(cell_display_height(&md_src.source, &md_src, 40, None, inner_cols, false), 2 + expected_rows);

        // Code cells follow the word_wrap toggle.
        let code = make(CellType::Code, false);
        assert_eq!(cell_display_height(&code.source, &code, 40, None, inner_cols, false), 2 + 1);
        assert_eq!(cell_display_height(&code.source, &code, 40, None, inner_cols, true), 2 + expected_rows);
    }

    /// Navigating through a *rendered* markdown cell must keep the cursor
    /// visible — the rendered view restyles the source without transforming
    /// it, so the cursor maps 1:1. (Regression: it used to be suppressed,
    /// vanishing while `j` passed through markdown cells.)
    #[test]
    fn cursor_is_visible_in_rendered_markdown_cell() {
        let cell = Cell {
            id: "m".into(),
            cell_type: CellType::Markdown,
            source: Rope::from_str("# Heading\n\nSome prose here."),
            outputs: vec![],
            execution_count: None,
            rendered: true,
        };
        let nb = Notebook {
            path: std::path::PathBuf::from("/tmp/cursor-test.ipynb"),
            metadata: crate::notebook::NotebookMeta { kernel_language: "python".into() },
            cells: vec![cell],
            modified: false,
            kernel: None,
        };
        let state = NotebookState::new();
        let rope = nb.cells[0].source.clone();
        let mode = Mode::Normal;
        let active = ActiveCellView {
            rope: &rope,
            cursor: 2, // on the 'H' of "# Heading"
            sel_anchor: 2,
            scroll_row: 0,
            mode: &mode,
            jump_labels: &[],
            jump_typed: "",
            word_wrap: false,
        };

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut cursor_pos = None;
        terminal
            .draw(|f| {
                let (_imgs, cursor) = render(
                    f,
                    &state,
                    &nb,
                    &active,
                    &std::collections::HashMap::new(),
                    &crate::config::NotebookConfig::default(),
                    None,
                    &mut CellHighlightCache::default(),
                );
                cursor_pos = cursor;
            })
            .unwrap();

        let (cx, cy) = cursor_pos.expect("cursor must be visible in a rendered markdown cell");
        // Border (1) + 2-char pad + cursor col 2 within the first line.
        assert_eq!((cx, cy), (1 + 2 + 2, 1));
    }

    #[test]
    fn cursor_visual_row_tracks_wrapped_sub_rows() {
        // Two logical lines; the first wraps to 3 rows at width 10.
        let rope = Rope::from_str("hello brave world\nsecond");
        let w = Some(10usize);
        // Cursor at start → row 0; on "brave" → row 1; on "world" → row 2.
        assert_eq!(cell_cursor_visual_row(&rope, 0, w), 0);
        assert_eq!(cell_cursor_visual_row(&rope, 7, w), 1);
        assert_eq!(cell_cursor_visual_row(&rope, 13, w), 2);
        // End of first line (after "world") stays on its last row.
        assert_eq!(cell_cursor_visual_row(&rope, 17, w), 2);
        // Second logical line starts after all wrapped rows of the first.
        assert_eq!(cell_cursor_visual_row(&rope, 18, w), 3);
        // Without wrapping, visual row == logical line.
        assert_eq!(cell_cursor_visual_row(&rope, 13, None), 0);
        assert_eq!(cell_cursor_visual_row(&rope, 18, None), 1);
        // Totals agree with the segmentation.
        assert_eq!(cell_visual_rows(&rope, w), 4);
        assert_eq!(cell_visual_rows(&rope, None), 2);
    }
}

