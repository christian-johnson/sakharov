//! External shell formatters (`[formatters.<lang>]` config): save → run the
//! formatter on the file → reload the result.  Takes priority over LSP
//! formatting when configured.

use crate::app::App;

use super::{is_special_path, lsp, recompute_highlights, refresh_git};

/// Run the configured shell formatter for the current buffer's language.
///
/// Flow: save buffer → run `command args... <file>` → reload formatted content.
///
/// Returns `true` if a formatter was configured and was attempted (the caller
/// should not try anything else for this save/format cycle).
/// Returns `false` if no formatter is configured for this language (caller
/// should fall back to LSP or a plain save).
pub(super) fn run_shell_formatter(app: &mut App) -> bool {
    let path = match app.buffer.path.clone() {
        Some(p) => p,
        None => return false,
    };
    if is_special_path(&path) {
        return false;
    }
    let lang = match app.current_language() {
        Some(l) => l.to_owned(),
        None => return false,
    };
    let fmt = match app.config.formatters.get(&lang).cloned() {
        Some(f) => f,
        None => return false,
    };

    // Save current buffer content to disk first so the formatter sees it.
    if let Err(e) = app.buffer.save(None, false) {
        app.messages.show(format!("Could not save before formatting: {e}"));
        return true;
    }

    let result = std::process::Command::new(&fmt.command)
        .args(&fmt.args)
        .arg(&path)
        .output();

    match result {
        Ok(out) if out.status.success() => {
            // Reload the formatter's output back into the buffer.
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    app.buffer.rope = ropey::Rope::from_str(&content);
                    app.buffer.modified = false;
                    // The formatter rewrote the file; re-stat so the next save's
                    // external-modification check doesn't false-positive.
                    app.buffer.refresh_disk_mtime();
                    recompute_highlights(app);
                    lsp::lsp_did_change(app);
                    refresh_git(app);
                }
                Err(e) => {
                    app.messages.show(format!("Could not reload after format: {e}"));
                }
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.trim();
            app.messages.show(if msg.is_empty() {
                format!("Formatter exited with code {}", out.status.code().unwrap_or(-1))
            } else {
                msg.chars().take(200).collect()
            });
        }
        Err(e) => {
            app.messages.show(format!("Formatter '{}': {e}", fmt.command));
        }
    }
    true
}