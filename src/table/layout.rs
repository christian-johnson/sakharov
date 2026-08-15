//! The single geometry model for the table view.
//!
//! Column widths, which columns are on screen, and how a value is squeezed into
//! its column all come from here.  The renderer ([`crate::table_ui`]) and the
//! scroll math ([`scroll_col_for_cursor`]) MUST both derive geometry from these
//! functions: if either computes a width or a visible range on its own, the
//! cursor lands on a different cell than the one drawn under it.  (This is the
//! same invariant `notebook_ui::nb_cell_height` carries for the notebook view,
//! and it is pinned by `layout_contains_cursor_after_scroll`.)

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::TableConfig;

use super::{Column, TableSource};

/// Blank columns drawn between two data columns.
pub const COL_GAP: u16 = 1;
/// Rows of the grid area taken by the column-name row itself.
pub const NAME_ROWS: u16 = 1;
/// Appended to a value that did not fit its column.
pub const ELLIPSIS: char = '…';
/// Stands in for a line break inside a cell, so a multi-line value stays one
/// row tall and still reads as having structure.
pub const NEWLINE_GLYPH: char = '↵';

/// Rows of the grid area the header occupies: the column names, plus the
/// distribution sparkline when it is enabled.
///
/// A function of config, computed **here and only here** — the renderer and the
/// scroll math both offset their rows by it, and if they ever disagreed by one,
/// the cursor would sit on a different row than the one drawn under it.
pub fn header_rows(cfg: &TableConfig) -> u16 {
    NAME_ROWS + u16::from(cfg.column_sparkline)
}

/// Data rows that fit in a grid area `area_height` rows tall.
///
/// The scroll math and the renderer both size the row window with this, so the
/// last row on screen is always a row the cursor can reach.
pub fn visible_rows(area_height: u16, cfg: &TableConfig) -> usize {
    area_height.saturating_sub(header_rows(cfg)) as usize
}

/// Display width of `s` (after [`sanitize`]; East-Asian wide chars count 2).
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Flatten a raw cell value to a single line of printable characters.
///
/// Newlines become [`NEWLINE_GLYPH`], tabs and other control characters become
/// spaces.  Every cell in the grid is exactly one row tall — that is what stops
/// a column of paragraph-length text from swallowing the view — so a value's
/// own line breaks must never reach the renderer.
pub fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // CRLF is one line break, not two.
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(NEWLINE_GLYPH);
            }
            '\n' => out.push(NEWLINE_GLYPH),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Fit a raw cell value into `width` display columns.
///
/// Returns the text to draw and whether it was truncated (the caller colours
/// the ellipsis differently, so "shortened" is never confused with a value that
/// genuinely ends in a `…`).  The full text stays reachable via the peek float
/// and the cell buffer, so truncating here loses nothing.
pub fn fit_cell(raw: &str, width: usize) -> (String, bool) {
    let text = sanitize(raw);
    if width == 0 {
        return (String::new(), !text.is_empty());
    }
    if display_width(&text) <= width {
        return (text, false);
    }

    // Reserve one column for the ellipsis, then take whole characters while
    // they fit — a double-width char that would straddle the boundary is
    // dropped rather than half-drawn.
    let budget = width - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push(ELLIPSIS);
    (out, true)
}

/// Columns `col` would need to show its widest sampled value and its header in
/// full — before any clamping.  This is the ceiling [`column_widths`] expands a
/// column towards; growing past it would only add padding.
fn natural_width(col: &Column) -> usize {
    col.width_hint.max(display_width(&col.name))
}

/// Drawn width of `col`: its natural width, clamped to the configured bounds.
///
/// The *base* width, computed from the column alone.  [`column_widths`] is what
/// the layout actually uses — it may widen a capped column into space the
/// viewport would otherwise leave blank.
pub fn column_width(col: &Column, cfg: &TableConfig) -> u16 {
    natural_width(col).clamp(cfg.min_col_width, cfg.max_col_width) as u16
}

/// Drawn width of every column, for a grid `area_width` columns wide.
///
/// Each column starts at its [`column_width`].  When all of them fit with room
/// to spare and `table.fill_width` is on, the leftover columns are handed to the
/// ones `max_col_width` cut short — proportionally to how much each is missing,
/// and never past its [`natural_width`].  That is what stops a three-column
/// table from truncating its one text column while half the terminal sits
/// blank, without stretching a `true`/`false` column to fill space it has no
/// content for.
///
/// Widths are a function of the whole column set and the viewport, **not** of
/// `scroll_col`: a column that changed width as the grid scrolled under it
/// would make [`scroll_col_for_cursor`] chase its own tail.  Expansion only
/// happens when every column is on screen at once, so the two never interact.
pub fn column_widths(cols: &[Column], gutter: u16, area_width: u16, cfg: &TableConfig) -> Vec<u16> {
    let mut widths: Vec<u16> = cols.iter().map(|c| column_width(c, cfg)).collect();
    if !cfg.fill_width || cols.is_empty() {
        return widths;
    }

    let used = widths.iter().map(|&w| w as usize).sum::<usize>()
        + gutter as usize
        + COL_GAP as usize * (cols.len() - 1);
    let slack = match (area_width as usize).checked_sub(used) {
        Some(s) if s > 0 => s,
        // Nothing spare — the grid scrolls horizontally, so there is no blank
        // space to reclaim in the first place.
        _ => return widths,
    };

    // What each column is still missing. A column drawn at its natural width
    // asks for nothing, however much room is left over.
    let want: Vec<usize> = cols
        .iter()
        .zip(&widths)
        .map(|(c, &w)| natural_width(c).saturating_sub(w as usize))
        .collect();
    let total: usize = want.iter().sum();
    if total == 0 {
        return widths;
    }

    let give = slack.min(total);
    let mut handed = 0usize;
    for (w, &d) in widths.iter_mut().zip(&want) {
        let add = d * give / total;
        *w += add as u16;
        handed += add;
    }
    // Largest-remainder tail: the proportional split rounds down, so walk the
    // still-unsatisfied columns handing out one column each until the leftover
    // is gone — otherwise the grid stops a few columns short of the edge.
    let mut leftover = give - handed;
    while leftover > 0 {
        let before = leftover;
        for (w, col) in widths.iter_mut().zip(cols) {
            if leftover == 0 {
                break;
            }
            if (*w as usize) < natural_width(col) {
                *w += 1;
                leftover -= 1;
            }
        }
        if leftover == before {
            break;
        }
    }
    widths
}

/// Width of the row-number gutter, including its trailing separator space.
/// `0` when row numbers are off.
pub fn gutter_width(rows: usize, cfg: &TableConfig) -> u16 {
    if !cfg.row_numbers {
        return 0;
    }
    // Row numbers are 1-based, so `rows` itself is the widest label.
    let digits = rows.max(1).to_string().len() as u16;
    digits + 1
}

/// A column's on-screen placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleColumn {
    /// Index into the source's columns.
    pub idx: usize,
    /// Screen x offset, relative to the grid area's left edge.
    pub x: u16,
    /// Columns actually available to draw in — less than `want` when clipped
    /// by the right edge.
    pub width: u16,
    /// The width this column was laid out at ([`column_widths`]).
    pub want: u16,
}

/// Where everything sits for one frame.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// Width of the row-number gutter (0 when disabled).
    pub gutter: u16,
    /// Columns intersecting the viewport, left to right.
    pub columns: Vec<VisibleColumn>,
}

impl Layout {
    /// The placement of source column `idx`, if it is on screen.
    pub fn find(&self, idx: usize) -> Option<&VisibleColumn> {
        self.columns.iter().find(|c| c.idx == idx)
    }

    /// True when column `idx` is on screen *and* not clipped by the right edge.
    /// The cursor column must satisfy this, or part of the cell it is sitting
    /// on is off-screen.
    pub fn shows_fully(&self, idx: usize) -> bool {
        self.find(idx).is_some_and(|v| v.width >= v.want)
    }
}

/// Lay out the columns visible in `area_width`, starting at `scroll_col`.
///
/// The first column is included even when it cannot fit, so a terminal too
/// narrow for one column still shows a clipped one rather than an empty grid.
pub fn compute(
    source: &dyn TableSource,
    scroll_col: usize,
    area_width: u16,
    cfg: &TableConfig,
) -> Layout {
    let cols = source.columns();
    let gutter = gutter_width(source.row_count().unwrap_or_else(|| source.loaded_rows()), cfg);
    let mut layout = Layout {
        gutter,
        columns: Vec::new(),
    };

    let widths = column_widths(cols, gutter, area_width, cfg);
    let mut x = gutter;
    for (idx, &want) in widths.iter().enumerate().skip(scroll_col) {
        let remaining = area_width.saturating_sub(x);
        if remaining == 0 {
            break;
        }
        let width = want.min(remaining);
        layout.columns.push(VisibleColumn { idx, x, width, want });
        if width < want {
            // Clipped by the right edge — nothing further can fit.
            break;
        }
        x += want + COL_GAP;
    }
    layout
}

/// The smallest `scroll_col` that keeps `cursor_col` fully on screen.
///
/// Scrolls right by the minimum number of columns needed to reveal the cursor,
/// then pulls back as far left as the cursor allows — so horizontal movement
/// advances a column at a time, and a viewport that grew (a resize, or a table
/// that now fits) never shows blank space on the right while columns sit hidden
/// off the left edge.
pub fn scroll_col_for_cursor(
    source: &dyn TableSource,
    scroll_col: usize,
    cursor_col: usize,
    area_width: u16,
    cfg: &TableConfig,
) -> usize {
    let mut scroll = scroll_col.min(cursor_col);
    // Terminates: `scroll == cursor_col` always shows the cursor column (the
    // first column is placed unconditionally by `compute`).
    while scroll < cursor_col && !compute(source, scroll, area_width, cfg).shows_fully(cursor_col) {
        scroll += 1;
    }
    while scroll > 0 && compute(source, scroll - 1, area_width, cfg).shows_fully(cursor_col) {
        scroll -= 1;
    }
    scroll
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::MemSource;

    /// Base config for the packing/scrolling tests: fill is off so a width is
    /// exactly `column_width`, which is what those assertions are about.
    /// `fill_cfg` covers the expanding case.
    fn cfg() -> TableConfig {
        TableConfig {
            row_numbers: false,
            min_col_width: 3,
            max_col_width: 10,
            fill_width: false,
            ..Default::default()
        }
    }

    fn fill_cfg() -> TableConfig {
        TableConfig { fill_width: true, ..cfg() }
    }

    // --- cell fitting -----------------------------------------------------

    #[test]
    fn value_that_fits_is_untouched() {
        assert_eq!(fit_cell("abc", 5), ("abc".to_string(), false));
        assert_eq!(fit_cell("abcde", 5), ("abcde".to_string(), false));
    }

    #[test]
    fn long_value_is_truncated_with_an_ellipsis() {
        let (text, truncated) = fit_cell("abcdefghij", 5);
        assert_eq!(text, "abcd…");
        assert!(truncated);
        assert_eq!(display_width(&text), 5, "never exceeds the column width");
    }

    #[test]
    fn truncation_never_overruns_on_wide_characters() {
        // Each CJK char is 2 columns wide: 2 fit in the 5-col budget (4 columns)
        // and the third is dropped rather than drawn half-width.
        let (text, truncated) = fit_cell("日本語テキスト", 5);
        assert!(truncated);
        assert_eq!(text, "日本…");
        assert_eq!(display_width(&text), 5);

        // An odd budget must not be overrun by a wide char straddling the edge.
        let (text, _) = fit_cell("日本語", 4);
        assert_eq!(text, "日…");
        assert!(display_width(&text) <= 4);
    }

    #[test]
    fn multiline_and_control_characters_collapse_to_one_row() {
        let (text, _) = fit_cell("line one\nline two", 40);
        assert_eq!(text, format!("line one{NEWLINE_GLYPH}line two"));
        assert!(!text.contains('\n'), "a cell is always exactly one row tall");

        // CRLF is a single break, and tabs become spaces.
        assert_eq!(sanitize("a\r\nb"), format!("a{NEWLINE_GLYPH}b"));
        assert_eq!(sanitize("a\tb"), "a b");
    }

    #[test]
    fn degenerate_widths_are_safe() {
        assert_eq!(fit_cell("abc", 0), (String::new(), true));
        assert_eq!(fit_cell("", 0), (String::new(), false));
        // One column of space can only hold the truncation marker itself.
        assert_eq!(fit_cell("abc", 1), ("…".to_string(), true));
    }

    // --- column widths ----------------------------------------------------

    #[test]
    fn column_width_is_clamped_to_the_configured_bounds() {
        let src = MemSource::new(
            &["id", "description"],
            &[&["1", "a value far longer than ten columns"], &["2", "x"]],
        );
        let cfg = cfg();
        // Short column: floored at min_col_width, and never narrower than its header.
        assert_eq!(column_width(&src.columns()[0], &cfg), 3);
        // Long column: capped at max_col_width.
        assert_eq!(column_width(&src.columns()[1], &cfg), 10);
    }

    #[test]
    fn a_wide_header_widens_a_narrow_column() {
        let src = MemSource::new(&["measurement"], &[&["1"], &["2"]]);
        // Header is 11 chars, capped to max_col_width = 10.
        assert_eq!(column_width(&src.columns()[0], &cfg()), 10);
    }

    // --- layout -----------------------------------------------------------

    fn wide_source() -> MemSource {
        MemSource::new(
            &["aaaa", "bbbb", "cccc", "dddd"],
            &[&["1", "2", "3", "4"], &["5", "6", "7", "8"]],
        )
    }

    #[test]
    fn columns_are_packed_left_to_right_with_a_gap() {
        let src = wide_source();
        let layout = compute(&src, 0, 40, &cfg());
        assert_eq!(layout.columns.len(), 4);
        assert_eq!(layout.columns[0].x, 0);
        assert_eq!(layout.columns[0].width, 4);
        // 4 wide + 1 gap.
        assert_eq!(layout.columns[1].x, 5);
        assert_eq!(layout.columns[3].x, 15);
    }

    #[test]
    fn the_gutter_offsets_the_first_column() {
        let src = wide_source();
        let with_numbers = TableConfig {
            row_numbers: true,
            ..cfg()
        };
        let layout = compute(&src, 0, 40, &with_numbers);
        // 2 rows → one digit + a separator space.
        assert_eq!(layout.gutter, 2);
        assert_eq!(layout.columns[0].x, 2);
    }

    #[test]
    fn narrow_viewport_clips_the_last_column_and_stops() {
        let src = wide_source();
        // Room for "aaaa gap bbbb" is 9; 7 columns clips the second at 2.
        let layout = compute(&src, 0, 7, &cfg());
        assert_eq!(layout.columns.len(), 2);
        assert_eq!(layout.columns[1].width, 2);
        assert!(!layout.shows_fully(1));
        assert!(layout.shows_fully(0));
    }

    // --- filling the viewport --------------------------------------------

    /// Longer than the viewports the fill tests use, so the text column's
    /// demand is never fully satisfied and the slack is what bounds it.
    const LONG_NOTE: &str = "a free-text note that runs on well past the width of \
        any terminal these tests pretend to have, so the column can always take \
        whatever room is going spare";

    /// The motivating shape: two short columns and one free-text column whose
    /// values are longer than any sane terminal, in a viewport wide enough for
    /// all three with room to spare.
    fn mixed_source() -> MemSource {
        MemSource::new(
            &["ok", "flag", "note"],
            &[
                &["true", "false", LONG_NOTE],
                &["false", "true", "another long free-text value"],
            ],
        )
    }

    #[test]
    fn leftover_width_goes_to_the_columns_that_were_truncated() {
        let src = mixed_source();
        let base = column_widths(src.columns(), 0, 80, &cfg());
        assert_eq!(base, vec![5, 5, 10], "capped at max_col_width without fill");

        let filled = column_widths(src.columns(), 0, 80, &fill_cfg());
        // The short columns already show their content in full, so they keep
        // their size — only the truncated text column grows.
        assert_eq!(filled[0], 5);
        assert_eq!(filled[1], 5);
        assert!(filled[2] > 10, "text column grew, got {}", filled[2]);
        let used: u16 = filled.iter().sum::<u16>() + COL_GAP * 2;
        assert_eq!(used, 80, "the grid reaches the right edge exactly");
    }

    #[test]
    fn a_column_never_grows_past_the_width_its_content_needs() {
        // Far more room than the data can use: every column lands on its
        // natural width and the rest of the viewport is simply left blank —
        // padding a column beyond its longest value buys nothing.
        let src = mixed_source();
        let filled = column_widths(src.columns(), 0, 400, &fill_cfg());
        let natural: Vec<u16> = src.columns().iter().map(|c| natural_width(c) as u16).collect();
        assert_eq!(filled, natural);
    }

    #[test]
    fn a_table_too_wide_to_fit_keeps_its_capped_widths() {
        // Nothing to reclaim: the grid scrolls horizontally, so filling would
        // only push more columns off the right edge.
        let src = mixed_source();
        assert_eq!(
            column_widths(src.columns(), 0, 12, &fill_cfg()),
            column_widths(src.columns(), 0, 12, &cfg()),
        );
    }

    #[test]
    fn the_gutter_is_charged_against_the_space_available_to_fill() {
        let src = mixed_source();
        let with_numbers = TableConfig { row_numbers: true, ..fill_cfg() };
        let gutter = gutter_width(src.loaded_rows(), &with_numbers);
        assert!(gutter > 0);
        let filled = column_widths(src.columns(), gutter, 80, &with_numbers);
        let used: u16 = filled.iter().sum::<u16>() + COL_GAP * 2 + gutter;
        assert_eq!(used, 80);
    }

    #[test]
    fn scrolling_right_pulls_back_left_rather_than_leaving_blank_space() {
        // A viewport that grew (resize, or a table that now fits) must not
        // leave the right edge blank while columns hide off the left.
        let src = wide_source();
        assert_eq!(scroll_col_for_cursor(&src, 3, 3, 40, &cfg()), 0);
    }

    #[test]
    fn scroll_col_skips_columns_to_the_left() {
        let src = wide_source();
        let layout = compute(&src, 2, 40, &cfg());
        assert_eq!(layout.columns[0].idx, 2);
        assert_eq!(layout.columns[0].x, 0);
        assert!(layout.find(0).is_none());
    }

    // --- the scroll/render agreement invariant ----------------------------

    #[test]
    fn scrolling_left_and_right_tracks_the_cursor_minimally() {
        let src = wide_source();
        // Cursor left of the viewport: jump straight to it.
        assert_eq!(scroll_col_for_cursor(&src, 2, 0, 40, &cfg()), 0);
        // Already fully visible: don't move.
        assert_eq!(scroll_col_for_cursor(&src, 0, 1, 40, &cfg()), 0);
        // Off the right edge: advance the minimum, not a whole page.
        assert_eq!(scroll_col_for_cursor(&src, 0, 3, 9, &cfg()), 2);
    }

    #[test]
    fn header_height_is_config_driven_and_data_rows_follow_it() {
        // The header's height must be a function of config computed *here* — the
        // renderer offsets its rows by it and the scroll math sizes its window by
        // it, so a second definition anywhere would put the cursor on a
        // different row than the one drawn under it.
        let plain = TableConfig { column_sparkline: false, ..cfg() };
        let sparked = TableConfig { column_sparkline: true, ..cfg() };
        assert_eq!(header_rows(&plain), 1);
        assert_eq!(header_rows(&sparked), 2);

        for height in 0u16..12 {
            for cfg in [&plain, &sparked] {
                // Header + data rows never claim more than the area has.
                assert!(
                    header_rows(cfg) as usize + visible_rows(height, cfg) <= height as usize
                        || visible_rows(height, cfg) == 0,
                    "height {height} overcommitted",
                );
            }
            // Turning the sparkline on costs exactly one data row, never more.
            let lost = visible_rows(height, &plain) - visible_rows(height, &sparked);
            assert!(lost <= 1, "height {height}: lost {lost} rows");
        }
    }

    #[test]
    fn layout_contains_cursor_after_scroll() {
        // The invariant tying the scroll math to the renderer: for any viewport
        // width and any cursor column, the layout drawn after adjusting
        // scroll_col shows the cursor's column in full.  If this ever fails,
        // the cursor is drawn on a cell that is partly or wholly off-screen.
        let src = MemSource::new(
            &["one", "twotwo", "threethree", "f", "fivefive"],
            &[&["1", "2", "3", "4", "5"]],
        );
        for cfg in [
            TableConfig { column_sparkline: false, ..cfg() },
            TableConfig { column_sparkline: true, ..cfg() },
            TableConfig { column_sparkline: false, ..fill_cfg() },
            TableConfig { column_sparkline: true, ..fill_cfg() },
        ] {
        for width in 4u16..40 {
            for cursor_col in 0..src.columns().len() {
                for start in 0..src.columns().len() {
                    let scroll = scroll_col_for_cursor(&src, start, cursor_col, width, &cfg);
                    let layout = compute(&src, scroll, width, &cfg);
                    assert!(
                        layout.find(cursor_col).is_some(),
                        "cursor col {cursor_col} not drawn at width {width} (scroll {scroll})"
                    );
                    // The only case where the cursor column may be clipped is a
                    // viewport too narrow for that single column.
                    let want = layout.find(cursor_col).unwrap().want;
                    if width >= want {
                        assert!(
                            layout.shows_fully(cursor_col),
                            "cursor col {cursor_col} clipped at width {width} (scroll {scroll})"
                        );
                    }
                }
            }
        }
        }
    }

    #[test]
    fn a_filled_layout_never_overruns_the_viewport() {
        // Filling hands out real screen columns, so the sum of what it hands
        // out plus the gaps and the gutter must still fit — an over-wide
        // column would push the last one off the edge it was widened to reach.
        let src = MemSource::new(
            &["id", "flag", "note", "other"],
            &[
                &["1", "true", "a value considerably longer than the cap", "short"],
                &["22", "false", "another long one", "x"],
            ],
        );
        for row_numbers in [false, true] {
            let cfg = TableConfig { row_numbers, ..fill_cfg() };
            for width in 1u16..200 {
                let layout = compute(&src, 0, width, &cfg);
                if let Some(last) = layout.columns.last() {
                    assert!(
                        last.x + last.width <= width,
                        "width {width}: layout runs past the right edge",
                    );
                }
            }
        }
    }
}
