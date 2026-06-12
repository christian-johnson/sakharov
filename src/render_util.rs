//! Small rendering helpers shared by the plain-editor (`ui`) and notebook
//! (`notebook_ui`) renderers.

use ratatui::{
    buffer::Buffer as RatBuffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::Widget,
};
use unicode_width::UnicodeWidthChar;

use crate::lsp_manager::DiagnosticSeverity;

/// Display width of `c` at display column `col` (tabs advance to the next stop).
pub fn char_display_width(c: char, col: usize, tab_width: usize) -> usize {
    if c == '\t' {
        tab_width - (col % tab_width)
    } else {
        c.width().unwrap_or(1)
    }
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
