//! Line diffs between a file's versions, and change-level staging.
//!
//! A **change** is one contiguous run of changed lines between HEAD and the
//! work tree, with no context lines — the unit `j`/`k` stop on and `Space`
//! stages.  Every change is stated against HEAD's lines, so the index can be
//! described as "HEAD with some of these changes applied" and staging one is
//! rebuilding that text with one more (or one fewer).

use std::ops::Range;

use similar::{DiffOp, TextDiff};

/// One run of changed lines: `old` lines of HEAD replaced by `new` lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Line range in the old text.
    pub old: Range<usize>,
    /// Line range in the new text.
    pub new: Range<usize>,
    /// The replacement lines, newlines included.
    pub text: String,
}

/// Whether a work-tree change is in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staged {
    No,
    Yes,
    /// The index has something else in this region.
    Partly,
}

/// Identifies a change across a reload: where it sits in HEAD and what it
/// puts there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeKey {
    pub old: Range<usize>,
    pub text: String,
}

impl Change {
    pub fn key(&self) -> ChangeKey {
        ChangeKey { old: self.old.clone(), text: self.text.clone() }
    }

    /// Whether two changes against the same old text touch the same lines.
    /// Pure insertions overlap anything at their position.
    fn overlaps(&self, other: &Change) -> bool {
        let (a, b) = (&self.old, &other.old);
        if a.is_empty() || b.is_empty() {
            return a.start <= b.end && b.start <= a.end;
        }
        a.start < b.end && b.start < a.end
    }
}

/// Split into lines, keeping each line's newline.
pub fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Every change from `old` to `new`, in order.
pub fn changes(old: &str, new: &str) -> Vec<Change> {
    let new_lines = lines(new);
    let diff = TextDiff::from_lines(old, new);
    let mut out: Vec<Change> = Vec::new();
    for op in diff.ops() {
        let (o, n) = match *op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete { old_index, old_len, new_index } => (old_index..old_index + old_len, new_index..new_index),
            DiffOp::Insert { old_index, new_index, new_len } => (old_index..old_index, new_index..new_index + new_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                (old_index..old_index + old_len, new_index..new_index + new_len)
            }
        };
        // Adjacent ops (a delete then an insert) are one change.
        match out.last_mut() {
            Some(last) if last.old.end == o.start && last.new.end == n.start => {
                last.old.end = o.end;
                last.new.end = n.end;
            }
            _ => out.push(Change { old: o, new: n, text: String::new() }),
        }
    }
    for change in &mut out {
        change.text = new_lines[change.new.clone()].concat();
    }
    out
}

/// Whether each of `work_changes` (HEAD → work) is in the index, given
/// `index_changes` (HEAD → index).
pub fn staged(work_changes: &[Change], index_changes: &[Change]) -> Vec<Staged> {
    work_changes
        .iter()
        .map(|w| {
            let hits: Vec<&Change> = index_changes.iter().filter(|i| i.overlaps(w)).collect();
            match hits.as_slice() {
                [] => Staged::No,
                [i] if i.old == w.old && i.text == w.text => Staged::Yes,
                _ => Staged::Partly,
            }
        })
        .collect()
}

/// `base` with `apply` laid over it.  The changes must be stated against
/// `base` and must not overlap.
pub fn splice(base: &str, apply: &[Change]) -> String {
    let lines = lines(base);
    let mut sorted: Vec<&Change> = apply.iter().collect();
    sorted.sort_by_key(|c| (c.old.start, c.old.end));
    let mut out = String::with_capacity(base.len());
    let mut at = 0;
    for change in sorted {
        let start = change.old.start.clamp(at, lines.len());
        out.push_str(&lines[at..start].concat());
        out.push_str(&change.text);
        at = change.old.end.clamp(start, lines.len());
    }
    out.push_str(&lines[at..].concat());
    out
}

/// The index text after staging (`stage = true`) or unstaging `target`.
///
/// Everything else already in the index is kept; whatever the index had in
/// the target's region is replaced.
pub fn restage(head: &str, index: &str, target: &Change, stage: bool) -> String {
    let mut keep: Vec<Change> = changes(head, index).into_iter().filter(|c| !c.overlaps(target)).collect();
    if stage {
        keep.push(target.clone());
    }
    splice(head, &keep)
}

/// The work-tree text with `target` taken back out (its region returns to
/// HEAD's lines).
pub fn revert(head: &str, work: &str, target: &Change) -> String {
    let keep: Vec<Change> = changes(head, work).into_iter().filter(|c| c != target).collect();
    splice(head, &keep)
}

/// Find the change `key` names in a fresh diff: the same text at the same
/// place, or failing that the same text nearest to it.
pub fn find(current: &[Change], key: &ChangeKey) -> Option<Change> {
    current
        .iter()
        .filter(|c| c.text == key.text && c.old.len() == key.old.len())
        .min_by_key(|c| c.old.start.abs_diff(key.old.start))
        .cloned()
}

/// Content git would call binary: a NUL in the first 8000 bytes.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "a\nb\nc\nd\ne\n";

    #[test]
    fn changes_have_no_context_so_nearby_edits_stay_separate() {
        let work = "a\nB\nc\nD\ne\nf\n";
        let cs = changes(HEAD, work);
        assert_eq!(cs.len(), 3, "{cs:?}");
        assert_eq!((cs[0].old.clone(), cs[0].text.as_str()), (1..2, "B\n"));
        assert_eq!((cs[1].old.clone(), cs[1].text.as_str()), (3..4, "D\n"));
        assert_eq!((cs[2].old.clone(), cs[2].text.as_str()), (5..5, "f\n"), "an insertion at the end");
    }

    #[test]
    fn staging_one_change_leaves_the_others_out_of_the_index() {
        let work = "a\nB\nc\nD\ne\n";
        let cs = changes(HEAD, work);
        let index = restage(HEAD, HEAD, &cs[1], true);
        assert_eq!(index, "a\nb\nc\nD\ne\n");
        assert_eq!(staged(&cs, &changes(HEAD, &index)), vec![Staged::No, Staged::Yes]);

        let both = restage(HEAD, &index, &cs[0], true);
        assert_eq!(both, work);
        let none = restage(HEAD, &both, &cs[1], false);
        assert_eq!(none, "a\nB\nc\nd\ne\n");
    }

    #[test]
    fn an_index_holding_a_third_version_is_partly_staged() {
        let cs = changes(HEAD, "a\nB\nc\nd\ne\n");
        let index_changes = changes(HEAD, "a\nX\nc\nd\ne\n");
        assert_eq!(staged(&cs, &index_changes), vec![Staged::Partly]);
    }

    #[test]
    fn reverting_a_change_puts_heads_lines_back_and_keeps_the_rest() {
        let work = "a\nB\nc\nD\ne\n";
        let cs = changes(HEAD, work);
        assert_eq!(revert(HEAD, work, &cs[0]), "a\nb\nc\nD\ne\n");
    }

    #[test]
    fn deletions_and_a_file_without_a_final_newline_splice_cleanly() {
        let work = "a\nc\nd\ne";
        let cs = changes(HEAD, work);
        let rebuilt = splice(HEAD, &cs);
        assert_eq!(rebuilt, work);
    }

    #[test]
    fn a_change_is_found_again_after_lines_move_above_it() {
        let cs = changes(HEAD, "a\nb\nc\nD\ne\n");
        let moved = changes("z\na\nb\nc\nd\ne\n", "z\na\nb\nc\nD\ne\n");
        let found = find(&moved, &cs[0].key()).expect("found");
        assert_eq!(found.old, 4..5);
    }
}
