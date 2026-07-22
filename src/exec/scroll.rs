//! Scroll management: the single authoritative `update_scroll` (plain-editor
//! fold/wrap-aware path + notebook in-cell path) and the fold-aware cursor
//! normalisation that keeps the cursor out of hidden fold regions.

use crate::{app::App, selection::Selection};

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
pub(super) fn normalize_cursor_folds_directional(app: &mut App, pre_exec_line: usize) {
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

/// Update scroll_row / scroll_col so the cursor is visible.
///
/// Uses the stored viewport dimensions (`app.viewport_height` / `app.viewport_width`)
/// which are refreshed at the top of every render frame.  This is the single
/// authoritative scroll function.
pub fn update_scroll(app: &mut App) {
    let visible_rows = app.viewport_height;
    let scroll_off = app.config.editor.scroll_off;

    // Seamless, row-granular notebook scroll.  The whole notebook is one
    // vertical stack of cells (each `height` rows, separated by a 1-row gap);
    // the viewport is a window into it anchored by `(scroll_cell,
    // scroll_offset)`.  We locate the cursor's absolute row in that stack —
    // inside the source, or (when `output_row` is set) inside the cell's
    // output block — and nudge the anchor just enough to keep it within the
    // scroll-off margin.  Because the anchor is measured in rows, scrolling
    // moves one line at a time instead of jumping a whole cell.  (The
    // full-screen focused-cell overlay falls through to the plain path.)
    if app.notebook.is_some() && !app.notebook_focused_edit() {
        notebook_update_scroll(app, visible_rows, scroll_off);
        return;
    }

    let git_col = if app.config.editor.git_gutter && app.notebook.is_none() { 1usize } else { 0 };
    let gutter_width = if app.config.editor.line_numbers { 5 + git_col } else { git_col };
    let visible_cols = app.viewport_width.saturating_sub(gutter_width);
    let word_wrap = app.config.editor.word_wrap;

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
    let tab_width = app.config.editor.tab_width;

    {
        // Normalize scroll_row so it never points inside a hidden fold region.
        app.scroll_row = app.fold.normalize_scroll_row(app.scroll_row);

        // Vertical — fold+wrap-aware row count from scroll_row to cursor line.
        let vdist = if word_wrap {
            wrap_visible_row_count(app, app.scroll_row, line_idx, visible_cols, tab_width)
        } else {
            app.fold.visible_row_count(app.scroll_row, line_idx, total_lines)
        };

        if vdist < scroll_off || app.scroll_row > line_idx {
            // Cursor too close to top (or above scroll area): scroll up.
            let desired = scroll_off.min(line_idx);
            app.scroll_row = if word_wrap {
                wrap_scroll_row_for_cursor(&app.fold, rope, line_idx, desired, visible_cols, tab_width)
            } else {
                app.fold.scroll_row_for_cursor(line_idx, desired)
            };
        } else if vdist + scroll_off >= visible_rows {
            // Cursor too close to bottom: scroll down.
            let desired = visible_rows.saturating_sub(scroll_off + 1);
            app.scroll_row = if word_wrap {
                wrap_scroll_row_for_cursor(&app.fold, rope, line_idx, desired, visible_cols, tab_width)
            } else {
                app.fold.scroll_row_for_cursor(line_idx, desired)
            };
        }
    }

    if word_wrap {
        // No horizontal scrolling when wrapping.
        app.scroll_col = 0;
        return;
    }

    // Horizontal — accurate display-column calculation (handles tabs)
    let line_start = rope.line_to_char(line_idx);
    let line_str = rope.line(line_idx);
    let cursor_off = pos - line_start;
    let mut display_col: usize = 0;
    for i in 0..cursor_off {
        display_col +=
            crate::render_util::char_display_width(line_str.char(i), display_col, tab_width);
    }

    if display_col < app.scroll_col {
        app.scroll_col = display_col;
    }
    if display_col >= app.scroll_col + visible_cols {
        app.scroll_col = display_col.saturating_sub(visible_cols) + 1;
    }
}

/// The notebook rendered as one vertical stack of rows: per-cell display
/// heights exactly as `notebook_ui` draws them, plus the focused cell.
struct NbLayout {
    /// `heights[i]` = rows cell `i` occupies, followed by a 1-row gap.
    heights: Vec<usize>,
    /// Focused cell index, clamped into range.
    focused: usize,
    /// Wrap width of the focused cell's source (`None` = no wrapping).
    wrap_width: Option<usize>,
}

/// Measure the whole notebook.  The focused cell is measured against the live
/// buffer rope (its unsaved edits are ahead of `cell.source`) and folded cells
/// collapse to their summary height — both exactly as the renderer does it.
fn nb_layout(app: &App) -> Option<NbLayout> {
    let (nb, state) = app.notebook.as_ref()?;
    if nb.cells.is_empty() {
        return None;
    }
    let cell_px = app.graphics.cell_pixel_size;
    let avail_cols = app.viewport_width.saturating_sub(2) as u16;
    let word_wrap = app.config.editor.word_wrap;
    let focused = state.focused_cell.min(nb.cells.len() - 1);

    let heights = nb.cells.iter().enumerate().map(|(idx, cell)| {
        let folded = state.is_cell_folded(idx) && idx != focused;
        let source = if idx == focused { &app.buffer.rope } else { &cell.source };
        let limits = crate::notebook_ui::OutputLimits::new(
            &app.config.notebook, state.is_output_expanded(idx),
        );
        crate::notebook_ui::nb_cell_height(
            cell, folded, source, limits, cell_px, avail_cols, word_wrap,
        )
    }).collect();

    let wrap_width = crate::notebook_ui::cell_wraps(&nb.cells[focused], word_wrap)
        .then(|| crate::notebook_ui::cell_text_width(avail_cols));

    Some(NbLayout { heights, focused, wrap_width })
}

/// Absolute row of cell `idx`'s top border (each cell is `h + 1` rows tall
/// including the gap that follows it).
fn cell_top(heights: &[usize], idx: usize) -> usize {
    heights[..idx.min(heights.len())].iter().map(|h| h + 1).sum()
}

/// The focused cell's on-screen source lines as `(first_line, line_count)`,
/// or `None` when no notebook is open.  `(0, 0)` means none of the cell's
/// source is currently visible.
///
/// `gw` jump labels must be generated over exactly this range: labelling from
/// the top of the cell instead puts every label above the viewport in a long
/// scrolled cell, so no labels appear at all.
pub fn notebook_visible_source_lines(app: &App) -> Option<(usize, usize)> {
    let layout = nb_layout(app)?;
    let (nb, state) = app.notebook.as_ref()?;
    let viewport = app.viewport_height;
    if viewport == 0 || state.is_cell_folded(layout.focused) {
        return Some((0, 0));
    }

    // Viewport as an absolute row window into the cell stack.
    let sc = state.scroll_cell.min(nb.cells.len() - 1);
    let view_top = cell_top(&layout.heights, sc)
        + state.scroll_offset.min(layout.heights[sc]);
    let view_bottom = view_top + viewport;

    // The focused cell's source rows start one row below its top border.
    let src_top = cell_top(&layout.heights, layout.focused) + 1;
    let src_rows = crate::notebook_ui::cell_visual_rows(&app.buffer.rope, layout.wrap_width);
    let (lo, hi) = (view_top.max(src_top), view_bottom.min(src_top + src_rows));
    if lo >= hi {
        return Some((0, 0));
    }

    let first = crate::notebook_ui::cell_line_at_visual_row(
        &app.buffer.rope, layout.wrap_width, lo - src_top,
    );
    let last = crate::notebook_ui::cell_line_at_visual_row(
        &app.buffer.rope, layout.wrap_width, hi - 1 - src_top,
    );
    Some((first, last + 1 - first))
}

/// Row-granular notebook scroll (see the call site in [`update_scroll`]).
///
/// Models the notebook as one tall stack: cell `i` occupies `heights[i]` rows
/// followed by a 1-row gap.  The scroll anchor `(scroll_cell, scroll_offset)`
/// is the absolute row of the first visible row.  We find the cursor's
/// absolute row, move the anchor the minimum needed to keep it inside the
/// scroll-off margin, clamp against the document end, then translate the new
/// absolute top back into `(scroll_cell, scroll_offset)`.
fn notebook_update_scroll(app: &mut App, viewport: usize, scroll_off: usize) {
    if viewport == 0 {
        return;
    }
    let Some(layout) = nb_layout(app) else { return };
    let cursor = app.selection.head;
    let cell_px = app.graphics.cell_pixel_size;
    let avail_cols = app.viewport_width.saturating_sub(2) as u16;

    let Some((nb, state)) = app.notebook.as_mut() else { return };
    if nb.cells.is_empty() {
        state.scroll_cell = 0;
        state.scroll_offset = 0;
        state.output_row = None;
        return;
    }
    let NbLayout { heights, focused, wrap_width } = layout;

    // Cursor's row within the focused cell.
    let focused_cell = &nb.cells[focused];
    let within = if state.is_cell_folded(focused) {
        1 // the single summary row
    } else {
        let src_rows = crate::notebook_ui::cell_visual_rows(&app.buffer.rope, wrap_width);
        match state.output_row {
            Some(r) => {
                // border + all source rows + divider + output row r
                let limits = crate::notebook_ui::OutputLimits::new(
                    &app.config.notebook, state.is_output_expanded(focused),
                );
                let out_rows = crate::notebook_ui::cell_output_rows(
                    focused_cell, limits, cell_px, avail_cols,
                );
                if out_rows == 0 {
                    // Outputs vanished (e.g. cleared) — fall back to the source.
                    state.output_row = None;
                    1 + crate::notebook_ui::cell_cursor_visual_row(
                        &app.buffer.rope, cursor, wrap_width,
                    )
                } else {
                    let r = r.min(out_rows - 1);
                    state.output_row = Some(r);
                    1 + src_rows + 1 + r
                }
            }
            None => {
                1 + crate::notebook_ui::cell_cursor_visual_row(&app.buffer.rope, cursor, wrap_width)
            }
        }
    };
    let cursor_abs = cell_top(&heights, focused) + within;

    // Total document rows (drop the trailing gap after the last cell).
    let total_rows: usize = heights.iter().map(|h| h + 1).sum::<usize>().saturating_sub(1);

    // Current anchor as an absolute row.
    let sc = state.scroll_cell.min(nb.cells.len() - 1);
    let cur_top = cell_top(&heights, sc) + state.scroll_offset.min(heights[sc]);

    let so = scroll_off.min(viewport.saturating_sub(1) / 2);
    let mut new_top = cur_top;
    if cursor_abs < cur_top + so {
        new_top = cursor_abs.saturating_sub(so);
    } else if cursor_abs + so + 1 > cur_top + viewport {
        new_top = (cursor_abs + so + 1).saturating_sub(viewport);
    }
    // Don't scroll past the end of the document (leave the last rows pinned to
    // the bottom), but never hide the cursor to do so.
    let max_top = total_rows.saturating_sub(viewport);
    if new_top > max_top {
        new_top = max_top;
    }
    if new_top > cursor_abs {
        new_top = cursor_abs;
    }

    // Translate the absolute top back into (scroll_cell, scroll_offset).
    let (sc, so_rows) = abs_row_to_anchor(&heights, new_top);
    state.scroll_cell = sc;
    state.scroll_offset = so_rows;
}

/// Convert an absolute document row into a `(scroll_cell, scroll_offset)`
/// anchor.
///
/// A row landing on the gap *after* cell `idx` is represented as
/// `(idx, heights[idx])` — an offset equal to the cell's height, which the
/// renderer draws as no cell rows followed by the gap row.  Rounding it up to
/// the next cell instead would make the realised top one row past the
/// requested one, so every crossing of a cell boundary scrolled two rows for
/// one keypress.
fn abs_row_to_anchor(heights: &[usize], abs_row: usize) -> (usize, usize) {
    let mut top = 0usize;
    for (idx, &h) in heights.iter().enumerate() {
        if abs_row <= top + h {
            return (idx, abs_row - top);
        }
        top += h + 1;
    }
    (heights.len().saturating_sub(1), 0)
}

/// Count visual rows from `from` (inclusive) to `to` (exclusive), accounting
/// for folds and word-wrap.  `text_width` is the number of display columns
/// available for text (viewport minus gutter).
fn wrap_visible_row_count(
    app: &App,
    from: usize,
    to: usize,
    text_width: usize,
    tab_width: usize,
) -> usize {
    let rope = &app.buffer.rope;
    let total_lines = rope.len_lines();
    let mut count = 0;
    let mut line = from;
    while line < to && line < total_lines {
        if app.fold.is_hidden(line) {
            line += 1;
            continue;
        }
        if let Some(end) = app.fold.fold_end_at(line) {
            count += 1;
            line = end + 1;
        } else {
            count += crate::ui::visual_line_height(rope, line, text_width, tab_width);
            line += 1;
        }
    }
    count
}

/// Walk backward from `cursor_line` by `desired_vrows` visual rows (fold+wrap
/// aware) and return the resulting scroll_row.
fn wrap_scroll_row_for_cursor(
    fold: &crate::fold::FoldState,
    rope: &ropey::Rope,
    cursor_line: usize,
    desired_vrows: usize,
    text_width: usize,
    tab_width: usize,
) -> usize {
    let mut line = cursor_line;
    let mut remaining = desired_vrows;

    while remaining > 0 && line > 0 {
        line -= 1;
        if let Some(start) = fold.fold_start_hiding(line) {
            line = start;
        }
        let height = if fold.is_hidden(line) {
            0
        } else if fold.fold_end_at(line).is_some() {
            1
        } else {
            crate::ui::visual_line_height(rope, line, text_width, tab_width)
        };
        remaining = remaining.saturating_sub(height);
    }
    line
}