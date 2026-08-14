//! `:attach` — read a local database file — and `:schema`, the browser over it.
//!
//! # Why there is no password here
//!
//! A local database file is a **path, not a secret**, so attaching one needs no
//! credential handling at all.  A remote or authenticated database is a
//! different thing entirely, and the editor deliberately does not reach it: the
//! user connects in a notebook cell with their own driver and their own auth,
//! and the result is viewed through the kernel bridge (`:view`).  That keeps
//! every credential in the channel that gets reviewed, versioned and re-run —
//! and it keeps DSN parsing, environment-variable precedence and TLS options,
//! all of them security-sensitive surface, out of the editor entirely.
//!
//! Everything attached here is attached `READ_ONLY`, by
//! [`duck::connect`](crate::table::duck::connect) — the editor issues those
//! statements itself precisely so that the flag is not something a typed query
//! can leave off.

use std::path::{Path, PathBuf};

use crate::app::App;
use crate::table::Attachment;

/// Identity of the schema browser's grid.  Stable, so re-running `:schema`
/// lands on the same buffer-list entry rather than piling up new ones.
const CATALOG: &str = "*schema*";

/// The directory a bare filename resolves against: whatever was open when the
/// command was issued.  `app.sql_dir` is the anchor captured on the way into the
/// path-less `*sql*` buffer, so a query and an attach agree on "here".
pub(super) fn anchor_dir(app: &App) -> Option<PathBuf> {
    // While a *virtual* source is what's open — the `*sql*` buffer, a query
    // result — there is no file to take a directory from, and asking for one
    // would answer with whatever directory the editor was launched in.  The
    // captured anchor is the only honest answer there.
    let virtual_source = super::table::current_source_id(app).map_or(true, |id| id.is_virtual());
    if virtual_source {
        if let Some(dir) = app.sql_dir.clone() {
            return Some(dir);
        }
    }
    super::buffers::sql_working_dir(app)
}

/// `:attach <path> [as <alias>]` — make a local database file readable.
pub(super) fn attach(app: &mut App, arg: &str) {
    let (path_arg, alias_arg) = split_as(arg);
    if path_arg.is_empty() {
        // Bare `:attach` is a question, not a mistake — answer it.
        app.messages.show(describe(app));
        return;
    }
    let path = resolve(app, path_arg);
    if !path.is_file() {
        app.messages
            .show(format!("No such database file: {}", path.display()));
        return;
    }
    let alias = match alias_arg {
        Some(a) => a.to_string(),
        None => default_alias(&path),
    };
    if app.attachments.iter().any(|a| a.alias == alias) {
        app.messages
            .show(format!("`{alias}` is already attached (:detach {alias} first)"));
        return;
    }

    let attachment = Attachment {
        alias: alias.clone(),
        kind: attach_kind(&path),
        path,
    };
    // Attach for real before recording it: an alias that only fails when the
    // next query runs is worse than one that fails now, and the failure text
    // (a missing extension, a lock, a file that isn't a database) is the useful
    // part of the answer.
    match verify(app, &attachment) {
        Ok(()) => {
            let where_ = attachment.path.display().to_string();
            app.attachments.push(attachment);
            app.messages
                .show(format!("Attached {where_} as `{alias}` (read-only) — :schema to browse"));
        }
        Err(e) => app.messages.show(e),
    }
}

/// `:detach [alias]` — drop one attachment, or all of them when bare.
pub(super) fn detach(app: &mut App, arg: &str) {
    let alias = arg.trim();
    if alias.is_empty() {
        let n = app.attachments.len();
        app.attachments.clear();
        app.messages.show(match n {
            0 => "Nothing attached".to_string(),
            1 => "Detached".to_string(),
            n => format!("Detached {n} databases"),
        });
        return;
    }
    let before = app.attachments.len();
    app.attachments.retain(|a| a.alias != alias);
    if app.attachments.len() == before {
        app.messages.show(format!("`{alias}` is not attached"));
    } else {
        app.messages.show(format!("Detached `{alias}`"));
    }
}

/// Split `path as alias` into its two halves.  The keyword is optional
/// (`:attach f.duckdb sales` works too), since the `as` is the SQL habit and
/// the second word is unambiguous either way.
fn split_as(arg: &str) -> (&str, Option<&str>) {
    let arg = arg.trim();
    let mut parts = arg.split_whitespace();
    let Some(path) = parts.next() else {
        return ("", None);
    };
    let rest: Vec<&str> = parts.collect();
    match rest.as_slice() {
        [] => (path, None),
        ["as", alias] => (path, Some(*alias)),
        [alias] => (path, Some(*alias)),
        _ => (path, None),
    }
}

/// Absolute path for a `:attach` argument, with `~` and a relative path
/// resolved against whatever was open when the command was issued.
fn resolve(app: &App, arg: &str) -> PathBuf {
    let expanded = if let Some(rest) = arg.strip_prefix("~/") {
        dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(arg))
    } else {
        PathBuf::from(arg)
    };
    if expanded.is_absolute() {
        return expanded;
    }
    match anchor_dir(app) {
        Some(dir) => dir.join(expanded),
        None => expanded,
    }
}

/// A SQL-safe name for a database file: its stem, with anything that isn't a
/// word character replaced.  The alias is an identifier in every query that
/// follows, so it has to be one.
fn default_alias(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("db");
    let cleaned: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    match cleaned.chars().next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => cleaned,
        // An identifier can't start with a digit, and an empty stem is possible.
        _ => format!("db_{cleaned}"),
    }
}

// ---------------------------------------------------------------------------
// Engine-facing half
// ---------------------------------------------------------------------------

#[cfg(feature = "dataframe")]
pub(super) use with_engine::{connection, open_catalog_row, open_schema_browser, verify};

#[cfg(feature = "dataframe")]
fn attach_kind(path: &Path) -> Option<&'static str> {
    crate::table::duck::attach_kind(path)
}

#[cfg(feature = "dataframe")]
mod with_engine {
    use super::{App, CATALOG};
    use crate::source::SourceId;
    use crate::table::{duck, Attachment, TableSource};

    /// A connection to the editor's scratch database with every attachment
    /// replayed.  The one way anything in the editor reaches the engine.
    pub(in crate::exec) fn connection(app: &App) -> anyhow::Result<duckdb::Connection> {
        duck::connect(&app.attachments, super::anchor_dir(app).as_deref())
    }

    /// Attach `candidate` on a throwaway connection to prove it works.
    pub(in crate::exec) fn verify(app: &App, candidate: &Attachment) -> Result<(), String> {
        let mut all = app.attachments.clone();
        all.push(candidate.clone());
        duck::connect(&all, super::anchor_dir(app).as_deref())
            .map(|_| ())
            .map_err(|e| match candidate.kind {
                // The sqlite reader is a DuckDB extension, and the editor
                // deliberately never runs INSTALL/LOAD — loading native code at
                // runtime is exactly the hole the statement gate exists to keep
                // shut.  Say where the door is instead.
                Some("SQLITE") => format!(
                    "{e:#} — SQLite needs DuckDB's sqlite extension; \
                     read it in a notebook cell instead and view it with :view"
                ),
                _ => format!("{e:#}"),
            })
    }

    /// `:schema` / `gt` — every table in every attached database, as a grid.
    ///
    /// No new view: a catalog *is* tabular data, so it is the ordinary grid over
    /// a query, with `Enter` on a row opening that table.
    pub(in crate::exec) fn open_schema_browser(app: &mut App) {
        if app.attachments.is_empty() {
            app.messages
                .show("Nothing attached — :attach <file.duckdb> first");
            return;
        }
        let conn = match connection(app) {
            Ok(conn) => conn,
            Err(e) => return app.messages.show(format!("Schema: {e:#}")),
        };
        let source = match duck::DuckDbSource::query(conn, duck::catalog_query(), "schema") {
            Ok(source) => source,
            Err(e) => return app.messages.show(format!("Schema: {e:#}")),
        };
        let n = source.row_count().unwrap_or(0);
        let origin = super::super::table::current_source_id(app);
        super::super::table::open_derived(
            app,
            SourceId::virtual_named(CATALOG),
            Box::new(source),
            origin,
        );
        // A catalog row is a *table*, so Enter opens it rather than reading the
        // cell's text — see `table::Drill`.
        if let Some(session) = app.table.as_mut() {
            session.drill = Some(super::super::table::Drill::Catalog);
        }
        app.messages
            .show(format!("{n} table(s) — Enter opens one, q goes back"));
    }

    /// `Enter` on a schema-browser row — open that table in the grid.
    pub(in crate::exec) fn open_catalog_row(app: &mut App) {
        let Some(session) = app.table.as_ref() else { return };
        let by_name = |name: &str| -> Option<String> {
            let idx = session.source.columns().iter().position(|c| c.name == name)?;
            session.source.cell(session.state.cursor_row, idx).map(str::to_string)
        };
        let (Some(db), Some(schema), Some(table)) =
            (by_name("database"), by_name("schema"), by_name("name"))
        else {
            app.messages.show("No table on this row");
            return;
        };

        let sql = duck::table_query(&db, &schema, &table);
        let label = format!("{db}.{schema}.{table}");
        let conn = match connection(app) {
            Ok(conn) => conn,
            Err(e) => return app.messages.show(format!("{label}: {e:#}")),
        };
        match duck::DuckDbSource::query(conn, &sql, label.clone()) {
            Ok(source) => {
                let shape = source.describe();
                let origin = session.id.clone();
                super::super::table::open_derived(
                    app,
                    SourceId::virtual_named(&label),
                    Box::new(source),
                    Some(origin),
                );
                app.messages.show(format!("{shape} — q goes back"));
            }
            Err(e) => app.messages.show(format!("{label}: {e:#}")),
        }
    }
}

// --- built without an engine -----------------------------------------------

#[cfg(not(feature = "dataframe"))]
fn attach_kind(_path: &Path) -> Option<&'static str> {
    None
}

#[cfg(not(feature = "dataframe"))]
fn verify(_app: &App, _candidate: &Attachment) -> Result<(), String> {
    Err("Built without the `dataframe` feature — no database engine".to_string())
}

#[cfg(not(feature = "dataframe"))]
pub(super) fn open_schema_browser(app: &mut App) {
    app.messages
        .show("Built without the `dataframe` feature — no database engine");
}

#[cfg(not(feature = "dataframe"))]
pub(super) fn open_catalog_row(app: &mut App) {
    let _ = app;
}

/// The attachments, as bare `:attach` reports them.
fn describe(app: &App) -> String {
    if app.attachments.is_empty() {
        return "Nothing attached — :attach <file.duckdb> [as <alias>]".to_string();
    }
    let names: Vec<String> = app
        .attachments
        .iter()
        .map(|a| format!("{} → {}", a.alias, a.path.display()))
        .collect();
    names.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_alias_is_derived_from_the_filename_and_is_a_valid_identifier() {
        assert_eq!(default_alias(Path::new("/tmp/analytics.duckdb")), "analytics");
        assert_eq!(default_alias(Path::new("/tmp/my-data.db")), "my_data");
        // An identifier can't start with a digit.
        assert_eq!(default_alias(Path::new("/tmp/2024.duckdb")), "db_2024");
    }

    /// The whole loop: attach a database file, browse its tables, open one with
    /// `Enter`, and `q` back out — the paths that only exist once all three
    /// pieces are wired together.
    #[cfg(feature = "dataframe")]
    #[test]
    fn a_local_database_attaches_read_only_and_its_tables_are_browsable() {
        use crate::app::View;
        use crate::command::Command;
        use crate::config::Config;

        let dir = std::env::temp_dir().join(format!("sv-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("analytics.duckdb");
        let _ = std::fs::remove_file(&db);
        {
            // Test setup writes through a writable connection — the only kind
            // the editor never opens.
            let conn = duckdb::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE sales (id INTEGER, city VARCHAR); \
                 INSERT INTO sales VALUES (1, 'oslo'), (2, 'lima');",
            )
            .unwrap();
        }

        let mut app = App::new(None, Config::load()).unwrap();
        app.viewport_height = 20;
        app.viewport_width = 80;
        app.buffer.path = Some(dir.join("notes.md"));

        // A relative path resolves against what you were looking at, and the
        // alias comes from the filename.
        super::super::execute(&mut app, &Command::Attach("analytics.duckdb".into()));
        assert_eq!(app.attachments.len(), 1, "{:?}", app.messages.log);
        assert_eq!(app.attachments[0].alias, "analytics");

        super::super::execute(&mut app, &Command::SchemaBrowser);
        assert_eq!(app.view(), View::Table);
        let session = app.table.as_ref().expect("schema browser open");
        assert_eq!(session.drill, Some(super::super::table::Drill::Catalog));
        let names: Vec<&str> = session.source.columns().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["database", "schema", "name", "type", "columns"]);
        assert_eq!(session.source.cell(0, 0), Some("analytics"));
        assert_eq!(session.source.cell(0, 2), Some("sales"));

        // `Enter` on a catalog row opens that table rather than reading the
        // cell's text — the row *names* something.
        super::super::execute(&mut app, &Command::TableOpenCell);
        let opened = app.table.as_ref().expect("the table opened");
        assert_eq!(opened.display_name(), "*analytics.main.sales*");
        assert_eq!(opened.source.row_count(), Some(2));
        assert_eq!(opened.source.cell(1, 1), Some("lima"));

        // ...and `q` walks back out: table → catalog, the same paradigm as any
        // other computed view.
        super::super::execute(&mut app, &Command::TableCloseDerived);
        assert_eq!(
            app.table.as_ref().map(|s| s.display_name()),
            Some("*schema*".to_string()),
        );

        // The write path is shut at the connection, not merely at the gate.
        let conn = connection(&app).expect("connection");
        assert!(conn.execute_batch("INSERT INTO analytics.main.sales VALUES (3, 'bern')").is_err());

        super::super::execute(&mut app, &Command::Detach(String::new()));
        assert!(app.attachments.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_as_keyword_is_optional() {
        assert_eq!(split_as("a.duckdb as sales"), ("a.duckdb", Some("sales")));
        assert_eq!(split_as("a.duckdb sales"), ("a.duckdb", Some("sales")));
        assert_eq!(split_as("  a.duckdb  "), ("a.duckdb", None));
        assert_eq!(split_as(""), ("", None));
    }
}
