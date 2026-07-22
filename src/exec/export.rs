//! Quarto export: render the current notebook (or markdown document) to
//! PDF / HTML / docx / … via `quarto render`, run in a background thread so
//! a long render (LaTeX!) never blocks the editor.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::app::App;

/// Handle for an in-flight `quarto render`, polled once per frame by the run
/// loop (see [`poll_export`]).
pub struct ExportJob {
    rx: Receiver<Result<String, String>>,
}

/// Kick off `quarto render <current document> --to <fmt>` in the background.
/// The document is saved first — quarto reads from disk.
pub(super) fn start_export(app: &mut App, fmt: &str) {
    if app.export_pending.is_some() {
        app.messages.show("An export is already running");
        return;
    }

    // Resolve the document to export: the open notebook, or a markdown buffer.
    let path: PathBuf = if let Some((nb, _)) = app.notebook.as_ref() {
        nb.path.clone()
    } else {
        match app.buffer.path.clone() {
            Some(p) if matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("md" | "markdown" | "qmd" | "ipynb")
            ) => p,
            _ => {
                app.messages.show("Export needs a notebook or markdown document");
                return;
            }
        }
    };

    // Flush unsaved changes so the export matches what's on screen.
    if app.notebook.is_some() {
        super::notebook::save_focused_cell(app);
        let dirty = matches!(app.notebook.as_ref(), Some((nb, _)) if nb.modified);
        if dirty {
            if let Err(e) = super::notebook::save_notebook(app) {
                app.messages.show(format!("Export aborted — save failed: {e}"));
                return;
            }
        }
    } else if app.buffer.modified {
        if let Err(e) = app.buffer.save(None, false) {
            app.messages.show(format!("Export aborted — save failed: {e}"));
            return;
        }
    }

    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("document").to_string();
    let fmt_owned = fmt.to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_quarto(&path, &fmt_owned));
    });
    app.export_pending = Some(ExportJob { rx });
    app.messages.show(format!("Exporting {name} → {fmt} via quarto…"));
}

/// Blocking quarto invocation (runs on the export thread). On success returns
/// the artifact path quarto reported, or an empty string if it didn't say.
fn run_quarto(path: &Path, fmt: &str) -> Result<String, String> {
    let out = std::process::Command::new("quarto")
        .arg("render")
        .arg(path)
        .arg("--to")
        .arg(fmt)
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("quarto not found on PATH (install from quarto.org)".into());
        }
        Err(e) => return Err(format!("could not run quarto: {e}")),
    };
    // Quarto logs progress to stderr and names the artifact in an
    // "Output created: …" line.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        let created = stdout
            .lines()
            .chain(stderr.lines())
            .filter_map(|l| l.trim().strip_prefix("Output created:"))
            .next_back()
            .map(|s| s.trim().to_owned());
        Ok(created.unwrap_or_default())
    } else {
        // Surface the tail of stderr — quarto's actual error is at the end.
        let tail: Vec<&str> = stderr
            .lines()
            .rev()
            .filter(|l| !l.trim().is_empty())
            .take(3)
            .collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        if tail.is_empty() {
            Err(format!("quarto exited with {}", out.status))
        } else {
            Err(tail.join(" · "))
        }
    }
}

/// Apply a finished export, if one is ready. Returns true when state changed
/// (the caller should redraw).
pub fn poll_export(app: &mut App) -> bool {
    let Some(job) = &app.export_pending else { return false };
    let result = match job.rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => Err("export worker died".to_string()),
    };
    app.export_pending = None;
    match result {
        Ok(artifact) if !artifact.is_empty() => {
            app.messages.show(format!("Export complete: {artifact}"));
        }
        Ok(_) => app.messages.show("Export complete"),
        Err(e) => app.messages.show(format!("Export failed: {e}")),
    }
    true
}
