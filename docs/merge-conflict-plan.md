# Merge-conflict resolver — implementation plan

A view for the one thing git is worst at explaining.

**Status: built.** This is the design record; CLAUDE.md ("Phase V2") describes
the code that exists and `docs/commands.md` has the key reference. Where the
implementation departed from the plan below, a note says so. See
`docs/version-control-plan.md` for the version-control view this hands off
from.

## The problem, stated exactly

Git's answer to a conflict is to write both versions into your file between
markers and leave:

```
<<<<<<< HEAD
    timeout = 30
=======
    timeout = 60
>>>>>>> 9f2c1ab (bump the timeout)
```

Three things are wrong with this, and they are three different problems:

1. **The labels are not names.** `HEAD` is a pointer, not a place. `9f2c1ab`
   is a hash. Neither tells you *whose work this is*, *when it was written*, or
   *which branch you would be choosing*. The one question a person actually has
   — "is the 30 mine or theirs?" — is the question the markers refuse to
   answer.
2. **Ours and theirs invert during a replay, silently.** In a merge, `HEAD` is
   your branch. In a rebase or a cherry-pick — which is what *this editor's own
   `g a` runs* — git checks out the base and replays your commits onto it, so
   `HEAD` is **the branch you are moving onto** and the incoming side is **your
   own commit**. Everyone gets this backwards, including people who know it,
   because the word "ours" is doing the opposite of what it says.
3. **The common ancestor is missing by default.** Without it you cannot tell
   *who changed what*. `30 → 60` on one side and `30 → 45` on the other is a
   real disagreement; `30 → 60` against an untouched `30` is not a
   disagreement at all, and git presents them identically.

The markers also put the resolver in the worst possible place: inside the file,
as text, where the only tool is deleting lines by hand and where forgetting a
`>>>>>>>` commits a syntax error.

## The bet

Same bet as the graph view. **State the shape; the editor works out the
commands.** Here the shape is smaller: for each conflicted region, which
side's text ends up in the file. So the view's whole job is to make that choice
*legible* — labelled, dated, attributed, side by side — and then to be the only
thing that writes.

The one design commitment that follows: **the working file's markers are never
parsed.** They are git's rendering of the conflict, in whatever
`merge.conflictStyle` the user happens to have, and reading them back is
reading our own output. The view reads the **index stages** instead, which are
the actual data.

## Where the data comes from

A conflicted path has up to three blobs in the index:

| Stage | git's name | What it is |
|-------|-----------|------------|
| 1 | base | the merge base — the last version both sides agreed on |
| 2 | "ours" | the version on `HEAD` |
| 3 | "theirs" | the incoming version |

**Built as described.** `git ls-files -u -z` lists the conflicted paths and which stages each has
(a file added on both sides has no stage 1; a modify/delete conflict is
missing 2 or 3 — both are real cases the model must carry rather than crash
on). `git cat-file blob :1:path` and friends fetch each version whole.

**The hunks come from a three-way diff we run ourselves**, not from the file:
`git merge-file -p --diff3 <ours> <base> <theirs>` over the three blobs written
to temporaries. The invocation is ours, so the marker style is ours regardless
of the user's config, and the base region is always present. Parsing our own
controlled output is a different proposition from parsing whatever was left in
the working tree.

*Built, with one thing the plan did not anticipate:* git labels the opening,
ancestor and closing markers with whatever `-L` is passed, but emits the
`=======` separator **bare**, with no label. Length is therefore all that
distinguishes it, so the merge runs with `--marker-size=32` and the separator is
only ever matched *inside* a conflict, where the surrounding labelled markers
have already established that this is our own output being read back.

### Resolving the labels

This is the part the whole view exists for. Each side is resolved to a name, a
commit, an author and a date, from the operation actually in progress:

| In progress | Detected by | Side 2 ("ours") is | Side 3 ("theirs") is |
|-------------|-------------|--------------------|----------------------|
| merge | `.git/MERGE_HEAD` | the branch you are on | the branch being merged in |
| cherry-pick | `.git/CHERRY_PICK_HEAD` | where you are replaying onto | the commit being replayed |
| rebase / a `g a` replay | `.git/rebase-merge/`, `REBASE_HEAD` | **the branch you are moving onto** | **your own commit** |
| revert | `.git/REVERT_HEAD` | the branch you are on | the commit being undone |

Each of those heads is an oid, so the label is a full commit block's worth of
fact — resolved with the same `git log --format` the graph already uses, and
worded in the same register:

```
  ┌─ On origin/main ──────────────────┐   ┌─ Your commit, being replayed ─────┐
  │ 9f2c1ab · Ada · 3 days ago        │   │ 4b1e7d0 · you · 20 minutes ago    │
  │ bump the request timeout          │   │ make the timeout configurable     │
```

The `rebase` row is stated in the header in words, not left to the reader:
during a replay the panes are labelled *"On <branch you are landing on>"* and
*"Your commit, being replayed"* — never "ours" and "theirs", which are the two
words that caused the problem.

## The model

Four layers, each a pure function of the one above, and only the last writes —
the same split as `vcs/`, for the same reason:

```
conflict/load.rs      ConflictSet  — an immutable snapshot: the conflicted
                                     files, their three stages, the resolved
                                     labels, and what operation is in progress
conflict/hunk.rs      Hunk         — the three-way diff, as a list of regions:
                                     agreed text, and conflicted text with its
                                     three versions
conflict/resolve.rs   Resolution   — a stack of choices, one per hunk; folds
                                     into the merged file text
conflict/write.rs                  — writes the file and `git add`s it.  The
                                     only module here that can lose anything
```

`Resolution` is a **stack**, not a mutated buffer, exactly as `Plan` and
`table::Session` are: `u` pops, a rejected choice cannot leave half-applied
state, and the merged text is always a fold from scratch over the snapshot, so
it cannot drift out of step with what the panes show.

### Choosing is a set of switches, not a menu

The obvious design is a menu — take ours / take theirs / take both / edit. The
better one, and the one that matches the language this editor already uses in
the branch picker and the staging view, is that **each side of a hunk is a
switch**, and the merged text is whichever sides are on, in file order:

| Sides on | What it means |
|----------|---------------|
| ours | take your version |
| theirs | take the incoming version |
| both | keep both, in that order (the common case for imports, list entries) |
| neither | the region is deleted, which is a real resolution and one no marker-editing workflow makes easy |

`Space` toggles the focused side. That is four resolutions and a reordering
from one key, and there is no menu to read. Anything genuinely needing a merge
rather than a choice gets `e`, which drops the hunk's text into an ordinary
buffer where the whole editor works, and takes the edited text back as that
hunk's resolution.

## The view

`View::Conflict`, a fifth variant, following the "Adding a view" table in
CLAUDE.md. Not a popup: resolving a conflict is a task with a cursor, its own
motions and its own scroll, which is what a view is.

**Layout.** Two panes side by side, ours left and theirs right, with a footer
showing the merged result for the focused hunk as it will actually be written.
On a terminal too narrow to give each side a readable column
(`< 2 × MIN_PANE`) the panes stack vertically instead — the same decision
`table::layout` makes about columns, and made in one place for the same reason.

**The base is a third pane on `3`**, off by default. It answers "who changed
what", which is the question you have only *sometimes*; showing it always costs
a third of the width on every conflict, including the many where both sides are
plainly different intents.

**A single geometry model**, `conflict/layout.rs`, owning pane rectangles, the
hunk row spans and which rows are on screen. The renderer and the scroll math
both derive from it, as they do in `table::layout` and `vcs::layout` — a
resolver whose cursor and whose drawn highlight disagree about which hunk is
focused is worse than no resolver.

**Colour** comes from the theme, per the standing rule: added/removed reuse the
git gutter colours the editor already has, and a resolved hunk is dimmed to the
same weight the staging view dims a staged file — so "what is left to do"
is readable down the page at a glance.

### Keys

Grouped the way the graph's `?` sheet is, and reachable the same way:

| Key | |
|-----|---|
| `n` / `N` | next / previous unresolved hunk |
| `h` / `l` | focus the ours / theirs pane |
| `Space` | take (or drop) the focused side |
| `a` / `b` | take only ours / only theirs, for this hunk |
| `A` / `B` | take only ours / only theirs, for **every** remaining hunk |
| `3` | show the common ancestor |
| `e` | edit this hunk's merged text in an ordinary buffer |
| `u` | undo the last choice |
| `d` | this file's diff against the base |
| `Enter` | mark the file resolved: write it and `git add` |
| `]` / `[` | next / previous conflicted file |
| `q` | leave (choices are kept in the stash, as every other view's are) |
| `?` | the key sheet |

### Getting there

Three doors, all of them from where the problem is noticed:

- The apply failure already names the conflicted files; it gains "Enter opens
  the resolver".
- `Enter` on a conflicted file in the staging view (`w`), which already draws
  conflicts first and marks them red.
- `:conflicts`, from anywhere, for the conflict you left and came back to.

Leaving the last file resolved offers `g C` (`:vc-continue`) directly, since
that is what a person wants next in every case and is the step most easily
forgotten.

## What changed in the building

- **The `Enter`/`e` split held**, but `e` seeds the buffer with whatever is
  currently chosen, so `a` then `e` is "take this side and adjust it" — which
  turned out to be the common shape of a hand merge, and is free.
- **`A`/`B` only settle what is *unanswered*.** The plan said "for every
  remaining hunk" without saying what "remaining" meant; overwriting deliberate
  answers makes it a key nobody can risk pressing.
- **The result strip earned its place** and then some: for *keep both* the
  result is a thing neither pane contains, so two panes of alternatives do not
  by themselves answer "so what does that give me".
- **The message line carries the inversion too.** The panes say it, but a line
  on the way in — "your work is on the RIGHT in a replay" — is what stops the
  first `a` being pressed out of habit.
- **`Choice::default()` is *neither side*, not "left".** Defaulting to the
  left — which is what git's markers effectively do — would let a file be
  written resolved without anybody having looked at it, which is the failure the
  whole view exists to prevent.
- **The `[`/`]` file keys and `n`/`N` vs `j`/`k` are two different questions.**
  `n` tours what is left; `j` reads the file. An `n` that stopped on settled
  hunks is the wrong tour in a file with forty conflicts and two outstanding.

## What it will not do

- **No auto-merging beyond what git already did.** git's own three-way merge
  has already taken every region the two sides agree on; a resolver that
  guessed at the rest would be guessing at exactly the regions git declined to.
- **No conflict style config.** The view renders from the stages; there is
  nothing for `merge.conflictStyle` to change.
- **No rerere.** Worth revisiting once the view exists, but replaying a
  remembered resolution silently is the same class of surprise this view is
  built to remove.
- **It does not run `git commit`.** Resolving writes files and stages them;
  finishing the operation is `g C`, which is where the existing view already
  puts it.

## Order of work

1. `conflict/load.rs` — the snapshot and the label resolution, with the
   parsers as pure functions of git's output (the way `vcs/load.rs` is split),
   since a rebase-in-progress with a modify/delete conflict is not something
   you can build as a fixture any other way.
2. `conflict/hunk.rs` + `resolve.rs` — the diff and the fold. Pure, and the
   place a test can assert that "both sides on" produces exactly the
   concatenation, and that a fold over an empty stack reproduces the file with
   markers removed and ours taken, which is git's own default.
3. `conflict/write.rs` against a real repository in a temp dir, as
   `vcs/apply.rs`'s tests do: this layer's whole job is driving git.
4. The view: `layout.rs`, `conflict_ui.rs`, `exec/conflict.rs`, and the
   thirteen sites the `View` match arms point at.
5. The doors, and the `g C` hand-off.
