//! Resolving a merge conflict: the data model.
//!
//! ## Why this exists
//!
//! Git's answer to a conflict is to write both versions into the file between
//! markers and leave.  Three separate things are wrong with that, and this
//! module exists to fix all three:
//!
//! 1. **The labels are not names.**  `HEAD` is a pointer and `9f2c1ab` is a
//!    hash.  Neither answers the only question a person actually has — *is the
//!    30 mine or theirs?*  So every side here carries a [`SideLabel`]: a
//!    branch name, an author, a date and a summary, resolved from whatever
//!    operation is in progress.
//! 2. **Ours and theirs invert during a replay, silently.**  In a merge,
//!    stage 2 is your branch.  In a rebase or a cherry-pick — which is what
//!    this editor's own `g a` runs — git checks out the base and replays your
//!    commits onto it, so stage 2 is **the branch you are landing on** and
//!    stage 3 is **your own commit**.  [`Operation`] knows which, and the
//!    labels say so in words rather than using "ours" and "theirs" at all.
//! 3. **The common ancestor is missing by default.**  Without it you cannot
//!    tell who changed what: `30 → 60` against `30 → 45` is a real
//!    disagreement, `30 → 60` against an untouched `30` is not, and git
//!    presents them identically.  Stage 1 is always read here.
//!
//! ## What is read, and what is never read
//!
//! The working file's conflict markers are **never parsed**.  They are git's
//! rendering of the conflict in whatever `merge.conflictStyle` the user
//! happens to have set, and reading them back would be reading our own output.
//! The three **index stages** are the actual data (see [`load`]), and the
//! regions come from a three-way diff this module runs itself ([`hunk`]) —
//! our invocation, so our marker style, regardless of the user's config.
//!
//! ## Layers
//!
//! ```text
//! load.rs     ConflictSet — an immutable snapshot: the files, their stages,
//!                           the resolved labels, the operation in progress
//! hunk.rs     Region      — the three-way diff: agreed text, and conflicted
//!                           text with its three versions
//! mod.rs      Resolution  — a stack of choices; folds into the merged text
//! write.rs                — writes the file and stages it.  The only module
//!                           here that can lose anything
//! ```
//!
//! See `docs/merge-conflict-plan.md` for the design record.

pub mod hunk;
pub mod layout;
pub mod load;
pub mod state;
pub mod write;

use std::path::PathBuf;

/// Which of a conflict's two competing versions.
///
/// Deliberately *not* named `Ours`/`Theirs`: those are the words that cause
/// the confusion this module exists to remove.  A side is identified by its
/// index-stage number, which is unambiguous, and named for the user by its
/// [`SideLabel`], which is resolved from the operation in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// Index stage 2 — `HEAD`'s version, which is *not* always yours (see
    /// [`Operation::left_is_yours`]).
    Left,
    /// Index stage 3 — the incoming version.
    Right,
}

/// What git is in the middle of, which is what decides who is who.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Merge,
    CherryPick,
    Rebase,
    Revert,
    /// A conflicted index with no operation file to explain it — a
    /// `git stash pop`, or an operation state this editor does not know.  The
    /// stages are still real, so the sides are still resolvable by commit;
    /// only the wording gets vaguer.
    Unknown,
}

impl Operation {
    /// Whether stage 2 is the user's own work.
    ///
    /// This is the inversion, stated once.  In a merge, stage 2 is the branch
    /// you are on and stage 3 is what is coming in.  In a replay, git has
    /// checked out the *base* and is applying your commits to it, so the two
    /// swap over.  Everything that words a side reads this rather than
    /// restating it, because restating it is how it gets stated backwards.
    pub fn left_is_yours(self) -> bool {
        match self {
            Operation::Merge | Operation::Revert | Operation::Unknown => true,
            Operation::CherryPick | Operation::Rebase => false,
        }
    }

    /// How the operation reads in the view's header.
    pub fn describe(self) -> &'static str {
        match self {
            Operation::Merge => "merge",
            Operation::CherryPick => "cherry-pick",
            Operation::Rebase => "rebase",
            Operation::Revert => "revert",
            Operation::Unknown => "conflicted index",
        }
    }
}

/// Everything one pane's header says about its side.
///
/// A commit's worth of fact rather than a ref name, because "which branch" is
/// only half the question: a stale topic branch and a colleague's push an hour
/// ago are told apart by the author and the date, not by the name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideLabel {
    /// What this side *is*, in words: "On origin/main", "Your commit, being
    /// replayed".  Never "ours" or "theirs".
    pub role: String,
    /// Abbreviated commit hash, when the side resolves to one.
    pub commit: String,
    pub author: String,
    /// Unix seconds; rendered relative by [`crate::vcs::relative_time`].
    pub when: i64,
    /// The commit's first line.
    pub summary: String,
}

impl SideLabel {
    /// The second header row: hash, author, age — omitting whatever is
    /// missing, since a side may not resolve to a commit at all.
    pub fn attribution(&self, now: i64) -> String {
        let mut parts = Vec::new();
        if !self.commit.is_empty() {
            parts.push(self.commit.clone());
        }
        if !self.author.is_empty() {
            parts.push(self.author.clone());
        }
        if self.when > 0 {
            parts.push(crate::vcs::relative_time(self.when, now));
        }
        parts.join(" · ")
    }
}

/// One conflicted path, with the three versions of it git could not reconcile.
///
/// A stage can be **absent**, and that is a real conflict rather than an error:
/// an add/add conflict has no base, and a modify/delete has no stage on the
/// side that deleted it.  Modelled as `Option` so those cases are carried
/// rather than crashed on — the resolver's answer for "the other side deleted
/// this" is one of its more useful ones.
#[derive(Debug, Clone)]
pub struct ConflictFile {
    /// Repository-relative, as git reports it.
    pub path: String,
    pub base: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
    /// The regions of the three-way diff, in file order.
    pub regions: Vec<hunk::Region>,
    /// One choice per **conflicted** region, in the same order as
    /// `regions.iter().filter(Region::is_conflict)`.
    pub choices: Vec<Choice>,
    /// What has been chosen, in order, so `u` can take the last one back.
    /// A stack rather than a mutated buffer, for the same reason
    /// [`crate::vcs::plan::Plan`] is one: the merged text is always a fold
    /// from scratch, so it cannot drift out of step with what is drawn.
    pub history: Vec<(usize, Choice)>,
    /// True once the file has been written and staged.
    pub resolved: bool,
}

/// What a conflicted region resolves to.
///
/// Each side is a **switch**, not a menu entry.  On/off across two sides gives
/// take-left, take-right, keep-both (in file order — the common answer for
/// imports and list entries) and delete-the-region, which is a real resolution
/// and the one no marker-editing workflow makes easy.  Anything that genuinely
/// needs merging rather than choosing becomes [`Choice::Edited`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Which sides are in, in file order.
    Sides { left: bool, right: bool },
    /// Text the user wrote by hand for this region.
    Edited(String),
}

impl Default for Choice {
    /// Nothing chosen yet: neither side is in.
    ///
    /// The alternative — defaulting to "left", which is what git's own markers
    /// effectively do — would let a file be written resolved without anybody
    /// having looked at it, which is the failure this whole view exists to
    /// prevent.  An unchosen region is instead what `n` walks to and what the
    /// footer counts.
    fn default() -> Self {
        Choice::Sides { left: false, right: false }
    }
}

impl Choice {
    /// True once this region has an answer.
    pub fn is_decided(&self) -> bool {
        !matches!(self, Choice::Sides { left: false, right: false })
    }

    /// Whether `side` is switched on.  An edited region has neither side on:
    /// its text is the user's, not either version's.
    pub fn takes(&self, side: Side) -> bool {
        match (self, side) {
            (Choice::Sides { left, .. }, Side::Left) => *left,
            (Choice::Sides { right, .. }, Side::Right) => *right,
            (Choice::Edited(_), _) => false,
        }
    }

    /// Flip one side, keeping the other.  Toggling a side of an *edited*
    /// region discards the hand-written text — the user asked for a version
    /// again, and silently keeping the edit underneath would make the panes
    /// lie about what would be written.
    pub fn toggle(&self, side: Side) -> Choice {
        let (mut left, mut right) = match self {
            Choice::Sides { left, right } => (*left, *right),
            Choice::Edited(_) => (false, false),
        };
        match side {
            Side::Left => left = !left,
            Side::Right => right = !right,
        }
        Choice::Sides { left, right }
    }

    /// Take only `side`.
    pub fn only(side: Side) -> Choice {
        Choice::Sides { left: side == Side::Left, right: side == Side::Right }
    }
}

impl ConflictFile {
    pub fn conflict_count(&self) -> usize {
        self.choices.len()
    }

    pub fn undecided_count(&self) -> usize {
        self.choices.iter().filter(|c| !c.is_decided()).count()
    }

    /// True when every conflicted region has an answer, so the file can be
    /// written.
    pub fn is_decided(&self) -> bool {
        self.undecided_count() == 0
    }

    /// Record a choice for the `n`th conflicted region.
    pub fn choose(&mut self, nth: usize, choice: Choice) {
        let Some(slot) = self.choices.get_mut(nth) else { return };
        let previous = std::mem::replace(slot, choice);
        self.history.push((nth, previous));
    }

    /// Take back the last choice.  Returns which region it was, so the view
    /// can put the cursor back on it — undoing something off screen and not
    /// saying where is the same as doing nothing.
    pub fn undo(&mut self) -> Option<usize> {
        let (nth, previous) = self.history.pop()?;
        if let Some(slot) = self.choices.get_mut(nth) {
            *slot = previous;
        }
        Some(nth)
    }

    /// The file as it would be written.
    ///
    /// A fold over the snapshot rather than a buffer that is edited in place:
    /// the panes and the merged text are then two renderings of one state and
    /// cannot disagree.  An undecided region contributes **nothing**, so the
    /// preview of a half-answered file shows what has been settled and no
    /// markers — writing it is refused separately, by [`Self::is_decided`].
    pub fn merged(&self) -> String {
        let mut out = String::new();
        let mut nth = 0;
        for region in &self.regions {
            match region {
                hunk::Region::Agreed(text) => out.push_str(text),
                hunk::Region::Conflict(versions) => {
                    let choice = self.choices.get(nth).cloned().unwrap_or_default();
                    nth += 1;
                    match choice {
                        Choice::Edited(text) => out.push_str(&text),
                        Choice::Sides { left, right } => {
                            if left {
                                out.push_str(&versions.left);
                            }
                            if right {
                                out.push_str(&versions.right);
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

/// Every conflicted file in the repository, as one immutable read.
///
/// Immutable in the same sense [`crate::vcs::Dag`] is: nothing here talks to
/// git after the load, and nothing here can write.  The *choices* on each file
/// are the mutable part, and they reach the repository in exactly one place
/// ([`write`]).
#[derive(Debug, Clone)]
pub struct ConflictSet {
    pub root: PathBuf,
    pub operation: Operation,
    /// What each side is, in words and in commits.  Resolved once at load,
    /// since it is a property of the operation rather than of the file.
    pub left: SideLabel,
    pub right: SideLabel,
    pub files: Vec<ConflictFile>,
}

impl ConflictSet {
    pub fn label(&self, side: Side) -> &SideLabel {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Files still holding an undecided region.
    pub fn unresolved(&self) -> usize {
        self.files.iter().filter(|f| !f.resolved).count()
    }
}

/// A throwaway repository on disk, for the tests in this module's submodules.
///
/// Real git, not a mock.  Every layer here is defined by what git's own state
/// looks like — a rebase stopped halfway, an add/add conflict with no base,
/// an index that says a path is resolved — and a mock would test the mock.
/// Lifted out of any one submodule because the reader, the writer and the view
/// all need to stand up the same conflicted repository.
#[cfg(test)]
pub mod testrepo {
    use std::path::PathBuf;

    use crate::vcs::load::git;

use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);

pub struct Repo {
    pub root: PathBuf,
}

impl Repo {
    pub fn new() -> Option<Self> {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("sv-conflict-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let repo = Repo { root };
        repo.run(&["init", "-b", "main"])?;
        repo.run(&["config", "user.email", "test@example.com"])?;
        repo.run(&["config", "user.name", "Test"])?;
        repo.run(&["config", "commit.gpgsign", "false"])?;
        // The user's own conflict style must not reach the parse: the
        // regions come from our own merge-file invocation, and this is the
        // test that would catch it if that ever stopped being true.
        repo.run(&["config", "merge.conflictStyle", "merge"])?;
        Some(repo)
    }

    pub fn run(&self, args: &[&str]) -> Option<String> {
        git(&self.root, args).ok()
    }

    /// Ignores the exit status — a conflicting merge or rebase *fails*,
    /// which is the state these tests are setting up.
    pub fn try_run(&self, args: &[&str]) {
        let _ = git(&self.root, args);
    }

    pub fn commit(&self, file: &str, contents: &str, message: &str) {
        std::fs::write(self.root.join(file), contents).unwrap();
        self.run(&["add", file]).unwrap();
        self.run(&["commit", "-m", message]).unwrap();
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `main` and `feature` both change the same line, from a shared base.
pub fn diverged() -> Option<Repo> {
    let repo = Repo::new()?;
    repo.commit("f.txt", "a\ntimeout = 30\nz\n", "base");
    repo.run(&["checkout", "-b", "feature"])?;
    repo.commit("f.txt", "a\ntimeout = 60\nz\n", "bump to 60");
    repo.run(&["checkout", "main"])?;
    repo.commit("f.txt", "a\ntimeout = 45\nz\n", "bump to 45");
    Some(repo)
}
}

#[cfg(test)]
mod tests {
    use super::hunk::{Region, Versions};
    use super::*;

    fn versions(base: &str, left: &str, right: &str) -> Versions {
        Versions { base: base.into(), left: left.into(), right: right.into() }
    }

    fn file(regions: Vec<Region>) -> ConflictFile {
        let conflicts = regions.iter().filter(|r| r.is_conflict()).count();
        ConflictFile {
            path: "f.txt".into(),
            base: None,
            left: None,
            right: None,
            regions,
            choices: vec![Choice::default(); conflicts],
            history: Vec::new(),
            resolved: false,
        }
    }

    fn one_conflict() -> ConflictFile {
        file(vec![
            Region::Agreed("a\n".into()),
            Region::Conflict(versions("timeout = 30\n", "timeout = 45\n", "timeout = 60\n")),
            Region::Agreed("z\n".into()),
        ])
    }

    /// The four resolutions the two switches produce, and the exact text each
    /// one writes.  "Both" is in **file order**, which is what makes it the
    /// right answer for two imports and the wrong one for two values.
    #[test]
    fn two_switches_give_four_resolutions() {
        let cases = [
            ((true, false), "a\ntimeout = 45\nz\n"),
            ((false, true), "a\ntimeout = 60\nz\n"),
            ((true, true), "a\ntimeout = 45\ntimeout = 60\nz\n"),
            ((false, false), "a\nz\n"),
        ];
        for ((left, right), expected) in cases {
            let mut f = one_conflict();
            f.choose(0, Choice::Sides { left, right });
            assert_eq!(f.merged(), expected, "left={left} right={right}");
        }
    }

    /// Taking neither side deletes the region, which is a real resolution —
    /// and the one no marker-editing workflow makes easy.  It is also the
    /// *default*, so it must not be mistaken for a decision.
    #[test]
    fn taking_neither_side_is_a_deletion_but_not_a_decision() {
        let mut f = one_conflict();
        assert!(!f.is_decided(), "nothing has been chosen yet");
        f.choose(0, Choice::Sides { left: true, right: false });
        f.choose(0, Choice::Sides { left: false, right: false });
        assert!(
            !f.is_decided(),
            "switching both sides back off is the undecided state again"
        );
    }

    /// Hand-edited text replaces the region outright — neither side is in it.
    #[test]
    fn an_edited_region_writes_the_users_own_text() {
        let mut f = one_conflict();
        f.choose(0, Choice::Edited("timeout = 50\n".into()));
        assert_eq!(f.merged(), "a\ntimeout = 50\nz\n");
        assert!(f.is_decided());
        assert!(!f.choices[0].takes(Side::Left) && !f.choices[0].takes(Side::Right));
    }

    /// Toggling a side of an edited region throws the edit away rather than
    /// keeping it underneath: the panes would otherwise show two sides switched
    /// on while something else entirely got written.
    #[test]
    fn toggling_a_side_of_an_edited_region_discards_the_edit() {
        let choice = Choice::Edited("x\n".into()).toggle(Side::Left);
        assert_eq!(choice, Choice::Sides { left: true, right: false });
    }

    /// Undo is a stack, and it says which region it put back — undoing
    /// something off screen without saying where is the same as doing nothing.
    #[test]
    fn undo_walks_back_through_the_choices_and_names_each_one() {
        let mut f = file(vec![
            Region::Conflict(versions("b\n", "l\n", "r\n")),
            Region::Agreed("mid\n".into()),
            Region::Conflict(versions("b2\n", "l2\n", "r2\n")),
        ]);
        f.choose(0, Choice::only(Side::Left));
        f.choose(1, Choice::only(Side::Right));
        assert_eq!(f.merged(), "l\nmid\nr2\n");

        assert_eq!(f.undo(), Some(1));
        assert_eq!(f.merged(), "l\nmid\n", "the second region went back to undecided");
        assert_eq!(f.undo(), Some(0));
        assert_eq!(f.merged(), "mid\n");
        assert_eq!(f.undo(), None, "nothing left to take back");
    }

    /// The inversion, pinned.  In a merge stage 2 is your branch; in a replay
    /// git has checked out the base and is applying your commits to it, so the
    /// two swap.  This is the single statement of that fact.
    #[test]
    fn ours_and_theirs_swap_over_during_a_replay() {
        assert!(Operation::Merge.left_is_yours());
        assert!(Operation::Revert.left_is_yours());
        assert!(!Operation::Rebase.left_is_yours());
        assert!(!Operation::CherryPick.left_is_yours());
    }

    /// A side that resolves to no commit still gets a usable header rather
    /// than a line of separators with nothing between them.
    #[test]
    fn a_label_with_nothing_to_attribute_says_nothing_rather_than_punctuation() {
        assert_eq!(SideLabel::default().attribution(0), "");
        let label = SideLabel {
            commit: "9f2c1ab".into(),
            author: "Ada".into(),
            when: 0,
            ..SideLabel::default()
        };
        assert_eq!(label.attribution(0), "9f2c1ab · Ada");
    }
}
