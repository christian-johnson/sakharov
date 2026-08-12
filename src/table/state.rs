//! Cursor + viewport state for the table view.

/// Where the cursor and the viewport are in a table.
///
/// Deliberately separate from the [`TableSource`](super::TableSource): the data
/// is a view onto a file or query, this is a view onto the data.  Mutated only
/// from `exec::table`, like every other piece of `App` state.
pub struct TableState {
    /// Cursor row, 0-based, indexing the source's rows (the header is not a row).
    pub cursor_row: usize,
    /// Cursor column, 0-based, indexing the source's columns.
    pub cursor_col: usize,
    /// First data row drawn below the header.
    pub scroll_row: usize,
    /// First column drawn after the row-number gutter.  Maintained by
    /// [`layout::scroll_col_for_cursor`](super::layout::scroll_col_for_cursor)
    /// so that scroll and rendering can never disagree about which columns fit.
    pub scroll_col: usize,
}

impl TableState {
    pub fn new() -> Self {
        Self {
            cursor_row: 0,
            cursor_col: 0,
            scroll_row: 0,
            scroll_col: 0,
        }
    }

    /// Clamp the cursor into `(rows, cols)`, which may have shrunk since the
    /// last frame (a load finishing short, a source being replaced).
    pub fn clamp(&mut self, rows: usize, cols: usize) {
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
        self.scroll_row = self.scroll_row.min(self.cursor_row);
        self.scroll_col = self.scroll_col.min(self.cursor_col);
    }
}

impl Default for TableState {
    fn default() -> Self {
        Self::new()
    }
}
