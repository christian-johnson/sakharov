//! Crash recovery for unsaved buffer contents.
//!
//! While a buffer has unsaved edits, its contents are periodically flushed to a
//! private recovery file under `$XDG_STATE_HOME/sakharov/recovery/` (see
//! [`crate::config::state_dir`]).  Each file is created owner-only (`0600`) and
//! written atomically (temp + rename).  Files are keyed by a hash of the
//! buffer's canonical path (or the literal `scratch` for the scratch buffer), so
//! the recovery directory stays tidy — one file per recoverable buffer, no
//! sidecars littered next to the user's files.
//!
//! A recovery file exists *only while there are unsaved edits*: it is removed
//! once the buffer is saved (the buffer is no longer dirty, so the next flush
//! drops it) and when the editor quits cleanly.  Its presence at the next
//! startup therefore signals an unclean exit, and the user is prompted to
//! restore or discard.
//!
//! `SIGKILL` runs no destructors, so the periodic flush is the primary safety
//! net.  As a best-effort extra for panics, the latest desired on-disk state is
//! mirrored into a global snapshot that the panic hook flushes (see
//! [`flush_panic_snapshot`]).

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::app::App;

/// Minimum wall-clock gap between recovery flushes.
const FLUSH_INTERVAL: Duration = Duration::from_millis(1500);
/// Recovery files older than this (by mtime) are GC'd at startup.
const MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;
/// Fixed key for the (path-less) scratch buffer.
const SCRATCH_KEY: &str = "scratch";

// ---------------------------------------------------------------------------
// On-disk record
// ---------------------------------------------------------------------------

/// JSON shape of a single recovery file.
#[derive(Serialize, Deserialize)]
struct RecoveryRecord {
    /// "file" | "notebook" | "scratch"
    kind: String,
    /// Absolute path of the original document (None for scratch).
    original_path: Option<String>,
    /// Unix seconds when this snapshot was taken (for the restore prompt).
    saved_at_unix: u64,
    /// The unsaved buffer contents (plain text, or nbformat JSON for notebooks).
    content: String,
}

/// What kind of buffer a pending recovery applies to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RecoveryKind {
    File,
    Notebook,
    Scratch,
}

/// A recovery offered to the user but not yet acted upon.
pub struct PendingRecovery {
    key: String,
    kind: RecoveryKind,
    /// Original document path (None for scratch).
    path: Option<PathBuf>,
    /// Recovered contents to restore.
    content: String,
    /// When the snapshot was taken (Unix seconds), for the prompt text.
    saved_at_unix: u64,
}

// ---------------------------------------------------------------------------
// Runtime state (lives on App)
// ---------------------------------------------------------------------------

/// Per-session recovery bookkeeping.
pub struct Recovery {
    /// Whether recovery is active (config flag AND a usable state dir exist).
    pub enabled: bool,
    /// `<state>/recovery`, or None if it couldn't be created.
    dir: Option<PathBuf>,
    /// Last flush time, for debouncing.
    last_flush: Option<Instant>,
    /// key → hash of the content last written, to skip redundant disk writes.
    written: HashMap<String, u64>,
}

impl Recovery {
    /// Build recovery state, creating the recovery directory if `enabled`.
    pub fn new(enabled: bool) -> Self {
        let dir = if enabled {
            crate::config::state_dir().and_then(|d| {
                let r = d.join("recovery");
                match std::fs::create_dir_all(&r) {
                    Ok(()) => {
                        crate::config::restrict_dir_permissions(&r);
                        Some(r)
                    }
                    Err(e) => {
                        eprintln!("sv: could not create recovery dir: {e}");
                        None
                    }
                }
            })
        } else {
            None
        };
        Self {
            enabled: enabled && dir.is_some(),
            dir,
            last_flush: None,
            written: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Periodic flush (called once per run-loop frame)
// ---------------------------------------------------------------------------

/// Flush dirty buffers to recovery files, throttled to `FLUSH_INTERVAL`.
/// Also removes recovery files for buffers that are no longer dirty.
pub fn tick(app: &mut App) {
    if !app.recovery.enabled {
        return;
    }
    let dir = match app.recovery.dir.clone() {
        Some(d) => d,
        None => return,
    };
    let now = Instant::now();
    if let Some(last) = app.recovery.last_flush {
        if now.duration_since(last) < FLUSH_INTERVAL {
            return;
        }
    }
    app.recovery.last_flush = Some(now);

    let entries = collect_entries(app);

    // Mirror the desired on-disk state into the panic snapshot so a panic that
    // strikes between flushes still persists the latest contents.
    let snapshot: Vec<(PathBuf, String)> = entries
        .iter()
        .filter_map(|e| {
            serde_json::to_string(&e.record)
                .ok()
                .map(|json| (dir.join(format!("{}.json", e.key)), json))
        })
        .collect();
    set_panic_snapshot(snapshot);

    // Write changed entries; remember which keys are currently dirty.
    let mut live: HashSet<String> = HashSet::new();
    for e in &entries {
        live.insert(e.key.clone());
        let json = match serde_json::to_string(&e.record) {
            Ok(j) => j,
            Err(_) => continue,
        };
        let h = hash_str(&json);
        if app.recovery.written.get(&e.key) == Some(&h) {
            continue; // unchanged since last write
        }
        let target = dir.join(format!("{}.json", e.key));
        if write_private(&target, &json).is_ok() {
            app.recovery.written.insert(e.key.clone(), h);
        }
    }

    // Drop recovery files for buffers this session wrote but that are now clean
    // (saved, or no longer present).  Keeps the directory tidy.
    let stale: Vec<String> = app
        .recovery
        .written
        .keys()
        .filter(|k| !live.contains(*k))
        .cloned()
        .collect();
    for k in stale {
        let _ = std::fs::remove_file(dir.join(format!("{k}.json")));
        app.recovery.written.remove(&k);
    }
}

/// One computed recovery entry awaiting a write.
struct Entry {
    key: String,
    record: RecoveryRecord,
}

/// Gather all in-memory buffers that currently hold unsaved edits.
fn collect_entries(app: &App) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let now = now_unix();

    // Active buffer: a live notebook, or a plain-text/scratch buffer.
    if let Some((nb, _)) = &app.notebook {
        if nb.modified {
            if let Ok(content) = nb.to_nbformat_string() {
                entries.push(Entry {
                    key: path_key(&nb.path),
                    record: RecoveryRecord {
                        kind: "notebook".into(),
                        original_path: Some(abs_string(&nb.path)),
                        saved_at_unix: now,
                        content,
                    },
                });
            }
        }
    } else if let Some(path) = app.buffer.path.as_ref() {
        if path.to_str() == Some("*scratch*") {
            push_scratch(&mut entries, &app.buffer.rope, now);
        } else if !crate::exec::is_special_path(path) && app.buffer.modified {
            entries.push(Entry {
                key: path_key(path),
                record: RecoveryRecord {
                    kind: "file".into(),
                    original_path: Some(abs_string(path)),
                    saved_at_unix: now,
                    content: app.buffer.rope.to_string(),
                },
            });
        }
    }

    // Stashed notebooks (navigated away from but still in memory).
    for (path, (nb, _)) in &app.notebook_buffers {
        let is_active = app
            .notebook
            .as_ref()
            .is_some_and(|(a, _)| same_path(&a.path, path));
        if is_active || !nb.modified {
            continue;
        }
        if let Ok(content) = nb.to_nbformat_string() {
            entries.push(Entry {
                key: path_key(path),
                record: RecoveryRecord {
                    kind: "notebook".into(),
                    original_path: Some(abs_string(path)),
                    saved_at_unix: now,
                    content,
                },
            });
        }
    }

    // Stashed scratch (when scratch isn't the active buffer).
    let scratch_active =
        app.buffer.path.as_deref().and_then(|p| p.to_str()) == Some("*scratch*");
    if !scratch_active {
        if let Some(rope) = app.special_buffer_ropes.get("*scratch*") {
            push_scratch(&mut entries, rope, now);
        }
    }

    entries
}

/// Push a scratch entry only if it differs from the default placeholder text.
fn push_scratch(entries: &mut Vec<Entry>, rope: &ropey::Rope, now: u64) {
    let content = rope.to_string();
    if content == crate::exec::SCRATCH_INTRO || content.trim().is_empty() {
        return;
    }
    entries.push(Entry {
        key: SCRATCH_KEY.to_string(),
        record: RecoveryRecord {
            kind: "scratch".into(),
            original_path: None,
            saved_at_unix: now,
            content,
        },
    });
}

// ---------------------------------------------------------------------------
// Startup scan + GC
// ---------------------------------------------------------------------------

/// At startup: GC stale/orphaned recovery files, and enqueue prompts for the
/// buffer opened on the command line plus the scratch buffer.  Shows the first
/// prompt if any were enqueued.
pub fn startup_scan(app: &mut App) {
    if !app.recovery.enabled {
        return;
    }
    let dir = match app.recovery.dir.clone() {
        Some(d) => d,
        None => return,
    };

    // Identity of the buffer opened on the command line, so we only prompt for
    // it (other orphaned recoveries are left for a later `sv <file>` / on-open).
    let active_key = active_buffer_key(app);

    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for ent in read.flatten() {
        let path = ent.path();
        let ext = path.extension().and_then(|e| e.to_str());
        if ext == Some("tmp") {
            let _ = std::fs::remove_file(&path); // leftover from an interrupted write
            continue;
        }
        if ext != Some("json") {
            continue;
        }
        if older_than(&path, MAX_AGE_SECS) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rec: RecoveryRecord = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(_) => {
                let _ = std::fs::remove_file(&path); // corrupt
                continue;
            }
        };
        let key = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();

        match rec.kind.as_str() {
            "scratch" => {
                if rec.content == crate::exec::SCRATCH_INTRO || rec.content.trim().is_empty() {
                    let _ = std::fs::remove_file(&path);
                } else {
                    enqueue(app, PendingRecovery {
                        key,
                        kind: RecoveryKind::Scratch,
                        path: None,
                        content: rec.content,
                        saved_at_unix: rec.saved_at_unix,
                    });
                }
            }
            kind @ ("file" | "notebook") => {
                let orig = rec.original_path.as_ref().map(PathBuf::from);
                let Some(orig) = orig else {
                    let _ = std::fs::remove_file(&path);
                    continue;
                };
                if !orig.exists() {
                    let _ = std::fs::remove_file(&path); // original is gone
                    continue;
                }
                // Only prompt for the file/notebook opened on the command line.
                if active_key.as_deref() != Some(key.as_str()) {
                    continue;
                }
                let disk = std::fs::read_to_string(&orig).unwrap_or_default();
                if disk == rec.content {
                    let _ = std::fs::remove_file(&path); // nothing new to recover
                    continue;
                }
                enqueue(app, PendingRecovery {
                    key,
                    kind: if kind == "notebook" {
                        RecoveryKind::Notebook
                    } else {
                        RecoveryKind::File
                    },
                    path: Some(orig),
                    content: rec.content,
                    saved_at_unix: rec.saved_at_unix,
                });
            }
            _ => {}
        }
    }

    show_next_prompt(app);
}

/// Check for a recovery file when a file/notebook is (re)opened during a
/// session.  If one exists and differs from what was just loaded from disk,
/// enqueue a prompt; if it matches disk, drop it as clean.
pub fn offer_on_open(app: &mut App, path: &Path) {
    if !app.recovery.enabled || app.popup.is_some() {
        return;
    }
    let dir = match app.recovery.dir.clone() {
        Some(d) => d,
        None => return,
    };
    let key = path_key(path);
    let file = dir.join(format!("{key}.json"));
    let raw = match std::fs::read_to_string(&file) {
        Ok(r) => r,
        Err(_) => return,
    };
    let rec: RecoveryRecord = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(_) => return,
    };
    let is_notebook = rec.kind == "notebook";
    let disk = std::fs::read_to_string(path).unwrap_or_default();
    if disk == rec.content {
        let _ = std::fs::remove_file(&file);
        return;
    }
    enqueue(app, PendingRecovery {
        key,
        kind: if is_notebook {
            RecoveryKind::Notebook
        } else {
            RecoveryKind::File
        },
        path: Some(path.to_path_buf()),
        content: rec.content,
        saved_at_unix: rec.saved_at_unix,
    });
    show_next_prompt(app);
}

// ---------------------------------------------------------------------------
// Prompt / restore / discard
// ---------------------------------------------------------------------------

fn enqueue(app: &mut App, item: PendingRecovery) {
    app.pending_recoveries.push_back(item);
}

/// Show the next queued recovery prompt, if any and no popup is currently open.
pub fn show_next_prompt(app: &mut App) {
    if app.active_recovery.is_some() || app.popup.is_some() {
        return;
    }
    if let Some(item) = app.pending_recoveries.pop_front() {
        let title = prompt_title(&item);
        app.active_recovery = Some(item);
        app.popup = Some(crate::popup::Popup::recovery_prompt(title));
    }
}

/// Handle the user's choice from a recovery prompt (`choice` is "restore" or
/// "discard"), then advance to the next queued prompt.
pub fn handle_choice(app: &mut App, choice: &str) {
    if let Some(item) = app.active_recovery.take() {
        if choice == "restore" {
            apply_restore(app, &item);
        } else {
            discard(app, &item);
        }
    }
    show_next_prompt(app);
}

fn apply_restore(app: &mut App, item: &PendingRecovery) {
    match item.kind {
        RecoveryKind::File => {
            // The file is the active buffer (we only prompt right after opening
            // it).  Replace its contents and mark dirty.
            let matches_active = app
                .buffer
                .path
                .as_ref()
                .is_some_and(|p| same_path(p, item.path.as_deref().unwrap_or(p)));
            if matches_active {
                app.buffer.begin_edit_session();
                let end = app.buffer.rope.len_chars();
                app.buffer.remove_raw(0, end);
                app.buffer.insert_raw(0, &item.content);
                app.selection = crate::selection::Selection::point(0);
                crate::exec::recompute_highlights(app);
                crate::exec::lsp_did_change(app);
                app.message = Some(format!(
                    "Restored unsaved changes ({})",
                    file_label(item)
                ));
            } else {
                app.message =
                    Some("Could not restore: file is no longer the active buffer".into());
            }
        }
        RecoveryKind::Notebook => {
            let matches_active = app
                .notebook
                .as_ref()
                .is_some_and(|(nb, _)| {
                    same_path(&nb.path, item.path.as_deref().unwrap_or(&nb.path))
                });
            let nb_path = item.path.clone().unwrap_or_default();
            match crate::notebook::Notebook::from_json_str(&nb_path, &item.content) {
                Ok(mut nb) if matches_active => {
                    nb.modified = true;
                    // Preserve any running kernel from the freshly-opened notebook.
                    if let Some((old, _)) = app.notebook.as_mut() {
                        nb.kernel = old.kernel.take();
                    }
                    app.notebook = Some((nb, crate::notebook_state::NotebookState::new()));
                    crate::exec::notebook::load_focused_cell(app);
                    crate::exec::recompute_highlights(app);
                    app.message =
                        Some(format!("Restored unsaved notebook ({})", file_label(item)));
                }
                Ok(_) => {
                    app.message = Some(
                        "Could not restore: notebook is no longer the active buffer".into(),
                    );
                }
                Err(e) => {
                    app.message = Some(format!("Could not restore notebook: {e}"));
                }
            }
        }
        RecoveryKind::Scratch => {
            let rope = ropey::Rope::from_str(&item.content);
            app.special_buffer_ropes
                .insert("*scratch*".to_string(), rope.clone());
            if app.buffer.path.as_deref().and_then(|p| p.to_str()) == Some("*scratch*") {
                app.buffer.rope = rope;
                app.buffer.modified = true;
                crate::exec::recompute_highlights(app);
            }
            app.message = Some("Restored scratch buffer (open with :scratch)".into());
        }
    }
}

fn discard(app: &mut App, item: &PendingRecovery) {
    if let Some(dir) = app.recovery.dir.clone() {
        let _ = std::fs::remove_file(dir.join(format!("{}.json", item.key)));
    }
    app.recovery.written.remove(&item.key);
}

// ---------------------------------------------------------------------------
// Clean-exit cleanup + panic snapshot
// ---------------------------------------------------------------------------

/// On a clean quit, remove every recovery file this session created — a clean
/// exit means there is nothing to recover next time.
pub fn cleanup_on_quit(app: &mut App) {
    if let Some(dir) = app.recovery.dir.clone() {
        for key in app.recovery.written.keys() {
            let _ = std::fs::remove_file(dir.join(format!("{key}.json")));
        }
    }
    app.recovery.written.clear();
    set_panic_snapshot(Vec::new());
}

static PANIC_SNAPSHOT: OnceLock<Mutex<Vec<(PathBuf, String)>>> = OnceLock::new();

fn panic_snapshot() -> &'static Mutex<Vec<(PathBuf, String)>> {
    PANIC_SNAPSHOT.get_or_init(|| Mutex::new(Vec::new()))
}

fn set_panic_snapshot(items: Vec<(PathBuf, String)>) {
    if let Ok(mut guard) = panic_snapshot().lock() {
        *guard = items;
    }
}

/// Flush the latest recovery snapshot to disk.  Called from the panic hook, so
/// it must not panic — every operation is best-effort.
pub fn flush_panic_snapshot() {
    if let Ok(guard) = panic_snapshot().lock() {
        for (path, content) in guard.iter() {
            let _ = write_private(path, content);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Atomically write `content` to `path` as an owner-only (`0600`) file.
fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Recovery-file key for a document path: hash of its canonical absolute path.
fn path_key(path: &Path) -> String {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canon.to_string_lossy().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Absolute path as a string (canonical if possible, else as-given).
fn abs_string(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Compare two paths by canonical form (falling back to literal equality).
fn same_path(a: &Path, b: &Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// The recovery key of the buffer opened on the command line, if any.
fn active_buffer_key(app: &App) -> Option<String> {
    if let Some((nb, _)) = &app.notebook {
        return Some(path_key(&nb.path));
    }
    let path = app.buffer.path.as_ref()?;
    if crate::exec::is_special_path(path) {
        return None;
    }
    Some(path_key(path))
}

fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True if `path`'s mtime is more than `max_age` seconds ago.
fn older_than(path: &Path, max_age: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    modified
        .elapsed()
        .map(|age| age.as_secs() > max_age)
        .unwrap_or(false)
}

fn file_label(item: &PendingRecovery) -> String {
    item.path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "scratch".into())
}

fn prompt_title(item: &PendingRecovery) -> String {
    let what = match item.kind {
        RecoveryKind::Scratch => "scratch buffer".to_string(),
        _ => file_label(item),
    };
    let ago = humanize_age(item.saved_at_unix);
    format!("Recover unsaved changes to {what}? (saved {ago})")
}

/// Render "Nm ago" / "Nh ago" / "just now" for the prompt.
fn humanize_age(saved_at_unix: u64) -> String {
    let now = now_unix();
    let secs = now.saturating_sub(saved_at_unix);
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_private_is_atomic_and_owner_only() {
        let dir = std::env::temp_dir().join(format!("sv-rec-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("x.json");

        write_private(&target, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        // The temp file used for the atomic rename must not linger.
        assert!(!target.with_extension("tmp").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "recovery file must be owner-only");
        }

        // Overwriting replaces contents cleanly.
        write_private(&target, "world").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "world");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_key_is_stable_and_distinct() {
        let a = std::env::temp_dir().join("sv-key-a.txt");
        let b = std::env::temp_dir().join("sv-key-b.txt");
        assert_eq!(path_key(&a), path_key(&a), "key is deterministic");
        assert_ne!(path_key(&a), path_key(&b), "distinct paths → distinct keys");
        // Keys are hex of a u64 hash → 16 chars.
        assert_eq!(path_key(&a).len(), 16);
    }

    #[test]
    fn humanize_age_buckets() {
        let now = now_unix();
        assert_eq!(humanize_age(now), "just now");
        assert_eq!(humanize_age(now.saturating_sub(120)), "2m ago");
        assert_eq!(humanize_age(now.saturating_sub(7200)), "2h ago");
        assert_eq!(humanize_age(now.saturating_sub(2 * 86_400)), "2d ago");
    }
}
