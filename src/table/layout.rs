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
/// Rows of the grid area taken by the column-name header.
pub const HEADER_ROWS: u16 = 1;
/// Appended to a value that did not fit its column.
pub const ELLIPSIS: char = '…';
/// Stands in for a line break inside a cell, so a multi-line value stays one
/// row tall and still reads as having structure.
pub const NEWLINE_GLYPH: char = '↵';

/// Data rows that fit in a grid area `area_height` rows tall.
///
/// The scroll math and the renderer both size the row window with this, so the
/// last row on screen is always a row the cursor can reach.
pub fn visible_rows(area_height: u16) -> usize {
    area_height.saturating_sub(HEADER_ROWS) as usize
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

/// Drawn width of `col`: its natural width, clamped to the configured bounds.
pub fn column_width(col: &Column, cfg: &TableConfig) -> u16 {
    let natural = col.width_hint.max(display_width(&col.name));
    natural.clamp(cfg.min_col_width, cfg.max_col_width) as u16
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
    /// Columns actually available to draw in — less than
    /// [`column_width`] when clipped by the right edge.
    pub width: u16,
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
    pub fn shows_fully(&self, idx: usize, cfg: &TableConfig, cols: &[Column]) -> bool {
        self.find(idx)
            .zip(cols.get(idx))
            .is_some_and(|(v, c)| v.width >= column_width(c, cfg))
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

    let mut x = gutter;
    for (idx, col) in cols.iter().enumerate().skip(scroll_col) {
        let remaining = area_width.saturating_sub(x);
        if remaining == 0 {
            break;
        }
        let want = column_width(col, cfg);
        let width = want.min(remaining);
        layout.columns.push(VisibleColumn { idx, x, width });
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
/// Scrolls left when the cursor is left of the viewport, and right by the
/// minimum number of columns otherwise — so horizontal movement advances a
/// column at a time instead of paging.
pub fn scroll_col_for_cursor(
    source: &dyn TableSource,
    scroll_col: usize,
    cursor_col: usize,
    area_width: u16,
    cfg: &TableConfig,
) -> usize {
    if cursor_col < scroll_col {
        return cursor_col;
    }
    let cols = source.columns();
    let mut scroll = scroll_col;
    // Terminates: `scroll == cursor_col` always shows the cursor column (the
    // first column is placed unconditionally by `compute`).
    while scroll < cursor_col
        && !compute(source, scroll, area_width, cfg).shows_fully(cursor_col, cfg, cols)
    {
        scroll += 1;
    }
    scroll
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::VecSource;

    fn cfg() -> TableConfig {
        TableConfig {
            row_numbers: false,
            min_col_width: 3,
            max_col_width: 10,
            ..Default::default()
        }
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
        let src = VecSource::new(
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
        let src = VecSource::new(&["measurement"], &[&["1"], &["2"]]);
        // Header is 11 chars, capped to max_col_width = 10.
        assert_eq!(column_width(&src.columns()[0], &cfg()), 10);
    }

    // --- layout -----------------------------------------------------------

    fn wide_source() -> VecSource {
        VecSource::new(
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
        assert!(!layout.shows_fully(1, &cfg(), src.columns()));
        assert!(layout.shows_fully(0, &cfg(), src.columns()));
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
    fn layout_contains_cursor_after_scroll() {
        // The invariant tying the scroll math to the renderer: for any viewport
        // width and any cursor column, the layout drawn after adjusting
        // scroll_col shows the cursor's column in full.  If this ever fails,
        // the cursor is drawn on a cell that is partly or wholly off-screen.
        let src = VecSource::new(
            &["one", "twotwo", "threethree", "f", "fivefive"],
            &[&["1", "2", "3", "4", "5"]],
        );
        let cfg = cfg();
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
                    let want = column_width(&src.columns()[cursor_col], &cfg);
                    if width >= want {
                        assert!(
                            layout.shows_fully(cursor_col, &cfg, src.columns()),
                            "cursor col {cursor_col} clipped at width {width} (scroll {scroll})"
                        );
                    }
                }
            }
        }
    }
}
