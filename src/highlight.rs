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
}

impl Language {
    /// Detect language from a file extension.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "js" | "jsx" => Some(Self::JavaScript),
            _ => None,
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

    /// Update the language based on a new path.
    #[allow(dead_code)]
    pub fn set_path(&mut self, path: Option<&std::path::Path>) {
        let language = path
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension);

        self.markdown = crate::markdown::is_markdown(path);
        if language != self.language {
            self.language = language;
            self.config = language.and_then(|lang| build_config(lang).ok());
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
        let mut byte_start: usize = 0;

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
                    byte_start = end;
                }
                HighlightEvent::HighlightEnd => {
                    current_highlight = None;
                }
            }
        }
        let _ = byte_start;

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
    let (ts_lang, highlights_query, injections_query, locals_query) = match lang {
        Language::Rust => (
            tree_sitter_rust::language(),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
        Language::Python => (
            tree_sitter_python::language(),
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
        Language::JavaScript => (
            tree_sitter_javascript::language(),
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::INJECTIONS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY,
        ),
    };

    let mut config =
        HighlightConfiguration::new(ts_lang, "highlights", highlights_query, injections_query, locals_query)?;
    config.configure(HIGHLIGHT_NAMES);
    Ok(config)
}
