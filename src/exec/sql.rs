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
-- Ctrl+E runs the statement the cursor is in (or the selection).\n\
-- Keep as many queries here as you like, separated by `;`.\n\
-- Files can be queried by name:\n\
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
    // …and where `q` goes back to, for the same reason: once the switch has
    // happened there is nothing left on screen to infer it from.
    app.sql_origin = app.current_source_id();
    // Seed the template only the first time; after that the buffer holds
    // whatever the user last typed, which is the point of a scratch buffer.
    app.special_buffer_ropes
        .entry(SQL_BUFFER.to_string())
        .or_insert_with(|| ropey::Rope::from_str(TEMPLATE));
    super::switch_to_special_buffer(app, SQL_BUFFER);
    app.messages.show("SQL buffer — Ctrl+E runs the query");
}

/// `q` / `:bd` in the query buffer — leave it for whatever `:sql` was invoked
/// from, falling back to the scratch buffer.  Returns false when the SQL buffer
/// is not what's open, so the caller can carry on with its own close.
///
/// The query text is not lost: leaving a special buffer stashes its rope, and
/// `:sql` seeds the template only when there is nothing stashed.
pub(super) fn close_buffer(app: &mut App) -> bool {
    if !app.in_sql_buffer() {
        return false;
    }
    let origin = app
        .sql_origin
        .take()
        .filter(|id| id != &SourceId::virtual_named(SQL_BUFFER))
        .map(|id| id.to_path())
        .unwrap_or_else(|| std::path::PathBuf::from(crate::app::SCRATCH_BUFFER));
    super::open_path(app, &origin);
    true
}

/// `Ctrl+E` / `:run-query` — run a statement from the buffer and show the result.
///
/// *A* statement, not the whole buffer: the buffer is a scratch pad you keep
/// several queries in (the template itself ships one), and the engine runs one
/// statement at a time.  A selection wins if there is one; otherwise it is the
/// statement the cursor is in, which is what every SQL console does and what
/// makes appending a query below the last one work.
pub(super) fn run(app: &mut App) {
    if !app.in_sql_buffer() {
        app.messages.show("Not the SQL buffer (:sql opens it)");
        return;
    }
    let text = app.buffer.rope.to_string();
    // Keep the text: `switch_to_special_buffer` reads the stash, and running a
    // query navigates away from the buffer being run.
    app.special_buffer_ropes
        .insert(SQL_BUFFER.to_string(), app.buffer.rope.clone());

    let chars: Vec<char> = text.chars().collect();
    let sql: String = if app.selection.end() > app.selection.start() {
        let (a, b) = (app.selection.start(), app.selection.end().min(chars.len()));
        chars[a..b].iter().collect()
    } else {
        match statement_at(&chars, app.selection.head) {
            Some(range) => chars[range].iter().collect(),
            None => {
                app.messages.show("Nothing to run — the buffer has no statement");
                return;
            }
        }
    };
    run_query(app, &sql);
}

/// The statement `cursor` sits in, as a char range into `chars`.
///
/// Statements are split on top-level `;` — one inside a string literal or a
/// comment is text, not a separator.  A range that is only whitespace and
/// comments is not a statement, so a cursor parked on the trailing blank line
/// runs the last real query above it rather than reporting "nothing to run".
fn statement_at(chars: &[char], cursor: usize) -> Option<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut start = 0usize;
    let mut quote: Option<char> = None;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '-' if chars.get(i + 1) == Some(&'-') => {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                    continue;
                }
                '/' if chars.get(i + 1) == Some(&'*') => {
                    i += 2;
                    while i < chars.len() && !(chars[i - 1] == '*' && chars[i] == '/') {
                        i += 1;
                    }
                }
                // The `;` belongs to the statement it ends: the gate accepts a
                // trailing one, and keeping it means the ranges tile the buffer.
                ';' => {
                    ranges.push(start..i + 1);
                    start = i + 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    ranges.push(start..chars.len());

    let real: Vec<std::ops::Range<usize>> =
        ranges.into_iter().filter(|r| has_statement(&chars[r.clone()])).collect();
    real.iter()
        .find(|r| cursor < r.end)
        .or_else(|| real.last())
        .cloned()
}

/// Whether `chars` holds anything but whitespace and comments.
fn has_statement(chars: &[char]) -> bool {
    let text: String = chars.iter().collect();
    text.lines()
        .map(|l| l.split("--").next().unwrap_or("").trim())
        .any(|l| !l.is_empty() && l != ";")
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
        Err(e) => return show_error(app, &format!("{e:#}")),
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
        Err(e) => show_error(app, &format!("{e:#}")),
    }
}

/// Report a failed query.
///
/// An engine error is not one line: DuckDB answers a typo with the message, a
/// suggestion, and a `LINE n:` caret pointing at the offending token — and the
/// minibuffer is one row, so all of that except the first few words used to be
/// unreadable. Anything with more to say than fits there opens the same
/// scrollable float as hover and the cell peek, focused, so `j`/`k` walk it and
/// `Esc` closes it. A genuinely short error stays in the minibuffer, where a
/// float would be ceremony.
#[cfg(feature = "dataframe")]
fn show_error(app: &mut App, error: &str) {
    let error = error.trim();
    let first = error.lines().next().unwrap_or(error);
    app.messages.show(format!("SQL: {first}"));

    let inner = super::table::popup_text_width(app.viewport_width);
    if error.lines().count() <= 1 && crate::table::layout::display_width(error) + 5 <= inner {
        return;
    }
    let wrapped: Vec<String> = error
        .lines()
        .flat_map(|line| {
            crate::render_util::wrap_segments(line, inner)
                .into_iter()
                .map(|(_, seg)| seg.to_string())
        })
        .collect();
    app.popup = Some(crate::popup::Popup::text_focused(
        " SQL error ",
        &wrapped.join("\n"),
    ));
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

    /// A special buffer has no file, but it can still have a syntax — the
    /// wiring, not just the lexer, has to carry that.
    #[test]
    fn the_query_buffer_is_highlighted_as_sql() {
        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        assert!(app.highlighter.sql, "the *sql* buffer highlights as SQL");
        let spans = app.highlighter.highlight(&app.buffer.rope).expect("highlight runs");
        assert!(!spans.is_empty(), "the template alone has comments and a keyword");
        // ...and so does an ordinary `.sql` file.
        assert!(crate::highlight::Highlighter::new(Some(std::path::Path::new("q.sql"))).sql);
    }

    /// The query buffer used to be a trap: `:bd` refuses every `*…*` name, `q`
    /// was unbound, and the only way out was to run something.
    #[test]
    fn q_backs_out_of_the_sql_buffer_to_where_it_came_from() {
        let dir = std::env::temp_dir().join(format!("sv-sqlexit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.md");
        std::fs::write(&file, "hello\n").unwrap();

        let mut app = app();
        super::super::open_path(&mut app, &file);
        super::super::execute(&mut app, &Command::SqlBuffer);
        assert!(app.in_sql_buffer());
        // `q` is bound to BufferClose in the SQL buffer's override map.
        assert!(app
            .keymap
            .lookup_sql(&crate::keymap::KeyBinding::char('q'))
            .is_some_and(|c| matches!(c, [Command::BufferClose])));

        app.buffer.rope = ropey::Rope::from_str("SELECT 1\n");
        super::super::execute(&mut app, &Command::BufferClose);

        assert!(!app.in_sql_buffer(), "must leave the query buffer");
        assert_eq!(
            app.buffer.path.as_deref().map(SourceId::of),
            Some(SourceId::of(&file)),
            "and land back where :sql was invoked from",
        );
        // The query is kept — coming back shows what was typed, not the template.
        super::super::execute(&mut app, &Command::SqlBuffer);
        assert_eq!(app.buffer.rope.to_string(), "SELECT 1\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With nothing else open there is still a way out.
    #[test]
    fn q_falls_back_to_scratch_when_there_is_no_origin() {
        let mut app = app();
        app.sql_origin = None;
        super::super::execute(&mut app, &Command::SqlBuffer);
        app.sql_origin = None;
        super::super::execute(&mut app, &Command::BufferClose);
        assert!(!app.in_sql_buffer());
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

    /// The buffer is a scratch pad, and the template ships a statement of its
    /// own — so writing a query underneath it used to run into "one statement
    /// at a time", which reads as the editor ignoring what you just typed.
    #[test]
    fn a_query_written_under_the_template_is_the_one_that_runs() {
        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        let text = format!("{}\nSELECT 7 AS n\n", app.buffer.rope);
        app.buffer.rope = ropey::Rope::from_str(&text);
        // Cursor where you would leave it: at the end of what you just typed.
        app.selection = crate::selection::Selection::point(app.buffer.rope.len_chars() - 1);

        let chars: Vec<char> = text.chars().collect();
        let picked: String = chars[statement_at(&chars, app.selection.head).unwrap()]
            .iter()
            .collect();
        assert_eq!(picked.trim(), "SELECT 7 AS n");

        // ...and with the cursor back up on the template's own query, that one
        // runs instead — the cursor picks, nothing else.
        let on_template = text.find("SELECT 42").unwrap();
        let picked: String = chars[statement_at(&chars, on_template + 2).unwrap()]
            .iter()
            .collect();
        assert!(picked.contains("SELECT 42 AS answer;"));
        assert!(!picked.contains("SELECT 7"));
    }

    /// The buffer as it ships must run — template comments, trailing `;` and
    /// all.  It did not: the terminator survived into the subquery the window
    /// fetch wraps the statement in, and came back as a parser error against
    /// text the user never wrote.
    #[cfg(feature = "dataframe")]
    #[test]
    fn the_template_runs_as_it_ships() {
        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        super::super::execute(&mut app, &Command::SqlRun);

        let session = app.table.as_ref().unwrap_or_else(|| {
            panic!("the shipped template must run: {:?}", app.messages.current())
        });
        assert_eq!(session.source().cell(0, 0), Some("42"));

        // ...and so does the same statement with the comments deleted.
        super::super::execute(&mut app, &Command::TableCloseDerived);
        app.buffer.rope = ropey::Rope::from_str("SELECT 42 AS answer;\n");
        super::super::execute(&mut app, &Command::SqlRun);
        assert!(
            app.table.is_some(),
            "a bare statement with a trailing semicolon: {:?}",
            app.messages.current(),
        );
    }

    /// The same thing end to end, through the engine.
    #[cfg(feature = "dataframe")]
    #[test]
    fn appending_to_the_template_and_running_returns_the_new_result() {
        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        let text = format!("{}\nSELECT 7 AS n\n", app.buffer.rope);
        app.buffer.rope = ropey::Rope::from_str(&text);
        app.selection = crate::selection::Selection::point(app.buffer.rope.len_chars() - 1);
        super::super::execute(&mut app, &Command::SqlRun);

        let session = app.table.as_ref().expect("a grid, not a refusal");
        assert_eq!(session.source().columns()[0].name, "n");
        assert_eq!(session.source().cell(0, 0), Some("7"));
    }

    #[test]
    fn statements_split_on_top_level_semicolons_only() {
        let pick = |sql: &str, cursor: usize| -> String {
            let chars: Vec<char> = sql.chars().collect();
            chars[statement_at(&chars, cursor).unwrap()].iter().collect::<String>().trim().to_string()
        };
        // A `;` inside a literal or a comment separates nothing.
        assert_eq!(pick("SELECT ';' AS c", 0), "SELECT ';' AS c");
        assert_eq!(pick("SELECT 1 -- ; not a split\n", 0), "SELECT 1 -- ; not a split");
        assert_eq!(pick("SELECT /* ; */ 1", 0), "SELECT /* ; */ 1");
        // A cursor past the last statement, on the trailing blank line, still
        // runs the query above it rather than an empty range.
        assert_eq!(pick("SELECT 1;\n\n", 11), "SELECT 1;");
        // Nothing but comments is nothing to run.
        let chars: Vec<char> = "-- just a note\n".chars().collect();
        assert!(statement_at(&chars, 0).is_none());
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
        use crate::view::View;

        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        app.buffer.rope = ropey::Rope::from_str("SELECT i AS n FROM range(0, 3) t(i)\n");
        super::super::execute(&mut app, &Command::SqlRun);

        assert_eq!(app.view(), View::Table);
        let session = app.table.as_ref().expect("result grid");
        assert_eq!(session.source().row_count(), Some(3));
        assert_eq!(session.source().cell(2, 0), Some("2"));
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

    /// An engine error is several lines — the message, a suggestion, and a
    /// `LINE n:` caret under the offending token — and the minibuffer is one
    /// row. All but the first few words used to be unreadable.
    #[cfg(feature = "dataframe")]
    #[test]
    fn a_broken_query_opens_its_error_in_a_scrollable_float() {
        use crate::popup::PopupContent;

        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        app.buffer.rope = ropey::Rope::from_str("SELECT * FROM no_such_table\n");
        super::super::execute(&mut app, &Command::SqlRun);

        assert!(
            app.messages.log.iter().any(|m| m.starts_with("SQL:")),
            "{:?}",
            app.messages.log,
        );
        let popup = app.popup.as_ref().expect("the error opens a float");
        let PopupContent::Text(ref text) = popup.content else {
            panic!("an error is scrollable text");
        };
        assert!(
            text.focused,
            "focused from the first keypress — a passive float would be dismissed \
             by the next key, taking the explanation with it",
        );
        assert!(text.lines.len() > 1, "the whole error, not its first line");
        assert!(
            text.lines.iter().any(|l| l.contains("no_such_table")),
            "{:?}",
            text.lines,
        );
        // Every line fits the float, so nothing is clipped away.
        let inner = super::super::table::popup_text_width(app.viewport_width);
        assert!(text.lines.iter().all(|l| crate::table::layout::display_width(l) <= inner));

        // ...and the query text is untouched, as before.
        assert!(app.in_sql_buffer());
        assert!(app.buffer.rope.to_string().contains("no_such_table"));
    }

    /// A one-line refusal is a minibuffer message; a float there is ceremony.
    #[cfg(feature = "dataframe")]
    #[test]
    fn a_short_refusal_stays_in_the_minibuffer() {
        let mut app = app();
        super::super::execute(&mut app, &Command::SqlBuffer);
        app.buffer.rope = ropey::Rope::from_str("-- nothing here\n");
        super::super::execute(&mut app, &Command::SqlRun);
        assert!(app.popup.is_none(), "no float for a one-liner");
        assert!(app.messages.current().is_some());
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
        assert_eq!(session.source().row_count(), Some(2));
        assert_eq!(session.source().cell(1, 1), Some("y"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}


