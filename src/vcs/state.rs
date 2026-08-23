//! What the version-control view is showing, and where the cursor is in it.
//!
//! The session state: the loaded snapshot, the [`Plan`] built on top of it,
//! the cursor, and the drag in progress.  Kept separate from `exec::vcs`
//! (which owns the `App` mutations) so the interesting part — what dragging a
//! thing onto another thing *means* — is a pure state machine with tests.

use std::path::PathBuf;

use super::{
    layout::{self, Dir, Focus, Layout},
    plan::{Edit, Plan},
    Dag, Oid,
};

/// An open version-control session.
pub struct VcsState {
    /// The work tree this graph came from.
    pub root: PathBuf,
    pub dag: Dag,
    pub plan: Plan,
    /// What the cursor is on.  `None` only for an empty repository.
    pub focus: Option<Focus>,
    /// What is being dragged, if the user has pressed the grab key.
    ///
    /// The cursor keeps moving while this is set; what changes is that every
    /// move rewrites the plan's provisional edit, so the graph rearranges
    /// under the cursor instead of the cursor merely travelling over it.
    pub grabbed: Option<Focus>,
    /// Scroll anchor: the leftmost column drawn, and the topmost track.
    ///
    /// Columns are fine-grained (the time axis is the one you travel along, so
    /// it has to move smoothly) while tracks move a whole branch row at a
    /// time — half a block above the top edge is unreadable.
    pub scroll_col: u16,
    pub scroll_track: usize,
    /// The moment the snapshot was taken, so every "3d ago" on screen is
    /// relative to one instant rather than to whenever each was rendered.
    pub now: i64,
}

/// What a grab gesture would do — worked out from what is held and what the
/// cursor is over.
///
/// Named rather than inlined because it is the whole interaction model: the
/// user never picks an operation, they pick two things, and this decides what
/// putting one on the other means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drop {
    /// Would produce this edit.
    Edit(Edit),
    /// Over something that is not a valid destination, with a reason.
    Invalid(String),
    /// Back where it started — dropping here changes nothing.
    Unchanged,
}

impl VcsState {
    pub fn new(root: PathBuf, dag: Dag, now: i64) -> Self {
        let mut state = VcsState {
            root,
            dag,
            plan: Plan::default(),
            focus: None,
            grabbed: None,
            scroll_col: 0,
            scroll_track: 0,
            now,
        };
        state.focus = state.layout(80).initial_focus();
        state
    }

    /// Lay the graph out as the plan currently leaves it.
    ///
    /// Recomputed rather than cached: the layout is a function of the plan,
    /// and the plan changes on every step of a drag.  A cache would need
    /// invalidating on exactly the events that make it worth having, and
    /// showing a stale graph is the one failure this view cannot afford.
    pub fn layout(&self, width: u16) -> Layout {
        layout::compute(&self.dag, &self.plan.project(&self.dag), width)
    }

    /// The graph as it stands, ignoring any drag in progress.
    ///
    /// This is what navigation walks while something is held, and the reason
    /// is that the preview reshapes the very graph the cursor is moving
    /// through.  Stepping through the previewed layout meant each `j` changed
    /// where everything was, so the next `j` went somewhere unrelated, the
    /// preview blinked on and off, and the cursor stalled after two or three
    /// presses.  What the user is choosing between is *things* — this commit
    /// or that one — and those are the same set either way.
    fn stable_layout(&self, width: u16) -> Layout {
        layout::compute(&self.dag, &self.plan.project_committed(&self.dag), width)
    }

    /// Replace the snapshot after a refresh, keeping the cursor where it can
    /// be kept.  The plan is dropped: it described a graph that no longer
    /// exists.
    pub fn reload(&mut self, dag: Dag, now: i64) {
        let previous = self.focus.clone();
        self.dag = dag;
        self.plan.clear();
        self.grabbed = None;
        self.now = now;
        let layout = self.layout(80);
        self.focus = previous
            .filter(|f| layout.locate(f).is_some())
            .or_else(|| layout.initial_focus());
    }

    /// The commit the cursor is over, whatever kind of thing it is over.
    ///
    /// A ref label and an arrow both *identify* a commit, and every action
    /// that takes one (checkout, copy the hash, show the diff) should work
    /// from any of them rather than only from the block itself.
    pub fn focused_commit(&self) -> Option<Oid> {
        match self.focus.as_ref()? {
            Focus::Commit(id) => Some(id.clone()),
            Focus::Edge { child, .. } => Some(child.clone()),
            Focus::Ref(name) => self
                .plan
                .project(&self.dag)
                .ref_target(&self.dag, name)
                .cloned(),
            Focus::Head => self.dag.head.target.clone(),
        }
    }

    /// The branch the cursor is over, if it is over one.
    pub fn focused_branch(&self) -> Option<String> {
        match self.focus.as_ref()? {
            Focus::Ref(name) => Some(name.clone()),
            Focus::Head => self.dag.head.branch.clone(),
            _ => None,
        }
    }

    /// Move the cursor, updating the drag if one is in progress.
    ///
    /// Returns false when there was nowhere to go, so the caller can leave the
    /// screen alone rather than redraw an identical frame.
    pub fn step(&mut self, dir: Dir, width: u16) -> bool {
        // While dragging, walk the un-previewed graph: see `stable_layout`.
        let dragging = self.grabbed.is_some();
        let layout = if dragging { self.stable_layout(width) } else { self.layout(width) };
        let Some(current) = self.focus.clone() else {
            self.focus = layout.initial_focus();
            return self.focus.is_some();
        };
        // While something is held the walk is over *destinations*, and the
        // only destination is a commit: everything being dragged is a pointer,
        // and a pointer points at a commit.  Letting the cursor stop on a
        // branch label or an arrow on the way offered a choice that was never
        // a choice — both are just other names for a commit already on the
        // walk — and doubled the number of presses to cross the graph.
        let held = self.grabbed.clone();
        let allow = |focus: &Focus| match (&held, focus) {
            (None, _) => true,
            (Some(held), Focus::Commit(id)) => {
                !matches!(self.drop_onto(held, id), Drop::Invalid(_))
            }
            (Some(_), _) => false,
        };
        let Some(next) = layout.step_where(&current, dir, allow) else {
            return false;
        };
        self.focus = Some(next);
        self.retarget_drag();
        true
    }

    /// Start dragging whatever the cursor is on.
    pub fn grab(&mut self) -> Result<(), String> {
        let Some(focus) = self.focus.clone() else {
            return Err("nothing to move".to_string());
        };
        match focus {
            // HEAD is not a thing you drag: where it points is decided by
            // which branch is checked out, and pretending otherwise would let
            // the user build a plan whose only honest derivation is a checkout.
            Focus::Head => Err("HEAD follows whichever branch is checked out".to_string()),
            _ => {
                self.grabbed = Some(focus);
                Ok(())
            }
        }
    }

    /// What dropping the held thing where the cursor is would do.
    pub fn pending_drop(&self) -> Option<Drop> {
        let held = self.grabbed.as_ref()?;
        let over = self.focus.as_ref()?;
        // The destination is always a commit: everything being dragged is a
        // pointer of some kind, and a pointer points at a commit.
        let Some(target) = self.commit_under(over) else {
            return Some(Drop::Invalid("drop onto a commit".to_string()));
        };
        Some(self.drop_onto(held, &target))
    }

    /// Finish the drag, putting the edit on the stack.
    pub fn release(&mut self) -> Result<Option<Edit>, String> {
        let outcome = self.pending_drop();
        self.grabbed = None;
        self.plan.clear_provisional();
        match outcome {
            None => Ok(None),
            Some(Drop::Unchanged) => Ok(None),
            Some(Drop::Invalid(why)) => Err(why),
            Some(Drop::Edit(edit)) => self.push_edit(edit.clone()).map(|()| Some(edit)),
        }
    }

    /// Put an edit on the plan's stack directly, without a drag.
    ///
    /// The commands that do not have a two-thing gesture behind them go
    /// through here: `:vc-drop` names one commit, `:vc-merge` names one
    /// branch and one commit.
    pub fn push_edit(&mut self, edit: Edit) -> Result<(), String> {
        // The plan borrows the dag to project it, and both live on `self`.
        // Moving the dag out for the duration is cheaper than cloning it and
        // clearer than splitting the struct for one call.
        let dag = std::mem::replace(&mut self.dag, empty_dag());
        let result = self.plan.push(&dag, edit);
        self.dag = dag;
        result
    }

    /// Abandon the drag, leaving the graph as it was.
    pub fn cancel_drag(&mut self) -> bool {
        let had = self.grabbed.take().is_some();
        self.plan.clear_provisional();
        had
    }

    /// Recompute the preview after the cursor moves during a drag.
    fn retarget_drag(&mut self) {
        if self.grabbed.is_none() {
            return;
        }
        match self.pending_drop() {
            Some(Drop::Edit(edit)) => self.plan.set_provisional(edit),
            // Over an invalid or unchanged destination the preview reverts to
            // the un-dragged graph, so the picture always matches what
            // releasing here would leave behind.
            _ => self.plan.clear_provisional(),
        }
    }

    /// The commit a focus identifies, in the *projected* graph.
    fn commit_under(&self, focus: &Focus) -> Option<Oid> {
        // The committed stack, not the drag: what the destination *means* must
        // not be changed by the drag currently hovering over it.
        let projection = self.plan.project_committed(&self.dag);
        match focus {
            Focus::Commit(id) => Some(id.clone()),
            Focus::Ref(name) => projection.ref_target(&self.dag, name).cloned(),
            Focus::Head => self.dag.head.target.clone(),
            // An arrow identifies its child; dropping one arrow on another
            // means "follow the same commit this one does".
            Focus::Edge { child, slot } => {
                projection.parents(&self.dag, child).get(*slot).cloned()
            }
        }
    }

    /// An edit, checked against the same rule [`Plan::push`] applies.
    ///
    /// The preview sets the provisional edit directly, which would otherwise
    /// bypass that check: dragging an arrow over its own child drew a cyclic
    /// graph — lanes and all — that no git command could ever produce, and only
    /// said so when the drag was released.  Refusing here means the picture is
    /// always one that could exist.
    fn validated(&self, edit: Edit) -> Drop {
        let mut trial = self.plan.clone();
        trial.clear_provisional();
        match trial.push(&self.dag, edit.clone()) {
            Ok(()) => Drop::Edit(edit),
            Err(why) => Drop::Invalid(why),
        }
    }

    /// What putting `held` on `target` means.
    ///
    /// The whole interaction, in one function.  Note that no git verb appears:
    /// each case is a topological statement, and which command it becomes is
    /// [`super::derive`]'s problem.
    fn drop_onto(&self, held: &Focus, target: &Oid) -> Drop {
        // See `commit_under`: comparing against a projection that already
        // contains the preview makes every drop look like a no-op.
        let projection = self.plan.project_committed(&self.dag);
        match held {
            // An arrow *is* the "follows" relation, so moving it says which
            // commit its child should follow.
            Focus::Edge { child, slot } => {
                if child == target {
                    return Drop::Invalid("a commit cannot follow itself".to_string());
                }
                if projection.parents(&self.dag, child).get(*slot) == Some(target) {
                    return Drop::Unchanged;
                }
                self.validated(Edit::Reparent {
                    child: child.clone(),
                    slot: *slot,
                    new_parent: Some(target.clone()),
                })
            }

            // Dragging a block is the same statement made about its first
            // parent — the gesture most people reach for, and the one the
            // arrow underneath it would have made.
            Focus::Commit(id) => {
                if id == target {
                    return Drop::Unchanged;
                }
                if projection.parents(&self.dag, id).first() == Some(target) {
                    return Drop::Unchanged;
                }
                self.validated(Edit::Reparent {
                    child: id.clone(),
                    slot: 0,
                    new_parent: Some(target.clone()),
                })
            }

            // A branch label is a pointer, so moving it repoints the branch —
            // which is a reset or a fast-forward depending on direction, and
            // needs to be neither here.
            Focus::Ref(name) => {
                let Some(r) = self.dag.find_ref(name) else {
                    return Drop::Invalid(format!("{name} is gone"));
                };
                if r.kind != super::RefKind::Local {
                    return Drop::Invalid(format!(
                        "{name} is not a local branch — it records where the remote was"
                    ));
                }
                if projection.ref_target(&self.dag, name) == Some(target) {
                    return Drop::Unchanged;
                }
                self.validated(Edit::MoveRef {
                    name: name.clone(),
                    new_target: target.clone(),
                })
            }

            Focus::Head => Drop::Invalid("HEAD follows whichever branch is checked out".into()),
        }
    }
}

/// A placeholder used only to move the real `Dag` out for the duration of a
/// borrow that needs `&mut self` and `&Dag` at once.
fn empty_dag() -> Dag {
    Dag::new(Vec::new(), Vec::new(), super::Head::default(), super::WorkTree::default(), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{Commit, Head, Ref, RefKind, WorkTree};

    fn commit(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: Oid::new(id),
            parents: parents.iter().map(|p| Oid::new(*p)).collect(),
            summary: id.into(),
            author: "T".into(),
            when: 0,
            insertions: 0,
            deletions: 0,
        }
    }

    fn state() -> VcsState {
        let dag = Dag::new(
            vec![
                commit("d", &["c"]),
                commit("f", &["e"]),
                commit("c", &["a"]),
                commit("e", &["a"]),
                commit("a", &[]),
            ],
            vec![
                Ref { name: "feature".into(), kind: RefKind::Local, target: Oid::new("d"), upstream: None },
                Ref { name: "main".into(), kind: RefKind::Local, target: Oid::new("f"), upstream: None },
                Ref { name: "origin/main".into(), kind: RefKind::Remote, target: Oid::new("e"), upstream: None },
            ],
            Head { branch: Some("main".into()), target: Some(Oid::new("f")) },
            WorkTree::default(),
            false,
        );
        VcsState::new(PathBuf::from("/tmp/x"), dag, 0)
    }

    /// The gesture the whole feature is built around: grab an arrow, move to
    /// another commit, drop.  Between the two presses the graph already shows
    /// the result, and only the second press makes it a decision.
    #[test]
    fn grabbing_an_arrow_and_dropping_it_elsewhere_reparents() {
        let mut state = state();
        state.focus = Some(Focus::Edge { child: Oid::new("c"), slot: 0 });
        state.grab().unwrap();

        state.focus = Some(Focus::Commit(Oid::new("f")));
        state.retarget_drag();

        // Previewed, but not decided: the drawn graph already shows the
        // result while the stack is still empty.
        assert!(!state.plan.is_empty());
        assert_eq!(state.plan.edits().len(), 0);
        let projection = state.plan.project(&state.dag);
        assert_eq!(projection.parents(&state.dag, &Oid::new("c")), [Oid::new("f")]);

        let edit = state.release().unwrap().expect("an edit was made");
        assert_eq!(
            edit,
            Edit::Reparent { child: Oid::new("c"), slot: 0, new_parent: Some(Oid::new("f")) }
        );
        assert_eq!(state.plan.edits().len(), 1);
        assert!(state.grabbed.is_none());
    }

    /// Abandoning a drag has to leave no trace, or the preview becomes a
    /// series of accidental edits.
    #[test]
    fn cancelling_a_drag_leaves_the_graph_exactly_as_it_was() {
        let mut state = state();
        state.focus = Some(Focus::Edge { child: Oid::new("c"), slot: 0 });
        state.grab().unwrap();
        state.focus = Some(Focus::Commit(Oid::new("f")));
        state.retarget_drag();
        assert!(!state.plan.is_empty());

        assert!(state.cancel_drag());
        assert!(state.plan.is_empty());
        assert!(state.grabbed.is_none());
        assert!(!state.cancel_drag(), "cancelling twice is not a second cancel");
    }

    /// Dragging the block is the gesture most people reach for, and it means
    /// the same thing as dragging the arrow beneath it.
    #[test]
    fn dragging_a_block_moves_its_first_parent_link() {
        let mut state = state();
        state.focus = Some(Focus::Commit(Oid::new("c")));
        state.grab().unwrap();
        state.focus = Some(Focus::Commit(Oid::new("f")));

        assert_eq!(
            state.pending_drop(),
            Some(Drop::Edit(Edit::Reparent {
                child: Oid::new("c"),
                slot: 0,
                new_parent: Some(Oid::new("f"))
            }))
        );
    }

    /// Moving a branch label is a repoint — which is a reset or a
    /// fast-forward depending on which way it went, and is neither here.
    #[test]
    fn dragging_a_branch_label_repoints_the_branch() {
        let mut state = state();
        state.focus = Some(Focus::Ref("main".into()));
        state.grab().unwrap();
        state.focus = Some(Focus::Commit(Oid::new("d")));

        assert_eq!(
            state.pending_drop(),
            Some(Drop::Edit(Edit::MoveRef {
                name: "main".into(),
                new_target: Oid::new("d")
            }))
        );
    }

    /// A remote-tracking branch records where the remote was at the last
    /// fetch.  Moving it locally would make the view lie about the remote.
    #[test]
    fn a_remote_branch_refuses_to_be_dragged_and_says_why() {
        let mut state = state();
        state.focus = Some(Focus::Ref("origin/main".into()));
        state.grab().unwrap();
        state.focus = Some(Focus::Commit(Oid::new("d")));

        let Some(Drop::Invalid(why)) = state.pending_drop() else {
            panic!("a remote branch is not movable");
        };
        assert!(why.contains("remote"), "{why}");
        assert!(state.release().is_err());
        assert!(state.plan.is_empty(), "a refused drop leaves no edit");
    }

    /// Dropping something back where it came from is not an edit — otherwise
    /// the stack fills with no-ops that `u` then has to be pressed through.
    #[test]
    fn dropping_something_back_where_it_started_is_not_an_edit() {
        let mut state = state();
        state.focus = Some(Focus::Edge { child: Oid::new("c"), slot: 0 });
        state.grab().unwrap();
        state.focus = Some(Focus::Commit(Oid::new("a")));

        assert_eq!(state.pending_drop(), Some(Drop::Unchanged));
        assert_eq!(state.release().unwrap(), None);
        assert!(state.plan.is_empty());
    }

    /// The cycle refusal from `Plan::push` has to survive the drag layer:
    /// releasing over an impossible target reports it and changes nothing.
    #[test]
    fn dropping_a_commit_onto_its_own_descendant_is_refused() {
        let mut state = state();
        state.focus = Some(Focus::Commit(Oid::new("a")));
        state.grab().unwrap();
        state.focus = Some(Focus::Commit(Oid::new("d")));

        let err = state.release().expect_err("a would become its own ancestor");
        assert!(err.contains("own ancestor"), "{err}");
        assert!(state.plan.is_empty());
        assert!(state.grabbed.is_none(), "the drag ends either way");
    }

    /// The preview must never draw a graph git has no meaning for.  Dragging
    /// an arrow over its own child used to render a cyclic graph — lanes and
    /// all — and only object when the drag was released.
    #[test]
    fn a_drag_over_an_impossible_target_previews_nothing() {
        let mut state = state();
        state.focus = Some(Focus::Edge { child: Oid::new("c"), slot: 0 });
        state.grab().unwrap();
        // `d` is `c`'s own child: `c` cannot follow it.
        state.focus = Some(Focus::Commit(Oid::new("d")));
        state.retarget_drag();

        assert!(matches!(state.pending_drop(), Some(Drop::Invalid(_))));
        assert!(state.plan.is_empty(), "and nothing is previewed");
        let projection = state.plan.project(&state.dag);
        assert_eq!(projection.parents(&state.dag, &Oid::new("c")), [Oid::new("a")]);
    }

    #[test]
    fn head_is_not_something_you_drag() {
        let mut state = state();
        state.focus = Some(Focus::Head);
        let err = state.grab().expect_err("HEAD is not draggable");
        assert!(err.contains("checked out"), "{err}");
        assert!(state.grabbed.is_none());
    }

    /// Every kind of cursor position identifies a commit, so an action like
    /// checkout works from a label or an arrow, not only from the block.
    #[test]
    fn every_focus_names_the_commit_it_is_about() {
        let mut state = state();
        for (focus, expected) in [
            (Focus::Commit(Oid::new("c")), "c"),
            (Focus::Edge { child: Oid::new("c"), slot: 0 }, "c"),
            (Focus::Ref("main".into()), "f"),
            (Focus::Head, "f"),
        ] {
            state.focus = Some(focus.clone());
            assert_eq!(state.focused_commit(), Some(Oid::new(expected)), "{focus:?}");
        }
    }

    /// Walking with `j` while holding something must keep rewriting the
    /// preview, not merely move a cursor over a static picture.
    #[test]
    fn the_graph_rearranges_as_the_cursor_moves_during_a_drag() {
        let mut state = state();
        state.focus = Some(Focus::Commit(Oid::new("c")));
        state.grab().unwrap();

        let mut seen = Vec::new();
        for _ in 0..8 {
            if !state.step(Dir::Down, 120) {
                break;
            }
            let projection = state.plan.project(&state.dag);
            seen.push(projection.parents(&state.dag, &Oid::new("c")).to_vec());
        }
        assert!(
            seen.iter().any(|parents| parents != &[Oid::new("a")]),
            "the preview never changed as the cursor moved: {seen:?}"
        );
    }

    /// A reload describes a different graph, so the plan built on the old one
    /// cannot survive it — an edit naming a commit that was amended away would
    /// derive into nonsense.
    #[test]
    fn reloading_drops_the_plan_and_keeps_the_cursor_where_it_can() {
        let mut state = state();
        state.focus = Some(Focus::Commit(Oid::new("c")));
        state.grab().unwrap();
        state.focus = Some(Focus::Commit(Oid::new("f")));
        state.retarget_drag();

        let dag = Dag::new(
            vec![commit("c", &["a"]), commit("a", &[])],
            Vec::new(),
            Head::default(),
            WorkTree::default(),
            false,
        );
        state.reload(dag, 99);
        assert!(state.plan.is_empty());
        assert!(state.grabbed.is_none());
        assert_eq!(state.now, 99);
        // `c` survived the reload, so the cursor stays on it.
        assert_eq!(state.focus, Some(Focus::Commit(Oid::new("c"))));
    }

    /// A cursor sitting on something the new snapshot does not contain has to
    /// be re-seeded rather than left dangling.
    #[test]
    fn a_cursor_on_something_that_is_gone_is_reseeded() {
        let mut state = state();
        state.focus = Some(Focus::Commit(Oid::new("d")));
        let dag = Dag::new(
            vec![commit("z", &[])],
            Vec::new(),
            Head::default(),
            WorkTree::default(),
            false,
        );
        state.reload(dag, 0);
        assert_eq!(state.focus, Some(Focus::Commit(Oid::new("z"))));
    }
}
