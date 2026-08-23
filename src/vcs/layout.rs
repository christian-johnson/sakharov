//! Where every block and arrow goes.
//!
//! The single geometry model for the version-control view — the same role
//! `table::layout` plays for the grid and `notebook_ui::nb_cell_height` for
//! the notebook.  The renderer draws from a [`Layout`] and the navigation
//! moves through the same [`Layout`], so "what is under the cursor" and "what
//! is on screen" cannot drift apart.
//!
//! ## The shape
//!
//! Time runs **left to right**: the oldest commit loaded is at the left edge,
//! the newest at the right, and an arrow points *backwards* — from a commit to
//! the parent it follows.  That is the direction people already read a
//! timeline in, and it is the direction the branch metaphor is drawn in
//! everywhere else (a branch comes *off* a line and rejoins it further along).
//!
//! One commit per column band, and **tracks** give the vertical position: a
//! commit's track is inherited by its first parent, so a chain stays in a row
//! and a branch point opens another.
//!
//! Commits are *not* packed several to a column even when they would fit.  A
//! generation-packed layout looks tidier, but it can place a commit visually
//! to the left of one of its own descendants, and in a view whose entire
//! purpose is that the picture is the truth, that is not a cosmetic problem.

use std::collections::HashMap;

use super::{
    plan::Projection,
    Dag, Head, Oid,
};

/// Rows in a block: border, two summary rows, metadata, border.
///
/// Every block is the same height, HEAD included, so a track is a row band of
/// one fixed size and the arrows between two blocks in one track are a
/// straight line.
pub const BLOCK_H: u16 = 5;
/// Columns between one block and the next, where the arrows are drawn.
pub const GAP: u16 = 3;
/// Rows between one track and the next.
pub const TRACK_GAP: u16 = 1;
/// Narrowest a block may be.
pub const MIN_BLOCK: u16 = 22;
/// Widest a block grows, so one long commit message does not eat the whole
/// terminal and leave two commits on screen.
pub const MAX_BLOCK: u16 = 38;
/// Columns budgeted for the relative age on a commit's metadata row.
///
/// An estimate rather than the rendered string: the width has to be settled
/// before anything is drawn, and `11mo ago` is the longest [`relative_time`]
/// produces.
///
/// [`relative_time`]: super::relative_time
const AGE_COLS: u16 = 8;
/// Columns budgeted for the `+123 -45` change counts, which are written along
/// the block's bottom border.
const COUNTS_COLS: u16 = 12;
/// Columns the abbreviated hash takes on a block's top border, with its
/// surrounding spaces — where the ref labels start.
pub const HASH_COLS: u16 = super::SHORT_LEN as u16 + 3;

/// What the cursor can be on.
///
/// Arrows are focusable because moving one *is* the feature: an edge is the
/// only handle on "which commit does this one follow", which is the single
/// fact every history rewrite changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    /// A commit block.
    Commit(Oid),
    /// The arrow from `child`'s parent link `slot`.
    Edge { child: Oid, slot: usize },
    /// A branch, tag or remote label sitting on a block.
    Ref(String),
    /// The HEAD block.
    Head,
}

/// What a block is drawn as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Commit,
    /// A merge the plan would create — no object behind it yet.
    Pending,
    Head,
}

/// One block's place in the stack.
#[derive(Debug, Clone)]
pub struct Block {
    pub id: Oid,
    pub kind: BlockKind,
    /// Which row band the block sits in.
    pub track: usize,
    /// Leftmost column, measured across the whole graph rather than the
    /// viewport.
    pub col: u16,
    /// Ref labels drawn along this block's top border, with the column each
    /// starts at (relative to the block).  Computed here rather than in the
    /// renderer so the focusable positions and the drawn positions agree.
    pub labels: Vec<(String, u16)>,
    /// Which colour group this commit belongs to — an index the theme cycles
    /// its palette over.  See [`assign_tints`].
    pub tint: usize,
}

/// One parent link.
#[derive(Debug, Clone)]
pub struct Edge {
    pub child: Oid,
    pub slot: usize,
    /// `None` when the parent is past the loaded horizon — the arrow is drawn
    /// as a stub trailing off the left edge, which is the honest picture.
    pub parent: Option<Oid>,
    pub from_track: usize,
    pub to_track: usize,
    /// Column the arrow starts in (immediately left of the child block).
    pub col: u16,
    /// Column the arrowhead sits in (immediately right of the parent block),
    /// or 0 when the parent was never loaded.
    pub end_col: u16,
    /// The column the arrow changes track on.
    ///
    /// A first-parent link crosses **late**, in the gap immediately right of
    /// the commit it points at, so the long part of the run stays in the
    /// child's own track — which that chain owns outright.  A merge's second
    /// parent crosses **early**, immediately left of the child, because the
    /// child's track continues on to its own first parent and the run would
    /// go straight through it.  Either way the horizontal never enters a
    /// track somebody else's blocks are sitting in.
    pub cross_col: u16,
}

/// Where one arrow's cells actually go.
///
/// The route lives here rather than in the renderer because it is geometry,
/// and this module is the one place geometry is decided — the same reason
/// block positions and track assignment are here.  It is also what makes
/// [`no_arrow_is_drawn_through_a_block`] able to check the invariant the whole
/// track assignment exists to provide.
pub struct EdgeRoute {
    /// Row the arrow leaves along, and the row it arrives along.
    pub from_row: u16,
    pub to_row: u16,
    /// First column of the arrow, the column it changes track on, and the
    /// column the arrowhead sits in.  Columns *decrease* along the arrow:
    /// it points back in time.
    pub start: u16,
    pub cross: u16,
    pub head_col: u16,
    /// A parent past the loaded horizon: a stub that visibly goes nowhere.
    pub stub: bool,
}

impl EdgeRoute {
    /// Every cell the arrow paints.
    pub fn cells(&self) -> Vec<(u16, u16)> {
        if self.stub {
            return (self.start.saturating_sub(1)..=self.start)
                .map(|col| (self.from_row, col))
                .collect();
        }
        let (lo, hi) = (self.from_row.min(self.to_row), self.from_row.max(self.to_row));
        (self.cross + 1..=self.start)
            .map(|col| (self.from_row, col))
            .chain((lo..=hi).map(|row| (row, self.cross)))
            .chain((self.head_col..self.cross).map(|col| (self.to_row, col)))
            .collect()
    }
}

/// Something the cursor can sit on, and where it is.
///
/// Positions are `(row, col)` across the whole graph rather than track
/// indices, so several labels on one block's border are distinguishable and
/// `h`/`l` walks them in the order they are drawn.
#[derive(Debug, Clone)]
pub struct Focusable {
    pub focus: Focus,
    pub row: u16,
    pub col: u16,
}

/// The whole drawn graph.
pub struct Layout {
    pub blocks: Vec<Block>,
    pub edges: Vec<Edge>,
    pub focusables: Vec<Focusable>,
    pub track_count: usize,
    pub block_width: u16,
    /// The colour group each local branch label belongs to, so a label is
    /// drawn the same colour as the commits that are on it.
    pub branch_tints: HashMap<String, usize>,
    /// Total width of the graph, for the scroll anchor to clamp against.
    pub total_cols: u16,
    index: HashMap<Oid, usize>,
}

/// Which way a motion goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Layout {
    /// The block for `id`, if it is drawn.
    pub fn block(&self, id: &Oid) -> Option<&Block> {
        self.index.get(id).map(|&i| &self.blocks[i])
    }

    /// Where `focus` sits, if it is still on screen.
    ///
    /// Returns `None` after an edit removes what the cursor was on — a dropped
    /// commit, a detached arrow — which the caller answers by re-seeding the
    /// cursor rather than by keeping a stale one.
    pub fn locate(&self, focus: &Focus) -> Option<&Focusable> {
        self.focusables.iter().find(|f| f.focus == *focus)
    }

    /// Where `edge` is drawn.
    ///
    /// The arrowhead sits one column *right* of the parent's border: blocks
    /// are drawn after arrows so a line entering a box reads as passing behind
    /// it, which means anything drawn on the border itself is overwritten.
    pub fn route(&self, edge: &Edge) -> EdgeRoute {
        let head_col = edge.end_col.min(edge.col);
        EdgeRoute {
            from_row: self.arrow_row(edge.from_track),
            to_row: self.arrow_row(edge.to_track),
            start: edge.col,
            cross: edge.cross_col.clamp(head_col, edge.col),
            head_col,
            stub: edge.parent.is_none(),
        }
    }

    /// The screen row a track starts at.
    pub fn track_row(&self, track: usize) -> u16 {
        track as u16 * (BLOCK_H + TRACK_GAP)
    }

    /// The row arrows run along within a track: the middle of a block, so a
    /// link between two blocks in one track is a straight line through the
    /// gap between them.
    pub fn arrow_row(&self, track: usize) -> u16 {
        self.track_row(track) + BLOCK_H / 2
    }

    /// Columns from one block's left edge to the next one's.
    pub fn col_stride(&self) -> u16 {
        self.block_width + GAP
    }

    /// How many whole tracks fit in `height`.
    ///
    /// The `+ TRACK_GAP` is not a fudge: tracks are laid out with a gap
    /// *between* them, so N tracks occupy `N * (h + gap) - gap`.  Dividing the
    /// bare height instead reports one track too few whenever they fit
    /// exactly, which scrolled a two-branch graph on a screen tall enough for
    /// all of it.
    pub fn visible_tracks(&self, height: u16) -> usize {
        ((height + TRACK_GAP) / (BLOCK_H + TRACK_GAP)).max(1) as usize
    }

    /// Width available inside a block's borders.
    pub fn block_inner(&self) -> u16 {
        self.block_width.saturating_sub(2)
    }

    /// The nearest focusable in `dir` from `current`.
    ///
    /// Motion **along time** (`h`/`l`) sorts by distance travelled first, so
    /// `h` from a block lands on the arrow immediately to its left rather than
    /// on whatever happens to be furthest back.
    ///
    /// Motion **across tracks** (`j`/`k`) sorts the other way round — nearest
    /// column first, then nearest row.  A track is wide and short, so the
    /// thing two rows down is very often forty columns away (a label on some
    /// other branch's block), and travelling to it is not what `j` means.
    /// What `j` means is "the next branch, beside where I am".
    ///
    /// Restricted to the focusables `allow` accepts.  Used while something is
    /// being dragged: the walk is over *destinations* then, and stepping onto
    /// something you cannot drop on is a press that does nothing except take
    /// the cursor further from somewhere useful.
    pub fn step_where(
        &self,
        current: &Focus,
        dir: Dir,
        allow: impl Fn(&Focus) -> bool,
    ) -> Option<Focus> {
        let from = self.locate(current)?;
        let (row, col) = (from.row as i32, from.col as i32);

        self.focusables
            .iter()
            .filter(|f| f.focus != *current)
            .filter(|f| allow(&f.focus))
            .filter_map(|f| {
                let (dr, dc) = (f.row as i32 - row, f.col as i32 - col);
                let along = match dir {
                    Dir::Down => dr,
                    Dir::Up => -dr,
                    Dir::Right => dc,
                    Dir::Left => -dc,
                };
                // Strictly forward along the axis of travel; ties on the other
                // axis are what the second sort key resolves.
                (along > 0).then(|| {
                    let key = match dir {
                        Dir::Left | Dir::Right => (along, dr.abs()),
                        Dir::Up | Dir::Down => (dc.abs(), along),
                    };
                    (key, f.focus.clone())
                })
            })
            .min_by_key(|(key, _)| *key)
            .map(|(_, focus)| focus)
    }

    /// The first thing worth putting the cursor on: HEAD if it is drawn, else
    /// the newest thing in the graph, which is the right-hand end.
    pub fn initial_focus(&self) -> Option<Focus> {
        self.focusables
            .iter()
            .find(|f| f.focus == Focus::Head)
            .or_else(|| self.focusables.last())
            .map(|f| f.focus.clone())
    }
}

/// Lay out `dag` as `projection` leaves it, for a content area `width` wide.
pub fn compute(dag: &Dag, projection: &Projection, width: u16) -> Layout {
    let order = draw_order(dag, projection);
    // The block width settles first: where a block sits along the time axis is
    // a multiple of it, and the track assignment then needs those columns to
    // know which chains overlap and therefore cannot share a row.
    let block_width = block_width(width, natural_width(dag, projection, &order));
    let (cols, head_col, total_cols) = assign_cols(dag, &order, block_width);
    let placed = place(&order, &cols, head_col, block_width, dag, projection);
    let inner = block_width.saturating_sub(2);

    let mut blocks = Vec::new();

    // HEAD is drawn as a block rather than as one more label because it is a
    // different kind of thing from a branch — it is where *you* are — and it
    // is the one pointer always worth finding at a glance.  It sits directly
    // to the right of the commit it names, on the newer side, where the next
    // commit would go; see `place_head` for the one case where it cannot also
    // sit in that commit's track.
    if let Some(col) = head_col {
        blocks.push(Block {
            id: Oid::new("HEAD"),
            kind: BlockKind::Head,
            track: placed.head_track,
            col,
            labels: Vec::new(),
            tint: 0,
        });
    }

    for id in &order {
        let kind = if id.is_pending() { BlockKind::Pending } else { BlockKind::Commit };
        blocks.push(Block {
            id: id.clone(),
            kind,
            track: placed.track.get(id).copied().unwrap_or(0),
            col: cols[id],
            labels: labels_for(dag, projection, id, inner),
            tint: placed.tint.get(id).copied().unwrap_or(0),
        });
    }
    blocks.sort_by_key(|b| b.col);

    let index: HashMap<Oid, usize> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.clone(), i))
        .collect();

    let edges = build_edges(&blocks, &index, dag, projection, block_width);
    let mut layout = Layout {
        focusables: Vec::new(),
        blocks,
        edges,
        track_count: placed.track_count,
        block_width,
        branch_tints: placed.branch_tints,
        total_cols,
        index,
    };
    layout.focusables = build_focusables(&layout, &dag.head);
    layout
}

/// Commits to draw, newest first: the pending ones the plan would create,
/// then the snapshot's own topological order minus anything dropped.
///
/// Still newest-first, even though the drawing is oldest-leftmost: the chain
/// assignment depends on every child being visited before its parents, which
/// is what git's topological order gives.  [`assign_cols`] walks this list
/// backwards to put the oldest commit at column zero.
fn draw_order(dag: &Dag, projection: &Projection) -> Vec<Oid> {
    projection
        .pending()
        .iter()
        .map(|p| p.id.clone())
        .chain(
            dag.commits()
                .iter()
                .map(|c| c.id.clone())
                .filter(|id| !projection.is_dropped(id)),
        )
        .collect()
}

/// Where every block sits along the time axis: one per column band, oldest
/// first.
///
/// Returns the commits' columns, HEAD's column if it is drawn, and the total
/// width of the graph.  Every block occupies the same column bands whatever
/// track it ends up in, which is what makes the gap columns between them gaps
/// in *every* track — and that is what lets an arrow change track without ever
/// running through a block.
fn assign_cols(dag: &Dag, order: &[Oid], block_width: u16) -> (HashMap<Oid, u16>, Option<u16>, u16) {
    let mut cols = HashMap::new();
    let stride = block_width + GAP;
    // Room at the left for the stub that says older history was not loaded.
    let mut col = if dag.truncated { GAP } else { 0 };
    let mut head_col = None;

    let head_at = dag.head.target.clone().filter(|id| order.contains(id));
    // A repository with no commits yet still has a HEAD worth showing: it says
    // which branch the first commit will be on.
    if head_at.is_none() && (dag.head.target.is_some() || dag.head.branch.is_some()) {
        head_col = Some(col);
        col += stride;
    }

    // Backwards: `order` is newest-first, and the oldest commit belongs at the
    // left edge.
    for id in order.iter().rev() {
        cols.insert(id.clone(), col);
        col += stride;
        // HEAD sits immediately to the right of the commit it names — the slot
        // the next commit would take.
        if head_col.is_none() && head_at.as_ref() == Some(id) {
            head_col = Some(col);
            col += stride;
        }
    }
    (cols, head_col, col.saturating_sub(GAP))
}

/// Where every chain and the HEAD block sit vertically, and what colour each
/// commit is.
struct Placement {
    track: HashMap<Oid, usize>,
    head_track: usize,
    track_count: usize,
    tint: HashMap<Oid, usize>,
    branch_tints: HashMap<String, usize>,
}

/// Give each commit a track, and each a colour group.
///
/// Three steps, and the whole point of the first two is that a **chain owns
/// its row outright** for as long as it is on screen:
///
/// 1. **Chains.**  A chain is a maximal run of first-parent links — which is
///    what a branch looks like to a reader.  Where several commits share a
///    parent, the parent joins the chain that *started newest*, so the trunk
///    keeps going along one row instead of being annexed by whichever side
///    branch git happened to list first.
/// 2. **Spans.**  Each chain claims the columns from its oldest block to its
///    newest, extended left to the commit it points into and right to any
///    merge that points at it — the columns its arrows need as well as its
///    blocks.
/// 3. **Colouring.**  Chains whose spans overlap must get different tracks;
///    greedy topmost-free assignment does that, and lets two branches that
///    never coexist horizontally share a row.
///
/// The invariant this buys: a track holds one chain at a time, so a horizontal
/// arrow segment drawn in a track can never pass behind another chain's block.
/// The HEAD block is placed last, against the same reservations, because it is
/// a block in the graph like any other — see [`place_head`].
fn place(
    order: &[Oid],
    cols: &HashMap<Oid, u16>,
    head_col: Option<u16>,
    block_width: u16,
    dag: &Dag,
    projection: &Projection,
) -> Placement {
    // --- 1. chains ---
    let mut chain: HashMap<Oid, usize> = HashMap::new();
    let mut members: Vec<Vec<Oid>> = Vec::new();
    // Which chain has claimed each commit, and with what id.  Every child of a
    // commit precedes it in the draw order, so by the time a commit is reached
    // every claim on it has been made and the smallest wins — and chain ids
    // are handed out newest to oldest, so the smallest id is the newest chain.
    let mut claims: HashMap<Oid, usize> = HashMap::new();

    for id in order {
        let c = match claims.get(id) {
            Some(&c) => c,
            None => {
                members.push(Vec::new());
                members.len() - 1
            }
        };
        chain.insert(id.clone(), c);
        members[c].push(id.clone());
        // Only the *first* parent continues a chain.  A merge's other parents
        // are the heads of their own chains, which is what makes a merge read
        // as two rows coming together rather than one row forking.
        if let Some(parent) = projection.parents(dag, id).first() {
            claims
                .entry(parent.clone())
                .and_modify(|held| *held = (*held).min(c))
                .or_insert(c);
        }
    }

    // --- 2. spans ---
    let col_of = |id: &Oid| cols.get(id).copied();
    let mut spans: Vec<(u16, u16)> = members
        .iter()
        .map(|m| {
            let left = m.iter().filter_map(col_of).min().unwrap_or(0);
            let right = m.iter().filter_map(col_of).max().unwrap_or(0) + block_width;
            (left, right)
        })
        .collect();

    for (c, m) in members.iter().enumerate() {
        // The exit arrow runs back through this track to the gap right of the
        // commit it points at, so those columns belong to this chain too.
        if let Some(target) = m.last().and_then(|last| projection.parents(dag, last).first()) {
            if let Some(col) = col_of(target) {
                spans[c].0 = spans[c].0.min(col + block_width);
            }
        }
    }
    for id in order {
        // A merge's second arrow crosses immediately left of the merge and
        // then runs back through the *target's* track, so those columns belong
        // to it.
        for parent in projection.parents(dag, id).iter().skip(1) {
            let (Some(&c), Some(col)) = (chain.get(parent), col_of(id)) else { continue };
            spans[c].1 = spans[c].1.max(col);
        }
    }

    // --- 3. colouring ---
    let mut used: Vec<Vec<(u16, u16)>> = Vec::new();
    let mut track_of = vec![0usize; members.len()];
    let mut by_left: Vec<usize> = (0..members.len()).collect();
    by_left.sort_by_key(|&c| (spans[c].0, c));

    for c in by_left {
        track_of[c] = claim_track(&mut used, spans[c], None);
    }

    let track: HashMap<Oid, usize> =
        chain.iter().map(|(id, &c)| (id.clone(), track_of[c])).collect();
    let head_track = place_head(&mut used, &track, cols, head_col, block_width, dag);
    let (branch_tints, tint) = assign_tints(&members, dag, projection);

    Placement {
        track_count: used.len().max(1),
        track,
        head_track,
        tint,
        branch_tints,
    }
}

/// The topmost track free over `span`, preferring `want` when it is free.
fn claim_track(used: &mut Vec<Vec<(u16, u16)>>, span: (u16, u16), want: Option<usize>) -> usize {
    let (lo, hi) = span;
    let free = |taken: &Vec<(u16, u16)>| taken.iter().all(|&(a, b)| hi <= a || lo >= b);
    let track = want
        .filter(|&w| used.get(w).map_or(true, free))
        .or_else(|| used.iter().position(free))
        .unwrap_or(used.len());
    while used.len() <= track {
        used.push(Vec::new());
    }
    used[track].push((lo, hi));
    track
}

/// Which track the HEAD block goes in.
///
/// Its own commit's track, when that is free over HEAD's columns — the block
/// sits directly to the right of the commit it names, so the arrow is short
/// and reads as "you are here".  When HEAD names a commit part-way along a
/// chain, though, that track is carrying the arrow from the commit after it,
/// and putting a block in it hides the arrow completely: blocks are painted
/// after arrows, so what you get is a line that stops dead at HEAD and a
/// commit whose child is anybody's guess.  So HEAD then takes a track of its
/// own and points across instead.
fn place_head(
    used: &mut Vec<Vec<(u16, u16)>>,
    track: &HashMap<Oid, usize>,
    cols: &HashMap<Oid, u16>,
    head_col: Option<u16>,
    block_width: u16,
    dag: &Dag,
) -> usize {
    let Some(head_col) = head_col else { return 0 };
    let target = dag.head.target.as_ref();
    let want = target.and_then(|id| track.get(id)).copied();
    // Back to the gap right of its commit: that is where HEAD's own arrow runs.
    let left = target
        .and_then(|id| cols.get(id))
        .map_or(head_col, |col| col + block_width);
    claim_track(used, (left.min(head_col), head_col + block_width), want)
}

/// Which colour group each commit and each local branch belongs to.
///
/// Walking *back* along a chain — newest to oldest, right to left — a commit
/// takes the colour of the nearest branch label at or after it.  That is
/// exactly how the graph reads: to the right of `main`'s label the commits are
/// only on `test-branch`, and at `main`'s label and before it they are on
/// `main` — even though every one of them is also on `test-branch`.
///
/// Colour is deliberately **not** the track.  A branch that is merely ahead of
/// another is not a fork, so both sit in one row, correctly — and colouring by
/// row then painted the whole history one colour and lost the very distinction
/// the colours exist to draw.
fn assign_tints(
    members: &[Vec<Oid>],
    dag: &Dag,
    projection: &Projection,
) -> (HashMap<String, usize>, HashMap<Oid, usize>) {
    // Branches numbered by where their tip is drawn, so the numbering is
    // stable and neighbouring branches get neighbouring colours.
    let mut branches: Vec<(usize, String, Oid)> = Vec::new();
    for (c, m) in members.iter().enumerate() {
        for (i, id) in m.iter().enumerate() {
            for r in dag.local_branches() {
                if projection.ref_target(dag, &r.name) == Some(id) {
                    branches.push((c * 10_000 + i, r.name.clone(), id.clone()));
                }
            }
        }
    }
    branches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let branch_tints: HashMap<String, usize> = branches
        .iter()
        .enumerate()
        .map(|(t, (_, name, _))| (name.clone(), t))
        .collect();

    let mut tint = HashMap::new();
    for (c, m) in members.iter().enumerate() {
        // A chain with no branch on it at all still needs a colour of its own;
        // numbering it past the branches keeps it from borrowing one.
        let mut current = branch_tints.len() + c;
        for id in m {
            if let Some(t) = branches
                .iter()
                .filter(|(_, _, target)| target == id)
                .filter_map(|(_, name, _)| branch_tints.get(name))
                .min()
            {
                current = *t;
            }
            tint.insert(id.clone(), current);
        }
    }
    (branch_tints, tint)
}

/// How wide one block is.
///
/// A block is as wide as what is written in it, never as wide as the space
/// that happens to be free: dividing the viewport up made every block on a
/// wide terminal a banner around a 30-column commit message, which reads as a
/// layout bug rather than as a graph.  Clamped at both ends — narrow enough
/// that several commits fit on screen at once, wide enough to say something.
fn block_width(width: u16, natural: u16) -> u16 {
    let cap = width.clamp(8, MAX_BLOCK);
    natural.clamp(MIN_BLOCK.min(cap), cap)
}

/// The widest a block needs to be to hold its own contents, borders included.
///
/// Computed before anything is drawn, from the untruncated text: the drawing
/// then truncates to whatever this settled on ([`labels_for`] does it for the
/// ref labels), so a block is never wider than its longest line and never
/// narrower than the clamps allow.
fn natural_width(dag: &Dag, projection: &Projection, order: &[Oid]) -> u16 {
    let mut inner = 0u16;
    for id in order {
        // The top border: hash, then every ref label that sits here.
        let mut border = HASH_COLS;
        for r in &dag.refs {
            if projection.ref_target(dag, &r.name) == Some(id) {
                border += r.name.chars().count() as u16 + 3;
            }
        }
        inner = inner.max(border);

        let summary = projection
            .pending()
            .iter()
            .find(|p| p.id == *id)
            .map(|p| p.summary.chars().count())
            .or_else(|| dag.get(id).map(|c| c.summary.chars().count()))
            .unwrap_or(0) as u16;
        // The summary wraps over two rows, so half of it is what has to fit,
        // plus two columns of lead-in and one of trailing room.
        inner = inner.max(summary.div_ceil(2) + 3);

        if let Some(commit) = dag.get(id) {
            // `author · age` on the metadata row…
            inner = inner.max(commit.author.chars().count() as u16 + 3 + AGE_COLS + 1);
            // …and the change counts along the bottom border.
            inner = inner.max(COUNTS_COLS + 4);
        }
    }
    // The HEAD block's widest line: `● ` plus the branch name, or one of the
    // work-tree lines it carries underneath.  Sized from the same strings the
    // renderer draws (`WorkTree::summary_lines`), so a tree with something in
    // it does not end up with its state clipped mid-word.
    if let Some(branch) = dag.head.branch.as_ref() {
        inner = inner.max(branch.chars().count() as u16 + 4);
    }
    for line in dag.work.summary_lines() {
        inner = inner.max(line.chars().count() as u16 + 3);
    }
    inner + 2
}

/// The ref labels on `id`'s block, with the column each starts at.
///
/// Truncated to what the border can hold: a block with six tags on it must
/// not draw past its own edge and into the block beside it.
fn labels_for(dag: &Dag, projection: &Projection, id: &Oid, inner: u16) -> Vec<(String, u16)> {
    // Projected positions, not the snapshot's: a moved branch has to be drawn
    // where the plan puts it or the preview shows nothing.
    let mut labels = Vec::new();
    // After the hash, which the renderer writes at the start of the same
    // border line.  Both come from here so they cannot overlap — which they
    // did, and the result was a block labelled `╭ main aa ───`.
    let mut col = HASH_COLS;
    for r in &dag.refs {
        if projection.ref_target(dag, &r.name) != Some(id) {
            continue;
        }
        let width = r.name.chars().count() as u16 + 2;
        if col + width > inner {
            break;
        }
        labels.push((r.name.clone(), col));
        col += width + 1;
    }
    labels
}

/// Connect each block to its projected parents.
fn build_edges(
    blocks: &[Block],
    index: &HashMap<Oid, usize>,
    dag: &Dag,
    projection: &Projection,
    block_width: u16,
) -> Vec<Edge> {
    let mut edges = Vec::new();
    for block in blocks {
        // The HEAD block points at the commit it names, which is the whole
        // reason it is a block rather than a label.
        if block.kind == BlockKind::Head {
            let Some(target) = dag.head.target.as_ref().and_then(|id| index.get(id)) else {
                continue;
            };
            let target = &blocks[*target];
            let end_col = target.col + block_width;
            edges.push(Edge {
                child: block.id.clone(),
                slot: 0,
                parent: Some(target.id.clone()),
                from_track: block.track,
                to_track: target.track,
                col: block.col.saturating_sub(1),
                end_col,
                cross_col: end_col,
            });
            continue;
        }
        for (slot, parent) in projection.parents(dag, &block.id).iter().enumerate() {
            let target = index.get(parent).map(|&i| &blocks[i]);
            let col = block.col.saturating_sub(1);
            let end_col = target.map_or(0, |b| b.col + block_width);
            edges.push(Edge {
                child: block.id.clone(),
                slot,
                // A parent outside the drawn set is left as `None` so the
                // renderer draws a stub rather than an arrow to nowhere.
                parent: target.map(|b| b.id.clone()),
                from_track: block.track,
                to_track: target.map_or(block.track, |b| b.track),
                col,
                end_col,
                // See `Edge::cross_col`: the first parent crosses late, in the
                // gap right of the commit it points at, so the run stays in
                // the child's own track; a merge's other parents cross early,
                // because the child's track carries on past them.
                cross_col: if slot == 0 { end_col.min(col) } else { col },
            });
        }
    }
    edges
}

/// Everything the cursor can land on, in drawing order.
fn build_focusables(layout: &Layout, head: &Head) -> Vec<Focusable> {
    let mut out = Vec::new();
    for block in &layout.blocks {
        let top = layout.track_row(block.track);
        match block.kind {
            BlockKind::Head => out.push(Focusable {
                focus: Focus::Head,
                row: top + 1,
                col: block.col,
            }),
            _ => {
                // Labels sit on the block's top border, one row above the
                // block's own focus point, so `j` off a branch label lands on
                // the commit it labels rather than skipping past it.
                for (name, col) in &block.labels {
                    out.push(Focusable {
                        focus: Focus::Ref(name.clone()),
                        row: top,
                        col: block.col + col,
                    });
                }
                out.push(Focusable {
                    focus: Focus::Commit(block.id.clone()),
                    row: top + 1,
                    col: block.col,
                });
            }
        }
    }
    for edge in &layout.edges {
        // HEAD's arrow is not a parent link and cannot be moved, so it is not
        // a place the cursor can land.
        if edge.child.as_str() == "HEAD" {
            continue;
        }
        out.push(Focusable {
            focus: Focus::Edge { child: edge.child.clone(), slot: edge.slot },
            row: layout.arrow_row(edge.from_track),
            col: edge.col,
        });
    }
    let _ = head;
    out.sort_by_key(|f| (f.col, f.row));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{
        plan::{Edit, Plan},
        Commit, Ref, RefKind, WorkTree,
    };

    fn commit(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: Oid::new(id),
            parents: parents.iter().map(|p| Oid::new(*p)).collect(),
            summary: format!("summary {id}"),
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
    ///   feature:  a ─┬─ c ── d
    ///   main:        └─ e ── f
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

    /// A trunk with three topic branches cut from three different points —
    /// the shape where tracks used to be recycled under an arrow still in
    /// flight, and where the trunk lost its row at the end.
    ///
    /// ```text
    ///   main:    one ── two ── three ── four
    ///   topic-a:          └─ a1 ── a2
    ///   topic-b:                └─ b1
    ///   topic-c:    └─ c1 ── c2
    /// ```
    fn tangled() -> Dag {
        Dag::new(
            vec![
                commit("four", &["three"]),
                commit("a2", &["a1"]),
                commit("a1", &["two"]),
                commit("b1", &["three"]),
                commit("three", &["two"]),
                commit("c2", &["c1"]),
                commit("c1", &["one"]),
                commit("two", &["one"]),
                commit("one", &[]),
            ],
            vec![
                branch("main", "four"),
                branch("topic-a", "a2"),
                branch("topic-b", "b1"),
                branch("topic-c", "c2"),
            ],
            Head { branch: Some("main".into()), target: Some(Oid::new("four")) },
            WorkTree::default(),
            false,
        )
    }

    /// One branch simply ahead of another — not a fork, so both belong in one
    /// row, and HEAD names a commit part-way along it.  The shape a
    /// `git checkout -b` and one commit produces, and the one that had both an
    /// arrow drawn behind the HEAD block and a whole history in one colour.
    ///
    /// ```text
    ///   base ── mid ── top
    ///           ^ HEAD, main      ^ test-branch
    /// ```
    fn ahead() -> Dag {
        Dag::new(
            vec![commit("top", &["mid"]), commit("mid", &["base"]), commit("base", &[])],
            vec![branch("main", "mid"), branch("test-branch", "top")],
            Head { branch: Some("main".into()), target: Some(Oid::new("mid")) },
            WorkTree::default(),
            false,
        )
    }

    /// A real merge: two parents, the second of which starts its own chain.
    fn merged() -> Dag {
        Dag::new(
            vec![
                commit("m", &["three", "side"]),
                commit("side", &["two"]),
                commit("three", &["two"]),
                commit("two", &[]),
            ],
            vec![branch("main", "m"), branch("feature", "side")],
            Head { branch: Some("main".into()), target: Some(Oid::new("m")) },
            WorkTree::default(),
            false,
        )
    }

    fn laid_out(width: u16) -> (Dag, Projection, Layout) {
        let dag = dag();
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, width);
        (dag, projection, layout)
    }

    #[test]
    fn a_chain_keeps_one_track_and_a_branch_opens_another() {
        let (_, _, layout) = laid_out(120);
        let track = |id: &str| layout.block(&Oid::new(id)).unwrap().track;
        // `a → c → d` is one chain and stays in a row…
        assert_eq!(track("d"), track("c"));
        assert_eq!(track("c"), track("a"));
        // …while `e → f` is a second, in its own.
        assert_eq!(track("f"), track("e"));
        assert_ne!(track("d"), track("f"));
        assert_eq!(layout.track_count, 2);
    }

    /// Blocks never overlap: one commit per column band, oldest at the left.
    /// A packed layout can draw a commit to the left of its own descendant,
    /// which in a view whose premise is "the picture is the truth" is not
    /// cosmetic.
    #[test]
    fn every_block_starts_after_the_one_before_it_and_none_overlap() {
        let (_, _, layout) = laid_out(120);
        let mut last_right = 0;
        for block in &layout.blocks {
            assert!(
                block.col >= last_right,
                "block {} starts at {} but the previous ended at {last_right}",
                block.id,
                block.col
            );
            last_right = block.col + layout.block_width;
        }
        assert!(layout.total_cols >= last_right - GAP);
    }

    /// The oldest commit is at the left edge and time runs to the right, so
    /// every commit is drawn after the parent it follows.
    #[test]
    fn a_commit_is_drawn_to_the_right_of_its_parent() {
        let (dag, projection, layout) = laid_out(120);
        for c in dag.commits() {
            for parent in projection.parents(&dag, &c.id) {
                let (Some(child), Some(parent)) =
                    (layout.block(&c.id), layout.block(parent))
                else {
                    continue;
                };
                assert!(
                    parent.col + layout.block_width <= child.col,
                    "{} is not drawn after its parent",
                    child.id
                );
            }
        }
    }

    /// HEAD is a block of its own beside the graph, pointing at the commit it
    /// names — the one pointer that should be findable without reading.
    /// It sits *immediately* to the right of its own commit, where the next
    /// commit would go, not at the end of the graph: from there its arrow
    /// spans however far back HEAD happens to be, and a screenful of `─`
    /// between a block and its target says nothing.
    #[test]
    fn head_sits_directly_after_the_commit_it_names() {
        let (_, _, layout) = laid_out(120);
        let head = layout.block(&Oid::new("HEAD")).expect("HEAD is drawn");
        let target = layout.block(&Oid::new("f")).expect("its commit is drawn");
        assert_eq!(head.kind, BlockKind::Head);
        assert_eq!(head.track, target.track, "and in the same track");
        assert_eq!(target.col + layout.col_stride(), head.col, "one gap apart");
        assert!(layout.locate(&Focus::Head).is_some());
        // `f` is not the newest commit in this fixture, so this really is a
        // placement decision and not the end of the list by accident.
        assert!(head.col + layout.block_width < layout.total_cols);
    }

    /// A repository with no commits still has a HEAD worth showing: it names
    /// the branch the first commit will land on.
    #[test]
    fn an_unborn_head_is_still_drawn() {
        let dag = Dag::new(
            Vec::new(),
            Vec::new(),
            Head { branch: Some("main".into()), target: None },
            WorkTree::default(),
            false,
        );
        let layout = compute(&dag, &Plan::default().project(&dag), 100);
        assert_eq!(layout.blocks.len(), 1);
        assert_eq!(layout.blocks[0].kind, BlockKind::Head);
    }

    /// Tracks that fit must not be scrolled off: N tracks occupy
    /// `N * (h + gap) - gap`, and dividing the bare height reports one too few
    /// whenever they fit exactly.
    #[test]
    fn tracks_that_exactly_fit_are_all_counted_as_visible() {
        let (_, _, layout) = laid_out(120);
        assert_eq!(layout.track_count, 2);
        let span = 2 * (BLOCK_H + TRACK_GAP) - TRACK_GAP;
        assert_eq!(layout.visible_tracks(span), 2);
        // And a viewport one row too short honestly reports one.
        assert_eq!(layout.visible_tracks(span - 1), 1);
    }

    #[test]
    fn every_parent_link_becomes_an_arrow() {
        let (_, _, layout) = laid_out(120);
        let edge = |child: &str| {
            layout
                .edges
                .iter()
                .find(|e| e.child == Oid::new(child))
                .expect("an edge")
                .clone()
        };
        assert_eq!(edge("d").parent, Some(Oid::new("c")));
        assert_eq!(edge("c").parent, Some(Oid::new("a")));
        // Both chains converge on `a`, so two arrows end at the same block.
        assert_eq!(
            layout.edges.iter().filter(|e| e.parent == Some(Oid::new("a"))).count(),
            2
        );
        // A root commit has no parent link at all.
        assert!(!layout.edges.iter().any(|e| e.child == Oid::new("a")));
    }

    /// A commit whose parent was never loaded gets a stub, not an arrow to
    /// nowhere — the honest picture of a truncated walk.  The graph leaves
    /// room at the left edge for it.
    #[test]
    fn a_parent_past_the_horizon_leaves_a_stub() {
        let dag = Dag::new(
            vec![commit("x", &["beyond"])],
            vec![branch("main", "x")],
            Head::default(),
            WorkTree::default(),
            true,
        );
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 100);
        let edge = layout
            .edges
            .iter()
            .find(|e| e.child == Oid::new("x"))
            .expect("an edge");
        assert_eq!(edge.parent, None);
        assert!(layout.route(edge).stub);
        assert!(
            layout.block(&Oid::new("x")).unwrap().col >= GAP,
            "the stub needs room to the left of the oldest block"
        );
    }

    /// `h` from a block must land on the arrow immediately to its left, not on
    /// whatever else happens to be one column back in another track.
    #[test]
    fn moving_back_in_time_prefers_the_arrow_beside_it() {
        let (_, _, layout) = laid_out(120);
        let from = Focus::Commit(Oid::new("d"));
        assert_eq!(
            layout.step_where(&from, Dir::Left, |_| true),
            Some(Focus::Edge { child: Oid::new("d"), slot: 0 })
        );
    }

    #[test]
    fn moving_is_reversible_and_stops_at_the_edges() {
        let (_, _, layout) = laid_out(120);
        let start = Focus::Commit(Oid::new("d"));
        let back = layout.step_where(&start, Dir::Left, |_| true).unwrap();
        assert_eq!(layout.step_where(&back, Dir::Right, |_| true), Some(start));

        // The graph has ends: nothing before the first focusable, nothing
        // after the last.
        let first = layout.focusables.first().unwrap().focus.clone();
        let last = layout.focusables.last().unwrap().focus.clone();
        assert_eq!(layout.step_where(&first, Dir::Left, |_| true), None);
        assert_eq!(layout.step_where(&last, Dir::Right, |_| true), None);
    }

    /// `j`/`k` cross tracks at a comparable point in history rather than
    /// jumping to the far end of another branch.  A track is wide and short,
    /// so the nearest thing *by row* is very often a label forty columns away
    /// — which is not what the key means.
    #[test]
    fn moving_across_tracks_stays_at_the_same_point_in_history() {
        let (_, _, layout) = laid_out(120);
        let from = Focus::Commit(Oid::new("d"));
        let col_of = |f: &Focus| layout.locate(f).unwrap().col as i32;
        let target = layout
            .step_where(&from, Dir::Down, |_| true)
            .expect("a track below");
        assert!(
            (col_of(&target) - col_of(&from)).abs() <= layout.col_stride() as i32,
            "crossing tracks travelled {} columns",
            (col_of(&target) - col_of(&from)).abs()
        );
        // And it really did change track.
        let row_of = |f: &Focus| layout.locate(f).unwrap().row;
        assert!(row_of(&target) > row_of(&from));
    }

    /// A branch label sits on its block's top border, so `j` off the label
    /// lands on the commit it labels.
    #[test]
    fn a_branch_label_is_selectable_and_sits_above_its_commit() {
        let (_, _, layout) = laid_out(120);
        let label = layout.locate(&Focus::Ref("main".into())).expect("main is drawn");
        let block = layout.block(&Oid::new("f")).unwrap();
        assert_eq!(label.row, layout.track_row(block.track));
        assert_eq!(
            layout.step_where(&Focus::Ref("main".into()), Dir::Down, |_| true),
            Some(Focus::Commit(Oid::new("f")))
        );
    }

    /// The preview is the point: a moved branch is drawn where the plan puts
    /// it, not where git still has it.
    #[test]
    fn a_moved_branch_label_is_drawn_at_its_new_home() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::MoveRef { name: "main".into(), new_target: Oid::new("a") })
            .unwrap();
        let layout = compute(&dag, &plan.project(&dag), 120);

        let block_of = |id: &str| layout.block(&Oid::new(id)).unwrap();
        assert!(block_of("a").labels.iter().any(|(n, _)| n == "main"));
        assert!(!block_of("f").labels.iter().any(|(n, _)| n == "main"));
    }

    /// A dropped commit leaves the picture entirely — that is the preview.
    #[test]
    fn a_dropped_commit_is_not_drawn() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Drop { commit: Oid::new("c") }).unwrap();
        let layout = compute(&dag, &plan.project(&dag), 120);
        assert!(layout.block(&Oid::new("c")).is_none());
        // And `d` now points straight at `a`.
        let edge = layout.edges.iter().find(|e| e.child == Oid::new("d")).unwrap();
        assert_eq!(edge.parent, Some(Oid::new("a")));
    }

    /// A merge the plan would create is drawn as a block like any other, so
    /// the user can see what they are agreeing to before they agree to it.
    #[test]
    fn a_pending_merge_is_drawn_as_a_block_with_two_arrows() {
        let dag = dag();
        let mut plan = Plan::default();
        plan.push(&dag, Edit::Merge { into: "main".into(), from: Oid::new("d") })
            .unwrap();
        let projection = plan.project(&dag);
        let layout = compute(&dag, &projection, 120);

        let pending = layout
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Pending)
            .expect("the merge is drawn");
        assert!(pending.labels.iter().any(|(n, _)| n == "main"));
        let arrows: Vec<_> = layout
            .edges
            .iter()
            .filter(|e| e.child == pending.id)
            .filter_map(|e| e.parent.clone())
            .collect();
        assert_eq!(arrows, vec![Oid::new("f"), Oid::new("d")]);
    }

    /// One commit must not stretch across a 200-column terminal, and a long
    /// message must not leave two commits on screen.
    #[test]
    fn block_width_is_clamped_at_both_ends() {
        assert_eq!(block_width(300, 500), MAX_BLOCK);
        assert_eq!(block_width(300, 4), MIN_BLOCK);
        // A terminal narrower than a block gets what there is.
        assert!(block_width(12, 500) <= 12);
    }

    /// A block is as wide as what is written in it.  Dividing the viewport up
    /// instead drew a banner around a 30-column commit message on any
    /// reasonably wide terminal.
    #[test]
    fn a_block_is_no_wider_than_its_contents() {
        let (dag, projection, layout) = laid_out(300);
        let natural = natural_width(&dag, &projection, &draw_order(&dag, &projection));
        assert!(natural < MAX_BLOCK, "the fixture is short: {natural}");
        assert_eq!(layout.block_width, natural.max(MIN_BLOCK));
    }

    /// …and long contents still get the room, up to the clamp.
    #[test]
    fn a_long_summary_widens_the_block() {
        let long = "a commit message long enough that it needs the whole block to itself";
        let mut wide = commit("d", &["c"]);
        wide.summary = long.into();
        let dag = Dag::new(
            vec![wide, commit("c", &[])],
            vec![branch("feature", "d")],
            Head::default(),
            WorkTree::default(),
            false,
        );
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 300);
        assert_eq!(layout.block_width, MAX_BLOCK, "a long summary fills the clamp");
    }

    /// A block full of tags must not draw past its own edge into the block
    /// beside it.
    #[test]
    fn labels_stop_at_the_block_edge() {
        let mut dag = dag();
        dag.refs = (0..12)
            .map(|i| branch(&format!("branch-number-{i}"), "f"))
            .collect();
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 80);
        let block = layout.block(&Oid::new("f")).unwrap();
        let inner = layout.block_inner();
        for (name, col) in &block.labels {
            assert!(
                col + name.chars().count() as u16 + 2 <= inner,
                "label {name} at {col} overruns {inner}"
            );
        }
        assert!(block.labels.len() < 12, "and it stopped early");
    }

    /// An empty repository must lay out without panicking — there is no HEAD
    /// commit, no track and no block.
    #[test]
    fn an_empty_repository_lays_out_to_nothing() {
        let dag = Dag::new(Vec::new(), Vec::new(), Head::default(), WorkTree::default(), false);
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 80);
        assert!(layout.blocks.is_empty());
        assert!(layout.focusables.is_empty());
        assert_eq!(layout.initial_focus(), None);
    }

    /// The invariant the whole track assignment exists to provide: **no arrow
    /// is ever drawn through a block**.  Blocks are painted after arrows, so a
    /// line that crosses one does not merely look wrong — it vanishes, and the
    /// reader is left with an arrow that stops dead and a commit whose parent
    /// is anybody's guess.
    ///
    /// Checked over the awkward shapes rather than the tidy one: several
    /// topic branches cut from different points of a trunk is where tracks
    /// used to get recycled underneath an arrow still using them.
    #[test]
    fn no_arrow_is_drawn_through_a_block() {
        for (name, dag) in [
            ("two branches", dag()),
            ("many branches", tangled()),
            ("a merge", merged()),
            // HEAD part-way along a chain: the block lands in the middle of a
            // track an arrow is already using.
            ("a branch ahead", ahead()),
        ] {
            let projection = Plan::default().project(&dag);
            for width in [40, 80, 120, 400] {
                let layout = compute(&dag, &projection, width);
                for edge in &layout.edges {
                    for (row, col) in layout.route(edge).cells() {
                        for block in &layout.blocks {
                            // The endpoints are meant to touch: an arrow
                            // leaves one border and its head sits in the gap
                            // beside the next.  Everything else is a crossing.
                            let top = layout.track_row(block.track);
                            let inside = col > block.col
                                && col < block.col + layout.block_width
                                && row >= top
                                && row < top + BLOCK_H;
                            assert!(
                                !inside,
                                "{name} at width {width}: the arrow {} -> {:?} runs through \
                                 block {} at ({row}, {col})",
                                edge.child, edge.parent, block.id
                            );
                        }
                    }
                }
            }
        }
    }

    /// A branch is a row.  The trunk used to hand its oldest commits to
    /// whichever topic branch git happened to list first, so `main` stepped
    /// sideways near the start for no reason a reader could see.
    #[test]
    fn a_first_parent_chain_keeps_one_track_all_the_way_along() {
        let dag = tangled();
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 200);
        let track = |id: &str| layout.block(&Oid::new(id)).expect(id).track;

        // The trunk, end to end.
        for id in ["four", "three", "two", "one"] {
            assert_eq!(track(id), track("four"), "{id} left the trunk's track");
        }
        // …and each topic branch is somewhere else, in one track of its own.
        for (tip, base) in [("a2", "a1"), ("c2", "c1")] {
            assert_eq!(track(tip), track(base), "{tip} and {base} are one chain");
            assert_ne!(track(tip), track("four"), "{tip} is not the trunk");
        }
    }

    /// Two branches that never coexist horizontally may share a row: the
    /// point of colouring spans rather than handing every chain its own track
    /// is that a graph with a long history does not grow a track per branch
    /// that ever existed.
    #[test]
    fn chains_that_do_not_overlap_share_a_track() {
        let dag = Dag::new(
            vec![
                commit("tip", &["mid"]),
                commit("early-topic", &["mid"]),
                commit("mid", &["base"]),
                commit("late-topic", &["base"]),
                commit("base", &[]),
            ],
            vec![branch("main", "tip")],
            Head::default(),
            WorkTree::default(),
            false,
        );
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 200);
        // Both topics hang off the trunk at different points and neither
        // outlives the other, so two tracks are enough for four chains.
        assert!(layout.track_count <= 2, "{} tracks for two side commits", layout.track_count);
    }

    /// A branch that is merely *ahead* of another is not a fork, so both sit
    /// in one row — and colouring by row then painted the whole history one
    /// colour and lost the distinction entirely.  Colour is the branch a
    /// commit is on: walking back, each takes the nearest label at or after it.
    #[test]
    fn commits_take_the_colour_of_the_branch_they_are_on() {
        let dag = ahead();
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 200);
        let tint = |id: &str| layout.block(&Oid::new(id)).expect(id).tint;

        // One row, because that is the truth of this repository…
        assert_eq!(
            layout.block(&Oid::new("top")).unwrap().track,
            layout.block(&Oid::new("base")).unwrap().track
        );
        // …and two colours, because there are two branches.
        assert_ne!(tint("top"), tint("mid"), "the branches are one colour");
        assert_eq!(tint("mid"), tint("base"), "everything at and before `main`");

        // The labels match the commits they name, so a label and its run of
        // history read as one thing.
        assert_eq!(layout.branch_tints.get("test-branch"), Some(&tint("top")));
        assert_eq!(layout.branch_tints.get("main"), Some(&tint("mid")));
    }

    /// HEAD sits directly after the commit it names, in that commit's track —
    /// unless that track is carrying an arrow past it, in which case a block
    /// there would hide the arrow completely.
    #[test]
    fn head_steps_aside_rather_than_landing_on_an_arrow() {
        // At a branch tip there is nothing passing, so it stays put.
        let dag = dag();
        let layout = compute(&dag, &Plan::default().project(&dag), 200);
        let head = layout.block(&Oid::new("HEAD")).expect("HEAD is drawn");
        assert_eq!(head.track, layout.block(&Oid::new("f")).unwrap().track);

        // Part-way along a chain, the arrow from the commit after it owns that
        // track, so HEAD takes one of its own and points across.
        let dag = ahead();
        let layout = compute(&dag, &Plan::default().project(&dag), 200);
        let head = layout.block(&Oid::new("HEAD")).expect("HEAD is drawn");
        let target = layout.block(&Oid::new("mid")).unwrap();
        assert_ne!(head.track, target.track, "HEAD is sitting on the arrow");
        assert!(head.col > target.col, "and still directly after its commit");
    }
}
