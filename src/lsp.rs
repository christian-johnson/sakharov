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
#[derive(Debug, Clone)]
pub enum PendingKind {
    Initialize,
    Completion,
    Hover,
    Definition,
    References,
    TypeDefinition,
    Implementation,
    CodeAction,
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
    stdin: BufWriter<ChildStdin>,
    rx: Receiver<ServerMessage>,
    child: Child,
    next_id: u64,
    pub pending: HashMap<u64, PendingKind>,
    pub initialized: bool,
    pub server_capabilities: Value,
    doc_versions: HashMap<String, i32>,
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

        Ok(Self {
            stdin: BufWriter::new(stdin),
            rx,
            child,
            next_id: 1,
            pending: HashMap::new(),
            initialized: false,
            server_capabilities: Value::Null,
            doc_versions: HashMap::new(),
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

    pub fn did_close(&mut self, uri: &str) {
        self.doc_versions.remove(uri);
        self.send_notification("textDocument/didClose", json!({
            "textDocument": { "uri": uri }
        }));
    }

    pub fn request_completion(&mut self, uri: &str, line: u32, character: u32) -> u64 {
        self.send_request("textDocument/completion", json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "triggerKind": 1 }
        }), PendingKind::Completion)
    }

    pub fn request_hover(&mut self, uri: &str, line: u32, character: u32) -> u64 {
        self.send_request("textDocument/hover", json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }), PendingKind::Hover)
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
        let nb_cells: Vec<Value> = cells
            .iter()
            .map(|c| json!({"kind": c.kind, "document": c.uri}))
            .collect();

        // Only code cells get a text-document entry; markup cells have no LSP content.
        let cell_docs: Vec<Value> = cells
            .iter()
            .filter(|c| c.kind == 2)
            .map(|c| {
                self.doc_versions.insert(c.uri.clone(), 1);
                json!({
                    "uri": c.uri,
                    "languageId": c.language_id,
                    "version": 1,
                    "text": c.text,
                })
            })
            .collect();

        self.send_notification("notebookDocument/didOpen", json!({
            "notebookDocument": {
                "uri": notebook_uri,
                "notebookType": "jupyter-notebook",
                "version": version,
                "cells": nb_cells,
            },
            "cellTextDocuments": cell_docs,
        }));
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
        self.write_message(&msg);
        self.pending.insert(id, kind);
        id
    }

    pub fn send_notification(&mut self, method: &str, params: Value) {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&msg);
    }

    fn write_message(&mut self, msg: &Value) {
        let body = serde_json::to_string(msg).unwrap_or_default();
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let _ = self.stdin.write_all(header.as_bytes());
        let _ = self.stdin.write_all(body.as_bytes());
        let _ = self.stdin.flush();
    }

    /// Non-blocking drain of all pending server messages.
    pub fn poll(&mut self) -> Vec<ServerMessage> {
        let mut msgs = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            msgs.push(msg);
        }
        msgs
    }

    #[allow(dead_code)]
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if self.initialized {
            self.send_notification("exit", json!(null));
        }
        let _ = self.child.kill();
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
    } else if let Some(method) = val.get("method").and_then(|m| m.as_str()) {
        Some(ServerMessage::Notification {
            method: method.to_owned(),
            params: val.get("params").cloned(),
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Position helpers
// ---------------------------------------------------------------------------

/// Convert a filesystem path to an LSP `file://` URI.
pub fn path_to_uri(path: &std::path::Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    };
    let resolved = abs.canonicalize().unwrap_or(abs);
    format!("file://{}", resolved.display())
}

pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    uri.strip_prefix("file://").map(std::path::PathBuf::from)
}

/// Convert a rope char-index to an LSP `(line, character)` position.
pub fn char_to_lsp_pos(rope: &ropey::Rope, char_idx: usize) -> (u32, u32) {
    let char_idx = char_idx.min(rope.len_chars());
    let line = rope.char_to_line(char_idx);
    let line_start = rope.line_to_char(line);
    (line as u32, (char_idx - line_start) as u32)
}

/// Convert an LSP `(line, character)` position to a rope char-index.
pub fn lsp_pos_to_char(rope: &ropey::Rope, line: usize, character: usize) -> usize {
    if line >= rope.len_lines() {
        return rope.len_chars();
    }
    (rope.line_to_char(line) + character).min(rope.len_chars())
}
