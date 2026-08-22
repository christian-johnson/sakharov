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
//! One commit per row band, newest at the top, time flowing down.  Lanes give
//! the horizontal position: a commit's lane is inherited by its first parent,
//! so a chain stays in a column and a branch point opens a new one.
//!
//! Commits are *not* packed several to a row even when they would fit.  A
//! generation-packed layout looks tidier, but it can place a commit visually
//! above one of its own ancestors, and in a view whose entire purpose is that
//! the picture is the truth, that is not a cosmetic problem.

use std::collections::HashMap;

use super::{
    plan::Projection,
    Dag, Head, Oid,
};

/// Rows in a commit block: border, summary, metadata, border.
pub const BLOCK_H: u16 = 4;
/// Rows in the HEAD block: border, content, border.
pub const HEAD_H: u16 = 3;
/// Rows between one block and the next, where the arrows are drawn.
pub const GAP: u16 = 2;
/// Narrowest a lane may be before lanes start scrolling off instead.
pub const MIN_LANE: u16 = 24;
/// Columns budgeted for the relative age on a commit's metadata row.
///
/// An estimate rather than the rendered string: the width has to be settled
/// before anything is drawn, and "11 months ago" is the longest this gets.
const AGE_COLS: u16 = 14;
/// Columns budgeted for the `+123 -45` change counts, right-aligned on the
/// same row as the metadata.
const COUNTS_COLS: u16 = 12;
/// Widest a lane grows, so a single-branch repository does not draw one block
/// stretched across a 200-column terminal.
pub const MAX_LANE: u16 = 72;
/// Blank columns between one lane and the next.
const LANE_GAP: u16 = 2;
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
    pub lane: usize,
    /// Top row, measured in the whole stack rather than the viewport.
    pub row: u16,
    pub height: u16,
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
    /// as a stub trailing off the bottom, which is the honest picture.
    pub parent: Option<Oid>,
    pub from_lane: usize,
    pub to_lane: usize,
    /// Row the arrow starts on (immediately below the child block).
    pub row: u16,
    /// Row the arrow ends on (the parent block's top), or the stack bottom.
    pub end_row: u16,
    /// The row the arrow changes lane on.
    ///
    /// A first-parent link crosses **late**, in the gap immediately above the
    /// commit it points at, so the long part of the drop stays in the child's
    /// own lane — which that chain owns outright.  A merge's second parent
    /// crosses **early**, immediately below the child, because the child's
    /// lane continues on down to its own first parent and the drop would run
    /// straight through it.  Either way the vertical never enters a lane
    /// somebody else's blocks are sitting in.
    pub cross_row: u16,
}

/// Where one arrow's cells actually go.
///
/// The route lives here rather than in the renderer because it is geometry,
/// and this module is the one place geometry is decided — the same reason
/// block positions and lane widths are here.  It is also what makes
/// [`no_arrow_is_drawn_through_a_block`] able to check the invariant the whole
/// lane assignment exists to provide.
pub struct EdgeRoute {
    /// Column the arrow leaves in, and the column it arrives in.
    pub from_col: u16,
    pub to_col: u16,
    /// First row of the arrow, the row it changes lane on, and the row the
    /// arrowhead sits on.
    pub start: u16,
    pub cross: u16,
    pub head_row: u16,
    /// A parent past the loaded horizon: a stub that visibly goes nowhere.
    pub stub: bool,
}

impl EdgeRoute {
    /// Every cell the arrow paints.
    pub fn cells(&self) -> Vec<(u16, u16)> {
        if self.stub {
            return (self.start..(self.start + 2).min(self.head_row.max(self.start)))
                .map(|row| (row, self.from_col))
                .collect();
        }
        let (lo, hi) = (self.from_col.min(self.to_col), self.from_col.max(self.to_col));
        (self.start..self.cross)
            .map(|row| (row, self.from_col))
            .chain((lo..=hi).map(|col| (self.cross, col)))
            .chain((self.cross + 1..self.head_row).map(|row| (row, self.to_col)))
            .chain(std::iter::once((self.head_row, self.to_col)))
            .collect()
    }
}

/// Something the cursor can sit on, and where it is.
///
/// Positions are `(row, col)` in the full stack rather than lane indices, so
/// several labels on one block's border are distinguishable and `h`/`l` walks
/// them in the order they are drawn.
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
    pub lane_count: usize,
    pub lane_width: u16,
    /// The colour group each local branch label belongs to, so a label is
    /// drawn the same colour as the commits that are on it.
    pub branch_tints: HashMap<String, usize>,
    /// Total height of the stack, for the scroll anchor to clamp against.
    pub total_rows: u16,
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
    /// The arrowhead sits one row *above* the parent's top border: blocks are
    /// drawn after arrows so a line entering a box reads as passing behind it,
    /// which means anything drawn on the border itself is overwritten.
    pub fn route(&self, edge: &Edge) -> EdgeRoute {
        let head_row = edge.end_row.saturating_sub(1).max(edge.row);
        EdgeRoute {
            from_col: self.lane_col(edge.from_lane) + 2,
            to_col: self.lane_col(edge.to_lane) + 2,
            start: edge.row,
            cross: edge.cross_row.clamp(edge.row, head_row),
            head_row,
            stub: edge.parent.is_none(),
        }
    }

    /// The screen column a lane starts at.
    pub fn lane_col(&self, lane: usize) -> u16 {
        lane as u16 * (self.lane_width + LANE_GAP)
    }

    /// How many whole lanes fit in `width`.
    ///
    /// The `+ LANE_GAP` is not a fudge: lanes are laid out with a gap
    /// *between* them, so N lanes occupy `N * (w + gap) - gap`.  Dividing the
    /// bare width instead reports one lane too few whenever they fit exactly,
    /// which scrolled a two-branch graph sideways until half of it was off
    /// screen on a terminal wide enough for all of it.
    pub fn visible_lanes(&self, width: u16) -> usize {
        ((width + LANE_GAP) / (self.lane_width + LANE_GAP)).max(1) as usize
    }

    /// Width available inside a block's borders.
    pub fn block_inner(&self) -> u16 {
        self.lane_width.saturating_sub(2)
    }

    /// The nearest focusable in `dir` from `current`.
    ///
    /// Vertical motion sorts by distance travelled first, so `j` from a block
    /// lands on the arrow directly beneath it rather than on whatever happens
    /// to be lowest.
    ///
    /// Horizontal motion sorts the *other* way round — nearest row first, then
    /// nearest column.  A lane is tall and narrow, so the thing eight columns
    /// to the right is very often ten rows up (a branch label on some other
    /// block's border), and travelling to it is not what `l` means.  What `l`
    /// means is "the next lane, beside where I am".
    /// Restricted to the focusables `allow` accepts.
    ///
    /// Used while something is being dragged: the walk is over *destinations*
    /// then, and stepping onto something you cannot drop on is a press that
    /// does nothing except take the cursor further from somewhere useful.
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
                        Dir::Up | Dir::Down => (along, dc.abs()),
                        Dir::Left | Dir::Right => (dr.abs(), along),
                    };
                    (key, f.focus.clone())
                })
            })
            .min_by_key(|(key, _)| *key)
            .map(|(_, focus)| focus)
    }

    /// The first thing worth putting the cursor on: HEAD if it is drawn, else
    /// the topmost focusable.
    pub fn initial_focus(&self) -> Option<Focus> {
        self.focusables
            .iter()
            .find(|f| f.focus == Focus::Head)
            .or_else(|| self.focusables.first())
            .map(|f| f.focus.clone())
    }
}

/// Lay out `dag` as `projection` leaves it, for a content area `width` wide.
pub fn compute(dag: &Dag, projection: &Projection, width: u16) -> Layout {
    let order = draw_order(dag, projection);
    // Rows before lanes: where a block sits vertically depends only on the
    // draw order, and the lane assignment needs those rows to know which
    // chains overlap and therefore cannot share a column.
    let (rows, head_row, total_rows) = assign_rows(dag, &order);
    let placed = place(&order, &rows, head_row, dag, projection);
    let lane_width = lane_width(width, placed.lane_count, natural_width(dag, projection, &order));
    let inner = lane_width.saturating_sub(2);

    let mut blocks = Vec::new();

    // HEAD is drawn as a block rather than as one more label because it is a
    // different kind of thing from a branch — it is where *you* are — and it
    // is the one pointer always worth finding at a glance.  It sits directly
    // above the commit it names; see `place_head` for the one case where it
    // cannot also sit in that commit's lane.
    if let Some(row) = head_row {
        blocks.push(Block {
            id: Oid::new("HEAD"),
            kind: BlockKind::Head,
            lane: placed.head_lane,
            row,
            height: HEAD_H,
            labels: Vec::new(),
            tint: 0,
        });
    }

    for id in &order {
        let kind = if id.is_pending() { BlockKind::Pending } else { BlockKind::Commit };
        blocks.push(Block {
            id: id.clone(),
            kind,
            lane: placed.lane.get(id).copied().unwrap_or(0),
            row: rows[id],
            height: BLOCK_H,
            labels: labels_for(dag, projection, id, inner),
            tint: placed.tint.get(id).copied().unwrap_or(0),
        });
    }
    blocks.sort_by_key(|b| b.row);

    let index: HashMap<Oid, usize> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.clone(), i))
        .collect();

    let edges = build_edges(&blocks, &index, dag, projection, total_rows);
    let mut layout = Layout {
        focusables: Vec::new(),
        blocks,
        edges,
        lane_count: placed.lane_count,
        lane_width,
        branch_tints: placed.branch_tints,
        total_rows,
        index,
    };
    layout.focusables = build_focusables(&layout, &dag.head);
    layout
}

/// Commits to draw, newest first: the pending ones the plan would create,
/// then the snapshot's own topological order minus anything dropped.
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

/// Where every block sits vertically: one per row band, in draw order.
///
/// Returns the commits' rows, HEAD's row if it is drawn, and the height of the
/// whole stack.  Every block occupies the same row bands whatever lane it ends
/// up in, which is what makes the gap rows between them gaps in *every* lane —
/// and that is what lets an arrow cross lanes without ever running through a
/// block.
fn assign_rows(dag: &Dag, order: &[Oid]) -> (HashMap<Oid, u16>, Option<u16>, u16) {
    let mut rows = HashMap::new();
    let mut row = 0u16;
    let mut head_row = None;

    let head_before = dag.head.target.clone().filter(|id| order.contains(id));
    // A repository with no commits yet still has a HEAD worth showing: it says
    // which branch the first commit will be on.
    if head_before.is_none() && (dag.head.target.is_some() || dag.head.branch.is_some()) {
        head_row = Some(row);
        row += HEAD_H + GAP;
    }

    for id in order {
        if head_row.is_none() && head_before.as_ref() == Some(id) {
            head_row = Some(row);
            row += HEAD_H + GAP;
        }
        rows.insert(id.clone(), row);
        row += BLOCK_H + GAP;
    }
    (rows, head_row, row.saturating_sub(GAP))
}

/// Where every chain and the HEAD block sit horizontally, and what colour
/// each commit is.
struct Placement {
    lane: HashMap<Oid, usize>,
    head_lane: usize,
    lane_count: usize,
    tint: HashMap<Oid, usize>,
    branch_tints: HashMap<String, usize>,
}

/// Give each commit a lane, and each a colour group.
///
/// Three steps, and the whole point of the first three is that a **chain owns
/// its column outright** for as long as it is on screen:
///
/// 1. **Chains.**  A chain is a maximal run of first-parent links — which is
///    what a branch looks like to a reader.  Where several commits share a
///    parent, the parent joins the chain that *started highest*, so the trunk
///    keeps going down one column instead of being annexed by whichever
///    side branch git happened to list first.  (It was: a four-commit `main`
///    handed its last commit to a topic branch, and the main line stepped
///    sideways at the bottom for no reason a reader could see.)
/// 2. **Spans.**  Each chain claims the rows from its first block to its last,
///    extended down to the commit it points into and up to any merge that
///    points at it — the rows its arrows need as well as its blocks.
/// 3. **Colouring.**  Chains whose spans overlap must get different lanes;
///    greedy leftmost-free assignment does that, and lets two branches that
///    never coexist vertically share a column.
///
/// The invariant this buys: a lane holds one chain at a time, so a vertical
/// arrow segment drawn in a lane can never pass behind another chain's block.
/// The HEAD block is placed last, against the same reservations, because it
/// is a block in the stack like any other — see [`place_head`].
fn place(
    order: &[Oid],
    rows: &HashMap<Oid, u16>,
    head_row: Option<u16>,
    dag: &Dag,
    projection: &Projection,
) -> Placement {
    // --- 1. chains ---
    let mut chain: HashMap<Oid, usize> = HashMap::new();
    let mut members: Vec<Vec<Oid>> = Vec::new();
    // Which chain has claimed each commit, and with what id.  Every child of a
    // commit precedes it in the draw order, so by the time a commit is reached
    // every claim on it has been made and the smallest wins — and chain ids
    // are handed out top to bottom, so the smallest id is the highest chain.
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
        // are the tops of their own chains, which is what makes a merge read
        // as two columns coming together rather than one column forking.
        if let Some(parent) = projection.parents(dag, id).first() {
            claims
                .entry(parent.clone())
                .and_modify(|held| *held = (*held).min(c))
                .or_insert(c);
        }
    }

    // --- 2. spans ---
    let row_of = |id: &Oid| rows.get(id).copied();
    let mut spans: Vec<(u16, u16)> = members
        .iter()
        .map(|m| {
            let top = m.iter().filter_map(row_of).min().unwrap_or(0);
            let bottom = m.iter().filter_map(row_of).max().unwrap_or(0) + BLOCK_H;
            (top, bottom)
        })
        .collect();

    for (c, m) in members.iter().enumerate() {
        // The exit arrow drops through this lane down to the gap above the
        // commit it points at, so those rows belong to this chain too.
        if let Some(target) = m.last().and_then(|last| projection.parents(dag, last).first()) {
            if let Some(row) = row_of(target) {
                spans[c].1 = spans[c].1.max(row);
            }
        }
    }
    for id in order {
        // A merge's second arrow crosses immediately below the merge and then
        // drops through the *target's* lane, so those rows belong to it.
        for parent in projection.parents(dag, id).iter().skip(1) {
            let (Some(&c), Some(row)) = (chain.get(parent), row_of(id)) else { continue };
            spans[c].0 = spans[c].0.min(row + BLOCK_H);
        }
    }

    // --- 3. colouring ---
    let mut used: Vec<Vec<(u16, u16)>> = Vec::new();
    let mut lane_of = vec![0usize; members.len()];
    let mut by_top: Vec<usize> = (0..members.len()).collect();
    by_top.sort_by_key(|&c| (spans[c].0, c));

    for c in by_top {
        let lane = claim_lane(&mut used, spans[c], None);
        lane_of[c] = lane;
    }

    let lane: HashMap<Oid, usize> = chain.iter().map(|(id, &c)| (id.clone(), lane_of[c])).collect();
    let head_lane = place_head(&mut used, &lane, rows, head_row, dag);
    let (branch_tints, tint) = assign_tints(&members, dag, projection);

    Placement {
        lane_count: used.len().max(1),
        lane,
        head_lane,
        tint,
        branch_tints,
    }
}

/// The leftmost lane free over `span`, preferring `want` when it is free.
fn claim_lane(used: &mut Vec<Vec<(u16, u16)>>, span: (u16, u16), want: Option<usize>) -> usize {
    let (lo, hi) = span;
    let free = |taken: &Vec<(u16, u16)>| taken.iter().all(|&(a, b)| hi <= a || lo >= b);
    let lane = want
        .filter(|&w| used.get(w).map_or(true, free))
        .or_else(|| used.iter().position(free))
        .unwrap_or(used.len());
    while used.len() <= lane {
        used.push(Vec::new());
    }
    used[lane].push((lo, hi));
    lane
}

/// Which lane the HEAD block goes in.
///
/// Its own commit's lane, when that is free over HEAD's rows — the block sits
/// directly above the commit it names, so the arrow is one row long and reads
/// as "you are here".  When HEAD names a commit part-way down a chain, though,
/// that lane is carrying the arrow from the commit above, and putting a block
/// in it hides the arrow completely: blocks are painted after arrows, so what
/// you get is a line that stops dead at HEAD and a commit whose parent is
/// anybody's guess.  A checkout of anything but a branch tip did exactly that.
/// So HEAD then takes a lane of its own and points across instead.
fn place_head(
    used: &mut Vec<Vec<(u16, u16)>>,
    lane: &HashMap<Oid, usize>,
    rows: &HashMap<Oid, u16>,
    head_row: Option<u16>,
    dag: &Dag,
) -> usize {
    let Some(head_row) = head_row else { return 0 };
    let target = dag.head.target.as_ref();
    let want = target.and_then(|id| lane.get(id)).copied();
    // Down to the gap above its commit: that is where HEAD's own arrow runs.
    let bottom = target
        .and_then(|id| rows.get(id))
        .copied()
        .unwrap_or(head_row + HEAD_H);
    claim_lane(used, (head_row, bottom.max(head_row + HEAD_H)), want)
}

/// Which colour group each commit and each local branch belongs to.
///
/// Walking *down* a chain, a commit takes the colour of the nearest branch
/// label at or above it.  That is exactly how the graph reads: above `main`'s
/// label the commits are only on `test-branch`, and at `main`'s label and
/// below they are on `main` — even though every one of them is also on
/// `test-branch`.
///
/// Colour is deliberately **not** the lane.  A branch that is merely ahead of
/// another is not a fork, so both sit in one column, correctly — and colouring
/// by column then painted the whole history one colour and lost the very
/// distinction the colours exist to draw.
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

/// How wide one lane is./// How wide one lane is.
///
/// Divided between the lanes in play and then clamped: a single-branch
/// repository would otherwise draw one block across the whole terminal, and a
/// repository with nine branches would draw nine unreadable slivers (those
/// scroll horizontally instead).
fn lane_width(width: u16, lane_count: usize, natural: u16) -> u16 {
    let count = lane_count.max(1) as u16;
    let each = ((width + LANE_GAP) / count).saturating_sub(LANE_GAP);
    // A block is as wide as what is written in it, never as wide as the
    // space that happens to be free.  Dividing the viewport up made every
    // block on a wide terminal a 72-column banner around a 30-column commit
    // message, which reads as a layout bug rather than as a graph.
    natural.min(each).clamp(MIN_LANE, MAX_LANE)
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
        // Drawn at `left + 2`, so two columns of lead-in plus one of trailing
        // room inside the right border.
        inner = inner.max(summary + 3);

        if let Some(commit) = dag.get(id) {
            let meta = commit.author.chars().count() as u16 + 3 + AGE_COLS;
            inner = inner.max(meta + 2 + COUNTS_COLS);
        }
    }
    // The HEAD block's one line: `● ` plus the branch name.
    if let Some(branch) = dag.head.branch.as_ref() {
        inner = inner.max(branch.chars().count() as u16 + 4);
    }
    inner + 2
}

/// The ref labels on `id`'s block, with the column each starts at.
///
/// Truncated to what the border can hold: a block with six tags on it must
/// not draw past its own edge and into the lane beside it.
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
    total_rows: u16,
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
            edges.push(Edge {
                child: block.id.clone(),
                slot: 0,
                parent: Some(target.id.clone()),
                from_lane: block.lane,
                to_lane: target.lane,
                row: block.row + block.height,
                end_row: target.row,
                cross_row: target.row.saturating_sub(1),
            });
            continue;
        }
        for (slot, parent) in projection.parents(dag, &block.id).iter().enumerate() {
            let target = index.get(parent).map(|&i| &blocks[i]);
            let row = block.row + block.height;
            let end_row = target.map_or(total_rows, |b| b.row);
            edges.push(Edge {
                child: block.id.clone(),
                slot,
                // A parent outside the drawn set is left as `None` so the
                // renderer draws a stub rather than an arrow to nowhere.
                parent: target.map(|b| b.id.clone()),
                from_lane: block.lane,
                to_lane: target.map_or(block.lane, |b| b.lane),
                row,
                end_row,
                // See `Edge::cross_row`: the first parent crosses late, in the
                // gap above the commit it points at, so the drop stays in the
                // child's own lane; a merge's other parents cross early,
                // because the child's lane carries on down past them.
                cross_row: if slot == 0 { end_row.saturating_sub(1).max(row) } else { row },
            });
        }
    }
    edges
}

/// Everything the cursor can land on, in drawing order.
fn build_focusables(layout: &Layout, head: &Head) -> Vec<Focusable> {
    let mut out = Vec::new();
    for block in &layout.blocks {
        let base = layout.lane_col(block.lane);
        match block.kind {
            BlockKind::Head => out.push(Focusable {
                focus: Focus::Head,
                row: block.row + 1,
                col: base,
            }),
            _ => {
                // Labels sit on the block's top border, one row above the
                // block's own focus point, so `j` off a branch label lands on
                // the commit it labels rather than skipping past it.
                for (name, col) in &block.labels {
                    out.push(Focusable {
                        focus: Focus::Ref(name.clone()),
                        row: block.row,
                        col: base + col,
                    });
                }
                out.push(Focusable {
                    focus: Focus::Commit(block.id.clone()),
                    row: block.row + 1,
                    col: base,
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
            row: edge.row,
            col: layout.lane_col(edge.from_lane) + 1,
        });
    }
    let _ = head;
    out.sort_by_key(|f| (f.row, f.col));
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
    ///   feature: d ── c ─┐
    ///                    ├── a
    ///   main:    f ── e ─┘
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
    /// the shape where lanes used to be recycled under an arrow still in
    /// flight, and where the trunk lost its column at the bottom.
    ///
    /// ```text
    ///   main:    four ── three ── two ── one
    ///   topic-a:            a2 ── a1 ──────┘   (from two)
    ///   topic-b:                  b1 ─────┘    (from three)
    ///   topic-c:            c2 ── c1 ──────┘   (from one)
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
    /// column, and HEAD names a commit part-way down it.  The shape a
    /// `git checkout -b` and one commit produces, and the one that had both an
    /// arrow drawn behind the HEAD block and a whole history in one colour.
    ///
    /// ```text
    ///   test-branch: top
    ///   main:        mid   <- HEAD
    ///                base
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
    fn a_chain_keeps_one_lane_and_a_branch_opens_another() {
        let (_, _, layout) = laid_out(120);
        let lane = |id: &str| layout.block(&Oid::new(id)).unwrap().lane;
        // `d → c → a` is one chain and stays in a column…
        assert_eq!(lane("d"), lane("c"));
        assert_eq!(lane("c"), lane("a"));
        // …while `f → e` is a second, in its own.
        assert_eq!(lane("f"), lane("e"));
        assert_ne!(lane("d"), lane("f"));
        assert_eq!(layout.lane_count, 2);
    }

    /// Blocks never overlap: one commit per row band, in topological order.
    /// A packed layout can draw a commit above its own ancestor, which in a
    /// view whose premise is "the picture is the truth" is not cosmetic.
    #[test]
    fn every_block_sits_below_the_one_before_it_and_none_overlap() {
        let (_, _, layout) = laid_out(120);
        let mut last_bottom = 0;
        for block in &layout.blocks {
            assert!(
                block.row >= last_bottom,
                "block {} starts at {} but the previous ended at {last_bottom}",
                block.id,
                block.row
            );
            last_bottom = block.row + block.height;
        }
        assert!(layout.total_rows >= last_bottom - GAP);
    }

    /// HEAD is a block of its own above the graph, pointing at the commit it
    /// names — the one pointer that should be findable without reading.
    /// HEAD sits *immediately* above its own commit, not at the top of the
    /// graph: from the top its arrow spans however deep HEAD happens to be,
    /// and a screenful of `│` between a block and its target says nothing.
    #[test]
    fn head_sits_directly_above_the_commit_it_names() {
        let (_, _, layout) = laid_out(120);
        let head = layout.block(&Oid::new("HEAD")).expect("HEAD is drawn");
        let target = layout.block(&Oid::new("f")).expect("its commit is drawn");
        assert_eq!(head.kind, BlockKind::Head);
        assert_eq!(head.lane, target.lane, "and in the same lane");
        assert_eq!(head.row + head.height + GAP, target.row, "one gap apart");
        assert!(layout.locate(&Focus::Head).is_some());
        // `f` is not the newest commit in this fixture, so this really is a
        // placement decision and not the top of the list by accident.
        assert!(head.row > 0);
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

    /// Lanes that fit must not be scrolled off: N lanes occupy
    /// `N * (w + gap) - gap`, and dividing the bare width reports one too few
    /// whenever they fit exactly.
    #[test]
    fn lanes_that_exactly_fit_are_all_counted_as_visible() {
        let (_, _, layout) = laid_out(104);
        assert_eq!(layout.lane_count, 2);
        let span = layout.lane_col(1) + layout.lane_width;
        assert!(span <= 104, "two lanes really do fit in 104 columns: {span}");
        assert_eq!(layout.visible_lanes(104), 2);
        // And a viewport one column too narrow honestly reports one.
        assert_eq!(layout.visible_lanes(span - 1), 1);
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
    /// nowhere — the honest picture of a truncated walk.
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
        let edge = &layout.edges[0];
        assert_eq!(edge.parent, None);
        assert_eq!(edge.end_row, layout.total_rows);
    }

    /// `j` from a block must land on the arrow directly beneath it, not on
    /// whatever else happens to be one row down in another lane.
    #[test]
    fn moving_down_prefers_the_thing_directly_below() {
        let (_, _, layout) = laid_out(120);
        let from = Focus::Commit(Oid::new("d"));
        assert_eq!(
            layout.step_where(&from, Dir::Down, |_| true),
            Some(Focus::Edge { child: Oid::new("d"), slot: 0 })
        );
    }

    #[test]
    fn moving_is_reversible_and_stops_at_the_edges() {
        let (_, _, layout) = laid_out(120);
        let start = Focus::Commit(Oid::new("d"));
        let down = layout.step_where(&start, Dir::Down, |_| true).unwrap();
        assert_eq!(layout.step_where(&down, Dir::Up, |_| true), Some(start));

        // The graph has ends: nothing above the first focusable, nothing
        // below the last.  (HEAD is no longer either — it sits beside its own
        // commit, wherever in the graph that is.)
        let first = layout.focusables.first().unwrap().focus.clone();
        let last = layout.focusables.last().unwrap().focus.clone();
        assert_eq!(layout.step_where(&first, Dir::Up, |_| true), None);
        assert_eq!(layout.step_where(&last, Dir::Down, |_| true), None);
    }

    /// `h`/`l` cross lanes at a comparable height rather than jumping to the
    /// top of the next branch.
    /// `l` means "the next lane, beside where I am".  A lane is tall and
    /// narrow, so the nearest thing *by column* is very often a label ten rows
    /// up on some other block — which is not what the key means.
    #[test]
    fn moving_sideways_stays_at_the_same_height() {
        let (_, _, layout) = laid_out(120);
        let from = Focus::Commit(Oid::new("d"));
        let row_of = |f: &Focus| layout.locate(f).unwrap().row as i32;
        let target = layout.step_where(&from, Dir::Right, |_| true).expect("a lane to the right");
        assert!(
            (row_of(&target) - row_of(&from)).abs() <= BLOCK_H as i32,
            "sideways travelled {} rows",
            (row_of(&target) - row_of(&from)).abs()
        );
        // And it really did change lane.
        let col_of = |f: &Focus| layout.locate(f).unwrap().col;
        assert!(col_of(&target) > col_of(&from));
    }

    /// A branch label sits on its block's top border, so `j` off the label
    /// lands on the commit it labels.
    #[test]
    fn a_branch_label_is_selectable_and_sits_above_its_commit() {
        let (_, _, layout) = laid_out(120);
        let label = layout.locate(&Focus::Ref("main".into())).expect("main is drawn");
        let block = layout.block(&Oid::new("f")).unwrap();
        assert_eq!(label.row, block.row);
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

    /// One branch must not stretch across a 200-column terminal, and nine
    /// branches must not become nine unreadable slivers.
    #[test]
    fn lane_width_is_clamped_at_both_ends() {
        assert_eq!(lane_width(300, 1, 500), MAX_LANE);
        assert_eq!(lane_width(40, 9, 500), MIN_LANE);
        // In between it divides the space up.
        let mid = lane_width(120, 2, 500);
        assert!((MIN_LANE..=MAX_LANE).contains(&mid), "{mid}");
    }

    /// A block is as wide as what is written in it.  Dividing the viewport up
    /// instead drew a 72-column banner around a 30-column commit message on
    /// any reasonably wide terminal.
    #[test]
    fn a_block_is_no_wider_than_its_contents() {
        let (dag, projection, layout) = laid_out(300);
        let natural = natural_width(&dag, &projection, &draw_order(&dag, &projection));
        assert!(natural < MAX_LANE, "the fixture is short: {natural}");
        assert_eq!(layout.lane_width, natural.max(MIN_LANE));
        assert!(
            layout.lane_width < MAX_LANE,
            "a wide terminal stretched a short commit to {}",
            layout.lane_width
        );
    }

    /// …and long contents still get the room, up to the clamp.
    #[test]
    fn a_long_summary_widens_the_block() {
        let long = "a commit message long enough that it needs the whole lane to itself";
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
        assert!(
            layout.lane_width >= long.chars().count() as u16,
            "a {}-column summary got {} columns",
            long.chars().count(),
            layout.lane_width
        );
    }

    /// A block full of tags must not draw past its own edge into the lane
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
    /// commit, no lane and no block.
    #[test]
    fn an_empty_repository_lays_out_to_nothing() {
        let dag = Dag::new(Vec::new(), Vec::new(), Head::default(), WorkTree::default(), false);
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 80);
        assert!(layout.blocks.is_empty());
        assert!(layout.focusables.is_empty());
        assert_eq!(layout.initial_focus(), None);
    }
    /// The invariant the whole lane assignment exists to provide: **no arrow
    /// is ever drawn through a block**.  Blocks are painted after arrows, so a
    /// line that crosses one does not merely look wrong — it vanishes, and the
    /// reader is left with an arrow that stops dead and a commit whose parent
    /// is anybody's guess.
    ///
    /// Checked over the awkward shapes rather than the tidy one: several
    /// topic branches cut from different points of a trunk is where lanes
    /// used to get recycled underneath an arrow still using them.
    #[test]
    fn no_arrow_is_drawn_through_a_block() {
        for (name, dag) in [
            ("two branches", dag()),
            ("many branches", tangled()),
            ("a merge", merged()),
            // HEAD part-way down a chain: the block lands in the middle of a
            // lane an arrow is already using.
            ("a branch ahead", ahead()),
        ] {
            let projection = Plan::default().project(&dag);
            for width in [80, 120, 200, 400] {
                let layout = compute(&dag, &projection, width);
                for edge in &layout.edges {
                    for (row, col) in layout.route(edge).cells() {
                        for block in &layout.blocks {
                            // The endpoints are meant to touch: an arrow
                            // leaves one border and its head sits in the gap
                            // above the next.  Everything else is a crossing.
                            let left = layout.lane_col(block.lane);
                            let inside = row > block.row
                                && row < block.row + block.height
                                && col >= left
                                && col < left + layout.lane_width;
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

    /// A branch is a column.  The trunk used to hand its oldest commits to
    /// whichever topic branch git happened to list first, so `main` stepped
    /// sideways near the bottom for no reason a reader could see.
    #[test]
    fn a_first_parent_chain_keeps_one_lane_all_the_way_down() {
        let dag = tangled();
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 200);
        let lane = |id: &str| layout.block(&Oid::new(id)).expect(id).lane;

        // The trunk, top to bottom.
        for id in ["four", "three", "two", "one"] {
            assert_eq!(lane(id), lane("four"), "{id} left the trunk's lane");
        }
        // …and each topic branch is somewhere else, in one lane of its own.
        for (tip, base) in [("a2", "a1"), ("c2", "c1")] {
            assert_eq!(lane(tip), lane(base), "{tip} and {base} are one chain");
            assert_ne!(lane(tip), lane("four"), "{tip} is not the trunk");
        }
    }

    /// Two branches that never coexist vertically may share a column: the
    /// point of colouring spans rather than handing every chain its own lane
    /// is that a graph with a long history does not grow a lane per branch
    /// that ever existed.
    #[test]
    fn chains_that_do_not_overlap_share_a_lane() {
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
        // Both topics hang off the trunk at different heights and neither
        // outlives the other, so two lanes are enough for four chains.
        assert!(layout.lane_count <= 2, "{} lanes for two side commits", layout.lane_count);
    }

    /// A branch that is merely *ahead* of another is not a fork, so both sit
    /// in one column — and colouring by column then painted the whole history
    /// one colour and lost the distinction entirely.  Colour is the branch a
    /// commit is on: walking down, each takes the nearest label at or above it.
    #[test]
    fn commits_take_the_colour_of_the_branch_they_are_on() {
        let dag = ahead();
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 200);
        let tint = |id: &str| layout.block(&Oid::new(id)).expect(id).tint;

        // One column, because that is the truth of this repository…
        assert_eq!(
            layout.block(&Oid::new("top")).unwrap().lane,
            layout.block(&Oid::new("base")).unwrap().lane
        );
        // …and two colours, because there are two branches.
        assert_ne!(tint("top"), tint("mid"), "the branches are one colour");
        assert_eq!(tint("mid"), tint("base"), "everything at and below `main`");

        // The labels match the commits they name, so a label and its run of
        // history read as one thing.
        assert_eq!(layout.branch_tints.get("test-branch"), Some(&tint("top")));
        assert_eq!(layout.branch_tints.get("main"), Some(&tint("mid")));
    }

    /// HEAD sits directly above the commit it names, in that commit's lane —
    /// unless that lane is carrying an arrow past it, in which case a block
    /// there would hide the arrow completely.
    #[test]
    fn head_steps_aside_rather_than_landing_on_an_arrow() {
        // At a branch tip there is nothing passing, so it stays put.
        let dag = dag();
        let layout = compute(&dag, &Plan::default().project(&dag), 200);
        let head = layout.block(&Oid::new("HEAD")).expect("HEAD is drawn");
        assert_eq!(head.lane, layout.block(&Oid::new("f")).unwrap().lane);

        // Part-way down a chain, the arrow from the commit above owns that
        // lane, so HEAD takes one of its own and points across.
        let dag = ahead();
        let layout = compute(&dag, &Plan::default().project(&dag), 200);
        let head = layout.block(&Oid::new("HEAD")).expect("HEAD is drawn");
        let target = layout.block(&Oid::new("mid")).unwrap();
        assert_ne!(head.lane, target.lane, "HEAD is sitting on the arrow");
        assert!(head.row < target.row, "and still directly above its commit");
    }

}
