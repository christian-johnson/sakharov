//! Custom Markdown support: header-aware folding and a lightweight inline
//! highlighter. Markdown isn't handled through tree-sitter (its split
//! block/inline grammar doesn't fit the single-grammar `tree_sitter_highlight`
//! flow cleanly), so this module produces the same `highlight::Span` list and
//! `fold::FoldRange` list the rest of the editor consumes — no new dependency.
//!
//! Applies to `.md`, `.markdown`, and `.qmd` files.

use std::path::Path;

use ropey::Rope;

use crate::fold::FoldRange;
use crate::highlight::{
    Span, MD_BOLD, MD_HEADING_1, MD_HEADING_2, MD_HEADING_3, MD_HEADING_4, MD_HEADING_5,
    MD_HEADING_6, MD_ITALIC, MD_LINK, MD_LIST, MD_QUOTE, MD_RAW,
};

/// True if `path` names a Markdown document we should highlight/fold.
pub fn is_markdown(path: Option<&Path>) -> bool {
    path.and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "qmd"))
        .unwrap_or(false)
}

/// ATX heading level (1–6) if `trimmed` (leading whitespace already removed)
/// begins with 1–6 `#` followed by a space/tab or end-of-line.
fn atx_level(trimmed: &str) -> Option<usize> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &trimmed[hashes..];
        if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t') {
            return Some(hashes);
        }
    }
    None
}

/// True if `trimmed` opens or closes a fenced code block (``` or ~~~).
fn is_fence(trimmed: &str) -> bool {
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// Length (in chars) of a leading list marker in `s` (whitespace already
/// stripped), including the trailing space — e.g. `- ` → 2, `12. ` → 4.
fn list_marker_len(s: &str) -> Option<usize> {
    let mut chars = s.chars();
    match chars.next() {
        Some('-') | Some('*') | Some('+') => {
            if s.chars().nth(1) == Some(' ') {
                Some(2)
            } else {
                None
            }
        }
        Some(c) if c.is_ascii_digit() => {
            let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
            let rest = &s[digits..];
            if rest.starts_with(". ") || rest.starts_with(") ") {
                Some(digits + 2)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Compute highlight spans for an entire Markdown buffer.
pub fn highlight(rope: &Rope) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut in_fence = false;

    for line_idx in 0..rope.len_lines() {
        let line_start = rope.line_to_char(line_idx);
        let text: String = rope.line(line_idx).chars().collect();
        let content = text.trim_end_matches(['\n', '\r']);
        let line_end = line_start + content.chars().count();
        let trimmed = content.trim_start();

        // Fenced code blocks: the whole block (including the fence lines)
        // renders as raw text, with no inline formatting inside.
        if in_fence {
            if line_end > line_start {
                spans.push((line_start, line_end, MD_RAW));
            }
            if is_fence(trimmed) {
                in_fence = false;
            }
            continue;
        }
        if is_fence(trimmed) {
            in_fence = true;
            if line_end > line_start {
                spans.push((line_start, line_end, MD_RAW));
            }
            continue;
        }

        // ATX headings colour the whole line by level.
        if let Some(level) = atx_level(trimmed) {
            let idx = match level {
                1 => MD_HEADING_1,
                2 => MD_HEADING_2,
                3 => MD_HEADING_3,
                4 => MD_HEADING_4,
                5 => MD_HEADING_5,
                _ => MD_HEADING_6,
            };
            if line_end > line_start {
                spans.push((line_start, line_end, idx));
            }
            continue;
        }

        // Blockquotes.
        if trimmed.starts_with('>') {
            if line_end > line_start {
                spans.push((line_start, line_end, MD_QUOTE));
            }
            continue;
        }

        // List marker (the bullet/number itself), then inline scan of the rest.
        let leading_ws = content.chars().take_while(|c| *c == ' ' || *c == '\t').count();
        let after_ws: String = content.chars().skip(leading_ws).collect();
        if let Some(marker_len) = list_marker_len(&after_ws) {
            let m_start = line_start + leading_ws;
            spans.push((m_start, m_start + marker_len, MD_LIST));
        }

        scan_inline(content, line_start, &mut spans);
    }

    spans
}

/// First index `>= from` where `chars[i] == target`.
fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == target)
}

/// First index `>= from` that begins a `delim delim` pair (start of the pair).
fn find_double(chars: &[char], from: usize, delim: char) -> Option<usize> {
    let n = chars.len();
    let mut i = from;
    while i + 1 < n {
        if chars[i] == delim && chars[i + 1] == delim {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// First index `>= from` of a single `delim` that closes an italic run (not part
/// of a `**`/`__` pair). Underscores must close on a word boundary so that
/// identifiers like `my_var_name` are not italicised.
fn find_italic_close(chars: &[char], from: usize, delim: char) -> Option<usize> {
    let n = chars.len();
    let mut j = from;
    while j < n {
        if chars[j] == delim {
            if j + 1 < n && chars[j + 1] == delim {
                // Part of a double delimiter — skip both.
                j += 2;
                continue;
            }
            if delim == '_' && j + 1 < n && chars[j + 1].is_alphanumeric() {
                j += 1;
                continue;
            }
            if j > from {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

/// Scan one line's `content` for inline emphasis, code, and links, pushing
/// spans whose char offsets are relative to `base` (the line's start char).
fn scan_inline(content: &str, base: usize, spans: &mut Vec<Span>) {
    let chars: Vec<char> = content.chars().collect();
    let n = chars.len();
    let mut i = 0;

    while i < n {
        let c = chars[i];

        // Inline code `...` — wins over emphasis and contains no formatting.
        if c == '`' {
            if let Some(close) = find_char(&chars, i + 1, '`') {
                spans.push((base + i, base + close + 1, MD_RAW));
                i = close + 1;
                continue;
            }
        }

        // Link [text](url).
        if c == '[' {
            if let Some(rb) = find_char(&chars, i + 1, ']') {
                if rb + 1 < n && chars[rb + 1] == '(' {
                    if let Some(rp) = find_char(&chars, rb + 2, ')') {
                        spans.push((base + i, base + rp + 1, MD_LINK));
                        i = rp + 1;
                        continue;
                    }
                }
            }
        }

        // Bold **...** / __...__.
        if (c == '*' || c == '_') && i + 1 < n && chars[i + 1] == c {
            if let Some(close) = find_double(&chars, i + 2, c) {
                spans.push((base + i, base + close + 2, MD_BOLD));
                i = close + 2;
                continue;
            }
        }

        // Italic *...* / _..._.
        if c == '*' || c == '_' {
            let prev_alnum = i > 0 && chars[i - 1].is_alphanumeric();
            if !(c == '_' && prev_alnum) {
                if let Some(close) = find_italic_close(&chars, i + 1, c) {
                    spans.push((base + i, base + close + 1, MD_ITALIC));
                    i = close + 1;
                    continue;
                }
            }
        }

        i += 1;
    }
}

/// Compute foldable ranges: each heading folds down to (but not including) the
/// next heading of equal-or-higher level, and each fenced code block folds.
pub fn fold_ranges(rope: &Rope) -> Vec<FoldRange> {
    let total = rope.len_lines();
    if total == 0 {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut headings: Vec<(usize, usize)> = Vec::new(); // (line, level)
    let mut in_fence = false;
    let mut fence_start: Option<usize> = None;

    for line_idx in 0..total {
        let text: String = rope.line(line_idx).chars().collect();
        let trimmed = text.trim_end_matches(['\n', '\r']).trim_start();

        if in_fence {
            if is_fence(trimmed) {
                if let Some(s) = fence_start.take() {
                    if line_idx > s {
                        ranges.push((s, line_idx));
                    }
                }
                in_fence = false;
            }
            continue;
        }
        if is_fence(trimmed) {
            in_fence = true;
            fence_start = Some(line_idx);
            continue;
        }
        if let Some(level) = atx_level(trimmed) {
            headings.push((line_idx, level));
        }
    }

    let last_line = total - 1;
    for (idx, &(line, level)) in headings.iter().enumerate() {
        let mut end = last_line;
        for &(next_line, next_level) in &headings[idx + 1..] {
            if next_level <= level {
                end = next_line.saturating_sub(1);
                break;
            }
        }
        if end > line {
            ranges.push((line, end));
        }
    }

    ranges.sort_by_key(|&(s, _)| s);
    ranges.dedup_by_key(|&mut (s, _)| s);
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detects_markdown_extensions() {
        assert!(is_markdown(Some(Path::new("README.md"))));
        assert!(is_markdown(Some(Path::new("notes.QMD"))));
        assert!(is_markdown(Some(Path::new("a.markdown"))));
        assert!(!is_markdown(Some(Path::new("main.rs"))));
        assert!(!is_markdown(None));
    }

    #[test]
    fn atx_levels() {
        assert_eq!(atx_level("# Title"), Some(1));
        assert_eq!(atx_level("###### Deep"), Some(6));
        assert_eq!(atx_level("####### TooDeep"), None);
        assert_eq!(atx_level("#NoSpace"), None);
        assert_eq!(atx_level("not a heading"), None);
    }

    #[test]
    fn folds_nested_sections_and_fences() {
        let src = "\
# A
text
## B
more
# C
```
code
```
";
        let rope = Rope::from_str(src);
        let ranges = fold_ranges(&rope);
        // # A (line 0) extends to just before # C (line 4) → (0, 3)
        assert!(ranges.contains(&(0, 3)), "ranges = {ranges:?}");
        // ## B (line 2) extends to just before # C → (2, 3)
        assert!(ranges.contains(&(2, 3)), "ranges = {ranges:?}");
        // fenced block spans its two fence lines (5, 7)
        assert!(ranges.contains(&(5, 7)), "ranges = {ranges:?}");
    }

    #[test]
    fn highlights_heading_and_inline() {
        // "# Hi" then a body line with bold, italic, code, link.
        let body = "a **b** _c_ `d` [e](f)";
        let src = format!("# Hi\n{body}\n");
        let rope = Rope::from_str(&src);
        let spans = highlight(&rope);

        // Heading span covers the whole first line with the H1 index.
        assert!(spans.iter().any(|&(s, e, h)| s == 0 && e == 4 && h == MD_HEADING_1));

        // Each inline construct produces a span over exactly its source text.
        let span_text = |idx: usize, frag: &str| {
            spans
                .iter()
                .any(|&(s, e, h)| h == idx && rope.slice(s..e).to_string() == frag)
        };
        assert!(span_text(MD_BOLD, "**b**"), "spans={spans:?}");
        assert!(span_text(MD_ITALIC, "_c_"), "spans={spans:?}");
        assert!(span_text(MD_RAW, "`d`"), "spans={spans:?}");
        assert!(span_text(MD_LINK, "[e](f)"), "spans={spans:?}");
    }

    #[test]
    fn underscore_in_identifier_not_italic() {
        let rope = Rope::from_str("call my_var_name here\n");
        let spans = highlight(&rope);
        assert!(!spans.iter().any(|&(_, _, h)| h == MD_ITALIC), "spans={spans:?}");
    }
}
