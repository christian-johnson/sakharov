use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

use serde_json::{json, Value};

/// A raw message received from the language server.
pub enum ServerMessage {
    Response {
        id: u64,
        result: Option<Value>,
        #[allow(dead_code)]
        error: Option<Value>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
}

/// What a pending request is waiting for — tells the manager how to handle the response.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingKind {
    Initialize,
    Completion,
    /// `completionItem/resolve` — enrich a completion item with documentation.
    CompletionResolve,
    Hover,
    /// `textDocument/signatureHelp` — call-argument hints shown in the minibuffer.
    SignatureHelp,
    Definition,
    References,
    TypeDefinition,
    Implementation,
    CodeAction,
    Formatting,
    /// Fire-and-forget server command — response is discarded.
    ExecuteCommand,
}

/// One cell in a `notebookDocument/didOpen` or `didChange` payload.
pub struct NotebookCell {
    /// 1 = markup (markdown/raw), 2 = code.
    pub kind: u8,
    /// The cell's virtual textDocument URI.
    pub uri: String,
    /// LSP languageId for the cell (e.g. "python", "markdown").
    pub language_id: String,
    /// Full source text of the cell.
    pub text: String,
}

/// A single language server process.
pub struct LspClient {
    /// Outgoing messages are serialized + written on a dedicated thread so a
    /// slow or wedged server pipe can never stall the UI thread.
    writer: Option<Sender<Value>>,
    writer_handle: Option<std::thread::JoinHandle<()>>,
    rx: Receiver<ServerMessage>,
    child: Child,
    next_id: u64,
    pub pending: HashMap<u64, PendingKind>,
    pub initialized: bool,
    pub server_capabilities: Value,
    doc_versions: HashMap<String, i32>,
    /// Content fingerprint per full-doc-synced URI (the notebook shadow doc):
    /// lets `sync_full_doc` skip the didChange when nothing changed.
    full_doc_hashes: HashMap<String, u64>,
}

impl LspClient {
    pub fn start(command: &str, args: &[String]) -> anyhow::Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || reader_thread(BufReader::new(stdout), tx));

        let (wtx, wrx) = mpsc::channel();
        let writer_handle = std::thread::spawn(move || writer_thread(BufWriter::new(stdin), wrx));

        Ok(Self {
            writer: Some(wtx),
            writer_handle: Some(writer_handle),
            rx,
            child,
            next_id: 1,
            pending: HashMap::new(),
            initialized: false,
            server_capabilities: Value::Null,
            doc_versions: HashMap::new(),
            full_doc_hashes: HashMap::new(),
        })
    }

    pub fn initialize(
        &mut self,
        root_uri: &str,
        init_options: Option<&serde_json::Value>,
    ) -> u64 {
        let workspace_folder = json!([{"uri": root_uri, "name": "workspace"}]);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": workspace_folder,
            "initializationOptions": init_options,
            "capabilities": {
                "workspace": {
                    "workspaceFolders": true,
                    "didChangeWatchedFiles": { "dynamicRegistration": false }
                },
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "didSave": true,
                    },
                    "completion": {
                        "completionItem": { "snippetSupport": false }
                    },
                    "hover": {
                        "contentFormat": ["plaintext", "markdown"]
                    },
                    "definition": { "dynamicRegistration": false },
                    "references": { "dynamicRegistration": false },
                    "typeDefinition": { "dynamicRegistration": false },
                    "implementation": { "dynamicRegistration": false },
                    "publishDiagnostics": {},
                    "codeAction": {
                        "dynamicRegistration": false,
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": [
                                    "", "quickfix", "refactor",
                                    "refactor.extract", "refactor.inline", "refactor.rewrite",
                                    "source", "source.organizeImports", "source.fixAll"
                                ]
                            }
                        },
                        "resolveSupport": { "properties": ["edit"] }
                    }
                },
                "notebookDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "executionSummarySupport": false
                    }
                }
            }
        });
        self.send_request("initialize", params, PendingKind::Initialize)
    }

    pub fn send_initialized(&mut self) {
        self.send_notification("initialized", json!({}));
    }

    pub fn did_open(&mut self, uri: &str, language_id: &str, text: &str) {
        let v = self.doc_versions.entry(uri.to_owned()).or_insert(0);
        *v = 1;
        let version = *v;
        self.send_notification("textDocument/didOpen", json!({
            "textDocument": {
                "uri": uri,
                "languageId": language_id,
                "version": version,
                "text": text,
            }
        }));
    }

    pub fn did_change(&mut self, uri: &str, text: &str) {
        let v = self.doc_versions.entry(uri.to_owned()).or_insert(0);
        *v += 1;
        let version = *v;
        self.send_notification("textDocument/didChange", json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{ "text": text }]
        }));
    }

    /// True if the server negotiated incremental text sync
    /// (`textDocumentSync` of 2, or `{change: 2}` in the options form).
    pub fn supports_incremental_sync(&self) -> bool {
        let sync = &self.server_capabilities["textDocumentSync"];
        let change = if sync.is_object() { &sync["change"] } else { sync };
        change.as_u64() == Some(2)
    }

    /// Incremental `didChange`: replace the span `start..end` (UTF-16 LSP
    /// positions in the server's current copy) with `text`. Only valid against
    /// servers where `supports_incremental_sync()` is true.
    pub fn did_change_incremental(
        &mut self,
        uri: &str,
        start: (u32, u32),
        end: (u32, u32),
        text: &str,
    ) {
        let v = self.doc_versions.entry(uri.to_owned()).or_insert(0);
        *v += 1;
        let version = *v;
        self.send_notification("textDocument/didChange", json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{
                "range": {
                    "start": { "line": start.0, "character": start.1 },
                    "end":   { "line": end.0,   "character": end.1   },
                },
                "text": text,
            }]
        }));
    }

    /// Open-or-update a full-text-synced document (the notebook shadow doc),
    /// skipping the `didChange` entirely when the content hasn't changed since
    /// the last sync. The shadow doc is rebuilt from the whole notebook before
    /// every hover/signature/references request, but between keystrokes its
    /// content is usually identical — no need to retransmit it.
    pub fn sync_full_doc(&mut self, uri: &str, language_id: &str, text: &str) {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let fingerprint = hasher.finish();

        if self.is_doc_open(uri) {
            if self.full_doc_hashes.get(uri) == Some(&fingerprint) {
                return;
            }
            self.did_change(uri, text);
        } else {
            self.did_open(uri, language_id, text);
        }
        self.full_doc_hashes.insert(uri.to_owned(), fingerprint);
    }

    pub fn did_close(&mut self, uri: &str) {
        self.doc_versions.remove(uri);
        self.full_doc_hashes.remove(uri);
        self.send_notification("textDocument/didClose", json!({
            "textDocument": { "uri": uri }
        }));
    }

    pub fn request_completion(&mut self, uri: &str, line: u32, character: u32) -> u64 {
        self.supersede_pending(PendingKind::Completion);
        self.send_request("textDocument/completion", json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "triggerKind": 1 }
        }), PendingKind::Completion)
    }

    /// Resolve a completion item (fetch its documentation). The request params
    /// are the completion item object echoed back enriched in the response.
    pub fn request_completion_resolve(&mut self, item: Value) -> u64 {
        self.send_request("completionItem/resolve", item, PendingKind::CompletionResolve)
    }

    pub fn request_hover(&mut self, uri: &str, line: u32, character: u32) -> u64 {
        self.supersede_pending(PendingKind::Hover);
        self.send_request("textDocument/hover", json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }), PendingKind::Hover)
    }

    pub fn request_signature_help(&mut self, uri: &str, line: u32, character: u32) -> u64 {
        self.supersede_pending(PendingKind::SignatureHelp);
        self.send_request("textDocument/signatureHelp", json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }), PendingKind::SignatureHelp)
    }

    pub fn request_definition(&mut self, uri: &str, line: u32, character: u32) -> u64 {
        self.send_request("textDocument/definition", json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }), PendingKind::Definition)
    }

    pub fn request_references(&mut self, uri: &str, line: u32, character: u32) -> u64 {
        self.send_request("textDocument/references", json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": true }
        }), PendingKind::References)
    }

    pub fn request_type_definition(&mut self, uri: &str, line: u32, character: u32) -> u64 {
        self.send_request("textDocument/typeDefinition", json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }), PendingKind::TypeDefinition)
    }

    pub fn request_implementation(&mut self, uri: &str, line: u32, character: u32) -> u64 {
        self.send_request("textDocument/implementation", json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }), PendingKind::Implementation)
    }

    pub fn request_code_actions(
        &mut self,
        uri: &str,
        start_line: u32,
        start_char: u32,
        end_line: u32,
        end_char: u32,
    ) -> u64 {
        self.send_request("textDocument/codeAction", json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": start_line, "character": start_char },
                "end":   { "line": end_line,   "character": end_char   }
            },
            "context": { "diagnostics": [] }
        }), PendingKind::CodeAction)
    }

    pub fn request_formatting(&mut self, uri: &str, tab_size: u32, insert_spaces: bool) -> u64 {
        self.send_request("textDocument/formatting", json!({
            "textDocument": { "uri": uri },
            "options": {
                "tabSize": tab_size,
                "insertSpaces": insert_spaces,
            }
        }), PendingKind::Formatting)
    }

    pub fn execute_command(&mut self, command: &str, args: serde_json::Value) {
        self.send_request("workspace/executeCommand", json!({
            "command": command,
            "arguments": args,
        }), PendingKind::ExecuteCommand);
    }

    /// True if `uri` has been opened on this client (via `did_open` or `notebook_did_open`).
    pub fn is_doc_open(&self, uri: &str) -> bool {
        self.doc_versions.contains_key(uri)
    }

    /// True if the server advertised `notebookDocumentSync` in its capabilities.
    pub fn supports_notebook_sync(&self) -> bool {
        self.server_capabilities
            .get("notebookDocumentSync")
            .map(|v| !v.is_null())
            .unwrap_or(false)
    }

    pub fn notebook_did_open(&mut self, notebook_uri: &str, version: i32, cells: &[NotebookCell]) {
        // Track the notebook itself so `is_doc_open(notebook_uri)` answers whether
        // THIS server has the notebook open (notebook sync is per-server state).
        self.doc_versions.insert(notebook_uri.to_owned(), version);
        for cell in cells.iter().filter(|c| c.kind == 2) {
            self.doc_versions.insert(cell.uri.clone(), 1);
        }
        self.send_notification(
            "notebookDocument/didOpen",
            notebook_did_open_params(notebook_uri, version, cells),
        );
    }

    pub fn notebook_did_change_cell(
        &mut self,
        notebook_uri: &str,
        nb_version: i32,
        cell_uri: &str,
        text: &str,
    ) {
        let cell_v = self.doc_versions.entry(cell_uri.to_owned()).or_insert(0);
        *cell_v += 1;
        let cell_version = *cell_v;
        self.send_notification("notebookDocument/didChange", json!({
            "notebookDocument": {
                "uri": notebook_uri,
                "version": nb_version,
            },
            "change": {
                "cells": {
                    "textContent": [{
                        "document": {
                            "uri": cell_uri,
                            "version": cell_version,
                        },
                        "changes": [{"text": text}],
                    }]
                }
            }
        }));
    }

    pub fn notebook_did_close(&mut self, notebook_uri: &str, cell_uris: &[String]) {
        self.doc_versions.remove(notebook_uri);
        let cell_docs: Vec<Value> = cell_uris
            .iter()
            .map(|uri| {
                self.doc_versions.remove(uri);
                json!({"uri": uri})
            })
            .collect();
        self.send_notification("notebookDocument/didClose", json!({
            "notebookDocument": {"uri": notebook_uri},
            "cellTextDocuments": cell_docs,
        }));
    }

    pub fn send_request(&mut self, method: &str, params: Value, kind: PendingKind) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_message(msg);
        self.pending.insert(id, kind);
        id
    }

    pub fn send_notification(&mut self, method: &str, params: Value) {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(msg);
    }

    /// Hand the message to the writer thread (serialization + pipe I/O happen
    /// there, off the UI thread). Ordering is preserved by the channel.
    fn write_message(&mut self, msg: Value) {
        if let Some(writer) = &self.writer {
            let _ = writer.send(msg);
        }
    }

    /// Drop any still-pending request of `kind` and tell the server to stop
    /// working on it. High-frequency requests (completion, signature-help)
    /// supersede their predecessor: only the newest response can be shown, so
    /// a stale request is pure queue latency ahead of the one that matters.
    fn supersede_pending(&mut self, kind: PendingKind) {
        let stale: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, k)| **k == kind)
            .map(|(&id, _)| id)
            .collect();
        for id in stale {
            self.pending.remove(&id);
            self.send_notification("$/cancelRequest", json!({ "id": id }));
        }
    }

    /// Non-blocking drain of all pending server messages.
    pub fn poll(&mut self) -> Vec<ServerMessage> {
        let mut msgs = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            msgs.push(msg);
        }
        msgs
    }

}

impl Drop for LspClient {
    fn drop(&mut self) {
        if self.initialized {
            self.send_notification("exit", json!(null));
        }
        // Close the channel and wait for the writer thread to flush queued
        // messages (including the exit notification) before killing the child.
        self.writer = None;
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.join();
        }
        let _ = self.child.kill();
    }
}

/// Owns the server's stdin: serializes and writes each queued message, exiting
/// when the channel closes (client dropped).
fn writer_thread(mut stdin: BufWriter<ChildStdin>, rx: Receiver<Value>) {
    while let Ok(msg) = rx.recv() {
        let body = serde_json::to_string(&msg).unwrap_or_default();
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let _ = stdin.write_all(header.as_bytes());
        let _ = stdin.write_all(body.as_bytes());
        let _ = stdin.flush();
    }
}

// ---------------------------------------------------------------------------
// Background reader thread
// ---------------------------------------------------------------------------

fn reader_thread(
    mut reader: BufReader<std::process::ChildStdout>,
    tx: Sender<ServerMessage>,
) {
    use std::io::Read;
    loop {
        // Parse Content-Length from HTTP-style headers
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break; // blank line separates headers from body
            }
            if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
                if let Ok(n) = rest.trim().parse::<usize>() {
                    content_length = Some(n);
                }
            }
        }

        let len = match content_length {
            Some(n) if n > 0 => n,
            _ => continue,
        };

        let mut body = vec![0u8; len];
        if reader.read_exact(&mut body).is_err() {
            return;
        }

        let val: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(msg) = parse_message(val) {
            if tx.send(msg).is_err() {
                return;
            }
        }
    }
}

fn parse_message(val: Value) -> Option<ServerMessage> {
    if val.get("result").is_some() || val.get("error").is_some() {
        let id = val.get("id")?.as_u64()?;
        Some(ServerMessage::Response {
            id,
            result: val.get("result").cloned(),
            error: val.get("error").cloned(),
        })
    } else { val.get("method").and_then(|m| m.as_str()).map(|method| ServerMessage::Notification {
            method: method.to_owned(),
            params: val.get("params").cloned(),
        }) }
}

/// Build the `notebookDocument/didOpen` params.
///
/// Markup (markdown/raw) cells are omitted from BOTH `cells` and
/// `cellTextDocuments`: servers negotiate notebook sync with a cell selector
/// (pylsp's is `cells: [{language: "python"}]`), so non-matching cells must not
/// be transmitted at all. Listing a cell without its backing text document
/// crashes pylsp's notebook handling (`cell_document.line_count` on `None`),
/// which kills every subsequent request against the notebook.
fn notebook_did_open_params(notebook_uri: &str, version: i32, cells: &[NotebookCell]) -> Value {
    let code_cells: Vec<&NotebookCell> = cells.iter().filter(|c| c.kind == 2).collect();
    let nb_cells: Vec<Value> = code_cells
        .iter()
        .map(|c| json!({"kind": c.kind, "document": c.uri}))
        .collect();
    let cell_docs: Vec<Value> = code_cells
        .iter()
        .map(|c| {
            json!({
                "uri": c.uri,
                "languageId": c.language_id,
                "version": 1,
                "text": c.text,
            })
        })
        .collect();
    json!({
        "notebookDocument": {
            "uri": notebook_uri,
            "notebookType": "jupyter-notebook",
            "version": version,
            "cells": nb_cells,
        },
        "cellTextDocuments": cell_docs,
    })
}

// ---------------------------------------------------------------------------
// Position helpers
// ---------------------------------------------------------------------------

/// Resolve a path to the absolute, canonicalized form used as a document's LSP
/// identity.  Falls back to the plain absolute path when the file does not exist
/// on disk (e.g. virtual notebook-cell paths), so the result is still stable.
pub fn resolve_path(path: &std::path::Path) -> std::path::PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    };
    abs.canonicalize().unwrap_or(abs)
}

/// Percent-encode a filesystem path for use in a `file://` URI.
/// Keeps RFC 3986 unreserved characters and `/`; encodes everything else
/// (spaces, non-ASCII, …) byte-wise as `%XX`.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decode `%XX` escapes in a URI path back to bytes (lossy on invalid UTF-8).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // `.get` also guards against non-char-boundary slices after `%`.
            if let Some(b) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Convert a filesystem path to an LSP `file://` URI (percent-encoded).
pub fn path_to_uri(path: &std::path::Path) -> String {
    format!(
        "file://{}",
        percent_encode_path(&resolve_path(path).to_string_lossy())
    )
}

/// String key under which diagnostics for `path` are stored.
///
/// Diagnostics arrive keyed by the URI the server echoes back, which is the
/// output of [`path_to_uri`].  Readers must resolve their local path the same
/// way or the lookup silently misses (the cause of diagnostics not showing for
/// files/notebooks opened via a relative path).
pub fn diagnostic_key(path: &std::path::Path) -> String {
    resolve_path(path).to_string_lossy().into_owned()
}

pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    uri.strip_prefix("file://")
        .map(|p| std::path::PathBuf::from(percent_decode(p)))
}

/// Convert a rope char-index to an LSP `(line, character)` position.
pub fn char_to_lsp_pos(rope: &ropey::Rope, char_idx: usize) -> (u32, u32) {
    let char_idx = char_idx.min(rope.len_chars());
    let line = rope.char_to_line(char_idx);
    let line_start = rope.line_to_char(line);
    (line as u32, (char_idx - line_start) as u32)
}

/// Like `char_to_lsp_pos` but with the column in UTF-16 code units — the LSP
/// default position encoding. Incremental `didChange` ranges splice the
/// server's copy of the document, so they must be exact: char-unit columns
/// would corrupt the server's text on lines containing astral-plane
/// characters (emoji etc.), where one `char` is two UTF-16 units.
pub fn char_to_lsp_pos_utf16(rope: &ropey::Rope, char_idx: usize) -> (u32, u32) {
    let char_idx = char_idx.min(rope.len_chars());
    let line = rope.char_to_line(char_idx);
    let line_start = rope.line_to_char(line);
    let col: usize = rope
        .slice(line_start..char_idx)
        .chars()
        .map(char::len_utf16)
        .sum();
    (line as u32, col as u32)
}

/// Advance an LSP position over `text` (UTF-16 columns), giving the end
/// position of a span that starts at `start` and contains exactly `text`.
pub fn advance_lsp_pos_utf16(start: (u32, u32), text: &str) -> (u32, u32) {
    let (mut line, mut col) = (start.0, start.1 as usize);
    for c in text.chars() {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += c.len_utf16();
        }
    }
    (line, col as u32)
}

/// Convert an LSP `(line, character)` position to a rope char-index.
pub fn lsp_pos_to_char(rope: &ropey::Rope, line: usize, character: usize) -> usize {
    if line >= rope.len_lines() {
        return rope.len_chars();
    }
    (rope.line_to_char(line) + character).min(rope.len_chars())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Markup cells must be omitted from BOTH `cells` and `cellTextDocuments`.
    /// Listing a cell whose document is not transmitted crashes pylsp's
    /// notebook handling and kills every request against the notebook.
    #[test]
    fn notebook_did_open_omits_markup_cells_entirely() {
        let cells = vec![
            NotebookCell {
                kind: 1,
                uri: "file:///nb__cell0.py".into(),
                language_id: "markdown".into(),
                text: "# heading".into(),
            },
            NotebookCell {
                kind: 2,
                uri: "file:///nb__cell1.py".into(),
                language_id: "python".into(),
                text: "import math".into(),
            },
        ];
        let params = notebook_did_open_params("file:///nb.ipynb", 1, &cells);
        let nb_cells = params["notebookDocument"]["cells"].as_array().unwrap();
        let cell_docs = params["cellTextDocuments"].as_array().unwrap();
        assert_eq!(nb_cells.len(), 1);
        assert_eq!(cell_docs.len(), 1, "every listed cell must be backed by a document");
        assert_eq!(nb_cells[0]["document"], "file:///nb__cell1.py");
        assert_eq!(cell_docs[0]["uri"], "file:///nb__cell1.py");
    }

    /// The key readers compute (`diagnostic_key`) must equal the key the store
    /// side derives from the server-echoed URI (`uri_to_path(path_to_uri(p))`),
    /// otherwise diagnostics lookups silently miss.  This must hold whether the
    /// path is given relative or absolute, and for virtual cell paths that do
    /// not exist on disk.
    fn store_key(path: &std::path::Path) -> String {
        uri_to_path(&path_to_uri(path))
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn diagnostic_key_matches_store_for_existing_file() {
        let dir = std::env::temp_dir().join("sv_lsp_key_test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("real.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        assert_eq!(diagnostic_key(&file), store_key(&file));
    }

    #[test]
    fn diagnostic_key_resolves_relative_to_absolute() {
        // A relative path must resolve to the same key as its absolute form,
        // so `sv foo.py` and an absolute open of the same file agree.
        let cwd = std::env::current_dir().unwrap();
        let rel = std::path::Path::new("Cargo.toml");
        assert_eq!(diagnostic_key(rel), diagnostic_key(&cwd.join("Cargo.toml")));
    }

    #[test]
    fn diagnostic_key_matches_store_for_virtual_cell_path() {
        // Virtual notebook-cell paths never exist on disk; the key must still
        // round-trip through the URI transform.
        let vpath = std::env::temp_dir().join("sv_nb__cell0.py");
        assert_eq!(diagnostic_key(&vpath), store_key(&vpath));
    }

    #[test]
    fn utf16_positions_count_utf16_units_not_chars() {
        // "a😀b" — the emoji is one char but two UTF-16 units.
        let rope = ropey::Rope::from_str("a😀b\nxyz");
        assert_eq!(char_to_lsp_pos_utf16(&rope, 0), (0, 0));
        assert_eq!(char_to_lsp_pos_utf16(&rope, 1), (0, 1)); // before 😀
        assert_eq!(char_to_lsp_pos_utf16(&rope, 2), (0, 3)); // after 😀 (2 units)
        assert_eq!(char_to_lsp_pos_utf16(&rope, 3), (0, 4)); // after b
        assert_eq!(char_to_lsp_pos_utf16(&rope, 5), (1, 1)); // second line
        // char-based converter would disagree after the emoji:
        assert_eq!(char_to_lsp_pos(&rope, 2), (0, 2));
    }

    #[test]
    fn advance_lsp_pos_over_text() {
        assert_eq!(advance_lsp_pos_utf16((3, 5), "ab"), (3, 7));
        assert_eq!(advance_lsp_pos_utf16((3, 5), "a\nbc"), (4, 2));
        assert_eq!(advance_lsp_pos_utf16((3, 5), "😀"), (3, 7));
        assert_eq!(advance_lsp_pos_utf16((3, 5), "\n"), (4, 0));
    }

    /// A new completion request supersedes a still-pending one: the stale id
    /// is dropped from `pending` (its response will be ignored) and at most
    /// one completion request is ever in flight per server.
    #[test]
    fn completion_request_supersedes_pending() {
        // `cat` stands in for a server process; we only inspect client state.
        let mut client = LspClient::start("cat", &[]).expect("spawn cat");
        let first = client.request_completion("file:///t.py", 0, 0);
        let second = client.request_completion("file:///t.py", 0, 1);
        let completions: Vec<u64> = client
            .pending
            .iter()
            .filter(|(_, k)| **k == PendingKind::Completion)
            .map(|(&id, _)| id)
            .collect();
        assert_eq!(completions, vec![second]);
        assert!(!client.pending.contains_key(&first));
    }

    #[test]
    fn incremental_sync_capability_detection() {
        let mut client = LspClient::start("cat", &[]).expect("spawn cat");
        client.server_capabilities = json!({ "textDocumentSync": 2 });
        assert!(client.supports_incremental_sync());
        client.server_capabilities = json!({ "textDocumentSync": { "change": 2 } });
        assert!(client.supports_incremental_sync());
        client.server_capabilities = json!({ "textDocumentSync": 1 });
        assert!(!client.supports_incremental_sync());
        client.server_capabilities = json!({ "textDocumentSync": { "change": 1 } });
        assert!(!client.supports_incremental_sync());
        client.server_capabilities = json!({});
        assert!(!client.supports_incremental_sync());
    }

    #[test]
    fn uri_round_trips_spaces_and_non_ascii() {
        let p = std::env::temp_dir().join("my nötebook (v2).ipynb");
        let uri = path_to_uri(&p);
        // The URI itself must not contain raw spaces (breaks LSP servers).
        assert!(!uri.contains(' '), "uri must be percent-encoded: {uri}");
        // …and must decode back to the same path / diagnostic key.
        assert_eq!(uri_to_path(&uri).unwrap(), resolve_path(&p));
        assert_eq!(diagnostic_key(&p), store_key(&p));
    }
}
