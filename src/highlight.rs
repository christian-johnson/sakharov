use std::sync::{Arc, Mutex, OnceLock};

use ropey::Rope;
use tree_sitter::{InputEdit, Parser, Point, Query, QueryCursor, Tree};

/// Ordered list of highlight names that tree-sitter will resolve.
/// The index of each name matches what `style_for_highlight` expects.
pub const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "function",
    "function.builtin",
    "function.method",
    "keyword",
    "label",
    "namespace",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
    // --- Markdown / markup (indices 25.. — see the MD_* constants below) ---
    "markup.heading.1",
    "markup.heading.2",
    "markup.heading.3",
    "markup.heading.4",
    "markup.heading.5",
    "markup.heading.6",
    "markup.bold",
    "markup.italic",
    "markup.raw",
    "markup.link",
    "markup.quote",
    "markup.list",
    // --- git output (indices 37.. — see the GIT_* constants below) ---
    "git.added",
    "git.removed",
    "git.hunk",
    "git.meta",
    "git.hash",
    "git.warning",
];

// Highlight indices for the markdown markup names appended to `HIGHLIGHT_NAMES`.
// These are emitted directly by the custom markdown highlighter (`crate::markdown`),
// which does not use tree-sitter. Keep them in sync with the array order above and
// with the match arms in `theme::style_for_highlight`.
pub const MD_HEADING_1: usize = 25;
pub const MD_HEADING_2: usize = 26;
pub const MD_HEADING_3: usize = 27;
pub const MD_HEADING_4: usize = 28;
pub const MD_HEADING_5: usize = 29;
pub const MD_HEADING_6: usize = 30;
pub const MD_BOLD: usize = 31;
pub const MD_ITALIC: usize = 32;
pub const MD_RAW: usize = 33;
pub const MD_LINK: usize = 34;
pub const MD_QUOTE: usize = 35;
pub const MD_LIST: usize = 36;

// Highlight indices for git output — a diff, `git status`, or a hook's own
// printing (`crate::git_highlight`).  Six slots rather than a colour per thing
// git can say: what a reader is scanning for is "added / removed / where /
// what went wrong", and everything else is context.
pub const GIT_ADDED: usize = 37;
pub const GIT_REMOVED: usize = 38;
pub const GIT_HUNK: usize = 39;
pub const GIT_META: usize = 40;
pub const GIT_HASH: usize = 41;
pub const GIT_WARNING: usize = 42;

/// A highlighted span: (char_start, char_end, highlight_index).
pub type Span = (usize, usize, usize);

/// Detected language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    Toml,
    Json,
    Yaml,
    Bash,
    Go,
    C,
    Html,
    Css,
}

impl Language {
    /// Detect language from a file extension.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "js" | "jsx" => Some(Self::JavaScript),
            "toml" => Some(Self::Toml),
            "json" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            "sh" | "bash" | "zsh" => Some(Self::Bash),
            "go" => Some(Self::Go),
            "c" | "h" => Some(Self::C),
            "html" | "htm" => Some(Self::Html),
            "css" => Some(Self::Css),
            _ => None,
        }
    }

    /// Detect language from a full path: extension first, falling back to
    /// well-known shell dotfiles (`.zshrc`, `.bashrc`, ...) that have no
    /// extension `Path::extension()` can see at all.
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        if let Some(lang) = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
        {
            return Some(lang);
        }
        let name = path.file_name()?.to_str()?;
        crate::lang::SHELL_DOTFILES
            .contains(&name)
            .then_some(Self::Bash)
    }

    /// The raw tree-sitter grammar for this language (shared by the
    /// highlighter and the fold-range walker).
    pub fn ts_language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::language(),
            Self::Python => tree_sitter_python::language(),
            Self::JavaScript => tree_sitter_javascript::language(),
            Self::Toml => tree_sitter_toml_ng::language(),
            Self::Json => tree_sitter_json::language(),
            Self::Yaml => tree_sitter_yaml::language(),
            Self::Bash => tree_sitter_bash::language(),
            Self::Go => tree_sitter_go::language(),
            Self::C => tree_sitter_c::language(),
            Self::Html => tree_sitter_html::language(),
            Self::Css => tree_sitter_css::language(),
        }
    }
}

/// A language's compiled highlight query and the `HIGHLIGHT_NAMES` index each
/// of its captures maps to.  Compiling a query is expensive, so each is built
/// once per process and shared by every highlighter.
struct Grammar {
    query: Query,
    capture_map: Vec<Option<usize>>,
}

fn grammar(lang: Language) -> Option<Arc<Grammar>> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<Language, Option<Arc<Grammar>>>>> =
        OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().ok()?;
    cache
        .entry(lang)
        .or_insert_with(|| {
            let query = Query::new(&lang.ts_language(), highlights_query(lang)).ok()?;
            let capture_map = query.capture_names().iter().map(|c| highlight_index(c)).collect();
            Some(Arc::new(Grammar { query, capture_map }))
        })
        .clone()
}

/// The `HIGHLIGHT_NAMES` entry a capture name maps to: the one with the most
/// dot-separated parts, all of which appear in the capture's name (so
/// `function.method.call` → `function.method`).  Same rule as
/// `tree_sitter_highlight`, which the highlighter used to be built on.
fn highlight_index(capture: &str) -> Option<usize> {
    let parts: Vec<&str> = capture.split('.').collect();
    HIGHLIGHT_NAMES
        .iter()
        .enumerate()
        .filter(|(_, name)| name.split('.').all(|p| parts.contains(&p)))
        .max_by_key(|(i, name)| (name.split('.').count(), std::cmp::Reverse(*i)))
        .map(|(i, _)| i)
}

/// Syntax highlighter.
///
/// Tree-sitter languages keep their parse tree between edits: [`edit`](Self::edit)
/// records each change, and the next highlight re-parses only what changed,
/// then runs the highlight query over just the requested lines.  Folds come
/// from the same tree.  Markdown, SQL and git output use hand-written
/// highlighters instead.
pub struct Highlighter {
    pub language: Option<Language>,
    /// True when the open file is Markdown (`.md`/`.qmd`). Markdown is highlighted
    /// and folded by the custom, non-tree-sitter `crate::markdown` module.
    pub markdown: bool,
    /// True for `.sql` files and the `*sql*` query buffer, highlighted by the
    /// custom `crate::sql_highlight` lexer (same reason as markdown: no usable
    /// grammar at this tree-sitter ABI).
    pub sql: bool,
    /// True for the buffers git's own output is read in (`*git output*`,
    /// `*commit …*`, a `.diff`/`.patch` file), coloured by
    /// `crate::git_highlight`.
    pub git: bool,
    grammar: Option<Arc<Grammar>>,
    parser: Parser,
    tree: Option<ParsedTree>,
}

/// A parse tree plus what it takes to trust it for an incremental re-parse.
struct ParsedTree {
    tree: Tree,
    /// `Buffer::id` of the text it was parsed from.
    buffer_id: u64,
    /// Byte length the text will have once every recorded edit is applied.  A
    /// mismatch means some change bypassed [`Highlighter::edit`], and the tree
    /// is rebuilt from scratch rather than trusted.
    expected_len: usize,
}

impl Highlighter {
    /// Create a highlighter, detecting language from the optional file path.
    pub fn new(path: Option<&std::path::Path>) -> Self {
        let language = path.and_then(Language::from_path);
        let grammar = language.and_then(grammar);
        let mut parser = Parser::new();
        if let Some(lang) = language.filter(|_| grammar.is_some()) {
            // Cannot fail: the grammar's query just compiled against it.
            let _ = parser.set_language(&lang.ts_language());
        }
        Self {
            language,
            markdown: crate::markdown::is_markdown(path),
            sql: crate::sql_highlight::is_sql(path),
            git: crate::git_highlight::is_git_output(path),
            grammar,
            parser,
            tree: None,
        }
    }

    /// Record an edit so the next parse can reuse the unchanged parts of the
    /// tree.  Call after the rope is changed (see `exec::edit`).
    pub fn edit(&mut self, edit: &InputEdit) {
        if let Some(parsed) = self.tree.as_mut() {
            parsed.tree.edit(edit);
            parsed.expected_len = (parsed.expected_len + edit.new_end_byte)
                .saturating_sub(edit.old_end_byte);
        }
    }

    /// True when highlights always cover the whole document (the hand-written
    /// highlighters), so scrolling never needs them recomputed.
    pub fn whole_document(&self) -> bool {
        self.markdown || self.sql || self.git
    }

    /// Forget the tree: the text was replaced wholesale (undo, reload, …).
    pub fn invalidate(&mut self) {
        self.tree = None;
    }

    /// Bring the tree up to date with `rope` and return it.
    fn tree(&mut self, rope: &Rope, buffer_id: u64) -> Option<&Tree> {
        self.grammar.as_ref()?;
        let old = self
            .tree
            .take()
            .filter(|t| t.buffer_id == buffer_id && t.expected_len == rope.len_bytes());
        let tree = self.parser.parse_with(
            &mut |byte, _| {
                if byte >= rope.len_bytes() {
                    return &[][..];
                }
                let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
                &chunk.as_bytes()[byte - chunk_start..]
            },
            old.as_ref().map(|t| &t.tree),
        )?;
        self.tree = Some(ParsedTree { tree, buffer_id, expected_len: rope.len_bytes() });
        self.tree.as_ref().map(|t| &t.tree)
    }

    /// Compute the foldable line ranges for the current buffer contents.
    /// Routes to the markdown section/fence folder or the tree-sitter folder.
    pub fn fold_ranges(&mut self, rope: &Rope, buffer_id: u64) -> Vec<crate::fold::FoldRange> {
        if self.markdown {
            return crate::markdown::fold_ranges(rope);
        }
        let Some(lang) = self.language else { return Vec::new() };
        match self.tree(rope, buffer_id) {
            Some(tree) => crate::fold::fold_ranges_in(tree, rope, lang),
            None => Vec::new(),
        }
    }

    /// Highlight spans for lines `lines` of the buffer `buffer_id`, re-parsing
    /// incrementally from the recorded edits.  Spans are [`flatten`]ed.
    pub fn highlight_lines(
        &mut self,
        rope: &Rope,
        buffer_id: u64,
        lines: std::ops::Range<usize>,
    ) -> Vec<Span> {
        if self.markdown {
            return flatten(crate::markdown::highlight(rope));
        }
        if self.sql {
            return flatten(crate::sql_highlight::highlight(rope));
        }
        if self.git {
            return flatten(crate::git_highlight::highlight(rope));
        }
        let Some(grammar) = self.grammar.clone() else { return Vec::new() };
        let Some(tree) = self.tree(rope, buffer_id) else { return Vec::new() };
        let last = rope.len_lines();
        let bytes = rope.line_to_byte(lines.start.min(last))..rope.line_to_byte(lines.end.min(last));
        flatten(query_spans(&grammar, tree, rope, bytes))
    }

    /// Highlight the whole of `rope` from scratch — for text that isn't the
    /// tracked buffer (notebook cells, tests).
    pub fn highlight(&mut self, rope: &Rope) -> Vec<Span> {
        self.invalidate();
        let spans = self.highlight_lines(rope, u64::MAX, 0..rope.len_lines());
        self.invalidate();
        spans
    }
}

/// Run the highlight query over `bytes` of `tree`.
///
/// Where several patterns capture the same node, the last one wins — the
/// highlight queries are written against that rule (general patterns first,
/// special cases after).
fn query_spans(grammar: &Grammar, tree: &Tree, rope: &Rope, bytes: std::ops::Range<usize>) -> Vec<Span> {
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(bytes);
    let text = |node: tree_sitter::Node| {
        rope.byte_slice(node.byte_range()).chunks().map(str::as_bytes)
    };
    let mut spans: Vec<Span> = Vec::new();
    // The node the previous capture was on, and whether it produced a span.
    let mut last: Option<(usize, bool)> = None;
    for (m, idx) in cursor.captures(&grammar.query, tree.root_node(), text) {
        let capture = m.captures[idx];
        let node = capture.node;
        if last == Some((node.id(), true)) {
            // A later pattern on the same node replaces the earlier one.
            spans.pop();
        }
        let hl = grammar.capture_map[capture.index as usize];
        let (start, end) = (rope.byte_to_char(node.start_byte()), rope.byte_to_char(node.end_byte()));
        if let Some(hl) = hl.filter(|_| start < end) {
            spans.push((start, end, hl));
        }
        last = Some((node.id(), hl.is_some() && start < end));
    }
    spans
}

/// The change `rope` is about to undergo, as tree-sitter needs it: replacing
/// chars `start..old_end` with `new_text`.  Call **before** mutating the rope.
pub fn input_edit(rope: &Rope, start: usize, old_end: usize, new_text: &str) -> InputEdit {
    let point_at = |byte: usize| {
        let line = rope.byte_to_line(byte);
        Point::new(line, byte - rope.line_to_byte(line))
    };
    let start_byte = rope.char_to_byte(start);
    let old_end_byte = rope.char_to_byte(old_end);
    let start_position = point_at(start_byte);
    let new_end_position = match new_text.rfind('\n') {
        Some(nl) => Point::new(
            start_position.row + new_text.matches('\n').count(),
            new_text.len() - nl - 1,
        ),
        None => Point::new(start_position.row, start_position.column + new_text.len()),
    };
    InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte: start_byte + new_text.len(),
        start_position,
        old_end_position: point_at(old_end_byte),
        new_end_position,
    }
}

/// Resolve possibly-overlapping spans into a sorted, non-overlapping list.
///
/// Where spans overlap, the later one (after a stable sort by start) wins —
/// the inner-scope-wins rule the highlighters are written against.  Adjacent
/// pieces with the same highlight are merged.  Done once per edit so
/// [`style_at`] can answer with a single binary search.
pub fn flatten(mut spans: Vec<Span>) -> Vec<Span> {
    spans.retain(|&(s, e, _)| s < e);
    spans.sort_by_key(|&(s, _, _)| s);
    if spans.windows(2).all(|w| w[0].1 <= w[1].0) {
        return spans;
    }

    // Sweep the span boundaries; the highest-index open span wins each piece.
    let mut events: Vec<(usize, usize)> = Vec::with_capacity(spans.len() * 2);
    for (i, &(s, e, _)) in spans.iter().enumerate() {
        events.push((s, i));
        events.push((e, i));
    }
    events.sort_unstable_by_key(|&(pos, _)| pos);

    let mut open: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    let mut prev = 0;
    let mut k = 0;
    while k < events.len() {
        let pos = events[k].0;
        if let Some(&top) = open.last() {
            let hl = spans[top].2;
            match out.last_mut() {
                Some(last) if last.1 == prev && last.2 == hl => last.1 = pos,
                _ if prev < pos => out.push((prev, pos, hl)),
                _ => {}
            }
        }
        while k < events.len() && events[k].0 == pos {
            let i = events[k].1;
            let (s, _, _) = spans[i];
            if s == pos {
                open.insert(i);
            } else {
                open.remove(&i);
            }
            k += 1;
        }
        prev = pos;
    }
    out
}

/// Return the ratatui `Style` for the span covering `char_idx`.
///
/// `spans` must be [`flatten`]ed: sorted and non-overlapping.
pub fn style_at(spans: &[Span], char_idx: usize) -> ratatui::style::Style {
    let right = spans.partition_point(|&(start, _, _)| start <= char_idx);
    match right.checked_sub(1).map(|i| spans[i]) {
        Some((_, end, hl)) if char_idx < end => crate::theme::style_for_highlight(hl),
        _ => ratatui::style::Style::default(),
    }
}

/// The highlight query shipped with `lang`'s grammar crate.
fn highlights_query(lang: Language) -> &'static str {
    match lang {
        Language::Rust => tree_sitter_rust::HIGHLIGHTS_QUERY,
        Language::Python => tree_sitter_python::HIGHLIGHTS_QUERY,
        Language::JavaScript => tree_sitter_javascript::HIGHLIGHT_QUERY,
        Language::Toml => tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        Language::Json => tree_sitter_json::HIGHLIGHTS_QUERY,
        Language::Yaml => tree_sitter_yaml::HIGHLIGHTS_QUERY,
        Language::Bash => tree_sitter_bash::HIGHLIGHT_QUERY,
        Language::Go => tree_sitter_go::HIGHLIGHTS_QUERY,
        Language::C => tree_sitter_c::HIGHLIGHT_QUERY,
        Language::Html => tree_sitter_html::HIGHLIGHTS_QUERY,
        Language::Css => tree_sitter_css::HIGHLIGHTS_QUERY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every supported language must produce a working highlight config and
    /// non-empty spans for a representative snippet — a query-syntax error in
    /// a grammar crate would otherwise silently disable highlighting.
    #[test]
    fn all_languages_highlight() {
        let samples: &[(&str, &str)] = &[
            ("rs", "fn main() { let x = 1; }"),
            ("py", "def f():\n    return 1\n"),
            ("js", "function f() { return 1; }"),
            ("toml", "[table]\nkey = \"value\"\n"),
            ("json", "{\"key\": [1, 2, true]}"),
            ("yaml", "key: value\nlist:\n  - 1\n"),
            ("sh", "if true; then echo hi; fi\n"),
            ("go", "func main() { x := 1 }"),
            ("c", "int main(void) { return 0; }"),
            ("html", "<html><body class=\"x\">hi</body></html>"),
            ("css", ".cls { color: red; }"),
        ];
        for (ext, src) in samples {
            let lang = Language::from_extension(ext)
                .unwrap_or_else(|| panic!("no language for extension {ext:?}"));
            assert!(grammar(lang).is_some(), "{lang:?}: highlight query failed to compile");
            let mut hl = Highlighter::new(Some(std::path::Path::new(&format!("x.{ext}"))));
            let spans = hl.highlight(&Rope::from_str(src));
            assert!(!spans.is_empty(), "{lang:?}: no highlight spans for {src:?}");
        }
    }

    /// Edits recorded through `input_edit` + `edit` give the same highlights
    /// as parsing the new text from scratch.
    #[test]
    fn incremental_reparse_matches_a_fresh_parse() {
        let path = std::path::Path::new("x.rs");
        let mut rope = Rope::from_str("fn main() {\n    let x = 1;\n}\n");
        let mut hl = Highlighter::new(Some(path));
        hl.highlight_lines(&rope, 7, 0..rope.len_lines());

        for (start, end, text) in [(16, 19, "\"a\""), (0, 0, "// c\n"), (5, 9, "")] {
            let edit = input_edit(&rope, start, end, text);
            rope.remove(start..end);
            rope.insert(start, text);
            hl.edit(&edit);
        }
        let incremental = hl.highlight_lines(&rope, 7, 0..rope.len_lines());

        assert_eq!(incremental, Highlighter::new(Some(path)).highlight(&rope));
    }

    /// Same rule as `tree_sitter_highlight`: the most specific known name.
    #[test]
    fn capture_names_map_to_the_most_specific_highlight() {
        let name = |i: Option<usize>| i.map(|i| HIGHLIGHT_NAMES[i]);
        assert_eq!(name(highlight_index("function.method.call")), Some("function.method"));
        assert_eq!(name(highlight_index("string.escape")), Some("string"));
        assert_eq!(name(highlight_index("unknown")), None);
    }

    #[test]
    fn flatten_resolves_overlaps_inner_wins() {
        // An outer span with an inner one in the middle, plus a later span.
        let flat = flatten(vec![(0, 10, 1), (3, 5, 2), (12, 14, 3)]);
        assert_eq!(flat, vec![(0, 3, 1), (3, 5, 2), (5, 10, 1), (12, 14, 3)]);
    }

    #[test]
    fn style_at_uncovered_char_is_default() {
        let flat = flatten(vec![(0, 2, 1), (5, 7, 2)]);
        assert_eq!(style_at(&flat, 3), ratatui::style::Style::default());
        assert_eq!(style_at(&flat, 6), crate::theme::style_for_highlight(2));
    }

    /// Shell dotfiles like `.zshrc` have no extension `Path::extension()` can
    /// see, so they must resolve to Bash via the filename fallback instead.
    #[test]
    fn shell_dotfiles_resolve_to_bash_by_filename() {
        for name in crate::lang::SHELL_DOTFILES {
            let path = std::path::Path::new(name);
            assert_eq!(
                Language::from_path(path),
                Some(Language::Bash),
                "{name:?} should resolve to Bash"
            );
        }
    }

    /// A path with a recognised extension must still take priority over any
    /// filename fallback.
    #[test]
    fn from_path_prefers_extension_over_filename() {
        let path = std::path::Path::new("main.py");
        assert_eq!(Language::from_path(path), Some(Language::Python));
    }
}
