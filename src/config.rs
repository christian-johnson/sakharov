use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

/// Top-level configuration structure.
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct Config {
    pub theme: ThemeConfig,
    pub editor: EditorConfig,
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
}

const DEFAULT_CONFIG: &str = include_str!("../config/default.toml");

impl Config {
    /// Load config from `~/.config/ki/config.toml` (or `$XDG_CONFIG_HOME/ki/config.toml`).
    /// Falls back to compiled-in defaults when the file is absent or unreadable.
    pub fn load() -> Result<Self> {
        let path = config_path();
        if let Some(p) = path {
            if p.exists() {
                let text = std::fs::read_to_string(&p)?;
                return Ok(toml::from_str(&text)?);
            }
        }
        Ok(toml::from_str(DEFAULT_CONFIG)?)
    }
}

/// Return the path to the user config file, if determinable.
fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir())?;
    Some(base.join("ki").join("config.toml"))
}
