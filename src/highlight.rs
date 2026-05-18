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
];

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
    #[allow(dead_code)]
    language: Option<Language>,
    config: Option<HighlightConfiguration>,
}

impl Highlighter {
    /// Create a highlighter, detecting language from the optional file path.
    pub fn new(path: Option<&std::path::Path>) -> Self {
        let language = path
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension);

        let config = language.and_then(|lang| build_config(lang).ok());

        Self { language, config }
    }

    /// Update the language based on a new path.
    #[allow(dead_code)]
    pub fn set_path(&mut self, path: Option<&std::path::Path>) {
        let language = path
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension);

        if language != self.language {
            self.language = language;
            self.config = language.and_then(|lang| build_config(lang).ok());
        }
    }

    /// Compute highlight spans for the given rope contents.
    ///
    /// Returns a list of `(char_start, char_end, highlight_index)` triples.
    pub fn highlight(&self, rope: &Rope) -> Result<Vec<Span>> {
        let config = match &self.config {
            Some(c) => c,
            None => return Ok(Vec::new()),
        };

        let text = rope.to_string();
        let source = text.as_bytes();

        let mut highlighter = TsHighlighter::new();
        let events =
            highlighter.highlight(config, source, None, |_| None)?;

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
