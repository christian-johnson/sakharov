//! External shell formatters (`[formatters.<lang>]` config): save → run the
//! formatter on the file → reload the result.  Takes priority over LSP
//! formatting when configured.

use crate::app::App;

use super::{is_special_path, lsp, notebook, recompute_highlights, refresh_git};

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

    let in_notebook = app.notebook.is_some();

    // A notebook cell's `app.buffer.path` is a *virtual* location inside the
    // notebook's own directory (`notebook::cell_virtual_path`) — nothing on
    // disk actually lives there. Writing it for real (as the plain-file path
    // below does) would litter the project directory with a stray
    // `{notebook}__cellN.{ext}` file on every format. Format a real scratch
    // file instead (same extension, so extension-sniffing formatters still
    // pick the right language) and discard it once formatting is done.
    let disk_path = if in_notebook {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("sakharov-fmt-{}.{ext}", std::process::id()));
        if let Err(e) = std::fs::write(&tmp, app.buffer.rope.to_string()) {
            app.messages.show(format!("Could not write temp file for formatting: {e}"));
            return true;
        }
        tmp
    } else {
        // Save current buffer content to disk first so the formatter sees it.
        if let Err(e) = app.buffer.save(None, false) {
            app.messages.show(format!("Could not save before formatting: {e}"));
            return true;
        }
        path.clone()
    };

    let result = std::process::Command::new(&fmt.command)
        .args(&fmt.args)
        .arg(&disk_path)
        .output();

    match result {
        Ok(out) if out.status.success() => {
            // Reload the formatter's output back into the buffer.
            match std::fs::read_to_string(&disk_path) {
                Ok(content) => {
                    app.buffer.rope = ropey::Rope::from_str(&content);
                    if in_notebook {
                        // No real file to re-stat; the notebook itself is still
                        // unsaved and its own dirty tracking covers this cell.
                        app.buffer.modified = true;
                        notebook::save_focused_cell(app);
                    } else {
                        app.buffer.modified = false;
                        // The formatter rewrote the file; re-stat so the next save's
                        // external-modification check doesn't false-positive.
                        app.buffer.refresh_disk_mtime();
                    }
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

    if in_notebook {
        let _ = std::fs::remove_file(&disk_path);
    }

    true
}