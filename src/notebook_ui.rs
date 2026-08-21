use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders},
    Frame,
};

use crate::{
    highlight::{self, Highlighter},
    kitty::ImageRequest,
    lang::lang_to_ext,
    lsp_manager::{Diagnostic, DiagnosticSeverity},
    mode::Mode,
    notebook::{Cell, CellType, MimeData, Notebook, Output},
    notebook_state::NotebookState,
    render_util::{apply_diag_underline, for_each_jump_label_char, wrap_segments, SingleLineWidget},
};


/// Info about the focused cell that comes from `app.buffer`/`app.selection`.
pub struct ActiveCellView<'a> {
    /// The rope backing the focused cell (= `app.buffer.rope`).
    pub rope: &'a ropey::Rope,
    /// Cursor char-index within that rope (= `app.selection.head`).
    pub cursor: usize,
    /// Selection anchor (= `app.selection.anchor`). Equal to cursor when no selection.
    pub sel_anchor: usize,
    /// When `Some(r)`, the cursor sits on visual row `r` of the focused cell's
    /// output block (see `NotebookState::output_row`); the source cursor is
    /// hidden and a block cursor is drawn on that output row instead.
    pub output_row: Option<usize>,
    /// Char column within `output_row`'s content (`NotebookState::output_col`).
    pub output_col: usize,
    /// Anchor of an in-progress output-text selection (`NotebookState::output_anchor`).
    pub output_anchor: Option<(usize, usize)>,
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

/// Truncation caps applied to one cell's output block: the configured limits,
/// or effectively unlimited when the user has expanded that cell's output
/// (`NotebookState::expanded_outputs`).
///
/// The height model ([`cell_output_rows`]) and the renderer must derive these
/// identically, or cell heights drift from what is actually drawn.
#[derive(Clone, Copy)]
pub struct OutputLimits {
    /// Cap on stream / text-plain output lines.
    pub max_lines: usize,
    /// Cap on traceback lines below an error's headline row.
    pub max_traceback: usize,
    /// Cap on the rows an image may occupy (never lifted by expansion — an
    /// image has no truncated tail to reveal).
    pub image_rows: u16,
}

impl OutputLimits {
    pub fn new(cfg: &crate::config::NotebookConfig, expanded: bool) -> Self {
        Self {
            max_lines: if expanded { usize::MAX } else { cfg.max_output_lines },
            max_traceback: if expanded { usize::MAX } else { cfg.max_traceback_lines },
            image_rows: cfg.image_rows,
        }
    }
}

/// Columns a cell's left and right borders cost.
const BORDER_COLS: u16 = 2;
/// Narrowest inner width a cell is ever measured against, so a very narrow
/// terminal degrades rather than producing zero-width arithmetic.
const MIN_INNER_COLS: u16 = 4;

/// The measurements every notebook height question depends on.
///
/// The scroll math and the renderer MUST agree row-for-row (see the invariant
/// in CLAUDE.md), and they can only do that if they measure against the same
/// numbers.  These used to be re-derived by hand at six places — five in
/// `exec`, from `app.viewport_width`, and one in the renderer, from
/// `area.width` — two independent derivations of one quantity that agreed only
/// by coincidence, and disagreed outright below four columns, where only the
/// renderer clamped.
///
/// Build it once per frame from the content area the notebook is drawn into,
/// and pass it down.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    /// Terminal cell size in pixels, for sizing images (`None` = assume).
    pub cell_px: Option<(u16, u16)>,
    /// Columns available inside a cell's borders.
    pub inner_cols: u16,
    /// Whether code cells soft-wrap (`editor.word_wrap`).  Markdown cells wrap
    /// regardless — see [`cell_wraps`].
    pub word_wrap: bool,
}

impl Geometry {
    /// Derive from the width of the area the notebook is drawn into.
    pub fn new(content_width: u16, cell_px: Option<(u16, u16)>, word_wrap: bool) -> Self {
        Self {
            cell_px,
            inner_cols: content_width.saturating_sub(BORDER_COLS).max(MIN_INNER_COLS),
            word_wrap,
        }
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

/// The logical line owning visual row `vrow` of a (possibly wrapped) cell —
/// the inverse of [`cell_cursor_visual_row`]'s row accounting.  `width = None`
/// means no wrapping (visual row == logical line).  A `vrow` past the end
/// clamps to the last line.
pub(crate) fn cell_line_at_visual_row(
    rope: &ropey::Rope,
    width: Option<usize>,
    vrow: usize,
) -> usize {
    let last = rope.len_lines().saturating_sub(1);
    let Some(width) = width else { return vrow.min(last) };
    let text = rope.to_string();
    let lines: Vec<&str> = if text.is_empty() { vec![""] } else { text.split('\n').collect() };
    let mut acc = 0usize;
    for (idx, line) in lines.iter().enumerate() {
        acc += wrap_segments(line, width).len();
        if vrow < acc {
            return idx;
        }
    }
    lines.len().saturating_sub(1)
}

/// Total visual rows of a cell's source (`width = None` → logical line count).
pub(crate) fn cell_visual_rows(rope: &ropey::Rope, width: Option<usize>) -> usize {
    match width {
        Some(w) => wrapped_source_rows(rope, w) as usize,
        None => rope.len_lines().max(1),
    }
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
    area: Rect,
    state: &NotebookState,
    nb: &Notebook,
    active: &ActiveCellView<'_>,
    lsp_diagnostics: &std::collections::HashMap<String, Vec<Diagnostic>>,
    nb_config: &crate::config::NotebookConfig,
    cell_px: Option<(u16, u16)>,
    cache: &mut CellHighlightCache,
) -> (Vec<ImageRequest>, Option<(u16, u16)>) {
    if area.height == 0 {
        return (vec![], None);
    }
    // Built once, from the area we were actually given to draw into, and
    // passed down.  Everything that measures a cell measures against this.
    let geo = Geometry::new(area.width, cell_px, active.word_wrap);
    render_cells(frame, state, nb, active, lsp_diagnostics, area, nb_config, geo, cache)
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
    geo: Geometry,
    cache: &mut CellHighlightCache,
) -> (Vec<ImageRequest>, Option<(u16, u16)>) {
    let mut image_requests = Vec::new();
    let mut current_row = area.top();
    let mut focused_cell_screen_pos: Option<(u16, u16)> = None;

    // Rows of the first visible cell hidden above the viewport top — the
    // row-granular half of the scroll anchor (see NotebookState::scroll_offset).
    // Consumed by the first rendered cell; every later cell starts at 0.
    let mut skip = state.scroll_offset as u16;

    // Inner column width available for cell content (subtract left+right borders).
    let heights = nb_cell_heights(nb, state, active.rope, nb_config, geo);

    for (cell_idx, cell) in nb.cells.iter().enumerate() {
        if cell_idx < state.scroll_cell {
            continue;
        }
        if current_row >= area.bottom() {
            break;
        }

        let is_focused = cell_idx == state.focused_cell;
        let is_folded = state.is_cell_folded(cell_idx);
        let limits = OutputLimits::new(nb_config, state.is_output_expanded(cell_idx));
        let full_height = heights[cell_idx] as u16;

        // Clip: `clip_top` rows are scrolled off above the viewport; the
        // visible slice is further capped by the rows left before the bottom.
        let clip_top = skip.min(full_height);
        skip = 0;
        let visible = (full_height - clip_top).min(area.bottom() - current_row);

        if visible > 0 {
            let cell_rect = Rect {
                x: area.x,
                y: current_row,
                width: area.width,
                height: visible,
            };
            let cursor_screen = render_cell(
                frame, state, nb, cell, cell_idx, is_focused, is_folded, clip_top,
                full_height, cell_rect, active, lsp_diagnostics, &mut image_requests,
                cache, limits, geo,
            );
            if is_focused {
                focused_cell_screen_pos = cursor_screen;
            }
            current_row += visible;
        }

        current_row += 1; // blank gap row between cells
    }

    // Position the hardware cursor inside the focused cell.
    if let Some((cx, cy)) = focused_cell_screen_pos {
        frame.set_cursor_position((cx, cy));
    }

    (image_requests, focused_cell_screen_pos)
}

/// Render one cell, possibly clipped at the viewport edges: `clip_top` rows of
/// the cell are scrolled off above the screen, and `cell_rect.height` may stop
/// short of `full_height` when the cell runs past the bottom.  Clipped edges
/// lose their border line — the cell visibly continues past the screen edge.
/// Returns the cursor screen position when it falls inside the visible slice.
#[allow(clippy::too_many_arguments)]
fn render_cell(
    frame: &mut Frame,
    state: &NotebookState,
    nb: &Notebook,
    cell: &Cell,
    cell_idx: usize,
    is_focused: bool,
    is_folded: bool,
    clip_top: u16,
    full_height: u16,
    cell_rect: Rect,
    active: &ActiveCellView<'_>,
    lsp_diagnostics: &std::collections::HashMap<String, Vec<Diagnostic>>,
    image_requests: &mut Vec<ImageRequest>,
    cache: &mut CellHighlightCache,
    limits: OutputLimits,
    geo: Geometry,
) -> Option<(u16, u16)> {
    let th = crate::theme::active();
    // Border colour encodes cell execution state
    let border_color = cell_border_color(cell, state.executing_cell, cell_idx);

    let top_visible = clip_top == 0;
    let bottom_visible = clip_top + cell_rect.height == full_height;
    let mut borders = Borders::LEFT | Borders::RIGHT;
    if top_visible {
        borders |= Borders::TOP;
    }
    if bottom_visible {
        borders |= Borders::BOTTOM;
    }

    let mut block = Block::default()
        .borders(borders)
        .border_type(if is_focused { BorderType::Thick } else { BorderType::Rounded })
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(th.cell_bg));

    // Cell title sits inside the top border line (absent while scrolled off).
    if top_visible {
        let count_str = cell.execution_count
            .map(|n| format!("[{n}]"))
            .unwrap_or_else(|| "[ ]".to_string());
        let type_label = cell_type_label(cell, &nb.metadata.kernel_language);
        let title = format!(" {count_str} {type_label} ");
        let title_style = if is_focused {
            Style::default().fg(th.fg()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.dim)
        };
        block = block.title(ratatui::text::Span::styled(title, title_style));
    }

    let inner = block.inner(cell_rect);
    frame.render_widget(block, cell_rect);
    if inner.height == 0 {
        return None;
    }

    // Content rows hidden above the viewport: everything above minus the
    // (1-row) top border.
    let content_skip = clip_top.saturating_sub(1) as usize;

    if is_folded {
        if content_skip == 0 {
            // For the focused cell, use the live rope so unsaved edits are shown.
            let rope_for_summary = if is_focused { active.rope } else { &cell.source };
            render_folded_cell_summary_rope(frame, rope_for_summary, &cell.outputs, inner);
        }
        return None;
    }

    render_cell_content(
        frame, nb, cell, cell_idx, is_focused, inner, active, lsp_diagnostics,
        image_requests, cache, limits, geo, content_skip,
    )
}

/// Render source lines and outputs inside a cell's bordered inner area.
/// `skip_rows` content rows (source + divider + output, in visual rows) are
/// scrolled off above the viewport and consumed without drawing.
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
    limits: OutputLimits,
    geo: Geometry,
    skip_rows: usize,
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
    // `j`/`k` passing through the cell keeps a visible cursor.  While the
    // cursor traverses the output block (`active.output_row`) the source
    // cursor is hidden — the block cursor is drawn on the output row instead.
    let (cursor_char_idx, sel_range) = if is_focused && active.output_row.is_none() {
        let lo = active.cursor.min(active.sel_anchor);
        let hi = active.cursor.max(active.sel_anchor);
        (Some(active.cursor), (lo, hi))
    } else {
        (None, (0usize, 0usize))
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

    // Visual rows still to consume before drawing (the clip handed down by
    // `render_cell` — rows scrolled off above the viewport).
    let mut skip_rows = skip_rows;
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
        // Divider row (not part of the output-row index space).
        if skip_rows > 0 {
            skip_rows -= 1;
        } else if current_row < area.bottom() {
            frame.render_widget(
                SingleLineWidget {
                    text: " \u{2500}\u{2500} output \u{2500}\u{2500}".to_string(),
                    style: Style::default().fg(crate::theme::active().dim),
                },
                single_row(area, current_row),
            );
            current_row += 1;
        }

        // Absolute char positions of the output cursor/selection, addressed
        // into the block's virtual rope (`output_virtual_rope`) exactly like
        // a source cursor addresses the buffer rope — see
        // `OutputCtx::advance`, which intersects these against each row as
        // it's drawn.
        let (out_cursor_char, out_sel) = if is_focused {
            active.output_row.map(|row| {
                let vrope = output_virtual_rope(cell, limits, geo);
                let to_char = |r: usize, c: usize| {
                    let r = r.min(vrope.len_lines().saturating_sub(1));
                    let line_start = vrope.line_to_char(r);
                    let line = vrope.line(r);
                    let n = line.len_chars();
                    let content_len = if n > 0 && line.char(n - 1) == '\n' { n - 1 } else { n };
                    line_start + c.min(content_len)
                };
                let cursor_char = to_char(row, active.output_col);
                let sel = active
                    .output_anchor
                    .map(|(ar, ac)| {
                        let a = to_char(ar, ac);
                        (a.min(cursor_char), a.max(cursor_char))
                    })
                    .filter(|(lo, hi)| lo != hi);
                (cursor_char, sel)
            })
            .map_or((None, None), |(c, s)| (Some(c), s))
        } else {
            (None, None)
        };

        let th = crate::theme::active();
        let mut out_ctx = OutputCtx {
            skip: skip_rows,
            char_pos: 0,
            cursor_char: out_cursor_char,
            sel: out_sel,
            cursor_style: crate::theme::cursor_style(active.mode),
            selection_style: Style::default()
                .bg(th.cell_selection_bg)
                .fg(th.selection_fg.unwrap_or_else(|| th.fg())),
            cursor_pos: None,
            limits,
        };
        for output in &cell.outputs {
            if current_row >= area.bottom() {
                break;
            }
            render_output(
                frame, output, area, &mut current_row, image_requests,
                geo, &mut out_ctx,
            );
        }
        if out_ctx.cursor_pos.is_some() {
            cursor_screen = out_ctx.cursor_pos;
        }
    }

    cursor_screen
}

/// Shared bookkeeping while rendering a cell's output block: the scroll clip
/// still to consume, the running output-row index (0 = first row after the
/// divider — the index space of `NotebookState::output_row`) and matching
/// char offset into the block's virtual rope (see `output_virtual_rope`),
/// and the output cursor/selection when the focused cell's cursor traverses
/// its outputs.
struct OutputCtx {
    /// Visual output rows still hidden above the viewport.
    skip: usize,
    /// Absolute char offset (into the block's virtual rope) of the next row's
    /// first character.
    char_pos: usize,
    /// Absolute char index of the output cursor (focused cell only), in the
    /// same virtual-rope address space as `char_pos`.
    cursor_char: Option<usize>,
    /// Absolute char range `(lo, hi)` of an active output-text selection
    /// (half-open), or `None` when the cursor is a point.
    sel: Option<(usize, usize)>,
    cursor_style: Style,
    selection_style: Style,
    /// Screen position of the output cursor once its row has been drawn.
    cursor_pos: Option<(u16, u16)>,
    /// Truncation caps for this cell — must be the same ones the height model
    /// used, or the cell's border won't enclose its output.
    limits: OutputLimits,
}

/// Where one output row lands relative to the viewport clip and the active
/// cursor/selection, computed by [`OutputCtx::advance`].
struct RowSlot {
    /// False when the row is scrolled off above the viewport (still
    /// accounted for in `char_pos`/`row_idx`, just not drawn).
    visible: bool,
    /// Selected char sub-range within this row's text (row-local offsets),
    /// if the active selection overlaps it.
    sel: Option<(usize, usize)>,
    /// Row-local column the cursor sits on, if this is the cursor's row.
    /// May equal the row's char length (the "end of row" position, like the
    /// EOL cursor cell on a source line).
    cursor_col: Option<usize>,
}

impl OutputCtx {
    /// Account for one output row of content length `len` (chars, no UI
    /// padding): advance the row/char bookkeeping unconditionally (so later
    /// rows stay correctly addressed even while this one is clipped), and
    /// report its visibility plus any cursor/selection overlap.
    fn advance(&mut self, len: usize) -> RowSlot {
        let row_start = self.char_pos;
        let row_end = row_start + len;
        self.char_pos = row_end + 1; // +1 for the virtual rope's line-joining '\n'

        let visible = if self.skip > 0 {
            self.skip -= 1;
            false
        } else {
            true
        };
        let sel = self.sel.and_then(|(lo, hi)| {
            let s = lo.max(row_start);
            let e = hi.min(row_end);
            (s < e).then(|| (s - row_start, e - row_start))
        });
        let cursor_col = self
            .cursor_char
            .filter(|&c| c >= row_start && c <= row_end)
            .map(|c| c - row_start);
        RowSlot { visible, sel, cursor_col }
    }

    /// Paint the cursor at row-local column `col` (clamped to the row's
    /// drawn width) and remember its screen position for the hardware
    /// cursor. `content_x` is the screen column of the row's first content
    /// char (after any left padding/gutter).
    fn place_cursor(&mut self, frame: &mut Frame, content_x: u16, row_right: u16, col: u16, y: u16) {
        let x = (content_x + col).min(row_right.saturating_sub(1));
        let buf = frame.buffer_mut();
        buf[(x, y)].set_style(self.cursor_style);
        self.cursor_pos = Some((x, y));
    }
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
    cell_px: Option<(u16, u16)>,
    max_image_rows: u16,
) -> u16 {
    let (cell_h, cell_w) = cell_px.unwrap_or((18, 9));

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
    limits: OutputLimits,
    geo: Geometry,
) -> u16 {
    let source_lines = if cell_wraps(cell, geo.word_wrap) {
        wrapped_source_rows(source, cell_text_width(geo.inner_cols)).max(1)
    } else {
        source.len_lines().max(1) as u16
    };
    let out_rows = cell_output_rows(cell, limits, geo);
    let output_h = if out_rows > 0 { 1 + out_rows as u16 } else { 0 }; // 1 = divider row
    2 + source_lines + output_h // 2 = top border + bottom border
}

/// Display height of cell `idx` exactly as the notebook renderer draws it —
/// folded cells collapse to 3 rows, everything else via [`cell_display_height`].
/// The single height model shared by the renderer and the seamless-scroll math
/// in `exec::update_scroll`; they must agree row-for-row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nb_cell_height(
    cell: &Cell,
    folded: bool,
    source: &ropey::Rope,
    limits: OutputLimits,
    geo: Geometry,
) -> usize {
    if folded {
        3 // top border + 1 summary line + bottom border
    } else {
        cell_display_height(source, cell, limits, geo)
            as usize
    }
}

/// Per-cell display heights for the whole notebook, exactly as the renderer
/// draws them — a folded cell always collapses to its 3-row summary
/// regardless of focus, and the focused cell is measured against the live
/// buffer rope (its unsaved edits are ahead of `cell.source`). This is the
/// single source of truth shared by `render_cells` and the scroll math
/// (`exec::scroll::nb_layout`) so the two can never disagree row-for-row.
pub(crate) fn nb_cell_heights(
    nb: &Notebook,
    state: &NotebookState,
    active_rope: &ropey::Rope,
    nb_config: &crate::config::NotebookConfig,
    geo: Geometry,
) -> Vec<usize> {
    nb.cells
        .iter()
        .enumerate()
        .map(|(idx, cell)| {
            let is_focused = idx == state.focused_cell;
            let folded = state.is_cell_folded(idx);
            let source = if is_focused { active_rope } else { &cell.source };
            let limits = OutputLimits::new(nb_config, state.is_output_expanded(idx));
            nb_cell_height(cell, folded, source, limits, geo)
        })
        .collect()
}

/// Total visual rows of a cell's output block, *excluding* the `── output ──`
/// divider row (0 for markdown/raw cells or when there are no outputs).
/// Output row indices — `NotebookState::output_row`, the renderer's output
/// cursor — count within this range.
pub(crate) fn cell_output_rows(
    cell: &Cell,
    limits: OutputLimits,
    geo: Geometry,
) -> usize {
    if cell.cell_type != CellType::Code {
        return 0;
    }
    cell.outputs
        .iter()
        .map(|o| single_output_height_count(o, limits, geo) as usize)
        .sum()
}

/// Width one output text row has for its content: the output block draws
/// after the same 2-char pad as source lines.
///
/// Output text **always** wraps to this width, regardless of the
/// `editor.word_wrap` toggle (which governs cell *source* only — see
/// [`cell_wraps`]). The output block has no horizontal scroll and no cursor
/// column beyond the rendered rows, so an unwrapped long line would put its
/// tail permanently out of reach — not merely clipped, as in a source cell.
pub(crate) fn output_text_width(available_cols: u16) -> usize {
    cell_text_width(available_cols)
}

/// Rows shown for a truncated, word-wrapped line list: the first `max`
/// *logical* lines (so `max_output_lines` still counts printed lines, not
/// screen rows), each wrapped to `width`, plus one "… (N more lines)"
/// indicator row.  Shared by the height model, [`output_rows_content`] and
/// the renderer so they cannot drift.
fn truncated_rows<S: AsRef<str>>(lines: &[S], max: usize, width: usize) -> usize {
    let shown = lines.len().min(max);
    let body: usize = lines[..shown]
        .iter()
        .map(|l| wrap_segments(l.as_ref(), width).len())
        .sum();
    body + usize::from(lines.len() > max)
}

/// Columns reserved to the left of an image so the output cursor always has
/// a spot that isn't covered by the image's own pixel data (see
/// [`OutputRowKind::Image`] and [`render_mime_data`]). Matches the 2-char
/// pad every text output row already draws before its content, so the
/// cursor gutter lines up whether the row underneath it is text or image.
const IMAGE_GUTTER: u16 = 2;

fn image_available_cols(available_cols: u16) -> u16 {
    available_cols.saturating_sub(IMAGE_GUTTER)
}

/// Row count for `data`'s embedded image (`None` when it has none), scaled
/// from the PNG's aspect ratio and capped at `limits.image_rows`. The single
/// place image sizing is computed — shared by the height model
/// (`single_output_height_count`/`output_rows_content`) and the renderer
/// (`render_mime_data`) so they can never disagree on row count.
fn mime_image_rows(
    data: &MimeData,
    limits: OutputLimits,
    geo: Geometry,
) -> Option<u16> {
    let png = data.image_png.as_ref()?;
    let avail = image_available_cols(geo.inner_cols);
    Some(match png_pixel_size(png) {
        Some((pw, ph)) => compute_image_rows(pw, ph, avail, geo.cell_px, limits.image_rows),
        None => limits.image_rows,
    })
}

fn single_output_height_count(
    output: &Output,
    limits: OutputLimits,
    geo: Geometry,
) -> u16 {
    let width = output_text_width(geo.inner_cols);
    match output {
        Output::Stream { text, .. } => {
            let lines: Vec<&str> = text.lines().collect();
            truncated_rows(&lines, limits.max_lines, width) as u16
        }
        Output::DisplayData { data } | Output::ExecuteResult { data, .. } => {
            mime_image_rows(data, limits, geo).unwrap_or_else(|| {
                data.text_plain
                    .as_deref()
                    .map(|t| {
                        let lines: Vec<&str> = t.lines().collect();
                        truncated_rows(&lines, limits.max_lines, width)
                    })
                    .unwrap_or(0) as u16
            })
        }
        Output::Error { ename, evalue, traceback, .. } => {
            let headline = format!("{ename}: {evalue}");
            (wrap_segments(&headline, width).len()
                + truncated_rows(traceback, limits.max_traceback, width)) as u16
        }
    }
}

/// The navigable content of a cell's output block, one entry per row, in the
/// exact row-index space of `NotebookState::output_row` — row `i` here is
/// what a cursor at `output_row == i` sits on. Each string is the row's
/// content with no leading UI padding (that's applied at draw time, like
/// `render_source_line`'s pad).
///
/// This is the read-only counterpart of a cell's source rope: joining every
/// row with `'\n'` (see [`output_virtual_rope`]) gives a rope that
/// `motion::*` can navigate exactly like the plain buffer, so output-text
/// motions/selection are implemented by reusing that module instead of
/// hand-rolling char/word boundaries a second time. Exactly as many rows as
/// [`cell_output_rows`] counts — both derive image row counts via the same
/// [`image_available_cols`] + `compute_image_rows`, and text truncation via
/// the same [`truncated_rows`] logic, so they cannot drift apart.
pub(crate) fn output_rows_content(
    cell: &Cell,
    limits: OutputLimits,
    geo: Geometry,
) -> Vec<String> {
    if cell.cell_type != CellType::Code {
        return Vec::new();
    }

    /// Push a (possibly truncated) line list, each line word-wrapped to
    /// `width` — shared by stream output, execute-result/display-data text
    /// and tracebacks, and mirroring [`truncated_rows`] row for row.
    fn push_truncated_lines<S: AsRef<str>>(
        rows: &mut Vec<String>,
        lines: &[S],
        max: usize,
        width: usize,
    ) {
        let to_show = lines.len().min(max);
        for line in &lines[..to_show] {
            rows.extend(wrap_segments(line.as_ref(), width).into_iter().map(|(_, s)| s.to_string()));
        }
        if lines.len() > max {
            let extra = lines.len() - max;
            rows.push(format!("... ({extra} more lines — zO to expand)"));
        }
    }

    let width = output_text_width(geo.inner_cols);
    let mut rows = Vec::new();
    for output in &cell.outputs {
        match output {
            Output::Stream { text, .. } => {
                let lines: Vec<&str> = text.lines().collect();
                push_truncated_lines(&mut rows, &lines, limits.max_lines, width);
            }
            Output::DisplayData { data } | Output::ExecuteResult { data, .. } => {
                if let Some(n) = mime_image_rows(data, limits, geo) {
                    for i in 0..n {
                        rows.push(if i == 0 { "[image]".to_string() } else { String::new() });
                    }
                } else if let Some(t) = &data.text_plain {
                    let lines: Vec<&str> = t.lines().collect();
                    push_truncated_lines(&mut rows, &lines, limits.max_lines, width);
                }
            }
            Output::Error { ename, evalue, traceback, .. } => {
                let headline = format!("{ename}: {evalue}");
                rows.extend(wrap_segments(&headline, width).into_iter().map(|(_, s)| s.to_string()));
                push_truncated_lines(&mut rows, traceback, limits.max_traceback, width);
            }
        }
    }
    rows
}

/// A read-only rope over a cell's whole output block: every row from
/// [`output_rows_content`] joined by `'\n'`, one rope line per output row.
/// `rope.char_to_line(pos)` recovers `output_row`, `line_to_char` its start —
/// letting `motion::*` drive output-text navigation/selection exactly like
/// the plain buffer (see `exec::output_motion`).
pub(crate) fn output_virtual_rope(
    cell: &Cell,
    limits: OutputLimits,
    geo: Geometry,
) -> ropey::Rope {
    let rows = output_rows_content(cell, limits, geo);
    ropey::Rope::from_str(&rows.join("\n"))
}

/// The navigable error frame the output cursor sits on, if any.
///
/// `output_row` is an index into the cell's whole output block (the index space
/// of [`NotebookState::output_row`]). We walk the outputs — sizing each exactly
/// as the renderer does, so the mapping matches what is drawn — to find the one
/// under the cursor; within an `Error` output, row 0 is the `ename: evalue`
/// headline and row `1 + i` is `traceback[i]`, so a frame with `tb_index == i`
/// is the link on that row.
pub(crate) fn error_frame_at_output_row(
    cell: &Cell,
    output_row: usize,
    limits: OutputLimits,
    geo: Geometry,
) -> Option<&crate::notebook::ErrorFrame> {
    if cell.cell_type != CellType::Code {
        return None;
    }
    let mut base = 0usize;
    for output in &cell.outputs {
        let h = single_output_height_count(output, limits, geo) as usize;
        if output_row < base + h {
            if let Output::Error { ename, evalue, traceback, frames } = output {
                // Walk the block's wrapped rows: the headline first, then each
                // traceback line's own (possibly multi-row) span. A frame's link
                // covers every row its line wrapped onto.
                let width = output_text_width(geo.inner_cols);
                let headline = format!("{ename}: {evalue}");
                let mut row = base + wrap_segments(&headline, width).len();
                for (i, tb_line) in traceback.iter().take(limits.max_traceback).enumerate() {
                    row += wrap_segments(tb_line, width).len();
                    if output_row < row {
                        return frames.iter().find(|f| f.tb_index == i);
                    }
                }
            }
            return None;
        }
        base += h;
    }
    None
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

/// Draw one output-block content row: a 2-col left pad (matching the source
/// line's own pad), then `text` char-by-char with `base_style`, `link_range`
/// (if any) recoloured + underlined underneath the selection, and `sel` (a
/// row-local half-open char range) blended in via `selection_style`. A
/// selected position past the last real char (the row's virtual "end of
/// line") also gets one highlighted blank cell, mirroring a source line's
/// EOL cursor cell.
#[allow(clippy::too_many_arguments)]
fn draw_output_content_row(
    frame: &mut Frame,
    row: Rect,
    text: &str,
    base_style: Style,
    link_range: Option<(usize, usize)>,
    sel: Option<(usize, usize)>,
    selection_style: Style,
) {
    if row.width == 0 {
        return;
    }
    let pad_len = 2u16.min(row.width);
    frame.render_widget(
        SingleLineWidget { text: "  ".to_string(), style: Style::default() },
        Rect { x: row.x, y: row.y, width: pad_len, height: 1 },
    );
    let content_x = row.x + pad_len;
    if content_x >= row.right() {
        return;
    }

    let th = crate::theme::active();
    let link_style = Style::default().fg(th.info).add_modifier(Modifier::UNDERLINED);

    let buf = frame.buffer_mut();
    let mut x = content_x;
    let mut len = 0usize;
    for (i, c) in text.chars().enumerate() {
        len = i + 1;
        if x >= row.right() {
            break;
        }
        let mut style = base_style;
        if let Some((ls, le)) = link_range {
            if i >= ls && i < le {
                style = link_style;
            }
        }
        if let Some((sl, sh)) = sel {
            if i >= sl && i < sh {
                style = style.patch(selection_style);
            }
        }
        buf[(x, row.y)].set_char(c).set_style(style);
        x += 1;
    }
    if let Some((sl, sh)) = sel {
        if x < row.right() && len >= sl && len < sh {
            buf[(x, row.y)].set_char(' ').set_style(base_style.patch(selection_style));
        }
    }
}

/// Draw one output text row, honouring the scroll clip and output
/// cursor/selection in `octx`.  A skipped (scrolled-off) row is accounted for
/// but not drawn and does not advance `current_row`.  Returns false when the
/// viewport bottom is reached (caller should stop).
fn draw_output_row(
    frame: &mut Frame,
    area: Rect,
    current_row: &mut u16,
    octx: &mut OutputCtx,
    text: &str,
    style: Style,
) -> bool {
    let slot = octx.advance(text.chars().count());
    if !slot.visible {
        return true; // scrolled off above — consumed, keep going
    }
    if *current_row >= area.bottom() {
        return false;
    }
    let row = single_row(area, *current_row);
    draw_output_content_row(frame, row, text, style, None, slot.sel, octx.selection_style);
    if let Some(col) = slot.cursor_col {
        let content_x = row.x + 2u16.min(row.width);
        octx.place_cursor(frame, content_x, row.right(), col as u16, row.y);
    }
    *current_row += 1;
    true
}

/// Draw one traceback row, honouring the scroll clip and output
/// cursor/selection like [`draw_output_row`]. A `link` row (a navigable
/// `File …` frame) has its **visible text span only** recoloured and
/// underlined — so it reads like a hyperlink instead of a full-width bar
/// (the underline must not bleed across the row's trailing padding).
fn draw_traceback_row(
    frame: &mut Frame,
    area: Rect,
    current_row: &mut u16,
    octx: &mut OutputCtx,
    text: &str,
    link: bool,
) -> bool {
    let slot = octx.advance(text.chars().count());
    if !slot.visible {
        return true;
    }
    if *current_row >= area.bottom() {
        return false;
    }
    let row = single_row(area, *current_row);
    let base = Style::default().fg(crate::theme::active().dim);
    let link_range = link.then(|| {
        let chars: Vec<char> = text.chars().collect();
        let start = chars.iter().take_while(|c| c.is_whitespace()).count();
        let end = chars.iter().rposition(|c| !c.is_whitespace()).map_or(0, |i| i + 1);
        (start, end)
    });
    draw_output_content_row(frame, row, text, base, link_range, slot.sel, octx.selection_style);
    if let Some(col) = slot.cursor_col {
        let content_x = row.x + 2u16.min(row.width);
        octx.place_cursor(frame, content_x, row.right(), col as u16, row.y);
    }
    *current_row += 1;
    true
}

#[allow(clippy::too_many_arguments)]
fn render_output(
    frame: &mut Frame,
    output: &Output,
    area: Rect,
    current_row: &mut u16,
    image_requests: &mut Vec<ImageRequest>,
    geo: Geometry,
    octx: &mut OutputCtx,
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
            let max_lines = octx.limits.max_lines;
            let to_show = lines.len().min(max_lines);
            let width = output_text_width(area.width);
            for line in &lines[..to_show] {
                for (_, seg) in wrap_segments(line, width) {
                    if !draw_output_row(frame, area, current_row, octx, seg, style) {
                        return;
                    }
                }
            }
            draw_truncation_row(frame, area, current_row, octx, lines.len(), max_lines);
        }

        Output::DisplayData { data } | Output::ExecuteResult { data, .. } => {
            render_mime_data(frame, data, area, current_row, image_requests, geo, octx);
        }

        Output::Error { ename, evalue, traceback, frames } => {
            let width = output_text_width(area.width);
            let headline = format!("{ename}: {evalue}");
            for (_, seg) in wrap_segments(&headline, width) {
                if !draw_output_row(frame, area, current_row, octx, seg, Style::default().fg(th.error)) {
                    return;
                }
            }
            let max_tb = octx.limits.max_traceback;
            for (i, tb_line) in traceback.iter().take(max_tb).enumerate() {
                // Navigable frames (`File "Cell [N]", line L`) render like a
                // link — Enter on any row this frame wrapped onto jumps the
                // cursor to that source line.
                let is_link = frames.iter().any(|f| f.tb_index == i);
                for (_, seg) in wrap_segments(tb_line, width) {
                    if !draw_traceback_row(frame, area, current_row, octx, seg, is_link) {
                        return;
                    }
                }
            }
            draw_truncation_row(frame, area, current_row, octx, traceback.len(), max_tb);
        }
    }
}

/// Draw the "… (N more lines)" row that stands in for a truncated tail, and
/// point at the command that reveals it.  A no-op when nothing was cut —
/// exactly mirroring [`truncated_rows`], which reserves the row in the height
/// model on the same condition.
fn draw_truncation_row(
    frame: &mut Frame,
    area: Rect,
    current_row: &mut u16,
    octx: &mut OutputCtx,
    total: usize,
    max: usize,
) {
    if total <= max {
        return;
    }
    let extra = total - max;
    let text = format!("... ({extra} more lines — zO to expand)");
    draw_output_row(frame, area, current_row, octx, &text, Style::default().fg(crate::theme::active().dim));
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
fn estimated_image_cols(png_w: u32, png_h: u32, rows: u16, cell_px: Option<(u16, u16)>) -> u16 {
    let (cell_h, cell_w) = cell_px.unwrap_or((18, 9));
    let cols = (rows as u64) * (cell_h as u64) * (png_w as u64)
        / ((png_h as u64) * (cell_w as u64));
    cols.clamp(4, 512) as u16
}

#[allow(clippy::too_many_arguments)]
fn render_mime_data(
    frame: &mut Frame,
    data: &MimeData,
    area: Rect,
    current_row: &mut u16,
    image_requests: &mut Vec<ImageRequest>,
    geo: Geometry,
    octx: &mut OutputCtx,
) {
    if let Some(png) = &data.image_png {
        // Compute rows from image aspect ratio so the display height scales with
        // figsize.  image_rows acts as a cap, not a fixed height. Uses the same
        // `mime_image_rows` the height model calls, so the two never disagree on
        // row count.
        let natural_rows = mime_image_rows(data, octx.limits, geo)
            .expect("data.image_png just matched Some above");

        // Walk every image row through the same skip/cursor/selection
        // bookkeeping as text rows (row 0 carries the "[image]" placeholder
        // text from `output_rows_content`, the rest are empty), so the output
        // cursor/selection index stays aligned with a following output.
        let slots: Vec<RowSlot> = (0..natural_rows)
            .map(|i| octx.advance(if i == 0 { "[image]".chars().count() } else { 0 }))
            .collect();
        let Some(first_visible) = slots.iter().position(|s| s.visible) else { return };
        let skip_top = first_visible as u16;
        let visible_slots = &slots[first_visible..];

        let remaining_after_skip = natural_rows - skip_top;
        let available = area.bottom().saturating_sub(*current_row);
        let shown = remaining_after_skip.min(available);
        if shown > 0 {
            let image_top = *current_row;
            // The image itself starts past a reserved left gutter — never
            // covered by Kitty's raster — so the output cursor always has a
            // well-defined spot to render on, instead of disappearing under
            // arbitrary image pixels.
            let image_col = area.x + IMAGE_GUTTER;
            let image_width = area.width.saturating_sub(IMAGE_GUTTER);

            // Placeholder width = the same column count Kitty will use so the
            // dark background matches the rendered image footprint exactly.
            let placeholder_cols = if let Some((pw, ph)) = png_pixel_size(png) {
                estimated_image_cols(pw, ph, natural_rows, geo.cell_px).min(image_width)
            } else {
                image_width
            };

            // Draw a dark placeholder block; Kitty will paint over it.
            let th = crate::theme::active();
            for r in 0..shown {
                let row_area = Rect { x: image_col, y: image_top + r, width: placeholder_cols, height: 1 };
                let label = if skip_top == 0 && r == 0 { " ▸ image ".to_string() } else { String::new() };
                frame.render_widget(
                    SingleLineWidget { text: label, style: Style::default().bg(th.output_bg).fg(th.dim) },
                    row_area,
                );
                // A selection spanning this image row tints its gutter too —
                // there's no real text under the image to recolor instead.
                if visible_slots.get(r as usize).is_some_and(|s| s.sel.is_some()) {
                    let buf = frame.buffer_mut();
                    for gx in area.x..image_col.min(area.right()) {
                        buf[(gx, image_top + r)].set_style(octx.selection_style);
                    }
                }
            }

            // Vertical source crop (in image pixels) when clipped at either edge.
            let crop = png_pixel_size(png).and_then(|(_, ph)| {
                if skip_top == 0 && shown == natural_rows {
                    None // whole image visible
                } else {
                    let y = (skip_top as u32 * ph) / natural_rows as u32;
                    let h = (shown as u32 * ph) / natural_rows as u32;
                    Some((y, h.max(1)))
                }
            });

            image_requests.push(ImageRequest {
                col: image_col,
                row: image_top,
                rows: shown,
                cols: placeholder_cols,
                crop,
                png_data: png.clone(),
            });

            // The output cursor never sits on top of image pixels — it's
            // drawn in the reserved gutter instead, at whichever visible row
            // it's on.
            for (i, slot) in visible_slots.iter().take(shown as usize).enumerate() {
                if slot.cursor_col.is_some() {
                    let y = image_top + i as u16;
                    octx.place_cursor(frame, area.x, image_col, 0, y);
                }
            }
            *current_row += shown;
        }
    } else if let Some(text) = &data.text_plain {
        let lines: Vec<&str> = text.lines().collect();
        let max_lines = octx.limits.max_lines;
        let to_show = lines.len().min(max_lines);
        let info = Style::default().fg(crate::theme::active().info);
        let width = output_text_width(area.width);
        for line in &lines[..to_show] {
            for (_, seg) in wrap_segments(line, width) {
                if !draw_output_row(frame, area, current_row, octx, seg, info) {
                    return;
                }
            }
        }
        draw_truncation_row(frame, area, current_row, octx, lines.len(), max_lines);
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

    /// Geometry for a test measured against `inner_cols` columns inside the
    /// cell borders, with no known terminal pixel size.
    fn geo_of(inner_cols: u16, word_wrap: bool) -> Geometry {
        Geometry { cell_px: None, inner_cols, word_wrap }
    }

    /// The scroll math and the renderer must measure a cell against the same
    /// width or their row counts drift.  They used to derive it separately —
    /// `app.viewport_width - 2` in `exec`, `area.width - 2` in the renderer —
    /// and only the renderer clamped, so below four columns they disagreed
    /// outright.  One constructor, one answer.
    #[test]
    fn geometry_subtracts_the_borders_and_never_goes_below_the_floor() {
        assert_eq!(Geometry::new(80, None, false).inner_cols, 80 - BORDER_COLS);
        // Narrow terminals clamp rather than producing zero-width arithmetic.
        for w in 0..=BORDER_COLS + MIN_INNER_COLS {
            let inner = Geometry::new(w, None, false).inner_cols;
            assert!(inner >= MIN_INNER_COLS, "width {w} gave inner_cols {inner}");
        }
    }

    /// The area `app::draw_frame` hands the notebook renderer: the frame minus
    /// the two chrome rows.  Tests draw into a full frame, so they have to make
    /// the same split the real caller does.
    fn content_area(f: &Frame) -> Rect {
        crate::view::Chrome::split(f.area())
            .expect("test terminal is tall enough for the chrome")
            .content
    }

    use ropey::Rope;

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
        let cfg = crate::config::NotebookConfig::default();
        let limits = OutputLimits::new(&cfg, false);
        let expected_rows = wrapped_source_rows(&Rope::from_str(&long), cell_text_width(inner_cols));
        assert!(expected_rows > 1, "long prose must wrap to several rows");

        // Markdown wraps in both the rendered view and the source view —
        // word_wrap toggle irrelevant.
        let md = make(CellType::Markdown, true);
        assert_eq!(cell_display_height(&md.source, &md, limits, geo_of(inner_cols, false)), 2 + expected_rows);
        let md_src = make(CellType::Markdown, false);
        assert_eq!(cell_display_height(&md_src.source, &md_src, limits, geo_of(inner_cols, false)), 2 + expected_rows);

        // Code cells follow the word_wrap toggle.
        let code = make(CellType::Code, false);
        assert_eq!(cell_display_height(&code.source, &code, limits, geo_of(inner_cols, false)), 2 + 1);
        assert_eq!(cell_display_height(&code.source, &code, limits, geo_of(inner_cols, true)), 2 + expected_rows);
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
        };
        let state = NotebookState::new();
        let rope = nb.cells[0].source.clone();
        let mode = Mode::Normal;
        let active = ActiveCellView {
            rope: &rope,
            cursor: 2, // on the 'H' of "# Heading"
            sel_anchor: 2,
            output_row: None,
            output_col: 0,
            output_anchor: None,
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
                    content_area(f),
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

    /// A folded cell always collapses to its 3-row summary, even when it is
    /// also the focused cell — `render_cell`'s folded branch draws the
    /// compact summary unconditionally (see its comment "Folded cells always
    /// get the compact height regardless of focus"). Regression: `nb_layout`
    /// (the scroll math's copy of this computation) used to special-case the
    /// focused cell as never folded, so scrolling onto a folded, focused cell
    /// left the scroll anchor believing it was still full height — drifting
    /// from what the renderer actually drew. `nb_cell_heights` is now the one
    /// function both the renderer and the scroll math call, so they can't
    /// disagree again.
    #[test]
    fn nb_cell_heights_ignores_focus_when_folded() {
        let make = |text: &str| Cell {
            id: crate::notebook::new_cell_id(),
            cell_type: CellType::Code,
            source: Rope::from_str(text),
            outputs: vec![],
            execution_count: None,
            rendered: false,
        };
        let nb = Notebook {
            path: std::path::PathBuf::from("/tmp/fold-test.ipynb"),
            metadata: crate::notebook::NotebookMeta { kernel_language: "python".into() },
            cells: vec![make("line one\nline two\nline three"), make("x = 1")],
            modified: false,
        };
        let mut state = NotebookState::new();
        state.focused_cell = 0;
        state.folded_cells.insert(0);

        let active_rope = nb.cells[0].source.clone();
        let heights = nb_cell_heights(
            &nb, &state, &active_rope, &crate::config::NotebookConfig::default(), geo_of(40, false),
        );
        assert_eq!(heights[0], 3, "folded focused cell must collapse to the 3-row summary");
    }

    /// The height model and the renderer must agree row-for-row: the cell's
    /// bottom border has to land exactly on the last row `cell_display_height`
    /// claims.  Regression: a truncated error traceback reserved a
    /// "… N more lines" row in the height model that the renderer never drew,
    /// so tall error cells were one row short of their own border — and the
    /// scroll math (which uses the same model) drifted with them.
    #[test]
    fn output_block_height_matches_what_is_drawn() {
        use crate::notebook::Output;
        let cfg = crate::config::NotebookConfig::default();

        // Both truncating output kinds, exercised together and separately.
        let stream = Output::Stream {
            name: "stdout".into(),
            text: (0..cfg.max_output_lines * 2).map(|i| format!("o{i}\n")).collect(),
        };
        let error = Output::Error {
            frames: vec![],
            ename: "ValueError".into(),
            evalue: "boom".into(),
            traceback: (0..cfg.max_traceback_lines * 2).map(|i| format!("tb{i}")).collect(),
        };

        for (name, outputs, expanded) in [
            ("stream", vec![stream.clone()], false),
            ("error", vec![error.clone()], false),
            ("both", vec![stream.clone(), error.clone()], false),
            ("both-expanded", vec![stream, error], true),
        ] {
            let cell = Cell {
                id: "h".into(),
                cell_type: CellType::Code,
                source: Rope::from_str("a\nb"),
                outputs,
                execution_count: Some(1),
                rendered: false,
            };
            let nb = Notebook {
                path: std::path::PathBuf::from("/tmp/height-test.ipynb"),
                metadata: crate::notebook::NotebookMeta { kernel_language: "python".into() },
                cells: vec![cell],
                modified: false,
            };
            let mut state = NotebookState::new();
            if expanded {
                state.toggle_output_expand(0);
            }
            let limits = OutputLimits::new(&cfg, expanded);
            let rope = nb.cells[0].source.clone();
            let mode = Mode::Normal;
            let active = ActiveCellView {
                rope: &rope,
                cursor: 0,
                sel_anchor: 0,
                output_row: None,
                output_col: 0,
                output_anchor: None,
                mode: &mode,
                jump_labels: &[],
                jump_typed: "",
                word_wrap: false,
            };

            // Terminal tall enough for the whole cell plus the 2 status rows.
            let width = 80u16;
            let expected =
                cell_display_height(&rope, &nb.cells[0], limits, geo_of(width - 2, false));
            let backend = ratatui::backend::TestBackend::new(width, expected + 4);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|f| {
                    render(
                        f, content_area(f), &state, &nb, &active,
                        &std::collections::HashMap::new(),
                        &cfg, None, &mut CellHighlightCache::default(),
                    );
                })
                .unwrap();

            // The block always paints its border at the bottom of the rect it
            // was given, so a height/content mismatch shows up as a blank row
            // *inside* the cell.  Assert the content reaches the last interior
            // row (row `expected - 2`: bottom border is `expected - 1`).
            let buf = terminal.backend().buffer();
            let row_is_blank = |y: u16| {
                (1..width - 1).all(|x| buf[(x, y)].symbol() == " ")
            };
            let last_content = (1..expected - 1)
                .rev()
                .find(|&y| !row_is_blank(y))
                .unwrap_or_else(|| panic!("{name}: cell drew no content"));
            assert_eq!(
                last_content,
                expected - 2,
                "{name}: content ends on row {last_content} but the height model \
                 reserved through {} — the cell has a phantom blank row",
                expected - 2,
            );
        }
    }

    /// Output text has no horizontal scroll, so a long printed line must wrap
    /// onto extra output rows — reachable by `j`/`k` and actually drawn —
    /// rather than clipping at the cell border.
    #[test]
    fn long_output_lines_wrap_onto_extra_rows() {
        use crate::notebook::Output;
        let cfg = crate::config::NotebookConfig::default();
        let tail = "TAIL-MARKER";
        let long = format!("{}{tail}", "x ".repeat(80));
        let cell = Cell {
            id: "w".into(),
            cell_type: CellType::Code,
            source: Rope::from_str("print(row)"),
            outputs: vec![Output::Stream { name: "stdout".into(), text: format!("{long}\n") }],
            execution_count: Some(1),
            rendered: false,
        };
        let width = 60u16;
        let avail = width - 2;
        let limits = OutputLimits::new(&cfg, false);

        let rows = output_rows_content(&cell, limits, geo_of(avail, false));
        assert!(rows.len() > 1, "a 171-char line must wrap at width {avail}");
        assert_eq!(
            cell_output_rows(&cell, limits, geo_of(avail, false)),
            rows.len(),
            "height model and row content disagree on the wrapped row count",
        );
        assert!(
            rows.last().unwrap().contains(tail),
            "the line's tail must land on a real output row: {rows:?}",
        );

        // …and the tail must actually be painted, not clipped off the edge.
        let nb = Notebook {
            path: std::path::PathBuf::from("/tmp/wrap-test.ipynb"),
            metadata: crate::notebook::NotebookMeta { kernel_language: "python".into() },
            cells: vec![cell],
            modified: false,
        };
        let state = NotebookState::new();
        let rope = nb.cells[0].source.clone();
        let mode = Mode::Normal;
        let active = ActiveCellView {
            rope: &rope,
            cursor: 0,
            sel_anchor: 0,
            output_row: None,
            output_col: 0,
            output_anchor: None,
            mode: &mode,
            jump_labels: &[],
            jump_typed: "",
            // `editor.word_wrap` is off: output wraps regardless of the toggle.
            word_wrap: false,
        };
        let height = cell_display_height(&rope, &nb.cells[0], limits, Geometry { cell_px: None, inner_cols: avail, word_wrap: false }) + 4;
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(
                    f, content_area(f), &state, &nb, &active,
                    &std::collections::HashMap::new(),
                    &cfg, None, &mut CellHighlightCache::default(),
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let screen: String = (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains(tail), "wrapped tail was not drawn:\n{screen}");
    }

    /// A scrolled (clipped) tall cell whose output block includes an image
    /// and an error must render without panicking, and a cropped image
    /// request must be emitted when the image straddles the viewport edge.
    #[test]
    fn clipped_cell_with_image_and_error_renders() {
        use crate::notebook::{MimeData, Output};
        // Minimal 4×80 PNG so png_pixel_size() returns real dimensions.
        let png = {
            let mut v = vec![0u8; 24];
            v[16..20].copy_from_slice(&80u32.to_be_bytes());
            v[20..24].copy_from_slice(&600u32.to_be_bytes());
            std::sync::Arc::new(v)
        };
        let cell = Cell {
            id: "c".into(),
            cell_type: CellType::Code,
            source: Rope::from_str(&(0..40).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n")),
            outputs: vec![
                Output::DisplayData { data: MimeData { text_plain: None, image_png: Some(png) } },
                Output::Error {
                    frames: vec![],
                    ename: "E".into(),
                    evalue: "v".into(),
                    traceback: vec!["t1".into(), "t2".into()],
                },
            ],
            execution_count: Some(1),
            rendered: false,
        };
        let nb = Notebook {
            path: std::path::PathBuf::from("/tmp/clip-test.ipynb"),
            metadata: crate::notebook::NotebookMeta { kernel_language: "python".into() },
            cells: vec![cell],
            modified: false,
        };
        let mut state = NotebookState::new();
        // Scroll deep into the cell so the top border + many rows are clipped
        // and the image lands right at the viewport edge.
        state.scroll_offset = 38;
        let rope = nb.cells[0].source.clone();
        let mode = Mode::Normal;
        let active = ActiveCellView {
            rope: &rope,
            cursor: rope.len_chars(),
            sel_anchor: rope.len_chars(),
            output_row: Some(5),
            output_col: 0,
            output_anchor: None,
            mode: &mode,
            jump_labels: &[],
            jump_typed: "",
            word_wrap: false,
        };
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut imgs = Vec::new();
        terminal
            .draw(|f| {
                let (images, _cursor) = render(
                    f, content_area(f), &state, &nb, &active,
                    &std::collections::HashMap::new(),
                    &crate::config::NotebookConfig::default(),
                    Some((18, 9)),
                    &mut CellHighlightCache::default(),
                );
                imgs = images;
            })
            .unwrap();
        // The image is partially scrolled off, so its request carries a crop.
        assert!(imgs.iter().any(|r| r.crop.is_some()),
            "a clipped image must be emitted with a vertical crop");
    }

    /// The output cursor must never land inside an image's own pixel region
    /// — a terminal can visually swallow the hardware cursor under image
    /// content. A reserved left gutter keeps it in a spot the image never
    /// covers (regression: the cursor used to sit at the image's own first
    /// column, exactly where Kitty painted over it).
    #[test]
    fn image_output_cursor_stays_left_of_the_image() {
        use crate::notebook::{MimeData, Output};
        let png = {
            let mut v = vec![0u8; 24];
            v[16..20].copy_from_slice(&40u32.to_be_bytes());
            v[20..24].copy_from_slice(&40u32.to_be_bytes());
            std::sync::Arc::new(v)
        };
        let cell = Cell {
            id: "c".into(),
            cell_type: CellType::Code,
            source: Rope::from_str("x"),
            outputs: vec![Output::DisplayData {
                data: MimeData { text_plain: None, image_png: Some(png) },
            }],
            execution_count: Some(1),
            rendered: false,
        };
        let nb = Notebook {
            path: std::path::PathBuf::from("/tmp/image-cursor-test.ipynb"),
            metadata: crate::notebook::NotebookMeta { kernel_language: "python".into() },
            cells: vec![cell],
            modified: false,
        };
        let state = NotebookState::new();
        let rope = nb.cells[0].source.clone();
        let mode = Mode::Normal;
        let active = ActiveCellView {
            rope: &rope,
            cursor: rope.len_chars(),
            sel_anchor: rope.len_chars(),
            output_row: Some(0), // the image's first row
            output_col: 0,
            output_anchor: None,
            mode: &mode,
            jump_labels: &[],
            jump_typed: "",
            word_wrap: false,
        };
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut cursor_pos = None;
        let mut imgs = Vec::new();
        terminal
            .draw(|f| {
                let (images, cursor) = render(
                    f, content_area(f), &state, &nb, &active,
                    &std::collections::HashMap::new(),
                    &crate::config::NotebookConfig::default(),
                    Some((18, 9)),
                    &mut CellHighlightCache::default(),
                );
                cursor_pos = cursor;
                imgs = images;
            })
            .unwrap();

        let (cx, _cy) = cursor_pos.expect("the output cursor must be visible on the image row");
        let img = imgs.first().expect("an image request must be emitted");
        assert!(
            cx < img.col,
            "cursor at col {cx} must sit left of the image, which starts at col {}",
            img.col,
        );
    }

    /// A navigable frame line underlines only its visible text, not the row's
    /// full-width padding (regression: the underline spanned the whole screen).
    #[test]
    fn link_frame_underline_is_scoped_to_text() {
        use crate::notebook::{ErrorFrame, Output};
        let cell = Cell {
            id: "c".into(),
            cell_type: CellType::Code,
            source: Rope::from_str("boom()"),
            outputs: vec![Output::Error {
                ename: "IndexError".into(),
                evalue: "oops".into(),
                traceback: vec![
                    "Traceback (most recent call last):".into(),
                    "  File \"Cell [1]\", line 1, in <module>".into(),
                    "IndexError: oops".into(),
                ],
                frames: vec![ErrorFrame {
                    tb_index: 1,
                    cell_id: Some("c".into()),
                    cell_number: 1,
                    line: 0,
                }],
            }],
            execution_count: Some(1),
            rendered: false,
        };
        let nb = Notebook {
            path: std::path::PathBuf::from("/tmp/link-test.ipynb"),
            metadata: crate::notebook::NotebookMeta { kernel_language: "python".into() },
            cells: vec![cell],
            modified: false,
        };
        let state = NotebookState::new();
        let rope = nb.cells[0].source.clone();
        let mode = Mode::Normal;
        let active = ActiveCellView {
            rope: &rope,
            cursor: 0,
            sel_anchor: 0,
            output_row: None,
            output_col: 0,
            output_anchor: None,
            mode: &mode,
            jump_labels: &[],
            jump_typed: "",
            word_wrap: false,
        };
        let width = 80u16;
        let backend = ratatui::backend::TestBackend::new(width, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(
                    f, content_area(f), &state, &nb, &active,
                    &std::collections::HashMap::new(),
                    &crate::config::NotebookConfig::default(),
                    None, &mut CellHighlightCache::default(),
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let underlined = |x: u16, y: u16| {
            buf[(x, y)].style().add_modifier.contains(Modifier::UNDERLINED)
        };
        // Find the row carrying the frame text.
        let row_text = |y: u16| (0..width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>();
        let frame_row = (0..12)
            .find(|&y| row_text(y).contains("Cell [1]"))
            .expect("frame row must be drawn");
        // A cell inside the text ("File") is underlined; the far-right padding is not.
        let file_x = (0..width).find(|&x| buf[(x, frame_row)].symbol() == "F").unwrap();
        assert!(underlined(file_x, frame_row), "the link text must be underlined");
        assert!(!underlined(width - 2, frame_row), "trailing padding must not be underlined");
        // Column 0 (border) and the left pad before the text are not underlined.
        assert!(!underlined(0, frame_row));
    }

    #[test]
    fn error_frame_maps_to_output_cursor_row() {
        use crate::notebook::{ErrorFrame, Output};
        // A 2-line stream precedes the error, so the error headline starts at
        // output row 2; traceback lines follow at rows 3, 4, 5.
        let cell = Cell {
            id: "c".into(),
            cell_type: CellType::Code,
            source: Rope::from_str("boom()"),
            outputs: vec![
                Output::Stream { name: "stdout".into(), text: "one\ntwo\n".into() },
                Output::Error {
                    ename: "IndexError".into(),
                    evalue: "oops".into(),
                    // tb rows: 0="Traceback…", 1=File(frame), 2=source, 3=exc
                    traceback: vec![
                        "Traceback (most recent call last):".into(),
                        "  File \"Cell [1]\", line 2, in <module>".into(),
                        "    boom()".into(),
                        "IndexError: oops".into(),
                    ],
                    frames: vec![ErrorFrame {
                        tb_index: 1,
                        cell_id: Some("c".into()),
                        cell_number: 1,
                        line: 1,
                    }],
                },
            ],
            execution_count: Some(1),
            rendered: false,
        };
        let cfg = crate::config::NotebookConfig::default();
        let limits = OutputLimits::new(&cfg, false);
        // The frame's File line is the 2nd traceback row → base 2 (stream) +
        // 1 (headline) + 1 (tb_index) = output row 4.
        let hit = error_frame_at_output_row(&cell, 4, limits, geo_of(80, false));
        assert!(hit.is_some(), "output row 4 must resolve to the frame");
        assert_eq!(hit.unwrap().line, 1);
        // The headline row (2) and non-frame traceback rows are not links.
        assert!(error_frame_at_output_row(&cell, 2, limits, geo_of(80, false)).is_none());
        assert!(error_frame_at_output_row(&cell, 3, limits, geo_of(80, false)).is_none());
        // A row inside the leading stream isn't a link either.
        assert!(error_frame_at_output_row(&cell, 0, limits, geo_of(80, false)).is_none());
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

