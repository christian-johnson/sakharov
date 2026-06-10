use anyhow::{Context, Result};
use ropey::Rope;
use std::collections::VecDeque;
use std::path::PathBuf;

static MAX_UNDO: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Set the undo history limit.  Call once at startup before any buffer is used.
/// Subsequent calls (e.g. on config reload) are silently ignored, which is
/// intentional — changing the limit mid-session can corrupt existing stacks.
pub fn configure_max_undo(n: usize) {
    let _ = MAX_UNDO.set(n);
}

fn max_undo() -> usize {
    *MAX_UNDO.get().unwrap_or(&200)
}

/// A text buffer backed by a `ropey::Rope` with undo/redo support.
pub struct Buffer {
    pub rope: Rope,
    pub path: Option<PathBuf>,
    pub modified: bool,
    /// Undo stack: each entry is the full rope state before an edit.
    /// Capped at `max_undo()` entries; oldest entries are evicted first.
    undo_stack: VecDeque<Rope>,
    /// Redo stack: states pushed when undo is performed.
    redo_stack: Vec<Rope>,
    /// The file's mtime as of the last load/save.  Used to detect external
    /// modification before overwriting on save.  `None` for new/unsaved files.
    disk_mtime: Option<std::time::SystemTime>,
}

/// Read a path's mtime, or `None` when it doesn't exist / can't be statted.
fn mtime_of(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Atomically replace `path` with `text`: write to a temp file in the same
/// directory, fsync, preserve the target's permissions, then rename over it.
/// A crash mid-save can never leave a truncated file behind.
pub(crate) fn atomic_write(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("file"));
    name.push(format!(".sv-tmp{}", std::process::id()));
    let tmp = path.with_file_name(name);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // Keep the target's permissions (a plain create would reset them).
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

impl Buffer {
    /// Create an empty scratch buffer.
    pub fn new_empty() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            modified: false,
            undo_stack: VecDeque::new(),
            redo_stack: Vec::new(),
            disk_mtime: None,
        }
    }

    /// Load a buffer from the given file path.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read.
    pub fn from_path(path: &str) -> Result<Self> {
        let path = PathBuf::from(path);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let disk_mtime = mtime_of(&path);
        Ok(Self {
            rope: Rope::from_str(&text),
            path: Some(path),
            modified: false,
            undo_stack: VecDeque::new(),
            redo_stack: Vec::new(),
            disk_mtime,
        })
    }

    /// Re-stat the backing file and record its mtime.  Call after an external
    /// program (e.g. a shell formatter) legitimately rewrote the file and the
    /// buffer was reloaded from it.
    pub fn refresh_disk_mtime(&mut self) {
        self.disk_mtime = self.path.as_deref().and_then(mtime_of);
    }

    /// Save the current rope state for undo before making an edit.
    fn push_undo(&mut self) {
        self.undo_stack.push_back(self.rope.clone());
        if self.undo_stack.len() > max_undo() {
            self.undo_stack.pop_front();
        }
        self.redo_stack.clear();
    }

    /// Snapshot current state for undo, then insert `text` at char position `pos`.
    pub fn insert(&mut self, pos: usize, text: &str) {
        self.push_undo();
        self.rope.insert(pos, text);
        self.modified = true;
    }

    /// Insert without creating an undo snapshot (use inside an already-open edit session).
    pub fn insert_raw(&mut self, pos: usize, text: &str) {
        self.rope.insert(pos, text);
        self.modified = true;
    }

    /// Snapshot current state for undo, then remove chars in `[start, end)`.
    pub fn remove(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        self.push_undo();
        self.rope.remove(start..end);
        self.modified = true;
    }

    /// Remove without creating an undo snapshot (use inside an already-open edit session).
    pub fn remove_raw(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        self.rope.remove(start..end);
        self.modified = true;
    }

    /// Explicitly open an undo snapshot. Call once on the first edit of an insert session.
    pub fn begin_edit_session(&mut self) {
        self.push_undo();
    }

    /// Undo the last edit. Returns true if an undo was available.
    pub fn undo(&mut self) -> bool {
        if let Some(prev) = self.undo_stack.pop_back() {
            self.redo_stack.push(self.rope.clone());
            self.rope = prev;
            self.modified = true;
            true
        } else {
            false
        }
    }

    /// Redo the last undone edit. Returns true if a redo was available.
    pub fn redo(&mut self) -> bool {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push_back(self.rope.clone());
            if self.undo_stack.len() > max_undo() {
                self.undo_stack.pop_front();
            }
            self.rope = next;
            self.modified = true;
            true
        } else {
            false
        }
    }

    /// Save the buffer to its path, or to `override_path` if given.
    ///
    /// Refuses to overwrite when the file changed on disk since it was loaded
    /// (unless `force`), so an external edit is never silently clobbered.
    ///
    /// # Errors
    /// Returns an error if writing fails or the file was externally modified.
    pub fn save(&mut self, override_path: Option<&str>, force: bool) -> Result<()> {
        let path = if let Some(p) = override_path {
            let pb = PathBuf::from(p);
            self.path = Some(pb.clone());
            pb
        } else {
            self.path
                .clone()
                .context("no file path — use :w <path>")?
        };

        // External-modification check applies only when writing back to the
        // file this buffer was loaded from.
        if !force && override_path.is_none() {
            if let Some(loaded) = self.disk_mtime {
                if mtime_of(&path).is_some_and(|now| now != loaded) {
                    anyhow::bail!(
                        "{} changed on disk since it was loaded — :w! to overwrite",
                        path.display()
                    );
                }
            }
        }

        let text = self.rope.to_string();
        atomic_write(&path, &text)
            .with_context(|| format!("failed to write {}", path.display()))?;
        self.disk_mtime = mtime_of(&path);
        self.modified = false;
        Ok(())
    }

    /// Return a displayable name for the buffer.
    pub fn display_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[scratch]".to_string())
    }
}
