3. UX audit — recommendations (no code changed)

Ordered by how much I think each buys.

A. Show the blast radius of a plan, not just its shape. This is the biggest gap. derive already computes the fixpoint of "which commits need recreating" — every commit above a moved arrow gets a new oid even if nobody touched it. On screen those blocks look identical to untouched ones. Draw them in the pending style (or hatched/dimmed borders) the moment the plan changes. A user who drags an arrow and sees nine blocks turn pale has learned what a rebase is without being told the word.

B. Say what the gesture would do, live. vcs_selection already shows held → target. Extend it, or the message line, with the derived consequence while something is held: "c would follow f — 3 commits replayed" / "main moves forward, nothing replayed". The derivation is already a pure function; running it on the provisional plan each move costs nothing and turns the preview from "the picture moved" into "here is what will happen".

C. A ? overlay for the whole view. g has which-key hints; the primary gestures (Space, Enter, c, s, S, w, d, m, u, r, q) have none. ? should open a which-key popup split by the two kinds of action — "Planned (nothing happens until :vc-apply)" vs "Immediate (runs now)". That distinction is the single most important thing about this view and currently lives only in docs/commands.md.

D. A persistent banner while a plan is pending. A ✎3 planned chip in the status line is easy to miss. When the plan is non-empty, a one-line strip along the bottom of the graph — "3 planned changes · ga apply · u take back · gx discard" — makes the unreal state impossible to be surprised by.

E. Colour the two kinds of action in the message line. d (plan a drop) and s (stage everything, right now) are adjacent letters with wildly different consequence. Every planned action's message should share one colour/prefix (✎ planned:) and every immediate one another (✓ done:), so the category is learned by repetition rather than by reading.

F. A first-open hint. The first time the view is opened in a session, put "Space picks up an arrow, a commit or a branch — move to another commit and press Space again" in the message line. One sentence teaches the entire interaction model, and the view currently teaches it nowhere.

G. Time ruler along the top. Now that the axis is time, mark it: a faint caption where the date changes between adjacent commits (── Mon 18 Aug ──). Instant sense of pace — "these four were one afternoon, then nothing for a week" — for free, since the dates are already loaded.

H. Ahead/behind on branch labels. Upstreams are already loaded specifically so ahead/behind is computable, but nothing shows it. main ↑2 on the label answers "do I need to push?" without leaving the view.

I. Edge cues for off-screen history. The graph is usually wider than the terminal and there is no sign of it. A ‹ / › chevron in the gap column at each edge (and ⌃/⌄ for tracks below the fold) — the horizon marker at the far left already does this well; it just needs a sibling for "scrolled".

J. Collapse the blank summary row. A one-line commit message curreetween the summary and the metadata. It reads as a rendering bugrather than as whitespace. Either top-align the metadata when the summary is one line, or fill the second row with the commit body's first line.           
K. Make untracked files actionable from the list. The motivating case is scrap notebooks. Listing them is step one; i on a selected entry to append it to  .gitignore, and d to delete it (with a confirmation), turns "I cancleared it". Both are immediate actions and neither can make acommit unreachable, so they sit on the right side of your existing line.                                                                                   
L. Prune the backup namespace. refs/sakharov/undo/<stamp>/* accumulates one namespace per apply, forever, with no way to list or clear it from the view. A :vc-undo-list (and dropping all but the last N on apply) keeps gitfilling with them.

M. Name the empty state's next step. "no commits yet — this reposiw" is honest but terminal. Add "stage with s, then :vc-commit<message>".