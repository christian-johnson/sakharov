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

/// Row-granular notebook scroll (see the call site in [`update_scroll`]).
///
/// Models the notebook as one tall stack: cell `i` occupies `heights[i]` rows
/// followed by a 1-row gap.  The scroll anchor `(scroll_cell, scroll_offset)`
/// is the absolute row of the first visible row.  We find the cursor's
/// absolute row, move the anchor the minimum needed to keep it inside the
/// scroll-off margin, clamp against the document end, then translate the new
/// absolute top back into `(scroll_cell, scroll_offset)`.
fn notebook_update_scroll(app: &mut App, viewport: usize, scroll_off: usize) {
    let cell_px = app.graphics.cell_pixel_size;
    let avail_cols = app.viewport_width.saturating_sub(2) as u16;
    let word_wrap = app.config.editor.word_wrap;
    let nb_config = app.config.notebook.clone();
    let cursor = app.selection.head;

    if viewport == 0 {
        return;
    }

    let Some((nb, state)) = app.notebook.as_mut() else { return };
    if nb.cells.is_empty() {
        state.scroll_cell = 0;
        state.scroll_offset = 0;
        state.output_row = None;
        return;
    }
    let focused = state.focused_cell.min(nb.cells.len() - 1);

    // Per-cell heights, exactly as the renderer draws them (focused cell uses
    // the live buffer rope; folded cells collapse to 3 rows).
    let heights: Vec<usize> = nb.cells.iter().enumerate().map(|(idx, cell)| {
        let folded = state.is_cell_folded(idx) && idx != focused;
        let source = if idx == focused { &app.buffer.rope } else { &cell.source };
        crate::notebook_ui::nb_cell_height(
            cell, folded, source, &nb_config, cell_px, avail_cols, word_wrap,
        )
    }).collect();

    // Absolute row of each cell's top border (height + 1-row gap between cells).
    let cell_top = |idx: usize| -> usize {
        heights[..idx].iter().map(|h| h + 1).sum()
    };

    // Cursor's row within the focused cell.
    let focused_cell = &nb.cells[focused];
    let within = if state.is_cell_folded(focused) {
        1 // the single summary row
    } else {
        let wrap_w = crate::notebook_ui::cell_wraps(focused_cell, word_wrap)
            .then(|| crate::notebook_ui::cell_text_width(avail_cols));
        let src_rows = crate::notebook_ui::cell_visual_rows(&app.buffer.rope, wrap_w);
        match state.output_row {
            Some(r) => {
                // border + all source rows + divider + output row r
                let out_rows = crate::notebook_ui::cell_output_rows(
                    focused_cell, &nb_config, cell_px, avail_cols,
                );
                if out_rows == 0 {
                    // Outputs vanished (e.g. cleared) — fall back to the source.
                    state.output_row = None;
                    1 + crate::notebook_ui::cell_cursor_visual_row(
                        &app.buffer.rope, cursor, wrap_w,
                    )
                } else {
                    let r = r.min(out_rows - 1);
                    state.output_row = Some(r);
                    1 + src_rows + 1 + r
                }
            }
            None => {
                1 + crate::notebook_ui::cell_cursor_visual_row(&app.buffer.rope, cursor, wrap_w)
            }
        }
    };
    let cursor_abs = cell_top(focused) + within;

    // Total document rows (drop the trailing gap after the last cell).
    let total_rows: usize = heights.iter().map(|h| h + 1).sum::<usize>().saturating_sub(1);

    // Current anchor as an absolute row.
    let cur_top = cell_top(state.scroll_cell.min(nb.cells.len() - 1))
        + state.scroll_offset.min(heights[state.scroll_cell.min(nb.cells.len() - 1)]);

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
/// anchor.  A row that lands on an inter-cell gap snaps to the next cell's
/// top (offset 0), so the viewport never opens on a leading gap row.
fn abs_row_to_anchor(heights: &[usize], abs_row: usize) -> (usize, usize) {
    let mut top = 0usize;
    for (idx, &h) in heights.iter().enumerate() {
        if abs_row < top + h {
            return (idx, abs_row - top);
        }
        if abs_row == top + h {
            // On the gap row after cell `idx`: show the next cell at the top.
            return ((idx + 1).min(heights.len() - 1), 0);
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