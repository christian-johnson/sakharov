//! The statement gate: what SQL the editor is willing to run.
//!
//! **This is not the read-only guarantee.**  The guarantee is that the editor
//! never holds a writable handle on a database — every `ATTACH` carries
//! `READ_ONLY` and a file-backed connection is opened in read-only access mode
//! (see [`super::open_readonly`]).  The gate exists for the *error message*: it
//! catches a mutating statement before the engine does, and says what to do
//! instead, which a raw engine error cannot.
//!
//! It is an allowlist on the leading keyword, which is a heuristic — hence its
//! place as the second layer rather than the first.  If it ever needs to get
//! cleverer, the robust upgrade is to ask DuckDB's own parser for the statement
//! type with `json_serialize_sql()`, which parses without executing.

/// Statements that only read.  Anything not on this list is rejected, so a
/// keyword nobody anticipated fails closed.
const ALLOWED: &[&str] = &[
    "SELECT",
    "WITH",
    // DuckDB accepts `FROM tbl SELECT …` and bare `FROM tbl`; a gate that only
    // knows SELECT rejects idiomatic DuckDB.
    "FROM",
    "DESCRIBE",
    "SUMMARIZE",
    "SHOW",
    "EXPLAIN",
    "VALUES",
    "TABLE",
    "PIVOT",
    "UNPIVOT",
];

/// Read-only `PRAGMA`s.  `PRAGMA` as a family is not safe — it can enable
/// extensions and change settings — so it is allowed only by name.
const ALLOWED_PRAGMAS: &[&str] = &[
    "database_list",
    "database_size",
    "show_tables",
    "show_tables_expanded",
    "table_info",
    "version",
    "platform",
    "database_version",
];

/// Why a statement was refused, phrased for the minibuffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected(pub String);

impl Rejected {
    fn writes(keyword: &str) -> Self {
        Self(format!(
            "`{keyword}` would modify data — writes go through code, not the viewer. \
             Put the statement in a notebook cell where it can be reviewed and committed."
        ))
    }
}

/// Accept `sql` if it only reads.
///
/// Rejects on three grounds: a leading keyword that isn't on the allowlist, a
/// second statement after the first, and an empty input.
pub fn check(sql: &str) -> Result<(), Rejected> {
    let stripped = strip_comments(sql);
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return Err(Rejected("Nothing to run".to_string()));
    }

    // One statement per run.  Without this, `SELECT 1; DROP TABLE t` passes on
    // the strength of its first keyword.
    if let Some(rest) = trimmed.split_once(';').map(|(_, rest)| rest.trim()) {
        if !rest.is_empty() {
            return Err(Rejected(
                "One statement at a time — the editor runs a single query, so a \
                 second statement after the `;` would be a surprise."
                    .to_string(),
            ));
        }
    }

    let keyword: String = trimmed
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_uppercase();

    if ALLOWED.contains(&keyword.as_str()) {
        return Ok(());
    }
    if keyword == "PRAGMA" || keyword == "CALL" {
        let name = pragma_name(trimmed);
        return if ALLOWED_PRAGMAS.iter().any(|p| name.eq_ignore_ascii_case(p)) {
            Ok(())
        } else {
            Err(Rejected(format!(
                "`{keyword} {name}` isn't on the read-only list — it could change \
                 settings or load an extension."
            )))
        };
    }
    if keyword.is_empty() {
        return Err(Rejected(
            "That doesn't start with a statement the viewer can run".to_string(),
        ));
    }
    Err(Rejected::writes(&keyword))
}

/// The name after `PRAGMA` / `CALL`, up to the first `(` or whitespace.
fn pragma_name(sql: &str) -> String {
    sql.split_once(|c: char| c.is_whitespace())
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// Blank out `--` and `/* */` comments so they can't hide a keyword or a `;`.
///
/// Replaces rather than removes, so byte offsets in the result still line up
/// with the input (and a comment can't glue two words together).  Quoted text is
/// respected: a `--` inside a string literal is data, not a comment.
fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                out.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
                out.push(' ');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    if c == '\n' {
                        out.push('\n');
                    }
                    prev = c;
                }
                out.push(' ');
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(sql: &str) -> bool {
        check(sql).is_ok()
    }

    #[test]
    fn analytical_queries_are_allowed() {
        for sql in [
            "SELECT * FROM t",
            "select 1",
            "  \n SELECT 1  ",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            // DuckDB's FROM-first syntax, which is idiomatic and must not be
            // rejected by a gate that only knows about SELECT.
            "FROM t",
            "FROM t SELECT a, b",
            "FROM read_parquet('x.parquet')",
            "DESCRIBE SELECT * FROM t",
            "SUMMARIZE t",
            "SHOW TABLES",
            "EXPLAIN SELECT 1",
            "VALUES (1), (2)",
            "TABLE t",
            "PIVOT t ON a USING sum(b)",
            "SELECT * FROM t; ",
            "SELECT * FROM t;",
        ] {
            assert!(allowed(sql), "should be allowed: {sql}");
        }
    }

    #[test]
    fn anything_that_writes_is_rejected() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "insert into t values (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "DROP TABLE t",
            "CREATE TABLE t (a INT)",
            "CREATE OR REPLACE VIEW v AS SELECT 1",
            "ALTER TABLE t ADD COLUMN b INT",
            "TRUNCATE t",
            "MERGE INTO t USING s ON t.a = s.a",
            // COPY … TO and EXPORT are the ways to write a *file* even from a
            // connection that cannot write its database.
            "COPY t TO 'out.csv'",
            "EXPORT DATABASE 'dir'",
            // The editor issues its own ATTACHes, always READ_ONLY; a typed one
            // could attach something writable.
            "ATTACH 'other.db' AS other",
            "DETACH other",
            "INSTALL httpfs",
            "LOAD httpfs",
            // SET could re-enable whatever the gate is guarding.
            "SET memory_limit = '1GB'",
            "BEGIN TRANSACTION",
        ] {
            assert!(!allowed(sql), "should be rejected: {sql}");
        }
    }

    #[test]
    fn a_second_statement_cannot_ride_along() {
        // The classic: passes a leading-keyword check, drops a table.
        let err = check("SELECT 1; DROP TABLE t").unwrap_err();
        assert!(err.0.contains("One statement"), "got {}", err.0);
        assert!(!allowed("SELECT 1;DROP TABLE t"));
        assert!(!allowed("SELECT 1 ; INSERT INTO t VALUES (1)"));
        // A trailing semicolon (with or without whitespace) is not a second
        // statement, and must stay usable.
        assert!(allowed("SELECT 1;"));
        assert!(allowed("SELECT 1;  \n "));
        // ...nor is one followed only by a comment.
        assert!(allowed("SELECT 1; -- done"));
    }

    #[test]
    fn comments_cannot_hide_a_statement() {
        // A leading comment must not make the real keyword invisible.
        assert!(allowed("-- pick everything\nSELECT * FROM t"));
        assert!(allowed("/* block */ SELECT 1"));
        assert!(!allowed("-- harmless\nDROP TABLE t"));
        // ...and a comment must not smuggle a second statement past the check.
        assert!(!allowed("SELECT 1; /* sneaky */ DELETE FROM t"));
        // A `--` inside a string literal is data, not a comment: blanking it
        // would change what the query means.
        assert!(allowed("SELECT '-- not a comment' AS s"));
        assert!(!allowed("SELECT 'x'; DROP TABLE t"));
    }

    #[test]
    fn pragmas_are_allowed_only_by_name() {
        assert!(allowed("PRAGMA show_tables"));
        assert!(allowed("PRAGMA table_info('t')"));
        assert!(allowed("pragma version"));
        // Not on the list: it could change a setting or load an extension.
        assert!(!allowed("PRAGMA enable_external_access"));
        assert!(!allowed("PRAGMA disable_verification"));
        assert!(!allowed("CALL dbgen(sf=1)"));
    }

    #[test]
    fn nothing_and_nonsense_are_refused_with_a_reason() {
        assert!(!allowed(""));
        assert!(!allowed("   \n  "));
        assert!(!allowed("-- only a comment"));
        assert!(!allowed("(SELECT 1)"), "a leading paren is not a statement we vet");
    }

    #[test]
    fn the_refusal_points_at_the_auditable_path() {
        // A refusal should route the user to where writes belong, not dead-end.
        let err = check("DELETE FROM t").unwrap_err();
        assert!(err.0.contains("notebook cell"), "got {}", err.0);
        assert!(err.0.contains("DELETE"), "names the offending keyword: {}", err.0);
    }
}
