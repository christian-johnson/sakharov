//! THE geometry model for the version-control view: where the panels go,
//! and the selected file's diff as rows.  The renderer and the scroll math
//! both read from here.

use ratatui::layout::Rect;

use super::{
    diff::{self, Change, Staged},
    store::Store,
    FileState, MODE_GITLINK,
};

/// Narrowest a diff column may be before the panel switches to one column.
pub const MIN_SIDE: u16 = 30;

/// Rows of the plan panel's content before it stops growing.
const PLAN_MAX: usize = 8;

/// How many lines of one file the diff holds.
const MAX_LINES: usize = 20_000;

/// Where the three panels go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub tree: Rect,
    pub plan: Rect,
    pub main: Rect,
}

impl Layout {
    pub fn compute(area: Rect, plan_steps: usize) -> Layout {
        let left_w = (area.width * 3 / 10).clamp(20.min(area.width), 48.min(area.width));
        let plan_h = (plan_steps.clamp(1, PLAN_MAX) as u16 + 2).min(area.height / 2);
        let tree = Rect { width: left_w, height: area.height.saturating_sub(plan_h), ..area };
        let plan = Rect { y: area.y + tree.height, height: plan_h, width: left_w, ..area };
        let main = Rect { x: area.x + left_w, width: area.width - left_w, ..area };
        Layout { tree, plan, main }
    }

    /// Content rows inside a bordered panel.
    pub fn inner_rows(rect: Rect) -> usize {
        rect.height.saturating_sub(2) as usize
    }

    /// Rows the diff can show (inside the border, below the column header).
    pub fn diff_rows(&self) -> usize {
        self.main.height.saturating_sub(3) as usize
    }

    /// Side by side, or one column when the panel is too narrow.
    pub fn split(&self) -> bool {
        self.main.width.saturating_sub(2) > MIN_SIDE * 2
    }
}

/// One numbered line of a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// 1-based.
    pub no: usize,
    pub text: String,
}

/// One drawn row.  Side by side, `old` and `new` are the two columns; in one
/// column a row has only `old` (removed), only `new` (added), or both
/// (unchanged, drawn once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub old: Option<Line>,
    pub new: Option<Line>,
    /// Which change the row belongs to.
    pub change: Option<usize>,
}

/// One change, where it is drawn, and whether it is staged.
#[derive(Debug, Clone)]
pub struct ChangeInfo {
    pub change: Change,
    pub staged: Staged,
    /// `(first row, row count)` side by side, and in one column.
    pub split: (usize, usize),
    pub unified: (usize, usize),
}

/// A file's diff, HEAD against the work tree.
#[derive(Debug, Clone, Default)]
pub struct FileDiff {
    pub path: String,
    pub split: Vec<Row>,
    pub unified: Vec<Row>,
    pub changes: Vec<ChangeInfo>,
    /// Shown instead of (or above) the rows: binary, a submodule, truncated.
    pub note: Option<String>,
}

impl FileDiff {
    pub fn rows(&self, split: bool) -> &[Row] {
        if split { &self.split } else { &self.unified }
    }

    pub fn span(&self, change: usize, split: bool) -> Option<(usize, usize)> {
        self.changes.get(change).map(|c| if split { c.split } else { c.unified })
    }

    /// Build the diff of one path's versions.
    pub fn build(store: &Store, path: &str, state: &FileState) -> FileDiff {
        let mut out = FileDiff { path: path.to_string(), ..FileDiff::default() };
        let versions = [state.head.as_ref(), state.index.entry(), state.work.as_ref()];
        if versions.iter().flatten().any(|e| e.mode == MODE_GITLINK) {
            out.note = Some("a submodule — stage or discard it whole".to_string());
            return out;
        }
        let bytes = versions.map(|e| e.map(|e| store.bytes(&e.blob)));
        if bytes.iter().flatten().any(|b| diff::is_binary(b)) {
            let size = |b: &Option<std::sync::Arc<[u8]>>| b.as_ref().map_or("absent".to_string(), |b| format!("{} bytes", b.len()));
            out.note = Some(format!("binary file: {} at HEAD, {} on disk", size(&bytes[0]), size(&bytes[2])));
            return out;
        }
        let text = |i: usize| bytes[i].as_ref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
        let (head, index, work) = (text(0), text(1), text(2));
        let (old_lines, new_lines) = (diff::lines(&head), diff::lines(&work));
        if old_lines.len().max(new_lines.len()) > MAX_LINES {
            out.note = Some(format!("too long to show here ({} lines)", old_lines.len().max(new_lines.len())));
            return out;
        }
        let changes = diff::changes(&head, &work);
        let staged = diff::staged(&changes, &diff::changes(&head, &index));
        let line = |lines: &[&str], i: usize| Line { no: i + 1, text: lines[i].trim_end_matches(['\n', '\r']).to_string() };

        let (mut o, mut n) = (0, 0);
        for (ci, (change, staged)) in changes.into_iter().zip(staged).enumerate() {
            while o < change.old.start {
                out.split.push(Row { old: Some(line(&old_lines, o)), new: Some(line(&new_lines, n)), change: None });
                out.unified.push(Row { old: Some(line(&old_lines, o)), new: Some(line(&new_lines, n)), change: None });
                o += 1;
                n += 1;
            }
            let (split_start, unified_start) = (out.split.len(), out.unified.len());
            let height = change.old.len().max(change.new.len());
            for k in 0..height {
                let old = (k < change.old.len()).then(|| line(&old_lines, change.old.start + k));
                let new = (k < change.new.len()).then(|| line(&new_lines, change.new.start + k));
                out.split.push(Row { old, new, change: Some(ci) });
            }
            for i in change.old.clone() {
                out.unified.push(Row { old: Some(line(&old_lines, i)), new: None, change: Some(ci) });
            }
            for i in change.new.clone() {
                out.unified.push(Row { old: None, new: Some(line(&new_lines, i)), change: Some(ci) });
            }
            let split = (split_start, out.split.len() - split_start);
            let unified = (unified_start, out.unified.len() - unified_start);
            o = change.old.end;
            n = change.new.end;
            out.changes.push(ChangeInfo { change, staged, split, unified });
        }
        while o < old_lines.len() && n < new_lines.len() {
            out.split.push(Row { old: Some(line(&old_lines, o)), new: Some(line(&new_lines, n)), change: None });
            out.unified.push(Row { old: Some(line(&old_lines, o)), new: Some(line(&new_lines, n)), change: None });
            o += 1;
            n += 1;
        }
        if out.changes.is_empty() && state.index.entry() != state.head.as_ref() {
            out.note = Some("only the file's mode changed".to_string());
        }
        out
    }
}

/// The scroll offset that keeps rows `span` in a window of `height` rows,
/// moving as little as possible.
pub fn scroll_to(span: (usize, usize), current: usize, height: usize) -> usize {
    let (start, len) = span;
    let height = height.max(1);
    if start < current {
        start
    } else if start + len > current + height {
        // A change taller than the window is pinned to its top.
        start.min((start + len).saturating_sub(height))
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{Entry, Index, MODE_FILE};

    fn state(head: &str, work: &str) -> FileState {
        FileState {
            head: Some(Entry::text(MODE_FILE, head.as_bytes().to_vec())),
            index: Index::Entry(Some(Entry::text(MODE_FILE, head.as_bytes().to_vec()))),
            work: Some(Entry::text(MODE_FILE, work.as_bytes().to_vec())),
        }
    }

    #[test]
    fn side_by_side_rows_pair_a_change_and_one_column_lists_it_twice() {
        let store = Store::new(std::env::temp_dir());
        let d = FileDiff::build(&store, "f", &state("a\nb\nc\n", "a\nB\nB2\nc\n"));
        assert_eq!(d.changes.len(), 1);
        assert_eq!(d.changes[0].split, (1, 2), "max(old, new) rows");
        assert_eq!(d.changes[0].unified, (1, 3), "old row, then the two new ones");
        assert_eq!(d.split.len(), 4);
        assert_eq!(d.split[2].old, None, "the second new line has nothing beside it");
        assert_eq!(d.changes[0].staged, Staged::No);
    }

    #[test]
    fn the_panels_fill_the_screen_and_the_diff_goes_single_column_when_narrow() {
        let wide = Layout::compute(Rect::new(0, 0, 160, 40), 3);
        assert_eq!(wide.tree.width + wide.main.width, 160);
        assert_eq!(wide.tree.height + wide.plan.height, 40);
        assert!(wide.split());
        assert!(!Layout::compute(Rect::new(0, 0, 70, 20), 0).split());
    }

    #[test]
    fn scrolling_moves_only_as_far_as_it_has_to() {
        assert_eq!(scroll_to((5, 2), 0, 10), 0);
        assert_eq!(scroll_to((12, 2), 0, 10), 4);
        assert_eq!(scroll_to((3, 1), 8, 10), 3);
        assert_eq!(scroll_to((20, 30), 0, 10), 20, "a tall change shows its top");
    }
}
