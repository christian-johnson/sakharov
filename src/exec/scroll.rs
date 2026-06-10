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
    let scroll_off = app.config.editor.scroll_off;
    let tab_width = app.config.editor.tab_width;

    if app.notebook.is_some() && !app.notebook_focused_edit() {
        // Editing within the focused cell.  The cell is in `app.buffer`, but it
        // does not fill the viewport: cells above it (and its own top border)
        // push its content downward.  We first keep the focused cell visible at
        // the cell granularity, then scroll *within* the cell so the cursor
        // stays inside the rows actually available below that offset — the same
        // text-buffer behaviour you'd expect, rather than letting the cursor
        // slide off the bottom of the screen.
        let image_rows = app.config.notebook.image_rows;
        let cell_px = app.graphics.cell_pixel_size;
        let avail_cols = app.viewport_width.saturating_sub(2) as u16;
        let mut new_scroll_row = app.scroll_row;
        if let Some((nb, state)) = app.notebook.as_mut() {
            state.ensure_focused_visible(
                &nb.cells, visible_rows, rope, image_rows, cell_px, avail_cols,
            );
            let focused = state.focused_cell.min(nb.cells.len().saturating_sub(1));
            // Rows consumed above the focused cell's content: each fully-shown
            // preceding cell plus the 1-row inter-cell gap, then the focused
            // cell's own top border.
            let mut content_top = 0usize;
            for idx in state.scroll_cell..focused {
                let h = if state.is_cell_folded(idx) {
                    3
                } else {
                    crate::notebook_ui::cell_display_height(
                        &nb.cells[idx].source, &nb.cells[idx], image_rows, cell_px, avail_cols,
                    ) as usize
                };
                content_top += h + 1;
            }
            content_top += 1; // top border of the focused cell

            let avail = visible_rows.saturating_sub(content_top).max(1);
            let so = scroll_off.min(avail.saturating_sub(1) / 2);
            if line_idx < new_scroll_row + so || new_scroll_row > line_idx {
                new_scroll_row = line_idx.saturating_sub(so);
            } else if line_idx + so + 1 > new_scroll_row + avail {
                new_scroll_row = (line_idx + so + 1).saturating_sub(avail);
            }
            let max_scroll = total_lines.saturating_sub(1);
            if new_scroll_row > max_scroll {
                new_scroll_row = max_scroll;
            }
        }
        app.scroll_row = new_scroll_row;
    } else {
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