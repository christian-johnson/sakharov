//! What the resolver has on screen: which file, which hunk, which side.
//!
//! The cursor, not the data.  Everything the *data* consists of lives in
//! [`ConflictSet`] and is a pure read of git; this is the small amount of
//! per-session state that says which part of it you are looking at.

use super::{layout, Choice, ConflictFile, ConflictSet, Side};

/// The open resolver.
pub struct ConflictState {
    pub set: ConflictSet,
    /// Which conflicted file, an index into `set.files`.
    pub file: usize,
    /// Which conflicted region within it — the index into that file's
    /// `choices`, and the only numbering the user ever sees.
    pub nth: usize,
    /// Which pane the cursor is in.  `Space` acts on this one.
    pub side: Side,
    /// Pane content row offset.
    pub scroll: usize,
    /// The `3` toggle: show the common ancestor as a third pane.
    pub show_base: bool,
}

impl ConflictState {
    pub fn new(set: ConflictSet) -> Self {
        let mut state = ConflictState {
            set,
            file: 0,
            nth: 0,
            side: Side::Left,
            scroll: 0,
            show_base: false,
        };
        // Open on something worth looking at rather than on region 0, which in
        // a file whose first conflicts are already answered is a cursor parked
        // on finished work.
        state.nth = state.first_undecided().unwrap_or(0);
        state
    }

    pub fn current(&self) -> Option<&ConflictFile> {
        self.set.files.get(self.file)
    }

    pub fn current_mut(&mut self) -> Option<&mut ConflictFile> {
        self.set.files.get_mut(self.file)
    }

    /// The focused region's choice, or the default when there is none.
    pub fn choice(&self) -> Choice {
        self.current()
            .and_then(|f| f.choices.get(self.nth))
            .cloned()
            .unwrap_or_default()
    }

    fn conflicts(&self) -> usize {
        self.current().map_or(0, ConflictFile::conflict_count)
    }

    fn first_undecided(&self) -> Option<usize> {
        let file = self.current()?;
        file.choices.iter().position(|c| !c.is_decided())
    }

    /// Step to the next or previous **unanswered** region, wrapping.
    ///
    /// Unanswered rather than merely next, because the question the key is
    /// asked is "what is left" — stepping onto settled hunks would make `n`
    /// a tour of work already done, which in a file with forty conflicts and
    /// two left is the wrong tour.  Returns false when there are none, which
    /// is how the view knows to say the file is finished rather than
    /// silently not moving.
    pub fn step(&mut self, forward: bool) -> bool {
        let n = self.conflicts();
        if n == 0 {
            return false;
        }
        let Some(file) = self.current() else { return false };
        for offset in 1..=n {
            let candidate = match forward {
                true => (self.nth + offset) % n,
                false => (self.nth + n - offset % n) % n,
            };
            if file.choices.get(candidate).is_some_and(|c| !c.is_decided()) {
                self.nth = candidate;
                return true;
            }
        }
        false
    }

    /// Move to another region by index, whether or not it is answered.
    ///
    /// What `u` uses: undo has to be able to put the cursor back on a region
    /// it just made undecided *or* on one it made decided again.
    pub fn focus(&mut self, nth: usize) {
        if nth < self.conflicts() {
            self.nth = nth;
        }
    }

    /// Switch to another conflicted file.  Returns false when there is none
    /// that way, so the view can say so rather than appearing to ignore the
    /// key.
    pub fn step_file(&mut self, forward: bool) -> bool {
        let n = self.set.files.len();
        if n <= 1 {
            return false;
        }
        self.file = match forward {
            true => (self.file + 1) % n,
            false => (self.file + n - 1) % n,
        };
        self.nth = self.first_undecided().unwrap_or(0);
        self.scroll = 0;
        self.side = Side::Left;
        true
    }

    /// Follow the cursor: the focused region has to be on screen.
    ///
    /// Called once per frame from the run loop, exactly as the other views'
    /// scroll is, so the offset always reflects the current terminal size.
    pub fn update_scroll(&mut self, area: ratatui::layout::Rect) {
        let Some(file) = self.set.files.get(self.file) else { return };
        let l = layout::compute(area, file, self.show_base);
        self.scroll = l.scroll_to(self.nth, self.scroll).min(l.total);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{hunk::{Region, Versions}, Operation, SideLabel};
    use super::*;
    use std::path::PathBuf;

    fn state(decided: &[bool]) -> ConflictState {
        let regions: Vec<Region> = decided
            .iter()
            .map(|_| {
                Region::Conflict(Versions {
                    base: "b\n".into(),
                    left: "l\n".into(),
                    right: "r\n".into(),
                })
            })
            .collect();
        let choices = decided
            .iter()
            .map(|d| match d {
                true => Choice::only(Side::Left),
                false => Choice::default(),
            })
            .collect();
        let file = ConflictFile {
            path: "f.txt".into(),
            base: None,
            left: None,
            right: None,
            regions,
            choices,
            history: Vec::new(),
            resolved: false,
        };
        ConflictState::new(ConflictSet {
            root: PathBuf::from("/tmp"),
            operation: Operation::Merge,
            left: SideLabel::default(),
            right: SideLabel::default(),
            files: vec![file],
        })
    }

    /// Opening parks the cursor on the first thing that still needs an answer,
    /// not on region 0 — which in a partly-resolved file is finished work.
    #[test]
    fn opening_lands_on_the_first_unanswered_region() {
        assert_eq!(state(&[true, true, false, false]).nth, 2);
    }

    /// `n` tours what is *left*, not every hunk: in a file with forty
    /// conflicts and two unanswered, stepping through the thirty-eight settled
    /// ones is the wrong tour.
    #[test]
    fn stepping_visits_only_what_is_still_unanswered() {
        let mut s = state(&[false, true, false]);
        assert_eq!(s.nth, 0);
        assert!(s.step(true));
        assert_eq!(s.nth, 2, "the answered region in the middle is skipped");
        assert!(s.step(true));
        assert_eq!(s.nth, 0, "and it wraps");
        assert!(s.step(false));
        assert_eq!(s.nth, 2, "backwards wraps too");
    }

    /// With everything answered there is nowhere to step, and the view has to
    /// be told so it can say the file is finished rather than appear wedged —
    /// the same failure the graph's motion refusal was fixing.
    #[test]
    fn stepping_in_a_finished_file_reports_that_it_moved_nowhere() {
        let mut s = state(&[true, true]);
        assert!(!s.step(true));
        assert!(!s.step(false));
    }

    /// A single conflicted file has nowhere to page to, and must say so
    /// rather than silently reopening itself.
    #[test]
    fn there_is_no_next_file_when_there_is_only_one() {
        let mut s = state(&[false]);
        assert!(!s.step_file(true));
    }
}
