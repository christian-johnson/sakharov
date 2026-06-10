use anyhow::Result;
use ropey::Rope;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter as TsHighlighter};

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

/// A highlighted span: (char_start, char_end, highlight_index).
pub type Span = (usize, usize, usize);

/// Detected language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Syntax highlighter wrapping tree-sitter.
pub struct Highlighter {
    pub language: Option<Language>,
    /// True when the open file is Markdown (`.md`/`.qmd`). Markdown is highlighted
    /// and folded by the custom, non-tree-sitter `crate::markdown` module.
    pub markdown: bool,
    config: Option<HighlightConfiguration>,
    /// Reused across calls — avoids allocating a new Parser on every highlight pass.
    ts_highlighter: TsHighlighter,
}

impl Highlighter {
    /// Create a highlighter, detecting language from the optional file path.
    pub fn new(path: Option<&std::path::Path>) -> Self {
        let language = path
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension);

        let config = language.and_then(|lang| build_config(lang).ok());

        Self {
            language,
            markdown: crate::markdown::is_markdown(path),
            config,
            ts_highlighter: TsHighlighter::new(),
        }
    }

    /// Compute the foldable line ranges for the current buffer contents.
    /// Routes to the markdown section/fence folder or the tree-sitter folder.
    pub fn fold_ranges(&self, rope: &Rope) -> Vec<crate::fold::FoldRange> {
        if self.markdown {
            crate::markdown::fold_ranges(rope)
        } else if let Some(lang) = self.language {
            crate::fold::compute_fold_ranges(rope, lang)
        } else {
            Vec::new()
        }
    }

    /// Compute highlight spans for the given rope contents.
    ///
    /// Returns a list of `(char_start, char_end, highlight_index)` triples.
    /// Takes `&mut self` so the internal tree-sitter parser can be reused.
    pub fn highlight(&mut self, rope: &Rope) -> Result<Vec<Span>> {
        if self.markdown {
            return Ok(crate::markdown::highlight(rope));
        }
        let config = match &self.config {
            Some(c) => c,
            None => return Ok(Vec::new()),
        };

        let text = rope.to_string();
        let source = text.as_bytes();

        let events =
            self.ts_highlighter.highlight(config, source, None, |_| None)?;

        let mut spans = Vec::new();
        let mut current_highlight: Option<usize> = None;

        for event in events {
            match event? {
                HighlightEvent::HighlightStart(h) => {
                    current_highlight = Some(h.0);
                }
                HighlightEvent::Source { start, end } => {
                    if let Some(hl) = current_highlight {
                        let char_start = rope.byte_to_char(start);
                        let char_end = rope.byte_to_char(end);
                        if char_start < char_end {
                            spans.push((char_start, char_end, hl));
                        }
                    }
                }
                HighlightEvent::HighlightEnd => {
                    current_highlight = None;
                }
            }
        }

        Ok(spans)
    }
}

/// Return the ratatui `Style` for whichever highlight span covers `char_idx`.
///
/// Spans may overlap; the last (highest-index) one that contains the index
/// wins — matching tree-sitter's inner-scope-wins rendering semantic.
///
/// Uses binary search on the sorted span list, so O(log n + depth) instead
/// of the previous O(n) linear scan.
pub fn style_at(spans: &[Span], char_idx: usize) -> ratatui::style::Style {
    // Find the first span index whose start > char_idx.
    let right = spans.partition_point(|&(start, _, _)| start <= char_idx);
    // Scan backward: the first span we find that covers char_idx is the
    // last-indexed one (innermost scope), which is the "last wins" winner.
    for i in (0..right).rev() {
        let (_, end, hl) = spans[i];
        if char_idx < end {
            return crate::theme::style_for_highlight(hl);
        }
        // end <= char_idx: this span finishes before char_idx.
        // An earlier (longer) span might still cover it, so keep scanning.
    }
    ratatui::style::Style::default()
}

/// Build a `HighlightConfiguration` for the given language.
fn build_config(lang: Language) -> Result<HighlightConfiguration> {
    let (highlights_query, injections_query, locals_query) = match lang {
        Language::Rust => (tree_sitter_rust::HIGHLIGHTS_QUERY, "", ""),
        Language::Python => (tree_sitter_python::HIGHLIGHTS_QUERY, "", ""),
        Language::JavaScript => (
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::INJECTIONS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY,
        ),
        Language::Toml => (tree_sitter_toml_ng::HIGHLIGHTS_QUERY, "", ""),
        Language::Json => (tree_sitter_json::HIGHLIGHTS_QUERY, "", ""),
        Language::Yaml => (tree_sitter_yaml::HIGHLIGHTS_QUERY, "", ""),
        Language::Bash => (tree_sitter_bash::HIGHLIGHT_QUERY, "", ""),
        Language::Go => (tree_sitter_go::HIGHLIGHTS_QUERY, "", ""),
        Language::C => (tree_sitter_c::HIGHLIGHT_QUERY, "", ""),
        Language::Html => (
            tree_sitter_html::HIGHLIGHTS_QUERY,
            tree_sitter_html::INJECTIONS_QUERY,
            "",
        ),
        Language::Css => (tree_sitter_css::HIGHLIGHTS_QUERY, "", ""),
    };

    let mut config = HighlightConfiguration::new(
        lang.ts_language(),
        "highlights",
        highlights_query,
        injections_query,
        locals_query,
    )?;
    config.configure(HIGHLIGHT_NAMES);
    Ok(config)
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
            let config = build_config(lang)
                .unwrap_or_else(|e| panic!("{lang:?}: highlight query failed to compile: {e}"));
            let mut hl = Highlighter {
                language: Some(lang),
                markdown: false,
                config: Some(config),
                ts_highlighter: TsHighlighter::new(),
            };
            let spans = hl.highlight(&Rope::from_str(src)).expect("highlight runs");
            assert!(!spans.is_empty(), "{lang:?}: no highlight spans for {src:?}");
        }
    }
}
