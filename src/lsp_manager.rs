use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::{
    config::LanguageServerConfig,
    lsp::{
        char_to_lsp_pos, path_to_uri, uri_to_path, LspClient, NotebookCell, PendingKind,
        ServerMessage,
    },
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A processed event from any language server.
#[derive(Debug)]
pub enum LspEvent {
    /// Server finished initializing — re-send didOpen for the current document.
    Initialized { language: String },
    /// Server published diagnostics for a file.
    Diagnostics {
        #[allow(dead_code)]
        path: PathBuf,
        items: Vec<Diagnostic>,
    },
    /// Completion response — show popup if still in Insert mode.
    CompletionResult { items: Vec<CompletionItem> },
    /// Hover response — show documentation popup.
    HoverResult { content: String },
    /// Definition / type-definition / implementation response.
    DefinitionResult { location: Option<LspLocation> },
    /// References response — may be multiple locations.
    ReferencesResult { locations: Vec<LspLocation> },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Diagnostic {
    pub line: usize,
    pub col_start: usize,
    pub col_end: usize,
    pub message: String,
    pub severity: DiagnosticSeverity,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub kind: Option<String>,
    /// Text to insert; falls back to `label` when absent.
    pub insert_text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LspLocation {
    pub path: PathBuf,
    pub line: usize,
    pub character: usize,
}

// ---------------------------------------------------------------------------
// LspManager
// ---------------------------------------------------------------------------

pub struct LspManager {
    /// One server per language id (e.g. "python", "rust").
    clients: HashMap<String, LspClient>,
    /// Diagnostics indexed by canonicalized path string.
    pub diagnostics: HashMap<String, Vec<Diagnostic>>,
    /// Open notebooks: notebook_uri → (code_cell_uris, current_notebook_version).
    notebook_state: HashMap<String, (Vec<String>, i32)>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
            diagnostics: HashMap::new(),
            notebook_state: HashMap::new(),
        }
    }

    /// Start a language server for `language` unless one is already running.
    pub fn ensure_server(
        &mut self,
        language: &str,
        config: &LanguageServerConfig,
        root_path: Option<&std::path::Path>,
    ) {
        if self.clients.contains_key(language) {
            return;
        }
        match LspClient::start(&config.command, &config.args) {
            Ok(mut client) => {
                let cwd = std::env::current_dir().ok();
                // Workspace root for LSP (rootUri/workspaceFolders): prefer cwd
                // so pyproject.toml, Cargo.toml, etc. are visible to the server.
                let workspace_root = cwd.as_deref().or(root_path);
                let root_uri = workspace_root
                    .map(path_to_uri)
                    .unwrap_or_else(|| "file:///".into());

                // Venv/environment search root: prefer the file's own directory
                // so a .venv sitting next to the file is found even when ki was
                // launched from a different directory.
                let venv_root = root_path.or(cwd.as_deref());

                // Build initializationOptions: user config wins; fall back to
                // auto-detected options (e.g. venv for Python).
                let init_options = config.init_options.clone().or_else(|| {
                    build_init_options(language, venv_root)
                });

                client.initialize(&root_uri, init_options.as_ref());
                self.clients.insert(language.to_owned(), client);
            }
            Err(_) => {
                // Server binary not installed — silently ignore.
            }
        }
    }

    pub fn did_open(&mut self, language: &str, path: &std::path::Path, text: &str) {
        if let Some(client) = self.clients.get_mut(language) {
            if !client.initialized {
                return;
            }
            client.did_open(&path_to_uri(path), language, text);
        }
    }

    pub fn did_change(&mut self, language: &str, path: &std::path::Path, text: &str) {
        if let Some(client) = self.clients.get_mut(language) {
            if !client.initialized {
                return;
            }
            client.did_change(&path_to_uri(path), text);
        }
    }

    pub fn did_close(&mut self, language: &str, path: &std::path::Path) {
        if let Some(client) = self.clients.get_mut(language) {
            client.did_close(&path_to_uri(path));
        }
    }

    pub fn request_completion(
        &mut self,
        language: &str,
        path: &std::path::Path,
        rope: &ropey::Rope,
        char_idx: usize,
    ) -> bool {
        if let Some(client) = self.clients.get_mut(language) {
            if !client.initialized { return false; }
            let uri = path_to_uri(path);
            let (line, character) = char_to_lsp_pos(rope, char_idx);
            client.request_completion(&uri, line, character);
            return true;
        }
        false
    }

    pub fn request_hover(
        &mut self,
        language: &str,
        path: &std::path::Path,
        rope: &ropey::Rope,
        char_idx: usize,
    ) -> bool {
        if let Some(client) = self.clients.get_mut(language) {
            if !client.initialized { return false; }
            let uri = path_to_uri(path);
            let (line, character) = char_to_lsp_pos(rope, char_idx);
            client.request_hover(&uri, line, character);
            return true;
        }
        false
    }

    pub fn request_definition(
        &mut self,
        language: &str,
        path: &std::path::Path,
        rope: &ropey::Rope,
        char_idx: usize,
    ) -> bool {
        if let Some(client) = self.clients.get_mut(language) {
            if !client.initialized { return false; }
            let uri = path_to_uri(path);
            let (line, character) = char_to_lsp_pos(rope, char_idx);
            client.request_definition(&uri, line, character);
            return true;
        }
        false
    }

    pub fn request_references(
        &mut self,
        language: &str,
        path: &std::path::Path,
        rope: &ropey::Rope,
        char_idx: usize,
    ) -> bool {
        if let Some(client) = self.clients.get_mut(language) {
            if !client.initialized { return false; }
            let uri = path_to_uri(path);
            let (line, character) = char_to_lsp_pos(rope, char_idx);
            client.request_references(&uri, line, character);
            return true;
        }
        false
    }

    pub fn request_type_definition(
        &mut self,
        language: &str,
        path: &std::path::Path,
        rope: &ropey::Rope,
        char_idx: usize,
    ) -> bool {
        if let Some(client) = self.clients.get_mut(language) {
            if !client.initialized { return false; }
            let uri = path_to_uri(path);
            let (line, character) = char_to_lsp_pos(rope, char_idx);
            client.request_type_definition(&uri, line, character);
            return true;
        }
        false
    }

    pub fn request_implementation(
        &mut self,
        language: &str,
        path: &std::path::Path,
        rope: &ropey::Rope,
        char_idx: usize,
    ) -> bool {
        if let Some(client) = self.clients.get_mut(language) {
            if !client.initialized { return false; }
            let uri = path_to_uri(path);
            let (line, character) = char_to_lsp_pos(rope, char_idx);
            client.request_implementation(&uri, line, character);
            return true;
        }
        false
    }

    /// True if `path` has already been opened on the named server.
    pub fn is_doc_open(&self, language: &str, path: &std::path::Path) -> bool {
        let uri = path_to_uri(path);
        self.clients
            .get(language)
            .map(|c| c.is_doc_open(&uri))
            .unwrap_or(false)
    }

    /// True if the named server advertised `notebookDocumentSync`.
    pub fn notebook_sync_supported(&self, language: &str) -> bool {
        self.clients
            .get(language)
            .map(|c| c.supports_notebook_sync())
            .unwrap_or(false)
    }

    /// Send `notebookDocument/didOpen` for the whole notebook.
    /// Returns false if the server isn't initialised yet.
    pub fn notebook_did_open(
        &mut self,
        language: &str,
        notebook_uri: &str,
        cells: &[NotebookCell],
    ) -> bool {
        if let Some(client) = self.clients.get_mut(language) {
            if !client.initialized {
                return false;
            }
            let entry = self
                .notebook_state
                .entry(notebook_uri.to_owned())
                .or_insert((vec![], 0));
            entry.1 += 1;
            let version = entry.1;
            entry.0 = cells
                .iter()
                .filter(|c| c.kind == 2)
                .map(|c| c.uri.clone())
                .collect();
            client.notebook_did_open(notebook_uri, version, cells);
            return true;
        }
        false
    }

    /// Send `notebookDocument/didChange` for one cell's text content.
    /// Returns false if the server isn't initialised yet.
    pub fn notebook_did_change_cell(
        &mut self,
        language: &str,
        notebook_uri: &str,
        cell_uri: &str,
        text: &str,
    ) -> bool {
        if let Some(client) = self.clients.get_mut(language) {
            if !client.initialized {
                return false;
            }
            let entry = self
                .notebook_state
                .entry(notebook_uri.to_owned())
                .or_insert((vec![], 0));
            entry.1 += 1;
            let nb_version = entry.1;
            client.notebook_did_change_cell(notebook_uri, nb_version, cell_uri, text);
            return true;
        }
        false
    }

    /// Send `notebookDocument/didClose` and clear tracking state.
    pub fn notebook_did_close(&mut self, language: &str, notebook_uri: &str) {
        if let Some((cell_uris, _)) = self.notebook_state.remove(notebook_uri) {
            if let Some(client) = self.clients.get_mut(language) {
                client.notebook_did_close(notebook_uri, &cell_uris);
            }
        }
    }

    /// Drain all pending server messages and return semantic events.
    pub fn poll(&mut self) -> Vec<LspEvent> {
        let mut events = Vec::new();
        let languages: Vec<String> = self.clients.keys().cloned().collect();
        for lang in languages {
            // Collect raw messages first (separate borrow)
            let msgs = {
                let client = self.clients.get_mut(&lang).unwrap();
                client.poll()
            };
            for msg in msgs {
                let client = self.clients.get_mut(&lang).unwrap();
                if let Some(evt) =
                    process_message(client, &lang, msg, &mut self.diagnostics)
                {
                    events.push(evt);
                }
            }
        }
        events
    }

    /// Return true if the named server is running and initialized.
    pub fn is_ready(&self, language: &str) -> bool {
        self.clients.get(language).map(|c| c.initialized).unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Message processing
// ---------------------------------------------------------------------------

fn process_message(
    client: &mut LspClient,
    language: &str,
    msg: ServerMessage,
    diagnostics: &mut HashMap<String, Vec<Diagnostic>>,
) -> Option<LspEvent> {
    match msg {
        ServerMessage::Response { id, result, error: _ } => {
            let kind = client.pending.remove(&id)?;
            let result = result.unwrap_or(Value::Null);

            match kind {
                PendingKind::Initialize => {
                    if let Some(caps) = result.get("capabilities") {
                        client.server_capabilities = caps.clone();
                    }
                    client.initialized = true;
                    client.send_initialized();
                    Some(LspEvent::Initialized {
                        language: language.to_owned(),
                    })
                }
                PendingKind::Completion => {
                    let items = parse_completion_result(&result);
                    Some(LspEvent::CompletionResult { items })
                }
                PendingKind::Hover => {
                    parse_hover_result(&result).map(|content| LspEvent::HoverResult { content })
                }
                PendingKind::Definition
                | PendingKind::TypeDefinition
                | PendingKind::Implementation => {
                    let location = parse_location_result(&result);
                    Some(LspEvent::DefinitionResult { location })
                }
                PendingKind::References => {
                    let locations = parse_locations_result(&result);
                    Some(LspEvent::ReferencesResult { locations })
                }
            }
        }
        ServerMessage::Notification { method, params } => {
            if method == "textDocument/publishDiagnostics" {
                let params = params?;
                let uri = params.get("uri")?.as_str()?;
                let path = uri_to_path(uri)?;
                let path_str = path.to_string_lossy().to_string();
                let items = parse_diagnostics(params.get("diagnostics")?);
                diagnostics.insert(path_str, items.clone());
                Some(LspEvent::Diagnostics { path, items })
            } else {
                None // Ignore other server notifications (window/logMessage etc.)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Result parsers
// ---------------------------------------------------------------------------

fn parse_completion_result(val: &Value) -> Vec<CompletionItem> {
    let items = if val.is_array() {
        val.as_array().cloned().unwrap_or_default()
    } else {
        val.get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default()
    };

    items
        .iter()
        .filter_map(|item| {
            let label = item.get("label")?.as_str()?.to_owned();
            let detail = item
                .get("detail")
                .and_then(|d| d.as_str())
                .map(str::to_owned);
            // Prefer textEdit.newText, then insertText, then fall back to label.
            let insert_text = item
                .get("textEdit")
                .and_then(|te| te.get("newText"))
                .and_then(|t| t.as_str())
                .or_else(|| item.get("insertText").and_then(|t| t.as_str()))
                .map(strip_snippet_owned);
            let kind = item
                .get("kind")
                .and_then(|k| k.as_u64())
                .map(completion_kind_name);
            Some(CompletionItem {
                label,
                detail,
                kind,
                insert_text,
            })
        })
        .collect()
}

fn parse_hover_result(val: &Value) -> Option<String> {
    let contents = val.get("contents")?;

    if let Some(s) = contents.as_str() {
        return Some(s.to_owned());
    }
    if let Some(obj) = contents.as_object() {
        if let Some(value) = obj.get("value").and_then(|v| v.as_str()) {
            return Some(value.to_owned());
        }
    }
    // MarkedString[]
    if let Some(arr) = contents.as_array() {
        let parts: Vec<&str> = arr
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .or_else(|| item.get("value").and_then(|v| v.as_str()))
            })
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("\n\n"));
        }
    }
    None
}

fn parse_location_result(val: &Value) -> Option<LspLocation> {
    if val.is_array() {
        val.as_array()?.first().and_then(parse_single_location)
    } else if val.is_null() {
        None
    } else {
        parse_single_location(val)
    }
}

fn parse_locations_result(val: &Value) -> Vec<LspLocation> {
    match val.as_array() {
        Some(arr) => arr.iter().filter_map(parse_single_location).collect(),
        None => Vec::new(),
    }
}

fn parse_single_location(val: &Value) -> Option<LspLocation> {
    let uri = val.get("uri")?.as_str()?;
    let path = uri_to_path(uri)?;
    let range = val.get("range")?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as usize;
    let character = start.get("character")?.as_u64()? as usize;
    Some(LspLocation {
        path,
        line,
        character,
    })
}

fn parse_diagnostics(val: &Value) -> Vec<Diagnostic> {
    val.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    let message = d.get("message")?.as_str()?.to_owned();
                    let range = d.get("range")?;
                    let start = range.get("start")?;
                    let line = start.get("line")?.as_u64()? as usize;
                    let col_start = start.get("character")?.as_u64()? as usize;
                    let col_end = range
                        .get("end")?
                        .get("character")?
                        .as_u64()
                        .unwrap_or(col_start as u64) as usize;
                    let severity = d
                        .get("severity")
                        .and_then(|s| s.as_u64())
                        .map(|s| match s {
                            1 => DiagnosticSeverity::Error,
                            2 => DiagnosticSeverity::Warning,
                            3 => DiagnosticSeverity::Information,
                            _ => DiagnosticSeverity::Hint,
                        })
                        .unwrap_or(DiagnosticSeverity::Error);
                    Some(Diagnostic {
                        line,
                        col_start,
                        col_end,
                        message,
                        severity,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build server-specific initializationOptions for known servers.
fn build_init_options(
    language: &str,
    root: Option<&std::path::Path>,
) -> Option<serde_json::Value> {
    match language {
        "python" => {
            // Find the best Python interpreter: project venv first, then the
            // Python that's actually active in the user's shell (handles pyenv,
            // conda, globally-installed packages, etc.).
            let python = root.and_then(detect_python_venv)
                .or_else(detect_active_python);

            let mut jedi: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
            if let Some(ref p) = python {
                jedi.insert(
                    "environment".into(),
                    serde_json::json!(p.to_string_lossy().as_ref()),
                );
            }
            // Always tell Jedi where the project root is so it can resolve
            // local modules even without an explicit venv.
            if let Some(root_path) = root {
                jedi.insert(
                    "extra_paths".into(),
                    serde_json::json!([root_path.to_string_lossy().as_ref()]),
                );
            }

            Some(serde_json::json!({
                "pylsp": {
                    "plugins": {
                        "jedi": jedi
                    }
                }
            }))
        }
        _ => None,
    }
}

/// Walk `start` and its ancestors (up to 4 levels) looking for a venv.
fn detect_python_venv(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let candidates = ["bin/python3", "bin/python"];
    let venv_dirs = [".venv", "venv", ".env", "env"];

    let mut dir = start.to_path_buf();
    for _ in 0..4 {
        for venv in &venv_dirs {
            for bin in &candidates {
                let python = dir.join(venv).join(bin);
                if python.exists() {
                    return Some(python);
                }
            }
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
    None
}

/// Ask the current `python3` on PATH where its executable lives.
/// Handles pyenv shims, conda envs, and any other non-venv setup where the
/// user's packages are installed into whichever Python is active in the shell.
fn detect_active_python() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = std::path::PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim(),
    );
    if path.exists() { Some(path) } else { None }
}

/// Strip LSP snippet markers and return an owned String.
/// Handles `${N:placeholder}`, `$N`, and `$0` tab-stop markers.
fn strip_snippet_owned(s: &str) -> String {
    if !s.contains('$') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                // ${N:placeholder} or ${N} — emit the placeholder text only.
                chars.next(); // consume '{'
                // Skip until ':' or '}'
                let mut found_colon = false;
                for ch in chars.by_ref() {
                    if ch == ':' { found_colon = true; break; }
                    if ch == '}' { break; }
                }
                if found_colon {
                    // Emit chars up to matching '}'
                    for ch in chars.by_ref() {
                        if ch == '}' { break; }
                        out.push(ch);
                    }
                }
            }
            Some(c) if c.is_ascii_digit() || *c == '0' => {
                // $N — consume the number, emit nothing.
                while chars.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    chars.next();
                }
            }
            _ => {
                // Lone '$' — keep it.
                out.push('$');
            }
        }
    }
    out
}

fn completion_kind_name(kind: u64) -> String {
    match kind {
        1 => "text",
        2 => "method",
        3 => "fn",
        4 => "constructor",
        5 => "field",
        6 => "var",
        7 => "class",
        8 => "interface",
        9 => "module",
        10 => "property",
        12 => "value",
        13 => "enum",
        14 => "keyword",
        15 => "snippet",
        17 => "file",
        20 => "enum member",
        21 => "const",
        22 => "struct",
        25 => "type param",
        _ => "item",
    }
    .to_owned()
}

