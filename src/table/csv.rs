//! CSV/TSV [`TableSource`]: the first backend for the table view.
//!
//! Parsing is delegated to the `csv` crate — quoted fields, embedded newlines,
//! doubled quotes, CRLF and ragged rows are all its business, and a hand-rolled
//! RFC 4180 parser would only differ in the cases that matter.
//!
//! Loading is bounded (`table.max_rows`) and reports truncation rather than
//! silently showing a prefix of the file, and the whole thing is driven from a
//! background thread by [`crate::exec::table`], so a huge file never blocks a
//! frame.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::TableConfig;

use super::{infer_type, layout::display_width, Column, TableSource};

/// Delimiters considered when sniffing an unknown file.
const CANDIDATE_DELIMITERS: &[u8; 4] = b",\t;|";

/// A fully-loaded delimited-text table.
pub struct CsvSource {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
    /// True when the load stopped at `table.max_rows` before end of file.
    truncated: bool,
    /// The delimiter actually used (reported in the status line, since a
    /// wrong guess is otherwise a baffling single-column table).
    delimiter: u8,
}

/// The delimiter to parse `path` with: tab for `.tsv`/`.tab`, otherwise
/// whichever candidate appears most often in the first line.
///
/// Sniffing beats defaulting to a comma: a semicolon-delimited export opened as
/// one 40-column-wide text column is the single most common way a CSV viewer
/// looks broken.
pub fn sniff_delimiter(path: &Path, first_line: &str) -> u8 {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if matches!(ext.as_deref(), Some("tsv") | Some("tab")) {
        return b'\t';
    }
    // Count outside quoted regions so `"a,b";1` doesn't vote for a comma.
    let mut counts = [0usize; CANDIDATE_DELIMITERS.len()];
    let mut in_quotes = false;
    for b in first_line.bytes() {
        match b {
            b'"' => in_quotes = !in_quotes,
            _ if in_quotes => {}
            _ => {
                if let Some(i) = CANDIDATE_DELIMITERS.iter().position(|&d| d == b) {
                    counts[i] += 1;
                }
            }
        }
    }
    counts
        .iter()
        .enumerate()
        .max_by_key(|(i, &n)| (n, std::cmp::Reverse(*i)))
        .filter(|(_, &n)| n > 0)
        .map(|(i, _)| CANDIDATE_DELIMITERS[i])
        .unwrap_or(b',')
}

impl CsvSource {
    /// Load `path` in full (bounded by `cfg.max_rows`).
    pub fn load(path: &Path, cfg: &TableConfig) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        // A UTF-8 BOM would otherwise become part of the first header name.
        let bytes = bytes
            .strip_prefix(&[0xEF, 0xBB, 0xBF])
            .unwrap_or(&bytes)
            .to_vec();
        let first_line: String = String::from_utf8_lossy(&bytes)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let delimiter = sniff_delimiter(path, &first_line);
        Self::from_reader(&bytes[..], delimiter, cfg)
    }

    /// Parse delimited text from `reader`.  The filesystem-free entry point,
    /// so the parse rules can be tested directly.
    pub fn from_reader<R: Read>(reader: R, delimiter: u8, cfg: &TableConfig) -> Result<Self> {
        let mut rdr = ::csv::ReaderBuilder::new()
            .delimiter(delimiter)
            // Ragged rows are padded/trimmed below rather than being an error:
            // a malformed row late in a big file must not cost you the view.
            .flexible(true)
            .has_headers(true)
            .from_reader(reader);

        let headers: Vec<String> = rdr
            .headers()
            .context("reading header row")?
            .iter()
            .map(str::to_string)
            .collect();
        let n_cols = headers.len();

        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut truncated = false;
        for record in rdr.records() {
            if rows.len() >= cfg.max_rows {
                truncated = true;
                break;
            }
            let record = match record {
                Ok(r) => r,
                // Skip an unparseable record rather than abandoning the file.
                Err(_) => continue,
            };
            let mut row: Vec<String> = record.iter().map(str::to_string).collect();
            // Normalise to the header's column count so every row has the same
            // geometry — the renderer and cursor address cells by index.
            row.resize(n_cols, String::new());
            rows.push(row);
        }

        let columns = headers
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let sample = || {
                    rows.iter()
                        .take(cfg.sample_rows)
                        .filter_map(|r| r.get(i).map(String::as_str))
                };
                let width_hint = std::iter::once(display_width(&name))
                    .chain(sample().map(display_width))
                    .max()
                    .unwrap_or(0);
                Column {
                    ty: infer_type(sample()),
                    name,
                    width_hint,
                }
            })
            .collect();

        Ok(Self {
            columns,
            rows,
            truncated,
            delimiter,
        })
    }

    /// True when the load stopped early at `table.max_rows`.
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

impl TableSource for CsvSource {
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
        let delim = match self.delimiter {
            b'\t' => "tab".to_string(),
            d => (d as char).to_string(),
        };
        format!(
            "{} row{} × {} col{}{}  ·  '{delim}'",
            self.rows.len(),
            if self.rows.len() == 1 { "" } else { "s" },
            self.columns.len(),
            if self.columns.len() == 1 { "" } else { "s" },
            if self.truncated { " (truncated)" } else { "" },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::ColumnType;

    fn parse(text: &str) -> CsvSource {
        CsvSource::from_reader(text.as_bytes(), b',', &TableConfig::default()).expect("parse")
    }

    #[test]
    fn reads_headers_and_rows() {
        let src = parse("a,b\n1,2\n3,4\n");
        assert_eq!(
            src.columns().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(src.row_count(), Some(2));
        assert_eq!(src.cell(0, 0), Some("1"));
        assert_eq!(src.cell(1, 1), Some("4"));
        assert_eq!(src.cell(2, 0), None, "past the last row");
        assert_eq!(src.cell(0, 9), None, "past the last column");
    }

    #[test]
    fn quoted_fields_keep_commas_newlines_and_quotes() {
        let src = parse("a,b\n\"x,y\",\"line1\nline2\"\n\"say \"\"hi\"\"\",z\n");
        assert_eq!(src.cell(0, 0), Some("x,y"));
        assert_eq!(
            src.cell(0, 1),
            Some("line1\nline2"),
            "the embedded newline is preserved in the data; only the *display* flattens it"
        );
        assert_eq!(src.cell(1, 0), Some(r#"say "hi""#));
    }

    #[test]
    fn crlf_line_endings_do_not_leak_into_values() {
        let src = parse("a,b\r\n1,2\r\n");
        assert_eq!(src.cell(0, 1), Some("2"));
    }

    #[test]
    fn ragged_rows_are_padded_to_the_header_width() {
        // Short row gets empty cells; long row is trimmed. Every row must have
        // the same geometry or the cursor addresses cells that aren't drawn.
        let src = parse("a,b,c\n1\n1,2,3,4\n");
        assert_eq!(src.cell(0, 2), Some(""));
        assert_eq!(src.cell(1, 2), Some("3"));
        for row in 0..src.loaded_rows() {
            assert!(src.cell(row, src.columns().len() - 1).is_some());
            assert_eq!(src.cell(row, src.columns().len()), None);
        }
    }

    #[test]
    fn empty_file_and_header_only_file_are_not_errors() {
        let src = parse("a,b\n");
        assert_eq!(src.row_count(), Some(0));
        assert_eq!(src.columns().len(), 2);

        let src = parse("");
        assert_eq!(src.row_count(), Some(0));
        assert!(src.columns().is_empty());
    }

    #[test]
    fn load_stops_at_max_rows_and_reports_truncation() {
        let text = format!("a\n{}", "1\n".repeat(50));
        let cfg = TableConfig {
            max_rows: 10,
            ..Default::default()
        };
        let src = CsvSource::from_reader(text.as_bytes(), b',', &cfg).unwrap();
        assert_eq!(src.loaded_rows(), 10);
        assert!(src.truncated());
        assert!(src.describe().contains("truncated"));
    }

    #[test]
    fn column_types_and_widths_come_from_the_sample() {
        let src = parse("n,label\n1,short\n22,a much longer value\n");
        assert_eq!(src.columns()[0].ty, ColumnType::Int);
        assert_eq!(src.columns()[1].ty, ColumnType::Text);
        assert_eq!(src.columns()[0].width_hint, 2);
        assert_eq!(src.columns()[1].width_hint, "a much longer value".len());
    }

    #[test]
    fn width_hint_of_a_multiline_value_counts_its_flattened_form() {
        // The hint drives column width, so it must measure what gets drawn
        // (one row, break glyphs) not the raw value's longest line.
        let src = parse("a\n\"one\ntwo\"\n");
        assert_eq!(src.columns()[0].width_hint, display_width("one↵two"));
    }

    #[test]
    fn sample_rows_bounds_the_type_and_width_scan() {
        // Values past sample_rows don't affect inference — that's the point of
        // sampling — but they must still load and be addressable.
        let cfg = TableConfig {
            sample_rows: 2,
            ..Default::default()
        };
        let text = "a\n1\n2\nnot-a-number\n";
        let src = CsvSource::from_reader(text.as_bytes(), b',', &cfg).unwrap();
        assert_eq!(src.columns()[0].ty, ColumnType::Int);
        assert_eq!(src.cell(2, 0), Some("not-a-number"));
    }

    // --- delimiter sniffing ----------------------------------------------

    #[test]
    fn tsv_extension_forces_tab() {
        assert_eq!(sniff_delimiter(Path::new("x.tsv"), "a,b,c"), b'\t');
        assert_eq!(sniff_delimiter(Path::new("x.TSV"), "a,b,c"), b'\t');
    }

    #[test]
    fn delimiter_is_sniffed_from_the_header_line() {
        assert_eq!(sniff_delimiter(Path::new("x.csv"), "a,b,c"), b',');
        assert_eq!(sniff_delimiter(Path::new("x.csv"), "a;b;c"), b';');
        assert_eq!(sniff_delimiter(Path::new("x.csv"), "a|b|c"), b'|');
        assert_eq!(sniff_delimiter(Path::new("x.txt"), "a\tb\tc"), b'\t');
        // A single column has nothing to sniff — comma is the harmless default.
        assert_eq!(sniff_delimiter(Path::new("x.csv"), "only"), b',');
    }

    #[test]
    fn delimiters_inside_quotes_do_not_vote() {
        // One real `;` separator beats two commas inside a quoted field.
        assert_eq!(sniff_delimiter(Path::new("x.csv"), "\"a,b,c\";d"), b';');
    }

    #[test]
    fn a_semicolon_file_parses_as_columns_not_one_wide_column() {
        let src = CsvSource::from_reader(
            "a;b\n1;2\n".as_bytes(),
            sniff_delimiter(Path::new("t.csv"), "a;b"),
            &TableConfig::default(),
        )
        .unwrap();
        assert_eq!(src.columns().len(), 2);
        assert_eq!(src.cell(0, 1), Some("2"));
    }
}
