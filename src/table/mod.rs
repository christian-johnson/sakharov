//! Tabular-data view: a source-agnostic grid over rectangular data.
//!
//! The layer is split so that a new kind of data only has to implement
//! [`TableSource`]:
//!
//! * [`TableSource`] — the data: columns, row count, cell text.  CSV today
//!   ([`csv::CsvSource`]); a SQL table or parquet file later.
//! * [`state::TableState`] — where the cursor and viewport are.  Owned by the
//!   session, never by the source, so switching sources keeps the UI honest.
//! * [`layout`] — the *single* geometry model: column widths, which columns are
//!   on screen, and how a value is truncated to fit.  The renderer and the
//!   scroll math both derive geometry from here, exactly as the notebook view's
//!   renderer and scroll math share `notebook_ui::nb_cell_height`.  If they
//!   ever compute it separately, the cursor and the grid drift apart.
//! * [`crate::table_ui`] — drawing; [`crate::exec::table`] — mutation.

pub mod csv;
pub mod layout;
pub mod state;

pub use state::TableState;

/// Inferred value type of a column.
///
/// Inferred by sampling rather than declared, since a CSV carries no schema.
/// Drives right-alignment of numerics (and, later, sort order) — reading a
/// column of numbers whose digits don't line up is materially harder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Int,
    Float,
    Bool,
    Text,
}

impl ColumnType {
    /// True for the types that should be drawn right-aligned.
    pub fn is_numeric(self) -> bool {
        matches!(self, Self::Int | Self::Float)
    }
}

/// One column of a [`TableSource`].
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    /// Widest display width seen across the sampled values (and the header).
    /// A *hint*: [`layout::column_width`] clamps it to the configured bounds.
    pub width_hint: usize,
}

/// A rectangular, row-addressable dataset the table view can render.
///
/// Cell access is by `(row, col)` with rows fetched in windows: the view calls
/// [`ensure_rows`](TableSource::ensure_rows) with the visible range before
/// drawing, so a source that cannot hold the whole dataset (a SQL query, a
/// lazily-indexed CSV) can materialise just that window and still hand out
/// borrowed strings from it.
pub trait TableSource {
    fn columns(&self) -> &[Column];

    /// Total rows, or `None` while a background load is still counting.
    fn row_count(&self) -> Option<usize>;

    /// Rows available to [`cell`](TableSource::cell) right now.  Equals
    /// `row_count()` once a load has finished; less while it is streaming in.
    fn loaded_rows(&self) -> usize;

    /// Cell text, or `None` when `(row, col)` is outside the loaded data.
    fn cell(&self, row: usize, col: usize) -> Option<&str>;

    /// Ensure `rows` are available to `cell()`.  Called once per frame with the
    /// window about to be drawn.  The default is a no-op, which is correct for
    /// any source that holds every row in memory.
    fn ensure_rows(&mut self, _rows: std::ops::Range<usize>) {}

    /// Short human description for the status line (e.g. `"12,043 rows × 8 cols"`).
    fn describe(&self) -> String;
}

/// Infer a column's type from sampled values.
///
/// Empty values are ignored (they carry no type information and are drawn as
/// `table.null_display`); a column with no non-empty sample is [`Text`].
/// The order is narrowest-first: a column of `1`/`0` is `Int`, not `Bool`.
///
/// [`Text`]: ColumnType::Text
pub fn infer_type<'a>(values: impl Iterator<Item = &'a str>) -> ColumnType {
    let mut seen = false;
    let mut all_int = true;
    let mut all_float = true;
    let mut all_bool = true;

    for v in values {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        seen = true;
        all_int &= v.parse::<i64>().is_ok();
        all_float &= v.parse::<f64>().is_ok();
        all_bool &= matches!(
            v.to_ascii_lowercase().as_str(),
            "true" | "false" | "t" | "f" | "yes" | "no"
        );
        if !(all_int || all_float || all_bool) {
            return ColumnType::Text;
        }
    }

    if !seen {
        ColumnType::Text
    } else if all_int {
        ColumnType::Int
    } else if all_float {
        ColumnType::Float
    } else if all_bool {
        ColumnType::Bool
    } else {
        ColumnType::Text
    }
}

/// An in-memory [`TableSource`] over literal rows, for tests.
///
/// Lets the layout/renderer tests state their fixtures as a grid of strings
/// without going through a parser or the filesystem.
#[cfg(test)]
pub(crate) struct VecSource {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
}

#[cfg(test)]
impl VecSource {
    /// Build from a header row followed by data rows.
    pub fn new(header: &[&str], rows: &[&[&str]]) -> Self {
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(|c| (*c).to_string()).collect())
            .collect();
        let columns = header
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let width_hint = std::iter::once(*name)
                    .chain(rows.iter().filter_map(|r| r.get(i).map(String::as_str)))
                    .map(layout::display_width)
                    .max()
                    .unwrap_or(0);
                Column {
                    name: (*name).to_string(),
                    ty: infer_type(rows.iter().filter_map(|r| r.get(i).map(String::as_str))),
                    width_hint,
                }
            })
            .collect();
        Self { columns, rows }
    }
}

#[cfg(test)]
impl TableSource for VecSource {
    fn columns(&self) -> &[Column] {
        &self.columns
    }
    fn row_count(&self) -> Option<usize> {
        Some(self.rows.len())
    }
    fn loaded_rows(&self) -> usize {
        self.rows.len()
    }
    fn cell(&self, row: usize, col: usize) -> Option<&str> {
        self.rows.get(row)?.get(col).map(String::as_str)
    }
    fn describe(&self) -> String {
        format!("{} rows", self.rows.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_int_float_bool_and_text() {
        assert_eq!(infer_type(["1", "-2", "300"].into_iter()), ColumnType::Int);
        assert_eq!(infer_type(["1.5", "2", "-0.3"].into_iter()), ColumnType::Float);
        assert_eq!(infer_type(["true", "FALSE", "Yes"].into_iter()), ColumnType::Bool);
        assert_eq!(infer_type(["1", "abc"].into_iter()), ColumnType::Text);
    }

    #[test]
    fn empty_values_carry_no_type_information() {
        // Blanks are skipped, so a mostly-empty numeric column stays numeric...
        assert_eq!(infer_type(["", "42", ""].into_iter()), ColumnType::Int);
        // ...and an entirely empty column is Text (nothing says otherwise).
        assert_eq!(infer_type(["", ""].into_iter()), ColumnType::Text);
        assert_eq!(infer_type(std::iter::empty()), ColumnType::Text);
    }

    #[test]
    fn zero_one_column_is_int_not_bool() {
        // Narrowest-first: numeric alignment matters more than a bool label,
        // and `0`/`1` is far more often a count or flag-as-number than a bool.
        assert_eq!(infer_type(["0", "1", "1"].into_iter()), ColumnType::Int);
    }

    #[test]
    fn numeric_types_are_right_aligned() {
        assert!(ColumnType::Int.is_numeric());
        assert!(ColumnType::Float.is_numeric());
        assert!(!ColumnType::Bool.is_numeric());
        assert!(!ColumnType::Text.is_numeric());
    }
}
