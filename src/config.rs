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
    pub keys: KeysConfig,
    /// Language server definitions, keyed by language id (e.g. "python", "rust").
    #[serde(default)]
    pub language_servers: HashMap<String, LanguageServerConfig>,
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
    ///   MJ_PICKER_FILE  — path to a temp file; write the chosen path there
    ///                     (alternative to stdout, preferred for TUI pickers)
    ///   MJ_CURRENT_DIR  — directory of the currently open file
    ///
    /// If unset, the built-in fuzzy file list is used instead.
    #[serde(default)]
    pub file_picker: Option<String>,
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
    /// If absent, majorana auto-detects sensible defaults (e.g. venv for Python).
    #[serde(default)]
    pub init_options: Option<serde_json::Value>,
    /// Which LSP features this server provides.
    /// Empty (default) means all features. Non-empty restricts this server to only
    /// the listed features; another server with empty features handles the rest.
    ///
    /// Known feature names: "completion", "hover", "definition", "references",
    /// "type-definition", "implementation", "code-actions", "diagnostics".
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
    /// Load config from `~/.config/majorana/config.toml`.
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

/// Return the path to the user config file.
///
/// Search order:
/// 1. `$XDG_CONFIG_HOME/majorana/config.toml`
/// 2. `~/.config/majorana/config.toml`  (preferred on all platforms)
/// 3. `dirs::config_dir()/majorana/config.toml`  (macOS: ~/Library/Application Support)
fn config_path() -> Option<PathBuf> {
    // Explicit XDG override wins.
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("majorana").join("config.toml"));
    }
    // ~/.config is the standard location for CLI tools on all platforms.
    if let Some(home) = dirs::home_dir() {
        let xdg_default = home.join(".config").join("majorana").join("config.toml");
        if xdg_default.exists() {
            return Some(xdg_default);
        }
    }
    // Fallback to the platform-native location (~/Library/Application Support on macOS).
    dirs::config_dir().map(|d| d.join("majorana").join("config.toml"))
}
