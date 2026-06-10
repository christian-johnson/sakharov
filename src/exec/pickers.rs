//! Popup pickers and search/grep front-ends.
//!
//! Each public function builds (and installs) a [`Popup`](crate::popup::Popup)
//! on `app`, or — for the external file picker — suspends the TUI and shells out.
//! Extracted from the `execute` dispatch so that file lives as a routing table.

use crate::{app::App, symbols};

/// Open the command palette popup (all named commands, fuzzy-filterable).
pub(super) fn command_palette(app: &mut App) {
    let recency = crate::history::recency_map(app);
    app.popup = Some(crate::popup::Popup::command_palette(
        crate::popup::command_palette_items(),
        recency,
    ));
}

/// Telescope-style grep over the current buffer (one item per line).
pub(super) fn grep_buffer(app: &mut App) {
    let rope = &app.buffer.rope;
    let path = app.buffer.path.clone().unwrap_or_default();
    let items: Vec<crate::popup::ListItem> = rope
        .lines()
        .enumerate()
        .map(|(line_idx, line)| {
            let label = line
                .to_string()
                .trim_end_matches(&['\r', '\n'][..])
                .to_owned();
            crate::popup::ListItem::navigate(
                label,
                format!("Line {}", line_idx + 1),
                &path,
                line_idx,
                0,
            )
        })
        .collect();
    app.popup = Some(crate::popup::Popup::grep(
        "grep buffer",
        items,
        app.search.query.clone(),
    ));
}

/// Project-wide grep (ripgrep when available, otherwise `grep -rn`).
pub(super) fn grep_project(app: &mut App) {
    let root = app
        .buffer
        .path
        .as_deref()
        .and_then(|p| p.parent())
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let output = if rg_is_available() {
        std::process::Command::new("rg")
            .args(["--line-number", "--no-heading", "--color=never", "--with-filename", "."])
            .current_dir(&root)
            .output()
    } else {
        std::process::Command::new("grep")
            .args(["-rn", "-I", "."])
            .arg(".")
            .current_dir(&root)
            .output()
    };

    let items: Vec<crate::popup::ListItem> = match output {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines()
                .filter_map(|line| {
                    // format: file:lineno:content
                    let mut parts = line.splitn(3, ':');
                    let file = parts.next()?;
                    let lineno_str = parts.next()?;
                    let content = parts.next().unwrap_or("").trim_end_matches(&['\r', '\n'][..]);
                    let lineno: usize = lineno_str.parse().ok()?;
                    let path = root.join(file);
                    Some(crate::popup::ListItem::navigate(
                        content.to_owned(),
                        format!("{}:{}", file, lineno),
                        &path,
                        lineno.saturating_sub(1),
                        0,
                    ))
                })
                .collect()
        }
        Err(_) => {
            app.message = Some("grep not available".into());
            return;
        }
    };

    app.popup = Some(crate::popup::Popup::grep(
        "grep project",
        items,
        app.search.query.clone(),
    ));
}

/// Fuzzy picker over the open-buffers list (pre-selects the current buffer).
pub(super) fn buffer_picker(app: &mut App) {
    let current = app.buffer.path.clone();
    let items: Vec<crate::popup::ListItem> = app
        .open_buffers
        .iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            let detail = p.to_string_lossy().into_owned();
            Some(crate::popup::ListItem::navigate(name, detail, p, 0, 0))
        })
        .collect();
    if items.is_empty() {
        app.message = Some("No open buffers".into());
        return;
    }
    let mut popup = crate::popup::Popup::navigate("buffers", items);
    if let crate::popup::PopupContent::List(ref mut state) = popup.content {
        if let Some(cur) = &current {
            let cur_str = cur.to_string_lossy();
            if let Some(idx) = state
                .items
                .iter()
                .position(|it| it.detail.as_deref() == Some(cur_str.as_ref()))
            {
                state.selected = idx;
            }
        }
    }
    app.popup = Some(popup);
}

/// Tree-sitter symbol picker for the current buffer.
pub(super) fn symbol_picker(app: &mut App) {
    let lang = app.current_language().unwrap_or("").to_owned();
    let path = app.buffer.path.clone().unwrap_or_else(|| {
        std::path::PathBuf::from(format!("untitled.{}", crate::lang::lang_to_ext(&lang)))
    });
    let syms = symbols::extract_symbols(&app.buffer.rope, &lang);
    if syms.is_empty() {
        app.message = Some("No symbols found".into());
        return;
    }
    let items: Vec<crate::popup::ListItem> = syms
        .iter()
        .map(|s| {
            crate::popup::ListItem::navigate(
                format!("{} {}", s.kind, s.name),
                format!("line {}", s.line + 1),
                &path,
                s.line,
                s.col,
            )
        })
        .collect();
    app.popup = Some(crate::popup::Popup::navigate("symbols", items));
}

/// Picker over all current LSP diagnostics, sorted by severity.
pub(super) fn diagnostic_picker(app: &mut App) {
    use crate::lsp_manager::DiagnosticSeverity;
    let mut items: Vec<crate::popup::ListItem> = Vec::new();
    for (path_str, diags) in &app.lsp.diagnostics {
        let path = std::path::PathBuf::from(path_str);
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_str.clone());
        for d in diags {
            let sev = match d.severity {
                DiagnosticSeverity::Error => "error",
                DiagnosticSeverity::Warning => "warning",
                DiagnosticSeverity::Information => "info",
                DiagnosticSeverity::Hint => "hint",
            };
            items.push(crate::popup::ListItem::navigate(
                d.message.clone(),
                format!("{file}:{} [{sev}]", d.line + 1),
                &path,
                d.line,
                d.col_start,
            ));
        }
    }
    if items.is_empty() {
        app.message = Some("No diagnostics".into());
        return;
    }
    items.sort_by(|a, b| {
        let sev_rank = |detail: &Option<String>| -> u8 {
            match detail.as_deref().and_then(|d| d.split('[').nth(1)) {
                Some(s) if s.starts_with("error") => 0,
                Some(s) if s.starts_with("warning") => 1,
                _ => 2,
            }
        };
        sev_rank(&a.detail)
            .cmp(&sev_rank(&b.detail))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    app.popup = Some(crate::popup::Popup::navigate("diagnostics", items));
}

/// Open a file: an external picker command when configured, else the built-in
/// fuzzy file list.
pub(super) fn file_picker(app: &mut App) {
    match app.config.editor.file_picker.clone() {
        Some(cmd) => open_file_external_picker(app, &cmd),
        None => open_file_picker_popup(app),
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn rg_is_available() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_ok()
    })
}

/// Open a fuzzy-filterable file list popup from the project root.
fn open_file_picker_popup(app: &mut App) {
    let root = std::env::current_dir().unwrap_or_else(|_| {
        app.buffer
            .path
            .as_deref()
            .and_then(|p| p.parent())
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_default()
    });

    let max_files = app.config.editor.file_picker_max_files;
    let max_depth = app.config.editor.file_picker_max_depth;
    let mut items: Vec<crate::popup::ListItem> = Vec::new();
    collect_files(&root, &root, &mut items, 0, max_depth, max_files);
    items.sort_by(|a, b| a.label.cmp(&b.label));

    if items.is_empty() {
        app.message = Some("No files found".into());
        return;
    }

    app.popup = Some(crate::popup::Popup::navigate("open file", items));
}

/// Recursively collect files under `dir` relative to `base`, skipping noise.
fn collect_files(
    base: &std::path::Path,
    dir: &std::path::Path,
    items: &mut Vec<crate::popup::ListItem>,
    depth: usize,
    max_depth: usize,
    max_files: usize,
) {
    if depth > max_depth || items.len() >= max_files {
        return;
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else { return };

    let mut entries: Vec<_> = read_dir.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        if items.len() >= max_files {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            if matches!(
                name_str.as_ref(),
                "target" | "node_modules" | "__pycache__" | "dist" | "build" | "out"
            ) {
                continue;
            }
            collect_files(base, &path, items, depth + 1, max_depth, max_files);
        } else {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let label = rel.to_string_lossy().into_owned();
            let abs = path.canonicalize().unwrap_or_else(|_| path.clone());
            items.push(crate::popup::ListItem::navigate(label, abs.to_string_lossy(), &abs, 0, 0));
        }
    }
}

/// Suspend the TUI, run an external picker command, then resume.
///
/// The command receives:
///   SV_PICKER_FILE  — path to a temp file; write the chosen file path there
///                     (preferred for TUI pickers like yazi that own the screen)
///   SV_CURRENT_DIR  — directory of the currently open buffer
///
/// If SV_PICKER_FILE is non-empty after the command exits, that path is used.
/// Otherwise the command's stdout is used (works well with fzf).
fn open_file_external_picker(app: &mut App, cmd: &str) {
    use crossterm::{execute, terminal};
    use std::io::{self, Write};

    let tmp_path = std::env::temp_dir().join(format!("sv-picker-{}.txt", std::process::id()));

    let current_dir = app
        .buffer
        .path
        .as_deref()
        .and_then(|p| p.parent())
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // Suspend TUI so the external picker has the full terminal.
    let _ = terminal::disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, crossterm::terminal::LeaveAlternateScreen);
    let _ = stdout.flush();

    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("SV_PICKER_FILE", &tmp_path)
        .env("SV_CURRENT_DIR", &current_dir)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output();

    // Resume TUI.
    let _ = execute!(stdout, crossterm::terminal::EnterAlternateScreen);
    let _ = terminal::enable_raw_mode();
    crate::theme::initialize_color_cache();

    // Determine chosen path: temp file wins over stdout.
    let chosen = if tmp_path.exists() {
        let content = std::fs::read_to_string(&tmp_path).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp_path);
        content.trim().to_owned()
    } else {
        match output {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
            Err(e) => {
                app.message = Some(format!("File picker error: {e}"));
                return;
            }
        }
    };

    if !chosen.is_empty() {
        let path = std::path::PathBuf::from(&chosen);
        super::lsp::open_file_at(app, &path, 0, 0);
    }

    app.needs_clear = true;
}
