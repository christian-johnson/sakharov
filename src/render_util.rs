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
/// instead — see `notebook_ui::wrap_segments`.)
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
