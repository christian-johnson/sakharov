//! The kernel bridge: look at what your own code built.
//!
//! `gv` lists the kernel's namespace; `Enter` on a dataframe — or `:view df` —
//! opens it in the grid with every motion, summary and transform the editor has.
//!
//! # Why this is where remote databases live
//!
//! The editor handles no credentials (see [`super::attach`]).  It doesn't have
//! to: you connect in a notebook cell, with your own driver and your own auth,
//! and everything that comes back is a dataframe in the namespace — which is
//! exactly what this bridge shows.  So browsing a Postgres table is
//!
//! ```python
//! df = pl.read_database("select * from orders limit 1000", conn)
//! ```
//!
//! followed by `gv`.  The credential stays in the cell, where it is reviewed and
//! versioned like the rest of the analysis, and the editor never learns it.
//!
//! # Transport
//!
//! The kernel writes the frame to a parquet file under the state directory and
//! sends its path; the editor opens that with the same `DuckDbSource` every
//! `.parquet` file uses, then unlinks it.  Parquet rather than a base64 blob on
//! the JSON line protocol because a window of a wide frame is not small — and
//! rather than Arrow IPC because the editor can already read parquet, so the
//! bridge needs no second reader.
//!
//! Requests queue behind a running cell (the runner reads stdin only between
//! executions), which is deliberate — it is what keeps anything from touching a
//! namespace mid-execution.  So a busy kernel is *reported*, never worked
//! around.

use crate::app::App;
use crate::compute::{Consumer, RequestKind, VarInfo};
use crate::source::SourceId;

/// `gv` / `:vars` — ask the kernel what is in its namespace.
pub(super) fn list_variables(app: &mut App) {
    let Some(key) = app.compute.active_key().cloned() else {
        app.messages
            .show("No kernel — open a notebook and run a cell first");
        return;
    };
    let busy = app.compute.get(&key).is_some_and(|s| !s.is_idle());
    let Some(session) = app.compute.get_mut(&key) else { return };
    match session.request(RequestKind::Vars, "", Consumer::VariableList) {
        Ok(_) => app.messages.show(if busy {
            // The queueing is the design, so say so rather than appearing hung.
            "Kernel is busy — the variable list will come back when the cell finishes"
        } else {
            "Reading the kernel's namespace…"
        }),
        Err(e) => app.messages.show(format!("Kernel: {e}")),
    }
}

/// A listing has come back: show it as a picker.
pub(super) fn show_variables(app: &mut App, items: &[VarInfo]) {
    if items.is_empty() {
        app.messages.show("The kernel's namespace is empty");
        return;
    }
    let entries: Vec<crate::popup::ListItem> = items
        .iter()
        .map(|v| {
            let shape = if v.shape.is_empty() {
                String::new()
            } else {
                format!("  [{}]", v.shape)
            };
            // The ones `:view` can open say so — otherwise Enter on a plain int
            // looks like it should do something.
            let openable = if v.viewable { "  ·  Enter opens" } else { "" };
            crate::popup::ListItem::choice(
                v.name.clone(),
                format!("{}{shape}{openable}", v.type_name),
                v.name.clone(),
            )
        })
        .collect();
    app.popup = Some(crate::popup::Popup::variables(entries));
}

/// `:view <name>` (and `Enter` in the explorer) — open a kernel dataframe as a
/// grid.
pub(super) fn view_variable(app: &mut App, name: &str) {
    let name = name.trim();
    // A bound name, never an expression: the kernel looks this up in its
    // namespace rather than evaluating it, and this is what makes that true.
    if !is_identifier(name) {
        app.messages
            .show(format!("`{name}` is not a variable name (:view df)"));
        return;
    }
    let Some(key) = app.compute.active_key().cloned() else {
        app.messages
            .show("No kernel — open a notebook and run a cell first");
        return;
    };
    let path = export_path(name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let busy = app.compute.get(&key).is_some_and(|s| !s.is_idle());
    let Some(session) = app.compute.get_mut(&key) else { return };
    let request = RequestKind::Export { var: name.to_string(), path };
    match session.request(request, "", Consumer::ViewVariable(name.to_string())) {
        Ok(_) if busy => app
            .messages
            .show(format!("Kernel is busy — {name} will open when the cell finishes")),
        Ok(_) => app.messages.show(format!("Fetching {name}…")),
        Err(e) => app.messages.show(format!("Kernel: {e}")),
    }
}

/// An exported frame has landed: open it, then delete the file.
pub(super) fn open_exported(app: &mut App, name: &str, path: &std::path::Path, rows: Option<usize>) {
    #[cfg(feature = "dataframe")]
    {
        use crate::table::TableSource;
        let opened = crate::table::duck::DuckDbSource::open_file(path)
            .map(|source| (source.describe(), Box::new(source)));
        // The file is a transport detail, not a document: gone either way, so a
        // failed fetch can't leave a stale frame behind to be opened later.
        let _ = std::fs::remove_file(path);
        match opened {
            Ok((shape, source)) => {
                let origin = super::table::current_source_id(app);
                super::table::open_derived(
                    app,
                    SourceId::virtual_named(name),
                    source,
                    origin,
                );
                app.messages.show(format!("{name}: {shape} — q goes back"));
            }
            Err(e) => app.messages.show(format!("{name}: {e:#}")),
        }
    }
    #[cfg(not(feature = "dataframe"))]
    {
        let _ = (rows, path);
        let _ = std::fs::remove_file(path);
        app.messages
            .show(format!("Built without the `dataframe` feature — cannot open {name}"));
    }
    #[cfg(feature = "dataframe")]
    let _ = rows;
}

/// Where a fetched frame is staged: the state directory, keyed by name.
///
/// Under `state_dir` rather than the project, so a fetch never drops a file
/// into a directory the user is working in (or under version control).
fn export_path(name: &str) -> std::path::PathBuf {
    let dir = crate::config::state_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("bridge");
    dir.join(format!("{name}-{}.parquet", std::process::id()))
}

/// A Python identifier, conservatively: what can safely be looked up by name.
fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bound_name_is_accepted() {
        assert!(is_identifier("df"));
        assert!(is_identifier("_x1"));
        // Everything that isn't a name is refused before it reaches the kernel:
        // the request looks a value up, it does not evaluate anything.
        assert!(!is_identifier("df.head()"));
        assert!(!is_identifier("__import__('os').system('rm -rf /')"));
        assert!(!is_identifier("a b"));
        assert!(!is_identifier("1df"));
        assert!(!is_identifier(""));
    }

    #[test]
    fn viewing_without_a_kernel_says_so_rather_than_hanging() {
        let mut app = App::new(None, crate::config::Config::load()).unwrap();
        view_variable(&mut app, "df");
        assert!(
            app.messages.log.iter().any(|m| m.contains("No kernel")),
            "{:?}",
            app.messages.log,
        );
    }

    #[test]
    fn an_expression_is_refused_before_any_kernel_is_consulted() {
        let mut app = App::new(None, crate::config::Config::load()).unwrap();
        view_variable(&mut app, "os.system('boom')");
        assert!(
            app.messages.log.iter().any(|m| m.contains("is not a variable name")),
            "{:?}",
            app.messages.log,
        );
    }
}
