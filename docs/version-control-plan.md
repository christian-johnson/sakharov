# Version control view — implementation plan

A working-tree-first git client: changed files as a tree on the left, the
selected file's side-by-side diff (or its merge-conflict resolver) on the
right, and a running list of planned steps. Nothing touches the repository
until `:vc-apply`.

This replaces the commit-graph design (movable commit blocks). The graph
turned out to be hard to read and harder to maintain (`vcs/layout.rs` alone
was ~2,200 lines), and the everyday job is "what changed, what goes in this
commit, how do I settle this conflict", which the graph didn't help with.

This is a design record. See CLAUDE.md for the shape of the code that exists.

## What stays, what goes

Kept, because they hold up regardless of the UI:

- **Shell out to `git`, not libgit2.** Hooks, config, credential helpers and
  the reflog are the user's own; every process goes through
  `vcs::load::git_command`.
- **Layers that are pure until the last one.** load → plan → derive → apply,
  and only `apply` writes refs, the index or the work tree.
- **The plan is a stack.** `u` pops, the projection is rebuilt from scratch,
  and the stack records intent.
- **A backup before the first write**, under `refs/sakharov/undo/<stamp>/`.
- **`conflict/`**: the index-stage loading, `merge-file --diff3` regions,
  switch-per-side choices, the label resolution and `left_is_yours`.

Deleted:

- `vcs/layout.rs`, the graph half of `vcs_ui.rs`, the drag state in
  `vcs/state.rs`, `Edit::Reparent`/`MoveRef`/`AddParent`/`Drop` and the
  cherry-pick derivation that served them.
- The graph commands: grab, drop, flip, visible/hide branch, guide, and the
  `[vcs] orientation` setting and `vcs_hidden` status module.
- `View::Conflict` as a separate view. The resolver becomes a panel inside the
  version-control view (see below); `:conflicts` opens that view on the first
  conflicted file.
- The old staging popup (`popup::StageState`) — the new view is that popup,
  grown up.

## The model

```
vcs/load.rs      Repo       — immutable snapshot: HEAD, branches, tags,
     |                        stashes, in-progress operation, and per changed
     |                        path its HEAD / index / work-tree versions
     v
vcs/plan.rs      Plan       — Repo + an ordered stack of Steps
     |           project()  — folds the steps over the snapshot -> a Repo
     v
vcs/derive.rs    Vec<Op>    — the git commands that make reality match
     v
vcs/apply.rs                — backup, staleness check, run
```

### The projection is a `Repo`

`Plan::project` returns the same type `load` does. The tree, the diff panel,
the resolver and the status line only ever draw a `Repo`; none of them knows
whether it is the real one or the projected one. That is what makes "the UI
shows the repository as it will be after apply" cheap to keep true: there is
no second rendering path to drift.

A `Repo` holds, for each path that differs anywhere between HEAD, the index
and the work tree:

```rust
struct PathState {
    head:  Option<Version>,   // None = not in HEAD
    index: IndexEntry,        // Absent | Version | Conflicted { base, ours, theirs }
    work:  Option<Version>,   // None = deleted / not present
}
enum Version { Object(Oid), Text(Arc<str>) }   // Text = produced by the plan
```

Content is fetched lazily (`git cat-file` for objects, the file system for
the work tree) and cached, so a plan that stages one change in a 10,000-file
repository reads one file.

Commits the plan will create are `Pending` commits, as before: a parent, a
message and a tree described as overrides on the parent's tree. The
projected HEAD points at one.

### Steps

```rust
enum Step {
    // Index and work tree
    Stage   { path: String, scope: Scope },   // Scope = File | Dir | Change(ChangeKey)
    Unstage { path: String, scope: Scope },
    Discard { path: String, scope: Scope },
    Resolve { path: String, result: Option<Entry> },
    // History
    Commit  { message: String, amend: bool },
    Reset   { to: Target, mode: ResetMode },  // Soft | Mixed
    CherryPick { commit: Oid },
    Revert     { commit: Oid },
    Merge      { from: Target },
    Rebase     { onto: Target },
    // Refs
    Switch  { branch: String },
    CreateBranch { name: String },            // at HEAD, and switch to it
    DeleteBranch { name: String },
    RenameBranch { from: String, to: String },
    Tag     { name: String, at: Target },
    // Stash
    StashPush { message: String },
    StashPop { index: usize }, StashApply { index: usize }, StashDrop { index: usize },
    // Finishing the operation in progress, planned or already there
    Continue,
    Abort,
}
```

A directory scope is one step, not one per file, so the plan list stays
readable. `ChangeKey` identifies one change
by its old and new lines plus a line hint, so a step still finds its change
after an unrelated edit higher up the file.

Each step is validated against the projection *before* it is pushed, with the
same rules git would apply — switching branches with dirty paths that differ
between the two trees is refused, as `git switch` would refuse it. A step that
would fail at apply time is refused in the UI, where it is cheap to hear about.

### Simulating what git would do

| Step | How the projection computes it |
|------|--------------------------------|
| stage / unstage / discard | In memory: the index (or work-tree) text for that path becomes HEAD's, the work tree's, or a splice of the two for one change |
| resolve | The existing `conflict` fold over the regions |
| commit / amend | A `Pending` commit whose tree is the projected index |
| reset soft / mixed | Move HEAD; mixed also resets the projected index |
| switch | Read the target's tree (`git ls-tree`), carry dirty paths across, refuse where git would |
| branch / tag | Ref map edits |
| stash push / pop / apply / drop | push: projected index + work tree become a pending stash entry and the paths go clean. pop/apply: a three-way merge of the stash onto the projected work tree |
| merge / cherry-pick / revert / rebase | `git merge-tree --write-tree` (one call per replayed commit for rebase and cherry-pick ranges) |

**`git merge-tree` is the piece that makes the history steps simulatable.**
It runs the same `ort` merge `git merge` runs, without touching the index or
work tree, and reports the conflicted paths with their three stages. Those
stages feed straight into `conflict/`, so a planned merge that will conflict
**shows its conflicts in the resolver before anything has run**.  The
projection stops there, exactly as git will (`Repo::operation` is set); the
answers are ordinary `Resolve` steps, and `Continue` finishes the merge or
replays the rest of a rebase, stopping again at the next conflict.

The one relaxation of "the layers above apply don't write": to merge a tree
that only exists in the plan, the simulation writes **unreferenced objects**
(`git hash-object -w`, a scratch index's `write-tree`, `commit-tree`,
`merge-tree --write-tree`). Never a ref,
the index or the work tree. Unreachable objects are invisible to the user and
pruned by `gc`. This only happens when a history step follows a planned
commit; staging-and-committing plans write nothing.

**What can't be simulated, said plainly:**

- **Hooks.** A `pre-commit` hook can reject a commit or rewrite files. Apply
  stops at a rejected commit; after any apply the view reloads and shows what
  actually happened, not what was projected.
- **Custom merge drivers** in `.gitattributes`. `merge-tree` honours them, but
  a driver that behaves differently on a second run will differ. Apply checks
  for this (below).

## Apply

`derive` turns the projection into ops that set **target state**, not
replays of user gestures:

- Index entries: `git hash-object -w --stdin` then
  `git update-index --cacheinfo <mode>,<oid>,<path>` (or `--force-remove`). Each
  step writes the text the path should hold after it, never a patch.
- Work-tree files (discard, resolve): `buffer::atomic_write` / remove.
- Everything else is the git porcelain the step names: `git commit -m`,
  `git switch`, `git merge`, `git rebase`, `git cherry-pick`, `git revert
  --no-edit`, `git stash ...`, `git reset`, `git branch`, `git tag`, with
  `-c core.editor=true` where git would otherwise open an editor.

Ops run in plan order.

When a history op stops on a conflict, apply compares git's real conflict
stages with the simulated ones. If they match, it writes the planned
resolutions, stages them and continues (`git commit --no-edit` /
`rebase --continue`). If they differ, it stops, reloads, and the view opens on
the real conflict.

### Before the first write

1. **Staleness check.** HEAD, every branch tip, the stash list, the
   in-progress operation, and the index entry and work-tree content hash of
   every path the plan touches must equal the snapshot. Otherwise: refuse and
   say what changed ("src/app.rs changed on disk since the view loaded — `r`
   to reload"). This replaces the old blanket "work tree must be clean" rule,
   which made no sense once staging is part of the plan.
2. **Backups.** Every local branch, HEAD and every stash as refs under
   `refs/sakharov/undo/<stamp>/` (so `gc` keeps their commits), plus a copy of
   the index file and of every changed work-tree file — untracked ones
   included — under `<git dir>/sakharov/undo/<stamp>/`.  The index is copied
   rather than written as a tree because an index with conflicts in it cannot
   be written as one.
3. **Confirm.** A float lists every op exactly as it will run (`Op::describe`
   is still the single source of what is shown and what runs).

`:vc-undo` aborts any operation left in progress, restores the branches and
HEAD, resets the work tree to that HEAD, copies back the changed files and the
index, re-stores any dropped stash, and deletes the backup.

### Reload and the plan

`r` re-reads the repository and **re-validates the stack** against the new
snapshot instead of discarding it. Steps that still apply are kept; steps that
don't (the change they staged is gone, the branch they switch to was deleted)
are dropped and named in `*Messages*`. The editor saving a file you have
planned steps against is the common case, and losing the whole plan for it
would teach people not to plan.

## The view

```
+ Changes ------------------+ src/app.rs ------------------------------------+
| v src/                  ~ |  HEAD                  | working tree          |
|     app.rs              * |  118  fn draw(...)     |  118  fn draw(...)    |
|     ui.rs               . |  119- let w = 3;       |  119+ let w = width;  | S
| v docs/                 . |                        |  120+ debug!(w);      | U
|     notes.md            ! |  ...                                          |
|   new_file.txt          ? |                                               |
+ Plan (3) -----------------+                                               |
| 1 stage src/app.rs (2/3)  |                                               |
| 2 stage docs/             |                                               |
| 3 commit "Fix draw width" |                                               |
+---------------------------+-----------------------------------------------+
 main -> main+1 . 3 planned . 1 conflict                      ? keys
```

**Left: the changes tree.** Every path that differs between HEAD, index and
work tree (untracked included), as a collapsible directory tree. Each row's
mark says staged / unstaged / partly staged / untracked / conflicted, and a
directory's mark summarises its children. Conflicted files sort first within
their directory and are coloured as such. Renames are not detected (`status
--no-renames`): a rename is a deletion and an addition, each staged on its own.

**Left, below: the plan.** Always visible, because a plan you can't see is
one you forget is pending. It can be focused: `j`/`k` walk the steps, `x`
removes one. Removing a step from the middle re-projects from scratch; if a
later step no longer applies the removal is refused with the reason.

**Right: the selected file.**

- **Ordinary change:** one side-by-side diff, **HEAD on the left, the work
  tree on the right**, aligned row-for-row. Each change carries a staged /
  unstaged / partly-staged mark in the gutter and in its colour. Partly staged
  means the index holds a third version of that region (staged, then edited
  again); `Space` on it stages the work-tree version.
- **Conflicted file:** the existing resolver, unchanged in behaviour, drawn in
  this panel. Its "write and stage" becomes a `Resolve` step.
- **Untracked / added:** left side empty. **Deleted:** right side empty.
  **Binary:** a one-line size and type summary. Too narrow for two readable
  columns: one column, the way `conflict/layout.rs` already stacks.

**Changes are 0-context blocks.** The unit `j`/`k` stops on and `Space`
stages is each contiguous run of changed lines, not git's 3-line-context hunk
(which merges nearby edits into one and would make "stage just that one"
impossible).

Diffs are computed in-process (HEAD vs work tree, HEAD vs projected index)
with `similar`, since the projected index is not something `git diff` can see.
A diff is built once per selection or plan change, not per frame, and a file
over 20,000 lines is not diffed at all.

**One geometry model per panel** (`vcs/layout.rs` is rewritten for this:
tree rows, plan rows, diff row alignment), shared by renderer and scroll math
as everywhere else in the editor.

### Focus and keys

`Tab` cycles tree -> diff -> plan. In the tree, `l`/`Enter` on a file moves
into the diff, `h`/`l` on a directory folds/unfolds. In the diff, `j`/`k` step
change to change, `Ctrl+d`/`Ctrl+u` scroll, `h`/`Esc` goes back to the tree.
`]`/`[` jump to the next/previous file from anywhere.

Proposed bindings, all as rows in `keymap::defaults` so the `?` which-key
sheet and the prefix popups are generated, never hand-written:

| Key | Where | Step / action |
|-----|-------|---------------|
| `Space` | tree, diff | stage / unstage the file, directory or change |
| `d` | tree, diff | discard the file, directory or change |
| `a` / `A` | anywhere | stage all / unstage all |
| `c` / `C` | anywhere | commit / amend — opens a `*commit-message*` buffer (the existing `commit` layer); `q` saves it as a step |
| `b` prefix | anywhere | `bb` switch, `bn` new, `bd` delete, `br` rename, `bm` merge into current, `bR` rebase current onto |
| `s` prefix | anywhere | `ss` stash push, `sp` pop, `sa` apply, `sd` drop (each opens a stash picker where it needs one) |
| `L` | anywhere | commit log float; inside: `p` cherry-pick, `v` revert, `r`/`R` reset soft/mixed, `t` tag |
| `u` | anywhere | take back the last planned step |
| `ga` | anywhere | `:vc-apply` |
| `gx` | anywhere | discard the whole plan |
| `gU` | anywhere | `:vc-undo` (undo the last apply) |
| `gC` / `gA` | in-progress op | plan continue / plan abort |
| `r` | anywhere | reload, keeping the plan |
| `q` | anywhere | leave (the plan is stashed with the view, as every view's state is) |
| `?` | anywhere | every key, grouped |

The resolver's own keys (`Space`, `a`/`b`, `A`/`B`, `n`/`N`, `e`, `3`) apply
when the diff panel is showing a conflict, through a sub-layer; the which-key
sheet shows whichever set is live.

Status line: `vcs_head` shows real and projected HEAD when they differ
(`main -> main+1`), `vcs_plan` the step count, `vcs_worktree` the projected
counts.

## Testing

End-to-end against real temporary repositories, as `vcs/apply.rs` and
`conflict/` tests already do. One invariant test carries most of the weight,
in the spirit of `pushdown_and_local_execution_agree`:

> **`projection_and_apply_agree`** — for a plan, `project(snapshot)` equals
> `load()` after `apply` in a scratch repository: same HEAD, refs, index
> entries, work-tree contents and stash list.

A new `Step` variant must extend `project`, `derive` **and** this test.
Alongside it: staleness refusal, backup + `:vc-undo` round trip, reload
re-validation, and a simulated merge whose conflict stages equal git's.

## What changed in the building

All seven phases shipped together.  Where the code differs from the plan
above, the code is right and this records why:

- **Planned commits are realised as real (unreferenced) commit objects** when
  a later step needs to ask git about them — `merge-tree` and `rev-list` want
  commits, not trees — through `git commit-tree` over a tree built in a scratch
  index file.  Still never a ref, the index or the work tree.
- **Every step is one git command, or target state.**  Working-tree steps
  compare the projection before and after; a whole file staged as it is on
  disk becomes `git add` (so clean filters run), anything else
  `hash-object` + `update-index --cacheinfo`.
- **A merge fast-forwards when it can**, as `git merge` does.
- **The commit message is written in a buffer**: `c` opens
  `*commit message*` listing what is staged, `q` adds the commit to the plan.
- **The diff reads the disk**, not the editor's unsaved buffer.
- **Line-level staging inside a change** is not built yet.
- **Abort is approximate in the projection**: it keeps only untracked files
  and, for a rebase, puts the branch back.  The real abort may keep more; the
  view re-reads the repository after applying either way.
- **A stash that would conflict is refused** rather than projected, with the
  paths named.
- **The resolver's own `d` (diff) and `:conflict-revert` are gone**: the
  ancestor pane covers the first, and `u` / `x` on the plan cover the second.
- **Push, pull and fetch were removed** along with the graph, as the scope
  section below says, and with them the streaming `*git output*` transcript.
  A failed apply shows git's own message (a hook's output included) in a
  float instead.

## Not in scope (for now)

- **Push, pull, fetch.** They depend on a remote the editor can't simulate,
  and mixing "immediate" network ops into a planned stack needs its own design.
- **Bulk conflict resolution across files** ("take main everywhere"). Wanted;
  deferred until the per-file panel is solid. The design notes from the
  discussion: name sides by branch, never ours/theirs; per-region vs
  whole-file are different operations; only settle unanswered regions.
- **Interactive rebase** (reorder / squash / edit). The step list could grow
  into it later via the log float.
- Submodules, worktrees, bisect, LFS.
