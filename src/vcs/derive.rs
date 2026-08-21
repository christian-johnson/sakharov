//! Turning a [`Projection`] back into git commands.
//!
//! This is the piece that lets the view be topological.  The user rearranges
//! a *shape*; nobody says the word "rebase", and nothing here asks them to.
//! What comes out the other side is a list of [`Op`]s — and, notably, a
//! fast-forward is not a case in this code.  It is what gets emitted when a
//! ref moves and the replay list happens to be empty.
//!
//! Pure: it reads a [`Dag`] and a [`Plan`] and returns a list.  Running the
//! list is [`super::apply`]'s job, and is the only place anything is written.
//!
//! ## The algorithm
//!
//! 1. **Mark what must be recreated.**  A commit needs recreating if its
//!    projected parents differ from its real ones, *or if any projected
//!    ancestor does*.  The second clause is why this is a fixpoint rather than
//!    a filter: cherry-picking a rewritten parent yields a new object id, so
//!    every commit above it is new too — even the ones the user never touched.
//! 2. **Per ref, find the base.**  Walk the ref's projected first-parent chain
//!    down to the newest commit that does *not* need recreating.  Everything
//!    above it is the replay list.
//! 3. **Emit** `checkout --detach <base>`, `cherry-pick <replay…>`,
//!    `branch -f <ref> HEAD`.

use std::collections::HashSet;

use super::{
    plan::{Edit, Plan, Projection},
    Dag, Oid,
};

/// One git invocation.
///
/// A closed enum rather than a formatted string: the confirmation popup shows
/// these to the user, and [`super::apply`] runs them, and those two must be
/// looking at the same thing.  A string built for display would be re-parsed
/// or, worse, shown while something subtly different ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Leave HEAD detached at a commit, so no branch is checked out while the
    /// branches are being moved.
    Detach(Oid),
    /// Replay commits, in the given (oldest-first) order.
    CherryPick(Vec<Oid>),
    /// Point a branch somewhere.  `at: None` means wherever HEAD now is,
    /// which is where the replay left it.
    SetBranch { name: String, at: Option<Oid> },
    /// Create a merge commit on the current branch.
    Merge(Oid),
    /// Check a branch back out — the last thing a run does.
    Checkout(String),
}

impl Op {
    /// The git arguments, exactly as [`super::apply`] will run them.
    pub fn args(&self) -> Vec<String> {
        let s = |v: &str| v.to_string();
        match self {
            Op::Detach(oid) => vec![s("checkout"), s("--detach"), oid.to_string()],
            Op::CherryPick(oids) => {
                let mut args = vec![s("cherry-pick")];
                args.extend(oids.iter().map(Oid::to_string));
                args
            }
            Op::SetBranch { name, at } => {
                let mut args = vec![s("branch"), s("--force"), name.clone()];
                args.push(at.as_ref().map_or_else(|| s("HEAD"), Oid::to_string));
                args
            }
            Op::Merge(oid) => vec![s("merge"), s("--no-ff"), oid.to_string()],
            Op::Checkout(name) => vec![s("checkout"), name.clone()],
        }
    }

    /// How the op reads in the confirmation popup: `git` plus its arguments,
    /// with hashes abbreviated so a replay of six commits still fits a line.
    pub fn describe(&self) -> String {
        let short = |a: &String| match Oid::new(a.clone()) {
            oid if oid.as_str().len() == 40 => oid.short().to_string(),
            _ => a.clone(),
        };
        let args: Vec<String> = self.args().iter().map(short).collect();
        format!("git {}", args.join(" "))
    }
}

/// Work out the git commands that would make the repository match `plan`.
///
/// `Err` is a refusal to try, not a failure: a shape that is perfectly
/// drawable may still be one git has no single command for, and saying which
/// is more use than emitting something that half-works.
pub fn derive(dag: &Dag, plan: &Plan) -> Result<Vec<Op>, String> {
    if plan.is_empty() {
        return Ok(Vec::new());
    }
    let projection = plan.project(dag);
    let recreate = commits_needing_recreation(dag, &projection);

    let mut ops = Vec::new();

    // Nothing is done while a branch that might move is checked out: git
    // refuses to force a checked-out branch, and detaching for the duration is
    // simpler than special-casing whichever branch happens to be current.
    if let Some(head) = dag.head.target.clone() {
        ops.push(Op::Detach(head));
    }

    for branch in dag.local_branches() {
        let name = branch.name.clone();
        let Some(target) = projection.ref_target(dag, &name).cloned() else {
            continue;
        };
        // A pending merge is not a commit that can be replayed — it does not
        // exist yet.  It is emitted below, from the edit that asked for it.
        if target.is_pending() {
            continue;
        }
        let replay = replay_chain(dag, &projection, &recreate, &target)?;

        if replay.is_empty() {
            // Nothing to rewrite.  If the ref also did not move, there is
            // nothing to do at all; if it did, pointing it at its new target
            // *is* the whole operation — which is what every other tool calls
            // a fast-forward (or a reset), and needs no name here.
            if target != branch.target {
                ops.push(Op::SetBranch { name, at: Some(target) });
            }
            continue;
        }

        let base = base_of(dag, &projection, &recreate, &target);
        match base {
            Some(base) => ops.push(Op::Detach(base)),
            None => {
                return Err(format!(
                    "{name} would be replayed onto no parent at all; drop those commits instead"
                ))
            }
        }
        ops.push(Op::CherryPick(replay));
        ops.push(Op::SetBranch { name, at: None });
    }

    // Merges last: they build on branches that the replay above may have just
    // moved, and a merge of a stale tip would merge the wrong thing.
    for edit in plan.edits() {
        if let Edit::Merge { into, from } = edit {
            ops.push(Op::Checkout(into.clone()));
            ops.push(Op::Merge(from.clone()));
        }
    }

    // Put HEAD back where the user left it.  A branch that moved is checked
    // out at its new position, which is what they asked for.
    match (&dag.head.branch, &dag.head.target) {
        (Some(branch), _) => ops.push(Op::Checkout(branch.clone())),
        (None, Some(oid)) => ops.push(Op::Detach(oid.clone())),
        (None, None) => {}
    }

    Ok(prune(ops))
}

/// Every commit whose object id would change.
///
/// The fixpoint from the module docs, computed by walking the projected graph
/// parents-first so a commit is only decided once its ancestors are known.
fn commits_needing_recreation(dag: &Dag, projection: &Projection) -> HashSet<Oid> {
    let mut recreate = HashSet::new();
    for id in projected_order(dag, projection) {
        let changed = projection.parents_changed(dag, &id);
        let ancestor_moved = projection
            .parents(dag, &id)
            .iter()
            .any(|p| recreate.contains(p));
        if changed || ancestor_moved {
            recreate.insert(id);
        }
    }
    recreate
}

/// The projected graph in parents-first order.
///
/// Not the snapshot's order: an edit can point a commit at one that git listed
/// *after* it, and deciding a commit before its new parent would miss exactly
/// the propagation this exists to compute.  Cycles are impossible by the time
/// this runs ([`Plan::push`] refuses them) but the visited set makes it
/// terminate regardless, since a partially-built plan can be projected too.
fn projected_order(dag: &Dag, projection: &Projection) -> Vec<Oid> {
    let mut order = Vec::new();
    let mut done: HashSet<Oid> = HashSet::new();

    let roots = dag
        .commits()
        .iter()
        .map(|c| c.id.clone())
        .chain(projection.pending().iter().map(|p| p.id.clone()));

    for root in roots {
        if done.contains(&root) {
            continue;
        }
        // Iterative post-order: `(id, expanded)`.  The second visit is the one
        // that emits, by which point every parent already has.
        let mut stack = vec![(root, false)];
        let mut on_stack: HashSet<Oid> = HashSet::new();
        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                if done.insert(id.clone()) {
                    order.push(id);
                }
                continue;
            }
            if done.contains(&id) || !on_stack.insert(id.clone()) {
                continue;
            }
            stack.push((id.clone(), true));
            for parent in projection.parents(dag, &id) {
                if !done.contains(parent) {
                    stack.push((parent.clone(), false));
                }
            }
        }
    }
    order
}

/// The commits above `target` that must be replayed, oldest first.
///
/// Follows first parents only.  A merge commit cannot be recreated by
/// `cherry-pick` — this is the same limit `git rebase` has without
/// `--rebase-merges` — so one in the replay list is refused by name rather
/// than silently flattened into its first parent's side.
fn replay_chain(
    dag: &Dag,
    projection: &Projection,
    recreate: &HashSet<Oid>,
    target: &Oid,
) -> Result<Vec<Oid>, String> {
    let mut chain = Vec::new();
    let mut cursor = Some(target.clone());
    let mut guard = HashSet::new();

    while let Some(id) = cursor {
        if !recreate.contains(&id) || !guard.insert(id.clone()) {
            break;
        }
        if projection.parents(dag, &id).len() > 1 {
            return Err(format!(
                "{} is a merge, and a merge cannot be replayed onto a new parent",
                id.short()
            ));
        }
        chain.push(id.clone());
        cursor = projection.parents(dag, &id).first().cloned();
    }

    chain.reverse();
    Ok(chain)
}

/// The newest commit at or below `target` that keeps its identity — where the
/// replay starts from.
fn base_of(
    dag: &Dag,
    projection: &Projection,
    recreate: &HashSet<Oid>,
    target: &Oid,
) -> Option<Oid> {
    let mut cursor = Some(target.clone());
    let mut guard = HashSet::new();
    while let Some(id) = cursor {
        if !guard.insert(id.clone()) {
            return None;
        }
        if !recreate.contains(&id) {
            return Some(id);
        }
        cursor = projection.parents(dag, &id).first().cloned();
    }
    None
}

/// Drop ops that would do nothing.
///
/// The bracketing detach and checkout are emitted unconditionally because
/// whether they are needed depends on what the loop in between produced; it is
/// clearer to emit them and remove them than to predict them.
///
/// Two things go: a run whose net effect is nothing (which would still move
/// the user's HEAD and back for no reason), and a detach immediately
/// superseded by another — the bracketing one, when the first branch's replay
/// begins by detaching somewhere else anyway.  Both are noise in a
/// confirmation popup, and a popup listing steps that do nothing trains the
/// user not to read it.
fn prune(ops: Vec<Op>) -> Vec<Op> {
    let does_work = ops
        .iter()
        .any(|op| matches!(op, Op::CherryPick(_) | Op::SetBranch { .. } | Op::Merge(_)));
    if !does_work {
        return Vec::new();
    }
    let mut out: Vec<Op> = Vec::with_capacity(ops.len());
    for op in ops {
        // A detach is pure repositioning, so the last one before any real work
        // is the only one that mattered.
        if matches!(op, Op::Detach(_)) && matches!(out.last(), Some(Op::Detach(_))) {
            out.pop();
        }
        out.push(op);
    }
    out
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
        Ref { name: name.into(), kind: RefKind::Local, target: Oid::new(target), upstream: None }
    }

    /// ```text
    ///   feature:  d ── c ─┐
    ///                     ├── a
    ///   main:     f ── e ─┘
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

    fn described(ops: &[Op]) -> Vec<String> {
        ops.iter().map(Op::describe).collect()
    }

    #[test]
    fn an_empty_plan_derives_nothing_at_all() {
        assert_eq!(derive(&dag(), &Plan::default()).unwrap(), Vec::new());
    }

    /// The headline case.  The user moved one arrow — `c` onto `f` — and the
    /// derivation works out that `d` has to come along, because replaying `c`
    /// gives it a new object id.  Nobody typed "rebase".
    #[test]
    fn moving_one_arrow_replays_the_whole_chain_above_it() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(
            &dag,
            Edit::Reparent { child: Oid::new("c"), slot: 0, new_parent: Some(Oid::new("f")) },
        )
        .unwrap();

        assert_eq!(
            described(&derive(&dag, &plan).unwrap()),
            vec![
                "git checkout --detach f",
                "git cherry-pick c d",
                "git branch --force feature HEAD",
                "git checkout main",
            ]
        );
    }

    /// `d` was never touched by the user, but it sits above a rewritten
    /// commit, so its object id changes too.  This is the fixpoint clause; a
    /// filter over "what the user edited" would replay only `c` and silently
    /// drop `d` off the branch.
    #[test]
    fn a_commit_above_a_rewritten_one_is_recreated_even_though_it_was_untouched() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(
            &dag,
            Edit::Reparent { child: Oid::new("c"), slot: 0, new_parent: Some(Oid::new("e")) },
        )
        .unwrap();
        let projection = plan.project(&dag);
        let recreate = commits_needing_recreation(&dag, &projection);

        assert!(recreate.contains(&Oid::new("c")), "the edited commit");
        assert!(recreate.contains(&Oid::new("d")), "and everything above it");
        assert!(!recreate.contains(&Oid::new("a")), "but nothing below");
        assert!(!recreate.contains(&Oid::new("f")), "nor anything off the chain");
    }

    /// Moving a branch to a descendant is the thing every other tool calls a
    /// fast-forward.  Here it is simply the case where the replay list came
    /// out empty — no special case, no name, and nothing rewritten.
    #[test]
    fn moving_a_ref_with_nothing_to_replay_is_just_a_branch_move() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(
            &dag,
            Edit::MoveRef { name: "main".into(), new_target: Oid::new("d") },
        )
        .unwrap();

        let ops = derive(&dag, &plan).unwrap();
        assert_eq!(
            described(&ops),
            vec![
                "git checkout --detach f",
                "git branch --force main d",
                "git checkout main",
            ]
        );
        assert!(
            !ops.iter().any(|op| matches!(op, Op::CherryPick(_))),
            "nothing is rewritten by a ref move alone"
        );
    }

    /// Moving a ref *backwards* — what git calls a reset — is the same case.
    #[test]
    fn moving_a_ref_backwards_needs_no_separate_concept_either() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::MoveRef { name: "main".into(), new_target: Oid::new("a") })
            .unwrap();
        assert!(described(&derive(&dag, &plan).unwrap())
            .contains(&"git branch --force main a".to_string()));
    }

    /// An edit whose net effect is nothing must not still move the user's
    /// HEAD around: the bracketing detach/checkout are pruned with it.
    #[test]
    fn a_plan_that_changes_nothing_emits_nothing_not_a_bare_checkout() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::MoveRef { name: "main".into(), new_target: Oid::new("f") })
            .unwrap();
        assert_eq!(derive(&dag, &plan).unwrap(), Vec::new());
    }

    /// The replay order must be oldest-first: cherry-pick applies its
    /// arguments in the order given, and reversing them conflicts immediately.
    /// The bracketing detach and the replay's own base detach collapse: a
    /// confirmation popup listing a step that is immediately undone trains the
    /// reader to skip it.
    #[test]
    fn a_detach_immediately_superseded_by_another_is_dropped() {
        let ops = prune(vec![
            Op::Detach(Oid::new("f")),
            Op::Detach(Oid::new("a")),
            Op::CherryPick(vec![Oid::new("c")]),
        ]);
        assert_eq!(
            ops,
            vec![Op::Detach(Oid::new("a")), Op::CherryPick(vec![Oid::new("c")])]
        );
    }

    #[test]
    fn the_replay_list_is_oldest_first() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(
            &dag,
            Edit::Reparent { child: Oid::new("c"), slot: 0, new_parent: Some(Oid::new("f")) },
        )
        .unwrap();
        let ops = derive(&dag, &plan).unwrap();
        let picks = ops
            .iter()
            .find_map(|op| match op {
                Op::CherryPick(list) => Some(list.clone()),
                _ => None,
            })
            .expect("a replay happened");
        assert_eq!(picks, vec![Oid::new("c"), Oid::new("d")]);
    }

    /// `git cherry-pick` cannot recreate a merge, which is the same limit
    /// `git rebase` has without `--rebase-merges`.  Saying so beats flattening
    /// the merge into one side without mentioning it.
    #[test]
    fn replaying_a_merge_is_refused_by_name() {
        let dag = Dag::new(
            vec![
                commit("m", &["x", "y"]),
                commit("x", &["a"]),
                commit("y", &["a"]),
                commit("a", &[]),
            ],
            vec![branch("main", "m"), branch("other", "y")],
            Head { branch: Some("main".into()), target: Some(Oid::new("m")) },
            WorkTree::default(),
            false,
        );
        let mut plan = Plan::default();
        plan.push(
            &dag,
            Edit::Reparent { child: Oid::new("x"), slot: 0, new_parent: Some(Oid::new("y")) },
        )
        .unwrap();
        let err = derive(&dag, &plan).expect_err("a merge is in the replay list");
        assert!(err.contains("merge"), "{err}");
    }

    /// Replaying onto nothing is an orphan branch, which is a genuinely
    /// different operation — refused with a reason rather than half-done.
    #[test]
    fn replaying_onto_no_parent_is_refused_rather_than_attempted() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Reparent { child: Oid::new("c"), slot: 0, new_parent: None })
            .unwrap();
        let err = derive(&dag, &plan).expect_err("feature has no base to sit on");
        assert!(err.contains("no parent"), "{err}");
    }

    /// A merge is a new commit, so it is emitted as `git merge` on the branch
    /// rather than as a replay — and after any rewriting, so it merges the
    /// branch's final position and not a stale one.
    #[test]
    fn a_merge_is_emitted_as_a_merge_on_the_receiving_branch() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Merge { into: "main".into(), from: Oid::new("d") })
            .unwrap();
        assert_eq!(
            described(&derive(&dag, &plan).unwrap()),
            vec![
                "git checkout --detach f",
                "git checkout main",
                "git merge --no-ff d",
                "git checkout main",
            ]
        );
    }

    /// A detached HEAD must be left detached: checking a branch out on the
    /// user's behalf would silently change what they were working on.
    #[test]
    fn a_detached_head_is_put_back_detached() {
        let mut dag = dag();
        dag.head = Head { branch: None, target: Some(Oid::new("c")) };
        let mut plan = Plan::default();
        plan.push(&dag, Edit::MoveRef { name: "main".into(), new_target: Oid::new("d") })
            .unwrap();
        let ops = derive(&dag, &plan).unwrap();
        assert_eq!(ops.last(), Some(&Op::Detach(Oid::new("c"))));
        assert!(!ops.iter().any(|op| matches!(op, Op::Checkout(_))));
    }

    /// The ops the popup shows must be the ops that run — one enum, rendered
    /// two ways, rather than a display string built beside the real thing.
    #[test]
    fn what_is_described_is_what_would_run() {
        let long = Oid::new("a3f91c2b7d4e5f60718293a4b5c6d7e8f9012345");
        let op = Op::CherryPick(vec![long.clone()]);
        // The description abbreviates for the reader…
        assert_eq!(op.describe(), "git cherry-pick a3f91c2");
        // …while the arguments keep the full hash for git.
        assert_eq!(op.args(), vec!["cherry-pick".to_string(), long.to_string()]);
    }
}
