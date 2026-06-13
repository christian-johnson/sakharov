use std::collections::{HashMap, HashSet};
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
    SignatureHelp,
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
        // Signature help has no dedicated config feature; it rides on the
        // general (all-features) server alongside hover/completion.
        LspRequestKind::SignatureHelp  => "hover",
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
    /// Server published diagnostics (already merged into `LspManager::diagnostics`);
    /// the receiver just refreshes its per-line cache.
    Diagnostics,
    /// Completion response — show popup if still in Insert mode.
    CompletionResult { items: Vec<CompletionItem> },
    /// `completionItem/resolve` response — enriched documentation for an item.
    CompletionResolved {
        documentation: Option<String>,
        detail: Option<String>,
    },
    /// Hover response — show documentation popup.
    HoverResult { content: String },
    /// Signature-help response — call-argument hint for the minibuffer.
    /// `None` when the cursor is not inside a call the server recognises.
    SignatureHelpResult { signature: Option<String> },
    /// Definition / type-definition / implementation response.
    DefinitionResult { location: Option<LspLocation> },
    /// References response — may be multiple locations.
    ReferencesResult { locations: Vec<LspLocation> },
    /// Code actions available at the cursor/selection position.
    CodeActionsResult { actions: Vec<serde_json::Value> },
    /// Formatting result — apply these TextEdits to the buffer.
    FormattingResult { edits: Vec<serde_json::Value> },
    /// A pre-warm completion round-trip finished — the server's analysis cache
    /// is now hot. Carries nothing to apply; `LspManager::poll` logs it (with
    /// timing) to *Messages*.
    PrewarmComplete { language: String },
}

#[derive(Debug, Clone)]
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
    /// Documentation shown in the `K` doc panel, when the server includes it
    /// inline.  `None` means it may still be fetchable via `completionItem/resolve`.
    pub documentation: Option<String>,
    /// Raw JSON of the original completion item, sent verbatim with
    /// `completionItem/resolve` to fetch documentation on demand.
    pub data: Option<String>,
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
    /// Server-lifecycle log lines (venv discovery, launched/failed/ready),
    /// drained once per frame by `exec::lsp::process_lsp_events` into *Messages*.
    lifecycle_log: Vec<String>,
    /// Each distinct lifecycle line is logged at most once per session —
    /// `ensure_server` runs on every cell/buffer switch, so unconditional
    /// logging would spam (e.g. the no-venv line on every cell navigation).
    logged: HashSet<String>,
    /// Document URIs already pre-warmed this session — warm-up fires once per
    /// document (the server keeps its cache hot afterwards).
    prewarmed: HashSet<String>,
    /// In-flight pre-warm request id → start instant, for round-trip timing.
    prewarm_started: HashMap<u64, std::time::Instant>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            diagnostics: HashMap::new(),
            server_diagnostics: HashMap::new(),
            notebook_state: HashMap::new(),
            lifecycle_log: Vec::new(),
            logged: HashSet::new(),
            prewarmed: HashSet::new(),
            prewarm_started: HashMap::new(),
        }
    }

    fn log_once(&mut self, msg: String) {
        if self.logged.insert(msg.clone()) {
            self.lifecycle_log.push(msg);
        }
    }

    /// Drain pending lifecycle log lines (for the *Messages* log).
    pub fn take_lifecycle_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.lifecycle_log)
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
        // (command, args, init_options, features)
        type ServerSpec<'a> = (&'a str, &'a [String], Option<&'a serde_json::Value>, &'a [String]);
        let mut specs: Vec<ServerSpec> = vec![
            (&config.command, &config.args, config.init_options.as_ref(), &config.features),
        ];
        for extra in &config.extra_servers {
            specs.push((&extra.command, &extra.args, extra.init_options.as_ref(), &extra.features));
        }

        // Whether a feature-scoped server owns diagnostics / formatting. pylsp's
        // lint and format plugins then only duplicate that work on every
        // keystroke (the user's ruff already covers it), so build_init_options
        // turns them off — jedi intelligence is unaffected.
        let diagnostics_elsewhere = specs
            .iter()
            .any(|(_, _, _, features)| features.iter().any(|f| f == "diagnostics"));
        let format_elsewhere = specs
            .iter()
            .any(|(_, _, _, features)| features.iter().any(|f| f == "format"));

        let cwd = std::env::current_dir().ok();
        let workspace_root = cwd.as_deref().or(root_path);
        let root_uri = workspace_root
            .map(path_to_uri)
            .unwrap_or_else(|| "file:///".into());
        let venv_root = root_path.or(cwd.as_deref());

        // Python intelligence (jedi) resolves imports against an interpreter's
        // environment. We require the project's own virtualenv — discovered by
        // walking up from the file's location — and never fall back to the
        // system interpreter. With no venv there is nothing useful to resolve
        // against, so we don't start the Python server at all (no autocomplete
        // is better than autocomplete against the wrong environment).
        let py_venv = if language == "python" {
            match venv_root.and_then(crate::notebook::venv_python_up) {
                Some(p) => {
                    self.log_once(format!("LSP: python environment {}", p.display()));
                    Some(p)
                }
                None => {
                    let searched = venv_root
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(unknown dir)".into());
                    self.log_once(format!(
                        "Python LSP not started: no virtualenv (.venv/venv/.env/env) \
                         in {searched} or any parent"
                    ));
                    return;
                }
            }
        } else {
            None
        };

        for (command, args, init_options, features) in specs {
            // Skip if already started.
            let already_running = self.servers.get(language)
                .map(|ss| ss.iter().any(|s| s.command == command))
                .unwrap_or(false);
            if already_running {
                continue;
            }

            let init_opts = init_options.cloned().or_else(|| {
                build_init_options(
                    language,
                    venv_root,
                    py_venv.as_deref(),
                    diagnostics_elsewhere,
                    format_elsewhere,
                )
            });

            match LspClient::start(command, args) {
                Ok(mut client) => {
                    client.initialize(&root_uri, init_opts.as_ref());
                    self.log_once(format!("LSP: launched '{command}' ({language})"));
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
                Err(e) => {
                    // Server binary not installed or failed to start — soft degradation.
                    self.log_once(format!("LSP: failed to launch '{command}' ({language}): {e}"));
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

    /// Single-edit `didChange`: a range delta (`start..end` replaced by
    /// `new_text`, UTF-16 LSP positions in the pre-edit document) to servers
    /// that negotiated incremental sync, a full-text didChange to the rest.
    /// `rope` is the post-edit document — stringified at most once, and only
    /// when some server actually needs the full text.
    pub fn did_change_delta(
        &mut self,
        language: &str,
        path: &std::path::Path,
        start: (u32, u32),
        end: (u32, u32),
        new_text: &str,
        rope: &ropey::Rope,
    ) {
        let uri = path_to_uri(path);
        let mut full: Option<String> = None;
        if let Some(servers) = self.servers.get_mut(language) {
            for server in servers.iter_mut() {
                if !server.client.initialized {
                    continue;
                }
                if server.client.supports_incremental_sync() {
                    server.client.did_change_incremental(&uri, start, end, new_text);
                } else {
                    let text = full.get_or_insert_with(|| rope.to_string());
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
                    LspRequestKind::SignatureHelp => servers[idx].client.request_signature_help(&uri, line, character),
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

    /// Fire a one-shot throwaway completion on the completion server to warm its
    /// analysis cache, once per document. pylsp/jedi parses a file's imports
    /// lazily on the first request, so without this the cold-cache cost lands on
    /// the user's first real completion/hover; pre-warming pulls it forward to
    /// file-open time instead. The response is discarded (`PendingKind::Prewarm`),
    /// but the in-flight request drives the status-bar spinner. Returns true if a
    /// warm-up was dispatched.
    pub fn prewarm(
        &mut self,
        language: &str,
        path: &std::path::Path,
        rope: &ropey::Rope,
        char_idx: usize,
    ) -> bool {
        let uri = path_to_uri(path);
        if self.prewarmed.contains(&uri) {
            return false;
        }
        let Some(idx) = self.server_idx_for_feature(language, "completion") else {
            return false;
        };
        // Only warm a document the completion server actually has open.
        let Some(servers) = self.servers.get_mut(language) else {
            return false;
        };
        if !servers[idx].client.is_doc_open(&uri) {
            return false;
        }
        let (line, character) = char_to_lsp_pos(rope, char_idx);
        let id = servers[idx].client.request_prewarm(&uri, line, character);
        self.prewarmed.insert(uri);
        self.prewarm_started.insert(id, std::time::Instant::now());
        self.lifecycle_log
            .push(format!("LSP: pre-warming {language} language server…"));
        true
    }

    /// True if the completion server for `language` advertised
    /// `completionProvider.resolveProvider`, i.e. it can enrich items with docs.
    pub fn completion_resolve_supported(&self, language: &str) -> bool {
        self.server_idx_for_feature(language, "completion")
            .and_then(|idx| self.servers.get(language).map(|ss| (idx, ss)))
            .map(|(idx, ss)| {
                ss[idx]
                    .client
                    .server_capabilities
                    .get("completionProvider")
                    .and_then(|c| c.get("resolveProvider"))
                    .and_then(|r| r.as_bool())
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// Send `completionItem/resolve` for `item` via the completion server.
    /// Returns true if a request was actually dispatched.
    pub fn request_completion_resolve(&mut self, language: &str, item: serde_json::Value) -> bool {
        if !self.completion_resolve_supported(language) {
            return false;
        }
        if let Some(idx) = self.server_idx_for_feature(language, "completion") {
            if let Some(servers) = self.servers.get_mut(language) {
                servers[idx].client.request_completion_resolve(item);
                return true;
            }
        }
        false
    }

    /// Send the notebook to every initialized server: `notebookDocument/didOpen`
    /// to servers advertising notebook sync, per-cell `textDocument/didOpen` to the
    /// rest. Idempotent per server (skips servers that already have the notebook /
    /// cell open), so the per-server `Initialized` retrigger can call it repeatedly.
    pub fn notebook_did_open(
        &mut self,
        language: &str,
        notebook_uri: &str,
        cells: &[NotebookCell],
    ) -> bool {
        let any_ready = self
            .servers
            .get(language)
            .map(|ss| ss.iter().any(|s| s.client.initialized))
            .unwrap_or(false);
        if !any_ready {
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
            for server in servers.iter_mut() {
                if !server.client.initialized {
                    continue;
                }
                if server.client.supports_notebook_sync() {
                    if !server.client.is_doc_open(notebook_uri) {
                        server.client.notebook_did_open(notebook_uri, version, cells);
                    }
                } else {
                    for cell in cells.iter().filter(|c| c.kind == 2) {
                        if server.client.is_doc_open(&cell.uri) {
                            server.client.did_change(&cell.uri, &cell.text);
                        } else {
                            server.client.did_open(&cell.uri, &cell.language_id, &cell.text);
                        }
                    }
                }
            }
        }
        true
    }

    /// Propagate one cell's new text to every initialized server:
    /// `notebookDocument/didChange` where the server has the notebook open,
    /// plain `textDocument/didChange` on the cell's virtual doc otherwise.
    pub fn notebook_did_change_cell(
        &mut self,
        language: &str,
        notebook_uri: &str,
        cell_uri: &str,
        text: &str,
    ) -> bool {
        let any_ready = self
            .servers
            .get(language)
            .map(|ss| ss.iter().any(|s| s.client.initialized))
            .unwrap_or(false);
        if !any_ready {
            return false;
        }
        // Only code cells are transmitted to servers (markup cells are omitted
        // from the notebookDocument/didOpen payload entirely), so a change to a
        // markdown/raw cell must not be synced either — servers would reject or
        // crash on a didChange for a cell they were never told about.
        let is_code_cell = self
            .notebook_state
            .get(notebook_uri)
            .map(|(uris, _)| uris.iter().any(|u| u == cell_uri))
            .unwrap_or(false);
        if !is_code_cell {
            return false;
        }
        let nb_version = {
            let entry = self.notebook_state.entry(notebook_uri.to_owned()).or_insert((vec![], 0));
            entry.1 += 1;
            entry.1
        };
        if let Some(servers) = self.servers.get_mut(language) {
            for server in servers.iter_mut() {
                if !server.client.initialized {
                    continue;
                }
                if server.client.supports_notebook_sync() {
                    if server.client.is_doc_open(notebook_uri) {
                        server.client.notebook_did_change_cell(notebook_uri, nb_version, cell_uri, text);
                    }
                } else if server.client.is_doc_open(cell_uri) {
                    server.client.did_change(cell_uri, text);
                } else {
                    server.client.did_open(cell_uri, language, text);
                }
            }
        }
        true
    }

    /// Send `notebookDocument/didClose` (or per-cell `didClose` for servers
    /// without notebook sync) and clear tracking state.
    pub fn notebook_did_close(&mut self, language: &str, notebook_uri: &str) {
        // Remove returns owned cell_uris, releasing the borrow before we touch servers.
        let cell_uris = self.notebook_state.remove(notebook_uri).map(|(uris, _)| uris);
        if let Some(cell_uris) = cell_uris {
            if let Some(servers) = self.servers.get_mut(language) {
                for server in servers.iter_mut() {
                    if server.client.supports_notebook_sync() {
                        if server.client.is_doc_open(notebook_uri) {
                            server.client.notebook_did_close(notebook_uri, &cell_uris);
                        }
                    } else {
                        for uri in &cell_uris {
                            if server.client.is_doc_open(uri) {
                                server.client.did_close(uri);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Sync `text` into a shadow document on the server that handles `kind`,
    /// then send the request against it at (line, character).
    ///
    /// Used for notebook hover / signature-help / references: pylsp only
    /// concatenates cells for completion and definition, so cross-cell context
    /// is invisible to the other requests when made on a cell URI. The shadow
    /// document is the whole notebook joined into one virtual text document —
    /// a URI only, never written to disk — giving those requests full context.
    pub fn request_via_shadow_doc(
        &mut self,
        kind: LspRequestKind,
        language: &str,
        shadow_path: &std::path::Path,
        text: &str,
        line: u32,
        character: u32,
    ) -> bool {
        if !matches!(
            kind,
            LspRequestKind::Hover | LspRequestKind::SignatureHelp | LspRequestKind::References
        ) {
            return false;
        }
        let feature = request_feature(kind);
        let Some(idx) = self.server_idx_for_feature(language, feature) else {
            return false;
        };
        let uri = path_to_uri(shadow_path);
        let Some(servers) = self.servers.get_mut(language) else { return false };
        let client = &mut servers[idx].client;
        // Fingerprint-gated: retransmits the concatenated notebook only when
        // its content actually changed since the last shadow-doc request.
        client.sync_full_doc(&uri, language, text);
        let _ = match kind {
            LspRequestKind::Hover => client.request_hover(&uri, line, character),
            LspRequestKind::SignatureHelp => client.request_signature_help(&uri, line, character),
            LspRequestKind::References => client.request_references(&uri, line, character),
            _ => unreachable!(),
        };
        true
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
                    // Capture the response id before `msg` is consumed, so a
                    // pre-warm completion can be timed by its request id.
                    let msg_id = match &msg {
                        ServerMessage::Response { id, .. } => Some(*id),
                        _ => None,
                    };
                    let server = &mut self.servers.get_mut(&lang).unwrap()[idx];
                    let command = server.command.clone();
                    if let Some(evt) = process_message(
                        &mut server.client,
                        &lang,
                        idx,
                        msg,
                        &mut self.diagnostics,
                        &mut self.server_diagnostics,
                    ) {
                        match &evt {
                            LspEvent::Initialized { .. } => {
                                self.log_once(format!("LSP: '{command}' ready ({lang})"));
                            }
                            LspEvent::PrewarmComplete { language } => {
                                let dur = msg_id
                                    .and_then(|id| self.prewarm_started.remove(&id))
                                    .map(|t| format!(" in {:.1}s", t.elapsed().as_secs_f64()))
                                    .unwrap_or_default();
                                self.lifecycle_log.push(format!(
                                    "LSP: {language} language server warmed{dur} — completions ready"
                                ));
                            }
                            _ => {}
                        }
                        events.push(evt);
                    }
                }
            }
        }
        events
    }

    /// True if any server has an in-flight request awaiting a reply.  Drives the
    /// status-bar spinner; deliberately scoped to pending requests (not the
    /// initialization handshake) so a server that fails to initialize can't pin
    /// the spinner on forever.
    pub fn has_pending_requests(&self) -> bool {
        self.servers
            .values()
            .flatten()
            .any(|s| !s.client.pending.is_empty())
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
                PendingKind::CompletionResolve => {
                    let documentation = parse_documentation(&result);
                    let detail = result
                        .get("detail")
                        .and_then(|d| d.as_str())
                        .map(str::to_owned);
                    Some(LspEvent::CompletionResolved { documentation, detail })
                }
                PendingKind::Hover => {
                    // Always emit an event so the caller can show feedback even
                    // when the server returns null (no docs for this position).
                    let content = parse_hover_result(&result).unwrap_or_default();
                    Some(LspEvent::HoverResult { content })
                }
                PendingKind::SignatureHelp => {
                    Some(LspEvent::SignatureHelpResult {
                        signature: parse_signature_help(&result),
                    })
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
                PendingKind::Prewarm => Some(LspEvent::PrewarmComplete {
                    language: language.to_owned(),
                }),
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
                diagnostics.insert(path_str, merged);

                Some(LspEvent::Diagnostics)
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
            let documentation = parse_documentation(item);
            // Keep the raw item so it can be sent back for `completionItem/resolve`.
            let data = serde_json::to_string(item).ok();
            Some(CompletionItem {
                label,
                detail,
                kind,
                insert_text,
                documentation,
                data,
            })
        })
        .collect()
}

/// Extract a completion/resolve item's `documentation` field, which may be a
/// plain string or a `MarkupContent` object (`{ kind, value }`).
fn parse_documentation(item: &Value) -> Option<String> {
    let doc = item.get("documentation")?;
    if let Some(s) = doc.as_str() {
        return Some(s.to_owned());
    }
    doc.get("value").and_then(|v| v.as_str()).map(str::to_owned)
}

/// Extract the active signature's label from a `textDocument/signatureHelp`
/// result, with the active parameter marked. Returns `None` when the cursor is
/// not inside a recognised call.
fn parse_signature_help(val: &Value) -> Option<String> {
    let signatures = val.get("signatures")?.as_array()?;
    if signatures.is_empty() {
        return None;
    }
    let active_sig = val.get("activeSignature").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let sig = signatures.get(active_sig).or_else(|| signatures.first())?;
    let label = sig.get("label")?.as_str()?.to_owned();

    // Mark the active parameter with ‹…› so the user can see which arg they're on.
    let active_param = sig
        .get("activeParameter")
        .or_else(|| val.get("activeParameter"))
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    if let Some(pidx) = active_param {
        if let Some(params) = sig.get("parameters").and_then(|p| p.as_array()) {
            if let Some(param) = params.get(pidx) {
                // A parameter's `label` is either a string or an [start, end] range
                // into the signature label.
                if let Some(plabel) = param.get("label").and_then(|l| l.as_str()) {
                    if let Some(pos) = label.find(plabel) {
                        let mut marked = String::with_capacity(label.len() + 2);
                        marked.push_str(&label[..pos]);
                        marked.push('‹');
                        marked.push_str(plabel);
                        marked.push('›');
                        marked.push_str(&label[pos + plabel.len()..]);
                        return Some(marked);
                    }
                } else if let Some(range) = param.get("label").and_then(|l| l.as_array()) {
                    if let (Some(s), Some(e)) = (
                        range.first().and_then(|v| v.as_u64()).map(|n| n as usize),
                        range.get(1).and_then(|v| v.as_u64()).map(|n| n as usize),
                    ) {
                        // Range is in UTF-16 code units; for ASCII signatures this
                        // matches byte offsets. Guard against out-of-range slicing.
                        if s <= e && e <= label.len() && label.is_char_boundary(s) && label.is_char_boundary(e) {
                            let mut marked = String::with_capacity(label.len() + 2);
                            marked.push_str(&label[..s]);
                            marked.push('‹');
                            marked.push_str(&label[s..e]);
                            marked.push('›');
                            marked.push_str(&label[e..]);
                            return Some(marked);
                        }
                    }
                }
            }
        }
    }
    Some(label)
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
///
/// `venv_python` is the project virtualenv interpreter discovered by
/// [`detect_python_venv`]; it is the only environment jedi is pointed at.
fn build_init_options(
    language: &str,
    root: Option<&std::path::Path>,
    venv_python: Option<&std::path::Path>,
    diagnostics_elsewhere: bool,
    format_elsewhere: bool,
) -> Option<serde_json::Value> {
    match language {
        "python" => {
            let mut jedi: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
            // pylsp defaults this to ["numpy"], which makes jedi resolve numpy by
            // importing it instead of static analysis — and that path cannot
            // enumerate numpy's lazily-bound submodules (np.random / np.fft /
            // np.ma return zero completions, hovers, and signatures). Static
            // analysis handles numpy fine, so turn auto-import off.
            jedi.insert("auto_import_modules".into(), serde_json::json!([]));
            if let Some(p) = venv_python {
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

            let mut plugins: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
            plugins.insert("jedi".into(), serde_json::Value::Object(jedi));
            // pylsp runs its lint plugins on EVERY didChange — that work is
            // pure duplication (and per-keystroke latency in the process that
            // also answers completions) when a feature-scoped server such as
            // `ruff server` owns diagnostics / formatting. jedi_* plugins
            // (completion, hover, signatures, definitions) stay enabled.
            if diagnostics_elsewhere {
                for plugin in ["pycodestyle", "pyflakes", "mccabe", "pylint", "flake8", "pydocstyle"] {
                    plugins.insert(plugin.into(), serde_json::json!({ "enabled": false }));
                }
            }
            if format_elsewhere {
                for plugin in ["autopep8", "yapf"] {
                    plugins.insert(plugin.into(), serde_json::json!({ "enabled": false }));
                }
            }

            Some(serde_json::json!({ "pylsp": { "plugins": plugins } }))
        }
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::{build_init_options, parse_signature_help, LspManager, ManagedServer};
    use crate::lsp::{path_to_uri, LspClient, PendingKind};
    use serde_json::json;

    /// Inject a fake initialized all-features server (backed by `cat`) so the
    /// completion-routing / doc-open paths can be exercised without a real LSP.
    fn manager_with_ready_server(language: &str) -> LspManager {
        let mut client = LspClient::start("cat", &[]).expect("spawn cat");
        client.initialized = true; // skip the real initialize handshake
        let mut mgr = LspManager::new();
        mgr.servers.insert(
            language.to_owned(),
            vec![ManagedServer { client, features: vec![], command: "cat".into() }],
        );
        mgr
    }

    /// Pre-warm fires exactly once per document, only after the doc is open, and
    /// queues a discardable `Prewarm` request plus a *Messages* log line.
    #[test]
    fn prewarm_fires_once_per_open_document() {
        let mut mgr = manager_with_ready_server("python");
        let rope = ropey::Rope::from_str("import os\n");
        let path = std::path::Path::new("/tmp/sv_prewarm_test.py");

        // Doc not opened yet → nothing to warm.
        assert!(!mgr.prewarm("python", path, &rope, 0));

        // Open it, then warm: the first call dispatches, the second is gated.
        mgr.did_open("python", path, "import os\n");
        assert!(mgr.prewarm("python", path, &rope, 0));
        assert!(!mgr.prewarm("python", path, &rope, 0), "second warm is gated");

        // A Prewarm request is in flight (drives the spinner) and the start of
        // warm-up was logged for *Messages*.
        let uri = path_to_uri(path);
        assert!(mgr.servers["python"][0].client.is_doc_open(&uri));
        let pending_prewarms = mgr.servers["python"][0]
            .client
            .pending
            .values()
            .filter(|k| **k == PendingKind::Prewarm)
            .count();
        assert_eq!(pending_prewarms, 1);
        let log = mgr.take_lifecycle_log();
        assert!(
            log.iter().any(|l| l.contains("pre-warming python")),
            "expected a pre-warming log line, got {log:?}"
        );
    }

    /// pylsp's lint/format plugins are disabled exactly when a feature-scoped
    /// server owns diagnostics/format; jedi stays enabled either way, and a
    /// pylsp-only setup (no scoped servers) keeps pylsp's defaults untouched.
    #[test]
    fn pylsp_plugins_disabled_only_when_feature_owned_elsewhere() {
        let opts = build_init_options("python", None, None, true, true).unwrap();
        let plugins = &opts["pylsp"]["plugins"];
        assert_eq!(plugins["pyflakes"]["enabled"], json!(false));
        assert_eq!(plugins["pycodestyle"]["enabled"], json!(false));
        assert_eq!(plugins["autopep8"]["enabled"], json!(false));
        assert!(plugins["jedi"].is_object(), "jedi config must survive");

        let opts = build_init_options("python", None, None, false, false).unwrap();
        let plugins = &opts["pylsp"]["plugins"];
        assert!(plugins.get("pyflakes").is_none(), "pylsp-only setup keeps lint defaults");
        assert!(plugins.get("autopep8").is_none());
        assert!(plugins["jedi"].is_object());
    }

    #[test]
    fn python_without_venv_logs_once_and_starts_nothing() {
        let cfg: crate::config::LanguageServerConfig =
            serde_json::from_value(json!({ "command": "pylsp" })).unwrap();
        let mut mgr = LspManager::new();
        let dir = std::path::Path::new("/nonexistent-sakharov-test-dir");
        mgr.ensure_server("python", &cfg, Some(dir));
        // A second call (cell navigation re-runs ensure_server) must not repeat the line.
        mgr.ensure_server("python", &cfg, Some(dir));
        let log = mgr.take_lifecycle_log();
        assert_eq!(log.len(), 1, "expected exactly one no-venv line, got {log:?}");
        assert!(log[0].contains("no virtualenv"), "unexpected log line: {}", log[0]);
        assert!(!mgr.is_ready("python"));
        assert!(mgr.take_lifecycle_log().is_empty(), "log should drain");
    }

    #[test]
    fn signature_help_marks_active_parameter() {
        // A typical pylsp response for `randn(` with the cursor on the first arg.
        let resp = json!({
            "signatures": [{
                "label": "randn(d0, d1, ...)",
                "parameters": [{ "label": "d0" }, { "label": "d1" }],
            }],
            "activeSignature": 0,
            "activeParameter": 0,
        });
        assert_eq!(parse_signature_help(&resp).as_deref(), Some("randn(‹d0›, d1, ...)"));
    }

    #[test]
    fn signature_help_supports_range_parameter_labels() {
        // Parameters can be [start, end] offsets into the label instead of strings.
        let resp = json!({
            "signatures": [{
                "label": "f(a, b)",
                "parameters": [{ "label": [2, 3] }, { "label": [5, 6] }],
            }],
            "activeSignature": 0,
            "activeParameter": 1,
        });
        assert_eq!(parse_signature_help(&resp).as_deref(), Some("f(a, ‹b›)"));
    }

    #[test]
    fn signature_help_empty_is_none() {
        assert_eq!(parse_signature_help(&json!({ "signatures": [] })), None);
        assert_eq!(parse_signature_help(&json!(null)), None);
    }

    #[test]
    fn signature_help_without_active_param_returns_bare_label() {
        let resp = json!({ "signatures": [{ "label": "g()" }] });
        assert_eq!(parse_signature_help(&resp).as_deref(), Some("g()"));
    }
}
