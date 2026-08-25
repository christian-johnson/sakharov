//! Where every block and arrow goes.
//!
//! The single geometry model for the version-control view — the same role
//! `table::layout` plays for the grid and `notebook_ui::nb_cell_height` for
//! the notebook.  The renderer draws from a [`Layout`] and the navigation
//! moves through the same [`Layout`], so "what is under the cursor" and "what
//! is on screen" cannot drift apart.
//!
//! ## Two axes, not four directions
//!
//! The graph can be drawn either way round — history left to right, or top to
//! bottom ([`Orientation`]).  Rather than two layouts, everything here is
//! computed in **graph space**, whose two axes are named for what they carry
//! rather than for where they end up on screen:
//!
//! * **along** — the time axis.  `along` 0 is the *oldest* commit loaded and
//!   grows toward the newest, whichever way the picture is later turned, and
//!   an arrow always points **back** along it, from a commit to the parent it
//!   follows.
//! * **across** — the track axis.  A commit's track is inherited by its first
//!   parent, so a chain keeps one track and a branch point opens another.
//!
//! [`Metrics`] is the only thing that knows which screen axis is which: it
//! gives a block its extent along each, and the renderer maps a graph cell to
//! a screen cell through [`Layout::screen`].  Everything between — chains,
//! colours, spans, track assignment, arrow routing, the focus walk — is
//! written once and is true of both pictures.
//!
//! Vertical draws the newest commit at the *top* (the order `git log` prints,
//! and the order the old vertical view used), so its along axis is reversed on
//! the way to the screen and nowhere else — see [`Layout::display_along`].
//!
//! One commit per band along the time axis.  Commits are *not* packed several
//! to a band even when they would fit: a generation-packed layout looks
//! tidier, but it can place a commit visually before one of its own
//! descendants, and in a view whose entire purpose is that the picture is the
//! truth, that is not a cosmetic problem.

use std::collections::{HashMap, HashSet};

use super::{
    plan::Projection,
    Dag, Oid,
};

/// A block's extent across the track axis: border, two summary rows,
/// metadata, border.
///
/// Every block is the same size, HEAD included, so a track is a band of one
/// fixed size and the arrows between two blocks in one track are a straight
/// line.  On screen this is always the block's **height**: a block is drawn as
/// the same five-row box whichever way the graph runs, and only the axis its
/// neighbours are found along changes.
pub const BLOCK_H: u16 = 5;
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

/// Which way the graph is drawn.
///
/// A preference, not a mode: the two pictures show the same graph and every
/// gesture means the same thing in both.  `h`/`l` and `j`/`k` follow the
/// screen, so whichever axis history runs along is the one they travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    /// History left to right, a branch is a row.
    #[default]
    Horizontal,
    /// History top to bottom, newest first, a branch is a column.
    Vertical,
}

impl Orientation {
    /// Parse a config value.  Anything unrecognised is horizontal, which is
    /// the default rather than an error: a typo in a display preference must
    /// not stop the view opening.
    pub fn parse(name: &str) -> Orientation {
        match name.trim().to_ascii_lowercase().as_str() {
            "vertical" | "down" | "v" => Orientation::Vertical,
            _ => Orientation::Horizontal,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Orientation::Horizontal => "horizontal",
            Orientation::Vertical => "vertical",
        }
    }

    pub fn flipped(self) -> Orientation {
        match self {
            Orientation::Horizontal => Orientation::Vertical,
            Orientation::Vertical => Orientation::Horizontal,
        }
    }

    /// Along the time axis, the way the screen goes *forward* — right, or
    /// down — and the way it goes back.
    ///
    /// Paging and the two end-of-graph motions ask by name, and they ask in
    /// **screen** terms rather than in history's: `J` pages down the screen and
    /// `gg` goes to the top of it, in a graph exactly as in a buffer.  Which
    /// end of history that lands on is then a property of the picture — the
    /// oldest commit is at the left in one and at the bottom in the other —
    /// and stating it the other way round made `gg` walk *away* from the top
    /// of the screen, which is the one thing `gg` means everywhere else in the
    /// editor.
    pub fn forward(self) -> Dir {
        match self {
            Orientation::Horizontal => Dir::Right,
            Orientation::Vertical => Dir::Down,
        }
    }

    pub fn back(self) -> Dir {
        match self {
            Orientation::Horizontal => Dir::Left,
            Orientation::Vertical => Dir::Up,
        }
    }
}

/// How big things are in graph space, for one orientation.
///
/// The one place that knows which screen axis is which.  A block is always the
/// same box on screen — [`Layout::block_width`] columns by [`BLOCK_H`] rows —
/// so turning the graph only swaps which of the two the time axis runs along.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub orient: Orientation,
    /// A block's extent along the time axis.
    pub block_along: u16,
    /// A block's extent across the track axis.
    pub block_across: u16,
    /// Between one block and the next, where the arrows run.
    pub gap_along: u16,
    /// Between one track and the next.
    pub gap_across: u16,
    /// Extent the branch name band takes inside each track's stride.
    ///
    /// Horizontal writes the name on the row above its track, which costs a
    /// row per track.  Vertical writes it in one row pinned to the top of the
    /// viewport, above every track at once — so it costs nothing here, and the
    /// renderer reserves that row instead.
    pub label_across: u16,
}

/// Columns between blocks in the horizontal picture, where arrows run.
const GAP_H: u16 = 3;
/// Rows between tracks in the horizontal picture.
const TRACK_GAP_H: u16 = 1;
/// Rows between blocks in the vertical picture.
const GAP_V: u16 = 2;
/// Columns between tracks in the vertical picture.
const TRACK_GAP_V: u16 = 3;
/// The row above each horizontal track that carries the name of the branch
/// owning it.
///
/// A track *is* a branch (see [`place`]), so the band needs somewhere to say
/// which one — otherwise the only place a branch is named is the label on its
/// tip, which on a long history is a screenful away from the commits that are
/// on it.
pub const LABEL_H: u16 = 1;

impl Metrics {
    /// The metrics for `orient`, given the width one block was sized to.
    pub fn new(orient: Orientation, block_width: u16) -> Metrics {
        match orient {
            Orientation::Horizontal => Metrics {
                orient,
                block_along: block_width,
                block_across: BLOCK_H,
                gap_along: GAP_H,
                gap_across: TRACK_GAP_H,
                label_across: LABEL_H,
            },
            Orientation::Vertical => Metrics {
                orient,
                block_along: BLOCK_H,
                block_across: block_width,
                gap_along: GAP_V,
                gap_across: TRACK_GAP_V,
                label_across: 0,
            },
        }
    }

    /// From one block's start to the next's, along the time axis.
    pub fn along_stride(self) -> u16 {
        self.block_along + self.gap_along
    }

    /// From one track's start to the next's.
    pub fn across_stride(self) -> u16 {
        self.block_across + self.gap_across + self.label_across
    }

    /// True when the along axis is drawn back to front — vertical puts the
    /// newest commit at the top.
    pub fn reversed(self) -> bool {
        self.orient == Orientation::Vertical
    }
}

/// What the cursor can be on.
///
/// Arrows are deliberately **not** on the list.  An arrow is another name for
/// the commit it leaves — dragging one and dragging its block made the same
/// edit — so every arrow the cursor could stop on was a press that offered no
/// choice and put the next real destination one key further away.  Grab the
/// commit; the arrow follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    /// A commit block.
    Commit(Oid),
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
    /// Which track band the block sits in.
    pub track: usize,
    /// Where the block starts on the time axis, measured across the whole
    /// graph rather than the viewport.
    pub along: u16,
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
    /// `None` when the parent is past the loaded horizon — the arrow is drawn
    /// as a stub trailing off the left edge, which is the honest picture.
    pub parent: Option<Oid>,
    pub from_track: usize,
    pub to_track: usize,
    /// Where the arrow starts: one step back along time from the child block.
    pub along: u16,
    /// Where the arrowhead sits — one step *forward* in time from the parent
    /// block's far edge — or 0 when the parent was never loaded.
    pub end_along: u16,
    /// The point on the time axis where the arrow changes track.
    ///
    /// A first-parent link crosses **late**, in the gap immediately after the
    /// commit it points at, so the long part of the run stays in the child's
    /// own track — which that chain owns outright.  A merge's second parent
    /// crosses **early**, immediately before the child, because the child's
    /// track continues on to its own first parent and the run would go
    /// straight through it.  Either way the long run never enters a track
    /// somebody else's blocks are sitting in.
    pub cross_along: u16,
}

/// Where one arrow's cells actually go.
///
/// The route lives here rather than in the renderer because it is geometry,
/// and this module is the one place geometry is decided — the same reason
/// block positions and track assignment are here.  It is also what makes
/// [`no_arrow_is_drawn_through_a_block`] able to check the invariant the whole
/// track assignment exists to provide.
pub struct EdgeRoute {
    /// The track line the arrow leaves along, and the one it arrives along.
    pub from_across: u16,
    pub to_across: u16,
    /// Where the arrow starts, where it changes track, and where the arrowhead
    /// sits.  These *decrease* along the route: it points back in time.
    pub start: u16,
    pub cross: u16,
    pub head: u16,
    /// A parent past the loaded horizon: a stub that visibly goes nowhere.
    pub stub: bool,
}

impl EdgeRoute {
    /// Every cell the arrow paints, as `(along, across)`.
    pub fn cells(&self) -> Vec<(u16, u16)> {
        if self.stub {
            return (self.start.saturating_sub(1)..=self.start)
                .map(|along| (along, self.from_across))
                .collect();
        }
        let (lo, hi) = (
            self.from_across.min(self.to_across),
            self.from_across.max(self.to_across),
        );
        (self.cross + 1..=self.start)
            .map(|along| (along, self.from_across))
            .chain((lo..=hi).map(|across| (self.cross, across)))
            .chain((self.head..self.cross).map(|along| (along, self.to_across)))
            .collect()
    }
}

/// Something the cursor can sit on, and where it is.
///
/// Positions are graph cells rather than track indices, so several labels on
/// one block's border are distinguishable and the walk reaches them in the
/// order they are drawn.
#[derive(Debug, Clone)]
pub struct Focusable {
    pub focus: Focus,
    /// Position on the time axis.
    pub along: u16,
    /// Position on the track axis.
    pub across: u16,
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
    /// The branch each track belongs to, for the name written above it.
    /// Absent for a track holding history no branch points into.
    pub lane_labels: HashMap<usize, String>,
    /// Total extent of the graph along the time axis, for the scroll anchor
    /// to clamp against.
    pub total_along: u16,
    /// Which way this graph is drawn, and how big everything is in it.
    pub metrics: Metrics,
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
        let head = edge.end_along.min(edge.along);
        EdgeRoute {
            from_across: self.arrow_across(edge.from_track),
            to_across: self.arrow_across(edge.to_track),
            start: edge.along,
            cross: edge.cross_along.clamp(head, edge.along),
            head,
            stub: edge.parent.is_none(),
        }
    }

    /// Where a track's band starts — just past its name band, where it has one.
    pub fn track_across(&self, track: usize) -> u16 {
        track as u16 * self.metrics.across_stride() + self.metrics.label_across
    }

    /// Where a track's branch name is written.
    ///
    /// Horizontal writes it on the row above the band; vertical has no band of
    /// its own for it (`label_across` is 0) and the name goes in a row the
    /// renderer pins to the top of the viewport, over the track's columns.
    pub fn label_across(&self, track: usize) -> u16 {
        self.track_across(track) - self.metrics.label_across
    }

    /// Which track `across` falls in.  The inverse of [`Layout::track_across`],
    /// and the only place anything outside this module is allowed to work it
    /// out.
    pub fn track_at(&self, across: u16) -> usize {
        (across / self.metrics.across_stride()) as usize
    }

    /// The branch whose track this is, if it is a branch's.
    pub fn lane_label(&self, track: usize) -> Option<&str> {
        self.lane_labels.get(&track).map(String::as_str)
    }

    /// The line arrows run along within a track: the middle of a block, so a
    /// link between two blocks in one track is a straight line through the
    /// gap between them.
    pub fn arrow_across(&self, track: usize) -> u16 {
        self.track_across(track) + self.metrics.block_across / 2
    }

    /// From one block's start to the next's, along the time axis.
    pub fn along_stride(&self) -> u16 {
        self.metrics.along_stride()
    }

    /// How many whole tracks fit in `extent` (the viewport's size across).
    ///
    /// The `+ gap_across` is not a fudge: tracks are laid out with a gap
    /// *between* them, so N tracks occupy `N * stride - gap`.  Dividing the
    /// bare extent instead reports one track too few whenever they fit
    /// exactly, which scrolled a two-branch graph on a screen big enough for
    /// all of it.
    pub fn visible_tracks(&self, extent: u16) -> usize {
        ((extent + self.metrics.gap_across) / self.metrics.across_stride()).max(1) as usize
    }

    /// Where a run of `extent` cells starting at `along` is drawn.
    ///
    /// The identity in the horizontal picture.  Vertical draws the newest
    /// commit at the top, so its time axis is reversed here — the one place
    /// that happens, and the reason every other function in this module can be
    /// written as though time ran forwards.
    pub fn display_along(&self, along: u16, extent: u16) -> u16 {
        if self.metrics.reversed() {
            self.total_along.saturating_sub(along + extent)
        } else {
            along
        }
    }

    /// A graph cell as a screen cell `(x, y)`, before scrolling.
    ///
    /// The whole of the orientation, in four lines: which axis is which, and
    /// which way time runs.  The renderer maps through here and never works
    /// out a coordinate itself.
    pub fn screen(&self, along: u16, across: u16) -> (u16, u16) {
        let along = self.display_along(along, 1);
        match self.metrics.orient {
            Orientation::Horizontal => (along, across),
            Orientation::Vertical => (across, along),
        }
    }

    /// Where the viewport's scroll anchor lands on screen.
    ///
    /// `along` is already in display terms (it is what `update_scroll`
    /// maintains); the track becomes the other axis.
    pub fn screen_scroll(&self, along: u16, track: usize) -> (u16, u16) {
        let across = self.label_across(track);
        match self.metrics.orient {
            Orientation::Horizontal => (along, across),
            Orientation::Vertical => (across, along),
        }
    }

    /// A block's top-left corner on screen, before scrolling.
    ///
    /// Blocks are the same box either way round, so this is the only thing the
    /// renderer needs in order to draw one: everything inside it is written in
    /// ordinary screen coordinates.
    pub fn block_origin(&self, block: &Block) -> (u16, u16) {
        let along = self.display_along(block.along, self.metrics.block_along);
        let across = self.track_across(block.track);
        match self.metrics.orient {
            Orientation::Horizontal => (along, across),
            Orientation::Vertical => (across, along),
        }
    }

    /// Width available inside a block's borders.
    pub fn block_inner(&self) -> u16 {
        self.block_width.saturating_sub(2)
    }

    /// Which way `dir` travels in graph space: `+1`/`-1` along time, or
    /// across tracks.
    ///
    /// The only place a screen direction becomes a graph one.  Turning the
    /// graph turns the keys with it — in the vertical picture `j`/`k` walk
    /// history and `h`/`l` step between branches, without either the caller or
    /// the walk below knowing anything about it.  `Up` is *newer* there, since
    /// vertical draws the newest commit at the top.
    fn travel(&self, dir: Dir) -> (bool, i32) {
        match (self.metrics.orient, dir) {
            (Orientation::Horizontal, Dir::Right) => (true, 1),
            (Orientation::Horizontal, Dir::Left) => (true, -1),
            (Orientation::Horizontal, Dir::Down) => (false, 1),
            (Orientation::Horizontal, Dir::Up) => (false, -1),
            (Orientation::Vertical, Dir::Up) => (true, 1),
            (Orientation::Vertical, Dir::Down) => (true, -1),
            (Orientation::Vertical, Dir::Right) => (false, 1),
            (Orientation::Vertical, Dir::Left) => (false, -1),
        }
    }

    /// The nearest focusable in `dir` from `current`.
    ///
    /// Motion **along time** sorts by distance travelled first, so one press
    /// from a block lands on the commit immediately before it rather than on
    /// whatever happens to be furthest back.
    ///
    /// Motion **across tracks** sorts the other way round — nearest point in
    /// history first, then nearest track.  A track is long and thin, so the
    /// thing one track over is very often forty columns away (a label on some
    /// other branch's block), and travelling to it is not what that press
    /// means.  What it means is "the next branch, beside where I am".
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
        let (start_along, start_across) = (from.along as i32, from.across as i32);
        let (time_axis, sign) = self.travel(dir);

        self.focusables
            .iter()
            .filter(|f| f.focus != *current)
            .filter(|f| allow(&f.focus))
            .filter_map(|f| {
                let d_along = (f.along as i32 - start_along) * sign;
                let d_across = (f.across as i32 - start_across) * sign;
                let (travelled, sideways) = if time_axis {
                    (d_along, d_across.abs())
                } else {
                    (d_across, d_along.abs())
                };
                // Strictly forward along the axis of travel; ties on the other
                // axis are what the second sort key resolves.
                (travelled > 0).then(|| {
                    let key = if time_axis {
                        (travelled, sideways)
                    } else {
                        (sideways, travelled)
                    };
                    (key, f.focus.clone())
                })
            })
            .min_by_key(|(key, _)| *key)
            .map(|(_, focus)| focus)
    }

    /// The first thing worth putting the cursor on: HEAD if it is drawn, else
    /// the newest thing in the graph, which is the far end of the time axis.
    pub fn initial_focus(&self) -> Option<Focus> {
        self.focusables
            .iter()
            .find(|f| f.focus == Focus::Head)
            .or_else(|| self.focusables.last())
            .map(|f| f.focus.clone())
    }
}

/// Lay out `dag` as `projection` leaves it, for a content area `width` columns
/// wide, drawn the way `orient` says.
pub fn compute(dag: &Dag, projection: &Projection, width: u16, orient: Orientation) -> Layout {
    let order = draw_order(dag, projection);
    // The block width settles first: it is the block's width on screen in
    // either picture, so it decides both extents in graph space — and where a
    // block sits along the time axis is a multiple of one of them.
    let block_width = block_width(width, natural_width(dag, projection, &order));
    let metrics = Metrics::new(orient, block_width);
    let (cols, head_col, total_along) = assign_cols(dag, &order, metrics);
    let placed = place(&order, &cols, head_col, metrics, dag, projection);
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
            along: col,
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
            along: cols[id],
            labels: labels_for(dag, projection, id, inner),
            tint: placed.tint.get(id).copied().unwrap_or(0),
        });
    }
    blocks.sort_by_key(|b| b.along);

    let index: HashMap<Oid, usize> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.clone(), i))
        .collect();

    let edges = build_edges(&blocks, &index, dag, projection, metrics);
    let mut layout = Layout {
        focusables: Vec::new(),
        blocks,
        edges,
        track_count: placed.track_count,
        block_width,
        branch_tints: placed.branch_tints,
        lane_labels: placed.lane_labels,
        total_along,
        metrics,
        index,
    };
    layout.focusables = build_focusables(&layout);
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

/// Where every block sits along the time axis: one per band, oldest first.
///
/// Returns the commits' positions, HEAD's if it is drawn, and the total extent
/// of the graph.  Every block occupies the same band whatever track it ends up
/// in, which is what makes the gaps between them gaps in *every* track — and
/// that is what lets an arrow change track without ever running through a
/// block.
fn assign_cols(dag: &Dag, order: &[Oid], metrics: Metrics) -> (HashMap<Oid, u16>, Option<u16>, u16) {
    let mut cols = HashMap::new();
    let stride = metrics.along_stride();
    // Room at the old end for the stub that says older history was not loaded.
    let mut col = if dag.truncated { metrics.gap_along } else { 0 };
    let mut head_col = None;

    let head_at = dag.head.target.clone().filter(|id| order.contains(id));
    // A repository with no commits yet still has a HEAD worth showing: it says
    // which branch the first commit will be on.
    if head_at.is_none() && (dag.head.target.is_some() || dag.head.branch.is_some()) {
        head_col = Some(col);
        col += stride;
    }

    // Backwards: `order` is newest-first, and the oldest commit belongs at the
    // start of the time axis.
    for id in order.iter().rev() {
        cols.insert(id.clone(), col);
        col += stride;
        // HEAD sits immediately after the commit it names — the slot the next
        // commit would take.
        if head_col.is_none() && head_at.as_ref() == Some(id) {
            head_col = Some(col);
            col += stride;
        }
    }
    (cols, head_col, col.saturating_sub(metrics.gap_along))
}

/// Where every branch and the HEAD block sit vertically, and what colour each
/// commit is.
struct Placement {
    track: HashMap<Oid, usize>,
    head_track: usize,
    track_count: usize,
    tint: HashMap<Oid, usize>,
    branch_tints: HashMap<String, usize>,
    lane_labels: HashMap<usize, String>,
}

/// Give each commit a track, and each a colour group.
///
/// **A branch is a row.**  A track holds the commits of exactly one branch —
/// the ones [`assign_tints`] says are *on* it — and the row carries that
/// branch's name above it.  Sharing a row between two branches saves vertical
/// space and costs the one question the view exists to answer: a branch merely
/// *ahead* of another correctly shared a row, and the result was a history
/// where nothing on screen said which of the two you were looking at.
///
/// Four steps:
///
/// 1. **Chains.**  A chain is a maximal run of first-parent links.  Where
///    several commits share a parent, the parent joins the chain that *started
///    newest*, so the trunk keeps going along one row instead of being annexed
///    by whichever side branch git happened to list first.
/// 2. **Colours.**  [`assign_tints`] walks each chain back from its newest
///    commit, handing each one the nearest branch label at or after it.  A
///    colour group is therefore a contiguous segment of one chain — which is
///    what makes it safe to give it a row of its own.
/// 3. **Spans.**  Each group claims the columns from its oldest block to its
///    newest, extended left to whatever its arrows point into and right to any
///    merge that points at it — the columns its arrows need as well as its
///    blocks.
/// 4. **Rows.**  One per branch, in the order the branch tips are drawn.
///    History no branch points into (a topic whose branch was deleted, a
///    stretch reachable only through a merge) has no name to write, so those
///    groups share what is left by greedy interval colouring — never moving
///    into a named row, which would put commits under a branch name they are
///    not on.
///
/// The invariant this buys is unchanged: a track holds one group at a time and
/// a group is a contiguous chain segment, so a horizontal arrow segment drawn
/// in a track runs between two *consecutive* blocks of that group and can
/// never pass behind one.  The HEAD block is placed last, against the same
/// reservations, because it is a block in the graph like any other — see
/// [`place_head`].
fn place(
    order: &[Oid],
    cols: &HashMap<Oid, u16>,
    head_col: Option<u16>,
    metrics: Metrics,
    dag: &Dag,
    projection: &Projection,
) -> Placement {
    let block_along = metrics.block_along;
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

    // --- 2. colours ---
    //
    // Before the spans, not after: the row a commit sits in *is* the branch it
    // is on, and that is what this decides.
    let (branch_tints, tint) = assign_tints(&members, dag, projection);

    // --- 3. spans ---
    let col_of = |id: &Oid| cols.get(id).copied();
    let mut spans: HashMap<usize, (u16, u16)> = HashMap::new();
    for id in order {
        let (Some(&t), Some(col)) = (tint.get(id), col_of(id)) else { continue };
        let span = spans.entry(t).or_insert((col, col + block_along));
        span.0 = span.0.min(col);
        span.1 = span.1.max(col + block_along);
    }
    for id in order {
        let Some(&t) = tint.get(id) else { continue };
        // A first-parent arrow leaving the group runs back through *this*
        // track to the gap right of the commit it points at, so those columns
        // belong to the group too.
        if let Some(parent) = projection.parents(dag, id).first() {
            if tint.get(parent) != Some(&t) {
                if let (Some(col), Some(span)) = (col_of(parent), spans.get_mut(&t)) {
                    span.0 = span.0.min(col + block_along);
                }
            }
        }
        // A merge's second arrow crosses immediately left of the merge and
        // then runs back through the *target's* track, so those columns belong
        // to it.
        for parent in projection.parents(dag, id).iter().skip(1) {
            let (Some(&t), Some(col)) = (tint.get(parent), col_of(id)) else { continue };
            if let Some(span) = spans.get_mut(&t) {
                span.1 = span.1.max(col);
            }
        }
    }

    // --- 4. rows ---
    let named = branch_tints.len();
    let mut used: Vec<Vec<(u16, u16)>> = Vec::new();
    let mut track_of_tint: HashMap<usize, usize> = HashMap::new();
    // Branches first, one row each, in the order their tips are drawn.
    for t in 0..named {
        let Some(&span) = spans.get(&t) else { continue };
        track_of_tint.insert(t, used.len());
        used.push(vec![span]);
    }
    // Then the unnamed history, greedily, below every named row.
    let first_free = used.len();
    let mut unnamed: Vec<usize> = spans.keys().copied().filter(|&t| t >= named).collect();
    unnamed.sort_by_key(|t| (spans[t].0, *t));
    for t in unnamed {
        let track = claim_track(&mut used, spans[&t], first_free);
        track_of_tint.insert(t, track);
    }

    let track: HashMap<Oid, usize> = tint
        .iter()
        .filter_map(|(id, t)| track_of_tint.get(t).map(|&row| (id.clone(), row)))
        .collect();
    let lane_labels: HashMap<usize, String> = branch_tints
        .iter()
        .filter_map(|(name, t)| track_of_tint.get(t).map(|&row| (row, name.clone())))
        .collect();
    let head_track = place_head(&mut used, &track, cols, head_col, block_along, dag);

    Placement {
        track_count: used.len().max(1),
        track,
        head_track,
        tint,
        branch_tints,
        lane_labels,
    }
}

/// The topmost track at or below `from` that is free over `span`.
///
/// `from` is what keeps unnamed history out of a branch's row: a row with a
/// name written above it must hold that branch's commits and nothing else.
fn claim_track(used: &mut Vec<Vec<(u16, u16)>>, span: (u16, u16), from: usize) -> usize {
    let (lo, hi) = span;
    let free = |taken: &Vec<(u16, u16)>| taken.iter().all(|&(a, b)| hi <= a || lo >= b);
    let track = used
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, taken)| free(taken))
        .map_or(used.len(), |(i, _)| i);
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
/// branch, though, that row is carrying the arrow from the commit after it,
/// and putting a block in it hides the arrow completely: blocks are painted
/// after arrows, so what you get is a line that stops dead at HEAD and a
/// commit whose child is anybody's guess.  So HEAD then takes a row of its
/// own — a new one rather than some other branch's, which would file it under
/// a name it has nothing to do with.
fn place_head(
    used: &mut Vec<Vec<(u16, u16)>>,
    track: &HashMap<Oid, usize>,
    cols: &HashMap<Oid, u16>,
    head_col: Option<u16>,
    block_along: u16,
    dag: &Dag,
) -> usize {
    let Some(head_col) = head_col else { return 0 };
    let target = dag.head.target.as_ref();
    // Back to the gap right of its commit: that is where HEAD's own arrow runs.
    let left = target
        .and_then(|id| cols.get(id))
        .map_or(head_col, |col| col + block_along);
    let span = (left.min(head_col), head_col + block_along);
    let (lo, hi) = span;
    let free = |taken: &Vec<(u16, u16)>| taken.iter().all(|&(a, b)| hi <= a || lo >= b);

    let want = target
        .and_then(|id| track.get(id))
        .copied()
        .filter(|&w| used.get(w).is_some_and(free));
    let row = want.unwrap_or(used.len());
    while used.len() <= row {
        used.push(Vec::new());
    }
    used[row].push(span);
    row
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
///
/// One label is skipped on that walk: a branch **the branch you are on has
/// left behind** — one whose tip is an ancestor of HEAD's branch.  It owns no
/// history of its own; it is a pointer *into* the history you are standing in.
/// Letting it take the run gave a stale topic branch credit for the whole
/// trunk beneath it, so a repository whose every commit was made on `main`
/// drew almost all of them under some abandoned branch's name — the graph
/// saying the opposite of what happened.  It keeps its colour and its label on
/// the commit it points at, and since it then claims no commits it is given no
/// row (a named row with nothing in it says even less).
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

    // The branches HEAD's own branch contains outright: they name a commit in
    // the history you are on rather than any history of their own.  Detached,
    // there is no such branch and every label owns its run as before.
    let behind: HashSet<&str> = dag
        .head
        .branch
        .as_deref()
        .and_then(|on| projection.ref_target(dag, on).map(|tip| (on, tip)))
        .map(|(on, tip)| {
            let reach: HashSet<Oid> = projection.ancestors(dag, tip).into_iter().collect();
            branches
                .iter()
                .filter(|(_, name, target)| name != on && reach.contains(target))
                .map(|(_, name, _)| name.as_str())
                .collect()
        })
        .unwrap_or_default();

    let mut tint = HashMap::new();
    for (c, m) in members.iter().enumerate() {
        // A chain with no branch on it at all still needs a colour of its own;
        // numbering it past the branches keeps it from borrowing one.
        let mut current = branch_tints.len() + c;
        for id in m {
            if let Some(t) = branches
                .iter()
                .filter(|(_, _, target)| target == id)
                .filter(|(_, name, _)| !behind.contains(name.as_str()))
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

/// The shortest a truncated label is worth drawing: a space, two characters
/// and the ellipsis that says there is more.
const MIN_LABEL: u16 = 5;

/// The ref labels on `id`'s block, with the column each starts at.
///
/// Fitted to what the border can hold: a block with six tags on it must not
/// draw past its own edge and into the block beside it.  A name too long for
/// the room left is **shortened, never dropped** — a block width is clamped at
/// [`MAX_BLOCK`], so dropping meant a branch with a perfectly ordinary name
/// (`feat/vcs-graph-horizontal` is 25 characters) had no label anywhere on its
/// own tip, which reads as the editor not knowing the branch exists.
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
        let room = inner.saturating_sub(col);
        if room < MIN_LABEL {
            break;
        }
        let width = r.name.chars().count() as u16 + 2;
        let name = if width <= room {
            r.name.clone()
        } else {
            let keep = (room - 3) as usize;
            r.name.chars().take(keep).chain(std::iter::once('…')).collect()
        };
        let width = name.chars().count() as u16 + 2;
        labels.push((name, col));
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
    metrics: Metrics,
) -> Vec<Edge> {
    let block_along = metrics.block_along;
    let mut edges = Vec::new();
    for block in blocks {
        // The HEAD block points at the commit it names, which is the whole
        // reason it is a block rather than a label.
        if block.kind == BlockKind::Head {
            let Some(target) = dag.head.target.as_ref().and_then(|id| index.get(id)) else {
                continue;
            };
            let target = &blocks[*target];
            let end_along = target.along + block_along;
            edges.push(Edge {
                child: block.id.clone(),
                parent: Some(target.id.clone()),
                from_track: block.track,
                to_track: target.track,
                along: block.along.saturating_sub(1),
                end_along,
                cross_along: end_along,
            });
            continue;
        }
        for (slot, parent) in projection.parents(dag, &block.id).iter().enumerate() {
            let target = index.get(parent).map(|&i| &blocks[i]);
            let along = block.along.saturating_sub(1);
            let end_along = target.map_or(0, |b| b.along + block_along);
            edges.push(Edge {
                child: block.id.clone(),
                // A parent outside the drawn set is left as `None` so the
                // renderer draws a stub rather than an arrow to nowhere.
                parent: target.map(|b| b.id.clone()),
                from_track: block.track,
                to_track: target.map_or(block.track, |b| b.track),
                along,
                end_along,
                cross_along: cross_at(blocks, block, target, slot, along, end_along, metrics),
            });
        }
    }
    edges
}

/// Where one arrow changes track.
///
/// See [`Edge::cross_along`] for the rule: a first parent crosses **late**, so
/// its long run stays in the child's own track; a merge's other parents cross
/// **early**, because the child's track carries on past them to its own first
/// parent.
///
/// Either choice puts the long run in *somebody's* track, and a track is only
/// clear between two consecutive blocks of the group that owns it.  A merge of
/// a commit its branch has since built on — merge a feature branch, keep
/// working on it — leaves blocks between the merge and its parent, and the
/// early run went straight through them.  Blocks are painted after arrows, so
/// that is not a cosmetic problem: the line vanishes into a box and comes out
/// the other side, and the parent it names is anybody's guess.  So when the
/// preferred run is blocked and the other one is clear, the other one is taken;
/// when both are blocked the preference stands, which is at least the picture
/// the reader already knows.
fn cross_at(
    blocks: &[Block],
    child: &Block,
    parent: Option<&Block>,
    slot: usize,
    along: u16,
    end_along: u16,
    metrics: Metrics,
) -> u16 {
    let (early, late) = (along, end_along.min(along));
    let preferred = if slot == 0 { late } else { early };
    let Some(parent) = parent else { return preferred };

    // Which track the long run travels in, for each choice.
    let clear = |track: usize| {
        !blocks.iter().any(|b| {
            b.track == track
                && b.along + metrics.block_along > late
                && b.along < along
                && b.id != child.id
                && b.id != parent.id
        })
    };
    let (preferred_track, other_track, other) = if slot == 0 {
        (child.track, parent.track, early)
    } else {
        (parent.track, child.track, late)
    };
    if clear(preferred_track) || !clear(other_track) {
        preferred
    } else {
        other
    }
}

/// Everything the cursor can land on, in drawing order: the blocks, and the
/// ref labels written on their borders.  Arrows are not on the list — see
/// [`Focus`].
fn build_focusables(layout: &Layout) -> Vec<Focusable> {
    let mut out = Vec::new();
    let vertical = layout.metrics.orient == Orientation::Vertical;
    // Where the block's top border is, in graph terms.  Vertical draws the
    // newest commit at the top, so a block's top row is its *newest* cell
    // along time — the far end of the block, not its start.
    let top_of = |block: &Block| {
        if vertical {
            block.along + layout.metrics.block_along - 1
        } else {
            block.along
        }
    };
    for block in &layout.blocks {
        let track = layout.track_across(block.track);
        let top = top_of(block);
        // A label is written along the block's top border in both pictures,
        // because text always reads left to right — so the offset the layout
        // gave it is an offset along time in one picture and across tracks in
        // the other.  Everything downstream (the walk, the renderer) then
        // treats it as an ordinary graph cell.
        let label_at = |offset: u16| {
            if vertical {
                (top, track + offset)
            } else {
                (top + offset, track)
            }
        };
        // One step *into* the block from its top border, so a press off a
        // branch label lands on the commit it labels rather than skipping past
        // it.
        let body = if vertical { (top - 1, track) } else { (top, track + 1) };
        match block.kind {
            BlockKind::Head => out.push(Focusable {
                focus: Focus::Head,
                along: body.0,
                across: body.1,
            }),
            _ => {
                for (name, offset) in &block.labels {
                    let (along, across) = label_at(*offset);
                    out.push(Focusable { focus: Focus::Ref(name.clone()), along, across });
                }
                out.push(Focusable {
                    focus: Focus::Commit(block.id.clone()),
                    along: body.0,
                    across: body.1,
                });
            }
        }
    }
    out.sort_by_key(|f| (f.along, f.across));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{
        plan::{Edit, Plan},
        Commit, Head, Ref, RefKind, WorkTree,
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

    /// A merge of a commit whose branch then carried on.
    ///
    /// Merge a feature branch, keep working on it: the merge's second parent
    /// is no longer its branch's tip, so blocks of that branch sit between the
    /// merge and the commit it names — right where that arrow wants to run.
    fn merge_of_a_busy_branch() -> Dag {
        Dag::new(
            vec![
                commit("later", &["side"]),
                commit("m", &["trunk", "side"]),
                commit("side", &["root"]),
                commit("trunk", &["root"]),
                commit("root", &[]),
            ],
            vec![branch("main", "m"), branch("feature", "later")],
            Head { branch: Some("main".into()), target: Some(Oid::new("m")) },
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
        let layout = compute(&dag, &projection, width, Orientation::Horizontal);
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
                block.along >= last_right,
                "block {} starts at {} but the previous ended at {last_right}",
                block.id,
                block.along
            );
            last_right = block.along + layout.block_width;
        }
        assert!(layout.total_along >= last_right - layout.metrics.gap_along);
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
                    parent.along + layout.block_width <= child.along,
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
        assert_eq!(target.along + layout.along_stride(), head.along, "one gap apart");
        assert!(layout.locate(&Focus::Head).is_some());
        // `f` is not the newest commit in this fixture, so this really is a
        // placement decision and not the end of the list by accident.
        assert!(head.along + layout.block_width < layout.total_along);
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
        let layout = compute(&dag, &Plan::default().project(&dag), 100, Orientation::Horizontal);
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
        let span = 2 * layout.metrics.across_stride() - layout.metrics.gap_across;
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
        let layout = compute(&dag, &projection, 100, Orientation::Horizontal);
        let edge = layout
            .edges
            .iter()
            .find(|e| e.child == Oid::new("x"))
            .expect("an edge");
        assert_eq!(edge.parent, None);
        assert!(layout.route(edge).stub);
        assert!(
            layout.block(&Oid::new("x")).unwrap().along >= layout.metrics.gap_along,
            "the stub needs room to the left of the oldest block"
        );
    }

    /// `h` from a block lands on the commit immediately to its left, not on
    /// whatever else happens to be one column back in another track — and not
    /// on an arrow, which is no longer somewhere the cursor stops at all.
    #[test]
    fn moving_back_in_time_lands_on_the_commit_beside_it() {
        let (_, _, layout) = laid_out(120);
        // `f` is `main`'s tip; the block one column band to its left is `c`,
        // on another branch's row.  With arrows on the walk this took two
        // presses and the first one landed on nothing you could act on.
        let from = Focus::Commit(Oid::new("f"));
        assert_eq!(
            layout.step_where(&from, Dir::Left, |_| true),
            Some(Focus::Commit(Oid::new("c")))
        );
    }

    /// Arrows are not focusable: every focusable is a block or a label on one.
    /// Stopping on an arrow was a keypress that offered no choice — dragging
    /// it and dragging its block made the same edit.
    #[test]
    fn the_cursor_never_stops_on_an_arrow() {
        let (_, _, layout) = laid_out(120);
        assert!(!layout.edges.is_empty(), "the fixture has arrows to skip");
        let blocks = layout.blocks.len();
        let labels: usize = layout.blocks.iter().map(|b| b.labels.len()).sum();
        assert_eq!(layout.focusables.len(), blocks + labels);
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
        let along_of = |f: &Focus| layout.locate(f).unwrap().along as i32;
        let target = layout
            .step_where(&from, Dir::Down, |_| true)
            .expect("a track below");
        assert!(
            (along_of(&target) - along_of(&from)).abs() <= layout.along_stride() as i32,
            "crossing tracks travelled {} along the time axis",
            (along_of(&target) - along_of(&from)).abs()
        );
        // And it really did change track.
        let across_of = |f: &Focus| layout.locate(f).unwrap().across;
        assert!(across_of(&target) > across_of(&from));
    }

    /// A branch label sits on its block's top border, so `j` off the label
    /// lands on the commit it labels.
    #[test]
    fn a_branch_label_is_selectable_and_sits_above_its_commit() {
        let (_, _, layout) = laid_out(120);
        let label = layout.locate(&Focus::Ref("main".into())).expect("main is drawn");
        let block = layout.block(&Oid::new("f")).unwrap();
        assert_eq!(label.across, layout.track_across(block.track));
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
        let layout = compute(&dag, &plan.project(&dag), 120, Orientation::Horizontal);

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
        let layout = compute(&dag, &plan.project(&dag), 120, Orientation::Horizontal);
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
        let layout = compute(&dag, &projection, 120, Orientation::Horizontal);

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
        let layout = compute(&dag, &projection, 300, Orientation::Horizontal);
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
        let layout = compute(&dag, &projection, 80, Orientation::Horizontal);
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

    /// A branch whose name is longer than a block is wide keeps a label —
    /// shortened.  Dropping it left the branch unnamed on its own tip, which
    /// reads as the editor not knowing it exists.
    #[test]
    fn a_name_too_long_for_the_border_is_shortened_not_dropped() {
        let mut dag = dag();
        dag.refs = vec![branch("feat/a-branch-name-nobody-would-shorten", "f")];
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 200, Orientation::Horizontal);
        let block = layout.block(&Oid::new("f")).unwrap();
        let (name, col) = block.labels.first().expect("a label survives").clone();
        assert!(name.starts_with("feat/a-branch"), "{name}");
        assert!(name.ends_with('…'), "{name}");
        assert!(col + name.chars().count() as u16 + 2 <= layout.block_inner());
    }

    /// An empty repository must lay out without panicking — there is no HEAD
    /// commit, no track and no block.
    #[test]
    fn an_empty_repository_lays_out_to_nothing() {
        let dag = Dag::new(Vec::new(), Vec::new(), Head::default(), WorkTree::default(), false);
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 80, Orientation::Horizontal);
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
            // A merge whose second parent has blocks of its own track sitting
            // between it and the merge — the case that decides `cross_at`.
            ("a merge of a busy branch", merge_of_a_busy_branch()),
            // HEAD part-way along a chain: the block lands in the middle of a
            // track an arrow is already using.
            ("a branch ahead", ahead()),
        ] {
            let projection = Plan::default().project(&dag);
            for (width, orient) in [40, 80, 120, 400]
                .into_iter()
                .flat_map(|w| [(w, Orientation::Horizontal), (w, Orientation::Vertical)])
            {
                let layout = compute(&dag, &projection, width, orient);
                for edge in &layout.edges {
                    for (along, across) in layout.route(edge).cells() {
                        for block in &layout.blocks {
                            // The endpoints are meant to touch: an arrow
                            // leaves one border and its head sits in the gap
                            // beside the next.  Everything else is a crossing.
                            let start = layout.track_across(block.track);
                            let m = layout.metrics;
                            let inside = along > block.along
                                && along < block.along + m.block_along
                                && across >= start
                                && across < start + m.block_across;
                            assert!(
                                !inside,
                                "{name} at width {width} ({orient:?}): the arrow {} -> {:?} \
                                 runs through block {} at ({along}, {across})",
                                edge.child, edge.parent, block.id
                            );
                        }
                    }
                }
            }
        }
    }

    /// Turning the graph turns the keys with it: whichever way history runs
    /// is the way `h`/`l` or `j`/`k` travel it.  Anything else would be a
    /// second thing to remember for the same picture.
    #[test]
    fn the_keys_follow_whichever_way_history_runs() {
        let dag = dag();
        let projection = Plan::default().project(&dag);
        // `f` is `main`'s tip and the block one band back is `c`, on another
        // branch's track — the same pair `moving_back_in_time_lands_on_the_
        // commit_beside_it` pins for the horizontal picture.
        let from = Focus::Commit(Oid::new("f"));

        let across = compute(&dag, &projection, 120, Orientation::Horizontal);
        assert_eq!(
            across.step_where(&from, Dir::Left, |_| true),
            Some(Focus::Commit(Oid::new("c"))),
            "left goes back in time when history runs left to right"
        );

        let down = compute(&dag, &projection, 120, Orientation::Vertical);
        assert_eq!(
            down.step_where(&from, Dir::Down, |_| true),
            Some(Focus::Commit(Oid::new("c"))),
            "down goes back in time when history runs down the screen"
        );
        // …and the sideways keys are then the ones that change branch.
        let track = |f: &Focus| down.track_at(down.locate(f).unwrap().across);
        let sideways = [Dir::Right, Dir::Left]
            .into_iter()
            .filter_map(|dir| down.step_where(&from, dir, |_| true))
            .find(|f| track(f) != track(&from));
        assert!(sideways.is_some(), "no sideways key reached another branch");
    }

    /// Vertical draws the newest commit at the *top*, the order `git log`
    /// prints.  The graph's own time axis still runs oldest-first, so this is
    /// the one place the two disagree — and the only place that may.
    #[test]
    fn vertical_puts_the_newest_commit_at_the_top() {
        let dag = dag();
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 120, Orientation::Vertical);

        let screen_y = |id: &str| {
            let block = layout.block(&Oid::new(id)).expect(id);
            layout.block_origin(block).1
        };
        // `f` follows `e` follows `a`, so each is drawn above the one it
        // follows, and every block is a whole block clear of the next.
        assert!(screen_y("f") < screen_y("e"), "the newer commit is not above");
        assert!(screen_y("e") < screen_y("a"), "the newer commit is not above");
        assert!(screen_y("a") - screen_y("e") >= BLOCK_H, "blocks overlap");

        // A branch is a column, so two branches differ in x and not in y.
        let x = |id: &str| layout.block_origin(layout.block(&Oid::new(id)).expect(id)).0;
        assert_ne!(x("d"), x("f"), "two branches share a column");
    }

    /// The blocks themselves are the same box either way up — five rows, and
    /// as wide as what is written in them.  Only which way the *next* commit
    /// lies changes, which is what keeps every renderer in one piece.
    #[test]
    fn a_block_is_the_same_box_whichever_way_the_graph_runs() {
        let dag = dag();
        let projection = Plan::default().project(&dag);
        for orient in [Orientation::Horizontal, Orientation::Vertical] {
            let layout = compute(&dag, &projection, 120, orient);
            assert_eq!(layout.metrics.block_across.max(layout.metrics.block_along), layout.block_width);
            assert_eq!(layout.metrics.block_across.min(layout.metrics.block_along), BLOCK_H);
        }
    }

    /// A branch is a row.  The trunk used to hand its oldest commits to
    /// whichever topic branch git happened to list first, so `main` stepped
    /// sideways near the start for no reason a reader could see.
    #[test]
    fn a_first_parent_chain_keeps_one_track_all_the_way_along() {
        let dag = tangled();
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 200, Orientation::Horizontal);
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
        let layout = compute(&dag, &projection, 200, Orientation::Horizontal);
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
        let layout = compute(&dag, &projection, 200, Orientation::Horizontal);
        let tint = |id: &str| layout.block(&Oid::new(id)).expect(id).tint;

        // Two branches, so two colours and — since a branch is a row — two
        // rows.  Sharing one saved a line and cost the only thing on screen
        // that said which of the two you were looking at.
        assert_ne!(tint("top"), tint("mid"), "the branches are one colour");
        assert_eq!(tint("mid"), tint("base"), "everything at and before `main`");
        assert_ne!(
            layout.block(&Oid::new("top")).unwrap().track,
            layout.block(&Oid::new("base")).unwrap().track,
            "the two branches share a row"
        );
        assert_eq!(
            layout.block(&Oid::new("mid")).unwrap().track,
            layout.block(&Oid::new("base")).unwrap().track,
            "and everything on `main` is in one"
        );
        // Each row says whose it is, which is the whole point of giving a
        // branch one.
        assert_eq!(
            layout.lane_label(layout.block(&Oid::new("mid")).unwrap().track),
            Some("main")
        );
        assert_eq!(
            layout.lane_label(layout.block(&Oid::new("top")).unwrap().track),
            Some("test-branch")
        );

        // The labels match the commits they name, so a label and its run of
        // history read as one thing.
        assert_eq!(layout.branch_tints.get("test-branch"), Some(&tint("top")));
        assert_eq!(layout.branch_tints.get("main"), Some(&tint("mid")));
    }

    /// The mirror of [`commits_take_the_colour_of_the_branch_they_are_on`]: the
    /// stale label is the *topic* branch and the trunk is the one that moved
    /// on.  Every one of these commits was made on `main`, so drawing them
    /// under `topic`'s name — which is what the nearest-label walk did on its
    /// own — says the opposite of what happened.
    #[test]
    fn a_branch_left_behind_does_not_own_the_trunk_beneath_it() {
        let dag = Dag::new(
            vec![commit("top", &["mid"]), commit("mid", &["base"]), commit("base", &[])],
            vec![branch("main", "top"), branch("topic", "mid")],
            Head { branch: Some("main".into()), target: Some(Oid::new("top")) },
            WorkTree::default(),
            false,
        );
        let projection = Plan::default().project(&dag);
        let layout = compute(&dag, &projection, 200, Orientation::Horizontal);
        let tint = |id: &str| layout.block(&Oid::new(id)).expect(id).tint;

        assert_eq!(tint("top"), tint("mid"), "`topic` took the trunk it is on");
        assert_eq!(tint("mid"), tint("base"), "and the history below it");
        assert_eq!(tint("base"), layout.branch_tints["main"], "which is `main`'s");

        // Owning no commits, it is given no row: a branch name written over an
        // empty band claims a track's worth of screen and says nothing.
        assert_eq!(layout.track_count, 1, "`topic` was given a row of its own");
        for track in 0..layout.track_count {
            assert_ne!(layout.lane_label(track), Some("topic"));
        }
    }

    /// HEAD sits directly after the commit it names, in that commit's track —
    /// unless that track is carrying an arrow past it, in which case a block
    /// there would hide the arrow completely.
    #[test]
    fn head_steps_aside_rather_than_landing_on_an_arrow() {
        // At a branch tip there is nothing passing, so it stays put.
        let dag = dag();
        let layout = compute(&dag, &Plan::default().project(&dag), 200, Orientation::Horizontal);
        let head = layout.block(&Oid::new("HEAD")).expect("HEAD is drawn");
        assert_eq!(head.track, layout.block(&Oid::new("f")).unwrap().track);

        // Detached part-way along one branch's row, the arrow from the commit
        // after it owns that row, so HEAD takes one of its own and points
        // across.
        let dag = Dag::new(
            vec![commit("top", &["mid"]), commit("mid", &["base"]), commit("base", &[])],
            vec![branch("main", "top")],
            Head { branch: None, target: Some(Oid::new("mid")) },
            WorkTree::default(),
            false,
        );
        let layout = compute(&dag, &Plan::default().project(&dag), 200, Orientation::Horizontal);
        let head = layout.block(&Oid::new("HEAD")).expect("HEAD is drawn");
        let target = layout.block(&Oid::new("mid")).unwrap();
        assert_ne!(head.track, target.track, "HEAD is sitting on the arrow");
        assert!(head.along > target.along, "and still directly after its commit");
    }
}
