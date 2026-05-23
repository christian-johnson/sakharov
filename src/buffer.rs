use anyhow::{Context, Result};
use ropey::Rope;
use std::collections::VecDeque;
use std::path::PathBuf;

const MAX_UNDO: usize = 200;

/// A text buffer backed by a `ropey::Rope` with undo/redo support.
pub struct Buffer {
    pub rope: Rope,
    pub path: Option<PathBuf>,
    pub modified: bool,
    /// Undo stack: each entry is the full rope state before an edit.
    /// Capped at MAX_UNDO entries; oldest entries are evicted first.
    undo_stack: VecDeque<Rope>,
    /// Redo stack: states pushed when undo is performed.
    redo_stack: Vec<Rope>,
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
        Ok(Self {
            rope: Rope::from_str(&text),
            path: Some(path),
            modified: false,
            undo_stack: VecDeque::new(),
            redo_stack: Vec::new(),
        })
    }

    /// Return the total number of characters in the buffer.
    #[allow(dead_code)]
    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    /// Return the total number of lines.
    #[allow(dead_code)]
    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// Save the current rope state for undo before making an edit.
    fn push_undo(&mut self) {
        self.undo_stack.push_back(self.rope.clone());
        if self.undo_stack.len() > MAX_UNDO {
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
            if self.undo_stack.len() > MAX_UNDO {
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
    /// # Errors
    /// Returns an error if writing fails.
    pub fn save(&mut self, override_path: Option<&str>) -> Result<()> {
        let path = if let Some(p) = override_path {
            let pb = PathBuf::from(p);
            self.path = Some(pb.clone());
            pb
        } else {
            self.path
                .clone()
                .context("no file path — use :w <path>")?
        };

        let text = self.rope.to_string();
        std::fs::write(&path, text)
            .with_context(|| format!("failed to write {}", path.display()))?;
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
