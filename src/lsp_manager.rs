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

/// Which LSP position-based request to send.
#[derive(Debug, Clone, Copy)]
pub enum LspRequestKind {
    Completion,
    Hover,
    Definition,
    References,
    TypeDefinition,
    Implementation,
}

/// Map an LspRequestKind to the feature name used in server config.
fn request_feature(kind: LspRequestKind) -> &'static str {
    match kind {
        LspRequestKind::Completion     => "completion",
        LspRequestKind::Hover          => "hover",
        LspRequestKind::Definition     => "definition",
        LspRequestKind::References     => "references",
        LspRequestKind::TypeDefinition => "type-definition",
        LspRequestKind::Implementation => "implementation",
    }
}

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
    /// Code actions available at the cursor/selection position.
    CodeActionsResult { actions: Vec<serde_json::Value> },
    /// Formatting result — apply these TextEdits to the buffer.
    FormattingResult { edits: Vec<serde_json::Value> },
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
// Internal server instance
// ---------------------------------------------------------------------------

struct ManagedServer {
    client: LspClient,
    /// Feature list. Empty = all features (general/primary server).
    features: Vec<String>,
    /// Command name stored for dedup in `ensure_server`.
    command: String,
}

impl ManagedServer {
    fn supports_feature(&self, feature: &str) -> bool {
        self.features.is_empty() || self.features.iter().any(|f| f == feature)
    }

    fn has_specific_features(&self) -> bool {
        !self.features.is_empty()
    }
}

// ---------------------------------------------------------------------------
// LspManager
// ---------------------------------------------------------------------------

pub struct LspManager {
    /// Multiple servers per language id (primary first, then extra_servers).
    servers: HashMap<String, Vec<ManagedServer>>,
    /// Diagnostics indexed by canonicalized path string.
    /// When multiple servers report diagnostics for the same path, the last
    /// received wins per-server slot (keyed as "path\x00idx").
    /// The public `diagnostics` HashMap exposes the merged view keyed by plain path.
    pub diagnostics: HashMap<String, Vec<Diagnostic>>,
    /// Internal per-server diagnostics: "path\x00server_idx" → items.
    server_diagnostics: HashMap<String, Vec<Diagnostic>>,
    /// Open notebooks: notebook_uri → (code_cell_uris, current_notebook_version).
    notebook_state: HashMap<String, (Vec<String>, i32)>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            diagnostics: HashMap::new(),
            server_diagnostics: HashMap::new(),
            notebook_state: HashMap::new(),
        }
    }

    /// Start language server(s) for `language` unless they are already running.
    /// Handles both the primary server and any `extra_servers` in the config.
    pub fn ensure_server(
        &mut self,
        language: &str,
        config: &LanguageServerConfig,
        root_path: Option<&std::path::Path>,
    ) {
        // Collect all server specs: primary first, then extras.
        let mut specs: Vec<(&str, &[String], Option<&serde_json::Value>, &[String])> = vec![
            (&config.command, &config.args, config.init_options.as_ref(), &config.features),
        ];
        for extra in &config.extra_servers {
            specs.push((&extra.command, &extra.args, extra.init_options.as_ref(), &extra.features));
        }

        let cwd = std::env::current_dir().ok();
        let workspace_root = cwd.as_deref().or(root_path);
        let root_uri = workspace_root
            .map(path_to_uri)
            .unwrap_or_else(|| "file:///".into());
        let venv_root = root_path.or(cwd.as_deref());

        for (command, args, init_options, features) in specs {
            // Skip if already started.
            let already_running = self.servers.get(language)
                .map(|ss| ss.iter().any(|s| s.command == command))
                .unwrap_or(false);
            if already_running {
                continue;
            }

            let init_opts = init_options.cloned().or_else(|| {
                build_init_options(language, venv_root)
            });

            match LspClient::start(command, args) {
                Ok(mut client) => {
                    client.initialize(&root_uri, init_opts.as_ref());
                    let server = ManagedServer {
                        client,
                        features: features.to_vec(),
                        command: command.to_owned(),
                    };
                    self.servers
                        .entry(language.to_owned())
                        .or_default()
                        .push(server);
                }
                Err(_) => {
                    // Server binary not installed or failed to start — soft degradation.
                }
            }
        }
    }

    /// Send `textDocument/didOpen` to all initialized servers that don't already
    /// have this document open.
    pub fn did_open(&mut self, language: &str, path: &std::path::Path, text: &str) {
        let uri = path_to_uri(path);
        if let Some(servers) = self.servers.get_mut(language) {
            for server in servers.iter_mut() {
                if server.client.initialized && !server.client.is_doc_open(&uri) {
                    server.client.did_open(&uri, language, text);
                }
            }
        }
    }

    /// Send `textDocument/didChange` to all initialized servers.
    pub fn did_change(&mut self, language: &str, path: &std::path::Path, text: &str) {
        let uri = path_to_uri(path);
        if let Some(servers) = self.servers.get_mut(language) {
            for server in servers.iter_mut() {
                if server.client.initialized {
                    server.client.did_change(&uri, text);
                }
            }
        }
    }

    /// Send `textDocument/didClose` to all servers that have this document open.
    pub fn did_close(&mut self, language: &str, path: &std::path::Path) {
        let uri = path_to_uri(path);
        if let Some(servers) = self.servers.get_mut(language) {
            for server in servers.iter_mut() {
                if server.client.is_doc_open(&uri) {
                    server.client.did_close(&uri);
                }
            }
        }
    }

    /// Request code actions for the given character range.
    /// Routes to the server that explicitly handles "code-actions", falling back
    /// to the first all-features server.
    pub fn request_code_actions(
        &mut self,
        language: &str,
        path: &std::path::Path,
        rope: &ropey::Rope,
        start_char: usize,
        end_char: usize,
    ) -> bool {
        let uri = path_to_uri(path);
        let (sl, sc) = char_to_lsp_pos(rope, start_char);
        let (el, ec) = char_to_lsp_pos(rope, end_char);
        if let Some(idx) = self.server_idx_for_feature(language, "code-actions") {
            if let Some(servers) = self.servers.get_mut(language) {
                servers[idx].client.request_code_actions(&uri, sl, sc, el, ec);
                return true;
            }
        }
        false
    }

    /// Send `textDocument/formatting` via the appropriate server.
    /// Returns true if the request was dispatched.
    pub fn format_document(
        &mut self,
        language: &str,
        path: &std::path::Path,
        tab_size: usize,
        insert_spaces: bool,
    ) -> bool {
        let uri = path_to_uri(path);
        if let Some(idx) = self.server_idx_for_feature(language, "format") {
            if let Some(servers) = self.servers.get_mut(language) {
                servers[idx].client.request_formatting(&uri, tab_size as u32, insert_spaces);
                return true;
            }
        }
        false
    }

    /// Send `workspace/executeCommand` via the server responsible for code-actions.
    pub fn execute_command(&mut self, language: &str, command: &str, args: serde_json::Value) {
        if let Some(idx) = self.server_idx_for_feature(language, "code-actions") {
            if let Some(servers) = self.servers.get_mut(language) {
                if servers[idx].client.initialized {
                    servers[idx].client.execute_command(command, args);
                }
            }
        }
    }

    /// Dispatch a position-based LSP request to the appropriate server.
    pub fn request(
        &mut self,
        kind: LspRequestKind,
        language: &str,
        path: &std::path::Path,
        rope: &ropey::Rope,
        char_idx: usize,
    ) -> bool {
        let feature = request_feature(kind);
        let uri = path_to_uri(path);
        let (line, character) = char_to_lsp_pos(rope, char_idx);

        if let Some(idx) = self.server_idx_for_feature(language, feature) {
            if let Some(servers) = self.servers.get_mut(language) {
                let _ = match kind {
                    LspRequestKind::Completion    => servers[idx].client.request_completion(&uri, line, character),
                    LspRequestKind::Hover         => servers[idx].client.request_hover(&uri, line, character),
                    LspRequestKind::Definition    => servers[idx].client.request_definition(&uri, line, character),
                    LspRequestKind::References    => servers[idx].client.request_references(&uri, line, character),
                    LspRequestKind::TypeDefinition  => servers[idx].client.request_type_definition(&uri, line, character),
                    LspRequestKind::Implementation  => servers[idx].client.request_implementation(&uri, line, character),
                };
                return true;
            }
        }
        false
    }

    /// True if `path` has been opened on any server for `language`.
    pub fn is_doc_open(&self, language: &str, path: &std::path::Path) -> bool {
        let uri = path_to_uri(path);
        self.servers
            .get(language)
            .map(|ss| ss.iter().any(|s| s.client.is_doc_open(&uri)))
            .unwrap_or(false)
    }

    /// True if any server for `language` advertised `notebookDocumentSync`.
    pub fn notebook_sync_supported(&self, language: &str) -> bool {
        self.servers
            .get(language)
            .map(|ss| ss.iter().any(|s| s.client.supports_notebook_sync()))
            .unwrap_or(false)
    }

    /// Send `notebookDocument/didOpen` via the first server supporting notebook sync.
    pub fn notebook_did_open(
        &mut self,
        language: &str,
        notebook_uri: &str,
        cells: &[NotebookCell],
    ) -> bool {
        let Some(idx) = self.notebook_client_idx(language) else { return false };
        // Check initialized (immutable, released before mutable borrows below).
        if !self.servers.get(language).map(|ss| ss[idx].client.initialized).unwrap_or(false) {
            return false;
        }
        // Update notebook state — drop before borrowing servers mutably.
        let version = {
            let entry = self.notebook_state.entry(notebook_uri.to_owned()).or_insert((vec![], 0));
            entry.1 += 1;
            entry.0 = cells.iter().filter(|c| c.kind == 2).map(|c| c.uri.clone()).collect();
            entry.1
        };
        if let Some(servers) = self.servers.get_mut(language) {
            servers[idx].client.notebook_did_open(notebook_uri, version, cells);
            return true;
        }
        false
    }

    /// Send `notebookDocument/didChange` for one cell via the notebook-sync server.
    pub fn notebook_did_change_cell(
        &mut self,
        language: &str,
        notebook_uri: &str,
        cell_uri: &str,
        text: &str,
    ) -> bool {
        let Some(idx) = self.notebook_client_idx(language) else { return false };
        if !self.servers.get(language).map(|ss| ss[idx].client.initialized).unwrap_or(false) {
            return false;
        }
        let nb_version = {
            let entry = self.notebook_state.entry(notebook_uri.to_owned()).or_insert((vec![], 0));
            entry.1 += 1;
            entry.1
        };
        if let Some(servers) = self.servers.get_mut(language) {
            servers[idx].client.notebook_did_change_cell(notebook_uri, nb_version, cell_uri, text);
            return true;
        }
        false
    }

    /// Send `notebookDocument/didClose` and clear tracking state.
    pub fn notebook_did_close(&mut self, language: &str, notebook_uri: &str) {
        // Remove returns owned cell_uris, releasing the borrow before we touch servers.
        let cell_uris = self.notebook_state.remove(notebook_uri).map(|(uris, _)| uris);
        if let Some(cell_uris) = cell_uris {
            if let Some(idx) = self.notebook_client_idx(language) {
                if let Some(servers) = self.servers.get_mut(language) {
                    servers[idx].client.notebook_did_close(notebook_uri, &cell_uris);
                }
            }
        }
    }

    /// Drain all pending server messages and return semantic events.
    pub fn poll(&mut self) -> Vec<LspEvent> {
        let mut events = Vec::new();
        let languages: Vec<String> = self.servers.keys().cloned().collect();
        for lang in languages {
            let server_count = self.servers[&lang].len();
            for idx in 0..server_count {
                let msgs = {
                    self.servers.get_mut(&lang).unwrap()[idx].client.poll()
                };
                for msg in msgs {
                    let server = &mut self.servers.get_mut(&lang).unwrap()[idx];
                    if let Some(evt) = process_message(
                        &mut server.client,
                        &lang,
                        idx,
                        msg,
                        &mut self.diagnostics,
                        &mut self.server_diagnostics,
                    ) {
                        events.push(evt);
                    }
                }
            }
        }
        events
    }

    /// True if any server for `language` is running and initialized.
    pub fn is_ready(&self, language: &str) -> bool {
        self.servers
            .get(language)
            .map(|ss| ss.iter().any(|s| s.client.initialized))
            .unwrap_or(false)
    }

    // ---------------------------------------------------------------------------
    // Internal helpers
    // ---------------------------------------------------------------------------

    /// Return the index of the server to use for `feature`.
    ///
    /// Priority: a server with a non-empty feature list that includes `feature`
    /// takes precedence over an all-features (empty list) server.
    fn server_idx_for_feature(&self, language: &str, feature: &str) -> Option<usize> {
        let servers = self.servers.get(language)?;
        // First pass: specific-feature server wins.
        for (i, s) in servers.iter().enumerate() {
            if s.has_specific_features() && s.supports_feature(feature) && s.client.initialized {
                return Some(i);
            }
        }
        // Second pass: first all-features server.
        for (i, s) in servers.iter().enumerate() {
            if !s.has_specific_features() && s.client.initialized {
                return Some(i);
            }
        }
        None
    }

    /// Return the index of the server used for notebook sync operations.
    fn notebook_client_idx(&self, language: &str) -> Option<usize> {
        let servers = self.servers.get(language)?;
        // Prefer a server that supports notebook sync.
        for (i, s) in servers.iter().enumerate() {
            if s.client.initialized && s.client.supports_notebook_sync() {
                return Some(i);
            }
        }
        // Fall back to first initialized server.
        servers.iter().enumerate()
            .find(|(_, s)| s.client.initialized)
            .map(|(i, _)| i)
    }
}

// ---------------------------------------------------------------------------
// Message processing
// ---------------------------------------------------------------------------

fn process_message(
    client: &mut LspClient,
    language: &str,
    server_idx: usize,
    msg: ServerMessage,
    diagnostics: &mut HashMap<String, Vec<Diagnostic>>,
    server_diagnostics: &mut HashMap<String, Vec<Diagnostic>>,
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
                    // Always emit an event so the caller can show feedback even
                    // when the server returns null (no docs for this position).
                    let content = parse_hover_result(&result).unwrap_or_default();
                    Some(LspEvent::HoverResult { content })
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
                PendingKind::CodeAction => {
                    let actions = result.as_array().cloned().unwrap_or_default();
                    Some(LspEvent::CodeActionsResult { actions })
                }
                PendingKind::Formatting => {
                    let edits = result.as_array().cloned().unwrap_or_default();
                    Some(LspEvent::FormattingResult { edits })
                }
                PendingKind::ExecuteCommand => None,
            }
        }
        ServerMessage::Notification { method, params } => {
            if method == "textDocument/publishDiagnostics" {
                let params = params?;
                let uri = params.get("uri")?.as_str()?;
                let path = uri_to_path(uri)?;
                let path_str = path.to_string_lossy().to_string();
                let items = parse_diagnostics(params.get("diagnostics")?);

                // Store per-server diagnostics keyed by "path\x00server_idx".
                let slot_key = format!("{path_str}\x00{server_idx}");
                server_diagnostics.insert(slot_key, items.clone());

                // Rebuild the merged view for this path (across all server slots).
                let merged: Vec<Diagnostic> = server_diagnostics
                    .iter()
                    .filter(|(k, _)| {
                        k.split('\x00').next().map(|p| p == path_str).unwrap_or(false)
                    })
                    .flat_map(|(_, diags)| diags.iter().cloned())
                    .collect();
                diagnostics.insert(path_str, merged.clone());

                Some(LspEvent::Diagnostics { path, items: merged })
            } else {
                None
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
            let python = root.and_then(detect_python_venv)
                .or_else(detect_active_python);

            let mut jedi: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
            if let Some(ref p) = python {
                jedi.insert(
                    "environment".into(),
                    serde_json::json!(p.to_string_lossy().as_ref()),
                );
            }
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

fn detect_active_python() -> Option<std::path::PathBuf> {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = std::process::Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output();
        let _ = tx.send(result);
    });

    let out = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if path.exists() { Some(path) } else { None }
}

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
                chars.next();
                let mut found_colon = false;
                for ch in chars.by_ref() {
                    if ch == ':' { found_colon = true; break; }
                    if ch == '}' { break; }
                }
                if found_colon {
                    for ch in chars.by_ref() {
                        if ch == '}' { break; }
                        out.push(ch);
                    }
                }
            }
            Some(c) if c.is_ascii_digit() || *c == '0' => {
                while chars.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    chars.next();
                }
            }
            _ => {
                out.push('$');
            }
        }
    }
    out
}

fn completion_kind_name(kind: u64) -> String {
    match kind {
        1  => "text",
        2  => "method",
        3  => "fn",
        4  => "ctor",
        5  => "field",
        6  => "var",
        7  => "class",
        8  => "iface",
        9  => "mod",
        10 => "prop",
        12 => "value",
        13 => "enum",
        14 => "kw",
        15 => "snip",
        17 => "file",
        20 => "enum↳",
        21 => "const",
        22 => "struct",
        25 => "tparam",
        _  => "item",
    }
    .to_owned()
}
