//! Compute sessions: persistent Python processes, owned by the editor rather
//! than by any one view.
//!
//! A kernel used to hang off `Notebook`, which made Python reachable only while
//! a notebook was open.  It lives on [`App`](crate::app::App) instead so that a
//! grid, a plot, or a variable explorer can ask the interpreter the same
//! questions the notebook does.
//!
//! * [`KernelSession`] — the process and its stdio: spawn, write a request,
//!   drain replies.  Knows nothing about cells.
//! * [`ComputeSession`] — the editor's handle on one: assigns request ids and
//!   remembers which [`Consumer`] is waiting on each, so a reply is routed to
//!   whoever asked instead of to whatever the editor last did.
//! * [`ComputePool`] — all of them, keyed by whose they are, plus which one is
//!   *active* (the one a view with no kernel of its own talks to).
//!
//! **One kernel per notebook**, as in Jupyter, keyed by the notebook's
//! [`SourceId`].  Sharing one process across notebooks would let a `df` defined
//! in one silently answer a cell in another — wrong in the quiet way, with no
//! error and only the numbers to give it away.  A view with no notebook behind
//! it (a query result, a CSV grid) uses the session in focus, so "explore the
//! dataframe I just built" reaches the namespace that built it, and falls back
//! to a project-level session keyed by a virtual id.
//!
//! **Invariant:** sessions are owned by `App` and only ever *borrowed* by a
//! view.  Every consumer must tolerate one being absent, busy, or restarted
//! between frames — a view may not cache a handle to it across frames.

use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use anyhow::{Context, Result};
use base64::Engine as _;
use serde_json::Value;

use crate::source::SourceId;

/// The kernel runner, embedded in the binary and run with `python -c`.
///
/// The protocol, one request at a time:
///
/// ```text
/// __KI_REQ__{"id":7,"kind":"exec","tag":"cell-a3f"}   editor → kernel
/// <code lines>
/// __KI_CODE_END__
///
/// {"id":7,"t":"stream","name":"stdout","text":"…"}    kernel → editor
/// {"id":7,"t":"done"}
/// ```
const RUNNER_SCRIPT: &str = include_str!("runner.py");

// ---------------------------------------------------------------------------
// Requests and their consumers
// ---------------------------------------------------------------------------

/// What the editor is asking the kernel to do.
///
/// `kind` is a protocol field, so a new request type is additive on both sides.
#[derive(Debug, Clone)]
pub enum RequestKind {
    /// Execute a block of code.  `tag` becomes the compile filename, so a
    /// traceback frame reports `File "<tag>", line N` — the editor maps the tag
    /// back to a jump target.
    Exec { tag: String },
    /// List the namespace's variables (the variable explorer).
    Vars,
    /// Write the dataframe bound to `var` out to `path` as parquet, so the grid
    /// can open it.  `var` is a *bound name*, validated as an identifier by the
    /// caller — never an expression, so nothing typed in the editor is eval'd.
    Export { var: String, path: PathBuf },
}

impl RequestKind {
    fn name(&self) -> &'static str {
        match self {
            Self::Exec { .. } => "exec",
            Self::Vars => "vars",
            Self::Export { .. } => "export",
        }
    }

    fn tag(&self) -> Option<&str> {
        match self {
            Self::Exec { tag } => Some(tag),
            Self::Vars => None,
            Self::Export { var, .. } => Some(var),
        }
    }

    /// Where the reply should be written, for the request kinds that produce a
    /// file rather than a stream.
    fn path(&self) -> Option<&Path> {
        match self {
            Self::Export { path, .. } => Some(path),
            _ => None,
        }
    }
}

/// One entry of the kernel's namespace, as the variable explorer shows it.
///
/// Type, shape and whether it can be opened as a grid — deliberately nothing
/// that materialises a value.  Introspecting a live object already risks running
/// user code through a property; asking for as little as possible is the only
/// available mitigation.
#[derive(Debug, Clone)]
pub struct VarInfo {
    pub name: String,
    pub type_name: String,
    /// `"1200 x 8"`, a length, or empty when the object has no shape.
    pub shape: String,
    /// Whether `:view` can open it as a grid.
    pub viewable: bool,
}

impl VarInfo {
    fn from_json(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_owned();
        Self {
            name: s("name"),
            type_name: s("type"),
            shape: s("shape"),
            viewable: v.get("viewable").and_then(Value::as_bool).unwrap_or(false),
        }
    }
}

/// Who is waiting on a reply.
///
/// The kernel echoes a request's id on every message of its reply, and this is
/// what that id resolves to.  Routing by consumer rather than by "whatever the
/// editor last started" is what lets a non-notebook view use the kernel at all.
#[derive(Debug, Clone)]
pub enum Consumer {
    /// A notebook cell, by its **stable id** — not its index, which a
    /// structural edit can shift while the cell is still running.
    NotebookCell(String),
    /// The variable explorer, waiting for a listing.
    VariableList,
    /// `:view <name>`, waiting for that variable to be written out.
    ViewVariable(String),
}

// ---------------------------------------------------------------------------
// Kernel session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelStatus {
    /// Spawned but still booting — waiting for the `__KI_READY__` handshake.
    Starting,
    Idle,
    Busy,
    Dead,
}

/// One incremental message from the kernel, produced by the reader thread and
/// drained on the main thread via [`KernelSession::poll`].
///
/// `id` is the request the message belongs to (0 for the boot handshake and for
/// the process dying, which belong to no request).
pub struct KernelMessage {
    pub id: u64,
    pub body: MessageBody,
}

pub enum MessageBody {
    /// The kernel finished booting and sent `__KI_READY__`.
    Ready,
    /// A chunk of stdout/stderr text, emitted as the request produces it.
    Stream { name: String, text: String },
    /// A captured matplotlib figure (decoded PNG bytes).
    Image { png: Vec<u8> },
    /// An uncaught exception traceback.
    Error { traceback: String },
    /// The namespace's variables, in reply to [`RequestKind::Vars`].
    Vars(Vec<VarInfo>),
    /// A variable was written out, in reply to [`RequestKind::Export`].
    Export { path: PathBuf, rows: Option<usize> },
    /// The request finished.
    Done,
    /// The kernel process exited / closed its stdout.
    Dead,
}

pub struct KernelSession {
    child: std::process::Child,
    stdin: std::io::BufWriter<std::process::ChildStdin>,
    /// Messages from the background reader thread (drained by `poll`).
    rx: Receiver<KernelMessage>,
    pub execution_count: u32,
    pub status: KernelStatus,
    /// The interpreter this kernel runs (for status/log messages).
    pub python: String,
}

impl KernelSession {
    /// Spawn a persistent Python kernel running the runner script.
    ///
    /// Returns immediately with status [`KernelStatus::Starting`]; the
    /// background reader thread sends [`MessageBody::Ready`] once the kernel
    /// prints `__KI_READY__`, so a slow Python boot (venv, matplotlib import)
    /// never blocks the UI.
    pub fn new(python: &str, cwd: &Path) -> Result<Self> {
        use std::process::{Command, Stdio};

        let mut child = Command::new(python)
            .arg("-c")
            .arg(RUNNER_SCRIPT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(cwd)
            .spawn()
            .with_context(|| format!("spawning kernel python executable '{python}'"))?;

        let stdin = child
            .stdin
            .take()
            .context("kernel child process has no stdin")?;
        let stdout_raw = child
            .stdout
            .take()
            .context("kernel child process has no stdout")?;

        let stdin = std::io::BufWriter::new(stdin);
        let stdout = std::io::BufReader::new(stdout_raw);

        // The reader thread performs the __KI_READY__ handshake and then
        // parses the JSON message stream; nothing here blocks on the child.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || reader_thread(stdout, tx));

        Ok(Self {
            child,
            stdin,
            rx,
            execution_count: 0,
            status: KernelStatus::Starting,
            python: python.to_owned(),
        })
    }

    /// Write a request and return immediately.  Replies arrive later as
    /// [`KernelMessage`]s via [`poll`](Self::poll), each stamped with `id`; the
    /// kernel is marked busy until a `Done` for it is observed.
    fn send(&mut self, id: u64, kind: &RequestKind, code: &str) -> Result<()> {
        let mut header = serde_json::json!({ "id": id, "kind": kind.name() });
        if let Some(tag) = kind.tag() {
            header["tag"] = Value::String(tag.to_owned());
        }
        if let Some(path) = kind.path() {
            header["path"] = Value::String(path.to_string_lossy().into_owned());
        }
        self.stdin.write_all(b"__KI_REQ__")?;
        self.stdin.write_all(header.to_string().as_bytes())?;
        self.stdin.write_all(b"\n")?;
        for line in code.lines() {
            self.stdin.write_all(line.as_bytes())?;
            self.stdin.write_all(b"\n")?;
        }
        self.stdin.write_all(b"__KI_CODE_END__\n")?;
        self.stdin.flush()?;
        self.status = KernelStatus::Busy;
        Ok(())
    }

    /// Non-blocking drain of all messages the reader thread has queued.
    pub fn poll(&mut self) -> Vec<KernelMessage> {
        let mut msgs = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            msgs.push(msg);
        }
        msgs
    }

    /// Send SIGINT to the child process (Unix/macOS).
    pub fn interrupt(&self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGINT);
        }
    }

    /// Returns `true` if the kernel process is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for KernelSession {
    fn drop(&mut self) {
        // Killing the child closes its stdout, so the reader thread sees EOF
        // and exits on its own.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Background thread: wait for the `__KI_READY__` startup handshake (sending
/// `Ready`), then parse one JSON message per line from the kernel and forward
/// it to the session. Exits (sending `Dead`) when stdout closes.
fn reader_thread(
    mut reader: std::io::BufReader<std::process::ChildStdout>,
    tx: std::sync::mpsc::Sender<KernelMessage>,
) {
    let bare = |body| KernelMessage { id: 0, body };
    let mut line = String::new();
    // Handshake phase: scan for the ready marker, skipping any noise the
    // interpreter prints while booting.
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                let _ = tx.send(bare(MessageBody::Dead));
                return;
            }
            Ok(_) => {}
        }
        if line.trim() == "__KI_READY__" {
            if tx.send(bare(MessageBody::Ready)).is_err() {
                return; // session dropped
            }
            break;
        }
    }
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                let _ = tx.send(bare(MessageBody::Dead));
                return;
            }
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        // Lines that aren't our framed JSON (e.g. raw output from a subprocess
        // the cell spawned) are skipped rather than crashing the stream.
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let body = match v.get("t").and_then(|t| t.as_str()) {
            Some("stream") => MessageBody::Stream {
                name: v.get("name").and_then(|n| n.as_str()).unwrap_or("stdout").to_owned(),
                text: v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_owned(),
            },
            Some("image") => {
                match v.get("data").and_then(|d| d.as_str())
                    .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                {
                    Some(png) => MessageBody::Image { png },
                    None => continue,
                }
            }
            Some("error") => MessageBody::Error {
                traceback: v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_owned(),
            },
            Some("vars") => {
                let items = v
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().map(VarInfo::from_json).collect())
                    .unwrap_or_default();
                MessageBody::Vars(items)
            }
            Some("export") => {
                let Some(path) = v.get("path").and_then(|p| p.as_str()) else {
                    continue;
                };
                let rows = v
                    .get("rows")
                    .and_then(Value::as_i64)
                    .and_then(|n| (n >= 0).then_some(n as usize));
                MessageBody::Export { path: PathBuf::from(path), rows }
            }
            Some("done") => MessageBody::Done,
            _ => continue,
        };
        let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
        if tx.send(KernelMessage { id, body }).is_err() {
            return; // session dropped
        }
    }
}

// ---------------------------------------------------------------------------
// Compute session
// ---------------------------------------------------------------------------

/// The editor's handle on the kernel: the process, the directory its
/// interpreter was resolved against, and who is waiting on each request.
pub struct ComputeSession {
    pub kernel: KernelSession,
    /// The directory the venv search started from.  A view under a *different*
    /// root wants a different interpreter, so [`ComputeSession::serves`] is what
    /// decides whether this session can be reused or has to be replaced.
    root: PathBuf,
    next_id: u64,
    pending: HashMap<u64, Consumer>,
}

impl ComputeSession {
    /// Spawn a kernel for `root`, resolving its interpreter from `root`'s
    /// virtualenv.  Returns the session and whether a venv was actually found —
    /// when it wasn't, the caller should say so, since the system interpreter
    /// probably isn't what the user's code expects.
    pub fn start(root: &Path) -> Result<(Self, bool)> {
        let (python, found_venv) = find_python_executable(root);
        let kernel = KernelSession::new(&python, root)?;
        Ok((
            Self {
                kernel,
                root: root.to_path_buf(),
                next_id: 1,
                pending: HashMap::new(),
            },
            found_venv,
        ))
    }

    /// Whether this session's interpreter is the right one for `root`.  A
    /// notebook opened in another project resolves a different venv, so its
    /// kernel has to be replaced rather than reused.
    pub fn serves(&self, root: &Path) -> bool {
        self.root == root
    }

    /// The directory this session's interpreter was resolved against — what a
    /// restart should resolve against again.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn status(&self) -> &KernelStatus {
        &self.kernel.status
    }

    pub fn is_idle(&self) -> bool {
        self.kernel.status == KernelStatus::Idle
    }

    /// Issue a request on `consumer`'s behalf.  The returned id is what the
    /// kernel will stamp on every message of the reply.
    pub fn request(&mut self, kind: RequestKind, code: &str, consumer: Consumer) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.kernel.send(id, &kind, code)?;
        self.pending.insert(id, consumer);
        Ok(id)
    }

    /// The consumer waiting on `id`, if any.  A reply whose id is unknown is
    /// from a request that has already been retired (or from before a restart)
    /// and must be dropped, not applied to whatever is in view now.
    pub fn consumer(&self, id: u64) -> Option<&Consumer> {
        self.pending.get(&id)
    }

    /// Retire `id` — called when its `Done` arrives.
    pub fn finish(&mut self, id: u64) -> Option<Consumer> {
        self.pending.remove(&id)
    }

    /// Abandon every in-flight request.  The kernel died or is being restarted,
    /// so nothing that was asked will ever be answered.
    pub fn abandon_all(&mut self) -> Vec<Consumer> {
        self.pending.drain().map(|(_, c)| c).collect()
    }
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

/// Every live compute session, keyed by whose it is.
///
/// A notebook's key is its own [`SourceId`] (the `.ipynb` file), so opening a
/// second notebook gets a second interpreter rather than inheriting the first
/// one's variables.  A view with no notebook behind it keys by a virtual id —
/// `SourceId` already distinguishes "a file" from "a thing with no file", which
/// is exactly the distinction needed here.
#[derive(Default)]
pub struct ComputePool {
    sessions: HashMap<SourceId, ComputeSession>,
    /// Whose kernel a view without one of its own talks to — the last notebook
    /// worked in.  `None` until something has started a kernel.
    active: Option<SourceId>,
}

impl ComputePool {
    /// The session a view with no kernel of its own should use.
    pub fn active(&self) -> Option<&ComputeSession> {
        self.sessions.get(self.active.as_ref()?)
    }

    pub fn active_key(&self) -> Option<&SourceId> {
        self.active.as_ref()
    }

    pub fn get(&self, key: &SourceId) -> Option<&ComputeSession> {
        self.sessions.get(key)
    }

    pub fn get_mut(&mut self, key: &SourceId) -> Option<&mut ComputeSession> {
        self.sessions.get_mut(key)
    }

    /// Mark `key`'s session as the one in focus, if it exists.
    pub fn focus(&mut self, key: &SourceId) {
        if self.sessions.contains_key(key) {
            self.active = Some(key.clone());
        }
    }

    /// Make sure `key` has a live kernel resolved against `root`, spawning one
    /// if it is missing or dead, and focus it.
    ///
    /// `Ok(Some(started))` means a process was spawned — `started` is whether a
    /// venv was found, which the caller should surface, since the system
    /// interpreter probably isn't what the user's code expects.  `Ok(None)`
    /// means an existing session was reused.
    pub fn ensure(&mut self, key: &SourceId, root: &Path) -> Result<Option<bool>> {
        let alive = self
            .sessions
            .get_mut(key)
            .is_some_and(|s| s.serves(root) && s.kernel.is_alive());
        if alive {
            self.active = Some(key.clone());
            return Ok(None);
        }
        // A dead session, or one resolved against a root that has since moved,
        // is replaced rather than kept alongside.
        self.sessions.remove(key);
        let (session, found_venv) = ComputeSession::start(root)?;
        self.sessions.insert(key.clone(), session);
        self.active = Some(key.clone());
        Ok(Some(found_venv))
    }

    /// Shut down `key`'s kernel (killing the process) and forget it.  Called
    /// when its notebook is closed — otherwise a closed buffer leaves a Python
    /// process running for the rest of the session.
    pub fn shutdown(&mut self, key: &SourceId) -> bool {
        let existed = self.sessions.remove(key).is_some();
        if self.active.as_ref() == Some(key) {
            self.active = self.sessions.keys().next().cloned();
        }
        existed
    }

    /// Drain every session's queued messages, tagged with whose session they
    /// came from.  Polling *all* of them (not just the active one) is what lets
    /// a cell in a notebook you have navigated away from still finish.
    pub fn poll_all(&mut self) -> Vec<(SourceId, Vec<KernelMessage>)> {
        self.sessions
            .iter_mut()
            .filter_map(|(key, session)| {
                let msgs = session.kernel.poll();
                (!msgs.is_empty()).then(|| (key.clone(), msgs))
            })
            .collect()
    }

    /// True when any kernel is booting or running something — what the status
    /// spinner means by "there is background work".
    pub fn any_busy(&self) -> bool {
        self.sessions
            .values()
            .any(|s| matches!(s.kernel.status, KernelStatus::Starting | KernelStatus::Busy))
    }
}

// ---------------------------------------------------------------------------
// Python interpreter resolution
// ---------------------------------------------------------------------------

/// Find the best Python executable for the given directory.
/// Checks common virtual-environment layouts (.venv, venv, .env, env) by
/// walking up the directory tree from the notebook's directory (and, as a
/// fallback, from the current working directory), then falls back to the
/// system `python3`. Walking up matters because a notebook commonly lives in
/// a subdirectory of the project whose venv is at the project root — this is
/// the same ancestor search the LSP uses (`lsp_manager::detect_python_venv`),
/// so the kernel and LSP agree on which interpreter the project uses.
///
/// Returns `(python_path, found_venv)`. When `found_venv` is false the
/// caller should warn the user that the system python3 is being used.
pub fn find_python_executable(base: &Path) -> (String, bool) {
    // Search `base` and its ancestors first (most specific to the notebook),
    // then the cwd and its ancestors as a fallback.
    let mut roots = vec![base.to_path_buf()];
    if let Ok(cwd) = std::env::current_dir() {
        if cwd != base {
            roots.push(cwd);
        }
    }

    for root in &roots {
        if let Some(python) = venv_python_up(root) {
            return (python.to_string_lossy().into_owned(), true);
        }
    }

    ("python3".to_string(), false)
}

/// Walk up the directory tree from `start` looking for a project virtualenv
/// (`.venv`/`venv`/`.env`/`env`); return the path to its python interpreter.
///
/// This is the single venv discovery used by **both** the kernel
/// (`find_python_executable`) and the Python language server
/// (`lsp_manager::ensure_server`), so the code the user runs and the
/// environment jedi resolves against are always the same interpreter.
pub fn venv_python_up(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if let Some(python) = venv_python_in(d) {
            return Some(python);
        }
        dir = d.parent();
    }
    None
}

/// If `dir` directly contains a recognised virtualenv layout, return the path
/// to its python interpreter.
fn venv_python_in(dir: &Path) -> Option<PathBuf> {
    for name in [".venv", "venv", ".env", "env"] {
        let venv = dir.join(name);
        // Unix layout (python3 preferred), then Windows (bin → Scripts).
        for rel in ["bin/python3", "bin/python", "Scripts/python.exe"] {
            let candidate = venv.join(rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a real kernel to completion, returning every message it sent.
    fn run_to_done(session: &mut ComputeSession, code: &str, consumer: Consumer) -> (u64, Vec<KernelMessage>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut collected = Vec::new();
        // Wait for the boot handshake before sending: the runner reads stdin
        // only once it is up, and `request` would mark it busy prematurely.
        while !session.is_idle() {
            for msg in session.kernel.poll() {
                if matches!(msg.body, MessageBody::Ready) {
                    session.kernel.status = KernelStatus::Idle;
                }
                collected.push(msg);
            }
            assert!(std::time::Instant::now() < deadline, "kernel never became ready");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let id = session
            .request(RequestKind::Exec { tag: "cell-tag".into() }, code, consumer)
            .expect("request written");
        loop {
            let msgs = session.kernel.poll();
            let done = msgs.iter().any(|m| matches!(m.body, MessageBody::Done));
            collected.extend(msgs);
            if done {
                return (id, collected);
            }
            assert!(std::time::Instant::now() < deadline, "kernel execution timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn every_reply_carries_the_id_of_the_request_that_asked() {
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 not available — skipping kernel protocol test");
            return;
        }
        let root = std::env::temp_dir();
        let (mut session, _) = ComputeSession::start(&root).expect("kernel spawned");

        let consumer = Consumer::NotebookCell("cell-a3f".into());
        let (id, msgs) = run_to_done(&mut session, "print('hello')", consumer);

        // Routing depends on this: an unstamped reply would be applied to
        // whatever the editor happened to be doing last.
        for msg in &msgs {
            match msg.body {
                // The boot handshake belongs to no request.
                MessageBody::Ready | MessageBody::Dead => assert_eq!(msg.id, 0),
                _ => assert_eq!(msg.id, id, "reply not stamped with its request id"),
            }
        }
        assert!(
            msgs.iter().any(|m| matches!(&m.body, MessageBody::Stream { text, .. } if text.contains("hello"))),
            "the cell's output should have come back",
        );

        // The consumer is resolvable while the request is open, and gone once
        // it is retired — a late reply must not be applied to a stale target.
        assert!(matches!(session.consumer(id), Some(Consumer::NotebookCell(c)) if c == "cell-a3f"));
        assert!(matches!(session.finish(id), Some(Consumer::NotebookCell(_))));
        assert!(session.consumer(id).is_none());
        assert!(session.consumer(id + 99).is_none(), "an unknown id must not resolve");
    }

    #[test]
    fn a_session_is_reused_only_for_the_root_it_serves() {
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 not available — skipping kernel protocol test");
            return;
        }
        let root = std::env::temp_dir();
        let (session, _) = ComputeSession::start(&root).expect("kernel spawned");
        assert!(session.serves(&root));
        // A different project resolves a different venv, so its kernel must not
        // be silently reused with the wrong interpreter.
        assert!(!session.serves(&root.join("other-project")));
    }
}
