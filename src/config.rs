use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Top-level configuration structure.
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct Config {
    pub theme: ThemeConfig,
    pub editor: EditorConfig,
    #[serde(default)]
    pub ui: UiConfig,
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
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct ThemeConfig {
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub selection: String,
    pub line_numbers: String,
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
    /// Load config from `~/.config/sakharov/config.toml`.
    ///
    /// The user file is deep-merged on top of the compiled-in defaults, so a
    /// partial config (e.g. only `[language_servers]`) works without repeating
    /// all theme/editor values.
    pub fn load() -> Result<Self> {
        let mut base: toml::Value = toml::from_str(DEFAULT_CONFIG)?;

        if let Some(p) = config_path() {
            if p.exists() {
                let text = std::fs::read_to_string(&p)?;
                let user: toml::Value = toml::from_str(&text)?;
                base = deep_merge(base, user);
            }
        }

        // Serialize back to string then re-parse; this lets serde drive the
        // final struct construction cleanly without a custom Deserializer impl.
        let merged = toml::to_string(&base)?;
        Ok(toml::from_str(&merged)?)
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
