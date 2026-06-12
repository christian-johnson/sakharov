//! Theming: every color the renderers use, resolved from a named theme.
//!
//! There are three layers:
//!
//!   1. [`ThemeSpec`] — the human-readable TOML schema (`[palette]`, `[ui]`,
//!      `[syntax]`, `[markdown]`, `[modes]`, `[notebook]`).  Every key is
//!      optional; a theme only needs to define what it cares about.
//!   2. [`Theme`] — the fully resolved runtime palette: concrete
//!      [`Color`]s for every UI element and a `Style` per highlight index.
//!      Anything the spec leaves unset is *derived*: from related spec keys
//!      (`number` falls back to `constant`), or — when the theme defines a
//!      `ui.background` — blended from background/foreground, or finally from
//!      the built-in terminal-ANSI defaults (the `"default"` theme, which is
//!      exactly the editor's classic terminal-inherited look).
//!   3. The process-wide active theme (`set_active` / [`active`]) that all
//!      renderers read.  Set at startup from `[theme] name` in the config,
//!      switched at runtime by `:theme` / the theme picker.
//!
//! Built-in themes live as TOML files in `config/themes/` (embedded in the
//! binary); user themes are `*.toml` files in `~/.config/sakharov/themes/`.
//! A user theme with the same name as a built-in shadows it.  The `[theme]`
//! section of `config.toml` is deep-merged over the chosen theme, so any
//! individual color can be overridden without editing the theme file.

use crate::mode::Mode;
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

// ---------------------------------------------------------------------------
// ThemeSpec — the TOML schema
// ---------------------------------------------------------------------------

/// A theme as written in TOML.  All keys optional; see `config/themes/example.toml`
/// for the fully commented reference.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThemeSpec {
    /// Display name (shown in the theme picker detail column).
    #[serde(default)]
    pub name: Option<String>,
    /// Reusable named colors.  Any color value elsewhere in the theme may
    /// reference a palette key by name.
    #[serde(default)]
    pub palette: HashMap<String, String>,
    #[serde(default)]
    pub ui: UiSpec,
    /// Per-mode chip / cursor colors — same shape as `[theme.modes]` in the
    /// main config (which is merged over this).
    #[serde(default)]
    pub modes: crate::config::ModeColorsConfig,
    #[serde(default)]
    pub syntax: SyntaxSpec,
    #[serde(default)]
    pub markdown: MarkdownSpec,
    #[serde(default)]
    pub notebook: NotebookSpec,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UiSpec {
    /// Editor background. Omit (or "none") to use the terminal's own background.
    /// Setting this is what switches the resolver into "themed" mode, where all
    /// unset chrome colors are derived from background/foreground blends.
    #[serde(default)]
    pub background: Option<String>,
    /// Default text color. Omit to use the terminal's foreground.
    #[serde(default)]
    pub foreground: Option<String>,
    /// De-emphasized text (hints, separators, fold counts, quotes).
    #[serde(default)]
    pub dim: Option<String>,
    #[serde(default)]
    pub line_numbers: Option<String>,
    /// Selection background.
    #[serde(default)]
    pub selection: Option<String>,
    /// Text color over a selection. Omit to keep the theme foreground.
    #[serde(default)]
    pub selection_text: Option<String>,
    /// Fold arrows / badges and other small accents.
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub info: Option<String>,
    #[serde(default)]
    pub success: Option<String>,
    /// Git gutter `+` mark. Falls back to `success`.
    #[serde(default)]
    pub git_added: Option<String>,
    /// Git gutter `~` mark. Falls back to `warning`.
    #[serde(default)]
    pub git_modified: Option<String>,
    /// Status line background / text.
    #[serde(default)]
    pub statusline: Option<String>,
    #[serde(default)]
    pub statusline_text: Option<String>,
    /// Popup (pickers, completion, docs) background / text / border.
    #[serde(default)]
    pub popup: Option<String>,
    #[serde(default)]
    pub popup_text: Option<String>,
    #[serde(default)]
    pub popup_border: Option<String>,
    /// Border of a *focused* completion popup. Falls back to `info`.
    #[serde(default)]
    pub popup_border_focus: Option<String>,
    /// Selected-row background in pickers/completion.
    #[serde(default)]
    pub popup_selection: Option<String>,
    /// Highlight for the characters that matched the picker filter.
    #[serde(default, alias = "match")]
    pub popup_match: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SyntaxSpec {
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub function: Option<String>,
    #[serde(default)]
    pub string: Option<String>,
    #[serde(default, rename = "type")]
    pub type_: Option<String>,
    #[serde(default)]
    pub constant: Option<String>,
    /// Falls back to `constant`.
    #[serde(default)]
    pub number: Option<String>,
    /// Falls back to the theme foreground.
    #[serde(default)]
    pub variable: Option<String>,
    /// Falls back to `variable`.
    #[serde(default)]
    pub property: Option<String>,
    /// Function parameters. Falls back to `variable`.
    #[serde(default)]
    pub parameter: Option<String>,
    /// Falls back to `punctuation`.
    #[serde(default)]
    pub operator: Option<String>,
    #[serde(default)]
    pub punctuation: Option<String>,
    /// Module / namespace paths. Falls back to `type`.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Attributes / decorators / annotations. Falls back to `constant`.
    #[serde(default)]
    pub attribute: Option<String>,
    /// Markup tags (HTML). Falls back to `keyword`.
    #[serde(default)]
    pub tag: Option<String>,
    /// Falls back to `keyword`.
    #[serde(default)]
    pub label: Option<String>,
    /// Falls back to `type`.
    #[serde(default)]
    pub constructor: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MarkdownSpec {
    #[serde(default, alias = "h1")]
    pub heading1: Option<String>,
    #[serde(default, alias = "h2")]
    pub heading2: Option<String>,
    #[serde(default, alias = "h3")]
    pub heading3: Option<String>,
    #[serde(default, alias = "h4")]
    pub heading4: Option<String>,
    #[serde(default, alias = "h5")]
    pub heading5: Option<String>,
    #[serde(default, alias = "h6")]
    pub heading6: Option<String>,
    #[serde(default)]
    pub bold: Option<String>,
    #[serde(default)]
    pub italic: Option<String>,
    /// Inline / fenced code. Falls back to `syntax.string`.
    #[serde(default, alias = "raw")]
    pub code: Option<String>,
    /// Falls back to `syntax.function`.
    #[serde(default)]
    pub link: Option<String>,
    /// Falls back to `ui.dim`.
    #[serde(default)]
    pub quote: Option<String>,
    /// List bullets / markers. Falls back to `syntax.constant`.
    #[serde(default)]
    pub list: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotebookSpec {
    /// Cell interior background.
    #[serde(default)]
    pub cell_background: Option<String>,
    /// Output area background (slightly recessed relative to the cell).
    #[serde(default)]
    pub output_background: Option<String>,
    /// Selection background inside cells. Falls back to `ui.selection`.
    #[serde(default)]
    pub cell_selection: Option<String>,
    /// Border of a not-yet-run cell.
    #[serde(default)]
    pub border: Option<String>,
    /// Border of the currently executing cell. Falls back to `ui.info`.
    #[serde(default)]
    pub border_running: Option<String>,
    /// Border of a successfully run cell. Falls back to `ui.success`.
    #[serde(default)]
    pub border_ok: Option<String>,
    /// Border of a failed cell. Falls back to `ui.error`.
    #[serde(default)]
    pub border_error: Option<String>,
}

// ---------------------------------------------------------------------------
// Theme — the resolved runtime palette
// ---------------------------------------------------------------------------

/// Per-mode chip / cursor colors, resolved.
#[derive(Debug, Clone, Copy)]
pub struct ModeColors {
    pub normal: Color,
    pub insert: Color,
    pub select: Color,
    pub command: Color,
    pub goto: Color,
    pub jump: Color,
    pub fold: Color,
}

/// Every color the renderers need, fully resolved.  Obtained via [`active`].
#[derive(Debug, Clone)]
pub struct Theme {
    /// Theme display name (for messages / the picker).
    pub name: String,
    /// Editor background; `None` = terminal default (don't paint).
    pub background: Option<Color>,
    /// Default text color; `None` = terminal default.
    pub foreground: Option<Color>,
    pub dim: Color,
    pub line_numbers: Color,
    pub selection_bg: Color,
    pub selection_fg: Option<Color>,
    pub accent: Color,
    pub error: Color,
    pub warning: Color,
    pub info: Color,
    pub success: Color,
    pub git_added: Color,
    pub git_modified: Color,
    pub statusline_bg: Color,
    pub statusline_fg: Color,
    pub statusline_dim: Color,
    pub popup_bg: Color,
    pub popup_fg: Color,
    pub popup_border: Color,
    pub popup_border_focus: Color,
    pub popup_selection_bg: Color,
    pub popup_match: Color,
    pub cell_bg: Color,
    pub cell_selection_bg: Color,
    pub output_bg: Color,
    pub nb_border: Color,
    pub nb_border_running: Color,
    pub nb_border_ok: Color,
    pub nb_border_error: Color,
    pub modes: ModeColors,
    /// Style per highlight index (see `highlight::HIGHLIGHT_NAMES` + `MD_*`).
    syntax: Vec<Style>,
}

impl Theme {
    /// The classic terminal-inherited look (theme name "default").
    pub fn terminal_default() -> Self {
        resolve(&ThemeSpec::default(), "default")
    }

    /// Foreground with a concrete fallback, for sites that previously used
    /// `Color::White` for emphasized text.
    pub fn fg(&self) -> Color {
        self.foreground.unwrap_or(Color::White)
    }

    /// Style for the highlight span index (syntax + markdown markup).
    pub fn syntax_style(&self, index: usize) -> Style {
        self.syntax.get(index).copied().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Active theme (process-wide)
// ---------------------------------------------------------------------------

static ACTIVE: RwLock<Option<Arc<Theme>>> = RwLock::new(None);

/// The currently active theme.  Cheap (one `Arc` clone); renderers grab it
/// once per render pass.
pub fn active() -> Arc<Theme> {
    if let Ok(guard) = ACTIVE.read() {
        if let Some(t) = guard.as_ref() {
            return t.clone();
        }
    }
    static FALLBACK: OnceLock<Arc<Theme>> = OnceLock::new();
    FALLBACK
        .get_or_init(|| Arc::new(Theme::terminal_default()))
        .clone()
}

/// Install a new active theme (startup, `:theme`, config reload).
pub fn set_active(theme: Theme) {
    if let Ok(mut guard) = ACTIVE.write() {
        *guard = Some(Arc::new(theme));
    }
}

// ---------------------------------------------------------------------------
// Color parsing
// ---------------------------------------------------------------------------

/// Parse a color value: `#rrggbb` hex, an ANSI color name (`"blue"`,
/// `"light-magenta"`, …, which tracks the terminal's palette), a `[palette]`
/// reference, or `"none"`/`""` for unset.
fn parse_color(s: &str, palette: &HashMap<String, String>) -> Option<Color> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        return None;
    }
    if let Some(c) = crate::config::parse_hex_color(s) {
        return Some(c);
    }
    // Palette reference (no recursion: palette values must be hex/ANSI).
    if let Some(v) = palette.get(s) {
        if let Some(c) = crate::config::parse_hex_color(v) {
            return Some(c);
        }
        return ansi_color_name(v);
    }
    ansi_color_name(s)
}

/// Map an ANSI color name to a [`Color`] (terminal-palette colors).
fn ansi_color_name(s: &str) -> Option<Color> {
    let key: String = s
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    Some(match key.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        _ => return None,
    })
}

/// Linear blend between two RGB colors: `t = 0` → all `a`, `t = 1` → all `b`.
/// Non-RGB inputs can't be blended; returns `a` unchanged in that case.
fn blend(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let mix = |x: u8, y: u8| -> u8 {
                (x as f32 * (1.0 - t) + y as f32 * t).round().clamp(0.0, 255.0) as u8
            };
            Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
        }
        _ => a,
    }
}

/// Pick a contrasting foreground (black or white) for a given background.
pub fn contrast_fg(bg: Color) -> Color {
    let lum: f32 = match bg {
        Color::Rgb(r, g, b) => r as f32 * 0.299 + g as f32 * 0.587 + b as f32 * 0.114,
        Color::White | Color::Yellow | Color::LightYellow
        | Color::Green | Color::LightGreen | Color::Cyan | Color::LightCyan => 180.0,
        _ => 0.0,
    };
    if lum > 128.0 { Color::Black } else { Color::White }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolve a [`ThemeSpec`] into a concrete [`Theme`].
///
/// Derivation rules when a key is unset:
///  * syntax keys fall back along documented chains (`number` → `constant` →
///    built-in ANSI default), so a theme can define ~8 colors and still cover
///    all 37 highlight indices;
///  * when `ui.background` is set ("themed" mode), chrome colors (statusline,
///    popups, selection, line numbers, notebook cells) are blended from
///    background/foreground so even a minimal theme looks coherent;
///  * without `ui.background`, everything falls back to the classic
///    terminal-ANSI defaults — the `"default"` theme is exactly this.
pub fn resolve(spec: &ThemeSpec, fallback_name: &str) -> Theme {
    let pal = &spec.palette;
    let c = |s: &Option<String>| -> Option<Color> {
        s.as_deref().and_then(|v| parse_color(v, pal))
    };
    // First-set wins; used for the documented fallback chains.
    let pick = |candidates: &[Option<Color>], default: Color| -> Color {
        candidates.iter().flatten().next().copied().unwrap_or(default)
    };

    let background = c(&spec.ui.background);
    let foreground = c(&spec.ui.foreground);
    let themed = background.is_some();
    // Blend anchor points; only meaningful in themed mode.
    let bg = background.unwrap_or(Color::Rgb(20, 20, 28));
    let fg = foreground.unwrap_or(Color::Rgb(208, 208, 208));
    // Blend toward fg by `t` when themed, else use the classic default.
    let shade = |t: f32, classic: Color| -> Color {
        if themed { blend(bg, fg, t) } else { classic }
    };

    let comment = c(&spec.syntax.comment);
    let dim = pick(&[c(&spec.ui.dim), comment], shade(0.45, Color::DarkGray));
    let line_numbers = pick(&[c(&spec.ui.line_numbers)], shade(0.30, Color::DarkGray));

    let error = pick(&[c(&spec.ui.error)], Color::Red);
    let warning = pick(&[c(&spec.ui.warning)], Color::Yellow);
    let info = pick(&[c(&spec.ui.info)], Color::Cyan);
    let success = pick(&[c(&spec.ui.success)], Color::Green);
    let accent = pick(&[c(&spec.ui.accent)], Color::Rgb(255, 160, 50));

    let selection_bg = pick(&[c(&spec.ui.selection)], shade(0.25, Color::Blue));
    // Themed selections keep the theme foreground (bg-only highlight); the
    // classic ANSI look paints black-on-blue.
    let selection_fg = c(&spec.ui.selection_text)
        .or(if themed { None } else { Some(Color::Black) });

    let statusline_bg = pick(&[c(&spec.ui.statusline)], shade(0.13, Color::DarkGray));
    let statusline_fg = pick(
        &[c(&spec.ui.statusline_text), foreground],
        if themed { fg } else { Color::White },
    );
    let statusline_dim = if themed {
        blend(statusline_fg, statusline_bg, 0.35)
    } else {
        Color::Rgb(170, 170, 170)
    };

    let popup_bg = pick(&[c(&spec.ui.popup)], shade(0.07, Color::Rgb(28, 28, 40)));
    let popup_fg = pick(
        &[c(&spec.ui.popup_text), foreground],
        if themed { fg } else { Color::Rgb(200, 200, 200) },
    );
    let popup_border = pick(
        &[c(&spec.ui.popup_border)],
        shade(0.35, Color::Rgb(100, 100, 180)),
    );
    let popup_border_focus = pick(
        &[c(&spec.ui.popup_border_focus)],
        if themed { info } else { Color::Rgb(80, 180, 255) },
    );
    let popup_selection_bg = pick(
        &[c(&spec.ui.popup_selection)],
        if themed { blend(popup_bg, fg, 0.18) } else { Color::Rgb(60, 60, 100) },
    );
    let popup_match = pick(
        &[c(&spec.ui.popup_match)],
        if themed { accent } else { Color::Rgb(255, 175, 0) },
    );

    let git_added = pick(&[c(&spec.ui.git_added)], success);
    let git_modified = pick(&[c(&spec.ui.git_modified)], warning);

    let cell_bg = pick(
        &[c(&spec.notebook.cell_background)],
        shade(0.045, Color::Rgb(20, 20, 30)),
    );
    let output_bg = pick(
        &[c(&spec.notebook.output_background)],
        if themed { bg } else { Color::Rgb(10, 10, 20) },
    );
    let cell_selection_bg = pick(
        &[c(&spec.notebook.cell_selection)],
        if themed { selection_bg } else { Color::Rgb(60, 80, 120) },
    );
    let nb_border = pick(
        &[c(&spec.notebook.border)],
        if themed { line_numbers } else { Color::Blue },
    );
    let nb_border_running = pick(
        &[c(&spec.notebook.border_running)],
        if themed { info } else { Color::LightBlue },
    );
    let nb_border_ok = pick(&[c(&spec.notebook.border_ok)], success);
    let nb_border_error = pick(&[c(&spec.notebook.border_error)], error);

    // --- Syntax palette ---
    let keyword = c(&spec.syntax.keyword);
    let function = c(&spec.syntax.function);
    let string = c(&spec.syntax.string);
    let type_ = c(&spec.syntax.type_);
    let constant = c(&spec.syntax.constant);
    let variable = c(&spec.syntax.variable);
    let punctuation = c(&spec.syntax.punctuation);

    let s = |color: Color| Style::default().fg(color);
    let mut syntax = vec![Style::default(); crate::highlight::HIGHLIGHT_NAMES.len()];
    // Indices follow highlight::HIGHLIGHT_NAMES order.
    syntax[0] = s(pick(&[c(&spec.syntax.attribute), constant], Color::Yellow)); // attribute
    syntax[1] = s(pick(&[comment], Color::DarkGray)); // comment
    syntax[2] = s(pick(&[constant], Color::Yellow)); // constant
    syntax[3] = s(pick(&[constant], Color::Yellow)); // constant.builtin
    syntax[4] = s(pick(&[c(&spec.syntax.constructor), type_], Color::Cyan)); // constructor
    syntax[5] = s(pick(&[function], Color::Blue)); // function
    syntax[6] = s(pick(&[function], Color::Cyan)); // function.builtin
    syntax[7] = s(pick(&[function], Color::Blue)); // function.method
    syntax[8] = s(pick(&[keyword], Color::Magenta)).add_modifier(Modifier::BOLD); // keyword
    syntax[9] = s(pick(&[c(&spec.syntax.label), keyword], Color::White)); // label
    syntax[10] = s(pick(&[c(&spec.syntax.namespace), type_], Color::Cyan)); // namespace
    syntax[11] = s(pick(&[c(&spec.syntax.number), constant], Color::Yellow)); // number
    syntax[12] = s(pick(&[c(&spec.syntax.operator), punctuation, foreground], Color::White)); // operator
    syntax[13] = s(pick(&[c(&spec.syntax.property), variable, foreground], Color::White)); // property
    syntax[14] = s(pick(&[punctuation, foreground], Color::Gray)); // punctuation
    syntax[15] = s(pick(&[punctuation, foreground], Color::Gray)); // punctuation.bracket
    syntax[16] = s(pick(&[punctuation, foreground], Color::Gray)); // punctuation.delimiter
    syntax[17] = s(pick(&[string], Color::Green)); // string
    syntax[18] = s(pick(&[string], Color::Green)); // string.special
    syntax[19] = s(pick(&[c(&spec.syntax.tag), keyword], Color::Red)); // tag
    syntax[20] = s(pick(&[type_], Color::Cyan)); // type
    syntax[21] = s(pick(&[type_], Color::Cyan)); // type.builtin
    syntax[22] = s(pick(&[variable, foreground], Color::White)); // variable
    syntax[23] = s(pick(&[keyword], Color::Red)); // variable.builtin
    syntax[24] = s(pick(&[c(&spec.syntax.parameter), variable, foreground], Color::White)); // variable.parameter

    // --- Markdown markup ---
    let md = &spec.markdown;
    let bold = Modifier::BOLD;
    syntax[crate::highlight::MD_HEADING_1] =
        s(pick(&[c(&md.heading1), keyword], Color::LightMagenta)).add_modifier(bold);
    syntax[crate::highlight::MD_HEADING_2] =
        s(pick(&[c(&md.heading2), function], Color::LightBlue)).add_modifier(bold);
    syntax[crate::highlight::MD_HEADING_3] =
        s(pick(&[c(&md.heading3), type_], Color::LightCyan)).add_modifier(bold);
    syntax[crate::highlight::MD_HEADING_4] =
        s(pick(&[c(&md.heading4), string], Color::LightGreen)).add_modifier(bold);
    syntax[crate::highlight::MD_HEADING_5] =
        s(pick(&[c(&md.heading5)], warning)).add_modifier(bold);
    syntax[crate::highlight::MD_HEADING_6] =
        s(pick(&[c(&md.heading6)], if themed { dim } else { Color::Gray })).add_modifier(bold);
    syntax[crate::highlight::MD_BOLD] =
        s(pick(&[c(&md.bold), foreground], Color::White)).add_modifier(bold);
    syntax[crate::highlight::MD_ITALIC] = match c(&md.italic) {
        Some(color) => s(color).add_modifier(Modifier::ITALIC),
        None => Style::default().add_modifier(Modifier::ITALIC),
    };
    syntax[crate::highlight::MD_RAW] = s(pick(&[c(&md.code), string], Color::Green));
    syntax[crate::highlight::MD_LINK] =
        s(pick(&[c(&md.link), function], Color::Blue)).add_modifier(Modifier::UNDERLINED);
    syntax[crate::highlight::MD_QUOTE] =
        s(pick(&[c(&md.quote)], dim)).add_modifier(Modifier::ITALIC);
    syntax[crate::highlight::MD_LIST] = s(pick(&[c(&md.list), constant], Color::Yellow));

    // --- Mode colors ---
    let m = &spec.modes;
    let mc = |v: &str| -> Option<Color> {
        if v.is_empty() { None } else { parse_color(v, pal) }
    };
    let modes = ModeColors {
        normal: pick(&[mc(&m.normal), if themed { function } else { None }], Color::Blue),
        insert: pick(&[mc(&m.insert), if themed { Some(success) } else { None }], Color::Green),
        select: pick(&[mc(&m.select), if themed { Some(warning) } else { None }], Color::Yellow),
        command: pick(&[mc(&m.command), if themed { Some(info) } else { None }], Color::Cyan),
        goto: pick(&[mc(&m.goto), if themed { keyword } else { None }], Color::Magenta),
        jump: pick(&[mc(&m.jump), if themed { Some(accent) } else { None }], Color::Rgb(255, 160, 0)),
        fold: pick(&[mc(&m.fold), if themed { Some(accent) } else { None }], Color::Rgb(255, 160, 50)),
    };

    Theme {
        name: spec.name.clone().unwrap_or_else(|| fallback_name.to_string()),
        background,
        foreground,
        dim,
        line_numbers,
        selection_bg,
        selection_fg,
        accent,
        error,
        warning,
        info,
        success,
        git_added,
        git_modified,
        statusline_bg,
        statusline_fg,
        statusline_dim,
        popup_bg,
        popup_fg,
        popup_border,
        popup_border_focus,
        popup_selection_bg,
        popup_match,
        cell_bg,
        cell_selection_bg,
        output_bg,
        nb_border,
        nb_border_running,
        nb_border_ok,
        nb_border_error,
        modes,
        syntax,
    }
}

// ---------------------------------------------------------------------------
// Theme discovery / loading
// ---------------------------------------------------------------------------

/// Built-in themes, embedded at compile time from `config/themes/`.
const BUILTIN_THEMES: &[(&str, &str)] = &[
    ("tokyonight", include_str!("../config/themes/tokyonight.toml")),
    ("tokyonight-storm", include_str!("../config/themes/tokyonight-storm.toml")),
    ("tokyonight-moon", include_str!("../config/themes/tokyonight-moon.toml")),
    ("tokyonight-day", include_str!("../config/themes/tokyonight-day.toml")),
    ("catppuccin-mocha", include_str!("../config/themes/catppuccin-mocha.toml")),
    ("catppuccin-macchiato", include_str!("../config/themes/catppuccin-macchiato.toml")),
    ("catppuccin-frappe", include_str!("../config/themes/catppuccin-frappe.toml")),
    ("catppuccin-latte", include_str!("../config/themes/catppuccin-latte.toml")),
    ("nord", include_str!("../config/themes/nord.toml")),
    ("nord-darker", include_str!("../config/themes/nord-darker.toml")),
    ("rose-pine", include_str!("../config/themes/rose-pine.toml")),
    ("rose-pine-moon", include_str!("../config/themes/rose-pine-moon.toml")),
    ("rose-pine-dawn", include_str!("../config/themes/rose-pine-dawn.toml")),
    ("dracula", include_str!("../config/themes/dracula.toml")),
    ("gruvbox", include_str!("../config/themes/gruvbox.toml")),
    ("gruvbox-light", include_str!("../config/themes/gruvbox-light.toml")),
    ("onedark", include_str!("../config/themes/onedark.toml")),
    ("solarized", include_str!("../config/themes/solarized.toml")),
    ("solarized-light", include_str!("../config/themes/solarized-light.toml")),
    ("kanagawa", include_str!("../config/themes/kanagawa.toml")),
    ("everforest", include_str!("../config/themes/everforest.toml")),
    ("monokai", include_str!("../config/themes/monokai.toml")),
];

/// The directory user themes are loaded from: `<config dir>/sakharov/themes/`.
pub fn user_themes_dir() -> Option<std::path::PathBuf> {
    Some(crate::config::config_file_path()?.parent()?.join("themes"))
}

/// One entry in the theme picker.
pub struct ThemeEntry {
    /// Name used to select the theme (`:theme <name>` / config `name = ...`).
    pub name: String,
    /// Display name from the theme file, when it has one.
    pub display: Option<String>,
    /// Where it comes from: `"built-in"` or the user theme file path.
    pub source: String,
}

/// All selectable themes: `default`, the built-ins, and any user theme files.
/// A user theme shadows a built-in with the same name.  Sorted by name,
/// `default` first.
pub fn available_themes() -> Vec<ThemeEntry> {
    let mut entries: Vec<ThemeEntry> = vec![ThemeEntry {
        name: "default".into(),
        display: Some("Terminal colors".into()),
        source: "built-in".into(),
    }];
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::from(["default".to_string()]);

    if let Some(dir) = user_themes_dir() {
        if let Ok(read) = std::fs::read_dir(&dir) {
            let mut files: Vec<_> = read.filter_map(|e| e.ok()).collect();
            files.sort_by_key(|e| e.file_name());
            for entry in files {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
                if !seen.insert(stem.to_string()) {
                    continue;
                }
                let display = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
                    .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned));
                entries.push(ThemeEntry {
                    name: stem.to_string(),
                    display,
                    source: path.to_string_lossy().into_owned(),
                });
            }
        }
    }

    for (name, text) in BUILTIN_THEMES {
        if !seen.insert((*name).to_string()) {
            continue; // shadowed by a user theme
        }
        let display = toml::from_str::<toml::Value>(text)
            .ok()
            .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned));
        entries.push(ThemeEntry {
            name: (*name).to_string(),
            display,
            source: "built-in".into(),
        });
    }

    entries[1..].sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Load the raw TOML value for a theme by name (user file wins over built-in).
fn find_theme_value(name: &str) -> Result<toml::Value, String> {
    if name == "default" {
        return Ok(toml::Value::Table(toml::map::Map::new()));
    }
    if let Some(dir) = user_themes_dir() {
        let path = dir.join(format!("{name}.toml"));
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            return toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()));
        }
    }
    for (builtin, text) in BUILTIN_THEMES {
        if *builtin == name {
            return toml::from_str(text)
                .map_err(|e| format!("BUG: built-in theme {name}: {e}"));
        }
    }
    Err(format!(
        "unknown theme {name:?} — see :theme for the list, or drop {name}.toml in {}",
        user_themes_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "the themes dir".into())
    ))
}

/// Load theme `name`, deep-merge the `[theme]` overrides from the config over
/// it, resolve, and install it as the active theme.  Returns the resolved
/// display name.
pub fn load_and_set(name: &str, overrides: &toml::map::Map<String, toml::Value>) -> Result<String, String> {
    let base = find_theme_value(name)?;
    let merged = crate::config::deep_merge(base, toml::Value::Table(overrides.clone()));
    let spec: ThemeSpec = merged
        .try_into()
        .map_err(|e| format!("theme {name:?}: {e}"))?;
    let theme = resolve(&spec, name);
    let display = theme.name.clone();
    set_active(theme);
    Ok(display)
}

/// Resolve and install the theme chosen by the config (startup / reload).
/// Any problem is reported on stderr and the default theme is used instead —
/// like config loading, theming is infallible.
pub fn init_from_config(config: &crate::config::Config) {
    let name = config.theme.name.clone();
    if let Err(e) = load_and_set(&name, &config.theme.overrides) {
        eprintln!("sv: warning: {e} — using default theme");
        // Still apply the [theme] overrides on top of the default look.
        let _ = load_and_set("default", &config.theme.overrides);
    }
}

// ---------------------------------------------------------------------------
// Renderer entry points
// ---------------------------------------------------------------------------

/// Map a highlight name index (from `HIGHLIGHT_NAMES`) to a ratatui `Style`,
/// per the active theme.
pub fn style_for_highlight(index: usize) -> Style {
    active().syntax_style(index)
}

/// Style for selected text in the plain editor.
pub fn selection_style() -> Style {
    let th = active();
    let mut style = Style::default().bg(th.selection_bg);
    if let Some(fg) = th.selection_fg {
        style = style.fg(fg);
    }
    style
}

/// Resolve the color for a given mode from the active theme.
pub fn mode_color(mode: &Mode) -> Color {
    let m = active().modes;
    match mode {
        Mode::Normal => m.normal,
        Mode::Insert => m.insert,
        Mode::Select => m.select,
        Mode::Command | Mode::Prompt { .. } => m.command,
        Mode::Goto { .. } | Mode::FindChar { .. } | Mode::Search { .. } => m.goto,
        Mode::Jump { .. } => m.jump,
        Mode::Fold => m.fold,
    }
}

/// Style for the cursor block — background is the mode color; the glyph is
/// painted in the theme background (black in the terminal-default theme) so
/// it reads as a cutout.
pub fn cursor_style(mode: &Mode) -> Style {
    let th = active();
    Style::default()
        .fg(th.background.unwrap_or(Color::Black))
        .bg(mode_color(mode))
}

/// Paint the theme background/foreground over the whole frame.  Call at the
/// top of every draw closure.  No-op for terminal-default themes.
pub fn fill_background(frame: &mut ratatui::Frame) {
    let th = active();
    let mut style = Style::default();
    if let Some(bg) = th.background {
        style = style.bg(bg);
    }
    if let Some(fg) = th.foreground {
        style = style.fg(fg);
    }
    if style != Style::default() {
        let area = frame.area();
        frame.buffer_mut().set_style(area, style);
    }
}

use std::sync::Mutex;

static COLOR_CACHE: Mutex<Option<HashMap<u8, String>>> = Mutex::new(None);

/// Convert a ratatui color to its ANSI color index.
pub fn color_to_ansi_index(color: Color) -> Option<u8> {
    match color {
        Color::Black => Some(0),
        Color::Red => Some(1),
        Color::Green => Some(2),
        Color::Yellow => Some(3),
        Color::Blue => Some(4),
        Color::Magenta => Some(5),
        Color::Cyan => Some(6),
        Color::Gray => Some(7),
        Color::DarkGray => Some(8),
        Color::LightRed => Some(9),
        Color::LightGreen => Some(10),
        Color::LightYellow => Some(11),
        Color::LightBlue => Some(12),
        Color::LightMagenta => Some(13),
        Color::LightCyan => Some(14),
        Color::White => Some(15),
        Color::Indexed(i) => Some(i),
        _ => None,
    }
}

/// Helper for non-blocking poll on stdin.
#[cfg(unix)]
fn wait_for_stdin(timeout_ms: i32) -> bool {
    let mut poll_fd = libc::pollfd {
        fd: 0, // stdin
        events: libc::POLLIN,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
    ret > 0 && (poll_fd.revents & libc::POLLIN) != 0
}

#[cfg(unix)]
fn read_all_stdin(timeout_ms: i32) -> Vec<u8> {
    use std::io::Read;
    let mut response = Vec::new();
    let mut buf = [0u8; 256];

    // Wait for the first byte to arrive
    if wait_for_stdin(timeout_ms) {
        let mut stdin = std::io::stdin();
        if let Ok(n) = stdin.read(&mut buf) {
            response.extend_from_slice(&buf[..n]);

            // Read any subsequent bytes that are immediately available
            while wait_for_stdin(5) {
                if let Ok(n) = stdin.read(&mut buf) {
                    response.extend_from_slice(&buf[..n]);
                } else {
                    break;
                }
                if response.len() > 1024 {
                    break;
                }
            }
        }
    }
    response
}

#[cfg(unix)]
fn parse_channel(hex_str: &str) -> Option<u8> {
    if hex_str.is_empty() {
        return None;
    }
    let hex_to_parse = if hex_str.len() >= 2 {
        &hex_str[..2]
    } else {
        return u8::from_str_radix(&format!("{}{}", hex_str, hex_str), 16).ok();
    };
    u8::from_str_radix(hex_to_parse, 16).ok()
}

#[cfg(unix)]
fn parse_all_responses(data: &[u8], cache: &mut HashMap<u8, String>) {
    let s = match String::from_utf8(data.to_vec()) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Split by "\x1b]" to handle multiple OSC sequences
    for part in s.split("\x1b]") {
        if part.is_empty() {
            continue;
        }

        let content = if let Some(s) = part.strip_suffix('\x07') {
            s
        } else if let Some(s) = part.strip_suffix("\x1b\\") {
            s
        } else {
            part.trim_end_matches(|c: char| c.is_control() || c == '\\')
        };

        if !content.starts_with("4;") {
            continue;
        }
        let content = &content[2..];

        let mut parts = content.split(';');
        let index_str = match parts.next() {
            Some(idx) => idx,
            None => continue,
        };
        let index: u8 = match index_str.parse() {
            Ok(idx) => idx,
            Err(_) => continue,
        };

        let rgb_part = match parts.next() {
            Some(p) => p,
            None => continue,
        };

        let rgb_val = match rgb_part.strip_prefix("rgb:") {
            Some(val) => val,
            None => continue,
        };

        let mut rgb_hex = rgb_val.split('/');
        let r_hex = match rgb_hex.next() {
            Some(h) => h,
            None => continue,
        };
        let g_hex = match rgb_hex.next() {
            Some(h) => h,
            None => continue,
        };
        let b_hex = match rgb_hex.next() {
            Some(h) => h,
            None => continue,
        };

        let r = match parse_channel(r_hex) {
            Some(val) => val,
            None => continue,
        };
        let g = match parse_channel(g_hex) {
            Some(val) => val,
            None => continue,
        };
        let b = match parse_channel(b_hex) {
            Some(val) => val,
            None => continue,
        };

        cache.insert(index, format!("#{:02x}{:02x}{:02x}", r, g, b));
    }
}

/// Query the terminal theme color palette on startup and cache the values.
pub fn initialize_color_cache() {
    #[cfg(unix)]
    {
        if let Ok(term) = std::env::var("TERM") {
            if term == "dumb" {
                return;
            }
        }

        // Flush stdin first to clear any pending user keystrokes
        unsafe {
            libc::tcflush(0, libc::TCIFLUSH);
        }

        use std::io::Write;
        let mut stdout = std::io::stdout();
        // Query green (2), yellow (3), blue (4), magenta (5), cyan (6)
        let _ = write!(
            stdout,
            "\x1b]4;2;?\x07\x1b]4;3;?\x07\x1b]4;4;?\x07\x1b]4;5;?\x07\x1b]4;6;?\x07"
        );
        let _ = stdout.flush();

        let data = read_all_stdin(50);
        if !data.is_empty() {
            let mut cache = HashMap::new();
            parse_all_responses(&data, &mut cache);
            if !cache.is_empty() {
                if let Ok(mut guard) = COLOR_CACHE.lock() {
                    *guard = Some(cache);
                }
            }
        }
    }
}

/// Convert a ratatui color to its OSC-compatible color string.
pub fn color_to_osc_spec(color: Color) -> Option<String> {
    if let Some(index) = color_to_ansi_index(color) {
        if let Ok(guard) = COLOR_CACHE.lock() {
            if let Some(ref cache) = *guard {
                if let Some(hex) = cache.get(&index) {
                    return Some(hex.clone());
                }
            }
        }
    }
    match color {
        Color::Reset => None,
        Color::Black => Some("color0".to_string()),
        Color::Red => Some("color1".to_string()),
        Color::Green => Some("color2".to_string()),
        Color::Yellow => Some("color3".to_string()),
        Color::Blue => Some("color4".to_string()),
        Color::Magenta => Some("color5".to_string()),
        Color::Cyan => Some("color6".to_string()),
        Color::Gray => Some("color7".to_string()),
        Color::DarkGray => Some("color8".to_string()),
        Color::LightRed => Some("color9".to_string()),
        Color::LightGreen => Some("color10".to_string()),
        Color::LightYellow => Some("color11".to_string()),
        Color::LightBlue => Some("color12".to_string()),
        Color::LightMagenta => Some("color13".to_string()),
        Color::LightCyan => Some("color14".to_string()),
        Color::White => Some("color15".to_string()),
        Color::Indexed(i) => Some(format!("color{}", i)),
        Color::Rgb(r, g, b) => Some(format!("#{:02x}{:02x}{:02x}", r, g, b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_channel() {
        assert_eq!(parse_channel("2626"), Some(0x26));
        assert_eq!(parse_channel("8b8b"), Some(0x8b));
        assert_eq!(parse_channel("ff"), Some(0xff));
        assert_eq!(parse_channel("f"), Some(0xff));
        assert_eq!(parse_channel(""), None);
    }

    #[test]
    fn test_parse_all_responses() {
        let mut cache = HashMap::new();
        // Test single response with BEL terminator
        let data1 = b"\x1b]4;2;rgb:5050/adad/7070\x07";
        parse_all_responses(data1, &mut cache);
        assert_eq!(cache.get(&2), Some(&"#50ad70".to_string()));

        // Test multiple responses back-to-back with ST (\x1b\\) and BEL mixed
        let data2 = b"\x1b]4;4;rgb:2626/8b8b/e2e2\x1b\\\x1b]4;6;rgb:1111/2222/3333\x07";
        parse_all_responses(data2, &mut cache);
        assert_eq!(cache.get(&4), Some(&"#268be2".to_string()));
        assert_eq!(cache.get(&6), Some(&"#112233".to_string()));
    }

    #[test]
    fn test_color_to_ansi_index() {
        assert_eq!(color_to_ansi_index(Color::Blue), Some(4));
        assert_eq!(color_to_ansi_index(Color::Green), Some(2));
        assert_eq!(color_to_ansi_index(Color::Indexed(42)), Some(42));
        assert_eq!(color_to_ansi_index(Color::Rgb(1, 2, 3)), None);
    }

    /// The "default" theme must reproduce the classic terminal-inherited look
    /// exactly — no background painting, ANSI syntax colors, black-on-blue
    /// selection.
    #[test]
    fn default_theme_matches_classic_look() {
        let th = Theme::terminal_default();
        assert_eq!(th.background, None);
        assert_eq!(th.foreground, None);
        assert_eq!(th.dim, Color::DarkGray);
        assert_eq!(th.selection_bg, Color::Blue);
        assert_eq!(th.selection_fg, Some(Color::Black));
        assert_eq!(th.statusline_bg, Color::DarkGray);
        assert_eq!(th.statusline_fg, Color::White);
        assert_eq!(th.popup_bg, Color::Rgb(28, 28, 40));
        assert_eq!(th.popup_border, Color::Rgb(100, 100, 180));
        assert_eq!(th.cell_bg, Color::Rgb(20, 20, 30));
        assert_eq!(th.cell_selection_bg, Color::Rgb(60, 80, 120));
        assert_eq!(th.nb_border, Color::Blue);
        assert_eq!(th.nb_border_running, Color::LightBlue);
        assert_eq!(th.error, Color::Red);
        assert_eq!(th.modes.normal, Color::Blue);
        assert_eq!(th.modes.jump, Color::Rgb(255, 160, 0));
        // Spot-check syntax defaults: keyword magenta+bold, comment dark gray.
        assert_eq!(th.syntax_style(8).fg, Some(Color::Magenta));
        assert!(th.syntax_style(8).add_modifier.contains(Modifier::BOLD));
        assert_eq!(th.syntax_style(1).fg, Some(Color::DarkGray));
        assert_eq!(th.syntax_style(17).fg, Some(Color::Green));
    }

    /// Every built-in theme must parse, resolve, and define a background
    /// (built-ins are full themes, not partial overlays).
    #[test]
    fn all_builtin_themes_parse_and_resolve() {
        for (name, text) in BUILTIN_THEMES {
            let val: toml::Value = toml::from_str(text)
                .unwrap_or_else(|e| panic!("theme {name}: invalid TOML: {e}"));
            let spec: ThemeSpec = val
                .try_into()
                .unwrap_or_else(|e| panic!("theme {name}: bad schema: {e}"));
            // Every palette entry and every referenced color must parse.
            for (k, v) in &spec.palette {
                assert!(
                    crate::config::parse_hex_color(v).is_some() || ansi_color_name(v).is_some(),
                    "theme {name}: palette entry {k} = {v:?} is not a valid color"
                );
            }
            let th = resolve(&spec, name);
            assert!(th.background.is_some(), "theme {name}: no ui.background");
            assert!(th.foreground.is_some(), "theme {name}: no ui.foreground");
            assert!(spec.name.is_some(), "theme {name}: no display name");
            // Syntax basics must be set explicitly (not ANSI fallbacks).
            for (idx, what) in [(1usize, "comment"), (8, "keyword"), (17, "string"), (5, "function")] {
                assert!(
                    matches!(th.syntax_style(idx).fg, Some(Color::Rgb(..))),
                    "theme {name}: syntax.{what} not set"
                );
            }
        }
    }

    /// Built-in theme names must match their registry key (file name) — the
    /// picker and `:theme` select by that key.
    #[test]
    fn builtin_theme_registry_is_consistent() {
        let names: Vec<&str> = BUILTIN_THEMES.iter().map(|(n, _)| *n).collect();
        let mut deduped = names.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), names.len(), "duplicate built-in theme name");
    }

    /// Palette references and ANSI names resolve; minimal themes derive the
    /// rest of the chrome from bg/fg blends.
    #[test]
    fn palette_refs_and_derivation() {
        let toml_text = r##"
            name = "Mini"
            [palette]
            base = "#101020"
            text = "#d0d0e0"
            love = "#ff5577"
            [ui]
            background = "base"
            foreground = "text"
            error = "love"
            [syntax]
            keyword = "light-magenta"
        "##;
        let spec: ThemeSpec = toml::from_str(toml_text).unwrap();
        let th = resolve(&spec, "mini");
        assert_eq!(th.background, Some(Color::Rgb(0x10, 0x10, 0x20)));
        assert_eq!(th.error, Color::Rgb(0xff, 0x55, 0x77));
        assert_eq!(th.syntax_style(8).fg, Some(Color::LightMagenta));
        // Derived chrome: not the classic constants, but blends of bg/fg.
        assert_ne!(th.statusline_bg, Color::DarkGray);
        assert_ne!(th.popup_bg, Color::Rgb(28, 28, 40));
        assert_eq!(th.selection_fg, None);
        // Variable falls back to the theme foreground.
        assert_eq!(th.syntax_style(22).fg, Some(Color::Rgb(0xd0, 0xd0, 0xe0)));
    }

    /// `[theme]` config overrides deep-merge over the chosen theme.
    #[test]
    fn config_overrides_merge_over_theme() {
        let mut overrides = toml::map::Map::new();
        let mut ui = toml::map::Map::new();
        ui.insert("accent".into(), toml::Value::String("#123456".into()));
        overrides.insert("ui".into(), toml::Value::Table(ui));

        let base = find_theme_value("tokyonight").unwrap();
        let merged = crate::config::deep_merge(base, toml::Value::Table(overrides));
        let spec: ThemeSpec = merged.try_into().unwrap();
        let th = resolve(&spec, "tokyonight");
        assert_eq!(th.accent, Color::Rgb(0x12, 0x34, 0x56));
        // The rest of the theme is untouched.
        assert!(th.background.is_some());
    }

    /// The commented reference theme (the template users copy) must always
    /// parse against the current schema and resolve with every key honoured.
    #[test]
    fn example_theme_stays_valid() {
        let text = include_str!("../config/themes/example.toml");
        let spec: ThemeSpec = toml::from_str(text).expect("example.toml parses");
        let th = resolve(&spec, "example");
        assert!(th.background.is_some());
        assert_eq!(th.accent, Color::Rgb(0xff, 0x9e, 0x64)); // palette "orange"
        assert_eq!(th.modes.goto, Color::Rgb(0xbb, 0x9a, 0xf7)); // palette "purple"
    }

    #[test]
    fn blend_midpoint() {
        assert_eq!(
            blend(Color::Rgb(0, 0, 0), Color::Rgb(100, 200, 50), 0.5),
            Color::Rgb(50, 100, 25)
        );
        // Non-RGB inputs pass through.
        assert_eq!(blend(Color::Blue, Color::Rgb(1, 2, 3), 0.5), Color::Blue);
    }
}
