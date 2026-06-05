use ratatui::style::Color;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Parse a `#rrggbb` hex color string into a ratatui [`Color`].
/// Returns `None` for empty strings and unrecognised formats.
pub(crate) fn parse_hex_color(s: &str) -> Option<Color> {
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

/// Top-level configuration structure.
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct Config {
    pub theme: ThemeConfig,
    pub editor: EditorConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub statusline: StatuslineConfig,
    #[serde(default)]
    pub notebook: NotebookConfig,
    #[serde(default)]
    pub keys: KeysConfig,
    /// Language server definitions, keyed by language id (e.g. "python", "rust").
    #[serde(default)]
    pub language_servers: HashMap<String, LanguageServerConfig>,
    /// Shell formatters keyed by language id.
    /// When configured, `:fmt` and `format_on_save` run this command on the file
    /// instead of (and taking priority over) LSP-based formatting.
    #[serde(default)]
    pub formatters: HashMap<String, FormatterConfig>,
}

/// Custom key bindings config.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct KeysConfig {
    #[serde(default)]
    pub normal: HashMap<String, String>,
    #[serde(default)]
    pub select: HashMap<String, String>,
}

/// Theme color configuration (hex strings).
/// Per-mode color overrides.  Each field is a `#rrggbb` hex string; an empty
/// string (the default) falls back to the built-in ANSI color for that mode.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ModeColorsConfig {
    /// Normal / navigation mode.  Default: ANSI Blue.
    #[serde(default)]
    pub normal: String,
    /// Insert (text-entry) mode.  Default: ANSI Green.
    #[serde(default)]
    pub insert: String,
    /// Visual / Select mode.  Default: ANSI Yellow.
    #[serde(default)]
    pub select: String,
    /// `:` command-line mode.  Default: ANSI Cyan.
    #[serde(default)]
    pub command: String,
    /// Notebook navigation mode.  Default: ANSI Cyan.
    #[serde(default)]
    pub notebook: String,
    /// `g` Goto / `/` Search / `f` FindChar sub-modes.  Default: ANSI Magenta.
    #[serde(default)]
    pub goto: String,
    /// `gw` jump-label mode.  Default: orange `#ffa000`.
    #[serde(default)]
    pub jump: String,
    /// `z` fold sub-mode.  Default: light orange `#ffb060`.
    #[serde(default)]
    pub fold: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct ThemeConfig {
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub selection: String,
    pub line_numbers: String,
    /// Per-mode chip / cursor colors.  Each entry is optional; unset entries
    /// use the built-in ANSI fallback for that mode.
    #[serde(default)]
    pub modes: ModeColorsConfig,
}

/// Editor behaviour configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct EditorConfig {
    pub tab_width: usize,
    /// When true (default), the Tab key and all auto-indentation insert
    /// `tab_width` spaces instead of a literal tab character — the editor never
    /// writes a `\t` of its own. Set to false to indent with real tab characters.
    #[serde(default = "default_expand_tabs")]
    pub expand_tabs: bool,
    pub line_numbers: bool,
    pub relative_line_numbers: bool,
    /// Lines to keep visible above/below cursor.
    pub scroll_off: usize,
    /// Show a 1-column git diff marker to the left of line numbers.
    #[serde(default)]
    pub git_gutter: bool,
    /// Shell command to invoke as an external file picker (e.g. yazi, fzf).
    ///
    /// The command receives two environment variables:
    ///   SV_PICKER_FILE  — path to a temp file; write the chosen path there
    ///                     (alternative to stdout, preferred for TUI pickers)
    ///   SV_CURRENT_DIR  — directory of the currently open file
    ///
    /// If unset, the built-in fuzzy file list is used instead.
    #[serde(default)]
    pub file_picker: Option<String>,
    /// Run the language server's formatter before each `:w` / `:wq` save.
    #[serde(default)]
    pub format_on_save: bool,
    /// Soft-wrap long lines to the window width instead of scrolling horizontally.
    #[serde(default)]
    pub word_wrap: bool,
    /// Maximum undo steps kept per buffer. Oldest steps are evicted when the
    /// limit is reached.
    #[serde(default = "default_max_undo")]
    pub max_undo: usize,
    /// Maximum number of files indexed by the built-in file picker (Ctrl+O
    /// without an external `file_picker` command configured).
    #[serde(default = "default_file_picker_max_files")]
    pub file_picker_max_files: usize,
    /// Maximum directory depth explored by the built-in file picker.
    #[serde(default = "default_file_picker_max_depth")]
    pub file_picker_max_depth: usize,
    /// Periodically persist unsaved buffer contents to a private recovery file
    /// (`$XDG_STATE_HOME/sakharov/recovery/`, owner-only `0600`) so they can be
    /// restored after a crash or kill.  The recovery file is deleted on a clean
    /// save and on a clean quit, so it only lingers when something went wrong.
    /// Set to false to disable recovery entirely (e.g. for sensitive trees).
    #[serde(default = "default_crash_recovery")]
    pub crash_recovery: bool,
}

fn default_expand_tabs() -> bool { true }
fn default_max_undo() -> usize { 200 }
fn default_file_picker_max_files() -> usize { 2000 }
fn default_file_picker_max_depth() -> usize { 10 }
fn default_crash_recovery() -> bool { true }

/// UI / interaction configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct UiConfig {
    /// Character alphabet used to generate 2-char jump labels (gw / `EnterJumpMode`).
    /// The first characters are preferred for the closest targets, so put your
    /// home-row keys first for best ergonomics.
    #[serde(default = "default_jump_keys")]
    pub jump_keys: String,
    /// Maximum items visible in the completion / symbol-picker popup at once.
    /// Increase if you have a tall terminal and want to see more candidates.
    #[serde(default = "default_completion_list_height")]
    pub completion_list_height: u16,
    /// Maximum lines shown in documentation / hover popups.
    #[serde(default = "default_doc_popup_height")]
    pub doc_popup_height: u16,
    /// Display label shown next to each symbol kind in the completion and
    /// symbol-picker popups.  Override any key to change its badge.
    ///
    /// Known keys: fn, class, struct, enum, trait, const, impl, method, var.
    /// Any unknown key falls back to the raw kind string.
    #[serde(default = "default_symbol_icons")]
    pub symbol_icons: HashMap<String, String>,
    /// How the command palette remembers recently-used commands, floating them
    /// toward the top:
    ///   "session" — kept in memory only, reset each launch (default).
    ///   "global"  — persisted to `$XDG_STATE_HOME/sakharov/command_history.json`
    ///               and restored across restarts.
    ///   "off"     — no recency weighting; alphabetical-within-tier as before.
    /// Recency only ever breaks ties between matches of equal fuzzy-match
    /// quality, so a better match always still wins.
    #[serde(default = "default_command_history")]
    pub command_history: String,
}

fn default_command_history() -> String { "session".into() }

/// Parsed form of `ui.command_history`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandHistoryMode {
    Off,
    Session,
    Global,
}

impl CommandHistoryMode {
    /// Parse the config string, defaulting to `Session` for unknown values.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "none" => CommandHistoryMode::Off,
            "global" | "persist" | "persistent" => CommandHistoryMode::Global,
            _ => CommandHistoryMode::Session,
        }
    }
}

fn default_jump_keys() -> String {
    "asdfghjklqwertyuiopzxcvbnm".into()
}
fn default_completion_list_height() -> u16 { 15 }
fn default_doc_popup_height() -> u16 { 18 }
fn default_symbol_icons() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("fn".into(),     "λ fn".into());
    m.insert("class".into(),  "○ class".into());
    m.insert("struct".into(), "□ struct".into());
    m.insert("enum".into(),   "◇ enum".into());
    m.insert("trait".into(),  "◈ trait".into());
    m.insert("const".into(),  "# const".into());
    m.insert("impl".into(),   "⊕ impl".into());
    m.insert("method".into(), "m mth".into());
    m.insert("var".into(),    "= var".into());
    m
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            jump_keys: default_jump_keys(),
            completion_list_height: default_completion_list_height(),
            doc_popup_height: default_doc_popup_height(),
            symbol_icons: default_symbol_icons(),
            command_history: default_command_history(),
        }
    }
}

/// Starship-style status line layout.  `left` and `right` are ordered lists of
/// module names; a name that isn't a known module is rendered as literal text
/// (handy as a custom separator).  Known modules: `mode`, `file`, `git`,
/// `diagnostics`, `position`, `scroll`, `spinner`, `cell`, `kernel`.
#[derive(Debug, Deserialize, Clone)]
pub struct StatuslineConfig {
    #[serde(default = "default_statusline_left")]
    pub left: Vec<String>,
    #[serde(default = "default_statusline_right")]
    pub right: Vec<String>,
    /// Layout used while a notebook is open (the multi-cell view).
    #[serde(default)]
    pub notebook: NotebookStatuslineConfig,
    /// String inserted between adjacent modules.  Default `""` relies on each
    /// module's own padding (a single leading/trailing space) for visual
    /// separation.  Try `">"`, `"|"`, `"/"`, or `"\\"` for a powerline-inspired
    /// look; `" | "` for a spaced pipe.
    #[serde(default)]
    pub separator: String,
    /// Per-module foreground color overrides.  Keys are module names (e.g.
    /// `"file"`, `"git"`, `"mode"`); values are `#rrggbb` hex strings.
    ///
    /// Example:
    /// ```toml
    /// [statusline.styles]
    /// file = "#50fa7b"
    /// git  = "#bd93f9"
    /// ```
    #[serde(default)]
    pub styles: HashMap<String, String>,
}

/// Status line layout for the notebook view.
#[derive(Debug, Deserialize, Clone)]
pub struct NotebookStatuslineConfig {
    #[serde(default = "default_nb_statusline_left")]
    pub left: Vec<String>,
    #[serde(default = "default_nb_statusline_right")]
    pub right: Vec<String>,
}

fn default_statusline_left() -> Vec<String> {
    vec!["mode".into(), "git".into(), "file".into()]
}
fn default_statusline_right() -> Vec<String> {
    vec!["diagnostics".into(), "spinner".into(), "position".into(), "scroll".into()]
}
fn default_nb_statusline_left() -> Vec<String> {
    vec!["mode".into(), "file".into()]
}
fn default_nb_statusline_right() -> Vec<String> {
    vec!["diagnostics".into(), "cell".into(), "kernel".into()]
}

impl Default for StatuslineConfig {
    fn default() -> Self {
        Self {
            left: default_statusline_left(),
            right: default_statusline_right(),
            notebook: NotebookStatuslineConfig::default(),
            separator: String::new(),
            styles: HashMap::new(),
        }
    }
}

impl Default for NotebookStatuslineConfig {
    fn default() -> Self {
        Self {
            left: default_nb_statusline_left(),
            right: default_nb_statusline_right(),
        }
    }
}

/// Notebook-specific configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct NotebookConfig {
    /// Terminal rows reserved for each image output block (Kitty graphics protocol).
    /// Increase if images are getting clipped; decrease to show more cells on screen.
    #[serde(default = "default_image_rows")]
    pub image_rows: u16,
    /// Maximum stdout/stderr lines shown per output block before truncation.
    #[serde(default = "default_max_output_lines")]
    pub max_output_lines: usize,
    /// Maximum Python traceback lines shown per error output.
    #[serde(default = "default_max_traceback_lines")]
    pub max_traceback_lines: usize,
}

fn default_image_rows() -> u16 { 40 }
fn default_max_output_lines() -> usize { 20 }
fn default_max_traceback_lines() -> usize { 5 }

impl Default for NotebookConfig {
    fn default() -> Self {
        Self {
            image_rows: default_image_rows(),
            max_output_lines: default_max_output_lines(),
            max_traceback_lines: default_max_traceback_lines(),
        }
    }
}

/// Configuration for a shell-based document formatter.
#[derive(Debug, Deserialize, Clone)]
pub struct FormatterConfig {
    /// The formatter executable (must be on $PATH or an absolute path).
    pub command: String,
    /// Additional arguments passed before the filename.
    /// Example: `["format"]` for `ruff format <file>`.
    #[serde(default)]
    pub args: Vec<String>,
}

/// Configuration for a single language server.
#[derive(Debug, Deserialize, Clone)]
pub struct LanguageServerConfig {
    /// The executable to run (must be on $PATH or an absolute path).
    pub command: String,
    /// Additional command-line arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Server-specific `initializationOptions` (arbitrary JSON).
    /// If absent, sakharov auto-detects sensible defaults (e.g. venv for Python).
    #[serde(default)]
    pub init_options: Option<serde_json::Value>,
    /// Which LSP features this server provides.
    /// Empty (default) means all features. Non-empty restricts this server to only
    /// the listed features; another server with empty features handles the rest.
    ///
    /// Known feature names: "completion", "hover", "definition", "references",
    /// "type-definition", "implementation", "code-actions", "diagnostics", "format".
    #[serde(default)]
    pub features: Vec<String>,
    /// Additional language servers for the same language, each with their own
    /// feature scope.  Useful for combining e.g. `pylsp` (completions, hover,
    /// goto-definition) with `ruff` (code-actions, formatting).
    ///
    /// Example config:
    /// ```toml
    /// [language_servers.python]
    /// command = "pylsp"
    ///
    /// [[language_servers.python.extra_servers]]
    /// command = "ruff"
    /// args = ["server"]
    /// features = ["code-actions"]
    /// ```
    #[serde(default)]
    pub extra_servers: Vec<ExtraServerConfig>,
}

/// Configuration for one additional server in a multiplexed setup.
#[derive(Debug, Deserialize, Clone)]
pub struct ExtraServerConfig {
    /// The executable to run.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub init_options: Option<serde_json::Value>,
    /// Feature scope — same semantics as `LanguageServerConfig::features`.
    #[serde(default)]
    pub features: Vec<String>,
}

const DEFAULT_CONFIG: &str = include_str!("../config/default.toml");

impl Config {
    /// Load config from `~/.config/sakharov/config.toml`, deep-merged over the
    /// compiled-in defaults.  **Never fails**: any problem reading or parsing
    /// the user file is reported to stderr and the built-in defaults are used
    /// instead, so the editor always starts in a known-good state.
    pub fn load() -> Self {
        // The compiled-in defaults must always be valid — treat any failure as
        // a programming error rather than a runtime error.
        let default_val: toml::Value = toml::from_str(DEFAULT_CONFIG)
            .expect("BUG: compiled-in default.toml is invalid TOML");
        let default_cfg: Self = default_val
            .clone()
            .try_into()
            .expect("BUG: compiled-in default.toml failed to deserialize");

        let path = match config_path() {
            Some(p) if p.exists() => p,
            _ => return default_cfg,
        };

        // Read the file.
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "sv: warning: cannot read config {}: {e} — using built-in defaults",
                    path.display()
                );
                return default_cfg;
            }
        };

        // Parse as TOML.  A syntax error here is the most common user mistake.
        let user_val: toml::Value = match toml::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "sv: warning: config {}: {e} — using built-in defaults",
                    path.display()
                );
                return default_cfg;
            }
        };

        // Deep-merge over defaults then deserialize.  A type mismatch (e.g.
        // `tab_width = "four"`) surfaces here.
        let merged_val = deep_merge(default_val, user_val);
        let merged_str = match toml::to_string(&merged_val) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "sv: warning: config {}: serialization error: {e} — using built-in defaults",
                    path.display()
                );
                return default_cfg;
            }
        };
        match toml::from_str(&merged_str) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!(
                    "sv: warning: config {}: {e} — using built-in defaults",
                    path.display()
                );
                default_cfg
            }
        }
    }
}

/// Deep-merge `over` into `base`.  Tables are merged recursively;
/// any other type is replaced by `over`.
fn deep_merge(base: toml::Value, over: toml::Value) -> toml::Value {
    use toml::Value::Table;
    match (base, over) {
        (Table(mut b), Table(o)) => {
            for (k, v) in o {
                let existing = b
                    .remove(&k)
                    .unwrap_or_else(|| Table(toml::map::Map::new()));
                b.insert(k, deep_merge(existing, v));
            }
            Table(b)
        }
        // Non-table values: the override wins outright.
        (_, o) => o,
    }
}

/// Return the path that the user config file lives at (or should be created at).
/// Same search order as `Config::load`, but never returns `None` when home is available.
pub fn config_file_path() -> Option<PathBuf> {
    config_path()
}

/// Return the path to the user config file.
///
/// Search order:
/// 1. `$XDG_CONFIG_HOME/sakharov/config.toml`
/// 2. `~/.config/sakharov/config.toml`  (preferred on all platforms)
/// 3. `dirs::config_dir()/sakharov/config.toml`  (macOS: ~/Library/Application Support)
fn config_path() -> Option<PathBuf> {
    // Explicit XDG override wins.
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("sakharov").join("config.toml"));
    }
    // ~/.config is the standard location for CLI tools on all platforms.
    if let Some(home) = dirs::home_dir() {
        let xdg_default = home.join(".config").join("sakharov").join("config.toml");
        if xdg_default.exists() {
            return Some(xdg_default);
        }
    }
    // Fallback to the platform-native location (~/Library/Application Support on macOS).
    dirs::config_dir().map(|d| d.join("sakharov").join("config.toml"))
}

/// Return (creating if necessary) the per-user state directory for sakharov,
/// used for non-config runtime state: crash-recovery files and the command
/// history.  Search order mirrors `config_path`:
///   1. `$XDG_STATE_HOME/sakharov`
///   2. `dirs::state_dir()/sakharov`  (Linux: ~/.local/state)
///   3. `dirs::data_dir()/sakharov`   (fallback for platforms without a state dir)
///
/// The directory is created with `0700` permissions on Unix so its contents
/// (which may include unsaved buffer text) are not readable by other users.
/// Returns `None` if no suitable base directory exists or creation fails.
pub fn state_dir() -> Option<PathBuf> {
    let base = if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(xdg)
    } else {
        dirs::state_dir().or_else(dirs::data_dir)?
    };
    let dir = base.join("sakharov");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("sv: could not create state dir {}: {e}", dir.display());
        return None;
    }
    restrict_dir_permissions(&dir);
    Some(dir)
}

/// Tighten a directory to owner-only (`0700`) on Unix.  No-op elsewhere.
#[cfg(unix)]
pub fn restrict_dir_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o700);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
pub fn restrict_dir_permissions(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compiled-in default config must always parse into `Config`, including
    /// the `[statusline]` section.
    #[test]
    fn default_config_parses() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("default.toml parses");
        assert_eq!(cfg.statusline.left, vec!["mode", "git", "file"]);
        assert!(cfg.statusline.right.contains(&"diagnostics".to_string()));
        assert_eq!(cfg.statusline.notebook.right, vec!["diagnostics", "cell", "kernel"]);
    }

    /// A partial user `[statusline]` override replaces only the keys it sets and
    /// deep-merges the rest from defaults.
    #[test]
    fn statusline_partial_override_merges() {
        let base: toml::Value = toml::from_str(DEFAULT_CONFIG).unwrap();
        let user: toml::Value = toml::from_str("[statusline]\nleft = [\"mode\"]\n").unwrap();
        let merged = deep_merge(base, user);
        let cfg: Config = merged.try_into().unwrap();
        assert_eq!(cfg.statusline.left, vec!["mode"]);
        // right + notebook untouched by the override.
        assert!(cfg.statusline.right.contains(&"position".to_string()));
        assert_eq!(cfg.statusline.notebook.left, vec!["mode", "file"]);
    }
}
