//! Table-view state: opening/closing a tabular data source, the background
//! load, cursor movement, and scroll.
//!
//! The table view is **read-only**.  While it is active `app.buffer` is a
//! detached empty buffer with no path, so no save path in the editor can write
//! over the data file, and the write commands are refused explicitly on top of
//! that.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::{
    app::{App, View},
    command::Command,
    source::SourceId,
    table::{self, csv::CsvSource, layout, summary::{self, ColumnSummary},
           transform::{Agg, Predicate, Transform}, TableSource},
};

/// What a row of a *catalog* table names, so `Enter` can open it.
///
/// A grid whose rows are descriptions of other data reads exactly like one whose
/// rows are data, and the difference has to live somewhere: this is that
/// somewhere, set by whoever built the catalog rather than sniffed from the
/// column names at keypress time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drill {
    /// Rows carry `database` / `schema` / `name` — the schema browser.
    Catalog,
}

/// An open tabular data source plus where the cursor is in it.
pub struct Session {
    /// The data as it was opened.  Boxed so a new backend needs no changes here.
    base: Box<dyn TableSource>,
    /// The transforms applied to `base`, innermost first.  A **stack**: `u` pops
    /// the last one, which is the natural undo for a read-only view and reuses
    /// the key that already means undo.
    stack: Vec<Transform>,
    /// The result of folding `stack` over `base`.  `None` when the stack is
    /// empty, in which case [`source`](Self::source) *is* the base — so an
    /// untransformed table costs nothing.
    derived: Option<Box<dyn TableSource>>,
    pub state: table::TableState,
    /// What this table *is*: the file it was opened from, or (later, for a query
    /// result or a derived table) a virtual identity with no file behind it.
    /// It is the stash key, the buffer-list entry, and the status-line name —
    /// `app.buffer` has no path while the grid is open.
    pub id: SourceId,
    /// For a *computed* table (a frequency table), the table it was derived
    /// from — where `q` goes back to, mirroring how `q` backs out of a
    /// `*cell …*` buffer.  `None` for a table read from a file.
    pub origin: Option<SourceId>,
    /// What `Enter` means on a row.  `None` — the ordinary case — reads the
    /// cell's full text; a catalog's rows *name* things, so `Enter` opens the
    /// thing instead.
    pub drill: Option<Drill>,
    /// Per-column statistics, computed once on demand and kept.
    ///
    /// Cached because a summary is a full scan of the column: the header
    /// sparkline needs one per visible column *per frame*, which is only
    /// affordable if the answer is remembered.  Keyed by column index; keyed
    /// alongside `summaries_rows` so a source that has since loaded more rows
    /// recomputes instead of reporting stale statistics.
    summaries: std::collections::HashMap<usize, ColumnSummary>,
    summaries_rows: usize,
}

impl Session {
    /// A session over `source`, identified by `id`.
    pub fn new(id: SourceId, source: Box<dyn TableSource>) -> Self {
        Self {
            base: source,
            stack: Vec::new(),
            derived: None,
            state: table::TableState::new(),
            id,
            origin: None,
            drill: None,
            summaries: std::collections::HashMap::new(),
            summaries_rows: 0,
        }
    }

    /// The data as the grid sees it: the base, or the top of the transform
    /// stack once anything has been applied.  **Every** reader goes through
    /// here, so a filtered grid cannot accidentally be drawn from the unfiltered
    /// source.
    pub fn source(&self) -> &dyn TableSource {
        match self.derived.as_deref() {
            Some(source) => source,
            None => self.base.as_ref(),
        }
    }

    /// The same, for the one caller that mutates: `ensure_rows`, which moves a
    /// windowed source's window to what is about to be drawn.
    fn source_mut(&mut self) -> &mut dyn TableSource {
        match self.derived.as_deref_mut() {
            Some(source) => source,
            None => self.base.as_mut(),
        }
    }

    /// Swap the data out from under a session, for tests that need a specific
    /// shape without going through a file and a background load.
    #[cfg(test)]
    pub(crate) fn replace_source(&mut self, source: Box<dyn TableSource>) {
        self.base = source;
        self.derived = None;
        self.stack.clear();
    }

    /// The transforms currently applied, innermost first.
    pub fn transforms(&self) -> &[Transform] {
        &self.stack
    }

    /// Apply one more transform, or leave the stack untouched and say why not.
    pub(super) fn push_transform(&mut self, op: Transform, max_rows: usize) -> Result<(), String> {
        self.stack.push(op);
        match self.rebuild(max_rows) {
            Ok(()) => Ok(()),
            Err(why) => {
                self.stack.pop();
                // Rebuild again so the view is what the surviving stack says it
                // is, not the half-built state the failure left behind.
                let _ = self.rebuild(max_rows);
                Err(why)
            }
        }
    }

    /// Drop the most recent transform.  Returns what was dropped.
    pub(super) fn pop_transform(&mut self, max_rows: usize) -> Option<Transform> {
        let popped = self.stack.pop()?;
        let _ = self.rebuild(max_rows);
        Some(popped)
    }

    /// Drop every transform, back to the table as it was opened.
    pub(super) fn clear_transforms(&mut self, max_rows: usize) -> usize {
        let n = self.stack.len();
        self.stack.clear();
        let _ = self.rebuild(max_rows);
        n
    }

    /// Fold the stack over the base, from scratch.
    ///
    /// From scratch rather than incrementally, because popping is the whole
    /// point of a stack and an incremental undo would have to keep every
    /// intermediate source alive.  Each step is pushed down when the source can
    /// do it and executed locally when it can't — the difference is invisible
    /// here, which is the design.
    fn rebuild(&mut self, max_rows: usize) -> Result<(), String> {
        let mut current: Option<Box<dyn TableSource>> = None;
        let mut failure = None;
        for op in &self.stack {
            let src: &dyn TableSource = match current.as_deref() {
                Some(s) => s,
                None => self.base.as_ref(),
            };
            let next = match src.derive(op) {
                Some(derived) => derived,
                // Local execution *reads* the source, and a windowed source can
                // only answer for the rows on screen — filtering those and
                // calling the result a filtered table would be a lie the grid
                // tells convincingly.
                None if src.is_windowed() => {
                    failure = Some(format!(
                        "This source can't {} — it is read a window at a time",
                        op.label(src.columns()),
                    ));
                    break;
                }
                None => Box::new(table::transform::apply_local(src, op, max_rows)),
            };
            current = Some(next);
        }
        self.derived = current;
        // The shape changed under the cursor; the caller's `update_scroll` puts
        // it back on screen, but it must not be left pointing off the end.
        let rows = self.source().loaded_rows();
        let cols = self.source().columns().len();
        self.state.clamp(rows, cols);
        // Statistics describe the data in view, and that is now different data.
        self.summaries.clear();
        self.summaries_rows = usize::MAX;
        match failure {
            Some(why) => Err(why),
            None => Ok(()),
        }
    }

    /// The cached summary for `col`, if one has been computed.
    ///
    /// Read-only, so the renderer can call it: a column whose summary hasn't
    /// been computed yet simply draws nothing this frame.  [`ensure_summaries`]
    /// is what fills the cache, from the exec layer where mutation belongs.
    ///
    /// [`ensure_summaries`]: Self::ensure_summaries
    pub fn summary(&self, col: usize) -> Option<&ColumnSummary> {
        (self.summaries_rows == self.source().loaded_rows())
            .then(|| self.summaries.get(&col))
            .flatten()
    }

    /// [`ensure_summaries`] for the renderer's tests, which draw a session
    /// directly rather than going through a frame of the exec layer.
    ///
    /// [`ensure_summaries`]: Self::ensure_summaries
    #[cfg(test)]
    pub(crate) fn ensure_summaries_for_test(&mut self, cols: impl IntoIterator<Item = usize>) {
        self.ensure_summaries(cols, usize::MAX);
    }

    /// Compute and cache summaries for `cols`, skipping any already cached.
    fn ensure_summaries(&mut self, cols: impl IntoIterator<Item = usize>, max_rows: usize) {
        // A source that has loaded more rows since invalidates every summary:
        // statistics over half a file are not statistics over the file.
        let rows = self.source().loaded_rows();
        if self.summaries_rows != rows {
            self.summaries.clear();
            self.summaries_rows = rows;
        }
        for col in cols {
            if !self.summaries.contains_key(&col) {
                self.summaries
                    .insert(col, summary::summarize(self.source(), col, max_rows));
            }
        }
    }

    /// Name shown in the status line.
    pub fn display_name(&self) -> String {
        self.id.label().to_string()
    }

    /// The file this table came from, or `None` for a virtual source.  Anything
    /// that reads or re-opens the underlying file must go through this.
    pub fn path(&self) -> Option<&Path> {
        self.id.as_path()
    }

    /// The cursor cell's **untruncated** value — what the grid can only show a
    /// clipped, single-line rendering of.
    pub fn cursor_value(&self) -> Option<&str> {
        self.source()
            .cell(self.state.cursor_row, self.state.cursor_col)
    }

    /// Header of the cursor's column.
    pub fn cursor_column_name(&self) -> Option<&str> {
        self.source()
            .columns()
            .get(self.state.cursor_col)
            .map(|c| c.name.as_str())
    }

    /// Every value in `row`, in column order (missing cells read as empty).
    fn row_values(&self, row: usize) -> Vec<&str> {
        (0..self.source().columns().len())
            .map(|c| self.source().cell(row, c).unwrap_or(""))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Column intelligence
// ---------------------------------------------------------------------------

/// `S` / `:column-summary` — statistics for the cursor's column, in a float.
///
/// The question a new dataset actually raises: how much is missing, what range
/// does it cover, is it skewed, which categories dominate.
pub(super) fn column_summary(app: &mut App) {
    let Some(session) = app.table.as_mut() else {
        app.messages.show("No table open");
        return;
    };
    let col = session.state.cursor_col;
    if session.source().columns().get(col).is_none() {
        app.messages.show("No column here");
        return;
    }
    let max_rows = app.config.table.summary_max_rows;
    let Some(session) = app.table.as_mut() else { return };
    session.ensure_summaries([col], max_rows);
    let session = app.table.as_ref().expect("still open");
    let Some(stats) = session.summary(col) else { return };
    let column = &session.source().columns()[col];
    let title = format!(" {} ", column.name);
    let body = summary_text(column, stats, session.source().row_count());
    app.popup = Some(crate::popup::Popup::documentation(&title, &body));
}

/// Lay a summary out as text for the float.
///
/// Deliberately labelled with the row count it *covered*: for a source that
/// holds only a window of its rows, statistics over the loaded rows are not
/// statistics over the dataset, and quietly presenting them as such would be the
/// worst kind of wrong.
fn summary_text(
    column: &table::Column,
    stats: &ColumnSummary,
    total: Option<usize>,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let kind = match column.ty {
        table::ColumnType::Int => "integer",
        table::ColumnType::Float => "number",
        table::ColumnType::Bool => "boolean",
        table::ColumnType::Text => "text",
    };
    let _ = writeln!(out, "{kind} · {} rows scanned", stats.rows);
    // Never let a partial scan read as the whole dataset: the row count it
    // actually covered is stated, and the shortfall called out.
    if total.is_some_and(|t| t > stats.rows) {
        let _ = writeln!(out, "(of {} — see table.summary_max_rows)", total.unwrap_or(0));
    }
    out.push('\n');

    let pct = |n: usize| -> String {
        if stats.rows == 0 {
            return String::new();
        }
        format!("  ({:.1}%)", n as f64 * 100.0 / stats.rows as f64)
    };
    let _ = writeln!(out, "  values    {}", stats.present());
    let _ = writeln!(out, "  missing   {}{}", stats.nulls, pct(stats.nulls));
    let _ = writeln!(out, "  distinct  {}", stats.distinct);

    if let (Some(q), Some(mean)) = (stats.quantiles, stats.mean) {
        out.push('\n');
        let _ = writeln!(out, "  min       {}", summary::fmt_num(q[0]));
        let _ = writeln!(out, "  p25       {}", summary::fmt_num(q[1]));
        let _ = writeln!(out, "  median    {}", summary::fmt_num(q[2]));
        let _ = writeln!(out, "  p75       {}", summary::fmt_num(q[3]));
        let _ = writeln!(out, "  max       {}", summary::fmt_num(q[4]));
        let _ = writeln!(out, "  mean      {}", summary::fmt_num(mean));
    }
    if stats.has_distribution() {
        out.push('\n');
        let _ = writeln!(out, "  {}", summary::sparkline(&stats.hist, summary::HIST_BINS));
    }
    if !stats.top.is_empty() {
        out.push('\n');
        let _ = writeln!(out, "  most common");
        let width = stats.top.iter().map(|(v, _)| v.chars().count().min(24)).max().unwrap_or(0);
        for (value, count) in &stats.top {
            let shown: String = value.chars().take(24).collect();
            let _ = writeln!(out, "    {shown:<width$}  {count}");
        }
        if stats.distinct > stats.top.len() {
            let _ = writeln!(
                out,
                "    … {} more (:column-frequency for all)",
                stats.distinct - stats.top.len()
            );
        }
    }
    out
}

/// `s` / `:sparkline` — show or hide the distribution row for the session.
///
/// A display preference rather than a table property, so it applies to every
/// grid and is not persisted; `[table] column_sparkline` is what makes it the
/// default.  Turning it on computes the summaries it needs on the next frame.
pub(super) fn toggle_sparkline(app: &mut App) {
    let on = !app.config.table.column_sparkline;
    app.config.table.column_sparkline = on;
    app.messages.show(if on {
        "Column sparkline on"
    } else {
        "Column sparkline off"
    });
}

/// `F` / `:column-frequency` — open the cursor column's value counts as a grid
/// of its own.
///
/// A derived table: the first source in the editor that is computed rather than
/// read, and the rehearsal for the groupby and pivot that come later.  It is a
/// grid rather than a float because it is data — sortable, scrollable, and its
/// own cells worth reading in full.
pub(super) fn column_frequency(app: &mut App) {
    let Some(session) = app.table.as_ref() else {
        app.messages.show("No table open");
        return;
    };
    let col = session.state.cursor_col;
    let Some(column) = session.source().columns().get(col) else {
        app.messages.show("No column here");
        return;
    };
    let name = column.name.clone();
    let scan_cap = app.config.table.summary_max_rows;
    let counts = summary::frequency(session.source(), col, scan_cap);
    if counts.is_empty() {
        app.messages.show("Nothing to count — the column is empty");
        return;
    }

    let scanned = session.source().loaded_rows().min(scan_cap);
    let rows: Vec<Vec<String>> = counts
        .iter()
        .map(|(value, count)| {
            let share = if scanned == 0 {
                String::new()
            } else {
                format!("{:.2}", *count as f64 * 100.0 / scanned as f64)
            };
            vec![value.clone(), count.to_string(), share]
        })
        .collect();
    let columns = derived_columns(
        &[
            (&name, table::ColumnType::Text),
            ("count", table::ColumnType::Int),
            ("percent", table::ColumnType::Float),
        ],
        &rows,
    );
    let distinct = rows.len();
    let source = table::MemSource::with_columns(
        columns,
        rows,
        format!("{distinct} distinct × 3 cols"),
    );

    let id = SourceId::virtual_named(&format!("freq {name}"));
    let origin = session.id.clone();
    open_derived(app, id, Box::new(source), Some(origin));
    app.messages.show(format!(
        "{distinct} distinct value(s) in {name} — q to go back"
    ));
}

/// Columns for a derived table: declared types, widths measured from the rows.
fn derived_columns(spec: &[(&str, table::ColumnType)], rows: &[Vec<String>]) -> Vec<table::Column> {
    spec.iter()
        .enumerate()
        .map(|(i, (name, ty))| table::Column {
            name: (*name).to_string(),
            ty: *ty,
            width_hint: rows
                .iter()
                .filter_map(|r| r.get(i))
                .map(|v| layout::display_width(&layout::sanitize(v)))
                .chain(std::iter::once(layout::display_width(name)))
                .max()
                .unwrap_or(0),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Transforms
// ---------------------------------------------------------------------------

/// Rows a locally-executed transform will scan.
///
/// A transform a source can't push down is executed by reading it, so this is
/// the same bound the loader already applies to a CSV.  A *windowed* source
/// never gets here — `Session::rebuild` refuses rather than filtering the
/// screenful that happens to be loaded.
fn transform_scan_cap(app: &App) -> usize {
    app.config.table.max_rows
}

/// Push `op` and report what happened.
fn apply_transform(app: &mut App, op: Transform) {
    let cap = transform_scan_cap(app);
    let Some(session) = app.table.as_mut() else {
        app.messages.show("No table open");
        return;
    };
    let label = op.label(session.source().columns());
    match session.push_transform(op, cap) {
        Ok(()) => {
            let shape = session.source().describe();
            app.messages.show(format!("{label} — {shape} (u undoes)"));
        }
        Err(why) => app.messages.show(why),
    }
    update_scroll(app);
}

/// `gs` — sort by the cursor column, cycling ascending → descending → unsorted.
///
/// Cycling on the same key rather than spending two bindings: the third press
/// putting the table back is also how you discover that the sort was a *view*
/// and the file was never touched.
pub(super) fn sort_cursor_column(app: &mut App) {
    let Some(session) = app.table.as_ref() else {
        app.messages.show("No table open");
        return;
    };
    let col = session.state.cursor_col;
    let cap = transform_scan_cap(app);
    match session.transforms().last() {
        Some(Transform::Sort { col: c, desc }) if *c == col => {
            let was_desc = *desc;
            if let Some(session) = app.table.as_mut() {
                let _ = session.pop_transform(cap);
            }
            if was_desc {
                app.messages.show("Unsorted");
                update_scroll(app);
                return;
            }
            apply_transform(app, Transform::Sort { col, desc: true });
        }
        _ => apply_transform(app, Transform::Sort { col, desc: false }),
    }
}

/// `gf` — ask what to filter the cursor column by.
pub(super) fn prompt_filter(app: &mut App) {
    if app.table.is_none() {
        app.messages.show("No table open");
        return;
    }
    app.command_buf.clear();
    app.mode = crate::mode::Mode::Prompt { kind: crate::mode::PromptKind::TableFilter };
}

/// The answer to that prompt.
pub(crate) fn apply_filter(app: &mut App, input: &str) {
    let Some(session) = app.table.as_ref() else { return };
    let col = session.state.cursor_col;
    match Predicate::parse(input) {
        Ok(pred) => apply_transform(app, Transform::Filter { col, pred }),
        Err(why) => app.messages.show(why),
    }
}

/// `gr` — ask which aggregates to compute alongside the group counts.
pub(super) fn prompt_group(app: &mut App) {
    if app.table.is_none() {
        app.messages.show("No table open");
        return;
    }
    app.command_buf.clear();
    app.mode = crate::mode::Mode::Prompt { kind: crate::mode::PromptKind::TableGroupBy };
}

/// The answer to that prompt: `sum qty, mean price`, or empty for counts alone.
pub(crate) fn apply_group(app: &mut App, input: &str) {
    let Some(session) = app.table.as_ref() else { return };
    let keys = vec![session.state.cursor_col];
    let columns = session.source().columns().to_vec();
    match parse_aggs(input, &columns) {
        Ok(aggs) => apply_transform(app, Transform::GroupBy { keys, aggs }),
        Err(why) => app.messages.show(why),
    }
}

/// Parse `sum qty, mean price` against the table's columns.
///
/// Column *names*, not indices: this is typed by a person looking at a header
/// row.  An unknown name is an error rather than a silently dropped clause —
/// a groupby missing a column you asked for is the kind of wrong that gets
/// believed.
fn parse_aggs(input: &str, columns: &[table::Column]) -> Result<Vec<(usize, Agg)>, String> {
    let mut out = Vec::new();
    for clause in input.split(',').map(str::trim).filter(|c| !c.is_empty()) {
        let mut words = clause.split_whitespace();
        let (Some(name), Some(column)) = (words.next(), words.next()) else {
            return Err(format!("`{clause}` should read like `sum qty`"));
        };
        let agg = Agg::parse(name).ok_or_else(|| {
            format!("`{name}` is not one of count/sum/mean/min/max")
        })?;
        let idx = columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(column))
            .ok_or_else(|| format!("no column called `{column}`"))?;
        out.push((idx, agg));
    }
    Ok(out)
}

/// `u` — pop the last transform.  The read-only view's undo.
pub(super) fn undo_transform(app: &mut App) {
    let cap = transform_scan_cap(app);
    let Some(session) = app.table.as_mut() else { return };
    let columns = session.source().columns().to_vec();
    match session.pop_transform(cap) {
        Some(op) => {
            let label = op.label(&columns);
            app.messages.show(format!("Undid {label}"));
        }
        None => app
            .messages
            .show("Nothing to undo — the table view never changes the file"),
    }
    update_scroll(app);
}

/// `gx` — back to the table as it was opened.
pub(super) fn clear_transforms(app: &mut App) {
    let cap = transform_scan_cap(app);
    let Some(session) = app.table.as_mut() else {
        app.messages.show("No table open");
        return;
    };
    match session.clear_transforms(cap) {
        0 => app.messages.show("No transforms to clear"),
        n => app.messages.show(format!("Cleared {n} transform(s)")),
    }
    update_scroll(app);
}

/// What a finished load hands back: the source, plus the two things only the
/// loader knows — whether it stopped at a row cap, and what to call it.
pub struct Loaded {
    source: Box<dyn TableSource + Send>,
    truncated: bool,
}

/// An in-flight background load, polled once per frame by the run loop.
pub struct TableLoad {
    path: PathBuf,
    rx: Receiver<Result<Loaded, String>>,
}

impl TableLoad {
    /// File name shown in the status line while the parse is in flight.
    pub fn display_name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("table")
    }
}

/// True when `path` is a delimited-text file the table view can open.
pub fn is_table_path(path: &Path) -> bool {
    is_delimited_text(path) || is_binary_data(path)
}

/// Delimited text the built-in parser can read (and the text editor can too, so
/// `:table-close` has somewhere to go).
fn is_delimited_text(path: &Path) -> bool {
    matches!(ext(path).as_deref(), Some("csv" | "tsv" | "tab"))
}

/// A data file that is *only* a table: not text, so there is no meaningful text
/// view of it and the grid is the only way to look.  Needs the `dataframe`
/// feature; without it, opening one reports that rather than showing bytes.
///
/// `.json` is deliberately absent: a JSON file is usually a document, and the
/// editor already highlights and folds it as one (`zt` on its records is a
/// feature). `:table` opens one in the grid on request.
fn is_binary_data(path: &Path) -> bool {
    matches!(
        ext(path).as_deref(),
        Some("parquet" | "pq" | "jsonl" | "ndjson" | "arrow" | "feather" | "ipc"),
    )
}

fn ext(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

/// Open `path` in the table view, replacing whatever is currently open.
///
/// The parse runs on a background thread: a large CSV must not stall a frame,
/// and there is no upper bound on what someone will try to open.  Until it
/// lands the editor shows an empty buffer with a "Loading…" message and the
/// status spinner running.
pub fn open_as_table(app: &mut App, path: &Path) {
    enter_table_view(app);
    super::register_buffer(&mut app.open_buffers, path);

    // A session stashed on the way out comes back whole — same parse, same
    // cursor cell.  This is what makes `Enter` into a cell buffer and back a
    // round trip rather than a reload.
    if let Some(session) = app.table_buffers.remove(&SourceId::of(path)) {
        app.table = Some(session);
        return;
    }
    if super::is_special_path(path) {
        // A virtual table with no stash behind it: there is no file to load, so
        // say so rather than trying to parse its name as CSV.
        app.messages
            .show(format!("{} is no longer open", path.to_string_lossy()));
        return;
    }
    start_load(app, path);
}

/// What is on screen right now, as a source identity: the open table, else the
/// open file.  What a newly computed table records as the place `q` goes back
/// to.
pub(super) fn current_source_id(app: &App) -> Option<SourceId> {
    if let Some(session) = app.table.as_ref() {
        return Some(session.id.clone());
    }
    if let Some((nb, _)) = app.notebook.as_ref() {
        return Some(SourceId::of(&nb.path));
    }
    app.buffer.path.as_deref().map(SourceId::of)
}

/// Show `source` in the grid under the virtual identity `id` — a table that was
/// *computed* rather than read from a file (a frequency table today; a query
/// result or a groupby later).
pub(super) fn open_derived(
    app: &mut App,
    id: SourceId,
    source: Box<dyn TableSource>,
    origin: Option<SourceId>,
) {
    debug_assert!(id.is_virtual(), "a derived table has no file behind it");
    enter_table_view(app);
    app.open_buffers.retain(|stored| *stored != id);
    app.open_buffers.push(id.clone());
    let mut session = Session::new(id, source);
    session.origin = origin;
    app.table = Some(session);
}

/// `q` / `:bd` in a computed table — go back to the table it was derived from.
///
/// The same paradigm as a `*cell …*` buffer: a frequency table is something you
/// open to answer one question and then back out of, so it is dropped rather
/// than stashed (`F` recomputes it in a keystroke) and its entry leaves the
/// buffer list instead of accumulating there.
///
/// Returns false when the open table isn't a derived one (nothing to go back to).
pub(super) fn close_derived_table(app: &mut App) -> bool {
    let Some(origin) = app
        .table
        .as_ref()
        .and_then(|s| s.origin.clone())
    else {
        return false;
    };
    let derived = app.table.as_ref().map(|s| s.id.clone());
    // Through `open_path` rather than `open_as_table`: an origin may be a file,
    // a stashed table, or a special buffer (the `*sql*` query a result came
    // from), and `open_path` is the one dispatcher that knows all three.
    super::open_path(app, &origin.to_path());
    if let Some(id) = derived {
        // `open_as_table` stashed it on the way past; drop it for good.
        app.table_buffers.remove(&id);
        app.open_buffers.retain(|stored| *stored != id);
    }
    true
}

/// Hand the screen to the table view: stash whatever was open, and detach
/// `app.buffer` so nothing in the editor holds a writable handle on the data.
fn enter_table_view(app: &mut App) {
    super::teardown_current_buffer(app);
    app.table = None;
    app.table_cell_origin = None;

    // Detached buffer: the table view renders from the source, and nothing that
    // could write `app.buffer` may be pointed at the data file.
    app.buffer = crate::buffer::Buffer::new_empty();
    app.selection = crate::selection::Selection::point(0);
    app.scroll_row = 0;
    app.scroll_col = 0;
    app.insert_session_active = false;
    app.lsp_language = None;
    app.highlighter = crate::highlight::Highlighter::new(None);
    app.mode = crate::mode::Mode::Normal;
    app.git_diff.clear();
    super::recompute_highlights(app);
    super::rebuild_diag_cache(app);
}

/// `:csv` — open the current buffer's file in the table view.
pub(super) fn open_current_as_table(app: &mut App) {
    if app.table.is_some() {
        app.messages.show("Already in the table view");
        return;
    }
    let Some(path) = app.buffer.path.clone().filter(|p| !super::is_special_path(p)) else {
        app.messages.show("No file to open as a table");
        return;
    };
    if app.buffer.modified {
        // The load reads from disk, so it would silently ignore the edits.
        app.messages
            .show("Save the buffer first — the table view reads from disk");
        return;
    }
    open_as_table(app, &path);
}

/// `:table-close` — leave the grid and open the same file as text.
pub(super) fn close_table(app: &mut App) {
    let Some(session) = app.table.take() else {
        app.messages.show("No table open");
        return;
    };
    let id = session.id.clone();
    let path = session.path().map(Path::to_path_buf);
    drop(session);
    // Deliberate exit from the grid: drop the stash too, so a later `:csv`
    // re-reads the file (which the user may have just edited as text) rather
    // than resurrecting the parse from before.
    app.table_buffers.remove(&id);
    match path {
        Some(path) => super::lsp::open_file_at(app, &path, 0, 0),
        // A virtual source has no text to fall back to.
        None => app.messages.show(format!("Closed {}", id.label())),
    }
}

// ---------------------------------------------------------------------------
// Reading a cell
// ---------------------------------------------------------------------------

/// Where an open `*cell …*` buffer came from, and what to undo when leaving it.
pub struct CellOrigin {
    /// The table the cell was read out of — what `:bd` returns to.
    pub id: SourceId,
    /// `editor.word_wrap` as it was before the cell buffer forced it on.
    prev_word_wrap: bool,
}

/// Called when leaving a cell buffer (from `teardown_current_buffer`, so every
/// exit path is covered): put the word-wrap setting back as it was.
pub(super) fn leave_cell_buffer(app: &mut App) {
    if let Some(origin) = app.table_cell_origin.take() {
        app.config.editor.word_wrap = origin.prev_word_wrap;
    }
}

/// Name of the virtual buffer a cell's text is read in.  The `*…*` form marks
/// it special (see [`super::is_special_path`]) so nothing tries to save it.
fn cell_buffer_name(session: &Session) -> String {
    let col = session.cursor_column_name().unwrap_or("?");
    format!("*cell {}:{}*", session.state.cursor_row + 1, col)
}

/// `Enter` — open the cursor cell's full text in its own buffer.
///
/// This is the point of the whole view: the grid deliberately shows one
/// truncated line per cell, and a paragraph-length value is only readable once
/// it is text in a buffer, with wrapping, motions and search.  The table is
/// stashed on the way out, so returning lands on the same cell.
pub(super) fn open_cell_buffer(app: &mut App) {
    let Some(session) = app.table.as_ref() else {
        app.messages.show("No table open");
        return;
    };
    let Some(value) = session.cursor_value() else {
        app.messages.show("No cell here");
        return;
    };
    if value.is_empty() {
        app.messages.show("Cell is empty");
        return;
    }

    let name = cell_buffer_name(session);
    let origin = session.id.clone();
    let short = session.display_name();
    let mut text = value.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }

    // One cell buffer at a time: without this every cell ever read would keep
    // its rope alive for the rest of the session.
    app.special_buffer_ropes.retain(|k, _| !k.starts_with("*cell "));
    app.special_buffer_ropes
        .insert(name.clone(), ropey::Rope::from_str(&text));

    super::switch_to_special_buffer(app, &name);
    // The value is prose far more often than it is code, and it is the long
    // ones the user came here to read — so wrap, and put the setting back when
    // the buffer is left (`leave_cell_buffer`).
    app.table_cell_origin = Some(CellOrigin {
        id: origin,
        prev_word_wrap: app.config.editor.word_wrap,
    });
    app.config.editor.word_wrap = true;
    app.messages
        .show(format!("Reading cell — :bd returns to {short}"));
}

/// `:bd` in a `*cell …*` buffer — go back to the table it came from.
/// Returns false when the current buffer is not a cell buffer.
pub(super) fn close_cell_buffer(app: &mut App) -> bool {
    if !app.in_cell_buffer() {
        return false;
    }
    let Some(origin) = app.table_cell_origin.as_ref().map(|o| o.id.to_path()) else {
        return false;
    };
    if let Some(name) = app.buffer.path.as_ref().and_then(|p| p.to_str()) {
        let name = name.to_string();
        app.special_buffer_ropes.remove(&name);
    }
    open_as_table(app, &origin);
    true
}

/// `K` / `gk` — peek the cursor cell's full text without leaving the grid.
///
/// Same question as `Enter` answers, asked without giving up your place: the
/// float is read-only and scrollable, and any key dismisses it.
pub(super) fn peek_cell(app: &mut App) {
    let Some(session) = app.table.as_ref() else {
        app.messages.show("No table open");
        return;
    };
    let Some(value) = session.cursor_value() else {
        app.messages.show("No cell here");
        return;
    };
    if value.is_empty() {
        app.messages.show("Cell is empty");
        return;
    }

    let title = format!(
        " {} · row {} ",
        session.cursor_column_name().unwrap_or("cell"),
        session.state.cursor_row + 1
    );
    // Wrap to the float's inner width: the text popup clips rather than wraps,
    // and a cell worth peeking at is exactly the one that would be clipped.
    let inner = popup_text_width(app.viewport_width);
    let wrapped: Vec<String> = value
        .lines()
        .flat_map(|line| {
            crate::render_util::wrap_segments(line, inner)
                .into_iter()
                .map(|(_, seg)| seg.to_string())
        })
        .collect();
    app.popup = Some(crate::popup::Popup::documentation(&title, &wrapped.join("\n")));
}

/// Inner text width of a `FractionOfScreen(0.6)` centered float, mirroring
/// `popup_ui::compute_width` (0.6 of the terminal, at least 20) minus borders.
fn popup_text_width(viewport_width: usize) -> usize {
    let w = ((viewport_width as f32 * 0.6) as usize).max(20);
    w.saturating_sub(2).max(1)
}

/// `y` — copy the cursor cell's full value to the system clipboard.
pub(super) fn yank_cell(app: &mut App) {
    let Some(session) = app.table.as_ref() else {
        return;
    };
    let Some(value) = session.cursor_value().map(str::to_string) else {
        app.messages.show("No cell here");
        return;
    };
    let n = value.chars().count();
    crate::clipboard::write(&value);
    app.messages.show(format!("Yanked cell ({n} chars)"));
}

/// One row rendered as a tab-separated line.  Values are flattened
/// ([`layout::sanitize`]) so an embedded newline can't split one row into two.
fn row_tsv(session: &Session, row: usize) -> String {
    session
        .row_values(row)
        .iter()
        .map(|v| layout::sanitize(v))
        .collect::<Vec<_>>()
        .join("\t")
}

/// `x` — copy the cursor row to the clipboard as a tab-separated line.
///
/// TSV rather than the file's own delimiter: it is what spreadsheets and chat
/// windows paste correctly, and it needs no quoting rules for the commas that
/// are already inside the values.
pub(super) fn yank_row(app: &mut App) {
    let Some(session) = app.table.as_ref() else {
        return;
    };
    let row = session.state.cursor_row;
    if row >= session.source().loaded_rows() {
        app.messages.show("No row here");
        return;
    }
    let line = row_tsv(session, row);
    let cols = session.source().columns().len();
    crate::clipboard::write(&line);
    app.messages
        .show(format!("Yanked row {} ({cols} columns)", row + 1));
}

/// Spawn the background parse for `path`.
fn start_load(app: &mut App, path: &Path) {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let (tx, rx) = mpsc::channel();
    let owned = path.to_path_buf();
    let cfg = app.config.table.clone();
    let thread_path = owned.clone();
    // Parsing *and* the engine's schema/count queries happen here, off the UI
    // thread: there is no upper bound on what someone will try to open.
    std::thread::spawn(move || {
        let _ = tx.send(load_source(&thread_path, &cfg));
    });
    app.table_pending = Some(TableLoad { path: owned, rx });
    app.messages.show(format!("Loading {name}…"));
}

/// Pick a backend for `path` and build its source.  Runs on the loader thread.
///
/// The built-in CSV parser holds every row in memory and is the right answer for
/// the ordinary case; DuckDB reads a window at a time and is the only answer for
/// parquet, newline-delimited JSON, arrow, and a CSV bigger than memory.
fn load_source(path: &Path, cfg: &crate::config::TableConfig) -> Result<Loaded, String> {
    if prefers_duckdb(path, cfg) {
        #[cfg(feature = "dataframe")]
        {
            return crate::table::duck::DuckDbSource::open_file(path)
                .map(|source| Loaded { source: Box::new(source), truncated: false })
                .map_err(|e| format!("{e:#}"));
        }
        #[cfg(not(feature = "dataframe"))]
        return Err(format!(
            "{} needs the dataframe feature (built without it)",
            path.display(),
        ));
    }
    CsvSource::load(path, cfg)
        .map(|source| Loaded { truncated: source.truncated(), source: Box::new(source) })
        .map_err(|e| e.to_string())
}

/// Whether `path` should go through DuckDB rather than the built-in parser.
///
/// A delimited-text file follows `[table] engine`; anything the built-in parser
/// cannot read at all (parquet, arrow, ndjson) has no choice.
fn prefers_duckdb(path: &Path, cfg: &crate::config::TableConfig) -> bool {
    if is_delimited_text(path) {
        return cfg.engine.eq_ignore_ascii_case("duckdb");
    }
    true
}

/// Install a finished load, if one is ready.  Returns true when state changed.
pub fn poll_table_load(app: &mut App) -> bool {
    let Some(job) = &app.table_pending else {
        return false;
    };
    let result = match job.rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => Err("load worker died".to_string()),
    };
    let path = job.path.clone();
    app.table_pending = None;

    match result {
        Ok(Loaded { source, truncated }) => {
            let rows = source.loaded_rows();
            let cols = source.columns().len();
            let shape = source.describe();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("table")
                .to_string();
            app.table = Some(Session::new(SourceId::of(&path), source));
            if cols == 0 {
                app.messages
                    .show(format!("{name} has no columns — nothing to show"));
            } else if truncated {
                app.messages.show(format!(
                    "{name}: {shape} — stopped at table.max_rows ({rows} rows)"
                ));
            } else {
                app.messages.show(format!("{name}: {shape}"));
            }
        }
        Err(e) => {
            app.messages.show(format!("Failed to load table: {e}"));
            // Don't leave the user in the blank detached buffer with no way
            // back.  A delimited-text file has a text view worth falling back
            // to; a parquet does not — showing its bytes as text would be worse
            // than the error, so fall back to scratch instead.
            if is_delimited_text(&path) {
                super::lsp::open_file_at(app, &path, 0, 0);
            } else {
                super::switch_to_special_buffer(app, "*scratch*");
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Command routing
// ---------------------------------------------------------------------------

/// Handle `cmd` in the table view.  Returns true when it was consumed, so
/// `execute()` skips the text-buffer path entirely; anything not listed here
/// (`:q`, the command palette, `:theme`, buffer switching, …) falls through and
/// behaves exactly as it does elsewhere.
pub(super) fn handle(app: &mut App, cmd: &Command) -> bool {
    debug_assert_eq!(app.view(), View::Table);
    let rows = app.table.as_ref().map_or(0, |s| s.source().loaded_rows());
    let cols = app.table.as_ref().map_or(0, |s| s.source().columns().len());
    let page = (visible_rows(app) / 2).max(1);

    let moved = |app: &mut App, f: &dyn Fn(&mut table::TableState)| {
        if let Some(session) = app.table.as_mut() {
            f(&mut session.state);
            session.state.clamp(rows, cols);
        }
    };

    match cmd {
        Command::MoveLeft => moved(app, &|s| s.cursor_col = s.cursor_col.saturating_sub(1)),
        Command::MoveRight => moved(app, &|s| s.cursor_col += 1),
        Command::MoveUp => moved(app, &|s| s.cursor_row = s.cursor_row.saturating_sub(1)),
        Command::MoveDown => moved(app, &|s| s.cursor_row += 1),

        // Word motions step a column at a time — the column is the table's
        // unit of horizontal structure, so `w`/`b` mean what they always mean.
        Command::MoveWordForward | Command::MoveWordEnd | Command::MoveBigWordForward
        | Command::MoveBigWordEnd => moved(app, &|s| s.cursor_col += 1),
        Command::MoveWordBackward | Command::MoveBigWordBackward => {
            moved(app, &|s| s.cursor_col = s.cursor_col.saturating_sub(1))
        }

        Command::MoveLineStart | Command::MoveLineFirstNonWs => moved(app, &|s| s.cursor_col = 0),
        Command::MoveLineEnd => moved(app, &|s| s.cursor_col = cols.saturating_sub(1)),
        Command::GotoFileStart => moved(app, &|s| s.cursor_row = 0),
        Command::GotoFileEnd => moved(app, &|s| s.cursor_row = rows.saturating_sub(1)),
        Command::PageDown => moved(app, &|s| s.cursor_row += page),
        Command::PageUp => moved(app, &|s| s.cursor_row = s.cursor_row.saturating_sub(page)),

        Command::TableClose => {
            close_table(app);
            return true;
        }
        Command::OpenAsTable => {
            app.messages.show("Already in the table view");
            return true;
        }

        // --- reading a cell ---
        Command::TableOpenCell => {
            match app.table.as_ref().and_then(|s| s.drill) {
                Some(Drill::Catalog) => super::attach::open_catalog_row(app),
                None => open_cell_buffer(app),
            }
            return true;
        }
        Command::SchemaBrowser => {
            super::attach::open_schema_browser(app);
            return true;
        }
        // `K` / `gk` mean "tell me more about the thing under the cursor"
        // everywhere in the editor; in a grid that is the cell's full text.
        Command::TablePeekCell | Command::LspShowDocumentation => {
            peek_cell(app);
            return true;
        }
        Command::TableYankCell => {
            yank_cell(app);
            return true;
        }
        Command::TableColumnSummary => {
            column_summary(app);
            return true;
        }
        Command::TableColumnFrequency => {
            column_frequency(app);
            return true;
        }

        // --- transforms ---
        Command::TableSort => {
            sort_cursor_column(app);
            return true;
        }
        Command::TableFilter => {
            prompt_filter(app);
            return true;
        }
        Command::TableGroupBy => {
            prompt_group(app);
            return true;
        }
        // `u` is undo everywhere in the editor; the read-only view's undo is
        // popping the transform stack, which is the only thing here that
        // *was* changed.
        Command::TableUndoTransform | Command::Undo => {
            undo_transform(app);
            return true;
        }
        Command::TableClearTransforms => {
            clear_transforms(app);
            return true;
        }
        Command::TableToggleSparkline => {
            toggle_sparkline(app);
            return true;
        }
        Command::TableCloseDerived => {
            if !close_derived_table(app) {
                app.messages
                    .show("Not a computed table — :table-close leaves the grid");
            }
            return true;
        }
        Command::TableYankRow => {
            yank_row(app);
            return true;
        }
        Command::TableCloseCell => {
            app.messages.show("No cell buffer open");
            return true;
        }
        // `:42` addresses a row, the grid's equivalent of a line.
        Command::GotoLine(n) => {
            let target = n.saturating_sub(1);
            moved(app, &|s| s.cursor_row = target);
        }
        // The palette's generic "yank" is the cell here — the grid has no text
        // selection for it to mean anything else.
        Command::YankSelection => {
            yank_cell(app);
            return true;
        }

        // Anything that would act on the detached buffer behind the grid is
        // refused with a reason, rather than silently operating on an empty
        // buffer (`ga` used to ask the LSP about nothing at all).
        _ => match refusal(cmd) {
            Some(why) => {
                app.messages.show(why.message(cmd));
                return true;
            }
            None => return false,
        },
    }

    update_scroll(app);
    true
}

/// Why a command doesn't run in the table view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// Would edit or save — the view is a read-only window on the file.
    ReadOnly,
    /// Operates on text structure (words, symbols, folds, the LSP's idea of the
    /// document). The buffer behind the grid is empty and path-less, so these
    /// answer nothing at best and lie at worst.
    NeedsText,
    /// Meaningful in a grid, just not built yet.
    NotImplemented,
}

impl Refusal {
    fn message(self, cmd: &Command) -> String {
        match self {
            Refusal::ReadOnly => {
                "The table view is read-only (:table-close to edit as text)".to_string()
            }
            Refusal::NeedsText => format!(
                "`{}` needs a text buffer (:table-close to edit as text)",
                cmd.name()
            ),
            Refusal::NotImplemented => format!(
                "`{}` isn't implemented for the table view yet",
                cmd.name()
            ),
        }
    }
}

/// Classify a command the grid does not implement.  `None` means "not ours" —
/// it falls through to the ordinary path and behaves as it does everywhere else
/// (`:q`, the palette, `:theme`, buffer switching, the toggles, …).
pub(super) fn refusal(cmd: &Command) -> Option<Refusal> {
    Some(match cmd {
        Command::EnterInsert
        | Command::EnterInsertAfter
        | Command::EnterInsertAtLineStart
        | Command::EnterInsertAtLineEnd
        | Command::DeleteSelection
        | Command::ChangeSelection
        | Command::PasteAfter
        | Command::PasteBefore
        | Command::OpenLineBelow
        | Command::OpenLineAbove
        | Command::Redo
        | Command::CommentRegion
        | Command::IndentRegion
        | Command::DedentRegion
        | Command::KillToEndOfLine
        | Command::Write
        | Command::WriteForce
        | Command::WriteQuit
        | Command::WriteAs(_)
        | Command::FormatDocument => Refusal::ReadOnly,

        // LSP: there is no document under the cursor to ask about.
        Command::LspCodeActions
        | Command::LspGotoDefinition
        | Command::LspGotoReferences
        | Command::LspGotoTypeDefinition
        | Command::LspGotoImplementation
        | Command::LspRequestCompletion
        // Text structure: characters, words, symbols, folds.
        | Command::FindCharForward
        | Command::FindCharBackward
        | Command::TillCharForward
        | Command::TillCharBackward
        | Command::EnterJumpMode
        | Command::EnterSelect
        | Command::SelectLine
        | Command::SelectAll
        | Command::OpenSymbolPicker
        | Command::OpenDiagnosticPicker
        | Command::GrepBuffer
        | Command::EnterFoldMode
        | Command::FoldToggle
        | Command::FoldToggleAll
        | Command::ScrollCursorCenter => Refusal::NeedsText,

        // Searching a grid is a real feature, it just doesn't exist yet.
        Command::SearchForward
        | Command::SearchBackward
        | Command::SearchNext
        | Command::SearchPrev => Refusal::NotImplemented,

        _ => return None,
    })
}

/// Data rows that fit on screen (the header takes one or two rows of the grid
/// area — `layout::header_rows` is the single definition of how many).
fn visible_rows(app: &App) -> usize {
    layout::visible_rows(app.viewport_height as u16, &app.config.table)
}

/// Keep the cursor cell on screen.  The column half delegates to
/// [`layout::scroll_col_for_cursor`] so the scroll and the renderer share one
/// definition of "fits".
pub(super) fn update_scroll(app: &mut App) {
    let rows_visible = visible_rows(app);
    let scroll_off = app.config.editor.scroll_off;
    let width = app.viewport_width as u16;
    let cfg = app.config.table.clone();

    let Some(session) = app.table.as_mut() else {
        return;
    };
    let total_rows = session.source().loaded_rows();
    session
        .state
        .clamp(total_rows, session.source().columns().len());

    // --- rows ---
    if rows_visible > 0 {
        let cursor = session.state.cursor_row;
        // Margin, shrunk so it can't exceed half the viewport on a short screen.
        let margin = scroll_off.min(rows_visible.saturating_sub(1) / 2);
        let top = session.state.scroll_row;
        if cursor < top + margin {
            session.state.scroll_row = cursor.saturating_sub(margin);
        } else if cursor + margin >= top + rows_visible {
            session.state.scroll_row = (cursor + margin + 1).saturating_sub(rows_visible);
        }
        // Never scroll past the end while rows remain to fill the screen.
        let max_top = total_rows.saturating_sub(rows_visible);
        session.state.scroll_row = session.state.scroll_row.min(max_top);
    }

    // --- columns ---
    session.state.scroll_col = layout::scroll_col_for_cursor(
        session.source(),
        session.state.scroll_col,
        session.state.cursor_col,
        width,
        &cfg,
    );

    // Make the visible window available to the source (a no-op for CSV, the
    // fetch trigger for a windowed source later).
    let first = session.state.scroll_row;
    let last = (first + rows_visible.max(1)).min(total_rows);
    session.source_mut().ensure_rows(first..last);

    // Summaries for the columns about to be drawn.  Here rather than in the
    // renderer because each one is a full scan of its column: the work has to
    // happen where it can be cached, and at most a screenful of columns is ever
    // new on a given frame.
    if cfg.column_sparkline {
        let visible: Vec<usize> = layout::compute(
            session.source(),
            session.state.scroll_col,
            width,
            &cfg,
        )
        .columns
        .iter()
        .map(|v| v.idx)
        .collect();
        session.ensure_summaries(visible, cfg.summary_max_rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TableConfig;

    fn source(rows: usize, cols: usize) -> CsvSource {
        let header: Vec<String> = (0..cols).map(|c| format!("col{c}")).collect();
        let mut text = header.join(",");
        text.push('\n');
        for r in 0..rows {
            let row: Vec<String> = (0..cols).map(|c| format!("{r}-{c}")).collect();
            text.push_str(&row.join(","));
            text.push('\n');
        }
        CsvSource::from_reader(text.as_bytes(), b',', &TableConfig::default()).unwrap()
    }

    /// An app in the table view over a `rows × cols` table.
    fn app_with_table(rows: usize, cols: usize) -> App {
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.viewport_height = 11; // 1 header + 10 data rows
        app.viewport_width = 80;
        // Pin the settings the geometry depends on — `Config::load()` picks up
        // the developer's own config file.
        app.config.editor.scroll_off = 2;
        app.config.table = TableConfig { column_sparkline: false, ..TableConfig::default() };
        app.table = Some(Session::new(
            SourceId::of(Path::new("t.csv")),
            Box::new(source(rows, cols)),
        ));
        app
    }

    fn cursor(app: &App) -> (usize, usize) {
        let s = &app.table.as_ref().unwrap().state;
        (s.cursor_row, s.cursor_col)
    }

    /// An app in the table view over `text`, parsed as CSV.
    fn app_with_csv(text: &str) -> App {
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.viewport_height = 11;
        app.viewport_width = 80;
        app.config.editor.scroll_off = 2;
        app.config.table = TableConfig { column_sparkline: false, ..TableConfig::default() };
        let src = CsvSource::from_reader(text.as_bytes(), b',', &app.config.table).unwrap();
        app.table = Some(Session::new(SourceId::of(Path::new("t.csv")), Box::new(src)));
        app
    }

    fn popup_text(app: &App) -> String {
        match app.popup.as_ref().map(|p| &p.content) {
            Some(crate::popup::PopupContent::Text(state)) => state.lines.join("\n"),
            _ => panic!("expected a text float"),
        }
    }

    #[test]
    fn column_summary_describes_a_numeric_column() {
        let mut app = app_with_csv("city,price\noslo,10\nlima,20\nbern,30\n");
        handle(&mut app, &Command::MoveRight); // onto `price`
        handle(&mut app, &Command::TableColumnSummary);

        let text = popup_text(&app);
        assert!(text.contains("integer"), "got {text}");
        assert!(text.contains("min       10"), "got {text}");
        assert!(text.contains("median    20"), "got {text}");
        assert!(text.contains("max       30"), "got {text}");
        assert!(text.contains("mean      20"), "got {text}");
        assert!(text.contains("distinct  3"), "got {text}");
        // A measurement column is summarised by its shape, not its top values.
        assert!(!text.contains("most common"), "got {text}");
    }

    #[test]
    fn column_summary_of_a_text_column_lists_its_common_values() {
        let mut app = app_with_csv("city,n\noslo,1\noslo,2\nlima,3\n");
        handle(&mut app, &Command::TableColumnSummary);

        let text = popup_text(&app);
        assert!(text.contains("text"), "got {text}");
        assert!(text.contains("most common"), "got {text}");
        assert!(text.contains("oslo"), "got {text}");
        // No range statistics for a category column — a "median city" is not a
        // thing, and printing one would be worse than printing nothing.
        assert!(!text.contains("median"), "got {text}");
    }

    #[test]
    fn column_summary_counts_missing_values_separately_from_zeros() {
        // An empty *field* — a blank line would be skipped by the parser, and
        // a missing value in real data is an empty field.
        let mut app = app_with_csv("n,tag\n1,a\n,b\n3,c\n");
        handle(&mut app, &Command::TableColumnSummary);
        let text = popup_text(&app);
        assert!(text.contains("missing   1"), "got {text}");
        assert!(text.contains("values    2"), "got {text}");
        // The blank row must not drag the mean down to 1.33.
        assert!(text.contains("mean      2"), "got {text}");
    }

    #[test]
    fn frequency_opens_the_value_counts_as_a_derived_grid() {
        let mut app = app_with_csv("city,n\noslo,1\nlima,2\noslo,3\n");
        handle(&mut app, &Command::TableColumnFrequency);

        // Still the table view, now over a virtual source with no file behind it
        // — so nothing in the editor can be pointed at a path to write.
        assert_eq!(app.view(), View::Table);
        let session = app.table.as_ref().expect("derived table open");
        assert!(session.id.is_virtual());
        assert!(session.path().is_none(), "a derived table has no file");
        assert_eq!(session.display_name(), "*freq city*");

        // value / count / percent, most frequent first.
        let cols: Vec<&str> = session.source().columns().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, vec!["city", "count", "percent"]);
        assert_eq!(session.source().cell(0, 0), Some("oslo"));
        assert_eq!(session.source().cell(0, 1), Some("2"));
        assert_eq!(session.source().cell(1, 0), Some("lima"));
        assert_eq!(session.source().loaded_rows(), 2);

        // It joins the buffer list under its virtual id, and `open_path` routes
        // back to the grid rather than opening a blank buffer named after it.
        let id = session.id.clone();
        assert!(app.open_buffers.contains(&id));
        super::super::buffers::open_path(&mut app, Path::new("t.csv"));
        assert!(app.table_buffers.contains_key(&id), "the derived table is stashed");
        super::super::buffers::open_path(&mut app, &id.to_path());
        assert_eq!(app.view(), View::Table);
        assert_eq!(app.table.as_ref().unwrap().id, id, "came back to the same table");
    }

    #[test]
    fn a_derived_table_has_no_text_to_fall_back_to() {
        let mut app = app_with_csv("city\noslo\n");
        handle(&mut app, &Command::TableColumnFrequency);
        // `:table-close` means "edit this as text", which a computed table has
        // none of — it must say so rather than strand an empty buffer.
        handle(&mut app, &Command::TableClose);
        assert!(app.table.is_none());
        assert!(app.messages.log.iter().any(|m| m.contains("Closed")), "{:?}", app.messages.log);
    }

    #[test]
    fn a_capped_summary_says_how_much_it_read() {
        // A summary is a full column scan, so it is capped; overstating its
        // reach would be the worst kind of wrong, so the panel names the number
        // of rows it actually covered.
        let text: String = std::iter::once("n".to_string())
            .chain((0..50).map(|i| i.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = app_with_csv(&text);
        app.config.table.summary_max_rows = 10;
        handle(&mut app, &Command::TableColumnSummary);

        let panel = popup_text(&app);
        assert!(panel.contains("10 rows scanned"), "got {panel}");
        assert!(panel.contains("of 50"), "got {panel}");
        // Statistics reflect the scanned prefix, not the whole column.
        assert!(panel.contains("max       9"), "got {panel}");
    }

    /// The `g`-prefixed analytic keys, end to end through the dispatcher the
    /// keyboard actually uses.
    #[test]
    fn sort_filter_and_group_are_view_transforms_that_stack_and_pop() {
        let mut app = app_with_csv("city,qty\noslo,10\nlima,3\noslo,7\n");

        // `gs` on the qty column sorts ascending; again reverses; again clears.
        handle(&mut app, &Command::MoveRight);
        handle(&mut app, &Command::TableSort);
        let col = |app: &App, c: usize| -> Vec<String> {
            let s = app.table.as_ref().unwrap();
            (0..s.source().loaded_rows())
                .map(|r| s.source().cell(r, c).unwrap_or_default().to_string())
                .collect()
        };
        assert_eq!(col(&app, 1), vec!["3", "7", "10"]);
        handle(&mut app, &Command::TableSort);
        assert_eq!(col(&app, 1), vec!["10", "7", "3"]);
        handle(&mut app, &Command::TableSort);
        assert_eq!(col(&app, 1), vec!["10", "3", "7"], "back to file order");
        assert!(app.table.as_ref().unwrap().transforms().is_empty());

        // `gf` asks, and the answer filters.  The prompt is a mode, so the
        // command only opens it.
        handle(&mut app, &Command::TableFilter);
        assert!(matches!(app.mode, crate::mode::Mode::Prompt { .. }));
        apply_filter(&mut app, "> 5");
        assert_eq!(col(&app, 1), vec!["10", "7"]);

        // Transforms stack: the sort applies to the filtered rows.
        handle(&mut app, &Command::TableSort);
        assert_eq!(col(&app, 1), vec!["7", "10"]);
        assert_eq!(app.table.as_ref().unwrap().transforms().len(), 2);

        // `u` is the read-only view's undo: it pops one, and eventually says
        // there was never anything to undo *about the file*.
        handle(&mut app, &Command::Undo);
        assert_eq!(col(&app, 1), vec!["10", "7"]);
        handle(&mut app, &Command::Undo);
        assert_eq!(col(&app, 1), vec!["10", "3", "7"]);
        handle(&mut app, &Command::Undo);
        assert!(
            app.messages.log.iter().any(|m| m.contains("never changes the file")),
            "{:?}",
            app.messages.log,
        );

        // `gr` groups by the cursor column, counting.
        handle(&mut app, &Command::MoveLeft);
        handle(&mut app, &Command::TableGroupBy);
        apply_group(&mut app, "");
        let names: Vec<String> = app
            .table
            .as_ref()
            .unwrap()
            .source()
            .columns()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(names, vec!["city", "count"]);
        assert_eq!(col(&app, 0), vec!["oslo", "lima"]);
        assert_eq!(col(&app, 1), vec!["2", "1"]);

        // ...and `gx` puts the whole table back.
        handle(&mut app, &Command::TableClearTransforms);
        assert_eq!(col(&app, 0), vec!["oslo", "lima", "oslo"]);
    }

    #[test]
    fn a_bad_aggregate_says_what_is_wrong_and_changes_nothing() {
        let mut app = app_with_csv("city,qty\noslo,10\n");
        handle(&mut app, &Command::TableGroupBy);
        apply_group(&mut app, "sum nope");
        assert!(
            app.messages.log.iter().any(|m| m.contains("no column called `nope`")),
            "{:?}",
            app.messages.log,
        );
        assert!(app.table.as_ref().unwrap().transforms().is_empty());
    }

    /// A transform changes the shape under the cursor, and the summaries
    /// describe the shape — so they must not survive it.
    #[test]
    fn a_transform_invalidates_the_cached_summaries() {
        let mut app = app_with_csv("n\n1\n2\n3\n4\n");
        app.table.as_mut().unwrap().ensure_summaries_for_test([0]);
        assert_eq!(app.table.as_ref().unwrap().summary(0).unwrap().mean, Some(2.5));
        handle(&mut app, &Command::TableFilter);
        apply_filter(&mut app, "> 2");
        assert!(app.table.as_ref().unwrap().summary(0).is_none(), "stale stats dropped");
        app.table.as_mut().unwrap().ensure_summaries_for_test([0]);
        assert_eq!(app.table.as_ref().unwrap().summary(0).unwrap().mean, Some(3.5));
    }

    #[test]
    fn summaries_are_computed_once_and_kept() {
        let mut app = app_with_csv("n\n1\n2\n3\n");
        let session = app.table.as_mut().unwrap();
        assert!(session.summary(0).is_none(), "nothing cached yet");
        session.ensure_summaries([0], usize::MAX);
        assert_eq!(session.summary(0).unwrap().mean, Some(2.0));
        // The renderer reads the cache every frame; it must not have to scan.
        let before = session.summaries.len();
        session.ensure_summaries([0], usize::MAX);
        assert_eq!(session.summaries.len(), before);
    }

    /// End-to-end: a real file goes through the background load, the run loop's
    /// poll, the table view, and back out to text — the wiring the pure-function
    /// tests above can't reach.
    #[test]
    fn opening_a_file_loads_asynchronously_and_closing_returns_to_text() {
        let dir = std::env::temp_dir().join(format!("sv-table-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        std::fs::write(&path, "a,b\n1,x\n2,y\n").unwrap();

        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.config.table = TableConfig { column_sparkline: false, ..TableConfig::default() };
        open_as_table(&mut app, &path);

        // The load is off-thread: the view is still Text and the buffer is
        // detached, so nothing here can write over the data file.
        assert_eq!(app.view(), View::Text);
        assert!(app.table_pending.is_some());
        assert!(app.buffer.path.is_none());

        // Poll like the run loop does until it lands.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.table.is_none() && std::time::Instant::now() < deadline {
            poll_table_load(&mut app);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let session = app.table.as_ref().expect("table loaded");
        assert_eq!(session.source().loaded_rows(), 2);
        assert_eq!(session.source().cell(1, 1), Some("y"));
        assert_eq!(session.display_name(), "data.csv");
        assert_eq!(app.view(), View::Table);
        assert!(app.table_pending.is_none());

        // `:table-close` gives back an editable text buffer on the same file.
        close_table(&mut app);
        assert!(app.table.is_none());
        assert_eq!(app.view(), View::Text);
        // The same file — compared through `SourceId`, since a session's
        // identity is the canonical path and `/var` is a symlink on macOS.
        assert_eq!(
            app.buffer.path.as_deref().map(SourceId::of),
            Some(SourceId::of(&path)),
        );
        assert!(app.buffer.rope.to_string().starts_with("a,b"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `:bd` in the grid used to read the identity off `app.buffer`, which the
    /// table view deliberately leaves detached and path-less — so it removed
    /// nothing, and the "closed" table came straight back from `H`/`L` or the
    /// buffer picker, stashed by the very switch that was supposed to close it.
    #[test]
    fn closing_a_file_backed_table_really_closes_it() {
        let dir = std::env::temp_dir().join(format!("sv-table-bd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        std::fs::write(&path, "a,b\n1,x\n2,y\n").unwrap();

        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.config.table = TableConfig { column_sparkline: false, ..TableConfig::default() };
        crate::exec::open_path(&mut app, &path);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.table.is_none() && std::time::Instant::now() < deadline {
            poll_table_load(&mut app);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.view(), View::Table);

        crate::exec::execute(&mut app, &crate::command::Command::BufferClose);

        let id = SourceId::of(&path);
        assert!(app.table.is_none(), "the grid must be gone");
        assert!(!app.open_buffers.contains(&id), "closed table must leave the buffer list");
        assert!(!app.table_buffers.contains_key(&id), "and must not be stashed on the way out");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The external file picker (yazi/fzf) opens a file without going through a
    /// popup confirm, so `open_path` is the only thing that can retire the
    /// dashboard — otherwise it stays painted over the opened file until the
    /// next keypress.  Also covers the load being visibly in progress.
    #[test]
    fn opening_from_the_dashboard_retires_the_splash_and_names_the_load() {
        let dir = std::env::temp_dir().join(format!("sv-splash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        std::fs::write(&path, "a,b\n1,x\n").unwrap();

        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.config.table = TableConfig { column_sparkline: false, ..TableConfig::default() };
        app.show_splash = true;

        crate::exec::open_path(&mut app, &path);
        assert!(!app.show_splash, "the dashboard must not stay over the file");

        // While the parse is in flight the status line names it and the spinner
        // has something to be active about.
        assert_eq!(app.table_load_name(), Some("data.csv"));
        assert!(app.table_pending.is_some());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.table.is_none() && std::time::Instant::now() < deadline {
            poll_table_load(&mut app);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.view(), View::Table);
        assert_eq!(app.table_load_name(), None, "cleared once the load lands");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Drive the run loop's poll until the load lands.
    #[cfg(feature = "dataframe")]
    fn await_load(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while app.table_pending.is_some() && std::time::Instant::now() < deadline {
            poll_table_load(app);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[cfg(feature = "dataframe")]
    #[test]
    fn a_parquet_file_opens_in_the_grid_through_the_dispatcher() {
        let dir = std::env::temp_dir().join(format!("sv-pq-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sales.parquet");
        let _ = std::fs::remove_file(&path);
        {
            let conn = crate::table::duck::open_readonly(None).unwrap();
            conn.execute_batch(&format!(
                "COPY (SELECT i AS id, 'r' || i AS name FROM range(0, 4) t(i)) \
                 TO '{}' (FORMAT PARQUET)",
                path.display(),
            ))
            .expect("write the fixture");
        }

        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.config.table = TableConfig { column_sparkline: false, ..TableConfig::default() };
        // Parquet is not text, so `open_path` — the one "user picked a file"
        // dispatcher — must route it to the grid without being asked.
        super::super::buffers::open_path(&mut app, &path);
        await_load(&mut app);

        let session = app.table.as_ref().expect("parquet loaded");
        assert_eq!(app.view(), View::Table);
        assert_eq!(session.source().row_count(), Some(4));
        assert_eq!(session.source().cell(3, 1), Some("r3"));
        assert!(session.source().describe().contains("duckdb"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "dataframe")]
    #[test]
    fn the_engine_setting_routes_a_csv_through_duckdb() {
        let dir = std::env::temp_dir().join(format!("sv-engine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        std::fs::write(&path, "a,b\n1,x\n2,y\n").unwrap();

        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.config.table = TableConfig {
            column_sparkline: false,
            engine: "duckdb".to_string(),
            ..TableConfig::default()
        };
        open_as_table(&mut app, &path);
        await_load(&mut app);

        let session = app.table.as_ref().expect("csv loaded via duckdb");
        assert!(session.source().describe().contains("duckdb"), "got {}", session.source().describe());
        assert_eq!(session.source().row_count(), Some(2));
        assert_eq!(session.source().cell(1, 1), Some("y"));
        // Same file, so the text view is still reachable — the engine choice is
        // about how it is read, not about what it is.
        close_table(&mut app);
        assert_eq!(app.view(), View::Text);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_reports_the_error_and_does_not_strand_the_view() {
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.config.table = TableConfig { column_sparkline: false, ..TableConfig::default() };
        let path = std::env::temp_dir().join("sv-table-does-not-exist.csv");
        let _ = std::fs::remove_file(&path);
        open_as_table(&mut app, &path);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.table_pending.is_some() && std::time::Instant::now() < deadline {
            poll_table_load(&mut app);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.table.is_none());
        assert_eq!(app.view(), View::Text);
        assert!(app
            .messages
            .log
            .iter()
            .any(|m| m.contains("Failed to load table")));
    }

    #[test]
    fn hjkl_moves_one_cell_and_stops_at_the_edges() {
        let mut app = app_with_table(3, 3);
        handle(&mut app, &Command::MoveDown);
        handle(&mut app, &Command::MoveRight);
        assert_eq!(cursor(&app), (1, 1));

        // Clamped at the far edges rather than wrapping or overflowing.
        for _ in 0..10 {
            handle(&mut app, &Command::MoveDown);
            handle(&mut app, &Command::MoveRight);
        }
        assert_eq!(cursor(&app), (2, 2));
        for _ in 0..10 {
            handle(&mut app, &Command::MoveUp);
            handle(&mut app, &Command::MoveLeft);
        }
        assert_eq!(cursor(&app), (0, 0));
    }

    #[test]
    fn line_and_file_motions_address_columns_and_rows() {
        let mut app = app_with_table(50, 4);
        handle(&mut app, &Command::MoveLineEnd);
        assert_eq!(cursor(&app), (0, 3), "$ goes to the last column");
        handle(&mut app, &Command::GotoFileEnd);
        assert_eq!(cursor(&app), (49, 3), "G goes to the last row, keeping the column");
        handle(&mut app, &Command::MoveLineStart);
        assert_eq!(cursor(&app), (49, 0));
        handle(&mut app, &Command::GotoFileStart);
        assert_eq!(cursor(&app), (0, 0));
    }

    /// Driven through `input::handle_key`, not `handle` directly: the goto
    /// sub-mode used to call `motion::*` on the buffer itself, so `gg`/`ge`/
    /// `gh`/`gl` never reached the table router and did nothing at all.
    #[test]
    fn goto_submode_keys_move_the_grid_cursor() {
        use crossterm::event::{KeyCode, KeyEvent};

        let mut app = app_with_table(50, 4);
        let press = |app: &mut App, c: char| {
            crate::input::handle_key(app, KeyEvent::from(KeyCode::Char(c)));
        };

        press(&mut app, 'g');
        press(&mut app, 'e');
        assert_eq!(cursor(&app), (49, 0), "ge → last row");

        press(&mut app, 'g');
        press(&mut app, 'l');
        assert_eq!(cursor(&app), (49, 3), "gl → last column");

        press(&mut app, 'g');
        press(&mut app, 'h');
        assert_eq!(cursor(&app), (49, 0), "gh → first column");

        press(&mut app, 'g');
        press(&mut app, 'g');
        assert_eq!(cursor(&app), (0, 0), "gg → first row");
    }

    #[test]
    fn word_motions_step_by_column() {
        let mut app = app_with_table(3, 5);
        handle(&mut app, &Command::MoveWordForward);
        handle(&mut app, &Command::MoveWordForward);
        assert_eq!(cursor(&app), (0, 2));
        handle(&mut app, &Command::MoveWordBackward);
        assert_eq!(cursor(&app), (0, 1));
    }

    #[test]
    fn paging_moves_half_a_screen_of_rows() {
        let mut app = app_with_table(100, 2);
        handle(&mut app, &Command::PageDown);
        assert_eq!(cursor(&app).0, 5, "half of the 10 visible data rows");
        handle(&mut app, &Command::PageUp);
        assert_eq!(cursor(&app).0, 0);
    }

    #[test]
    fn scroll_follows_the_cursor_with_a_margin() {
        let mut app = app_with_table(100, 2);
        // Walking down: the top only moves once the cursor reaches the margin.
        for _ in 0..7 {
            handle(&mut app, &Command::MoveDown);
        }
        let scroll = app.table.as_ref().unwrap().state.scroll_row;
        assert_eq!(scroll, 0, "cursor at row 7 of 10 visible is still inside");
        handle(&mut app, &Command::MoveDown);
        assert_eq!(app.table.as_ref().unwrap().state.scroll_row, 1);

        // The cursor is always within the visible window.
        handle(&mut app, &Command::GotoFileEnd);
        let s = &app.table.as_ref().unwrap().state;
        assert!(s.cursor_row >= s.scroll_row);
        assert!(s.cursor_row < s.scroll_row + 10);
    }

    #[test]
    fn scroll_does_not_run_past_the_last_screenful() {
        let mut app = app_with_table(100, 2);
        handle(&mut app, &Command::GotoFileEnd);
        assert_eq!(
            app.table.as_ref().unwrap().state.scroll_row,
            90,
            "the last screen is full, not one row of data under the header"
        );
    }

    #[test]
    fn a_table_shorter_than_the_screen_never_scrolls() {
        let mut app = app_with_table(4, 2);
        handle(&mut app, &Command::GotoFileEnd);
        assert_eq!(app.table.as_ref().unwrap().state.scroll_row, 0);
    }

    #[test]
    fn edits_and_writes_are_refused_not_applied_to_the_hidden_buffer() {
        let mut app = app_with_table(3, 2);
        for cmd in [
            Command::EnterInsert,
            Command::DeleteSelection,
            Command::PasteAfter,
            Command::Write,
            Command::WriteQuit,
        ] {
            assert!(handle(&mut app, &cmd), "{} should be consumed", cmd.name());
            assert_eq!(app.mode, crate::mode::Mode::Normal, "no mode change");
            assert!(app.buffer.rope.len_chars() == 0, "buffer untouched");
            assert!(!app.buffer.modified);
            assert!(app.messages.current().is_some_and(|m| m.contains("read-only")));
        }
    }

    // -----------------------------------------------------------------------
    // Reading a cell (the point of the view)
    // -----------------------------------------------------------------------

    /// A table whose one long cell is exactly what the grid cannot show.
    fn app_with_long_cell() -> (App, String) {
        let long: String = "lorem ipsum dolor ".repeat(30).trim_end().to_string();
        let text = format!("id,notes\n1,short\n2,\"{long}\"\n");
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.viewport_height = 11;
        app.viewport_width = 80;
        app.config.editor.scroll_off = 2;
        app.config.editor.word_wrap = false;
        app.config.table = TableConfig { column_sparkline: false, ..TableConfig::default() };
        app.table = Some(Session::new(
            SourceId::of(Path::new("notes.csv")),
            Box::new(
                CsvSource::from_reader(text.as_bytes(), b',', &TableConfig::default()).unwrap(),
            ),
        ));
        // Cursor on the long value.
        handle(&mut app, &Command::MoveDown);
        handle(&mut app, &Command::MoveRight);
        (app, long)
    }

    #[test]
    fn enter_opens_the_full_cell_text_in_its_own_buffer() {
        let (mut app, long) = app_with_long_cell();
        assert!(handle(&mut app, &Command::TableOpenCell));

        // The buffer holds the value in full — untruncated, unlike the grid.
        assert_eq!(app.buffer.rope.to_string().trim_end(), long);
        assert_eq!(
            app.buffer.path.as_ref().unwrap().to_str(),
            Some("*cell 2:notes*"),
            "named for the cell it came from"
        );
        assert_eq!(app.view(), View::Text, "the grid gives up the screen");
        // Nothing here can write the data file: a virtual buffer, no path.
        assert!(crate::exec::is_special_path(app.buffer.path.as_ref().unwrap()));
        assert!(app.config.editor.word_wrap, "wrapped, so it is readable");
    }

    #[test]
    fn returning_from_a_cell_buffer_lands_on_the_same_cell() {
        let (mut app, _) = app_with_long_cell();
        handle(&mut app, &Command::TableOpenCell);
        // The parsed table is stashed, not dropped.
        assert_eq!(app.table_buffers.len(), 1);

        assert!(close_cell_buffer(&mut app));
        assert_eq!(app.view(), View::Table, "back in the grid");
        assert_eq!(cursor(&app), (1, 1), "on the cell we were reading");
        assert!(
            app.table_pending.is_none(),
            "restored from the stash, not re-parsed from disk"
        );
        assert!(!app.config.editor.word_wrap, "wrap setting put back");
        assert!(app.table_cell_origin.is_none());
    }

    /// `:bd` is the natural "close this" for the cell buffer, and it must not
    /// hit the "cannot close special buffer" refusal.
    #[test]
    fn bd_in_a_cell_buffer_returns_to_the_table() {
        let (mut app, _) = app_with_long_cell();
        handle(&mut app, &Command::TableOpenCell);
        crate::exec::execute(&mut app, &Command::BufferClose);
        assert_eq!(app.view(), View::Table);
        assert_eq!(cursor(&app), (1, 1));
    }

    #[test]
    fn peek_shows_the_whole_value_wrapped_without_leaving_the_grid() {
        let (mut app, long) = app_with_long_cell();
        assert!(handle(&mut app, &Command::TablePeekCell));
        assert_eq!(app.view(), View::Table, "still in the grid");

        let popup = app.popup.as_ref().expect("peek float");
        let crate::popup::PopupContent::Text(state) = &popup.content else {
            panic!("peek should be a text float");
        };
        // Wrapped to the float, and complete: the rejoined rows are the value.
        let width = popup_text_width(app.viewport_width);
        assert!(state.lines.len() > 1, "a long value wraps to many rows");
        assert!(state.lines.iter().all(|l| l.chars().count() <= width));
        assert_eq!(state.lines.join(" "), long);
    }

    /// The peek float behaves like the completion popup: it is a passive
    /// overlay until Tab engages it, then j/k/J/K scroll and Esc leaves.
    /// Driven through `input::handle_key` so the popup layer is exercised.
    #[test]
    fn tab_engages_the_peek_float_then_j_k_scroll_and_esc_leaves() {
        use crossterm::event::{KeyCode, KeyEvent};
        let press = |app: &mut App, code: KeyCode| {
            crate::input::handle_key(app, KeyEvent::from(code));
        };
        let scroll = |app: &App| match &app.popup.as_ref().unwrap().content {
            crate::popup::PopupContent::Text(s) => s.scroll,
            _ => panic!("not a text float"),
        };
        let focused = |app: &App| match &app.popup.as_ref().unwrap().content {
            crate::popup::PopupContent::Text(s) => s.focused,
            _ => panic!("not a text float"),
        };

        let (mut app, _) = app_with_long_cell();
        app.config.ui.doc_popup_height = 3;
        // Opened by command, not by key: `K` is remappable in config, and this
        // test is about the float's behaviour once it is up.
        handle(&mut app, &Command::TablePeekCell);
        assert!(!focused(&app), "passive to begin with");

        press(&mut app, KeyCode::Tab);
        assert!(focused(&app));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(scroll(&app), 1, "j scrolls one line");
        press(&mut app, KeyCode::Char('J'));
        assert_eq!(scroll(&app), 2, "J scrolls half a float");
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(scroll(&app), 1);
        press(&mut app, KeyCode::Char('G'));
        assert!(scroll(&app) > 1, "G goes to the end");

        // Focused means focused: `l` scrolls nothing and must not reach the grid.
        let before = cursor(&app);
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(cursor(&app), before, "keys don't leak past a focused float");

        press(&mut app, KeyCode::Esc);
        assert!(app.popup.is_none(), "Esc leaves the float");
        assert_eq!(app.view(), View::Table, "and lands back in the grid");
    }

    /// While passive the float is a glance, not a mode: moving on closes it and
    /// the key still does its job.
    #[test]
    fn a_passive_peek_float_gets_out_of_the_way() {
        use crossterm::event::{KeyCode, KeyEvent};
        let (mut app, _) = app_with_long_cell();
        let before = cursor(&app);
        handle(&mut app, &Command::TablePeekCell);
        crate::input::handle_key(&mut app, KeyEvent::from(KeyCode::Char('k')));
        assert!(app.popup.is_none());
        assert_eq!(cursor(&app).0, before.0 - 1, "k still moved up a row");
    }

    /// `K` and `gk` mean "tell me more about this" everywhere else, so they
    /// must peek here rather than firing an LSP request against the empty
    /// buffer behind the grid.
    #[test]
    fn k_peeks_instead_of_asking_the_lsp() {
        let (mut app, _) = app_with_long_cell();
        assert!(handle(&mut app, &Command::LspShowDocumentation));
        assert!(app.popup.is_some());
    }

    #[test]
    fn empty_and_missing_cells_say_so_instead_of_opening_a_blank_buffer() {
        let mut app = app_with_table(3, 3);
        // Blank the cell under the cursor by pointing at a column past the data.
        let source =
            CsvSource::from_reader("a,b\n1,\n".as_bytes(), b',', &TableConfig::default()).unwrap();
        app.table.as_mut().unwrap().replace_source(Box::new(source));
        handle(&mut app, &Command::MoveRight);

        handle(&mut app, &Command::TableOpenCell);
        assert_eq!(app.view(), View::Table, "no buffer opened");
        assert!(app.messages.current().is_some_and(|m| m.contains("empty")));

        handle(&mut app, &Command::TablePeekCell);
        assert!(app.popup.is_none());
    }

    /// The yank commands themselves are not driven here: `clipboard::write`
    /// shells out to the real system clipboard, which a test must neither
    /// depend on nor clobber.  What they copy is this.
    #[test]
    fn yank_text_is_the_full_cell_and_the_row_as_tsv() {
        let (app, long) = app_with_long_cell();
        let session = app.table.as_ref().unwrap();

        assert_eq!(session.cursor_value(), Some(long.as_str()), "untruncated");
        assert_eq!(
            row_tsv(session, 1),
            format!("2\t{long}"),
            "every column of the row, tab-separated"
        );
    }

    /// An embedded newline must not turn one copied row into two lines.
    #[test]
    fn a_multiline_value_stays_on_one_line_when_the_row_is_yanked() {
        let mut app = app_with_table(1, 1);
        let source = CsvSource::from_reader(
            "a,b\n\"one\ntwo\",x\n".as_bytes(),
            b',',
            &TableConfig::default(),
        )
        .unwrap();
        app.table.as_mut().unwrap().replace_source(Box::new(source));
        let line = row_tsv(app.table.as_ref().unwrap(), 0);
        assert!(!line.contains('\n'));
        assert_eq!(line, "one↵two\tx");
    }

    #[test]
    fn unrelated_commands_fall_through_to_the_normal_path() {
        let mut app = app_with_table(3, 2);
        assert!(!handle(&mut app, &Command::Quit));
        assert!(!handle(&mut app, &Command::OpenCommandPalette));
        assert!(!handle(&mut app, &Command::EnterCommandMode));
        // Things that work the same everywhere must keep working here.
        for cmd in [
            Command::BufferNext,
            Command::OpenFilePicker,
            Command::OpenThemePicker,
            Command::ToggleLineNumbers,
            Command::SwitchToMessages,
        ] {
            assert!(refusal(&cmd).is_none(), "{} should fall through", cmd.name());
        }
    }

    /// The buffer behind the grid is empty and path-less, so a command that
    /// reads it answers about nothing — `ga` used to ask the LSP for code
    /// actions on an empty document.  Each one now says why it can't run.
    #[test]
    fn commands_that_need_a_text_buffer_are_refused_with_a_reason() {
        let mut app = app_with_table(3, 2);
        for cmd in [
            Command::LspCodeActions,
            Command::LspGotoDefinition,
            Command::LspGotoReferences,
            Command::EnterJumpMode,
            Command::OpenSymbolPicker,
            Command::EnterSelect,
            Command::FoldToggle,
            Command::FindCharForward,
        ] {
            assert!(handle(&mut app, &cmd), "{} should be consumed", cmd.name());
            assert_eq!(app.mode, crate::mode::Mode::Normal, "no mode change");
            assert!(app.popup.is_none(), "no popup opened");
            let msg = app.messages.current().unwrap_or_default().to_string();
            assert!(
                msg.contains("needs a text buffer") && msg.contains(cmd.name()),
                "{} was refused with {msg:?}",
                cmd.name()
            );
        }
    }

    #[test]
    fn search_says_it_is_not_built_yet_rather_than_searching_nothing() {
        let mut app = app_with_table(3, 2);
        assert!(handle(&mut app, &Command::SearchForward));
        assert_eq!(app.mode, crate::mode::Mode::Normal);
        assert!(app
            .messages
            .current()
            .is_some_and(|m| m.contains("isn't implemented")));
    }

    /// `:42` addresses a row — the grid's equivalent of a line number.
    #[test]
    fn a_line_number_command_goes_to_that_row() {
        let mut app = app_with_table(50, 3);
        handle(&mut app, &Command::GotoLine(12));
        assert_eq!(cursor(&app), (11, 0), "1-based, like the row gutter");
        handle(&mut app, &Command::GotoLine(9999));
        assert_eq!(cursor(&app).0, 49, "clamped to the last row");
    }

    /// visidata muscle memory: `q` backs out of the cell text. Driven through
    /// `input::handle_key`, since the binding lives in the cell override map.
    #[test]
    fn q_closes_a_cell_buffer_and_returns_to_the_grid() {
        use crossterm::event::{KeyCode, KeyEvent};
        let (mut app, _) = app_with_long_cell();
        handle(&mut app, &Command::TableOpenCell);
        assert!(app.in_cell_buffer());

        crate::input::handle_key(&mut app, KeyEvent::from(KeyCode::Char('q')));
        assert_eq!(app.view(), View::Table);
        assert_eq!(cursor(&app), (1, 1), "on the cell we were reading");
        assert!(!app.should_quit, "q closes the cell, it does not quit sv");
    }

    /// …and `q` keeps meaning nothing in an ordinary buffer.
    #[test]
    fn q_backs_out_of_a_derived_table_to_the_one_it_came_from() {
        let mut app = app_with_csv("city,n\noslo,1\nlima,2\noslo,3\n");
        let origin = app.table.as_ref().unwrap().id.clone();
        handle(&mut app, &Command::MoveDown); // somewhere other than row 0
        handle(&mut app, &Command::TableColumnFrequency);
        let derived = app.table.as_ref().unwrap().id.clone();
        assert_eq!(app.table.as_ref().unwrap().origin, Some(origin.clone()));

        // `q` — the same "back out of the temporary thing" as in a cell buffer.
        handle(&mut app, &Command::TableCloseDerived);
        assert_eq!(app.view(), View::Table);
        let session = app.table.as_ref().expect("back in the original table");
        assert_eq!(session.id, origin);
        assert_eq!(session.state.cursor_row, 1, "the original cursor is preserved");

        // The frequency table is gone rather than stashed: `F` rebuilds it in a
        // keystroke, and a buffer list that accumulates `*freq …*` entries is
        // worse than one that doesn't.
        assert!(!app.table_buffers.contains_key(&derived));
        assert!(!app.open_buffers.contains(&derived));
    }

    #[test]
    fn bd_in_a_derived_table_goes_back_too() {
        let mut app = app_with_csv("city\noslo\n");
        let origin = app.table.as_ref().unwrap().id.clone();
        handle(&mut app, &Command::TableColumnFrequency);
        super::super::execute(&mut app, &Command::BufferClose);
        assert_eq!(app.table.as_ref().map(|s| s.id.clone()), Some(origin));
    }

    #[test]
    fn q_on_a_file_backed_table_says_there_is_nowhere_to_go_back_to() {
        let mut app = app_with_csv("city\noslo\n");
        handle(&mut app, &Command::TableCloseDerived);
        // Still in the grid, and told why rather than silently doing nothing.
        assert!(app.table.is_some());
        assert!(
            app.messages.log.iter().any(|m| m.contains("Not a computed table")),
            "{:?}",
            app.messages.log,
        );
    }

    #[test]
    fn the_sparkline_toggles_and_the_row_window_follows() {
        let mut app = app_with_table(50, 3);
        assert!(!app.config.table.column_sparkline, "off by default");
        let with_row = visible_rows(&app);

        handle(&mut app, &Command::TableToggleSparkline);
        assert!(app.config.table.column_sparkline);
        // The taller header takes its row from the data, through
        // `layout::header_rows` — the one place that height is defined.
        assert_eq!(visible_rows(&app), with_row - 1);

        handle(&mut app, &Command::TableToggleSparkline);
        assert!(!app.config.table.column_sparkline);
        assert_eq!(visible_rows(&app), with_row);
    }

    #[test]
    fn q_is_inert_outside_a_cell_buffer() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new(None, crate::config::Config::load()).expect("app");
        app.buffer.rope = ropey::Rope::from_str("hello\n");
        assert!(!app.in_cell_buffer());
        crate::input::handle_key(&mut app, KeyEvent::from(KeyCode::Char('q')));
        assert!(!app.should_quit);
        assert_eq!(app.buffer.rope.to_string(), "hello\n");
    }
}
