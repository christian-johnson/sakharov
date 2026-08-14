//! `:sql` — a scratch buffer you run as a query.
//!
//! The query is ordinary buffer text, so it is edited with the editor's own
//! motions, undo, and search rather than a bespoke input widget; the execute
//! keys (`Ctrl+E`, `Shift`/`Ctrl+Enter`) run it, the same keys that run a
//! notebook cell. The result opens as a grid under a virtual identity, and `q`
//! goes back to the query — so editing and re-running is a loop, not a
//! round trip through the file system.
//!
//! Everything the query can do is bounded by
//! [`table::duck`](crate::table::duck): a read-only connection and the statement
//! gate. This module only decides *where the text comes from* and *where the
//! result goes*.

use crate::app::{App, SQL_BUFFER};
use crate::source::SourceId;
#[cfg(feature = "dataframe")]
use crate::table::TableSource;

/// The starter text of an empty `*sql*` buffer.
///
/// A comment rather than an empty buffer: the whole feature is invisible
/// otherwise, and the one thing nobody guesses is that a file can be queried
/// directly by name.
const TEMPLATE: &str = "\
-- Ctrl+E runs this query.  Files can be queried by name:\n\
--   SELECT * FROM 'data.csv' WHERE amount > 100\n\
--   SELECT * FROM read_parquet('sales.parquet') LIMIT 100\n\
-- Reads only: a statement that would write is refused.\n\
\n\
SELECT 42 AS answer;\n";

/// Identity of the result grid.
///
/// Stable across runs on purpose: iterating on one query is the common case, and
/// a fresh id per run would both lose your place and pile `*sql 1*`, `*sql 2*` …
/// into the buffer list.
const RESULT: &str = "*sql result*";

/// `:sql` — open (or return to) the query buffer.
pub(super) fn open_buffer(app: &mut App) {
    if app.in_sql_buffer() {
        app.messages.show("Already in the SQL buffer — Ctrl+E runs it");
        return;
    }
    // Anchor relative filenames to what you were last looking at, before the
    // switch replaces `app.buffer` with the path-less SQL buffer.
    app.sql_dir = super::buffers::sql_working_dir(app);
    // Seed the template only the first time; after that the buffer holds
    // whatever the user last typed, which is the point of a scratch buffer.
    app.special_buffer_ropes
        .entry(SQL_BUFFER.to_string())
        .or_insert_with(|| ropey::Rope::from_str(TEMPLATE));
    super::switch_to_special_buffer(app, SQL_BUFFER);
    app.messages.show("SQL buffer — Ctrl+E runs the query");
}

/// `Ctrl+E` / `:run-query` — run the buffer's contents and show the result.
pub(super) fn run(app: &mut App) {
    if !app.in_sql_buffer() {
        app.messages.show("Not the SQL buffer (:sql opens it)");
        return;
    }
    let sql = app.buffer.rope.to_string();
    // Keep the text: `switch_to_special_buffer` reads the stash, and running a
    // query navigates away from the buffer being run.
    app.special_buffer_ropes
        .insert(SQL_BUFFER.to_string(), app.buffer.rope.clone());
    run_query(app, &sql);
}

#[cfg(feature = "dataframe")]
fn run_query(app: &mut App, sql: &str) {
    use crate::table::duck::DuckDbSource;

    // One connection builder for the whole editor: an in-memory scratch database
    // anchored at the working directory, with every `:attach`ed database hanging
    // off it read-only.  So a query can join a parquet file to a table in an
    // attached database without either of them being writable.
    let conn = match super::attach::connection(app) {
        Ok(conn) => conn,
        Err(e) => {
            app.messages.show(format!("SQL: {e:#}"));
            return;
        }
    };

    let label = first_line(sql);
    match DuckDbSource::query(conn, sql, label) {
        Ok(source) => {
            let rows = source.row_count();
            let cols = source.columns().len();
            let id = SourceId::virtual_named(RESULT);
            super::table::open_derived(
                app,
                id,
                Box::new(source),
                Some(SourceId::virtual_named(SQL_BUFFER)),
            );
            app.messages.show(match rows {
                Some(n) => format!("{n} row(s) × {cols} cols — q to edit the query"),
                None => format!("{cols} cols — q to edit the query"),
            });
        }
        // A refused or failing query leaves the buffer alone: the text is the
        // thing being iterated on, and losing it to a typo would be hostile.
        Err(e) => app.messages.show(format!("SQL: {e:#}")),
    }
}

#[cfg(not(feature = "dataframe"))]
fn run_query(app: &mut App, _sql: &str) {
    app.messages
        .show("Built without the `dataframe` feature — no SQL engine");
}

/// A short label for the result grid: the query's first non-comment line.
fn first_line(sql: &str) -> String {
    let line = sql
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or("query");
    let short: String = line.chars().take(40).collect();
    short.trim_end_matches(';').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::config::Config;

    fn app() -> App {
        let mut app = App::new(None, Config::load()).expect("app");
        app.viewport_height = 20;
        app.viewport_width = 80;
        app
    }

    #[test]
    fn the_buffer_opens_with_a_template_and_keeps_edits() {
        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        assert!(app.in_sql_buffer());
        assert!(app.buffer.rope.to_string().contains("Ctrl+E"));

        // Edit it, navigate away, come back: it is a scratch buffer, so the text
        // is what you last left.
        app.buffer.rope = ropey::Rope::from_str("SELECT 7 AS n\n");
        super::super::execute(&mut app, &Command::SqlRun);
        super::super::execute(&mut app, &Command::SqlBuffer);
        assert_eq!(app.buffer.rope.to_string(), "SELECT 7 AS n\n");
    }

    #[test]
    fn run_outside_the_sql_buffer_says_where_to_go() {
        let mut app = app();
        super::super::execute(&mut app, &Command::SqlRun);
        assert!(
            app.messages.log.iter().any(|m| m.contains(":sql opens it")),
            "{:?}",
            app.messages.log,
        );
    }

    #[test]
    fn the_result_label_is_the_first_real_line() {
        assert_eq!(first_line("-- a comment\n\nSELECT 1;\n"), "SELECT 1");
        assert_eq!(first_line("   \n"), "query");
        assert_eq!(first_line(&"x".repeat(80)).len(), 40);
    }

    #[cfg(feature = "dataframe")]
    #[test]
    fn a_query_opens_its_result_as_a_grid_and_q_returns_to_the_query() {
        use crate::app::View;

        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        app.buffer.rope = ropey::Rope::from_str("SELECT i AS n FROM range(0, 3) t(i)\n");
        super::super::execute(&mut app, &Command::SqlRun);

        assert_eq!(app.view(), View::Table);
        let session = app.table.as_ref().expect("result grid");
        assert_eq!(session.source.row_count(), Some(3));
        assert_eq!(session.source.cell(2, 0), Some("2"));
        assert!(session.id.is_virtual());

        // `q` goes back to the query text, so editing and re-running is a loop.
        super::super::execute(&mut app, &Command::TableCloseDerived);
        assert!(app.in_sql_buffer(), "back in the query buffer");
        assert!(app.buffer.rope.to_string().contains("range(0, 3)"));
    }

    #[cfg(feature = "dataframe")]
    #[test]
    fn a_query_that_writes_is_refused_and_the_text_is_kept() {
        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        app.buffer.rope = ropey::Rope::from_str("DROP TABLE t\n");
        super::super::execute(&mut app, &Command::SqlRun);

        // Refused, with the reason and where writes belong...
        assert!(
            app.messages.log.iter().any(|m| m.contains("writes go through code")),
            "{:?}",
            app.messages.log,
        );
        // ...and still in the buffer with the text intact: losing a query to a
        // refusal would be hostile.
        assert!(app.in_sql_buffer());
        assert_eq!(app.buffer.rope.to_string(), "DROP TABLE t\n");
        assert!(app.table.is_none());
    }

    #[cfg(feature = "dataframe")]
    #[test]
    fn a_broken_query_reports_the_engine_error_without_losing_the_text() {
        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        app.buffer.rope = ropey::Rope::from_str("SELECT * FROM no_such_table\n");
        super::super::execute(&mut app, &Command::SqlRun);
        assert!(
            app.messages.log.iter().any(|m| m.starts_with("SQL:")),
            "{:?}",
            app.messages.log,
        );
        assert!(app.in_sql_buffer());
        assert!(app.buffer.rope.to_string().contains("no_such_table"));
    }

    #[cfg(feature = "dataframe")]
    #[test]
    fn a_file_can_be_queried_by_name_relative_to_the_buffer() {
        let dir = std::env::temp_dir().join(format!("sv-sqlrel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.csv"), "a,b\n1,x\n2,y\n").unwrap();

        let mut app = app();
        // Anchor the working directory the way an open file would.
        app.buffer.path = Some(dir.join("notes.md"));
        super::super::execute(&mut app, &Command::SqlBuffer);
        app.buffer.rope = ropey::Rope::from_str("SELECT * FROM 'data.csv'\n");
        super::super::execute(&mut app, &Command::SqlRun);

        let session = app.table.as_ref().expect("queried the file by name");
        assert_eq!(session.source.row_count(), Some(2));
        assert_eq!(session.source.cell(1, 1), Some("y"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
