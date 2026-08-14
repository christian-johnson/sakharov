//! DuckDB-backed [`TableSource`]: parquet, JSON, big CSVs, and SQL.
//!
//! This is the backend that validates the trait's windowing design.  Where
//! [`CsvSource`](crate::table::csv::CsvSource) holds every row in memory, a
//! `DuckDbSource` holds a *query* plus the window of rows currently on screen,
//! and refetches as the cursor moves.  A file far larger than RAM is therefore
//! openable, and nothing about the grid changes.
//!
//! # Read-only
//!
//! The editor never holds a writable handle on a database, in three layers of
//! decreasing trust:
//!
//! 1. **The connection.** [`open_readonly`] is the only place a connection is
//!    created.  A database file is opened with
//!    `Config::access_mode(AccessMode::ReadOnly)`, and any `ATTACH` the editor
//!    issues carries `READ_ONLY`.  Reading *files* (parquet/CSV/JSON) needs an
//!    in-memory database, which cannot itself be opened read-only — so for that
//!    case the guarantee is narrower and honest: the in-memory database is
//!    scratch space that never touches disk, and every real database attached to
//!    it is read-only.
//! 2. **The [`gate`].** Every statement the user typed is checked before it
//!    reaches the connection.  Its job is the error message; it is also what
//!    blocks `COPY … TO` and `EXPORT DATABASE`, the ways to write a *file* from
//!    a connection that cannot write its own database.
//! 3. **The view.** The grid holds no writable handle at all: `app.buffer` is
//!    detached while a table is open, and `exec::table::refusal` turns down the
//!    write commands.
//!
//! Deliberately *not* used: DuckDB's own safe mode.
//! `enable_external_access = false` does block writes — and also blocks
//! `read_parquet`, `read_csv` and `read_json`, which is this backend's entire
//! purpose.  `SET disabled_filesystems` likewise blocks local reads.  Engine
//! file-gating is treated as a bonus, never as the guarantee.

pub mod gate;

use std::ops::Range;
use std::path::Path;

use anyhow::{bail, Context, Result};
use duckdb::{Config, Connection};

use crate::table::{Column, ColumnType, TableSource};

/// Rows fetched per round trip.  A window rather than a page: scrolling one row
/// past the edge should not cost a query, so the window is a good deal larger
/// than a screenful.
const WINDOW: usize = 500;

/// Open a connection that cannot write anything on disk.
///
/// `path` is a DuckDB database file to attach read-only; `None` gives an
/// in-memory database, which is what reading a parquet/CSV/JSON *file* needs
/// (the file is read through a table function, never attached).
///
/// This is the only constructor of a connection in the editor.  Anything that
/// needs one goes through here, so "the editor never opens a database
/// read-write" is a property of one function rather than a habit.
pub fn open_readonly(path: Option<&Path>) -> Result<Connection> {
    match path {
        Some(path) => {
            let config = Config::default()
                .access_mode(duckdb::AccessMode::ReadOnly)
                .context("configuring a read-only connection")?;
            Connection::open_with_flags(path, config)
                .with_context(|| format!("opening {} read-only", path.display()))
        }
        None => Connection::open_in_memory().context("opening an in-memory database"),
    }
}

/// The `ATTACH … (TYPE …)` a database file needs, or `None` for DuckDB's own
/// format.  `None` is also the answer for an unrecognised extension: DuckDB
/// sniffs the file itself and gives a better error than a guess would.
pub fn attach_kind(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("sqlite" | "sqlite3" | "db") => Some("SQLITE"),
        _ => None,
    }
}

/// A connection with every attachment replayed, and bare filenames resolved
/// against `dir`.
///
/// The in-memory database is scratch space that never touches disk; every real
/// database hanging off it is attached `READ_ONLY`, which is where the guarantee
/// actually lives.  The editor issues these `ATTACH` statements itself — the
/// [`gate`] rejects one typed by the user, precisely so that `READ_ONLY` is not
/// something a query can leave off.
pub fn connect(attachments: &[crate::table::Attachment], dir: Option<&Path>) -> Result<Connection> {
    let conn = open_readonly(None)?;
    // `FROM 'data.csv'` should mean the file next to what you were working on,
    // not one in whatever directory the editor was launched from.
    if let Some(dir) = dir.and_then(Path::to_str) {
        let stmt = format!("SET file_search_path = {}", sql_string_literal(dir));
        let _ = conn.execute_batch(&stmt);
    }
    for a in attachments {
        conn.execute_batch(&attach_statement(a))
            .with_context(|| format!("attaching {} as {}", a.path.display(), a.alias))?;
    }
    Ok(conn)
}

/// The `ATTACH` for one attachment.  Always `READ_ONLY`.
fn attach_statement(a: &crate::table::Attachment) -> String {
    let options = match a.kind {
        Some(kind) => format!("(TYPE {kind}, READ_ONLY)"),
        None => "(READ_ONLY)".to_string(),
    };
    format!(
        "ATTACH {} AS {} {options}",
        sql_string_literal(&a.path.to_string_lossy()),
        sql_ident(&a.alias),
    )
}

/// `SELECT * FROM "db"."schema"."name"` — the query behind `Enter` on a row of
/// the schema browser.  Each part is quoted, so a table called `select` or one
/// with a `"` in its name is addressed rather than parsed.
pub fn table_query(database: &str, schema: &str, name: &str) -> String {
    format!(
        "SELECT * FROM {}.{}.{}",
        sql_ident(database),
        sql_ident(schema),
        sql_ident(name),
    )
}

/// Every table and view in every attached database, for the schema browser.
///
/// `information_schema` rather than DuckDB's own `duckdb_tables()`: it is the
/// standard shape, and it already spans attached databases.
pub fn catalog_query() -> &'static str {
    "SELECT t.table_catalog AS database, \
            t.table_schema  AS schema, \
            t.table_name    AS name, \
            t.table_type    AS type, \
            (SELECT count(*) FROM information_schema.columns c \
              WHERE c.table_catalog = t.table_catalog \
                AND c.table_schema  = t.table_schema \
                AND c.table_name    = t.table_name) AS columns \
       FROM information_schema.tables t \
      ORDER BY 1, 2, 3"
}

/// A window onto the result of one query.
pub struct DuckDbSource {
    conn: Connection,
    /// The query this source is a window onto.  Every fetch wraps it in a
    /// subquery rather than re-parsing or rewriting it.
    sql: String,
    columns: Vec<Column>,
    /// Rows currently in `cells`.
    window: Range<usize>,
    cells: Vec<Vec<String>>,
    total: Option<usize>,
    label: String,
    /// Queries issued for row windows.  Only read by the test that pins the
    /// windowing behaviour — a source that quietly fetched everything would
    /// still look correct.
    fetches: std::cell::Cell<usize>,
}

impl DuckDbSource {
    /// Run `sql` on `conn` and take a window onto its result.
    ///
    /// The statement goes through the [`gate`] first, so every path into the
    /// engine — a file the editor opened, a query the user typed, and later a
    /// pushed-down transform — is checked in one place.
    pub fn query(conn: Connection, sql: impl Into<String>, label: impl Into<String>) -> Result<Self> {
        let sql = sql.into();
        if let Err(rejected) = gate::check(&sql) {
            bail!(rejected.0);
        }
        let columns = describe(&conn, &sql)?;
        if columns.is_empty() {
            bail!("the query returned no columns");
        }
        let total = count(&conn, &sql).ok();
        let mut source = Self {
            conn,
            sql,
            columns,
            window: 0..0,
            cells: Vec::new(),
            total,
            label: label.into(),
            fetches: std::cell::Cell::new(0),
        };
        source.fetch(0)?;
        Ok(source)
    }

    /// Open a data file through the table function that suits its extension.
    ///
    /// The path is passed as a bound parameter, so a filename containing a quote
    /// cannot end the string literal and become SQL.
    pub fn open_file(path: &Path) -> Result<Self> {
        let conn = open_readonly(None)?;
        let func = table_function(path)
            .with_context(|| format!("no reader for {}", path.display()))?;
        let literal = sql_string_literal(&path.to_string_lossy());
        let sql = format!("SELECT * FROM {func}({literal})");
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self::query(conn, sql, label)
    }

    /// Fetch the window containing `first`.
    fn fetch(&mut self, first: usize) -> Result<()> {
        let start = first - (first % WINDOW);
        // `COLUMNS(*)` applies the cast to every column *positionally*, which is
        // what a query like `SELECT a, a` needs — naming the columns in the
        // projection would read the first one twice.  Casting to VARCHAR gives
        // one uniform way to read a value out; the column's declared type, from
        // DESCRIBE, is what drives alignment in the grid.
        let sql = format!(
            "SELECT CAST(COLUMNS(*) AS VARCHAR) FROM ({}) AS \"src\" \
             LIMIT {WINDOW} OFFSET {start}",
            self.sql
        );

        let mut stmt = self.conn.prepare(&sql).context("preparing a row window")?;
        let mut rows = stmt.query([]).context("fetching a row window")?;
        let mut cells = Vec::new();
        while let Some(row) = rows.next().context("reading a row")? {
            let values = (0..self.columns.len())
                .map(|i| row.get::<usize, Option<String>>(i).unwrap_or(None).unwrap_or_default())
                .collect();
            cells.push(values);
        }
        self.window = start..start + cells.len();
        self.cells = cells;
        self.fetches.set(self.fetches.get() + 1);
        Ok(())
    }

    /// Row windows fetched so far — for the test that pins the windowing.
    #[cfg(test)]
    pub fn fetch_count(&self) -> usize {
        self.fetches.get()
    }
}

impl TableSource for DuckDbSource {
    fn columns(&self) -> &[Column] {
        &self.columns
    }

    fn row_count(&self) -> Option<usize> {
        self.total
    }

    fn loaded_rows(&self) -> usize {
        // The grid's row space is the whole result, not the window: `cell`
        // returns `None` outside the window, and `ensure_rows` moves it.
        self.total.unwrap_or(self.window.end)
    }

    fn cell(&self, row: usize, col: usize) -> Option<&str> {
        if !self.window.contains(&row) {
            return None;
        }
        self.cells
            .get(row - self.window.start)?
            .get(col)
            .map(String::as_str)
    }

    fn ensure_rows(&mut self, rows: Range<usize>) {
        if rows.is_empty() {
            return;
        }
        // Refetch only when the requested window isn't already covered.  A
        // fetch per frame while the cursor sits still would be a query storm.
        if self.window.contains(&rows.start) && self.window.contains(&(rows.end - 1)) {
            return;
        }
        // A failed refetch leaves the previous window in place: better a stale
        // screenful than an empty grid, and the error is already surfaced by the
        // load path.
        let _ = self.fetch(rows.start);
    }

    /// The point of this backend: only the window on screen is ever in memory.
    fn is_windowed(&self) -> bool {
        true
    }

    /// Every transform pushes down: this source is a *query*, so a derivation is
    /// one more layer of subquery, executed by the engine over the whole table
    /// rather than over the window that happens to be on screen.
    fn derive(&self, op: &crate::table::transform::Transform) -> Option<Box<dyn TableSource>> {
        let sql = op.to_sql(&self.sql, &self.columns)?;
        // A fresh handle on the same database — including whatever is attached
        // to it, since the derived query still names those tables.
        let conn = self.conn.try_clone().ok()?;
        Self::query(conn, sql, self.label.clone())
            .ok()
            .map(|s| Box::new(s) as Box<dyn TableSource>)
    }

    fn describe(&self) -> String {
        let rows = match self.total {
            Some(n) => format!("{n} rows"),
            None => format!("{}+ rows", self.window.end),
        };
        format!("{} · {rows} × {} cols · duckdb", self.label, self.columns.len())
    }
}

/// The table function that reads `path`, by extension.
fn table_function(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("parquet" | "pq") => Some("read_parquet"),
        Some("json" | "jsonl" | "ndjson") => Some("read_json_auto"),
        Some("arrow" | "feather" | "ipc") => Some("read_arrow"),
        Some("csv" | "tsv" | "tab" | "txt") => Some("read_csv_auto"),
        _ => None,
    }
}

/// Column names and types of `sql`'s result, via DuckDB's own `DESCRIBE`.
///
/// `DESCRIBE` rather than reaching into a prepared statement's metadata: it is a
/// documented statement with a stable shape, and it does not execute the query.
fn describe(conn: &Connection, sql: &str) -> Result<Vec<Column>> {
    let mut stmt = conn
        .prepare(&format!("DESCRIBE {sql}"))
        .context("describing the query")?;
    let mut rows = stmt.query([]).context("describing the query")?;
    let mut columns = Vec::new();
    while let Some(row) = rows.next().context("reading a column description")? {
        let name: String = row.get(0).unwrap_or_default();
        let ty: String = row.get(1).unwrap_or_default();
        columns.push(Column {
            name: name.clone(),
            ty: column_type(&ty),
            // A window's worth of values sets the real width once fetched; the
            // header is the floor until then.
            width_hint: name.chars().count(),
        });
    }
    Ok(columns)
}

/// Total rows in `sql`'s result.
fn count(conn: &Connection, sql: &str) -> Result<usize> {
    let n: i64 = conn
        .query_row(&format!("SELECT count(*) FROM ({sql}) AS \"src\""), [], |r| r.get(0))
        .context("counting rows")?;
    Ok(n.max(0) as usize)
}

/// Map a DuckDB type name onto the grid's four types.
///
/// Only alignment and (later) comparison depend on this, so the mapping is
/// coarse on purpose: anything that isn't plainly a number or a boolean is text.
fn column_type(ty: &str) -> ColumnType {
    let ty = ty.to_ascii_uppercase();
    let base = ty.split('(').next().unwrap_or(&ty).trim();
    match base {
        "BOOLEAN" | "BOOL" | "LOGICAL" => ColumnType::Bool,
        "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "HUGEINT" | "INT1" | "INT2" | "INT4"
        | "INT8" | "INT" | "SIGNED" | "UTINYINT" | "USMALLINT" | "UINTEGER" | "UBIGINT"
        | "UHUGEINT" => ColumnType::Int,
        "FLOAT" | "REAL" | "FLOAT4" | "DOUBLE" | "FLOAT8" | "DECIMAL" | "NUMERIC" => {
            ColumnType::Float
        }
        _ => ColumnType::Text,
    }
}

/// `'…'` with embedded quotes doubled — a SQL string literal for `s`.
fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `"…"` with embedded double quotes doubled — a quoted SQL identifier for `s`.
fn sql_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A source is built on a background thread and handed to the UI thread, so it
/// has to cross a thread boundary.  Asserted at compile time because the failure
/// mode is a confusing error at the *call site* rather than here.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<DuckDbSource>();
};

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        open_readonly(None).expect("in-memory connection")
    }

    #[test]
    fn a_query_reports_its_columns_types_and_row_count() {
        let src = DuckDbSource::query(
            mem(),
            "SELECT 1 AS n, 'a' AS s, 1.5 AS f, true AS b",
            "test",
        )
        .expect("query runs");
        let cols: Vec<(&str, ColumnType)> =
            src.columns().iter().map(|c| (c.name.as_str(), c.ty)).collect();
        assert_eq!(
            cols,
            vec![
                ("n", ColumnType::Int),
                ("s", ColumnType::Text),
                ("f", ColumnType::Float),
                ("b", ColumnType::Bool),
            ],
        );
        assert_eq!(src.row_count(), Some(1));
        assert_eq!(src.cell(0, 0), Some("1"));
        assert_eq!(src.cell(0, 1), Some("a"));
        assert_eq!(src.cell(0, 3), Some("true"));
    }

    #[test]
    fn a_null_reads_as_empty_not_as_the_word_null() {
        // The grid draws an empty value as `table.null_display`, which is the
        // user's choice; the source must not pre-empt it with "NULL".
        let src = DuckDbSource::query(mem(), "SELECT NULL AS n", "test").unwrap();
        assert_eq!(src.cell(0, 0), Some(""));
    }

    #[test]
    fn rows_outside_the_window_are_absent_until_asked_for() {
        // The point of the backend: a result larger than the window is never
        // held in memory all at once.
        let mut src = DuckDbSource::query(
            mem(),
            "SELECT i FROM range(0, 5000) t(i)",
            "range",
        )
        .unwrap();
        assert_eq!(src.row_count(), Some(5000));
        assert_eq!(src.loaded_rows(), 5000, "the grid can address every row");
        assert_eq!(src.cell(0, 0), Some("0"));
        assert_eq!(src.cell(4999, 0), None, "not fetched yet");

        let fetches = src.fetch_count();
        src.ensure_rows(4990..5000);
        assert_eq!(src.cell(4999, 0), Some("4999"));
        assert_eq!(src.fetch_count(), fetches + 1, "exactly one round trip");

        // Scrolling within the window costs nothing — otherwise every frame
        // would issue a query.
        let fetches = src.fetch_count();
        src.ensure_rows(4991..5000);
        src.ensure_rows(4990..4999);
        assert_eq!(src.fetch_count(), fetches, "no refetch for a covered window");
    }

    #[test]
    fn a_mutating_statement_is_refused_before_it_reaches_the_engine() {
        let err = DuckDbSource::query(mem(), "DROP TABLE t", "test")
            .err()
            .expect("a mutating statement must not run");
        assert!(
            err.to_string().contains("writes go through code"),
            "got {err}",
        );
    }

    #[test]
    fn the_connection_refuses_a_write_even_when_the_gate_is_bypassed() {
        // Layer 1 has to hold on its own: this is the same statement the gate
        // rejects, run straight at a read-only connection.  If this ever starts
        // succeeding, the gate is the only thing standing between a keystroke
        // and someone's data.
        let dir = std::env::temp_dir().join(format!("sv-duck-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ro.duckdb");
        let _ = std::fs::remove_file(&path);

        // Create a database to attach — deliberately through a *separate*
        // writable connection, which is the only way the editor ever gets one
        // (and it doesn't: this is test setup).
        {
            let conn = Connection::open(&path).expect("create the fixture");
            conn.execute_batch("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
                .expect("populate the fixture");
        }

        let conn = open_readonly(Some(&path)).expect("read-only connection");
        assert!(
            conn.execute_batch("INSERT INTO t VALUES (2)").is_err(),
            "a read-only connection must refuse an insert",
        );
        assert!(
            conn.execute_batch("DROP TABLE t").is_err(),
            "a read-only connection must refuse a drop",
        );
        // ...and still reads.
        let n: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .expect("reads still work");
        assert_eq!(n, 1, "the write must not have landed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_parquet_file_opens_with_its_column_types() {
        let dir = std::env::temp_dir().join(format!("sv-duck-pq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture.parquet");
        let _ = std::fs::remove_file(&path);

        // Written by DuckDB itself, so the fixture needs no committed binary and
        // no second parquet implementation to disagree with.
        {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "COPY (SELECT i AS id, i * 1.5 AS score, 'row' || i AS name \
                 FROM range(0, 3) t(i)) TO '{}' (FORMAT PARQUET)",
                path.display(),
            ))
            .expect("write the parquet fixture");
        }

        let src = DuckDbSource::open_file(&path).expect("open the parquet");
        let cols: Vec<(&str, ColumnType)> =
            src.columns().iter().map(|c| (c.name.as_str(), c.ty)).collect();
        assert_eq!(
            cols,
            vec![
                ("id", ColumnType::Int),
                ("score", ColumnType::Float),
                ("name", ColumnType::Text),
            ],
        );
        assert_eq!(src.row_count(), Some(3));
        assert_eq!(src.cell(2, 2), Some("row2"));
        assert!(src.describe().contains("duckdb"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_with_a_quote_in_it_cannot_become_sql() {
        // The filename is a bound literal, not string-glued into the query.
        let dir = std::env::temp_dir().join(format!("sv-duck-quote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("it's a file.csv");
        std::fs::write(&path, "a,b\n1,2\n").unwrap();

        let src = DuckDbSource::open_file(&path).expect("open a quoted filename");
        assert_eq!(src.row_count(), Some(1));
        assert_eq!(src.cell(0, 0), Some("1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_column_names_do_not_collide() {
        // `SELECT a, a` is legal and the grid addresses columns positionally, so
        // the projection must not alias two columns to one name.
        let src = DuckDbSource::query(mem(), "SELECT 1 AS a, 2 AS a", "dup").unwrap();
        assert_eq!(src.columns().len(), 2);
        assert_eq!(src.cell(0, 0), Some("1"));
        assert_eq!(src.cell(0, 1), Some("2"));
    }

    #[test]
    fn a_column_name_with_a_quote_survives_the_projection() {
        let src = DuckDbSource::query(mem(), "SELECT 1 AS \"we\"\"ird\"", "q").unwrap();
        assert_eq!(src.columns()[0].name, "we\"ird");
        assert_eq!(src.cell(0, 0), Some("1"));
    }

    /// The test the whole two-path design rests on.
    ///
    /// A transform executed by the engine and the same transform executed by
    /// scanning must produce the same grid, cell for cell.  Without this,
    /// pushdown drifts from local execution and a filtered view quietly lies
    /// about the data — the one failure a read-only viewer can still commit.
    #[test]
    fn pushdown_and_local_execution_agree() {
        use crate::table::transform::{apply_local, Agg, Predicate, Transform};
        use crate::table::MemSource;

        let rows: &[&[&str]] = &[
            &["oslo", "10"],
            &["oslo", "7"],
            &["oslo", ""],
            &["lima", "3"],
            &["lima", "5"],
            &["bern", "1"],
            // A group whose every value is missing: `sum` of nothing is NULL,
            // not zero, and both paths have to say so.
            &["kyiv", ""],
        ];
        let local_src = MemSource::new(&["city", "qty"], rows);
        let duck = DuckDbSource::query(
            mem(),
            "SELECT * FROM (VALUES ('oslo', 10), ('oslo', 7), ('oslo', NULL), \
                                   ('lima', 3), ('lima', 5), ('bern', 1), \
                                   ('kyiv', NULL)) AS t(city, qty)",
            "fixture",
        )
        .expect("fixture query");

        for op in [
            Transform::Sort { col: 1, desc: false },
            Transform::Sort { col: 1, desc: true },
            Transform::Sort { col: 0, desc: false },
            Transform::Filter { col: 1, pred: Predicate::Gt(4.0) },
            Transform::Filter { col: 0, pred: Predicate::Eq("oslo".into()) },
            // A NULL is not "not oslo" in SQL, but it *is* in the grid, where a
            // missing value is an empty string.  The two paths have to agree on
            // that too, so the pushdown admits NULL explicitly.
            Transform::Filter { col: 0, pred: Predicate::Ne("oslo".into()) },
            Transform::Filter { col: 1, pred: Predicate::IsNull },
            Transform::GroupBy { keys: vec![0], aggs: vec![] },
            Transform::GroupBy { keys: vec![0], aggs: vec![(1, Agg::Sum), (1, Agg::Mean)] },
        ] {
            let local = apply_local(&local_src, &op, usize::MAX);
            let pushed = duck.derive(&op).expect("duckdb pushes every transform down");

            let label = format!("{op:?}");
            assert_eq!(
                local.columns().iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
                pushed.columns().iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
                "column names differ for {label}",
            );
            assert_eq!(
                local.loaded_rows(),
                pushed.loaded_rows(),
                "row counts differ for {label}",
            );
            for r in 0..local.loaded_rows() {
                for c in 0..local.columns().len() {
                    assert_eq!(
                        local.cell(r, c),
                        pushed.cell(r, c),
                        "cell ({r},{c}) differs for {label}",
                    );
                }
            }
        }
    }

    #[test]
    fn type_names_map_onto_the_grids_four_types() {
        assert_eq!(column_type("BIGINT"), ColumnType::Int);
        assert_eq!(column_type("DECIMAL(18,3)"), ColumnType::Float);
        assert_eq!(column_type("BOOLEAN"), ColumnType::Bool);
        assert_eq!(column_type("VARCHAR"), ColumnType::Text);
        assert_eq!(column_type("TIMESTAMP"), ColumnType::Text);
        assert_eq!(column_type("STRUCT(a INTEGER)"), ColumnType::Text);
    }
}
