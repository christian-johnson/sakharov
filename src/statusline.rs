//! Configurable, starship-style status line shared by the plain editor and the
//! notebook view.
//!
//! The layout is driven by two ordered lists of *module* names (`left` and
//! `right`) from `[statusline]` (or `[statusline.notebook]`) in the config. Each
//! module expands to zero or more styled [`Segment`]s given the current
//! [`Ctx`]; empty modules contribute nothing. Adjacent modules are automatically
//! padded with a single leading and trailing space (so they never appear
//! smashed together), and separated by the configured `separator` string.
//!
//! A name that doesn't match a known module is rendered as literal text, so a
//! config like `left = ["mode", "│", "git", "file"]` works as a custom
//! separator.  This single renderer replaced the two hand-rolled status widgets
//! that previously lived in `ui.rs` and `notebook_ui.rs`.

use std::collections::HashMap;

use ratatui::{
    buffer::Buffer as RatBuffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    Frame,
};
use unicode_width::UnicodeWidthChar;

/// Kernel state as far as the status line is concerned (notebook only).
#[derive(Clone, Copy)]
pub enum KernelView {
    Idle,
    Busy,
    Dead,
    None,
}

/// Everything the modules might need to render.  Fields irrelevant to the
/// current view (e.g. `cell`/`kernel` in the plain editor) are left `None`.
pub struct Ctx {
    pub mode_label: String,
    pub mode_color: Color,
    pub filename: String,
    pub modified: bool,
    pub branch: Option<String>,
    pub diag_errors: usize,
    pub diag_warnings: usize,
    pub line: usize,
    pub col: usize,
    pub scroll_pct: usize,
    /// Animated spinner glyph; `None` when no background task is running.
    pub spinner: Option<char>,
    /// `(current_1_based, total)` cell position — notebook only.
    pub cell: Option<(usize, usize)>,
    /// Kernel state — notebook only.
    pub kernel: Option<KernelView>,
}

#[derive(Clone)]
pub struct Segment {
    pub text: String,
    pub style: Style,
}

impl Segment {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Segment { text: text.into(), style }
    }
}

/// The status-bar background / default text style.
fn base_style() -> Style {
    Style::default().bg(Color::DarkGray).fg(Color::White)
}

fn dim_style() -> Style {
    Style::default().bg(Color::DarkGray).fg(Color::Rgb(170, 170, 170))
}

/// Parse a `#rrggbb` hex color string into a ratatui `Color`.
fn parse_hex_color(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Color::Rgb(r, g, b))
    } else {
        None
    }
}

/// The bar's base background color — used for separator tinting and padding.
const BAR_BG: Color = Color::DarkGray;

/// Ensure a module's segment list has a leading space on the first segment and
/// a trailing space on the last.  Skips sides that already have a space so
/// `mode`'s `" NOR "` is never double-padded.
fn pad_module(segs: &mut [Segment]) {
    if segs.is_empty() {
        return;
    }
    if !segs[0].text.starts_with(' ') {
        segs[0].text.insert(0, ' ');
    }
    let last = segs.len() - 1;
    if !segs[last].text.ends_with(' ') {
        segs[last].text.push(' ');
    }
}

/// True when `sep` names a powerline glyph style rather than a literal string.
/// Powerline mode renders filled transition glyphs (requires Nerd Fonts) and
/// colors each module with a distinct background.
fn is_powerline(sep: &str) -> bool {
    matches!(sep, ">" | "/" | "\\" | "round")
}

/// Return (left_side_glyph, right_side_glyph) for a powerline separator style.
///
/// Left-side glyphs point → and are appended after each left-group module.
/// Right-side glyphs point ← and are prepended before each right-group module.
///
/// All glyphs are from the Nerd Fonts powerline-extra range (U+E0B0–U+E0BF).
fn powerline_glyphs(sep: &str) -> (char, char) {
    match sep {
        "/"     => ('\u{e0bc}', '\u{e0be}'),   // slanted (upper diagonal)
        "\\"    => ('\u{e0b8}', '\u{e0ba}'),   // reverse slant (lower diagonal)
        "round" => ('\u{e0b4}', '\u{e0b6}'),   // half-circle bubble
        _       => ('\u{e0b0}', '\u{e0b2}'),   // ">" — solid arrow (default)
    }
}

/// Background color for a named module.  Looks up `styles` first, then falls
/// back to `mode_color` for the mode chip and `BAR_BG` for everything else.
fn module_bg(name: &str, ctx: &Ctx, styles: &HashMap<String, String>) -> Color {
    if let Some(c) = styles.get(name).and_then(|s| parse_hex_color(s)) {
        return c;
    }
    if name == "mode" { ctx.mode_color } else { BAR_BG }
}

/// Pick a contrasting foreground for a given background.
fn fg_for_bg(bg: Color) -> Color {
    let lum: f32 = match bg {
        Color::Rgb(r, g, b) => {
            r as f32 * 0.299 + g as f32 * 0.587 + b as f32 * 0.114
        }
        Color::White | Color::Yellow | Color::LightYellow
        | Color::Green | Color::LightGreen | Color::Cyan | Color::LightCyan => 180.0,
        _ => 0.0,
    };
    if lum > 128.0 { Color::Black } else { Color::White }
}

/// Apply a module's background color to all its segments.  When the bg is the
/// default bar color the existing foreground (semantic red/yellow/cyan) is
/// preserved; when a custom bg is set both fg and bg are overridden for contrast.
fn apply_module_bg(segs: &mut [Segment], bg: Color) {
    let custom = !matches!(bg, Color::DarkGray);
    let fg = if custom { Some(fg_for_bg(bg)) } else { None };
    for seg in segs.iter_mut() {
        seg.style = seg.style.bg(bg);
        if let Some(f) = fg {
            seg.style = seg.style.fg(f);
        }
    }
}

/// Expand a single module name into styled segments.  Unknown names render as
/// literal text so they can double as separators / decoration.
fn expand(name: &str, ctx: &Ctx) -> Vec<Segment> {
    let base = base_style();
    match name {
        "mode" => vec![Segment::new(
            format!(" {} ", ctx.mode_label.trim()),
            Style::default()
                .fg(fg_for_bg(ctx.mode_color))
                .bg(ctx.mode_color)
                .add_modifier(Modifier::BOLD),
        )],
        "file" | "filename" => {
            let modified = if ctx.modified { " [+]" } else { "" };
            vec![Segment::new(format!("{}{modified}", ctx.filename), base)]
        }
        "git" | "branch" | "git_branch" => match &ctx.branch {
            Some(b) if !b.is_empty() => {
                vec![Segment::new(format!("\u{e0a0} {b}"), dim_style())]
            }
            _ => vec![],
        },
        "diagnostics" | "diag" => {
            let mut segs = Vec::new();
            if ctx.diag_errors > 0 {
                segs.push(Segment::new(
                    format!("\u{25cf}{}", ctx.diag_errors),
                    base.fg(Color::Red),
                ));
            }
            if ctx.diag_warnings > 0 {
                if !segs.is_empty() {
                    segs.push(Segment::new(" ", base));
                }
                segs.push(Segment::new(
                    format!("\u{25c6}{}", ctx.diag_warnings),
                    base.fg(Color::Yellow),
                ));
            }
            segs
        }
        "position" | "pos" => vec![Segment::new(format!("{}:{}", ctx.line, ctx.col), base)],
        "scroll" | "scroll_percent" => vec![Segment::new(format!("{}%", ctx.scroll_pct), base)],
        "spinner" => match ctx.spinner {
            Some(g) => vec![Segment::new(g.to_string(), base.fg(Color::Cyan))],
            None => vec![],
        },
        "cell" | "cell_position" => match ctx.cell {
            Some((cur, total)) => vec![Segment::new(format!("{cur}/{total}"), base)],
            None => vec![],
        },
        "kernel" => match ctx.kernel {
            Some(KernelView::Idle) => vec![Segment::new("[idle]", dim_style())],
            Some(KernelView::Busy) => {
                // Fold the live spinner into the busy indicator when available.
                let label = match ctx.spinner {
                    Some(g) => format!("[{g} busy]"),
                    None => "[busy]".to_string(),
                };
                vec![Segment::new(label, base.fg(Color::Cyan))]
            }
            Some(KernelView::Dead) => vec![Segment::new("[dead]", base.fg(Color::Red))],
            Some(KernelView::None) => vec![Segment::new("[no kernel]", dim_style())],
            None => vec![],
        },
        // Unknown → literal text (acts as a user-defined separator / label).
        other => vec![Segment::new(other.to_string(), base)],
    }
}

// ---------------------------------------------------------------------------
// Module group builders
// ---------------------------------------------------------------------------

/// Plain (non-powerline) group: pad each module, insert a literal separator
/// between adjacent modules, apply per-module fg color overrides.
fn build_group_plain(
    modules: &[String],
    ctx: &Ctx,
    separator: &str,
    styles: &HashMap<String, String>,
) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for name in modules {
        let mut segs = expand(name, ctx);
        if segs.is_empty() {
            continue;
        }
        pad_module(&mut segs);
        // Plain mode: `styles` entries are fg color overrides.
        if let Some(color) = styles.get(name.as_str()).and_then(|s| parse_hex_color(s)) {
            for seg in &mut segs {
                seg.style = seg.style.fg(color);
            }
        }
        if !out.is_empty() && !separator.is_empty() {
            out.push(Segment::new(separator.to_string(), base_style()));
        }
        out.extend(segs);
    }
    out
}

/// Powerline group: each module gets a distinct background color; separator
/// glyphs are tinted with adjacent module colors to create seamless transitions.
///
/// Requires Nerd Fonts for the glyph codepoints (U+E0B0–U+E0BF).
///
/// Left side (→):  content  ►  content  ►  content  ►  (tail to bar bg)
/// Right side (←): ◄  content  ◄  content  ◄  content
///
/// `styles` entries specify module *background* colors; fg is auto-calculated
/// for contrast.  Unknown names and `mode` fall back to `ctx.mode_color` /
/// `BAR_BG` respectively.
fn build_group_powerline(
    modules: &[String],
    ctx: &Ctx,
    sep: &str,
    styles: &HashMap<String, String>,
    left_side: bool,
) -> Vec<Segment> {
    let (left_glyph, right_glyph) = powerline_glyphs(sep);

    // Collect (segments, bg) for every non-empty module.
    let entries: Vec<(Vec<Segment>, Color)> = modules
        .iter()
        .filter_map(|name| {
            let mut segs = expand(name, ctx);
            if segs.is_empty() {
                return None;
            }
            pad_module(&mut segs);
            let bg = module_bg(name, ctx, styles);
            apply_module_bg(&mut segs, bg);
            Some((segs, bg))
        })
        .collect();

    if entries.is_empty() {
        return vec![];
    }

    let mut out = Vec::new();

    if left_side {
        // Append a right-pointing glyph after every module (including the last,
        // which transitions back to the bar background).
        for (i, (segs, bg)) in entries.iter().enumerate() {
            out.extend(segs.iter().cloned());
            let next_bg = entries.get(i + 1).map(|(_, c)| *c).unwrap_or(BAR_BG);
            out.push(Segment::new(
                left_glyph.to_string(),
                Style::default().fg(*bg).bg(next_bg),
            ));
        }
    } else {
        // Prepend a left-pointing glyph before every module.  The glyph's fg
        // is this module's bg (the solid triangle color); the glyph's bg is
        // what lies to its left (bar bg for the first module, previous module's
        // bg for subsequent ones).
        for (i, (segs, bg)) in entries.iter().enumerate() {
            let prev_bg = if i == 0 { BAR_BG } else { entries[i - 1].1 };
            out.push(Segment::new(
                right_glyph.to_string(),
                Style::default().fg(*bg).bg(prev_bg),
            ));
            out.extend(segs.iter().cloned());
        }
    }

    out
}

fn build_group(
    modules: &[String],
    ctx: &Ctx,
    separator: &str,
    styles: &HashMap<String, String>,
    left_side: bool,
) -> Vec<Segment> {
    if is_powerline(separator) {
        build_group_powerline(modules, ctx, separator, styles, left_side)
    } else {
        build_group_plain(modules, ctx, separator, styles)
    }
}

fn group_width(segs: &[Segment]) -> u16 {
    segs.iter()
        .flat_map(|s| s.text.chars())
        .map(|c| c.width().unwrap_or(0) as u16)
        .sum()
}

/// Render the status line into `area` using the given left/right module lists.
///
/// When `separator` is `">"`, `"/"`, `"\\"`, or `"round"`, powerline mode is
/// active: each module gets a background color (from `styles` or built-in
/// defaults) and Nerd Fonts transition glyphs are rendered between modules.
/// Otherwise `separator` is a literal string inserted between modules and
/// `styles` entries override the foreground color of each module.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    ctx: &Ctx,
    left: &[String],
    right: &[String],
    separator: &str,
    styles: &HashMap<String, String>,
) {
    let left_segs = build_group(left, ctx, separator, styles, true);
    let right_segs = build_group(right, ctx, separator, styles, false);
    frame.render_widget(
        StatusLineWidget { left: left_segs, right: right_segs },
        area,
    );
}

struct StatusLineWidget {
    left: Vec<Segment>,
    right: Vec<Segment>,
}

impl ratatui::widgets::Widget for StatusLineWidget {
    fn render(self, area: Rect, buf: &mut RatBuffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let y = area.top();
        let base = base_style();

        // Fill the whole bar with the background style first.
        for col in area.left()..area.right() {
            buf[(col, y)].set_char(' ').set_style(base);
        }

        // Left group, from the left edge.
        let mut x = area.left();
        for seg in &self.left {
            for c in seg.text.chars() {
                if x >= area.right() {
                    break;
                }
                let w = c.width().unwrap_or(0) as u16;
                if w == 0 {
                    continue;
                }
                buf[(x, y)].set_char(c).set_style(seg.style);
                x += w;
            }
        }

        // Right group, flush to the right edge (skip if it would overlap left).
        let rwidth = group_width(&self.right);
        if rwidth > 0 && area.width >= rwidth {
            let start = area.right() - rwidth;
            // Don't paint over the left group.
            let start = start.max(x);
            let mut rx = start;
            for seg in &self.right {
                for c in seg.text.chars() {
                    if rx >= area.right() {
                        break;
                    }
                    let w = c.width().unwrap_or(0) as u16;
                    if w == 0 {
                        continue;
                    }
                    buf[(rx, y)].set_char(c).set_style(seg.style);
                    rx += w;
                }
            }
        }
    }
}
