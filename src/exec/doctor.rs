//! `:lsp-doctor` — a readout of why the language server is (or isn't) working.
//!
//! Every LSP failure this editor can have looks identical from the outside: a
//! request is sent and nothing comes back, or an empty result does.  The cause
//! is somewhere in a chain the user cannot see — no virtualenv, a binary that
//! isn't installed, a server that died after initializing, a `features` list
//! that routes the request to nobody, a capability the server never advertised,
//! or a document that was never opened on the server the request went to.  This
//! walks that chain in order and reports each link, so the answer to "why are
//! there no completions" is one command rather than a bisection.
//!
//! It is strictly read-only: it inspects state the manager already holds and
//! sends no requests.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::{
    app::{language_for_path, App},
    lsp_manager::{FeatureRoute, ServerHealth, FEATURE_NAMES},
};

/// The buffer the report is written to.  A buffer rather than a float: it is
/// long, and it is worth being able to search and yank out of it.
const REPORT_BUFFER: &str = "*lsp-doctor*";

/// Build the report and show it in `*lsp-doctor*`.
pub fn lsp_doctor(app: &mut App) {
    let report = build_report(app);
    app.special_buffer_ropes
        .insert(REPORT_BUFFER.to_string(), ropey::Rope::from_str(&report));
    super::switch_to_special_buffer(app, REPORT_BUFFER);
    app.messages
        .show("LSP doctor: re-run :lsp-doctor to refresh  ·  :bd to close");
}

fn build_report(app: &mut App) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "LSP doctor\n==========\n");

    let (lang, path) = describe_buffer(app, &mut out);
    let mut problems: Vec<String> = Vec::new();

    let Some(lang) = lang else {
        let _ = writeln!(
            out,
            "No language server applies to this buffer: sakharov maps a file to a\n\
             language by extension (see `lang.rs`), and this one matched nothing.\n\
             Open a source file and re-run :lsp-doctor."
        );
        return out;
    };

    describe_config(app, &lang, &mut out, &mut problems);
    if lang == "python" {
        describe_python_env(app, &mut out, &mut problems);
    }
    let running = describe_servers(app, &lang, path.as_deref(), &mut out, &mut problems);
    describe_routing(app, &lang, running, &mut out, &mut problems);
    describe_documents(app, &lang, path.as_deref(), &mut out, &mut problems);
    describe_diagnostics(app, path.as_deref(), &mut out);

    let _ = writeln!(out, "Problems\n--------");
    if problems.is_empty() {
        let _ = writeln!(out, "  none detected\n");
    } else {
        for p in &problems {
            push_wrapped(&mut out, "  ! ", 4, p);
        }
        let _ = writeln!(out);
    }

    describe_log(app, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

/// What is open, and which language it resolved to.  Returns the language the
/// rest of the report is about plus the path LSP requests are made against —
/// for a notebook that is the focused cell's virtual path, which is what the
/// server was actually told about.
fn describe_buffer(app: &App, out: &mut String) -> (Option<String>, Option<PathBuf>) {
    let _ = writeln!(out, "Buffer\n------");
    let lang = app.current_language().map(str::to_owned);

    if let Some((nb, state)) = app.notebook.as_ref() {
        let code_cells = nb
            .cells
            .iter()
            .filter(|c| c.cell_type == crate::notebook::CellType::Code)
            .count();
        let _ = writeln!(out, "  file       {}", nb.path.display());
        let _ = writeln!(
            out,
            "  view       notebook — {} cells ({code_cells} code), focused [{}]",
            nb.cells.len(),
            state.focused_cell + 1,
        );
        let _ = writeln!(
            out,
            "  language   {}  (from the notebook's kernel metadata)",
            lang.as_deref().unwrap_or("(none)")
        );
        let path = lang.as_ref().map(|l| {
            crate::notebook::cell_virtual_path(&nb.path, l, state.focused_cell)
        });
        if let Some(p) = path.as_ref() {
            let _ = writeln!(out, "  cell doc   {}", p.display());
        }
        let _ = writeln!(out);
        return (lang, path);
    }

    let path = app.buffer.path.clone();
    match path.as_deref() {
        Some(p) if super::is_special_path(p) => {
            let _ = writeln!(out, "  file       {} (virtual buffer)", p.display());
        }
        Some(p) => {
            let _ = writeln!(out, "  file       {}", p.display());
        }
        None => {
            let _ = writeln!(out, "  file       (unsaved — no path)");
        }
    }
    let _ = writeln!(out, "  view       text");
    let detected = language_for_path(path.as_deref());
    let _ = writeln!(
        out,
        "  language   {}  (from the file extension)",
        detected.unwrap_or("(none)")
    );
    let _ = writeln!(out);
    (lang, path)
}

/// The `[language_servers.<lang>]` config, and whether each command actually
/// exists — "the binary isn't installed" is the single most common cause and
/// is otherwise only visible as a launch error scrolled off *Messages*.
fn describe_config(app: &App, lang: &str, out: &mut String, problems: &mut Vec<String>) {
    let _ = writeln!(out, "Configured servers  [language_servers.{lang}]\n{}", "-".repeat(20));
    let Some(cfg) = app.config.language_servers.get(lang) else {
        let _ = writeln!(
            out,
            "  (none) — add a [language_servers.{lang}] section to :config\n"
        );
        problems.push(format!(
            "no language server is configured for {lang} — add [language_servers.{lang}]"
        ));
        return;
    };

    let mut describe = |command: &str, args: &[String], features: &[String]| {
        let scope = if features.is_empty() {
            "(all features)".to_string()
        } else {
            features.join(", ")
        };
        let _ = writeln!(out, "  {command} {}", args.join(" "));
        let _ = writeln!(out, "      features   {scope}");
        match which(command) {
            Some(p) => {
                let _ = writeln!(out, "      binary     {}", p.display());
            }
            None => {
                let _ = writeln!(out, "      binary     NOT FOUND on $PATH");
                problems.push(format!(
                    "'{command}' is not on $PATH — install it, or point \
                     [language_servers.{lang}] at the right command"
                ));
            }
        }
    };
    describe(&cfg.command, &cfg.args, &cfg.features);
    for extra in &cfg.extra_servers {
        describe(&extra.command, &extra.args, &extra.features);
    }
    let _ = writeln!(out);
}

/// Python intelligence is resolved against a virtualenv or not at all (see
/// `LspManager::ensure_server`), so a missing venv means the server was never
/// started — a state with no other visible symptom.
fn describe_python_env(app: &App, out: &mut String, problems: &mut Vec<String>) {
    let _ = writeln!(out, "Python environment\n------------------");
    let root = app
        .notebook
        .as_ref()
        .map(|(nb, _)| nb.path.clone())
        .or_else(|| app.buffer.path.clone())
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::current_dir().ok());

    match root.as_deref().and_then(crate::compute::venv_python_up) {
        Some(p) => {
            let _ = writeln!(out, "  interpreter  {}", p.display());
            let _ = writeln!(
                out,
                "  jedi resolves imports against this environment; a package \
                 missing here\n  has no completions, hovers, or signatures."
            );
        }
        None => {
            let searched = root
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown dir)".into());
            let _ = writeln!(out, "  interpreter  NOT FOUND");
            let _ = writeln!(
                out,
                "  Searched .venv, venv, .env, env upward from {searched}."
            );
            let _ = writeln!(
                out,
                "  The Python language server is deliberately NOT started without one:\n  \
                 completions resolved against the wrong environment are worse than none.\n  \
                 Create one (`uv venv`) and reopen the file."
            );
            problems.push(
                "no virtualenv found — the Python language server was not started".into(),
            );
        }
    }
    let _ = writeln!(out);
}

/// Per-server live state: is the process alive, did it finish the handshake,
/// what did it actually promise to do.  Returns whether any server is running,
/// since with none the routing table below has nothing to describe.
fn describe_servers(
    app: &mut App,
    lang: &str,
    path: Option<&Path>,
    out: &mut String,
    problems: &mut Vec<String>,
) -> bool {
    let uri = path.map(crate::lsp::path_to_uri);
    let health = app.lsp.health(lang, uri.as_deref());
    let _ = writeln!(out, "Running servers\n---------------");
    if health.is_empty() {
        let _ = writeln!(
            out,
            "  (none started) — nothing was launched for {lang}\n"
        );
        problems.push(format!("no {lang} language server process is running"));
        return false;
    }

    for h in &health {
        let _ = writeln!(out, "  {}", h.command);
        report_server(h, out, problems);
    }
    let _ = writeln!(out);
    true
}

fn report_server(h: &ServerHealth, out: &mut String, problems: &mut Vec<String>) {
    let state = match (&h.exited, h.initialized) {
        (Some(status), _) => {
            problems.push(format!(
                "'{}' has exited ({status}) — requests to it will never answer; \
                 restart sakharov after fixing the cause",
                h.command
            ));
            format!("DIED ({status})")
        }
        (None, false) => {
            problems.push(format!(
                "'{}' has not finished the initialize handshake — it is either \
                 still starting or wedged",
                h.command
            ));
            "starting (initialize handshake not finished)".to_string()
        }
        (None, true) => "ready".to_string(),
    };
    let _ = writeln!(out, "      state        {state}");
    let _ = writeln!(out, "      scope        {}", if h.features.is_empty() {
        "(all features)".to_string()
    } else {
        h.features.join(", ")
    });
    let _ = writeln!(out, "      in flight    {} request(s)", h.pending);
    let _ = writeln!(
        out,
        "      documents    {} open  (this buffer: {})",
        h.open_docs,
        if h.current_doc_open { "open" } else { "NOT open" }
    );
    let _ = writeln!(
        out,
        "      sync         text: {}   notebook: {}",
        h.text_sync,
        if h.notebook_sync { "yes" } else { "no (per-cell fallback)" }
    );
    if h.stderr_lines > 0 {
        let _ = writeln!(
            out,
            "      stderr       {} notable line(s) — see *Messages*",
            h.stderr_lines
        );
        problems.push(format!(
            "'{}' wrote {} line(s) to stderr — read them in *Messages* (:messages); \
             a server can answer every request emptily while failing internally",
            h.command, h.stderr_lines
        ));
    }

    if h.initialized {
        let mut has: Vec<&str> = h
            .capabilities
            .iter()
            .filter(|(_, ok)| *ok)
            .map(|(f, _)| *f)
            .collect();
        if h.signature_help {
            has.push("signature-help");
        }
        if h.completion_resolve {
            has.push("completion-resolve");
        }
        let missing: Vec<&str> = h
            .capabilities
            .iter()
            .filter(|(_, ok)| !*ok)
            .map(|(f, _)| *f)
            .collect();
        let listed = if has.is_empty() { "(nothing)".to_string() } else { has.join(", ") };
        push_wrapped(out, "      provides     ", 19, &listed);
        if !missing.is_empty() {
            push_wrapped(out, "      not offered  ", 19, &missing.join(", "));
        }
    }
}

/// Which server each request kind reaches — the `features` routing made
/// explicit, since a request that lands nowhere is otherwise indistinguishable
/// from a server that answered with nothing.
fn describe_routing(
    app: &App,
    lang: &str,
    running: bool,
    out: &mut String,
    problems: &mut Vec<String>,
) {
    let _ = writeln!(out, "Feature routing\n---------------");
    // With nothing running, every feature routes nowhere for one reason —
    // stated once above.  Repeating it per feature, in the language of
    // `features` scoping, would point at config that is very likely fine.
    if !running {
        let _ = writeln!(
            out,
            "  (undetermined — no {lang} server is running; fix the above first)\n"
        );
        return;
    }
    for feature in FEATURE_NAMES {
        let route = app.lsp.feature_route(lang, feature);
        let _ = writeln!(out, "  {feature:<16} {}", route_note(&route, feature, lang, problems));
    }
    let _ = writeln!(
        out,
        "  {:<16} rides on the 'hover' feature's server",
        "signature-help"
    );
    let _ = writeln!(out);
}

fn route_note(
    route: &FeatureRoute,
    feature: &str,
    lang: &str,
    problems: &mut Vec<String>,
) -> String {
    match (&route.active, &route.configured) {
        (Some(active), _) if !route.capable => {
            problems.push(format!(
                "{feature}: '{active}' answers these requests but never advertised \
                 the capability — expect empty results"
            ));
            format!("{active}  — server does not advertise this capability")
        }
        (Some(active), Some(configured)) if active != configured => {
            format!("{active}  (falling back; '{configured}' owns it but is not ready)")
        }
        (Some(active), _) => format!("{active}  — ok"),
        (None, Some(configured)) => {
            format!("(nobody yet)  — '{configured}' owns it but is not ready")
        }
        (None, None) => {
            problems.push(format!(
                "{feature}: no server claims it — every `features` list under \
                 [language_servers.{lang}] is scoped, so leave one empty to make \
                 it the catch-all"
            ));
            "(nobody)  — no configured server claims this feature".to_string()
        }
    }
}

/// Whether the thing under the cursor was ever transmitted.  A server only
/// answers about documents it has been sent, and the notebook path has two
/// extra ways to go wrong (cells omitted, notebook never opened).
fn describe_documents(
    app: &mut App,
    lang: &str,
    path: Option<&Path>,
    out: &mut String,
    problems: &mut Vec<String>,
) {
    let _ = writeln!(out, "Documents\n---------");
    match path {
        None => {
            let _ = writeln!(out, "  this buffer has no path — nothing is synced to any server");
            let _ = writeln!(out, "  save it (:w) to enable LSP features\n");
            return;
        }
        Some(p) => {
            let _ = writeln!(out, "  uri          {}", crate::lsp::path_to_uri(p));
        }
    }

    let uri = path.map(crate::lsp::path_to_uri);
    let health = app.lsp.health(lang, uri.as_deref());
    if !health.is_empty() && !health.iter().any(|h| h.current_doc_open) {
        problems.push(
            "no server has this document open — requests about it return nothing. \
             Switching buffers away and back re-sends didOpen"
                .into(),
        );
    }

    let notebooks = app.lsp.synced_notebooks();
    if app.notebook.is_some() {
        if notebooks.is_empty() {
            let _ = writeln!(out, "  notebook     NOT synced to any server");
            problems.push(
                "the notebook was never opened with the LSP — cross-cell completions \
                 and diagnostics cannot work"
                    .into(),
            );
        }
        for (uri, cells) in &notebooks {
            let _ = writeln!(out, "  notebook     {cells} code cell(s) synced — {uri}");
        }
        let _ = writeln!(
            out,
            "  (markdown and raw cells are deliberately never transmitted)"
        );
    }
    let _ = writeln!(out);
}

/// Diagnostics are the one feature with no request to trace, so the evidence
/// that the server is doing anything at all is whether any ever arrived.
fn describe_diagnostics(app: &App, path: Option<&Path>, out: &mut String) {
    let _ = writeln!(out, "Diagnostics\n-----------");
    let files = app.lsp.diagnostics.values().filter(|v| !v.is_empty()).count();
    match path.map(crate::lsp::diagnostic_key) {
        Some(key) => {
            let n = app.lsp.diagnostics.get(&key).map_or(0, Vec::len);
            let _ = writeln!(out, "  this file    {n} published");
        }
        None => {
            let _ = writeln!(out, "  this file    (no path)");
        }
    }
    let _ = writeln!(out, "  workspace    {files} file(s) with diagnostics");
    let _ = writeln!(out);
}

/// The lifecycle log filtered to LSP lines: the launch errors, venv discovery
/// and server stderr this report's conclusions were drawn from.
fn describe_log(app: &App, out: &mut String) {
    let _ = writeln!(out, "LSP messages so far  (full log: :messages)\n{}", "-".repeat(20));
    let lines: Vec<&String> = app
        .messages
        .log
        .iter()
        .filter(|l| l.contains("LSP") || l.contains("language server"))
        .collect();
    if lines.is_empty() {
        let _ = writeln!(out, "  (none)");
    }
    for line in lines {
        push_wrapped(out, "  ", 4, line);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Column the report wraps at.  It is read in an ordinary buffer, whose
/// default is no soft wrap, so a long line would clip at the terminal edge
/// with the informative half off screen — the report wraps itself instead.
const WIDTH: usize = 78;

/// Append `text` after `prefix`, wrapping at [`WIDTH`] with continuation lines
/// indented to `indent`.
fn push_wrapped(out: &mut String, prefix: &str, indent: usize, text: &str) {
    let mut col = prefix.len();
    out.push_str(prefix);
    for (i, word) in text.split_whitespace().enumerate() {
        let w = word.chars().count();
        if i > 0 {
            if col + 1 + w > WIDTH {
                out.push('\n');
                out.push_str(&" ".repeat(indent));
                col = indent;
            } else {
                out.push(' ');
                col += 1;
            }
        }
        out.push_str(word);
        col += w;
    }
    out.push('\n');
}

/// Resolve `command` the way the OS will when the server is spawned: a name
/// containing a separator is used as-is, anything else is searched on `$PATH`.
fn which(command: &str) -> Option<PathBuf> {
    let candidate = Path::new(command);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(command))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::{build_report, which};
    use crate::{app::App, config::Config};

    /// The report's job is to name the broken link in the chain. With a server
    /// configured whose binary doesn't exist, it must say so, must not claim
    /// anything is answering requests, and must surface both in `Problems` —
    /// that is the whole difference between this and reading *Messages*.
    #[test]
    fn a_missing_server_binary_is_named_not_merely_implied() {
        let mut config = Config::load();
        // Pin the config: the developer's own ~/.config must not decide this.
        config.language_servers.clear();
        config.language_servers.insert(
            "python".into(),
            serde_json::from_value(serde_json::json!({
                "command": "definitely-not-a-real-binary-xyzzy"
            }))
            .unwrap(),
        );
        let mut app = App::new(None, config).unwrap();
        app.buffer.path = Some(std::env::temp_dir().join("sv_doctor_test.py"));
        app.lsp_language = Some("python".into());

        let report = build_report(&mut app);
        assert!(report.contains("language   python"), "{report}");
        assert!(
            report.contains("definitely-not-a-real-binary-xyzzy"),
            "the configured command must appear: {report}"
        );
        assert!(report.contains("NOT FOUND on $PATH"), "{report}");
        assert!(
            report.contains("(none started)"),
            "no process is running, and the report must say so: {report}"
        );
        // Routing must not imply requests are going somewhere useful, and with
        // nothing running it must not blame the `features` config either.
        assert!(!report.contains("— ok"), "nothing is ok here: {report}");
        assert!(report.contains("undetermined"), "{report}");
        assert!(!report.contains("no server claims it"), "{report}");

        let problems = report.split("Problems\n---").nth(1).unwrap_or("");
        assert!(problems.contains("not on $PATH"), "{report}");
        assert!(!problems.contains("none detected"), "{report}");
    }

    /// A buffer with no language at all is a dead end, not a half-report:
    /// nothing below the language line can mean anything.
    #[test]
    fn a_buffer_with_no_language_stops_at_the_first_link() {
        let mut app = App::new(None, Config::load()).unwrap();
        let report = build_report(&mut app);
        assert!(report.contains("No language server applies"), "{report}");
        assert!(!report.contains("Feature routing"), "{report}");
    }

    /// `which` must agree with the spawn: a bare name is looked up on $PATH, a
    /// path with separators is taken literally, and a nonexistent one is None
    /// (which is what turns into the "NOT FOUND on $PATH" problem line).
    #[test]
    fn which_resolves_the_way_spawn_will() {
        assert!(which("sh").is_some(), "sh must be found on PATH");
        assert_eq!(which("/bin/sh"), Some(std::path::PathBuf::from("/bin/sh")));
        assert!(which("definitely-not-a-real-binary-xyzzy").is_none());
        assert!(which("./definitely-not-here").is_none());
    }
}
