//! What a column *is*, computed by reading it.
//!
//! The grid answers "what is in this cell"; a column summary answers the
//! question you actually have when a dataset is new — how many values are
//! missing, what range they cover, whether the distribution is skewed, which
//! categories dominate.
//!
//! Pure Rust over [`TableSource`], and strictly read-only: a summary is one
//! scan of what the source already has.  For a source that holds only a window
//! of its rows, that means the statistics cover [`loaded_rows`] and no more —
//! [`ColumnSummary::rows`] records how many, so the caller can say so rather
//! than implying the number is the whole dataset.
//!
//! [`loaded_rows`]: TableSource::loaded_rows

use std::collections::HashMap;

use super::TableSource;

/// Bins in a summary's histogram.
///
/// Fixed rather than "one per drawn column" so a summary can be computed once
/// and cached: the header sparkline resamples it to whatever width the column
/// happens to have ([`sparkline`]).
pub const HIST_BINS: usize = 32;

/// Distinct values kept in a summary's frequency table.  The full table is a
/// separate view (`:column-frequency`), so this is only the head of it.
const TOP_N: usize = 8;

/// The eight partial-block glyphs, ascending.  Index 0 is the shortest bar that
/// is still visible — an empty bin draws a space instead, so "nothing here" and
/// "a little here" don't look the same.
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Statistics for one column.
///
/// Numeric fields are `None` for a non-numeric column, and `top` is empty for a
/// numeric one: the interesting summary of a category column is which values
/// recur, and of a measurement column, its distribution.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColumnSummary {
    /// Rows scanned — **not** necessarily the dataset's row count.
    pub rows: usize,
    /// Rows whose value was empty (or all whitespace).
    pub nulls: usize,
    /// Distinct non-null values.
    pub distinct: usize,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Arithmetic mean of the non-null numeric values.
    pub mean: Option<f64>,
    /// `[p0, p25, p50, p75, p100]`, linearly interpolated between the two
    /// nearest ranks — the same convention as numpy/pandas, so a value here
    /// matches what `df.describe()` would print.
    pub quantiles: Option<[f64; 5]>,
    /// Counts per equal-width bin across `[min, max]`.  Empty for a non-numeric
    /// column, or when every value is identical (there is no range to bin).
    pub hist: Vec<u32>,
    /// The most common values, most frequent first.  Empty for a numeric column.
    pub top: Vec<(String, usize)>,
}

impl ColumnSummary {
    /// Non-null values scanned.
    pub fn present(&self) -> usize {
        self.rows.saturating_sub(self.nulls)
    }

    /// True when there is a distribution worth drawing.
    pub fn has_distribution(&self) -> bool {
        self.hist.iter().any(|&c| c > 0)
    }
}

/// Summarise column `col` of `source`, reading at most `max_rows` rows.
///
/// One pass to collect, then a sort for the quantiles.  Values are read through
/// [`TableSource::cell`], so a windowed source contributes only its loaded rows
/// (see the module docs).
///
/// The cap exists because the header sparkline needs a summary per *visible*
/// column: on a very large table an uncapped scan of eight columns is a visible
/// stall on the first frame.  [`ColumnSummary::rows`] records what was actually
/// read, so a capped summary can say so instead of overstating its reach.
pub fn summarize(source: &dyn TableSource, col: usize, max_rows: usize) -> ColumnSummary {
    let rows = source.loaded_rows().min(max_rows);
    let numeric = source
        .columns()
        .get(col)
        .is_some_and(|c| c.ty.is_numeric());

    let mut out = ColumnSummary { rows, ..Default::default() };
    let mut values: Vec<f64> = Vec::new();
    let mut counts: HashMap<&str, usize> = HashMap::new();

    for row in 0..rows {
        let raw = source.cell(row, col).unwrap_or("").trim();
        if raw.is_empty() {
            out.nulls += 1;
            continue;
        }
        *counts.entry(raw).or_insert(0) += 1;
        if numeric {
            // A stray unparseable value in a numeric column is counted as
            // present (it is not missing) but contributes no statistic.
            if let Ok(v) = raw.parse::<f64>() {
                if v.is_finite() {
                    values.push(v);
                }
            }
        }
    }
    out.distinct = counts.len();

    if numeric {
        if !values.is_empty() {
            values.sort_unstable_by(|a, b| a.total_cmp(b));
            out.min = values.first().copied();
            out.max = values.last().copied();
            out.mean = Some(values.iter().sum::<f64>() / values.len() as f64);
            out.quantiles = Some([
                quantile(&values, 0.0),
                quantile(&values, 0.25),
                quantile(&values, 0.5),
                quantile(&values, 0.75),
                quantile(&values, 1.0),
            ]);
            out.hist = histogram(&values, HIST_BINS);
        }
    } else {
        out.top = top_values(&counts, TOP_N);
    }
    out
}

/// Every distinct value in column `col` with its count, most frequent first
/// (ties broken alphabetically, so the order is stable across runs).
///
/// The whole table rather than [`ColumnSummary::top`]'s head — this is what
/// backs the frequency view.  Reads at most `max_rows` rows, for the same reason
/// [`summarize`] does.
pub fn frequency(source: &dyn TableSource, col: usize, max_rows: usize) -> Vec<(String, usize)> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for row in 0..source.loaded_rows().min(max_rows) {
        let raw = source.cell(row, col).unwrap_or("").trim();
        *counts.entry(raw).or_insert(0) += 1;
    }
    top_values(&counts, usize::MAX)
}

/// The `n` most frequent entries, count-descending then value-ascending.
fn top_values(counts: &HashMap<&str, usize>, n: usize) -> Vec<(String, usize)> {
    let mut all: Vec<(&str, usize)> = counts.iter().map(|(k, v)| (*k, *v)).collect();
    all.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    all.truncate(n);
    all.into_iter().map(|(v, c)| (v.to_string(), c)).collect()
}

/// The `q`-quantile of a **sorted** slice, interpolating between ranks.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

/// Counts per equal-width bin across the data's range.
///
/// Empty when every value is identical: a single-valued column has no range to
/// spread over, and a full-height single bar would suggest a spread it doesn't
/// have.
fn histogram(sorted: &[f64], bins: usize) -> Vec<u32> {
    let (Some(&min), Some(&max)) = (sorted.first(), sorted.last()) else {
        return Vec::new();
    };
    let span = max - min;
    if bins == 0 || span <= 0.0 {
        return Vec::new();
    }
    let mut hist = vec![0u32; bins];
    for &v in sorted {
        // The top of the range belongs to the last bin, not one past it.
        let idx = (((v - min) / span) * bins as f64).floor() as usize;
        hist[idx.min(bins - 1)] += 1;
    }
    hist
}

/// Render `hist` as `width` block glyphs, resampling to fit.
///
/// Each output column takes the **max** of the bins that fall in it: a spike
/// narrower than one screen column should still be visible, which is the point
/// of the glyph row.  An empty bin is a space, so gaps in the distribution read
/// as gaps.
pub fn sparkline(hist: &[u32], width: usize) -> String {
    if hist.is_empty() || width == 0 {
        return String::new();
    }
    let peak = hist.iter().copied().max().unwrap_or(0);
    if peak == 0 {
        return String::new();
    }
    (0..width)
        .map(|i| {
            let from = i * hist.len() / width;
            let to = (((i + 1) * hist.len()) / width).max(from + 1).min(hist.len());
            let bucket = hist[from..to].iter().copied().max().unwrap_or(0);
            if bucket == 0 {
                return ' ';
            }
            // Scale into 1..=8 so any non-empty bin draws at least the shortest
            // bar rather than disappearing.
            let level = (bucket as u64 * BARS.len() as u64).div_ceil(peak as u64) as usize;
            BARS[level.clamp(1, BARS.len()) - 1]
        })
        .collect()
}

/// Format a statistic for display: integral values without a decimal point,
/// everything else to a few significant places.
pub fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return "—".to_string();
    }
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let mag = v.abs();
    if !(1e-4..1e6).contains(&mag) {
        format!("{v:.3e}")
    } else {
        format!("{v:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::MemSource;

    fn src(header: &[&str], rows: &[&[&str]]) -> MemSource {
        MemSource::new(header, rows)
    }

    #[test]
    fn numeric_column_gets_range_mean_and_quantiles() {
        let s = src(&["n"], &[&["1"], &["2"], &["3"], &["4"]]);
        let sum = summarize(&s, 0, usize::MAX);
        assert_eq!(sum.rows, 4);
        assert_eq!(sum.nulls, 0);
        assert_eq!(sum.distinct, 4);
        assert_eq!(sum.min, Some(1.0));
        assert_eq!(sum.max, Some(4.0));
        assert_eq!(sum.mean, Some(2.5));
        // Interpolated ranks, matching numpy/pandas defaults.
        assert_eq!(sum.quantiles, Some([1.0, 1.75, 2.5, 3.25, 4.0]));
        // A measurement column is summarised by its shape, not its top values.
        assert!(sum.top.is_empty());
        assert!(sum.has_distribution());
    }

    #[test]
    fn quantile_edges_are_the_extremes_and_the_median_is_centred() {
        // Odd count: the median is an actual element.
        let s = src(&["n"], &[&["10"], &["20"], &["30"]]);
        let q = summarize(&s, 0, usize::MAX).quantiles.unwrap();
        assert_eq!(q[0], 10.0, "p0 is the minimum");
        assert_eq!(q[2], 20.0);
        assert_eq!(q[4], 30.0, "p100 is the maximum");

        // A single value: every quantile is that value, with no NaN anywhere.
        let s = src(&["n"], &[&["7"]]);
        let sum = summarize(&s, 0, usize::MAX);
        assert_eq!(sum.quantiles, Some([7.0; 5]));
        assert_eq!(sum.mean, Some(7.0));
        // ...but no distribution: one value spans no range to bin.
        assert!(sum.hist.is_empty());
        assert!(!sum.has_distribution());
    }

    #[test]
    fn an_all_null_column_reports_nothing_rather_than_zero() {
        // The distinction matters: "min is 0" and "there are no values" are
        // very different readings of a column.
        let s = src(&["n", "x"], &[&["", "1"], &["  ", "2"]]);
        let sum = summarize(&s, 0, usize::MAX);
        assert_eq!(sum.rows, 2);
        assert_eq!(sum.nulls, 2);
        assert_eq!(sum.present(), 0);
        assert_eq!(sum.distinct, 0);
        assert_eq!(sum.min, None);
        assert_eq!(sum.max, None);
        assert_eq!(sum.mean, None);
        assert_eq!(sum.quantiles, None);
        assert!(sum.hist.is_empty());
    }

    #[test]
    fn nulls_are_excluded_from_the_statistics_but_counted() {
        let s = src(&["n"], &[&["1"], &[""], &["3"]]);
        let sum = summarize(&s, 0, usize::MAX);
        assert_eq!(sum.nulls, 1);
        assert_eq!(sum.present(), 2);
        assert_eq!(sum.mean, Some(2.0), "the empty row must not count as a 0");
    }

    #[test]
    fn a_text_column_gets_a_frequency_table_not_a_histogram() {
        let s = src(&["city"], &[&["oslo"], &["lima"], &["oslo"], &["oslo"], &["bern"]]);
        let sum = summarize(&s, 0, usize::MAX);
        assert_eq!(sum.distinct, 3);
        assert_eq!(sum.top[0], ("oslo".to_string(), 3));
        // Ties break alphabetically, so the order is stable run to run.
        assert_eq!(sum.top[1], ("bern".to_string(), 1));
        assert_eq!(sum.top[2], ("lima".to_string(), 1));
        assert!(sum.hist.is_empty());
        assert_eq!(sum.min, None);
    }

    #[test]
    fn the_top_list_is_only_a_head_and_frequency_is_the_whole_table() {
        let rows: Vec<Vec<String>> = (0..20).map(|i| vec![format!("v{i:02}")]).collect();
        let refs: Vec<Vec<&str>> = rows.iter().map(|r| r.iter().map(String::as_str).collect()).collect();
        let slices: Vec<&[&str]> = refs.iter().map(Vec::as_slice).collect();
        let s = src(&["v"], &slices);

        assert_eq!(summarize(&s, 0, usize::MAX).top.len(), TOP_N);
        assert_eq!(frequency(&s, 0, usize::MAX).len(), 20, "the frequency view shows every value");
    }

    #[test]
    fn frequency_counts_blanks_too() {
        // In a frequency view, "how many are missing" is one of the rows the
        // user is looking for, so blanks are a category rather than skipped.
        let s = src(&["x"], &[&["a"], &[""], &[""]]);
        let freq = frequency(&s, 0, usize::MAX);
        assert_eq!(freq[0], (String::new(), 2));
        assert_eq!(freq[1], ("a".to_string(), 1));
    }

    #[test]
    fn histogram_bins_span_the_range_and_include_the_maximum() {
        let s = src(&["n"], &[&["0"], &["5"], &["10"]]);
        let hist = summarize(&s, 0, usize::MAX).hist;
        assert_eq!(hist.len(), HIST_BINS);
        assert_eq!(hist.iter().sum::<u32>(), 3, "every value lands in exactly one bin");
        assert_eq!(hist[0], 1);
        assert_eq!(*hist.last().unwrap(), 1, "the maximum belongs to the last bin");
    }

    #[test]
    fn sparkline_fits_its_width_and_shows_gaps_as_gaps() {
        let line = sparkline(&[8, 0, 4, 1], 4);
        assert_eq!(line.chars().count(), 4);
        assert_eq!(line.chars().next(), Some('█'), "the peak is full height");
        assert_eq!(line.chars().nth(1), Some(' '), "an empty bin is blank");
        // A tiny-but-nonzero bin still draws.
        assert_ne!(line.chars().nth(3), Some(' '));

        // Resampling down keeps a narrow spike visible rather than averaging it
        // away — the whole point of the glyph row.
        let mut hist = vec![0u32; 32];
        hist[17] = 50;
        let narrow = sparkline(&hist, 8);
        assert_eq!(narrow.chars().count(), 8);
        assert!(narrow.contains('█'));

        // Degenerate inputs are empty, not panics.
        assert_eq!(sparkline(&[], 8), "");
        assert_eq!(sparkline(&[1, 2], 0), "");
        assert_eq!(sparkline(&[0, 0], 4), "");
    }

    #[test]
    fn numbers_are_formatted_without_noise() {
        assert_eq!(fmt_num(42.0), "42");
        assert_eq!(fmt_num(-7.0), "-7");
        assert_eq!(fmt_num(2.5), "2.5000");
        assert_eq!(fmt_num(f64::NAN), "—");
        // A whole number stays digits however large — `1230000000` reads better
        // than `1.230e9`.  Scientific notation is for the ones that need it.
        assert_eq!(fmt_num(1.23e9), "1230000000");
        assert!(fmt_num(1234567.89).contains('e'));
        assert!(fmt_num(0.000_012_3).contains('e'));
    }
}
