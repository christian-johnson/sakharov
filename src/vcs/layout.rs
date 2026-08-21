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
    pub fn step(&self, current: &Focus, dir: Dir) -> Option<Focus> {
        let from = self.locate(current)?;
        let (row, col) = (from.row as i32, from.col as i32);

        self.focusables
            .iter()
            .filter(|f| f.focus != *current)
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
    let lanes = assign_lanes(&order, dag, projection);
    let lane_count = lanes.values().copied().max().map_or(1, |m| m + 1);
    let lane_width = lane_width(width, lane_count);

    let mut blocks = Vec::new();
    let mut row = 0u16;
    let inner = lane_width.saturating_sub(2);

    // HEAD is drawn as a block rather than as one more label because it is a
    // different kind of thing from a branch — it is where *you* are — and it
    // is the one pointer always worth finding at a glance.
    //
    // It sits **directly above the commit it names**, in that commit's lane,
    // rather than at the top of the graph.  At the top its arrow has to span
    // however far down the graph HEAD happens to be, and a screenful of `│`
    // between a block and its target says nothing.  Here the arrow is one row
    // and reads as "you are here".
    let head_before = dag.head.target.clone().filter(|id| lanes.contains_key(id));
    let mut head_placed = false;
    let push_head = |blocks: &mut Vec<Block>, row: &mut u16, lane: usize| {
        blocks.push(Block {
            id: Oid::new("HEAD"),
            kind: BlockKind::Head,
            lane,
            row: *row,
            height: HEAD_H,
            labels: Vec::new(),
        });
        *row += HEAD_H + GAP;
    };

    // A repository with no commits yet still has a HEAD worth showing: it says
    // which branch the first commit will be on.
    if head_before.is_none() && (dag.head.target.is_some() || dag.head.branch.is_some()) {
        push_head(&mut blocks, &mut row, 0);
        head_placed = true;
    }

    for id in &order {
        let lane = lanes.get(id).copied().unwrap_or(0);
        if !head_placed && head_before.as_ref() == Some(id) {
            push_head(&mut blocks, &mut row, lane);
            head_placed = true;
        }
        let kind = if id.is_pending() { BlockKind::Pending } else { BlockKind::Commit };
        blocks.push(Block {
            id: id.clone(),
            kind,
            lane,
            row,
            height: BLOCK_H,
            labels: labels_for(dag, projection, id, inner),
        });
        row += BLOCK_H + GAP;
    }

    let index: HashMap<Oid, usize> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.clone(), i))
        .collect();
    let total_rows = row.saturating_sub(GAP);

    let edges = build_edges(&blocks, &index, dag, projection, total_rows);
    let mut layout = Layout {
        focusables: Vec::new(),
        blocks,
        edges,
        lane_count,
        lane_width,
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

/// Give each commit a lane.
///
/// A lane is *reserved* for a commit by its child, so a chain keeps one
/// column and stays readable.  The first parent inherits the child's lane;
/// further parents (a merge) open new ones, which is what makes a merge read
/// as two columns coming together.
fn assign_lanes(order: &[Oid], dag: &Dag, projection: &Projection) -> HashMap<Oid, usize> {
    let mut assigned: HashMap<Oid, usize> = HashMap::new();
    // Each slot holds the commit that lane is being held open for.
    let mut lanes: Vec<Option<Oid>> = Vec::new();

    for id in order {
        let lane = match lanes.iter().position(|slot| slot.as_ref() == Some(id)) {
            Some(lane) => lane,
            None => free_lane(&mut lanes),
        };
        assigned.insert(id.clone(), lane);
        lanes[lane] = None;

        let parents = projection.parents(dag, id);
        // The first parent continues this lane, unless something else already
        // holds a lane for it — in which case the two chains converge and this
        // lane is simply released.
        if let Some(first) = parents.first() {
            let held = lanes.iter().any(|slot| slot.as_ref() == Some(first));
            if !held {
                lanes[lane] = Some(first.clone());
            }
        }
        for parent in parents.iter().skip(1) {
            if !lanes.iter().any(|slot| slot.as_ref() == Some(parent)) {
                let slot = free_lane(&mut lanes);
                lanes[slot] = Some(parent.clone());
            }
        }
    }
    assigned
}

/// Leftmost unused lane, growing the set when they are all taken.
fn free_lane(lanes: &mut Vec<Option<Oid>>) -> usize {
    match lanes.iter().position(Option::is_none) {
        Some(lane) => lane,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

/// How wide one lane is.
///
/// Divided between the lanes in play and then clamped: a single-branch
/// repository would otherwise draw one block across the whole terminal, and a
/// repository with nine branches would draw nine unreadable slivers (those
/// scroll horizontally instead).
fn lane_width(width: u16, lane_count: usize) -> u16 {
    let count = lane_count.max(1) as u16;
    let each = (width + LANE_GAP) / count;
    each.saturating_sub(LANE_GAP).clamp(MIN_LANE, MAX_LANE)
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
            });
            continue;
        }
        for (slot, parent) in projection.parents(dag, &block.id).iter().enumerate() {
            let target = index.get(parent).map(|&i| &blocks[i]);
            edges.push(Edge {
                child: block.id.clone(),
                slot,
                // A parent outside the drawn set is left as `None` so the
                // renderer draws a stub rather than an arrow to nowhere.
                parent: target.map(|b| b.id.clone()),
                from_lane: block.lane,
                to_lane: target.map_or(block.lane, |b| b.lane),
                row: block.row + block.height,
                end_row: target.map_or(total_rows, |b| b.row),
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
            layout.step(&from, Dir::Down),
            Some(Focus::Edge { child: Oid::new("d"), slot: 0 })
        );
    }

    #[test]
    fn moving_is_reversible_and_stops_at_the_edges() {
        let (_, _, layout) = laid_out(120);
        let start = Focus::Commit(Oid::new("d"));
        let down = layout.step(&start, Dir::Down).unwrap();
        assert_eq!(layout.step(&down, Dir::Up), Some(start));

        // The graph has ends: nothing above the first focusable, nothing
        // below the last.  (HEAD is no longer either — it sits beside its own
        // commit, wherever in the graph that is.)
        let first = layout.focusables.first().unwrap().focus.clone();
        let last = layout.focusables.last().unwrap().focus.clone();
        assert_eq!(layout.step(&first, Dir::Up), None);
        assert_eq!(layout.step(&last, Dir::Down), None);
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
        let target = layout.step(&from, Dir::Right).expect("a lane to the right");
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
            layout.step(&Focus::Ref("main".into()), Dir::Down),
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
        assert_eq!(lane_width(300, 1), MAX_LANE);
        assert_eq!(lane_width(40, 9), MIN_LANE);
        // In between it divides the space up.
        let mid = lane_width(120, 2);
        assert!((MIN_LANE..=MAX_LANE).contains(&mid), "{mid}");
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
}
