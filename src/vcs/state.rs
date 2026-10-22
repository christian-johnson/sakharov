//! What the version-control view has on screen: the snapshot, the plan built
//! on it, and the cursors.  The selected file's diff (or its conflicts) is
//! derived here once per change, so the renderer only reads.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use ratatui::layout::Rect;

use super::{
    layout::{scroll_to, FileDiff, Layout},
    plan::{Plan, Step},
    store::Store,
    tree::{self, Row},
    FileState, Mark, Repo, Target,
};
use crate::conflict::{self, state::ConflictState, Choice};

/// A conflicted file's choices and their undo history.
type Answers = (Vec<Choice>, Vec<(usize, Choice)>);

/// Which panel the keys act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Diff,
    Plan,
}

pub struct VcsState {
    pub root: PathBuf,
    pub store: Store,
    /// The repository as it was loaded.
    pub snapshot: Repo,
    pub plan: Plan,
    pub focus: Focus,
    pub collapsed: HashSet<String>,
    /// The tree row under the cursor, by path.
    pub selected: Option<String>,
    pub tree_scroll: usize,
    /// The change under the cursor in the diff.
    pub change: usize,
    pub diff_scroll: usize,
    /// Scroll the diff to the change cursor on the next frame.  Off after
    /// the user pages through the diff by hand.
    pub follow: bool,
    pub plan_cursor: usize,
    pub plan_scroll: usize,
    /// The selected file's diff, when it is a file and not conflicted.
    pub diff: Option<FileDiff>,
    /// Every conflicted file in the projection, with the answers given.
    pub conflicts: Option<ConflictState>,
    /// A commit picked for `L t`, waiting for the tag's name.
    pub tag_target: Option<Target>,
    /// Bumped whenever the projection changes.
    generation: u64,
    diff_key: Option<(String, u64)>,
    conflict_key: Option<u64>,
}

impl VcsState {
    pub fn new(root: PathBuf, snapshot: Repo) -> Self {
        let mut state = VcsState {
            store: Store::new(root.clone()),
            root,
            snapshot,
            plan: Plan::default(),
            focus: Focus::Tree,
            collapsed: HashSet::new(),
            selected: None,
            tree_scroll: 0,
            change: 0,
            diff_scroll: 0,
            follow: true,
            plan_cursor: 0,
            plan_scroll: 0,
            diff: None,
            conflicts: None,
            tag_target: None,
            generation: 0,
            diff_key: None,
            conflict_key: None,
        };
        state.sync();
        state
    }

    /// The repository as the plan leaves it — what every panel draws.
    pub fn repo(&self) -> &Repo {
        self.plan.current(&self.snapshot)
    }

    pub fn rows(&self) -> Vec<Row> {
        tree::rows(self.repo(), &self.store, &self.collapsed)
    }

    pub fn selected_index(&self, rows: &[Row]) -> Option<usize> {
        let path = self.selected.as_ref()?;
        rows.iter().position(|r| &r.path == path)
    }

    pub fn selected_row(&self) -> Option<Row> {
        let rows = self.rows();
        self.selected_index(&rows).map(|i| rows[i].clone())
    }

    /// The selected file and its versions, when a file is selected.
    pub fn selected_file(&self) -> Option<(&str, &FileState)> {
        let path = self.selected.as_ref()?;
        self.repo().files.get_key_value(path).map(|(p, f)| (p.as_str(), f))
    }

    /// True when the main panel is showing the resolver.
    pub fn showing_conflict(&self) -> bool {
        self.selected_file().is_some_and(|(_, f)| f.index.is_conflict())
    }

    /// Add a step to the plan.
    pub fn push(&mut self, step: Step) -> Result<(), String> {
        self.plan.push(&self.store, &self.snapshot, step)?;
        self.changed();
        Ok(())
    }

    pub fn pop(&mut self) -> Option<Step> {
        let step = self.plan.pop();
        self.changed();
        step
    }

    /// Take out the step under the plan cursor.
    pub fn remove_step(&mut self) -> Result<Step, String> {
        let step = self.plan.remove(&self.store, &self.snapshot, self.plan_cursor)?;
        self.plan_cursor = self.plan_cursor.min(self.plan.len().saturating_sub(1));
        self.changed();
        Ok(step)
    }

    pub fn clear_plan(&mut self) {
        self.plan.clear();
        self.changed();
    }

    /// Swap in a freshly loaded snapshot, keeping every step that still
    /// applies.  Returns the ones that were dropped, and why.
    pub fn reload(&mut self, snapshot: Repo) -> Vec<(Step, String)> {
        self.snapshot = snapshot;
        self.store = Store::new(self.root.clone());
        let dropped = self.plan.replay(&self.store, &self.snapshot);
        self.plan_cursor = self.plan_cursor.min(self.plan.len().saturating_sub(1));
        self.changed();
        dropped
    }

    fn changed(&mut self) {
        self.generation += 1;
        self.sync();
    }

    /// Bring the selection, the diff and the conflicts up to date with the
    /// projection.  Cheap when nothing changed.
    pub fn sync(&mut self) {
        let rows = self.rows();
        if self.selected_index(&rows).is_none() {
            // Where the old selection would have sorted, or else the first file.
            let at = match &self.selected {
                Some(p) => rows.iter().filter(|r| &r.path < p).count(),
                None => rows.iter().position(|r| !r.dir).unwrap_or(0),
            };
            self.selected = rows.get(at.min(rows.len().saturating_sub(1))).map(|r| r.path.clone());
        }
        let any_conflict = self.repo().files.values().any(|f| f.index.is_conflict());
        if !any_conflict {
            self.conflicts = None;
            self.conflict_key = None;
        } else if self.conflict_key != Some(self.generation) {
            self.rebuild_conflicts();
        }
        if let (Some(c), Some(path)) = (self.conflicts.as_mut(), self.selected.as_ref()) {
            if let Some(i) = c.set.files.iter().position(|f| &f.path == path) {
                if c.file != i {
                    c.file = i;
                    c.nth = c.current().and_then(|f| f.choices.iter().position(|ch| !ch.is_decided())).unwrap_or(0);
                    c.scroll = 0;
                }
            }
        }

        let key = self.selected_file().filter(|(_, f)| !f.index.is_conflict()).map(|(p, _)| (p.to_string(), self.generation));
        if key != self.diff_key {
            let same_file = key.as_ref().map(|k| &k.0) == self.diff_key.as_ref().map(|k| &k.0);
            self.diff = self.selected_file().filter(|(_, f)| !f.index.is_conflict()).map(|(p, f)| FileDiff::build(&self.store, p, f));
            if !same_file {
                self.change = 0;
                self.diff_scroll = 0;
                self.follow = true;
            }
            self.diff_key = key;
        }
        let changes = self.diff.as_ref().map_or(0, |d| d.changes.len());
        self.change = self.change.min(changes.saturating_sub(1));
        if self.diff.is_none() && self.focus == Focus::Diff && !self.showing_conflict() {
            self.focus = Focus::Tree;
        }
    }

    /// Re-read the conflicted files, carrying over answers by path.
    fn rebuild_conflicts(&mut self) {
        let show_base = self.conflicts.as_ref().is_some_and(|c| c.show_base);
        let set = conflict::load::build(&self.store, self.repo());
        let old: HashMap<String, Answers> = self
            .conflicts
            .take()
            .map(|c| c.set.files.into_iter().map(|f| (f.path, (f.choices, f.history))).collect())
            .unwrap_or_default();
        let mut state = ConflictState::new(set);
        state.show_base = show_base;
        for file in &mut state.set.files {
            if let Some((choices, history)) = old.get(&file.path) {
                if choices.len() == file.choices.len() {
                    file.choices = choices.clone();
                    file.history = history.clone();
                }
            }
        }
        self.conflicts = Some(state);
        self.conflict_key = Some(self.generation);
    }

    /// Move the tree cursor by `delta` rows.
    pub fn step_tree(&mut self, delta: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let at = self.selected_index(&rows).unwrap_or(0) as isize;
        let next = (at + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.select(rows[next].path.clone());
    }

    pub fn select(&mut self, path: String) {
        if self.selected.as_ref() != Some(&path) {
            self.selected = Some(path);
            self.sync();
        }
    }

    /// Move to the next or previous *file*, opening folded directories.
    pub fn step_file(&mut self, forward: bool) -> bool {
        let paths: Vec<&String> = self.repo().files.keys().collect();
        if paths.is_empty() {
            return false;
        }
        let ordered = tree::rows(self.repo(), &self.store, &HashSet::new());
        let files: Vec<&Row> = ordered.iter().filter(|r| !r.dir).collect();
        let at = self.selected.as_ref().and_then(|p| files.iter().position(|r| &r.path == p));
        let next = match (at, forward) {
            (None, _) => 0,
            (Some(i), true) if i + 1 < files.len() => i + 1,
            (Some(i), false) if i > 0 => i - 1,
            _ => return false,
        };
        let path = files[next].path.clone();
        self.collapsed.retain(|dir| !path.starts_with(&format!("{dir}/")));
        self.select(path);
        true
    }

    /// Fold or unfold the selected directory.  Returns false on a file.
    pub fn toggle_fold(&mut self, open: Option<bool>) -> bool {
        let Some(row) = self.selected_row().filter(|r| r.dir) else { return false };
        let fold = open.map_or(!row.collapsed, |o| !o);
        if fold {
            self.collapsed.insert(row.path);
        } else {
            self.collapsed.remove(&row.path);
        }
        true
    }

    /// Move the diff cursor to another change.
    pub fn step_change(&mut self, delta: isize) -> bool {
        let n = self.diff.as_ref().map_or(0, |d| d.changes.len());
        if n == 0 {
            return false;
        }
        let next = (self.change as isize + delta).clamp(0, n as isize - 1) as usize;
        let moved = next != self.change;
        self.change = next;
        self.follow = true;
        moved
    }

    pub fn step_plan(&mut self, delta: isize) {
        let n = self.plan.len();
        if n > 0 {
            self.plan_cursor = (self.plan_cursor as isize + delta).clamp(0, n as isize - 1) as usize;
        }
    }

    /// The `(staged, unstaged, conflicted)` counts of the projection.
    pub fn counts(&self) -> (usize, usize, usize) {
        self.repo().counts(&self.store)
    }

    /// The selected path's mark, for messages.
    pub fn selected_mark(&self) -> Option<Mark> {
        self.selected_row().map(|r| r.mark)
    }

    /// Keep every cursor on screen.  Called once per frame.
    pub fn update_scroll(&mut self, area: Rect) {
        let layout = Layout::compute(area, self.plan.len());
        let rows = self.rows();
        if let Some(i) = self.selected_index(&rows) {
            self.tree_scroll = scroll_to((i, 1), self.tree_scroll, Layout::inner_rows(layout.tree));
        }
        if !self.plan.is_empty() {
            self.plan_scroll = scroll_to((self.plan_cursor, 1), self.plan_scroll, Layout::inner_rows(layout.plan));
        }
        if let Some(diff) = self.diff.as_ref() {
            let split = layout.split();
            match diff.span(self.change, split) {
                Some(span) if self.follow => self.diff_scroll = scroll_to(span, self.diff_scroll, layout.diff_rows()),
                _ => {}
            }
            let max = diff.rows(split).len().saturating_sub(layout.diff_rows() / 2);
            self.diff_scroll = self.diff_scroll.min(max);
            self.follow = false;
        }
        if let Some(c) = self.conflicts.as_mut() {
            c.update_scroll(layout.main);
        }
    }
}
