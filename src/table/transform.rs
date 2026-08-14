//! Sort, filter and group — expressed once, executed two different ways.
//!
//! A [`Transform`] is a *description* of a derived table, not a mutation.  It is
//! applied in one of two ways, and which one is invisible to everything above:
//!
//! * **Pushed down**, when the source can satisfy it natively —
//!   [`TableSource::derive`].  A [`DuckDbSource`](super::duck::DuckDbSource)
//!   wraps its query in a subquery, so a filter over a hundred-million-row
//!   parquet file is executed by the engine and only the window on screen is
//!   ever read.
//! * **Locally**, by [`apply_local`], which materialises the result as a
//!   [`MemSource`].  The fallback for a source that holds its rows anyway (a
//!   parsed CSV, a computed table), where a scan *is* the cheap answer.
//!
//! Both paths must agree cell for cell, or the grid quietly lies about filtered
//! data — the property `pushdown_and_local_execution_agree` exists to pin it.
//!
//! **Read-only by construction:** every function here takes `&dyn TableSource`
//! and returns a *new* source.  Nothing a transform can express reaches the
//! parent, and the SQL a pushdown generates goes through the same
//! [`gate`](super::duck::gate) as a query the user typed.
//!
//! Typed access is deliberately *not* a trait method.  Comparison needs one
//! question answered — "is this column a number?" — which [`Column::ty`] already
//! answers, so the ordering rules live in [`compare`] rather than in a `Value`
//! enum every backend would have to marshal into.

use std::cmp::Ordering;

use super::{Column, ColumnType, MemSource, TableSource};

/// How a group's rows are reduced to one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Count,
    Sum,
    Mean,
    Min,
    Max,
}

impl Agg {
    /// Name in the derived table's header, and in `:group`'s own syntax.
    pub fn name(self) -> &'static str {
        match self {
            Agg::Count => "count",
            Agg::Sum => "sum",
            Agg::Mean => "mean",
            Agg::Min => "min",
            Agg::Max => "max",
        }
    }

    /// The SQL function that computes it.
    fn sql(self) -> &'static str {
        match self {
            Agg::Count => "count",
            Agg::Sum => "sum",
            // SQL spells the arithmetic mean `avg`; the UI spells it the way
            // dataframe libraries do.
            Agg::Mean => "avg",
            Agg::Min => "min",
            Agg::Max => "max",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "count" | "n" => Agg::Count,
            "sum" | "total" => Agg::Sum,
            "mean" | "avg" | "average" => Agg::Mean,
            "min" => Agg::Min,
            "max" => Agg::Max,
            _ => return None,
        })
    }
}

/// A test one cell has to pass for its row to survive a [`Transform::Filter`].
///
/// A closed set rather than an expression language: the predicate is compiled
/// into SQL for the pushdown path, and a set of shapes that can be *generated*
/// is the difference between a filter and a place for a query to be injected.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Gt(f64),
    Ge(f64),
    Lt(f64),
    Le(f64),
    /// String equality against the cell's display text.
    Eq(String),
    Ne(String),
    /// Case-insensitive substring.
    Contains(String),
    IsNull,
    NotNull,
}

impl Predicate {
    /// Parse the `gf` prompt's little syntax: an operator and a value.
    ///
    /// `> 100`, `>=100`, `= oslo`, `!= oslo`, `~ osl`, `null`, `!null`.  The
    /// column is the one under the cursor, so it is never named here — which is
    /// also why this can't grow into arbitrary SQL.
    pub fn parse(input: &str) -> Result<Self, String> {
        let s = input.trim();
        if s.is_empty() {
            return Err("Nothing to filter by".to_string());
        }
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "null" | "is null" | "empty" => return Ok(Predicate::IsNull),
            "!null" | "not null" | "is not null" => return Ok(Predicate::NotNull),
            _ => {}
        }
        // Longest operators first, or `>=` parses as `>` with a value of `=100`.
        for (op, build) in [
            (">=", &Self::numeric_ge as &dyn Fn(f64) -> Predicate),
            ("<=", &Self::numeric_le),
            (">", &Self::numeric_gt),
            ("<", &Self::numeric_lt),
        ] {
            if let Some(rest) = s.strip_prefix(op) {
                let rest = rest.trim();
                let n: f64 = rest
                    .parse()
                    .map_err(|_| format!("`{rest}` is not a number"))?;
                return Ok(build(n));
            }
        }
        for (op, build) in [
            ("!=", &Self::ne as &dyn Fn(&str) -> Predicate),
            ("==", &Self::eq),
            ("=", &Self::eq),
            ("~", &Self::contains),
        ] {
            if let Some(rest) = s.strip_prefix(op) {
                return Ok(build(rest.trim()));
            }
        }
        // A bare word is the common case ("show me the oslo rows"), and
        // `contains` is the forgiving reading of it.
        Ok(Predicate::Contains(s.to_string()))
    }

    fn numeric_gt(n: f64) -> Predicate { Predicate::Gt(n) }
    fn numeric_ge(n: f64) -> Predicate { Predicate::Ge(n) }
    fn numeric_lt(n: f64) -> Predicate { Predicate::Lt(n) }
    fn numeric_le(n: f64) -> Predicate { Predicate::Le(n) }
    fn eq(s: &str) -> Predicate { Predicate::Eq(s.to_string()) }
    fn ne(s: &str) -> Predicate { Predicate::Ne(s.to_string()) }
    fn contains(s: &str) -> Predicate { Predicate::Contains(s.to_string()) }

    /// Does `cell` pass?  An absent cell (outside a window) never does.
    fn admits(&self, cell: &str) -> bool {
        let num = || cell.trim().parse::<f64>().ok();
        match self {
            Predicate::Gt(n) => num().is_some_and(|v| v > *n),
            Predicate::Ge(n) => num().is_some_and(|v| v >= *n),
            Predicate::Lt(n) => num().is_some_and(|v| v < *n),
            Predicate::Le(n) => num().is_some_and(|v| v <= *n),
            Predicate::Eq(s) => cell == s,
            Predicate::Ne(s) => cell != s,
            Predicate::Contains(s) => cell.to_lowercase().contains(&s.to_lowercase()),
            Predicate::IsNull => cell.is_empty(),
            Predicate::NotNull => !cell.is_empty(),
        }
    }

    /// How it reads in the status line.
    fn label(&self) -> String {
        match self {
            Predicate::Gt(n) => format!(">{n}"),
            Predicate::Ge(n) => format!(">={n}"),
            Predicate::Lt(n) => format!("<{n}"),
            Predicate::Le(n) => format!("<={n}"),
            Predicate::Eq(s) => format!("={s}"),
            Predicate::Ne(s) => format!("!={s}"),
            Predicate::Contains(s) => format!("~{s}"),
            Predicate::IsNull => "is null".to_string(),
            Predicate::NotNull => "not null".to_string(),
        }
    }

    /// The `WHERE` clause fragment for `column`, already quoted and escaped.
    ///
    /// Compared as text, matching [`admits`](Self::admits) — the grid's own
    /// reading of a cell is its display string, so `= 1` must match `1` whether
    /// the column is an integer or a label.  Numeric comparisons cast, since
    /// `'9' > '100'` as text.
    fn sql(&self, column: &str) -> String {
        let col = quote_ident(column);
        let text = format!("CAST({col} AS VARCHAR)");
        match self {
            Predicate::Gt(n) => format!("TRY_CAST({col} AS DOUBLE) > {n}"),
            Predicate::Ge(n) => format!("TRY_CAST({col} AS DOUBLE) >= {n}"),
            Predicate::Lt(n) => format!("TRY_CAST({col} AS DOUBLE) < {n}"),
            Predicate::Le(n) => format!("TRY_CAST({col} AS DOUBLE) <= {n}"),
            Predicate::Eq(s) => format!("{text} = {}", quote_str(s)),
            // A NULL compares to neither, so it must be admitted explicitly to
            // match the local path, where an absent value is the empty string.
            Predicate::Ne(s) => format!("({text} IS NULL OR {text} <> {})", quote_str(s)),
            Predicate::Contains(s) => {
                format!("contains(lower({text}), lower({}))", quote_str(s))
            }
            Predicate::IsNull => format!("({col} IS NULL OR {text} = '')"),
            Predicate::NotNull => format!("({col} IS NOT NULL AND {text} <> '')"),
        }
    }
}

/// One step of a derivation.
#[derive(Debug, Clone, PartialEq)]
pub enum Transform {
    Sort { col: usize, desc: bool },
    Filter { col: usize, pred: Predicate },
    /// Group by `keys`, reducing each group with `aggs` (a row count always
    /// comes first — it is the question a groupby is usually asked).
    GroupBy { keys: Vec<usize>, aggs: Vec<(usize, Agg)> },
}

impl Transform {
    /// How the step reads in the status line: `sort:price↓`, `filter:qty>0`.
    pub fn label(&self, columns: &[Column]) -> String {
        let name = |i: &usize| {
            columns
                .get(*i)
                .map_or_else(|| format!("col{i}"), |c| c.name.clone())
        };
        match self {
            Transform::Sort { col, desc } => {
                format!("sort:{}{}", name(col), if *desc { "\u{2193}" } else { "\u{2191}" })
            }
            Transform::Filter { col, pred } => {
                format!("filter:{}{}", name(col), pred.label())
            }
            Transform::GroupBy { keys, aggs } => {
                let by: Vec<String> = keys.iter().map(name).collect();
                let mut out = format!("group:{}", by.join(","));
                for (col, agg) in aggs {
                    out.push_str(&format!(" {}({})", agg.name(), name(col)));
                }
                out
            }
        }
    }

    /// The SQL that computes this step over `inner`, or `None` when the step
    /// names a column the source doesn't have.
    ///
    /// The one place a transform becomes a query — used by every pushdown
    /// implementation, so the two backends can't drift into different SQL.
    pub fn to_sql(&self, inner: &str, columns: &[Column]) -> Option<String> {
        let name = |i: &usize| columns.get(*i).map(|c| c.name.as_str());
        Some(match self {
            Transform::Sort { col, desc } => {
                let dir = if *desc { "DESC" } else { "ASC" };
                // Positional: a query is allowed to have two columns of the same
                // name, and the grid addresses them by position.
                format!("SELECT * FROM ({inner}) AS \"t\" ORDER BY {} {dir} NULLS LAST", col + 1)
            }
            Transform::Filter { col, pred } => {
                let clause = pred.sql(name(col)?);
                format!("SELECT * FROM ({inner}) AS \"t\" WHERE {clause}")
            }
            Transform::GroupBy { keys, aggs } => {
                let mut proj: Vec<String> = keys
                    .iter()
                    .map(|k| name(k).map(quote_ident))
                    .collect::<Option<_>>()?;
                proj.push("count(*) AS \"count\"".to_string());
                for (col, agg) in aggs {
                    let c = quote_ident(name(col)?);
                    proj.push(format!(
                        "{}({c}) AS {}",
                        agg.sql(),
                        quote_ident(&format!("{}_{}", agg.name(), name(col)?)),
                    ));
                }
                let group: Vec<String> = (1..=keys.len()).map(|i| i.to_string()).collect();
                // Biggest group first, ties broken by the key.  The tiebreak is
                // not cosmetic: without it two groups of equal size come back in
                // whatever order the engine felt like, which the local path
                // cannot reproduce — and "the same transform, executed two ways,
                // gives the same grid" is the contract.
                let mut order: Vec<String> = vec![format!("{} DESC", keys.len() + 1)];
                order.extend(group.iter().map(|g| format!("{g} ASC NULLS LAST")));
                format!(
                    "SELECT {} FROM ({inner}) AS \"t\" GROUP BY {} ORDER BY {}",
                    proj.join(", "),
                    group.join(", "),
                    order.join(", "),
                )
            }
        })
    }
}

/// `"…"` — a quoted SQL identifier.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `'…'` — a quoted SQL string literal.
fn quote_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// Local execution
// ---------------------------------------------------------------------------

/// Every value of `row`, in column order (an absent cell reads as empty, which
/// is how the grid draws it).
fn row_values(src: &dyn TableSource, row: usize, cols: usize) -> Vec<String> {
    (0..cols)
        .map(|c| src.cell(row, c).unwrap_or_default().to_string())
        .collect()
}

/// Order two cells of a column of type `ty`.
///
/// Numeric columns compare as numbers — otherwise `9` sorts after `100` — and
/// an empty value sorts last either way, matching SQL's `NULLS LAST`.
fn compare(a: &str, b: &str, ty: ColumnType) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    if ty.is_numeric() {
        if let (Ok(x), Ok(y)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
            return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
        }
    }
    a.cmp(b)
}

/// Execute `op` by scanning `src`, up to `max_rows` rows.
///
/// The fallback path, for a source with no native answer.  It materialises,
/// which is what makes it the *wrong* answer for a windowed source and the right
/// one for a source that holds its rows anyway.
pub fn apply_local(src: &dyn TableSource, op: &Transform, max_rows: usize) -> MemSource {
    let columns = src.columns().to_vec();
    let ncols = columns.len();
    let rows = src.loaded_rows().min(max_rows);

    match op {
        Transform::Sort { col, desc } => {
            let ty = columns.get(*col).map_or(ColumnType::Text, |c| c.ty);
            let mut out: Vec<Vec<String>> =
                (0..rows).map(|r| row_values(src, r, ncols)).collect();
            // Stable, so a second sort keeps the first one's order within ties —
            // which is what makes sorting by two columns work at all.
            out.sort_by(|a, b| {
                let ord = compare(
                    a.get(*col).map_or("", String::as_str),
                    b.get(*col).map_or("", String::as_str),
                    ty,
                );
                if *desc { flip_keeping_nulls_last(ord, a, b, *col) } else { ord }
            });
            MemSource::with_columns(columns, out, describe_rows(rows))
        }
        Transform::Filter { col, pred } => {
            let out: Vec<Vec<String>> = (0..rows)
                .filter(|r| pred.admits(src.cell(*r, *col).unwrap_or_default()))
                .map(|r| row_values(src, r, ncols))
                .collect();
            let n = out.len();
            MemSource::with_columns(columns, out, describe_rows(n))
        }
        Transform::GroupBy { keys, aggs } => group_local(src, keys, aggs, rows, &columns),
    }
}

/// Reverse an ordering without promoting empties past real values — SQL's
/// `NULLS LAST` holds in both directions, and the two paths have to agree.
fn flip_keeping_nulls_last(
    ord: Ordering,
    a: &[String],
    b: &[String],
    col: usize,
) -> Ordering {
    let empty = |r: &[String]| r.get(col).map_or(true, String::is_empty);
    match (empty(a), empty(b)) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => ord.reverse(),
    }
}

fn describe_rows(n: usize) -> String {
    format!("{n} rows")
}

/// The local groupby: one pass, insertion-ordered accumulators.
fn group_local(
    src: &dyn TableSource,
    keys: &[usize],
    aggs: &[(usize, Agg)],
    rows: usize,
    columns: &[Column],
) -> MemSource {
    /// What one group accumulates.
    struct Group {
        key: Vec<String>,
        count: usize,
        /// One accumulator per requested aggregate, in `aggs` order.
        acc: Vec<Acc>,
    }
    #[derive(Default)]
    struct Acc {
        sum: f64,
        n: usize,
        min: Option<f64>,
        max: Option<f64>,
    }

    let mut order: Vec<Vec<String>> = Vec::new();
    let mut groups: std::collections::HashMap<Vec<String>, Group> = std::collections::HashMap::new();

    for r in 0..rows {
        let key: Vec<String> = keys
            .iter()
            .map(|k| src.cell(r, *k).unwrap_or_default().to_string())
            .collect();
        let group = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Group {
                key: key.clone(),
                count: 0,
                acc: aggs.iter().map(|_| Acc::default()).collect(),
            }
        });
        group.count += 1;
        for (i, (col, _)) in aggs.iter().enumerate() {
            if let Some(v) = src
                .cell(r, *col)
                .and_then(|c| c.trim().parse::<f64>().ok())
            {
                let a = &mut group.acc[i];
                a.sum += v;
                a.n += 1;
                a.min = Some(a.min.map_or(v, |m: f64| m.min(v)));
                a.max = Some(a.max.map_or(v, |m: f64| m.max(v)));
            }
        }
    }

    let mut out: Vec<Vec<String>> = order
        .iter()
        .filter_map(|k| groups.get(k))
        .map(|g| {
            let mut row = g.key.clone();
            row.push(g.count.to_string());
            for (i, (col, agg)) in aggs.iter().enumerate() {
                let a = &g.acc[i];
                let integral = columns.get(*col).is_some_and(|c| c.ty == ColumnType::Int);
                // A group with no numbers to reduce has no answer, and SQL says
                // so with NULL — which the grid draws as missing.  Reporting a
                // zero instead would invent data, and would put the two
                // execution paths at odds.
                row.push(match agg {
                    Agg::Count => a.n.to_string(),
                    _ if a.n == 0 => String::new(),
                    Agg::Sum => fmt_agg(a.sum, integral),
                    Agg::Mean => fmt_agg(a.sum / a.n as f64, false),
                    Agg::Min => a.min.map(|v| fmt_agg(v, integral)).unwrap_or_default(),
                    Agg::Max => a.max.map(|v| fmt_agg(v, integral)).unwrap_or_default(),
                });
            }
            row
        })
        .collect();
    // Biggest group first, ties broken by the key — the same total order the
    // pushed-down `ORDER BY count DESC, key ASC` produces.  Anything less than
    // total and the two execution paths disagree on equal-sized groups.
    let count_col = keys.len();
    out.sort_by(|a, b| {
        let n = |r: &Vec<String>| r[count_col].parse::<usize>().unwrap_or(0);
        n(b).cmp(&n(a)).then_with(|| {
            keys.iter().enumerate().fold(Ordering::Equal, |acc, (i, k)| {
                acc.then_with(|| {
                    let ty = columns.get(*k).map_or(ColumnType::Text, |c| c.ty);
                    compare(&a[i], &b[i], ty)
                })
            })
        })
    });

    let mut spec: Vec<(String, ColumnType)> = keys
        .iter()
        .map(|k| {
            let c = &columns[*k];
            (c.name.clone(), c.ty)
        })
        .collect();
    spec.push(("count".to_string(), ColumnType::Int));
    for (col, agg) in aggs {
        let src_name = columns.get(*col).map_or("?", |c| c.name.as_str());
        let ty = match agg {
            Agg::Count => ColumnType::Int,
            Agg::Mean => ColumnType::Float,
            _ => columns.get(*col).map_or(ColumnType::Float, |c| c.ty),
        };
        spec.push((format!("{}_{src_name}", agg.name()), ty));
    }

    let n = out.len();
    let derived = spec
        .iter()
        .enumerate()
        .map(|(i, (name, ty))| Column {
            name: name.clone(),
            ty: *ty,
            width_hint: out
                .iter()
                .filter_map(|r| r.get(i))
                .map(|v| super::layout::display_width(v))
                .chain(std::iter::once(super::layout::display_width(name)))
                .max()
                .unwrap_or(0),
        })
        .collect();
    MemSource::with_columns(derived, out, format!("{n} groups"))
}

/// Format an aggregate the way the engine's `CAST(… AS VARCHAR)` would: a sum of
/// integers is an integer, everything else keeps its decimal point.
fn fmt_agg(v: f64, integral: bool) -> String {
    if integral && v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> MemSource {
        MemSource::new(
            &["city", "qty"],
            &[
                &["oslo", "10"],
                &["lima", "3"],
                &["oslo", "7"],
                &["bern", ""],
            ],
        )
    }

    fn cells(src: &MemSource, col: usize) -> Vec<String> {
        (0..src.loaded_rows())
            .map(|r| src.cell(r, col).unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn the_filter_syntax_covers_the_questions_a_column_raises() {
        assert_eq!(Predicate::parse("> 100"), Ok(Predicate::Gt(100.0)));
        // Longest operator first, or `>=` reads as `>` with a junk value.
        assert_eq!(Predicate::parse(">=100"), Ok(Predicate::Ge(100.0)));
        assert_eq!(Predicate::parse("= oslo"), Ok(Predicate::Eq("oslo".into())));
        assert_eq!(Predicate::parse("!= oslo"), Ok(Predicate::Ne("oslo".into())));
        assert_eq!(Predicate::parse("null"), Ok(Predicate::IsNull));
        assert_eq!(Predicate::parse("not null"), Ok(Predicate::NotNull));
        // A bare word is the common case, read forgivingly.
        assert_eq!(Predicate::parse("osl"), Ok(Predicate::Contains("osl".into())));
        assert!(Predicate::parse("> abc").is_err());
        assert!(Predicate::parse("  ").is_err());
    }

    #[test]
    fn a_numeric_sort_orders_by_value_and_puts_blanks_last() {
        let src = fixture();
        let asc = apply_local(&src, &Transform::Sort { col: 1, desc: false }, usize::MAX);
        assert_eq!(cells(&asc, 1), vec!["3", "7", "10", ""]);
        // Descending reverses the values but leaves the blank at the bottom —
        // `NULLS LAST` holds in both directions, as it does in SQL.
        let desc = apply_local(&src, &Transform::Sort { col: 1, desc: true }, usize::MAX);
        assert_eq!(cells(&desc, 1), vec!["10", "7", "3", ""]);
    }

    #[test]
    fn a_text_sort_is_lexicographic() {
        let src = fixture();
        let out = apply_local(&src, &Transform::Sort { col: 0, desc: false }, usize::MAX);
        assert_eq!(cells(&out, 0), vec!["bern", "lima", "oslo", "oslo"]);
    }

    #[test]
    fn filters_keep_the_rows_that_pass_and_nothing_else() {
        let src = fixture();
        let out = apply_local(
            &src,
            &Transform::Filter { col: 1, pred: Predicate::Gt(5.0) },
            usize::MAX,
        );
        assert_eq!(cells(&out, 1), vec!["10", "7"]);
        // A blank is not a number, so it fails a numeric test rather than
        // passing as zero.
        let out = apply_local(
            &src,
            &Transform::Filter { col: 1, pred: Predicate::Lt(5.0) },
            usize::MAX,
        );
        assert_eq!(cells(&out, 1), vec!["3"]);

        let out = apply_local(
            &src,
            &Transform::Filter { col: 0, pred: Predicate::Contains("OS".into()) },
            usize::MAX,
        );
        assert_eq!(out.loaded_rows(), 2, "contains is case-insensitive");
    }

    #[test]
    fn a_groupby_counts_and_aggregates_biggest_group_first() {
        let src = fixture();
        let out = apply_local(
            &src,
            &Transform::GroupBy { keys: vec![0], aggs: vec![(1, Agg::Sum), (1, Agg::Mean)] },
            usize::MAX,
        );
        let names: Vec<&str> = out.columns().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["city", "count", "sum_qty", "mean_qty"]);
        // Biggest group first, then by key — a *total* order, so the pushed-down
        // execution can reproduce it exactly.
        assert_eq!(cells(&out, 0), vec!["oslo", "bern", "lima"]);
        assert_eq!(cells(&out, 1), vec!["2", "1", "1"]);
        assert_eq!(cells(&out, 2)[0], "17");
        assert_eq!(cells(&out, 3)[0], "8.5");
        // The all-blank group has nothing to sum: empty, not a fabricated zero
        // (which is also what SQL's `sum` of all NULLs gives).
        assert_eq!(cells(&out, 2)[1], "");
    }

    #[test]
    fn a_transform_reads_as_what_it_did() {
        let src = fixture();
        let cols = src.columns();
        assert_eq!(
            Transform::Sort { col: 1, desc: true }.label(cols),
            "sort:qty\u{2193}"
        );
        assert_eq!(
            Transform::Filter { col: 1, pred: Predicate::Gt(5.0) }.label(cols),
            "filter:qty>5"
        );
        assert_eq!(
            Transform::GroupBy { keys: vec![0], aggs: vec![(1, Agg::Sum)] }.label(cols),
            "group:city sum(qty)"
        );
    }

    #[test]
    fn a_quote_in_a_filter_value_cannot_end_the_string_literal() {
        let cols = vec![Column {
            name: "it's".to_string(),
            ty: ColumnType::Text,
            width_hint: 4,
        }];
        let sql = Transform::Filter { col: 0, pred: Predicate::Eq("o'hara".into()) }
            .to_sql("SELECT 1", &cols)
            .unwrap();
        assert!(sql.contains("\"it's\""), "identifier quoted: {sql}");
        assert!(sql.contains("'o''hara'"), "literal escaped: {sql}");
    }
}
