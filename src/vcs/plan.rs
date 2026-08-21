//! The edited graph: a [`Dag`] plus an ordered stack of [`Edit`]s.
//!
//! Nothing here writes to the repository.  A [`Plan`] is a *description* of a
//! different shape for the history, and [`Plan::project`] turns it into the
//! [`Projection`] the renderer draws.  Working out which git commands would
//! make the repository match is [`super::derive`]'s job, and running them is
//! [`super::apply`]'s.
//!
//! ## Why a stack rather than a mutated copy
//!
//! The same shape as `table::Session`'s transform stack, for the same reasons:
//! `u` pops so undo is free, the projection is rebuilt from scratch so a
//! rejected edit cannot leave half-applied state, and — the one that matters
//! most here — the stack records **intent**.  "This edge moved from P to Q" is
//! exactly what the derivation needs to know.  A structurally-diffed copy of
//! the graph would have to guess it back out, and guessing wrong means
//! rewriting commits the user never touched.

use std::collections::{HashMap, HashSet};

use super::{Dag, Oid};

/// One structural change to the graph.
///
/// A closed set on purpose.  These four are the topological primitives every
/// history rewrite decomposes into; the git verbs (rebase, cherry-pick,
/// fast-forward, reset) are *derived* from them rather than named here, which
/// is the whole premise of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// `child`'s parent link number `slot` now points at `new_parent`, or at
    /// nothing (`None`), which makes `child` a root.
    ///
    /// The rebase/cherry-pick primitive: moving the arrow at the base of a
    /// chain carries the whole chain with it, because every commit above it
    /// keeps pointing at the one below.
    Reparent { child: Oid, slot: usize, new_parent: Option<Oid> },
    /// A branch now points at a different commit.
    MoveRef { name: String, new_target: Oid },
    /// Branch `into` gains a merge commit whose second parent is `from`.
    ///
    /// Not "`into`'s tip gains a parent": git never adds a parent to an
    /// existing commit.  A merge makes a **new** commit with the two tips as
    /// parents, so the projection grows a [`Pending`] node for it.
    /// `commit` leaves the graph; everything that pointed at it points at its
    /// first parent instead.
    Merge { into: String, from: Oid },
    /// `commit` leaves the graph; everything that pointed at it points at its
    /// first parent instead.
    Drop { commit: Oid },
}

impl Edit {
    /// One line describing what this edit did, for the status line and the
    /// confirmation popup.  Phrased structurally — "onto", "to" — because the
    /// git verb it becomes is not decided until the derivation runs.
    pub fn label(&self) -> String {
        match self {
            Edit::Reparent { child, new_parent: Some(p), .. } => {
                format!("{} onto {}", child.short(), p.short())
            }
            Edit::Reparent { child, new_parent: None, .. } => {
                format!("{} detached", child.short())
            }
            Edit::MoveRef { name, new_target } => format!("{name} to {}", new_target.short()),
            Edit::Merge { into, from } => format!("merge {} into {into}", from.short()),
            Edit::Drop { commit } => format!("drop {}", commit.short()),
        }
    }
}

/// A commit the plan would create, which therefore has no object behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: Oid,
    pub parents: Vec<Oid>,
    /// What its commit message would be — drawn in the block, so the user can
    /// see what the merge is before agreeing to make it.
    pub summary: String,
}

/// The graph as the edits leave it: overrides layered over the snapshot.
///
/// Overrides rather than a rewritten copy, so an untouched graph costs
/// nothing and so "did this commit's parentage change?" — the question the
/// derivation is built on — is a map lookup rather than a comparison.
#[derive(Debug, Clone, Default)]
pub struct Projection {
    parents: HashMap<Oid, Vec<Oid>>,
    refs: HashMap<String, Oid>,
    dropped: HashSet<Oid>,
    /// Commits the plan would create, in the order it would create them.
    pending: Vec<Pending>,
}

impl Projection {
    /// `id`'s parents as the plan leaves them.
    ///
    /// Falls back to the snapshot, and to empty for a commit past the loaded
    /// horizon — which reads as "a root", the only honest answer when its real
    /// parents were never fetched.
    pub fn parents<'a>(&'a self, dag: &'a Dag, id: &Oid) -> &'a [Oid] {
        if let Some(parents) = self.parents.get(id) {
            return parents;
        }
        if let Some(p) = self.pending.iter().find(|p| p.id == *id) {
            return &p.parents;
        }
        dag.get(id).map_or(&[], |c| c.parents.as_slice())
    }

    /// The commits this plan would create, oldest first.  The renderer draws
    /// them as blocks alongside the real ones so a merge is visible before it
    /// is agreed to.
    pub fn pending(&self) -> &[Pending] {
        &self.pending
    }

    /// Where `name` points as the plan leaves it.
    pub fn ref_target<'a>(&'a self, dag: &'a Dag, name: &str) -> Option<&'a Oid> {
        if let Some(target) = self.refs.get(name) {
            return Some(target);
        }
        dag.find_ref(name).map(|r| &r.target)
    }

    /// True when `id` was removed from the graph by a [`Edit::Drop`].
    pub fn is_dropped(&self, id: &Oid) -> bool {
        self.dropped.contains(id)
    }

    /// True when `id`'s parents differ from what git actually recorded — the
    /// trigger for the derivation to recreate it.
    pub fn parents_changed(&self, dag: &Dag, id: &Oid) -> bool {
        match self.parents.get(id) {
            // A commit past the loaded horizon has no recorded parents to
            // compare against, so any projection of it is a change.
            Some(projected) => dag.get(id).map(|c| &c.parents) != Some(projected),
            None => false,
        }
    }

    /// Walk `start`'s projected ancestry, visiting each commit once.
    ///
    /// Guarded against cycles by the visited set rather than by trusting the
    /// graph to be acyclic: a half-built projection during validation may not
    /// be, and that is exactly when this is called.
    pub fn ancestors<'a>(&'a self, dag: &'a Dag, start: &Oid) -> Vec<Oid> {
        let mut seen: HashSet<Oid> = HashSet::new();
        let mut out = Vec::new();
        let mut stack = vec![start.clone()];
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            out.push(id.clone());
            stack.extend(self.parents(dag, &id).iter().cloned());
        }
        out
    }

    /// Apply one edit in place.
    fn apply(&mut self, dag: &Dag, edit: &Edit) {
        match edit {
            Edit::Reparent { child, slot, new_parent } => {
                let mut parents = self.parents(dag, child).to_vec();
                match new_parent {
                    Some(p) if *slot < parents.len() => parents[*slot] = p.clone(),
                    Some(p) => parents.push(p.clone()),
                    None if *slot < parents.len() => {
                        parents.remove(*slot);
                    }
                    None => {}
                }
                self.parents.insert(child.clone(), parents);
            }
            Edit::Merge { into, from } => self.merge(dag, into, from),
            Edit::MoveRef { name, new_target } => {
                self.refs.insert(name.clone(), new_target.clone());
            }
            Edit::Drop { commit } => self.drop_commit(dag, commit),
        }
    }

    /// Grow a merge commit on `into` whose second parent is `from`.
    ///
    /// First parent is the branch being merged *into*, second is what is being
    /// merged — git's own convention, and what makes `--first-parent` follow
    /// the receiving branch afterwards.
    fn merge(&mut self, dag: &Dag, into: &str, from: &Oid) {
        let Some(tip) = self.ref_target(dag, into).cloned() else {
            return;
        };
        // Merging something already in the branch's history would produce
        // nothing; git calls it "already up to date" and so do we.
        if self.ancestors(dag, &tip).contains(from) {
            return;
        }
        let id = Oid::pending(self.pending.len());
        self.pending.push(Pending {
            id: id.clone(),
            parents: vec![tip, from.clone()],
            summary: format!("Merge {} into {into}", from.short()),
        });
        self.refs.insert(into.to_owned(), id);
    }

    /// Remove `commit`: everything pointing at it points at its first parent.
    ///
    /// Its first parent, not all of them — dropping a merge commit should
    /// leave the mainline intact rather than splicing the merged branch into
    /// every child.
    fn drop_commit(&mut self, dag: &Dag, commit: &Oid) {
        let replacement = self.parents(dag, commit).first().cloned();

        // Children are found by scanning: the snapshot records parent links
        // only in the child-to-parent direction, and a reverse index would
        // have to be rebuilt after every edit anyway.
        let children: Vec<Oid> = dag
            .commits()
            .iter()
            .map(|c| c.id.clone())
            .filter(|id| self.parents(dag, id).contains(commit))
            .collect();
        for child in children {
            let parents = self
                .parents(dag, &child)
                .iter()
                .filter_map(|p| {
                    if p != commit {
                        Some(p.clone())
                    } else {
                        replacement.clone()
                    }
                })
                // A merge of two branches that both led through the dropped
                // commit would otherwise list the replacement twice.
                .fold(Vec::new(), |mut acc: Vec<Oid>, p| {
                    if !acc.contains(&p) {
                        acc.push(p);
                    }
                    acc
                });
            self.parents.insert(child, parents);
        }

        // A ref sitting on the dropped commit has to go somewhere; its first
        // parent is where the history it named now ends.
        let landed: Vec<String> = dag
            .refs
            .iter()
            .map(|r| r.name.clone())
            .filter(|name| self.ref_target(dag, name) == Some(commit))
            .collect();
        for name in landed {
            match &replacement {
                Some(target) => {
                    self.refs.insert(name, target.clone());
                }
                None => {
                    self.refs.remove(&name);
                }
            }
        }

        self.dropped.insert(commit.clone());
    }
}

/// A [`Dag`] plus the edits made to it.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    edits: Vec<Edit>,
    /// The edit currently being *dragged*: included in the projection, so the
    /// graph rearranges live under the cursor, but not yet on the stack.
    /// Dropping the arrow is what commits it; moving on discards it.
    provisional: Option<Edit>,
}

impl Plan {
    pub fn edits(&self) -> &[Edit] {
        &self.edits
    }

    /// True when the plan would change nothing — no stack, no drag.
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.provisional.is_none()
    }

    /// The graph with every edit applied, in order, the provisional one last.
    ///
    /// This is what the renderer draws, so a drag in progress is visible.
    pub fn project(&self, dag: &Dag) -> Projection {
        let mut projection = self.project_committed(dag);
        if let Some(edit) = &self.provisional {
            projection.apply(dag, edit);
        }
        projection
    }

    /// The graph with only the *committed* stack applied — the drag excluded.
    ///
    /// What a drop means has to be decided against this, not against
    /// [`project`](Self::project).  Once a drag is being previewed the full
    /// projection already shows the result, so asking it "would this change
    /// anything?" answers no for every drop, and nothing can ever be
    /// committed.
    pub fn project_committed(&self, dag: &Dag) -> Projection {
        let mut projection = Projection::default();
        for edit in &self.edits {
            projection.apply(dag, edit);
        }
        projection
    }

    /// Show `edit` without committing it.  Replaces any previous drag.
    pub fn set_provisional(&mut self, edit: Edit) {
        self.provisional = Some(edit);
    }

    pub fn clear_provisional(&mut self) {
        self.provisional = None;
    }

    /// Push `edit` onto the stack, or reject it and change nothing.
    ///
    /// The one rejection is a cycle.  A commit that is its own ancestor has no
    /// git meaning at all, so it is refused here rather than at apply time —
    /// the view must not draw a shape that cannot exist.
    pub fn push(&mut self, dag: &Dag, edit: Edit) -> Result<(), String> {
        self.edits.push(edit);
        if let Some(cycle) = self.first_cycle(dag) {
            self.edits.pop();
            return Err(format!(
                "that would make {} its own ancestor",
                cycle.short()
            ));
        }
        Ok(())
    }

    /// Drop the most recent edit and return it.
    pub fn pop(&mut self) -> Option<Edit> {
        self.provisional = None;
        self.edits.pop()
    }

    pub fn clear(&mut self) {
        self.provisional = None;
        self.edits.clear();
    }

    /// The first commit reachable from itself in the current projection, if
    /// any.  `None` means the projected graph is a DAG.
    fn first_cycle(&self, dag: &Dag) -> Option<Oid> {
        let projection = self.project(dag);
        dag.commits()
            .iter()
            .map(|c| &c.id)
            .find(|id| {
                projection
                    .parents(dag, id)
                    .iter()
                    .any(|parent| projection.ancestors(dag, parent).contains(id))
            })
            .cloned()
    }
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

    fn branch(name: &str, target: &str) -> Ref {
        Ref {
            name: name.into(),
            kind: RefKind::Local,
            target: Oid::new(target),
            upstream: None,
        }
    }

    /// ```text
    ///   d (feature) ── c ─┐
    ///                     ├── a
    ///   f (main) ──── e ──┘
    /// ```
    fn dag() -> Dag {
        Dag::new(
            vec![
                commit("d", &["c"]),
                commit("f", &["e"]),
                commit("c", &["a"]),
                commit("e", &["a"]),
                commit("a", &[]),
            ],
            vec![branch("feature", "d"), branch("main", "f")],
            Head { branch: Some("main".into()), target: Some(Oid::new("f")) },
            WorkTree::default(),
            false,
        )
    }

    #[test]
    fn an_untouched_plan_projects_the_snapshot_unchanged() {
        let dag = dag();
        let plan = Plan::default();
        let p = plan.project(&dag);
        assert!(plan.is_empty());
        assert_eq!(p.parents(&dag, &Oid::new("d")), [Oid::new("c")]);
        assert_eq!(p.ref_target(&dag, "main"), Some(&Oid::new("f")));
        assert!(!p.parents_changed(&dag, &Oid::new("d")));
    }

    /// Moving the arrow at the base of a chain carries the chain with it:
    /// nothing above `c` was edited, but `d` still sits on top of it.
    #[test]
    fn reparenting_the_base_of_a_chain_carries_the_whole_chain() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(
            &dag,
            Edit::Reparent { child: Oid::new("c"), slot: 0, new_parent: Some(Oid::new("f")) },
        )
        .unwrap();
        let p = plan.project(&dag);

        assert_eq!(p.parents(&dag, &Oid::new("c")), [Oid::new("f")]);
        assert!(p.parents_changed(&dag, &Oid::new("c")));
        // `d` was never touched, and still follows `c`.
        assert!(!p.parents_changed(&dag, &Oid::new("d")));
        assert!(p.ancestors(&dag, &Oid::new("d")).contains(&Oid::new("f")));
    }

    #[test]
    fn detaching_an_arrow_makes_its_child_a_root() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Reparent { child: Oid::new("c"), slot: 0, new_parent: None })
            .unwrap();
        let p = plan.project(&dag);
        assert!(p.parents(&dag, &Oid::new("c")).is_empty());
        assert!(p.parents_changed(&dag, &Oid::new("c")));
    }

    /// A merge makes a **new** commit rather than giving an existing one
    /// another parent — which is what git does, and the only version of it
    /// the derivation can emit a command for.
    #[test]
    fn merging_grows_a_pending_commit_and_moves_the_branch_onto_it() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Merge { into: "main".into(), from: Oid::new("d") })
            .unwrap();
        let p = plan.project(&dag);

        assert_eq!(p.pending().len(), 1);
        let merge = &p.pending()[0];
        assert!(merge.id.is_pending());
        // First parent is the branch merged into, second is what came in —
        // git's own order, and what makes --first-parent follow main after.
        assert_eq!(merge.parents, vec![Oid::new("f"), Oid::new("d")]);
        assert_eq!(p.ref_target(&dag, "main"), Some(&merge.id));
        // The tip that was merged into is untouched: no commit was rewritten.
        assert!(!p.parents_changed(&dag, &Oid::new("f")));
        assert_eq!(p.parents(&dag, &merge.id), [Oid::new("f"), Oid::new("d")]);
    }

    /// Merging something the branch already contains produces nothing, the
    /// way `git merge` reports "already up to date".
    #[test]
    fn merging_an_ancestor_is_already_up_to_date() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Merge { into: "main".into(), from: Oid::new("a") })
            .unwrap();
        let p = plan.project(&dag);
        assert!(p.pending().is_empty());
        assert_eq!(p.ref_target(&dag, "main"), Some(&Oid::new("f")));
    }

    /// A commit cannot be its own ancestor.  Refused when the edit is pushed,
    /// so the view never draws a shape git has no meaning for.
    #[test]
    fn a_cycle_is_refused_and_leaves_the_stack_untouched() {
        let dag = dag();
        let mut plan = Plan::default();
        let err = plan
            .push(
                &dag,
                Edit::Reparent { child: Oid::new("a"), slot: 0, new_parent: Some(Oid::new("d")) },
            )
            .expect_err("a would become its own ancestor");
        assert!(err.contains("own ancestor"), "{err}");
        assert!(plan.is_empty(), "a rejected edit must not survive on the stack");
    }

    /// Self-parenthood is the degenerate cycle and must be caught too.
    #[test]
    fn a_commit_cannot_become_its_own_parent() {
        let dag = dag();
        let mut plan = Plan::default();
        assert!(plan
            .push(
                &dag,
                Edit::Reparent { child: Oid::new("c"), slot: 0, new_parent: Some(Oid::new("c")) }
            )
            .is_err());
    }

    #[test]
    fn dropping_a_commit_splices_its_children_onto_its_parent() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Drop { commit: Oid::new("c") }).unwrap();
        let p = plan.project(&dag);
        assert!(p.is_dropped(&Oid::new("c")));
        assert_eq!(p.parents(&dag, &Oid::new("d")), [Oid::new("a")]);
    }

    /// A ref standing on a dropped commit has to land somewhere; the first
    /// parent is where the history it named now ends.
    #[test]
    fn dropping_a_branch_tip_moves_the_branch_down() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Drop { commit: Oid::new("f") }).unwrap();
        let p = plan.project(&dag);
        assert_eq!(p.ref_target(&dag, "main"), Some(&Oid::new("e")));
    }

    /// Dropping the commit two branches merged through must not leave the
    /// replacement listed twice as a parent.
    #[test]
    fn dropping_a_shared_ancestor_does_not_duplicate_the_replacement() {
        let dag = Dag::new(
            vec![
                commit("m", &["x", "y"]),
                commit("x", &["s"]),
                commit("y", &["s"]),
                commit("s", &["r"]),
                commit("r", &[]),
            ],
            vec![branch("main", "m")],
            Head::default(),
            WorkTree::default(),
            false,
        );
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Drop { commit: Oid::new("x") }).unwrap();
        plan.push(&dag, Edit::Drop { commit: Oid::new("y") }).unwrap();
        let p = plan.project(&dag);
        assert_eq!(p.parents(&dag, &Oid::new("m")), [Oid::new("s")]);
    }

    /// The drag is visible in the projection but not on the stack, so moving
    /// on discards it and only dropping the arrow keeps it.
    #[test]
    fn a_provisional_edit_previews_without_committing() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.set_provisional(Edit::Reparent {
            child: Oid::new("c"),
            slot: 0,
            new_parent: Some(Oid::new("f")),
        });

        assert!(!plan.is_empty(), "a drag in progress is not an empty plan");
        assert_eq!(plan.edits().len(), 0, "and it is not on the stack yet");
        assert_eq!(plan.project(&dag).parents(&dag, &Oid::new("c")), [Oid::new("f")]);
        // …and the committed projection does not see it, which is what lets a
        // drop be told apart from a no-op.
        assert_eq!(
            plan.project_committed(&dag).parents(&dag, &Oid::new("c")),
            [Oid::new("a")]
        );
    }

    #[test]
    fn abandoning_a_drag_leaves_the_graph_as_it_was() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.set_provisional(Edit::Drop { commit: Oid::new("c") });
        plan.clear_provisional();
        assert!(plan.is_empty());
        assert!(!plan.project(&dag).is_dropped(&Oid::new("c")));
    }

    /// `u` in the grid pops a transform; here it pops an edit.  Popping is
    /// why the plan is a stack at all.
    #[test]
    fn popping_rebuilds_the_projection_from_what_is_left() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Drop { commit: Oid::new("c") }).unwrap();
        plan.push(
            &dag,
            Edit::MoveRef { name: "main".into(), new_target: Oid::new("a") },
        )
        .unwrap();

        assert_eq!(plan.pop().map(|e| e.label()), Some("main to a".to_string()));
        let p = plan.project(&dag);
        assert_eq!(p.ref_target(&dag, "main"), Some(&Oid::new("f")), "the move is undone");
        assert!(p.is_dropped(&Oid::new("c")), "the drop below it survives");

        plan.clear();
        assert!(plan.is_empty());
    }
}
