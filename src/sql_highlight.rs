//! Custom SQL highlighting for `.sql` files and the `*sql*` query buffer.
//!
//! Hand-written rather than tree-sitter, for the same reason `crate::markdown`
//! is: the available SQL grammar (`tree-sitter-sequel`) is built against the
//! `tree-sitter-language` ABI of tree-sitter 0.23+, while this editor is pinned
//! to 0.22 for the eleven grammars it already carries — and SQL's lexical
//! surface (comments, string literals, quoted identifiers, numbers, a keyword
//! list) is small enough that a lexer is less risk than an ABI mismatch that
//! compiles and then crashes.  It emits the same [`Span`] list every other
//! highlighter produces, so nothing downstream knows the difference.
//!
//! Deliberately lexical only: no parsing, so a half-typed query still colours
//! sensibly, which is the state a query buffer spends most of its life in.

use std::path::Path;

use ropey::Rope;

use crate::highlight::Span;

/// Highlight indices into `highlight::HIGHLIGHT_NAMES`.
const COMMENT: usize = 1;
const CONSTANT_BUILTIN: usize = 3;
const FUNCTION: usize = 5;
const KEYWORD: usize = 8;
const NUMBER: usize = 11;
const OPERATOR: usize = 12;
const PUNCTUATION: usize = 14;
const STRING: usize = 17;
const TYPE: usize = 20;
const VARIABLE: usize = 23;

/// Words that colour as keywords.  Not an exhaustive SQL dialect list — the
/// point is to make the shape of a query readable, and a word this misses is
/// simply drawn in the foreground colour.
const KEYWORDS: &[&str] = &[
    "ALL", "ALTER", "AND", "ANTI", "ANY", "AS", "ASC", "ASOF", "ATTACH", "BETWEEN", "BY", "CALL",
    "CASE", "CAST", "COLUMNS", "COPY", "CREATE", "CROSS", "CUBE", "CURRENT", "DATABASE", "DELETE",
    "DESC", "DESCRIBE", "DETACH", "DISTINCT", "DISTRIBUTE", "DO", "DROP", "ELSE", "END", "EXCEPT",
    "EXCLUDE", "EXISTS", "EXPLAIN", "EXPORT", "FETCH", "FILTER", "FIRST", "FOLLOWING", "FOR",
    "FROM", "FULL", "GLOB", "GROUP", "GROUPING", "HAVING", "ILIKE", "IN", "INNER", "INSERT",
    "INSTALL", "INTERSECT", "INTO", "IS", "JOIN", "LAST", "LATERAL", "LEFT", "LIKE", "LIMIT",
    "LOAD", "NATURAL", "NOT", "NULLS", "OFFSET", "ON", "OR", "ORDER", "OUTER", "OVER",
    "PARTITION", "PIVOT", "POSITIONAL", "PRAGMA", "PRECEDING", "QUALIFY", "RANGE", "RECURSIVE",
    "REPLACE", "RETURNING", "RIGHT", "ROLLUP", "ROW", "ROWS", "SAMPLE", "SELECT", "SEMI", "SET",
    "SHOW", "SIMILAR", "SUMMARIZE", "TABLE", "THEN", "TO", "UNBOUNDED", "UNION", "UNPIVOT",
    "UPDATE", "USING", "VALUES", "VIEW", "WHEN", "WHERE", "WINDOW", "WITH", "WITHIN",
];

/// Words that colour as types.
const TYPES: &[&str] = &[
    "BIGINT", "BIT", "BLOB", "BOOL", "BOOLEAN", "CHAR", "DATE", "DATETIME", "DECIMAL", "DOUBLE",
    "FLOAT", "HUGEINT", "INT", "INT2", "INT4", "INT8", "INTEGER", "INTERVAL", "JSON", "LIST",
    "MAP", "NUMERIC", "REAL", "SMALLINT", "STRUCT", "TEXT", "TIME", "TIMESTAMP", "TINYINT",
    "UBIGINT", "UNION", "USMALLINT", "UUID", "VARCHAR",
];

/// Words that colour as built-in constants.
const CONSTANTS: &[&str] = &["FALSE", "NULL", "TRUE", "UNKNOWN", "INFINITY", "NAN"];

/// True if `path` names something to highlight as SQL: a `.sql` file, or the
/// editor's own query buffer, whose "path" is the virtual name `*sql*`.
pub fn is_sql(path: Option<&Path>) -> bool {
    let Some(path) = path else { return false };
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
    {
        return true;
    }
    path.to_str() == Some(crate::app::SQL_BUFFER)
}

/// Highlight spans for `rope`, as `(char_start, char_end, highlight_index)`.
pub fn highlight(rope: &Rope) -> Vec<Span> {
    let text = rope.to_string();
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        match c {
            // `-- line comment`
            '-' if chars.get(i + 1) == Some(&'-') => {
                let start = i;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                spans.push((start, i, COMMENT));
            }
            // `/* block comment */`, which SQL does not nest.
            '/' if chars.get(i + 1) == Some(&'*') => {
                let start = i;
                i += 2;
                while i < chars.len() && !(chars[i - 1] == '*' && chars[i] == '/') {
                    i += 1;
                }
                i = (i + 1).min(chars.len());
                spans.push((start, i, COMMENT));
            }
            // String literal.  An unterminated one runs to the end of the
            // buffer — which is exactly right while it is being typed.
            '\'' => {
                let start = i;
                i = scan_quoted(&chars, i, '\'');
                spans.push((start, i, STRING));
            }
            // Quoted identifier: a column or table, not a string.
            '"' => {
                let start = i;
                i = scan_quoted(&chars, i, '"');
                spans.push((start, i, VARIABLE));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                    i += 1;
                }
                spans.push((start, i, NUMBER));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect::<String>().to_ascii_uppercase();
                let kind = if CONSTANTS.contains(&word.as_str()) {
                    Some(CONSTANT_BUILTIN)
                } else if KEYWORDS.contains(&word.as_str()) {
                    Some(KEYWORD)
                } else if TYPES.contains(&word.as_str()) {
                    Some(TYPE)
                } else if next_nonspace(&chars, i) == Some('(') {
                    // Not a keyword and called: a function, whichever dialect
                    // it comes from.  This is what keeps `read_parquet` and
                    // `strftime` coloured without a builtin list to maintain.
                    Some(FUNCTION)
                } else {
                    None
                };
                if let Some(kind) = kind {
                    spans.push((start, i, kind));
                }
            }
            '+' | '-' | '*' | '/' | '%' | '=' | '<' | '>' | '!' | '|' | '~' => {
                let start = i;
                i += 1;
                spans.push((start, i, OPERATOR));
            }
            '(' | ')' | ',' | ';' | '.' | '[' | ']' => {
                let start = i;
                i += 1;
                spans.push((start, i, PUNCTUATION));
            }
            _ => i += 1,
        }
    }
    spans
}

/// Index just past the quoted run starting at `open`, where a doubled quote
/// (`''`, `""`) is an escape rather than the end.
fn scan_quoted(chars: &[char], open: usize, quote: char) -> usize {
    let mut i = open + 1;
    while i < chars.len() {
        if chars[i] == quote {
            if chars.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    chars.len()
}

/// The next non-whitespace character at or after `from`.
fn next_nonspace(chars: &[char], from: usize) -> Option<char> {
    chars[from..].iter().copied().find(|c| !c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<(String, usize)> {
        let chars: Vec<char> = sql.chars().collect();
        highlight(&Rope::from_str(sql))
            .into_iter()
            .map(|(a, b, k)| (chars[a..b].iter().collect::<String>(), k))
            .collect()
    }

    fn kind_of(sql: &str, text: &str) -> Option<usize> {
        kinds(sql).into_iter().find(|(t, _)| t == text).map(|(_, k)| k)
    }

    #[test]
    fn the_shape_of_a_query_is_coloured() {
        let sql = "SELECT count(*) AS n, 'a''b' FROM t WHERE x > 1.5 AND y IS NULL -- note\n";
        assert_eq!(kind_of(sql, "SELECT"), Some(KEYWORD));
        assert_eq!(kind_of(sql, "FROM"), Some(KEYWORD));
        // Case-insensitive, like SQL itself.
        assert_eq!(kind_of("select 1", "select"), Some(KEYWORD));
        assert_eq!(kind_of(sql, "count"), Some(FUNCTION));
        assert_eq!(kind_of(sql, "NULL"), Some(CONSTANT_BUILTIN));
        assert_eq!(kind_of(sql, "1.5"), Some(NUMBER));
        // A doubled quote is an escape, so the literal is one span.
        assert_eq!(kind_of(sql, "'a''b'"), Some(STRING));
        assert_eq!(kind_of(sql, "-- note"), Some(COMMENT));
        // A plain identifier stays in the foreground colour.
        assert_eq!(kind_of(sql, "t"), None);
    }

    #[test]
    fn comments_and_literals_win_over_the_words_inside_them() {
        // A keyword inside a comment or a string is not a keyword.
        assert_eq!(kind_of("-- SELECT everything\n", "SELECT"), None);
        assert_eq!(kind_of("/* FROM */ SELECT 1", "FROM"), None);
        assert_eq!(kind_of("SELECT 'FROM' AS s", "FROM"), None);
        assert_eq!(kind_of("SELECT '-- not a comment'", "-- not a comment"), None);
        // ...and a quoted identifier is neither a string nor a keyword.
        assert_eq!(kind_of("SELECT \"from\" FROM t", "\"from\""), Some(VARIABLE));
    }

    #[test]
    fn an_unterminated_construct_still_highlights_to_the_end() {
        // The state a query buffer is in for most of its life.
        assert_eq!(kind_of("SELECT 'half typed", "'half typed"), Some(STRING));
        assert_eq!(kind_of("SELECT /* open", "/* open"), Some(COMMENT));
    }

    #[test]
    fn the_query_buffer_and_sql_files_are_recognised() {
        assert!(is_sql(Some(Path::new("q.sql"))));
        assert!(is_sql(Some(Path::new("Q.SQL"))));
        assert!(is_sql(Some(Path::new(crate::app::SQL_BUFFER))));
        assert!(!is_sql(Some(Path::new("notes.md"))));
        assert!(!is_sql(None));
    }

    /// Spans are consumed as ranges into the rope, so they must be ordered and
    /// non-overlapping whatever the input.
    #[test]
    fn spans_are_ordered_and_disjoint() {
        let sql = "WITH x AS (SELECT 1 /* c */) SELECT 'a', \"b\", 2.5, f(x) -- end";
        let mut prev_end = 0;
        for (start, end, _) in highlight(&Rope::from_str(sql)) {
            assert!(start >= prev_end, "overlap at {start}..{end}");
            assert!(end > start);
            prev_end = end;
        }
    }
}
