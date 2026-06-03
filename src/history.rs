//! Command palette recency history.
//!
//! Records the canonical names of commands invoked via the palette or the `:`
//! command line (never keystroke-bound motions) so the palette can float
//! recently-used commands toward the top.  Depending on `ui.command_history`
//! the list is kept in memory only (`session`), persisted to the state dir
//! (`global`), or disabled entirely (`off`).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use crate::app::App;
use crate::config::CommandHistoryMode;

/// Maximum number of distinct commands remembered.
const MAX_HISTORY: usize = 100;
const FILE: &str = "command_history.json";

/// Load the persisted history (only in `global` mode; empty otherwise).
pub fn load(mode: CommandHistoryMode) -> VecDeque<String> {
    if mode != CommandHistoryMode::Global {
        return VecDeque::new();
    }
    let Some(path) = file_path() else {
        return VecDeque::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return VecDeque::new();
    };
    serde_json::from_str::<Vec<String>>(&raw)
        .map(|v| v.into_iter().take(MAX_HISTORY).collect())
        .unwrap_or_default()
}

/// Record a command invocation, moving it to the front (most-recent) position.
/// No-op in `off` mode; persists to disk in `global` mode.
pub fn record(app: &mut App, name: &str) {
    if app.command_history_mode == CommandHistoryMode::Off {
        return;
    }
    app.command_history.retain(|c| c != name);
    app.command_history.push_front(name.to_string());
    app.command_history.truncate(MAX_HISTORY);
    if app.command_history_mode == CommandHistoryMode::Global {
        save(&app.command_history);
    }
}

/// Build a `name → rank` map (0 = most recent) for the palette's recency sort.
/// Returns an empty map in `off` mode so scoring falls back to alphabetical.
pub fn recency_map(app: &App) -> HashMap<String, usize> {
    if app.command_history_mode == CommandHistoryMode::Off {
        return HashMap::new();
    }
    app.command_history
        .iter()
        .enumerate()
        .map(|(rank, name)| (name.clone(), rank))
        .collect()
}

fn save(hist: &VecDeque<String>) {
    let Some(path) = file_path() else {
        return;
    };
    let v: Vec<&String> = hist.iter().collect();
    if let Ok(json) = serde_json::to_string(&v) {
        let _ = std::fs::write(path, json);
    }
}

fn file_path() -> Option<PathBuf> {
    crate::config::state_dir().map(|d| d.join(FILE))
}
