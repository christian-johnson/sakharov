//! Live project grep: the popup's query is ripgrep's pattern, re-run (debounced)
//! as the user types. Results stream in from a reader thread, capped at
//! [`MAX_HITS`], so a huge tree never blocks the editor or floods memory.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use crate::app::App;
use crate::popup::{ListItem, ListState, PopupContent};

/// Results kept per query; past this the user is asked to refine.
const MAX_HITS: usize = 10_000;
/// Quiet time after the last keystroke before a new search starts.
const DEBOUNCE: Duration = Duration::from_millis(120);
/// How long the reader holds hits before sending a batch.
const BATCH_WINDOW: Duration = Duration::from_millis(40);
/// Longest line content shown (ripgrep truncates past this).
const MAX_COLUMNS: &str = "300";

/// The live-grep popup's search state, polled once per frame by the run loop
/// (see [`poll`]). Dropped when its popup closes, which kills any search.
pub struct LiveGrep {
    root: PathBuf,
    tab_width: usize,
    /// The popup's query when last polled, and when it last changed.
    typed: String,
    typed_at: Instant,
    /// The query the current results are for.
    query: String,
    search: Option<Search>,
    hits: usize,
    status: Status,
}

enum Status {
    Idle,
    Searching,
    Done,
    Capped,
    Failed(String),
}

/// One running `rg`/`grep` process and the channel its reader thread fills.
struct Search {
    child: Child,
    rx: Receiver<Msg>,
}

impl Drop for Search {
    fn drop(&mut self) {
        // Already exited (finished or SIGPIPE after the cap) is fine.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

enum Msg {
    Hits(Vec<Hit>),
    /// The reader stopped: `capped` when it hit [`MAX_HITS`], with whatever the
    /// tool wrote to stderr.
    Done { capped: bool, stderr: String },
}

#[derive(Debug, PartialEq)]
struct Hit {
    /// Relative to the search root.
    file: String,
    /// 1-based, as the tools print them.
    line: usize,
    col: usize,
    content: String,
}

/// Open the live project grep, searching under the current buffer's directory
/// (or the working directory for a path-less buffer).
pub(super) fn grep_project(app: &mut App) {
    let root = app
        .buffer
        .path
        .as_deref()
        .and_then(Path::parent)
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    open(app, root);
}

fn open(app: &mut App, root: PathBuf) {
    let mut popup = crate::popup::Popup::grep("", Vec::new(), app.search.query.clone());
    if let PopupContent::List(ref mut list) = popup.content {
        list.live = true;
    }
    let job = LiveGrep {
        root,
        tab_width: app.config.editor.tab_width,
        typed: String::new(),
        typed_at: Instant::now(),
        query: String::new(),
        search: None,
        hits: 0,
        status: Status::Idle,
    };
    popup.title = Some(job.title());
    app.popup = Some(popup);
    app.jobs.grep = Some(job);
}

/// True while a search process is running.
pub fn busy(app: &App) -> bool {
    app.jobs.grep.as_ref().is_some_and(|g| g.search.is_some())
}

/// Follow the popup's query and pull in streamed results. Returns true when
/// the popup changed (the caller should redraw).
pub fn poll(app: &mut App) -> bool {
    let Some(job) = app.jobs.grep.as_mut() else { return false };
    let Some(popup) = app.popup.as_mut() else {
        app.jobs.grep = None;
        return false;
    };
    let PopupContent::List(list) = &mut popup.content else {
        app.jobs.grep = None;
        return false;
    };
    if !list.live {
        app.jobs.grep = None;
        return false;
    }

    let mut changed = false;
    if list.filter != job.typed {
        job.typed.clone_from(&list.filter);
        job.typed_at = Instant::now();
    }
    if job.typed != job.query && job.typed_at.elapsed() >= DEBOUNCE {
        job.restart(list);
        changed = true;
    }
    changed |= job.drain(list);
    if changed {
        popup.title = Some(job.title());
    }
    changed
}

impl LiveGrep {
    /// Drop the old results and search, and start one for the typed query.
    fn restart(&mut self, list: &mut ListState) {
        self.search = None;
        list.items.clear();
        list.selected = 0;
        list.invalidate_filter_cache();
        self.hits = 0;
        self.query.clone_from(&self.typed);
        if self.query.is_empty() {
            self.status = Status::Idle;
            return;
        }
        match spawn(&self.root, &self.query) {
            Ok(search) => {
                self.search = Some(search);
                self.status = Status::Searching;
            }
            Err(e) => self.status = Status::Failed(format!("could not run grep: {e}")),
        }
    }

    /// Append every batch the reader has sent. True when anything arrived.
    fn drain(&mut self, list: &mut ListState) -> bool {
        let Some(search) = &self.search else { return false };
        let mut changed = false;
        let mut finished = None;
        loop {
            match search.rx.try_recv() {
                Ok(Msg::Hits(hits)) => {
                    self.hits += hits.len();
                    list.items.extend(hits.into_iter().map(|h| self.item(h)));
                    changed = true;
                }
                Ok(Msg::Done { capped, stderr }) => {
                    finished = Some((capped, stderr));
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = Some((false, "grep reader died".into()));
                    break;
                }
            }
        }
        if changed {
            list.invalidate_filter_cache();
        }
        if let Some((capped, stderr)) = finished {
            self.search = None;
            self.status = if capped {
                Status::Capped
            } else if self.hits == 0 && !stderr.trim().is_empty() {
                // Typically an unfinished regex, e.g. `foo(`.
                Status::Failed(stderr.lines().next().unwrap_or_default().to_owned())
            } else {
                Status::Done
            };
            changed = true;
        }
        changed
    }

    fn item(&self, hit: Hit) -> ListItem {
        let content = crate::render_util::sanitize_source(&hit.content, self.tab_width);
        ListItem::navigate(
            content.trim_start(),
            format!("{}:{}", hit.file, hit.line),
            &self.root.join(&hit.file),
            hit.line.saturating_sub(1),
            hit.col.saturating_sub(1),
        )
    }

    fn title(&self) -> String {
        let root = self.root.display();
        match &self.status {
            Status::Idle => format!("grep {root}"),
            Status::Searching => format!("grep {root} · searching… {}", self.hits),
            Status::Done => format!("grep {root} · {} matches", self.hits),
            Status::Capped => format!("grep {root} · {}+ matches, refine the query", self.hits),
            Status::Failed(e) => format!("grep {root} · {e}"),
        }
    }
}

/// Start ripgrep (or `grep -r` without it) for `query` under `root`, with a
/// reader thread streaming its parsed output.
fn spawn(root: &Path, query: &str) -> std::io::Result<Search> {
    let has_rg = rg_is_available();
    let mut cmd = if has_rg {
        let mut c = Command::new("rg");
        c.args([
            "--line-number", "--column", "--no-heading", "--color=never", "--with-filename",
            "--null", "--smart-case", "--no-messages", "--max-columns-preview",
        ])
        .args(["--max-columns", MAX_COLUMNS]);
        c
    } else {
        // No ignore files here, so skip the usual heavyweight directories.
        let mut c = Command::new("grep");
        c.args(["-rnIsE", "--null"]).args(
            [".git", "node_modules", ".venv", "venv", "target", "__pycache__"]
                .map(|d| format!("--exclude-dir={d}")),
        );
        c
    };
    let mut child = cmd
        .arg("-e")
        .arg(query)
        .arg(".")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        unreachable!("both streams were piped");
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || read_hits(stdout, stderr, has_rg, &tx));
    Ok(Search { child, rx })
}

/// Reader thread: parse output lines into batched [`Hit`]s until EOF, the cap,
/// or the receiver going away.
fn read_hits(stdout: ChildStdout, stderr: ChildStderr, has_column: bool, tx: &Sender<Msg>) {
    let mut reader = BufReader::new(stdout);
    let mut raw = Vec::new();
    let mut batch = Vec::new();
    let mut sent_at = Instant::now();
    let mut total = 0;
    let mut capped = false;
    while reader.read_until(b'\n', &mut raw).is_ok_and(|n| n > 0) {
        if let Some(hit) = parse_line(&String::from_utf8_lossy(&raw), has_column) {
            batch.push(hit);
            total += 1;
        }
        raw.clear();
        if total >= MAX_HITS {
            capped = true;
            break;
        }
        if sent_at.elapsed() >= BATCH_WINDOW && !batch.is_empty() {
            if tx.send(Msg::Hits(std::mem::take(&mut batch))).is_err() {
                return;
            }
            sent_at = Instant::now();
        }
    }
    if !batch.is_empty() && tx.send(Msg::Hits(batch)).is_err() {
        return;
    }
    // Closing stdout first lets a capped search die of SIGPIPE.
    drop(reader);
    let mut err = String::new();
    if !capped {
        let _ = stderr.take(4096).read_to_string(&mut err);
    }
    let _ = tx.send(Msg::Done { capped, stderr: err });
}

/// Parse `path\0line[:col]:content` (the `--null` output of rg/grep).
fn parse_line(line: &str, has_column: bool) -> Option<Hit> {
    let (file, rest) = line.split_once('\0')?;
    let (lineno, rest) = rest.split_once(':')?;
    let (col, content) = if has_column {
        let (col, content) = rest.split_once(':')?;
        (col.parse().ok()?, content)
    } else {
        (1, rest)
    };
    Some(Hit {
        file: file.strip_prefix("./").unwrap_or(file).to_owned(),
        line: lineno.parse().ok()?,
        col,
        content: content.trim_end_matches(['\r', '\n']).to_owned(),
    })
}

fn rg_is_available() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| Command::new("rg").arg("--version").output().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::popup::ConfirmPayload;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sv-grep-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn list(app: &App) -> &ListState {
        match &app.popup.as_ref().expect("popup open").content {
            PopupContent::List(l) => l,
            _ => panic!("not a list popup"),
        }
    }

    /// Type `query` into the popup and poll until its search finishes.
    fn search(app: &mut App, query: &str) {
        if let Some(PopupContent::List(l)) = app.popup.as_mut().map(|p| &mut p.content) {
            l.filter = query.to_owned();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            poll(app);
            let job = app.jobs.grep.as_ref().unwrap();
            if job.query == query && job.search.is_none() {
                return;
            }
            assert!(Instant::now() < deadline, "search for {query:?} never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn live_grep_follows_the_query_and_jumps_to_the_match() {
        let root = temp_root("live");
        std::fs::write(root.join("a.py"), "import os\nimport requests\n").unwrap();
        std::fs::write(root.join("odd:name.txt"), "uses requests too\n").unwrap();
        let mut app = App::new(None, crate::config::Config::load());
        open(&mut app, root.clone());

        search(&mut app, "requests");
        let mut hits: Vec<_> = list(&app)
            .items
            .iter()
            .map(|i| (i.label.clone(), i.detail.clone().unwrap()))
            .collect();
        hits.sort();
        assert_eq!(
            hits,
            [
                ("import requests".to_owned(), "a.py:2".to_owned()),
                ("uses requests too".to_owned(), "odd:name.txt:1".to_owned()),
            ]
        );
        let a = list(&app).items.iter().find(|i| i.label == "import requests").unwrap();
        let Some(ConfirmPayload::Navigate { path, line, .. }) = &a.payload else { panic!() };
        assert_eq!((path, *line), (&root.join("a.py"), 1));

        search(&mut app, "import os");
        assert_eq!(list(&app).items.len(), 1);

        search(&mut app, "");
        assert!(list(&app).items.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn results_stop_at_the_cap() {
        let root = temp_root("cap");
        std::fs::write(root.join("big.txt"), "match\n".repeat(MAX_HITS + 500)).unwrap();
        let mut app = App::new(None, crate::config::Config::load());
        open(&mut app, root.clone());

        search(&mut app, "match");

        assert_eq!(list(&app).items.len(), MAX_HITS);
        assert!(matches!(app.jobs.grep.as_ref().unwrap().status, Status::Capped));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn closing_the_popup_drops_the_search() {
        let root = temp_root("close");
        let mut app = App::new(None, crate::config::Config::load());
        open(&mut app, root.clone());

        app.popup = None;
        poll(&mut app);

        assert!(app.jobs.grep.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
