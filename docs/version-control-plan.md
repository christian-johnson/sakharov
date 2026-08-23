# Version control view — implementation plan

A visual, direct-manipulation git client: commits are blocks, parent links are
arrows, and you rearrange history by grabbing an arrow and dropping it
somewhere else. Nothing touches the repository until `:vc-apply`.

This is a design record. See CLAUDE.md for the shape of the code that exists.

## The bet

Every git porcelain teaches the *verbs* — rebase, cherry-pick, fast-forward,
reset --hard. But those verbs are implementation detail. What a user actually
wants is a **shape**: "this branch should sit on top of that one", "this commit
belongs over there". The shape is topological, and it is directly drawable.

So the model here is inverted from every other git UI:

1. Load the real DAG.
2. Let the user edit a *projection* of it — structurally, by moving arrows.
3. **Derive** the git operations that would make the repository match the
   projection.

Fast-forwarding is not a concept the user is taught. It is what the derivation
emits when a ref moves to a descendant and nothing needed rewriting.

## Why shelling out to `git`, not libgit2

Three reasons, in order of weight:

- **Applying must run the user's git.** Hooks, `.gitconfig`, aliases, the
  credential helper and the reflog are all things the user already has set up.
  A library reimplementation of `cherry-pick` is a subtly different one.
- **No credentials in the editor.** `push`/`pull` go through the user's own
  credential helper because they *are* the user's own git. This is the same
  rule the data layer already follows (see `docs/data-layer-plan.md`).
- **No dependency.** The DuckDB feature is this repo's standing lesson about
  what a "bundled" native library costs: +33 MB of binary and +7 ms of startup
  from 406 global constructors that run whether or not the feature is used.
  `git.rs` already shells out for the gutter; this is the same trade.

The cost, stated plainly: one process spawn per query, and parsing text meant
for humans. Both are bounded by using explicit `--format` strings with ASCII
record/field separators, and by loading once into a snapshot rather than
querying per frame.

## Layers

```
  vcs/mod.rs      Dag   — an immutable snapshot: commits, refs, HEAD, worktree
       │
       ▼
  vcs/plan.rs     Plan  — Dag + an ordered stack of Edits
       │           Projection — what the DAG looks like with the stack applied
       ▼
  vcs/derive.rs   Ops   — the git commands that would make reality match
       │
       ▼
  vcs/apply.rs          — runs them, after a backup ref per moved branch
```

Each layer is a pure function of the one above it, and only `apply.rs` can
write. That is what makes "everything is only a view until `:vc-apply`" a
property of the architecture rather than a promise.

### `Plan` is a stack, not a mutated copy

The same shape as `table::Session`'s transform stack, for the same reasons:
`u` pops (undo is free), the projection is rebuilt from scratch so a failed
edit cannot leave half-applied state, and — crucially — the stack records
**intent**. "This edge moved from P to Q" is what the derivation needs. A
structurally-diffed copy would have to guess it back out.

```rust
enum Edit {
    Reparent { child: Oid, slot: usize, new_parent: Option<Oid> },
    MoveRef  { name: String, new_target: Oid },
    AddParent { child: Oid, parent: Oid },   // a merge
    Drop     { commit: Oid },
}
```

A **provisional** edit sits beside the stack while an arrow is held: the
projection includes it, so the graph rearranges live under the cursor, and
dropping the arrow is what commits it to the stack.

## The derivation

The one genuinely non-obvious algorithm. Given a projection:

1. **Mark what must be recreated.** A commit needs recreating iff its projected
   parents differ from its real parents, **or any projected ancestor needs
   recreating**. The second clause is the whole reason this is a fixpoint and
   not a filter: cherry-picking a rewritten parent yields a new oid, so
   everything downstream of it is new too, even though the user never touched
   those commits.

2. **Per ref, find the base.** Walk the ref's projected first-parent chain to
   the newest commit that does *not* need recreating. Everything above it is
   the replay list, in reverse topological order.

3. **Emit.** For each ref whose target moved or whose chain contains a
   recreated commit:

   ```
   checkout --detach <base>
   cherry-pick <replay...>          # omitted when the replay list is empty
   branch -f <ref> HEAD
   ```

   An empty replay list is exactly the case every other tool calls a
   fast-forward (or a reset, depending on direction). It needs no special case
   here, and no name.

4. **Bracket the whole run.** `checkout --detach <HEAD oid>` first, and
   `checkout <original branch>` last. A branch cannot be `branch -f`'d while it
   is checked out, and detaching for the duration is simpler than special-casing
   whichever branch happens to be current.

### Refusals

- A **dirty worktree** blocks apply. Cherry-picking over uncommitted work is
  how people lose it. (Stashing on the user's behalf is a decision they should
  make; the message says so.)
- A **cycle** in the projection is refused at edit time, not at apply time — a
  commit cannot become its own ancestor, and the view should not draw a shape
  that has no git meaning.
- A **conflict** during apply stops the run and leaves the repository mid
  cherry-pick, with the conflicted paths reported. Per the roadmap the user
  resolves them in the ordinary editor for now; the merge-conflict resolver is
  its own view, later.

### Undo

Before the first write, every ref the plan will move gets its current target
saved under `refs/sakharov/undo/<timestamp>/<ref>`. `:vc-undo` restores them.
This is a real ref, so it also keeps the old commits alive against `gc` —
which the reflog does too, but only for 90 days and only for refs that were
checked out.

## Roadmap

- **V1 — shipped.** The DAG view, arrow-dragging, derive + apply + undo, and
  the everyday non-topological actions: checkout, stage/unstage, commit, fetch,
  pull, push, set-upstream. See CLAUDE.md ("Phase V1") for what the code
  actually looks like and `docs/commands.md` for the keys.

  Two things changed shape during the build, both worth recording:

  - **A merge is a new commit, not another parent on an existing one.** The
    first cut modelled it as `AddParent { child, parent }`, which is a thing
    git cannot do — it never adds a parent to a commit, it makes a new one
    whose parents are the two tips. So the projection grew `Pending` nodes:
    commits the plan would create, with no object behind them, drawn as blocks
    like any other so the user can see the merge before agreeing to it.
  - **A drag preview must be validated like a committed edit.** Setting the
    provisional edit directly bypassed `Plan::push`'s cycle check, so dragging
    an arrow over its own child drew a cyclic graph — tracks and all — and only
    objected on release. `state::VcsState::validated` runs the same check
    before previewing, so the picture is always one that could exist.

- **V2** — a merge-conflict resolver view (its own `View` variant: ours/theirs/
  merged panes over a conflicted file). Until then a conflict stops the apply,
  names the files, and is resolved in the ordinary editor followed by
  `:vc-continue`.
- **Also open** — search within the graph, and replaying a merge (which needs
  the equivalent of `rebase --rebase-merges`; today it is refused by name).
- **Not planned** — bisect, submodules, worktrees, interactive add by hunk.
