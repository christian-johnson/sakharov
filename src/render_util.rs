//! Small rendering helpers shared by the plain-editor (`ui`) and notebook
//! (`notebook_ui`) renderers.

use ratatui::{
    buffer::Buffer as RatBuffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::Widget,
};
use unicode_width::UnicodeWidthChar;

use crate::lsp_manager::{Diagnostic, DiagnosticSeverity};

/// Display width of `c` at display column `col` (tabs advance to the next stop).
pub fn char_display_width(c: char, col: usize, tab_width: usize) -> usize {
    if c == '\t' {
        tab_width - (col % tab_width)
    } else {
        c.width().unwrap_or(1)
    }
}

/// Walk `line`'s soft-wrap breaks, calling `on_row_start` with the char offset
/// (within the line) at which each visual row begins — starting with 0, so it
/// fires at least once even for an empty line.
///
/// This is the plain editor's wrap rule in one place: a **hard** break at
/// `text_width` display columns, with tab stops measured from the start of the
/// visual row (which is what the renderer draws). Cursor motion, the scroll
/// math and the renderer all derive their row geometry from it, so they cannot
/// disagree about where a line breaks. (Notebook cells wrap at word boundaries
/// instead — see [`wrap_segments`].)
pub fn scan_wrap_rows(
    line: ropey::RopeSlice<'_>,
    text_width: usize,
    tab_width: usize,
    mut on_row_start: impl FnMut(usize),
) {
    on_row_start(0);
    if text_width == 0 {
        return;
    }
    let mut col = 0usize;
    for (i, c) in line.chars().enumerate() {
        if c == '\n' || c == '\r' {
            break;
        }
        col += char_display_width(c, col, tab_width);
        if col >= text_width {
            on_row_start(i + 1);
            col = 0;
        }
    }
}

/// Char offsets within `line` at which each soft-wrapped visual row starts.
/// Always non-empty (`[0]` for a line that doesn't wrap).
pub fn wrap_row_starts(line: ropey::RopeSlice<'_>, text_width: usize, tab_width: usize) -> Vec<usize> {
    let mut starts = Vec::new();
    scan_wrap_rows(line, text_width, tab_width, |o| starts.push(o));
    starts
}

/// Word-wrap a logical line into visual-row segments of at most `width` chars.
///
/// The **word-boundary** wrap rule, as opposed to [`scan_wrap_rows`]'s hard
/// break at the text width: notebook cells, cell outputs and the table view's
/// cell peek all wrap this way, and the renderers, the height models and
/// `motion::move_visual_up/_down` must all derive their rows from here.
///
/// Breaks at the last space within the window when possible (the space is
/// consumed by the break); a single word longer than `width` is hard-broken.
/// Returns `(char_offset_within_line, segment)` pairs — always at least one,
/// so an empty line still occupies one row. Char-based, like the rest of the
/// cell renderer (the width-1-chars assumption is a known rough edge).
pub fn wrap_segments(line: &str, width: usize) -> Vec<(usize, &str)> {
    let width = width.max(1);
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let n = chars.len();
    if n <= width {
        return vec![(0, line)];
    }
    let byte_at = |ci: usize| if ci < n { chars[ci].0 } else { line.len() };
    let mut segs = Vec::new();
    let mut start = 0usize; // char index of the current segment's first char
    while n - start > width {
        let limit = start + width; // exclusive end of a full-width segment
        // A space at `limit` itself is the ideal break: the segment is exactly
        // full and the space dies at the boundary.
        let brk = (start + 1..=limit).rev().find(|&i| chars[i].1 == ' ');
        let (end, next) = match brk {
            Some(i) => (i, i + 1),
            None => (limit, limit),
        };
        segs.push((start, &line[byte_at(start)..byte_at(end)]));
        start = next;
    }
    segs.push((start, &line[byte_at(start)..]));
    segs
}

/// Add a severity-coloured underline to `style` when any diagnostic covers the
/// character: red for errors, yellow for anything else. `severities` yields the
/// severities of all diagnostics covering the character.
pub fn apply_diag_underline<'a>(
    style: Style,
    mut severities: impl Iterator<Item = &'a DiagnosticSeverity>,
) -> Style {
    let mut worst: Option<&DiagnosticSeverity> = None;
    for sev in &mut severities {
        if *sev == DiagnosticSeverity::Error {
            worst = Some(&DiagnosticSeverity::Error);
            break;
        }
        worst = Some(sev);
    }
    match worst {
        Some(DiagnosticSeverity::Error) => style
            .add_modifier(Modifier::UNDERLINED)
            .underline_color(crate::theme::active().error),
        Some(_) => style
            .add_modifier(Modifier::UNDERLINED)
            .underline_color(crate::theme::active().warning),
        None => style,
    }
}

/// The diagnostic covering char-column `col` of `line`, if any — preferring the
/// most severe when several overlap (matches `apply_diag_underline`'s pick).
pub fn diagnostic_at(diagnostics: &[Diagnostic], line: usize, col: usize) -> Option<&Diagnostic> {
    diagnostics
        .iter()
        .filter(|d| d.line == line && col >= d.col_start && col < d.col_end)
        .max_by_key(|d| match d.severity {
            DiagnosticSeverity::Error => 3,
            DiagnosticSeverity::Warning => 2,
            DiagnosticSeverity::Information => 1,
            DiagnosticSeverity::Hint => 0,
        })
}

/// The two styles for `gw` jump-label overlays: (pending, confirmed).
/// "Confirmed" chars are the prefix the user has already typed.
pub fn jump_label_styles() -> (Style, Style) {
    let th = crate::theme::active();
    let pending_bg = th.modes.jump;
    let confirmed_bg = th.success;
    let pending = Style::default()
        .fg(crate::theme::contrast_fg(pending_bg))
        .bg(pending_bg)
        .add_modifier(Modifier::BOLD);
    let confirmed = Style::default()
        .fg(crate::theme::contrast_fg(confirmed_bg))
        .bg(confirmed_bg)
        .add_modifier(Modifier::BOLD);
    (pending, confirmed)
}

/// Walk every jump-label character that lands on the line starting at
/// `line_start_char` with `line_len` content chars, calling
/// `paint(char_offset_in_line, label_char, style)` for each. Labels whose
/// prefix doesn't match `typed` are skipped entirely.
pub fn for_each_jump_label_char(
    labels: &[(usize, String)],
    typed: &str,
    line_start_char: usize,
    line_len: usize,
    mut paint: impl FnMut(usize, char, Style),
) {
    if labels.is_empty() {
        return;
    }
    let (pending, confirmed) = jump_label_styles();
    let typed_len = typed.len();
    for (pos, label) in labels {
        if !label.starts_with(typed) || *pos < line_start_char {
            continue;
        }
        let char_off = pos - line_start_char;
        if char_off >= line_len {
            continue;
        }
        for (i, lc) in label.chars().enumerate() {
            let style = if i < typed_len { confirmed } else { pending };
            paint(char_off + i, lc, style);
        }
    }
}

/// A 1-row widget that clears its area with `style` then prints `text`.
pub struct SingleLineWidget {
    pub text: String,
    pub style: Style,
}

impl Widget for SingleLineWidget {
    fn render(self, area: Rect, buf: &mut RatBuffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let y = area.top();
        for col in area.left()..area.right() {
            buf[(col, y)].set_char(' ').set_style(self.style);
        }
        for (x, c) in (area.left()..area.right()).zip(self.text.chars()) {
            buf[(x, y)].set_char(c).set_style(self.style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_segments_breaks_at_word_boundaries() {
        // Width 10: "hello brave world" → "hello" / "brave" / "world"
        let segs = wrap_segments("hello brave world", 10);
        let texts: Vec<&str> = segs.iter().map(|&(_, s)| s).collect();
        assert_eq!(texts, vec!["hello", "brave", "world"]);
        // Offsets address the original line (for highlight-span lookup).
        assert_eq!(segs[1].0, 6);
        assert_eq!(segs[2].0, 12);
        // Every segment fits the width.
        assert!(segs.iter().all(|&(_, s)| s.chars().count() <= 10));
    }

    #[test]
    fn wrap_segments_hard_breaks_long_words_and_keeps_short_lines() {
        let segs = wrap_segments("abcdefghij", 4);
        let texts: Vec<&str> = segs.iter().map(|&(_, s)| s).collect();
        assert_eq!(texts, vec!["abcd", "efgh", "ij"]);
        // Short and empty lines occupy exactly one row.
        assert_eq!(wrap_segments("short", 80).len(), 1);
        assert_eq!(wrap_segments("", 80).len(), 1);
    }
}
