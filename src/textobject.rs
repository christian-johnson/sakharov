//! Text objects — the ranges `mi` (inside) and `mo` (outside) select.
//!
//! Every function here is pure: a rope, a cursor and an object in, an
//! inclusive char range out (`None` when the cursor is not in such an object).
//! `exec` turns that range into a selection; nothing in this module touches
//! `App`.

use ropey::Rope;

use crate::highlight::Language;

/// Whether an object's delimiters are part of the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Just the contents: `mi"` leaves the quotes behind.
    Inside,
    /// Contents plus delimiters: `mo"` takes the quotes too.  For objects with
    /// no delimiters (word, paragraph) this takes the trailing whitespace
    /// instead, so `mod` on a word deletes the gap as well.
    Outside,
}

/// A selectable region of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObject {
    /// Run of word characters (or of punctuation, when the cursor sits on it).
    Word,
    /// Run of non-whitespace.
    BigWord,
    /// A bracket pair with distinct open and close delimiters.
    Brackets(char, char),
    /// A quote-like delimiter that is its own partner.
    Quotes(char),
    /// Whichever bracket pair encloses the cursor most tightly.
    NearestPair,
    /// Tree-sitter: the enclosing function or method.
    Function,
    /// Tree-sitter: the enclosing class, struct, trait or impl block.
    Class,
    /// Tree-sitter: one parameter or argument of the enclosing list.
    Parameter,
    /// Block of lines between blank lines.
    Paragraph,
}

impl TextObject {
    /// True when this object is found by parsing rather than by scanning
    /// characters — i.e. it needs a language the grammar table knows.
    pub fn needs_syntax(self) -> bool {
        matches!(self, Self::Function | Self::Class | Self::Parameter)
    }

    /// Name used in messages when the object isn't there ("No function here").
    pub fn describe(self) -> &'static str {
        match self {
            Self::Word | Self::BigWord => "word",
            Self::Brackets(..) | Self::NearestPair => "bracket pair",
            Self::Quotes(_) => "quoted string",
            Self::Function => "function",
            Self::Class => "class or type",
            Self::Parameter => "parameter",
            Self::Paragraph => "paragraph",
        }
    }
}

/// Bracket pairs `NearestPair` and `mm` consider.  Angle brackets are left out
/// on purpose: `<` is a comparison far more often than a delimiter, so it is
/// only ever matched when asked for by name (`mi<`).
const PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}')];

/// Whether `object` exists at all in `language` — a buffer with no grammar has
/// no function object, and the which-key popup lists only what it can deliver.
pub fn available(language: Option<Language>, object: TextObject) -> bool {
    if !object.needs_syntax() {
        return true;
    }
    language.is_some_and(|l| !node_kinds(l, object).is_empty())
}

/// The inclusive char range `object` covers at `cursor`, or `None` when the
/// cursor is not inside one.
pub fn range(
    rope: &Rope,
    language: Option<Language>,
    cursor: usize,
    object: TextObject,
    scope: Scope,
) -> Option<(usize, usize)> {
    if rope.len_chars() == 0 {
        return None;
    }
    let cursor = cursor.min(rope.len_chars() - 1);
    match object {
        TextObject::Word => word(rope, cursor, false, scope),
        TextObject::BigWord => word(rope, cursor, true, scope),
        TextObject::Brackets(open, close) => brackets(rope, cursor, open, close).map(strip(scope)),
        TextObject::Quotes(q) => quotes(rope, cursor, q).map(strip(scope)),
        TextObject::NearestPair => PAIRS
            .iter()
            .filter_map(|&(o, c)| brackets(rope, cursor, o, c))
            .max_by_key(|&(start, _)| start)
            .map(strip(scope)),
        TextObject::Paragraph => paragraph(rope, cursor, scope),
        TextObject::Function | TextObject::Class | TextObject::Parameter => {
            syntax(rope, language?, cursor, object, scope)
        }
    }
    .filter(|&(start, end)| start <= end)
}

/// The position of the delimiter matching the one at (or enclosing) `cursor` —
/// what `mm` jumps to.
pub fn matching_bracket(rope: &Rope, cursor: usize) -> Option<usize> {
    if rope.len_chars() == 0 {
        return None;
    }
    let cursor = cursor.min(rope.len_chars() - 1);
    let here = rope.char(cursor);
    for &(open, close) in PAIRS {
        if here == open {
            return scan_forward(rope, cursor + 1, open, close);
        }
        if here == close {
            return scan_backward(rope, cursor, open, close);
        }
    }
    // Off a delimiter: the close of the pair the cursor sits inside.
    PAIRS
        .iter()
        .filter_map(|&(o, c)| brackets(rope, cursor, o, c))
        .max_by_key(|&(start, _)| start)
        .map(|(_, close)| close)
}

/// Turn a delimited range into what `scope` asks for: `Inside` drops the two
/// delimiters, `Outside` keeps them.
fn strip(scope: Scope) -> impl Fn((usize, usize)) -> (usize, usize) {
    move |(open, close)| match scope {
        Scope::Inside => (open + 1, close.saturating_sub(1)),
        Scope::Outside => (open, close),
    }
}

// ---------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------

/// Character classes a word object may not cross.  A word object is
/// line-local: `miw` at the end of a line must not swallow the next line's
/// first word.
#[derive(PartialEq, Eq)]
enum Class {
    Word,
    Punct,
    Space,
}

fn class_of(c: char, big: bool) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if big || crate::motion::word_char(c) {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Char range `[start, end)` of `cursor`'s line, excluding the line break.
fn line_bounds(rope: &Rope, cursor: usize) -> (usize, usize) {
    let line = rope.char_to_line(cursor);
    let start = rope.line_to_char(line);
    let mut end = start + rope.line(line).len_chars();
    while end > start && matches!(rope.char(end - 1), '\n' | '\r') {
        end -= 1;
    }
    (start, end)
}

fn word(rope: &Rope, cursor: usize, big: bool, scope: Scope) -> Option<(usize, usize)> {
    let (line_start, line_end) = line_bounds(rope, cursor);
    if line_start == line_end || cursor >= line_end {
        return None;
    }
    let class = class_of(rope.char(cursor), big);
    let mut start = cursor;
    while start > line_start && class_of(rope.char(start - 1), big) == class {
        start -= 1;
    }
    let mut end = cursor;
    while end + 1 < line_end && class_of(rope.char(end + 1), big) == class {
        end += 1;
    }
    if scope == Scope::Inside || class == Class::Space {
        return Some((start, end));
    }
    // Outside: the whitespace after the word, or — when there is none, as at
    // the end of an argument list — the whitespace before it.
    let mut outer_end = end;
    while outer_end + 1 < line_end && rope.char(outer_end + 1).is_whitespace() {
        outer_end += 1;
    }
    if outer_end > end {
        return Some((start, outer_end));
    }
    let mut outer_start = start;
    while outer_start > line_start && rope.char(outer_start - 1).is_whitespace() {
        outer_start -= 1;
    }
    Some((outer_start, end))
}

// ---------------------------------------------------------------------------
// Brackets and quotes
// ---------------------------------------------------------------------------

/// The positions of the bracket pair enclosing `cursor`, delimiters included.
/// A cursor resting on either delimiter means that pair, not the one around it.
fn brackets(rope: &Rope, cursor: usize, open: char, close: char) -> Option<(usize, usize)> {
    let here = rope.char(cursor);
    if here == open {
        return Some((cursor, scan_forward(rope, cursor + 1, open, close)?));
    }
    if here == close {
        return Some((scan_backward(rope, cursor, open, close)?, cursor));
    }
    let start = scan_backward(rope, cursor, open, close)?;
    let end = scan_forward(rope, cursor, open, close)?;
    Some((start, end))
}

/// Walk back from `from` (exclusive) for the first unmatched `open`.
fn scan_backward(rope: &Rope, from: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = from;
    while i > 0 {
        i -= 1;
        let c = rope.char(i);
        if c == close {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

/// Walk forward from `from` (inclusive) for the first unmatched `close`.
fn scan_forward(rope: &Rope, from: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for i in from..rope.len_chars() {
        let c = rope.char(i);
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

/// The quote pair enclosing `cursor`, delimiters included.
///
/// Quotes have no nesting to count, so they are paired left to right along the
/// line — first with second, third with fourth.  An odd one out (an
/// unterminated string) delimits nothing rather than reaching to the next line.
fn quotes(rope: &Rope, cursor: usize, quote: char) -> Option<(usize, usize)> {
    let (line_start, line_end) = line_bounds(rope, cursor);
    let mut marks = Vec::new();
    for i in line_start..line_end {
        if rope.char(i) != quote {
            continue;
        }
        // An escaped quote is a character, not a delimiter — and a backslash
        // can itself be escaped, so count the run.
        let mut backslashes = 0;
        let mut j = i;
        while j > line_start && rope.char(j - 1) == '\\' {
            backslashes += 1;
            j -= 1;
        }
        if backslashes % 2 == 0 {
            marks.push(i);
        }
    }
    marks
        .chunks_exact(2)
        .map(|pair| (pair[0], pair[1]))
        .find(|&(open, close)| cursor >= open && cursor <= close)
}

// ---------------------------------------------------------------------------
// Paragraph
// ---------------------------------------------------------------------------

fn paragraph(rope: &Rope, cursor: usize, scope: Scope) -> Option<(usize, usize)> {
    let blank = |line: usize| {
        rope.line(line)
            .chars()
            .all(|c| c.is_whitespace())
    };
    let last_line = rope.len_lines().saturating_sub(1);
    let line = rope.char_to_line(cursor);
    if blank(line) {
        return None;
    }
    let mut first = line;
    while first > 0 && !blank(first - 1) {
        first -= 1;
    }
    let mut last = line;
    while last < last_line && !blank(last + 1) {
        last += 1;
    }
    let start = rope.line_to_char(first);
    let (_, end) = line_bounds(rope, rope.line_to_char(last));
    let inside = (start, end.saturating_sub(1));
    if scope == Scope::Inside {
        return Some(inside);
    }
    // Outside: the blank lines that separate this paragraph from the next, or
    // — at the end of the file — the ones before it.
    let mut after = last;
    while after < last_line && blank(after + 1) {
        after += 1;
    }
    if after > last {
        let end = rope.line_to_char(after) + rope.line(after).len_chars();
        return Some((start, end.saturating_sub(1).min(rope.len_chars() - 1)));
    }
    let mut before = first;
    while before > 0 && blank(before - 1) {
        before -= 1;
    }
    Some((rope.line_to_char(before), inside.1))
}

// ---------------------------------------------------------------------------
// Syntax objects (tree-sitter)
// ---------------------------------------------------------------------------

fn syntax(
    rope: &Rope,
    language: Language,
    cursor: usize,
    object: TextObject,
    scope: Scope,
) -> Option<(usize, usize)> {
    let kinds = node_kinds(language, object);
    if kinds.is_empty() {
        return None;
    }
    let text = rope.to_string();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language.ts_language()).ok()?;
    let tree = parser.parse(text.as_bytes(), None)?;
    let byte = rope.char_to_byte(cursor);
    let mut node = tree.root_node().descendant_for_byte_range(byte, byte)?;
    loop {
        if kinds.contains(&node.kind()) {
            break;
        }
        node = node.parent()?;
    }
    // A parameter's range is already tight, and its `Outside` form deliberately
    // ends on the separator's trailing space — trimming would eat it.
    if object == TextObject::Parameter {
        let (start, end) = parameter(node, byte, scope)?;
        let end = rope.byte_to_char(end);
        return Some((rope.byte_to_char(start), end.saturating_sub(1)));
    }
    let (start, end) = match scope {
        Scope::Outside => (node.start_byte(), node.end_byte()),
        Scope::Inside => body_bytes(rope, node),
    };
    trim(rope, rope.byte_to_char(start), rope.byte_to_char(end))
}

/// The byte range of a node's body, with the braces of a braced block dropped —
/// `mif` means the statements, and `mof` is how you ask for the block itself.
fn body_bytes(rope: &Rope, node: tree_sitter::Node) -> (usize, usize) {
    let body = node.child_by_field_name("body").unwrap_or(node);
    let (start, end) = (body.start_byte(), body.end_byte());
    let first = rope.byte_to_char(start);
    let last = rope.byte_to_char(end).saturating_sub(1);
    if last > first && rope.char(first) == '{' && rope.char(last) == '}' {
        return (start + 1, end - 1);
    }
    (start, end)
}

/// One parameter of a list node: the child the cursor is in, plus its
/// separator when `Outside` — a trailing `, ` normally, the preceding one for
/// the last parameter, so what is left behind still parses.
fn parameter(
    list: tree_sitter::Node,
    byte: usize,
    scope: Scope,
) -> Option<(usize, usize)> {
    let mut cursor = list.walk();
    let params: Vec<_> = list.named_children(&mut cursor).collect();
    let idx = params
        .iter()
        .position(|p| p.start_byte() <= byte && byte < p.end_byte())?;
    let param = params[idx];
    if scope == Scope::Inside {
        return Some((param.start_byte(), param.end_byte()));
    }
    if let Some(next) = params.get(idx + 1) {
        return Some((param.start_byte(), next.start_byte()));
    }
    match params.get(idx.wrapping_sub(1)) {
        Some(prev) => Some((prev.end_byte(), param.end_byte())),
        None => Some((param.start_byte(), param.end_byte())),
    }
}

/// Drop leading and trailing whitespace from a char range, so that a body that
/// ends in a newline doesn't select the blank line after it.
fn trim(rope: &Rope, start: usize, end: usize) -> Option<(usize, usize)> {
    let mut start = start;
    let mut end = end.min(rope.len_chars());
    while start < end && rope.char(start).is_whitespace() {
        start += 1;
    }
    while end > start && rope.char(end - 1).is_whitespace() {
        end -= 1;
    }
    (end > start).then(|| (start, end - 1))
}

/// Which node kinds each syntax object means, per language.  A language with no
/// entry for an object simply has no such object (`mic` in JSON), which the
/// caller reports rather than guessing.
fn node_kinds(language: Language, object: TextObject) -> &'static [&'static str] {
    match (language, object) {
        (Language::Rust, TextObject::Function) => &["function_item", "closure_expression"],
        (Language::Rust, TextObject::Class) => {
            &["impl_item", "struct_item", "enum_item", "trait_item", "union_item"]
        }
        (Language::Rust, TextObject::Parameter) => {
            &["parameters", "arguments", "type_parameters", "type_arguments"]
        }

        (Language::Python, TextObject::Function) => &["function_definition", "lambda"],
        (Language::Python, TextObject::Class) => &["class_definition"],
        (Language::Python, TextObject::Parameter) => {
            &["parameters", "argument_list", "lambda_parameters"]
        }

        (Language::JavaScript, TextObject::Function) => &[
            "function_declaration",
            "function",
            "function_expression",
            "arrow_function",
            "method_definition",
            "generator_function_declaration",
        ],
        (Language::JavaScript, TextObject::Class) => &["class_declaration", "class"],
        (Language::JavaScript, TextObject::Parameter) => &["formal_parameters", "arguments"],

        (Language::Go, TextObject::Function) => {
            &["function_declaration", "method_declaration", "func_literal"]
        }
        (Language::Go, TextObject::Class) => {
            &["type_declaration", "struct_type", "interface_type"]
        }
        (Language::Go, TextObject::Parameter) => &["parameter_list", "argument_list"],

        (Language::C, TextObject::Function) => &["function_definition"],
        (Language::C, TextObject::Class) => {
            &["struct_specifier", "union_specifier", "enum_specifier"]
        }
        (Language::C, TextObject::Parameter) => &["parameter_list", "argument_list"],

        (Language::Bash, TextObject::Function) => &["function_definition"],

        (Language::Css, TextObject::Class) => &["rule_set"],

        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(start, end)` of the first occurrence of `needle`, inclusive, as the
    /// ranges in these tests are written.
    fn span(src: &str, needle: &str) -> (usize, usize) {
        let start = src.find(needle).expect("needle is in the source");
        assert_eq!(
            src[..start].chars().count(),
            start,
            "test sources are ASCII, so byte offsets are char offsets"
        );
        (start, start + needle.chars().count() - 1)
    }

    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle is in the source")
    }

    fn get(src: &str, cursor: usize, object: TextObject, scope: Scope) -> Option<(usize, usize)> {
        range(&Rope::from_str(src), None, cursor, object, scope)
    }

    fn get_in(
        src: &str,
        lang: Language,
        cursor: usize,
        object: TextObject,
        scope: Scope,
    ) -> Option<(usize, usize)> {
        range(&Rope::from_str(src), Some(lang), cursor, object, scope)
    }

    // --- words ---------------------------------------------------------

    #[test]
    fn word_inside_is_the_run_under_the_cursor() {
        let src = "let total_count = 3;";
        let cursor = at(src, "tal_count");
        assert_eq!(
            get(src, cursor, TextObject::Word, Scope::Inside),
            Some(span(src, "total_count"))
        );
    }

    #[test]
    fn word_outside_takes_the_gap_after_it() {
        let src = "let total = 3;";
        let cursor = at(src, "total");
        assert_eq!(
            get(src, cursor, TextObject::Word, Scope::Outside),
            Some(span(src, "total "))
        );
    }

    /// A word with nothing after it takes the gap *before* it instead, so that
    /// `mod` on the last word of a list doesn't leave a dangling space.
    #[test]
    fn the_last_word_on_a_line_takes_the_gap_before_it() {
        let src = "call(a, b)";
        let cursor = at(src, "b)");
        assert_eq!(
            get(src, cursor, TextObject::Word, Scope::Outside),
            Some(span(src, " b"))
        );
    }

    /// On punctuation the run of punctuation is the object — the same rule the
    /// `w` motion uses, so `miw` never silently jumps to a neighbouring word.
    #[test]
    fn word_inside_on_punctuation_selects_the_punctuation_run() {
        let src = "a ==> b";
        let cursor = at(src, "==>");
        assert_eq!(
            get(src, cursor, TextObject::Word, Scope::Inside),
            Some(span(src, "==>"))
        );
    }

    #[test]
    fn big_word_spans_the_punctuation_a_word_stops_at() {
        let src = "run  path/to/file.rs  end";
        let cursor = at(src, "to/file");
        assert_eq!(
            get(src, cursor, TextObject::BigWord, Scope::Inside),
            Some(span(src, "path/to/file.rs"))
        );
    }

    #[test]
    fn a_word_object_does_not_cross_a_line_break() {
        let src = "one\ntwo\n";
        let cursor = at(src, "two");
        assert_eq!(
            get(src, cursor, TextObject::Word, Scope::Outside),
            Some(span(src, "two"))
        );
    }

    // --- brackets and quotes ------------------------------------------

    #[test]
    fn brackets_inside_stops_at_the_delimiters() {
        let src = "f(a, g(b), c)";
        let cursor = at(src, ", c");
        assert_eq!(
            get(src, cursor, TextObject::Brackets('(', ')'), Scope::Inside),
            Some(span(src, "a, g(b), c"))
        );
        assert_eq!(
            get(src, cursor, TextObject::Brackets('(', ')'), Scope::Outside),
            Some(span(src, "(a, g(b), c)"))
        );
    }

    #[test]
    fn nested_brackets_resolve_to_the_innermost_enclosing_pair() {
        let src = "f(a, g(b), c)";
        let cursor = at(src, "b)");
        assert_eq!(
            get(src, cursor, TextObject::Brackets('(', ')'), Scope::Outside),
            Some(span(src, "(b)"))
        );
    }

    /// Sitting *on* an opening delimiter means that pair, not the one around
    /// it — otherwise `mo(` on `f(` would select the enclosing call.
    #[test]
    fn a_cursor_on_a_delimiter_uses_that_pair() {
        let src = "f(a, g(b), c)";
        let outer = span(src, "(a, g(b), c)");
        // On the outer open and on the outer close: the outer pair both times.
        for cursor in [outer.0, outer.1] {
            assert_eq!(
                get(src, cursor, TextObject::Brackets('(', ')'), Scope::Outside),
                Some(outer),
                "cursor at {cursor}"
            );
        }
        // On the inner open: the inner pair, not the one around it.
        assert_eq!(
            get(src, at(src, "(b"), TextObject::Brackets('(', ')'), Scope::Outside),
            Some(span(src, "(b)"))
        );
    }

    #[test]
    fn brackets_span_lines() {
        let src = "fn f() {\n    body;\n}\n";
        let cursor = at(src, "body");
        assert_eq!(
            get(src, cursor, TextObject::Brackets('{', '}'), Scope::Inside),
            Some(span(src, "\n    body;\n"))
        );
    }

    #[test]
    fn an_empty_pair_has_no_inside() {
        let src = "f()";
        let cursor = at(src, "(");
        assert_eq!(get(src, cursor, TextObject::Brackets('(', ')'), Scope::Inside), None);
        assert_eq!(
            get(src, cursor, TextObject::Brackets('(', ')'), Scope::Outside),
            Some(span(src, "()"))
        );
    }

    #[test]
    fn an_unclosed_pair_is_not_an_object() {
        let src = "f(a, b";
        let cursor = at(src, "a,");
        assert_eq!(get(src, cursor, TextObject::Brackets('(', ')'), Scope::Outside), None);
    }

    #[test]
    fn quotes_pair_up_along_the_line() {
        let src = r#"m = {"key": "value", "k2": "v2"}"#;
        let cursor = at(src, "value");
        assert_eq!(
            get(src, cursor, TextObject::Quotes('"'), Scope::Inside),
            Some(span(src, "value"))
        );
        assert_eq!(
            get(src, cursor, TextObject::Quotes('"'), Scope::Outside),
            Some(span(src, "\"value\""))
        );
    }

    /// An escaped quote is not a delimiter — pairing on it shifts every
    /// following pair by one and selects nonsense.
    #[test]
    fn an_escaped_quote_is_not_a_delimiter() {
        let src = r#"s = "a \" b" + "c""#;
        let cursor = at(src, "a \\");
        assert_eq!(
            get(src, cursor, TextObject::Quotes('"'), Scope::Inside),
            Some(span(src, r#"a \" b"#))
        );
    }

    /// A quoted string is a line-local object: an unterminated quote must not
    /// pair with one on the next line.
    #[test]
    fn quotes_do_not_pair_across_lines() {
        let src = "a = \"one\nb = \"two\n";
        let cursor = at(src, "b = ");
        assert_eq!(get(src, cursor, TextObject::Quotes('"'), Scope::Inside), None);
    }

    #[test]
    fn nearest_pair_picks_the_tightest_enclosing_bracket_of_any_kind() {
        let src = "f(v[i], w)";
        let cursor = at(src, "i]");
        assert_eq!(
            get(src, cursor, TextObject::NearestPair, Scope::Outside),
            Some(span(src, "[i]"))
        );
        let cursor = at(src, "w)");
        assert_eq!(
            get(src, cursor, TextObject::NearestPair, Scope::Outside),
            Some(span(src, "(v[i], w)"))
        );
    }

    // --- matching bracket (mm) ----------------------------------------

    #[test]
    fn mm_jumps_between_the_two_halves_of_a_pair() {
        let src = "f(a, g(b), c)";
        let open = at(src, "(a");
        let close = span(src, "f(a, g(b), c)").1;
        let rope = Rope::from_str(src);
        assert_eq!(matching_bracket(&rope, open), Some(close));
        assert_eq!(matching_bracket(&rope, close), Some(open));
    }

    /// Off a delimiter, `mm` goes to the close of the pair the cursor is in —
    /// the useful answer, rather than nothing.
    #[test]
    fn mm_from_inside_a_pair_goes_to_its_close() {
        let src = "f(a, b)";
        let rope = Rope::from_str(src);
        assert_eq!(matching_bracket(&rope, at(src, "a,")), Some(span(src, "f(a, b)").1));
        assert_eq!(matching_bracket(&rope, 0), None);
    }

    // --- paragraph ----------------------------------------------------

    #[test]
    fn a_paragraph_is_the_block_of_lines_between_blank_ones() {
        let src = "one\ntwo\n\nthree\n";
        let cursor = at(src, "two");
        assert_eq!(
            get(src, cursor, TextObject::Paragraph, Scope::Inside),
            Some(span(src, "one\ntwo"))
        );
        // Outside takes the blank separator with it.
        assert_eq!(
            get(src, cursor, TextObject::Paragraph, Scope::Outside),
            Some(span(src, "one\ntwo\n\n"))
        );
    }

    // --- syntax objects -----------------------------------------------

    const PY: &str = "class Shape:\n    def area(self, scale, unit=1):\n        return 0\n\n\ndef free():\n    pass\n";

    #[test]
    fn python_function_inside_is_the_body_and_outside_is_the_whole_def() {
        let cursor = at(PY, "return 0");
        assert_eq!(
            get_in(PY, Language::Python, cursor, TextObject::Function, Scope::Inside),
            Some(span(PY, "return 0"))
        );
        assert_eq!(
            get_in(PY, Language::Python, cursor, TextObject::Function, Scope::Outside),
            Some(span(PY, "def area(self, scale, unit=1):\n        return 0"))
        );
    }

    #[test]
    fn the_innermost_function_wins_over_the_class_around_it() {
        let cursor = at(PY, "return 0");
        assert_eq!(
            get_in(PY, Language::Python, cursor, TextObject::Class, Scope::Outside),
            Some(span(PY, "class Shape:\n    def area(self, scale, unit=1):\n        return 0"))
        );
    }

    #[test]
    fn a_parameter_object_is_one_parameter() {
        let cursor = at(PY, "scale");
        assert_eq!(
            get_in(PY, Language::Python, cursor, TextObject::Parameter, Scope::Inside),
            Some(span(PY, "scale"))
        );
        // Outside swallows the separator, so `mod` leaves a valid list behind.
        assert_eq!(
            get_in(PY, Language::Python, cursor, TextObject::Parameter, Scope::Outside),
            Some(span(PY, "scale, "))
        );
    }

    /// The last parameter has no comma after it, so outside takes the one
    /// before it — deleting `unit=1` must not leave `(self, scale,)`.
    #[test]
    fn the_last_parameter_takes_the_comma_before_it() {
        let cursor = at(PY, "unit=1");
        assert_eq!(
            get_in(PY, Language::Python, cursor, TextObject::Parameter, Scope::Outside),
            Some(span(PY, ", unit=1"))
        );
    }

    const RS: &str = "impl Shape {\n    fn area(&self, scale: f64) -> f64 {\n        scale * 2.0\n    }\n}\n";

    /// A Rust body is a braced block; `mif` means the statements, not the
    /// braces (that is `mof`'s job, via the block itself).
    #[test]
    fn a_rust_function_body_excludes_its_braces() {
        let cursor = at(RS, "scale * 2.0");
        assert_eq!(
            get_in(RS, Language::Rust, cursor, TextObject::Function, Scope::Inside),
            Some(span(RS, "scale * 2.0"))
        );
        assert_eq!(
            get_in(RS, Language::Rust, cursor, TextObject::Function, Scope::Outside),
            Some(span(RS, "fn area(&self, scale: f64) -> f64 {\n        scale * 2.0\n    }"))
        );
    }

    #[test]
    fn an_impl_block_is_a_class_object() {
        let cursor = at(RS, "scale * 2.0");
        let (_, end) = span(RS, "impl Shape {\n    fn area(&self, scale: f64) -> f64 {\n        scale * 2.0\n    }\n}");
        assert_eq!(
            get_in(RS, Language::Rust, cursor, TextObject::Class, Scope::Outside),
            Some((0, end))
        );
    }

    #[test]
    fn syntax_objects_need_a_language() {
        let cursor = at(PY, "return 0");
        for object in [TextObject::Function, TextObject::Class, TextObject::Parameter] {
            assert_eq!(get(PY, cursor, object, Scope::Inside), None, "{object:?}");
            assert!(object.needs_syntax());
        }
        // Character-scanned objects work without one.
        assert!(!TextObject::Word.needs_syntax());
        assert!(get(PY, cursor, TextObject::Word, Scope::Inside).is_some());
    }

    /// Outside a function there is no function object — the command reports
    /// that rather than selecting the nearest one.
    #[test]
    fn a_cursor_outside_every_function_has_no_function_object() {
        let src = "import os\n\n\ndef f():\n    pass\n";
        let cursor = at(src, "import");
        assert_eq!(
            get_in(src, Language::Python, cursor, TextObject::Function, Scope::Outside),
            None
        );
    }

    #[test]
    fn an_empty_rope_has_no_objects() {
        let rope = Rope::from_str("");
        for object in [
            TextObject::Word,
            TextObject::BigWord,
            TextObject::Brackets('(', ')'),
            TextObject::Quotes('"'),
            TextObject::NearestPair,
            TextObject::Paragraph,
        ] {
            assert_eq!(range(&rope, None, 0, object, Scope::Inside), None, "{object:?}");
        }
        assert_eq!(matching_bracket(&rope, 0), None);
    }
}
