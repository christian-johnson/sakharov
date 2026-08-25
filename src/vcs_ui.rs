//! Renderer for the version-control graph.
//!
//! Draws from a [`vcs::layout::Layout`] and nothing else.  The navigation in
//! `vcs::state` walks the same `Layout`, so what is under the cursor and what
//! is on screen cannot disagree — the same discipline `table_ui` has with
//! `table::layout`, and for the same reason.
//!
//! Everything is drawn into a cell buffer directly rather than through
//! ratatui widgets: blocks overlap arrows, arrows cross tracks, and the whole
//! picture is a coordinate grid rather than a stack of rectangles.
//!
//! ## Which way up
//!
//! The graph is drawn either way round (`vcs::layout::Orientation`), and this
//! module contains no second copy of anything to do it.  Two things differ,
//! and they are the two things that are genuinely about the screen rather than
//! about the graph:
//!
//! * **Where a graph cell lands** — [`Painter::at`] asks the layout
//!   (`Layout::screen`), which is also what the navigation walks, so the two
//!   cannot disagree about what is under the cursor.
//! * **Which glyph a stroke is** — a run along the time axis is `─` in one
//!   picture and `│` in the other ([`Strokes`]), and a corner or an arrowhead
//!   is worked out from the *screen* directions its arms leave on
//!   ([`corner`], [`arrowhead`]) rather than from a rule written down twice.
//!
//! A block is the same box either way: `block_width` by [`BLOCK_H`].  So
//! everything inside one is drawn in ordinary screen coordinates from the
//! block's origin, and none of it knows the orientation exists.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    Frame,
};

use crate::{
    theme,
    render_util::wrap_segments,
    vcs::{
        layout::{Block, BlockKind, Edge, Focus, Layout, Orientation, BLOCK_H},
        relative_time,
        state::VcsState,
        Dag, Oid, RefKind,
    },
};

/// Box-drawing pieces.  A focused block gets the heavy set, the way a focused
/// notebook cell does — one visual language for "this is the thing you are on".
struct BoxChars {
    tl: char,
    tr: char,
    bl: char,
    br: char,
    h: char,
    v: char,
}

const LIGHT: BoxChars = BoxChars { tl: '╭', tr: '╮', bl: '╰', br: '╯', h: '─', v: '│' };
const HEAVY: BoxChars = BoxChars { tl: '┏', tr: '┓', bl: '┗', br: '┛', h: '━', v: '┃' };

/// Text rows a block gives the commit summary.  The block is five rows tall:
/// two borders, these, and the metadata row.
const SUMMARY_ROWS: usize = 2;

/// Where the cursor is and what it is holding.
///
/// One value rather than two parameters threaded through every draw call:
/// which of the two applies is a single decision ([`border_style`]) that every
/// drawn thing makes the same way.
struct Cursor {
    focus: Option<Focus>,
    grabbed: Option<Focus>,
}

impl Cursor {
    /// How `this` should be drawn, if the cursor bears on it at all.
    ///
    /// Grab wins over focus: while something is held, the held thing is the
    /// more important fact on screen, and the cursor has moved on to hover
    /// somewhere else.
    fn mark(&self, this: &Focus) -> Option<Mark> {
        if self.grabbed.as_ref() == Some(this) {
            return Some(Mark::Grabbed);
        }
        if self.focus.as_ref() == Some(this) {
            return Some(Mark::Focused);
        }
        None
    }

    /// `base`, marked up for wherever the cursor is.
    ///
    /// The focused thing keeps its own colour and is drawn **bold**, with a
    /// heavy border or a heavy arrow.  It used to be recoloured, which meant
    /// moving the cursor over a green branch turned it the accent colour —
    /// erasing the one fact the colours are there to carry (which branch this
    /// is), and vanishing entirely on whichever lane already had that hue.
    ///
    /// Being *held* is different: that is a state the object is in, not a
    /// place the cursor happens to be, and it has to be unmistakable from
    /// across the screen.  So it does recolour.
    fn style(&self, this: &Focus, base: Color) -> Style {
        match self.mark(this) {
            Some(Mark::Grabbed) => Style::default()
                .fg(theme::active().vcs_grabbed)
                .add_modifier(Modifier::BOLD),
            Some(Mark::Focused) => Style::default().fg(base).add_modifier(Modifier::BOLD),
            None => Style::default().fg(base),
        }
    }
}

/// What the cursor is doing to a thing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Focused,
    Grabbed,
}

/// Writes cells in *stack* coordinates, clipped to the viewport.
///
/// Every write in this module goes through here, so nothing can be drawn
/// outside `area` and nothing has to check the scroll for itself — which
/// matters more than usual in a view that paints cells directly rather than
/// rendering widgets into rectangles.
struct Painter<'a, 'b, 'c> {
    frame: &'a mut Frame<'b>,
    /// Where the graph itself is drawn.
    area: Rect,
    /// The row branch names are pinned to, when they are pinned to one at all
    /// (vertical: one row above the graph, naming every track at once).
    names: Option<Rect>,
    layout: &'c Layout,
    scroll_x: u16,
    scroll_y: u16,
}

impl Painter<'_, '_, '_> {
    /// One cell in screen coordinates, or nothing if it is scrolled off or
    /// past the edge.
    fn put(&mut self, x: u16, y: u16, ch: char, style: Style) {
        let Some(x) = x.checked_sub(self.scroll_x) else { return };
        let Some(y) = y.checked_sub(self.scroll_y) else { return };
        if y >= self.area.height || x >= self.area.width {
            return;
        }
        self.frame.buffer_mut()[(self.area.x + x, self.area.y + y)]
            .set_char(ch)
            .set_style(style);
    }

    /// One cell in *graph* coordinates — the only place the orientation is
    /// consulted, and it is consulted by asking the layout.
    ///
    /// Where two arrows meet, their strokes are **merged into a junction**
    /// rather than one overwriting the other.  Several children of one commit
    /// converge on the same crossing line, and whichever turned there last
    /// used to stamp a corner into the middle of another arrow's straight run
    /// — a `╰` with line above and below it, which reads as one line ending
    /// and an unrelated one starting.
    fn at(&mut self, along: u16, across: u16, ch: char, style: Style) {
        let (x, y) = self.layout.screen(along, across);
        let ch = match (line_strokes(ch), self.read(x, y).and_then(line_strokes)) {
            (Some((new, heavy)), Some((old, was_heavy))) => {
                let mut merged = [false; 4];
                for (i, side) in merged.iter_mut().enumerate() {
                    *side = new[i] || old[i];
                }
                line_glyph(merged, heavy || was_heavy).unwrap_or(ch)
            }
            _ => ch,
        };
        self.put(x, y, ch, style);
    }

    /// A cell that replaces whatever is there, junctions included.
    ///
    /// An arrowhead is where a line *stops*; merging it with a run that
    /// happens to pass through would turn the one glyph carrying "this is the
    /// end, and this is what it points at" into a piece of plumbing.
    fn put_over(&mut self, along: u16, across: u16, ch: char, style: Style) {
        let (x, y) = self.layout.screen(along, across);
        self.put(x, y, ch, style);
    }

    /// What is already drawn at a screen cell, if it is on screen at all.
    fn read(&mut self, x: u16, y: u16) -> Option<char> {
        let x = x.checked_sub(self.scroll_x)?;
        let y = y.checked_sub(self.scroll_y)?;
        if y >= self.area.height || x >= self.area.width {
            return None;
        }
        self.frame.buffer_mut()[(self.area.x + x, self.area.y + y)]
            .symbol()
            .chars()
            .next()
    }

    /// `text`, truncated to `max` columns, in screen coordinates.
    fn text(&mut self, x: u16, y: u16, text: &str, style: Style, max: u16) {
        for (i, ch) in text.chars().take(max as usize).enumerate() {
            self.put(x + i as u16, y, ch, style);
        }
    }

    /// A track's branch name, written where that picture keeps them.
    ///
    /// Horizontal writes it in the band above the track, at the viewport's own
    /// left edge — scrolled with the graph across tracks, never along time.
    /// Vertical writes it in the reserved row above the whole graph, over its
    /// track's columns, scrolled the same way.  Either way it is pinned along
    /// the time axis, because "which branch am I looking at" is at its
    /// sharpest a hundred commits along — exactly where a name written in
    /// graph coordinates has scrolled off.
    fn lane_name(&mut self, track: usize, text: &str, style: Style) {
        let across = self.layout.label_across(track);
        match self.names {
            // Vertical: the reserved row, at the track's own columns.
            Some(row) => {
                let Some(x) = across.checked_sub(self.scroll_x) else { return };
                for (i, ch) in text.chars().enumerate() {
                    let x = x + i as u16;
                    if x >= row.width {
                        return;
                    }
                    self.frame.buffer_mut()[(row.x + x, row.y)]
                        .set_char(ch)
                        .set_style(style);
                }
            }
            // Horizontal: the band above the track, at the left edge.
            None => {
                let Some(y) = across.checked_sub(self.scroll_y) else { return };
                if y >= self.area.height {
                    return;
                }
                for (i, ch) in text.chars().enumerate() {
                    let x = i as u16;
                    if x >= self.area.width {
                        return;
                    }
                    self.frame.buffer_mut()[(self.area.x + x, self.area.y + y)]
                        .set_char(ch)
                        .set_style(style);
                }
            }
        }
    }
}

/// Draw the graph for `state` into `area`.
pub fn render(frame: &mut Frame, area: Rect, state: &VcsState) {
    let th = theme::active();
    let layout = state.layout(area.width);
    // Vertical names every track at once in a row above the graph, so that row
    // comes off the top before anything is laid against it.  Horizontal gives
    // each track a name band of its own inside the stack (`label_across`), so
    // there is nothing to reserve.
    let vertical = layout.metrics.orient == Orientation::Vertical;
    let names = (vertical && area.height > 1).then_some(Rect { height: 1, ..area });
    let graph = match names {
        Some(row) => Rect {
            y: area.y + row.height,
            height: area.height - row.height,
            ..area
        },
        None => area,
    };
    let (scroll_x, scroll_y) = layout.screen_scroll(state.scroll_along, state.scroll_track);
    let mut p = Painter { frame, area: graph, names, layout: &layout, scroll_x, scroll_y };

    if state.dag.is_empty() && state.dag.head.branch.is_none() {
        p.scroll_x = 0;
        p.scroll_y = 0;
        p.text(
            2,
            0,
            "no commits yet — this repository has no history to show",
            Style::default().fg(th.dim),
            area.width,
        );
        return;
    }

    let cursor = Cursor { focus: state.focus.clone(), grabbed: state.grabbed.clone() };

    // Arrows first: a block drawn over an arrow reads as the arrow passing
    // behind it, which is what a line entering the top of a box should look
    // like.  The other order puts line segments across the text.
    //
    // Runs before turns, across *all* the arrows: a merge's second link runs
    // back along the same row its target's own chain runs along, so whichever
    // was drawn second erased the other's corner and arrowhead — the two cells
    // that carry every bit of the information (which way it turns, and where
    // it ends).  Two passes cost one more walk of a list that is already laid
    // out, and no arrow can rub out another's ends.
    for edge in &layout.edges {
        draw_edge_runs(&mut p, &layout, edge, &cursor);
    }
    for edge in &layout.edges {
        draw_edge_turns(&mut p, &layout, edge, &cursor);
    }
    for block in &layout.blocks {
        draw_block(&mut p, state, &layout, block, &cursor);
    }
    draw_horizon(&mut p, state, &layout);
    // Last, so a track's name wins over an arrow that happens to cross the
    // cells it is written in.
    draw_lane_names(&mut p, &layout);
}

/// Write each row's branch name above it.
///
/// A track *is* a branch (see `vcs::layout::place`), and without the name the
/// only place a branch is written is the label on its tip — which on a long
/// history is a screenful to the right of the commits that are on it.  That
/// was the whole complaint: a feature branch's commits with nothing anywhere
/// on screen saying they were the feature branch's.
fn draw_lane_names(p: &mut Painter, layout: &Layout) {
    let th = theme::active();
    // The tail points *into* the band the name belongs to: sideways at a row,
    // downwards at a column.
    let tail = match layout.metrics.orient {
        Orientation::Horizontal => '╾',
        Orientation::Vertical => '╽',
    };
    for track in 0..layout.track_count {
        let Some(name) = layout.lane_label(track) else { continue };
        let colour = layout
            .branch_tints
            .get(name)
            .map_or(th.vcs_branch, |&t| th.vcs_tint(t));
        p.lane_name(
            track,
            &format!("{tail} {name} "),
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        );
    }
}

/// Say so when the walk stopped at the commit limit.
///
/// Without it the oldest block on screen looks like the repository's first
/// commit, and its missing parent arrow looks like a root — a picture that is
/// simply false about older history.  Written in the gap just below the oldest
/// block, because the cells before it are the few the stub arrow needs.
fn draw_horizon(p: &mut Painter, state: &VcsState, layout: &Layout) {
    if !state.dag.truncated {
        return;
    }
    let Some(oldest) = state.dag.commits().last() else { return };
    let Some(block) = layout.block(&oldest.id) else { return };
    let (left, top) = layout.block_origin(block);
    p.text(
        left,
        top + BLOCK_H,
        "⋯ older history not loaded",
        Style::default().fg(theme::active().dim),
        layout.block_width,
    );
}

/// The colour a block's border is drawn in, and whether it is heavy.
///
/// A cursor-bearing block gets the heavy box, the way a focused notebook cell
/// does — one visual language across the editor for "this is the thing you
/// are on".
fn border_style(block: &Block, cursor: &Cursor) -> (Style, bool) {
    let th = theme::active();
    let this = match block.kind {
        BlockKind::Head => Focus::Head,
        _ => Focus::Commit(block.id.clone()),
    };
    let base = match block.kind {
        BlockKind::Head => th.vcs_head,
        BlockKind::Pending => th.vcs_pending,
        // By branch, not by track: a branch that is merely *ahead* of another
        // shares its row, correctly, and colouring by row then painted the
        // whole history one colour.
        BlockKind::Commit => th.vcs_tint(block.tint),
    };
    (cursor.style(&this, base), cursor.mark(&this).is_some())
}

/// A block is the same box in either picture, so it — and everything written
/// inside it — is drawn in plain screen coordinates from
/// `Layout::block_origin`.  Only where that origin *is* depends on the
/// orientation.
fn draw_block(p: &mut Painter, state: &VcsState, layout: &Layout, block: &Block, cursor: &Cursor) {
    let (style, heavy) = border_style(block, cursor);
    let ch = if heavy { &HEAVY } else { &LIGHT };
    let (left, top) = layout.block_origin(block);
    let right = left + layout.block_width - 1;
    let bottom = top + BLOCK_H - 1;

    // --- frame ---
    for x in left..=right {
        p.put(x, top, ch.h, style);
        p.put(x, bottom, ch.h, style);
    }
    for y in top + 1..bottom {
        p.put(left, y, ch.v, style);
        p.put(right, y, ch.v, style);
    }
    p.put(left, top, ch.tl, style);
    p.put(right, top, ch.tr, style);
    p.put(left, bottom, ch.bl, style);
    p.put(right, bottom, ch.br, style);

    match block.kind {
        BlockKind::Head => draw_head_contents(p, state, layout, block),
        _ => draw_commit_contents(p, state, layout, block, cursor),
    }
}

/// HEAD: which branch you are on, and what is uncommitted.
///
/// The work tree lives here rather than on any commit because that is what it
/// is — the changes sitting on top of wherever HEAD points, on their way to
/// becoming the next block to the right.
fn draw_head_contents(p: &mut Painter, state: &VcsState, layout: &Layout, block: &Block) {
    let th = theme::active();
    let ((left, top), inner) = (layout.block_origin(block), layout.block_inner());
    let bold = Style::default().fg(th.vcs_head).add_modifier(Modifier::BOLD);
    p.text(left + 2, top, " HEAD ", bold, inner);

    let head = &state.dag.head;
    let where_ = match (&head.branch, &head.target) {
        (Some(branch), _) => format!("● {branch}"),
        (None, Some(oid)) => format!("● detached at {}", oid.short()),
        (None, None) => "● no commits yet".to_string(),
    };
    p.text(left + 2, top + 1, &where_, bold, inner.saturating_sub(1));

    // Two rows, because they answer two different questions: what would go
    // into the next commit, and what is lying around the tree that git is not
    // tracking at all.  The second is the one that quietly accumulates.
    //
    // The text comes from `WorkTree::summary_lines`, which is also what the
    // layout sized this block against — built in one place so a block sized
    // from one string and filled with another cannot clip the half that
    // matters.
    let work = &state.dag.work;
    let [tracked, stray] = work.summary_lines();
    let colour = if work.conflicted() > 0 {
        th.error
    } else if work.staged() > 0 {
        th.git_added
    } else if work.unstaged() > 0 {
        th.git_modified
    } else {
        th.dim
    };
    p.text(left + 2, top + 2, &tracked, Style::default().fg(colour), inner.saturating_sub(1));
    p.text(
        left + 2,
        top + 3,
        &stray,
        Style::default().fg(th.dim),
        inner.saturating_sub(1),
    );
}

/// A commit: hash and refs on the top border, the summary over two rows, then
/// author and age, with the line counts along the bottom border.
///
/// The counts sit on the border rather than on a row of their own because a
/// block is only five rows tall and the border is otherwise empty — and
/// because `+293 -45` is a shape you read without reading, so it does not need
/// to be in the text.
fn draw_commit_contents(
    p: &mut Painter,
    state: &VcsState,
    layout: &Layout,
    block: &Block,
    cursor: &Cursor,
) {
    let th = theme::active();
    let ((left, top), inner) = (layout.block_origin(block), layout.block_inner());
    let pending = state
        .plan
        .project(&state.dag)
        .pending()
        .iter()
        .find(|p| p.id == block.id)
        .cloned();
    let commit = state.dag.get(&block.id);

    // --- hash, on the top border ---
    let hash = format!(" {} ", block.id.short());
    let hash_style = match block.kind {
        BlockKind::Pending => Style::default().fg(th.vcs_pending).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(th.vcs_hash).add_modifier(Modifier::BOLD),
    };
    p.text(left + 1, top, &hash, hash_style, inner);

    // --- ref labels, further along the same border ---
    for (name, col) in &block.labels {
        let kind = state.dag.find_ref(name).map(|r| r.kind);
        let colour = match kind {
            Some(RefKind::Remote) => th.vcs_remote,
            Some(RefKind::Tag) => th.vcs_tag,
            // A local branch label is drawn the colour of the commits that are
            // on it, so the label and its run of history read as one thing.
            _ => layout
                .branch_tints
                .get(name)
                .map_or(th.vcs_branch, |&t| th.vcs_tint(t)),
        };
        // A label is small and sits on a border, so the cursor reverses it —
        // bold alone would be lost against a border that is already bold.
        // Reversing keeps the label's own colour, which is the point.
        let this = Focus::Ref(name.clone());
        let mut style = cursor.style(&this, colour).add_modifier(Modifier::BOLD);
        if cursor.mark(&this).is_some() {
            style = style.add_modifier(Modifier::REVERSED);
        }
        p.text(left + col, top, &format!(" {name} "), style, inner);
    }

    // --- summary, wrapped over the two text rows ---
    let summary = pending
        .as_ref()
        .map(|p| p.summary.clone())
        .or_else(|| commit.map(|c| c.summary.clone()))
        .unwrap_or_else(|| "(not loaded)".to_string());
    let text_width = inner.saturating_sub(1) as usize;
    let segments = wrap_segments(&summary, text_width);
    for (row, (_, segment)) in segments.iter().take(SUMMARY_ROWS).enumerate() {
        // The last row it fits on says so when there is more, rather than
        // stopping mid-sentence and leaving the reader to wonder.
        let last = row + 1 == SUMMARY_ROWS && segments.len() > SUMMARY_ROWS;
        let mut line = (*segment).to_string();
        if last {
            line.truncate(
                line.char_indices()
                    .nth(text_width.saturating_sub(1))
                    .map_or(line.len(), |(i, _)| i),
            );
            line.push('…');
        }
        p.text(
            left + 2,
            top + 1 + row as u16,
            line.trim_end(),
            Style::default(),
            text_width as u16,
        );
    }

    // --- author · age ---
    let Some(commit) = commit else {
        p.text(
            left + 2,
            top + 1 + SUMMARY_ROWS as u16,
            "created by :vc-apply",
            Style::default().fg(th.vcs_pending),
            inner.saturating_sub(1),
        );
        return;
    };
    let meta = format!("{} · {}", commit.author, relative_time(commit.when, state.now));
    p.text(
        left + 2,
        top + 1 + SUMMARY_ROWS as u16,
        &meta,
        Style::default().fg(th.dim),
        inner.saturating_sub(1),
    );

    // --- change counts, along the bottom border ---
    let plus = format!(" +{} ", commit.insertions);
    let minus = format!("-{} ", commit.deletions);
    let width = plus.chars().count() as u16 + minus.chars().count() as u16;
    let col = left + 1 + inner.saturating_sub(width);
    let bottom = top + BLOCK_H - 1;
    p.text(col, bottom, &plus, Style::default().fg(th.git_added), width);
    p.text(
        col + plus.chars().count() as u16,
        bottom,
        &minus,
        Style::default().fg(th.error),
        width,
    );
}

/// How one arrow is drawn.
///
/// An arrow belongs to the commit it leaves, so it takes that commit's colour
/// and a branch reads as one colour from tip to base.  HEAD's arrow is not a
/// parent link and keeps the neutral edge colour.  It is drawn heavy when its
/// commit is the one under the cursor, so grabbing a block visibly takes its
/// link along — the arrow itself is never a cursor target.
fn edge_style(layout: &Layout, edge: &Edge, cursor: &Cursor) -> (Style, bool) {
    let th = theme::active();
    let base = match layout.block(&edge.child) {
        Some(block) if block.kind != BlockKind::Head => th.vcs_tint(block.tint),
        _ => th.vcs_edge,
    };
    let this = Focus::Commit(edge.child.clone());
    (cursor.style(&this, base), cursor.mark(&this).is_some())
}

/// Which sides of a cell a box-drawing glyph has a stroke on: `[N, S, E, W]`.
///
/// `None` for anything that is not a line — a block's border is drawn *after*
/// the arrows and simply covers them, and an arrowhead terminates a line
/// rather than continuing it.
fn line_strokes(ch: char) -> Option<([bool; 4], bool)> {
    let (sides, heavy) = match ch {
        '─' => ([false, false, true, true], false),
        '━' => ([false, false, true, true], true),
        '│' => ([true, true, false, false], false),
        '┃' => ([true, true, false, false], true),
        '╭' => ([false, true, true, false], false),
        '┏' => ([false, true, true, false], true),
        '╮' => ([false, true, false, true], false),
        '┓' => ([false, true, false, true], true),
        '╰' => ([true, false, true, false], false),
        '┗' => ([true, false, true, false], true),
        '╯' => ([true, false, false, true], false),
        '┛' => ([true, false, false, true], true),
        '├' => ([true, true, true, false], false),
        '┣' => ([true, true, true, false], true),
        '┤' => ([true, true, false, true], false),
        '┫' => ([true, true, false, true], true),
        '┬' => ([false, true, true, true], false),
        '┳' => ([false, true, true, true], true),
        '┴' => ([true, false, true, true], false),
        '┻' => ([true, false, true, true], true),
        '┼' => ([true, true, true, true], false),
        '╋' => ([true, true, true, true], true),
        _ => return None,
    };
    Some((sides, heavy))
}

/// The glyph with strokes on exactly these sides.
fn line_glyph(sides: [bool; 4], heavy: bool) -> Option<char> {
    let light = match sides {
        [false, false, true, true] => '─',
        [true, true, false, false] => '│',
        [false, true, true, false] => '╭',
        [false, true, false, true] => '╮',
        [true, false, true, false] => '╰',
        [true, false, false, true] => '╯',
        [true, true, true, false] => '├',
        [true, true, false, true] => '┤',
        [false, true, true, true] => '┬',
        [true, false, true, true] => '┴',
        [true, true, true, true] => '┼',
        _ => return None,
    };
    Some(if heavy {
        match light {
            '─' => '━',
            '│' => '┃',
            '╭' => '┏',
            '╮' => '┓',
            '╰' => '┗',
            '╯' => '┛',
            '├' => '┣',
            '┤' => '┫',
            '┬' => '┳',
            '┴' => '┻',
            _ => '╋',
        }
    } else {
        light
    })
}

/// The strokes an arrow is built from, in the weight it is drawn.
///
/// A run *along time* is a horizontal line in one picture and a vertical one
/// in the other, and the crossing stroke is whichever the run is not.  Naming
/// them by the axis they travel rather than by their shape is what lets
/// [`draw_edge_runs`] be written once.
struct Strokes {
    along: char,
    across: char,
    stub: char,
}

fn strokes(orient: Orientation, heavy: bool) -> Strokes {
    let (h, v) = if heavy { ('━', '┃') } else { ('─', '│') };
    match orient {
        Orientation::Horizontal => Strokes { along: h, across: v, stub: '╌' },
        Orientation::Vertical => Strokes { along: v, across: h, stub: '┆' },
    }
}

/// Which side of a cell a neighbour lies on, on screen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    N,
    S,
    E,
    W,
}

/// The side of `from` that `to` lies on, both given as graph cells.
///
/// Going through the layout's own mapping rather than reasoning about the
/// orientation here: a corner that faces the wrong way draws a line which
/// appears to come from nowhere, and that is exactly the mistake a second
/// statement of the rule invites.
fn side(layout: &Layout, from: (u16, u16), to: (u16, u16)) -> Side {
    let (fx, fy) = layout.screen(from.0, from.1);
    let (tx, ty) = layout.screen(to.0, to.1);
    if tx > fx {
        Side::E
    } else if tx < fx {
        Side::W
    } else if ty > fy {
        Side::S
    } else {
        Side::N
    }
}

/// The corner glyph joining two arms leaving on sides `a` and `b`.
///
/// Stated in absolute screen directions, so it holds for a graph turned any
/// way up.  The weight has to match the strokes it joins: a light rounded
/// corner in the middle of a focused arrow's heavy run leaves a visible notch
/// exactly where a corner exists to prevent one.
fn corner(a: Side, b: Side, heavy: bool) -> char {
    let has = |s: Side| a == s || b == s;
    match (has(Side::E), has(Side::S), heavy) {
        (true, true, false) => '╭',
        (true, true, true) => '┏',
        (true, false, false) => '╰',
        (true, false, true) => '┗',
        (false, true, false) => '╮',
        (false, true, true) => '┓',
        (false, false, false) => '╯',
        (false, false, true) => '┛',
    }
}

/// The arrowhead for a link whose parent lies on `side` of the head cell.
///
/// Stated as "which way the commit it names is", not "which way the line was
/// travelling".  The two agree whenever the last stretch runs into the parent,
/// which is every arrow in the horizontal picture — and they part company in
/// the vertical one, where an arrow that changes track at its very last cell
/// arrives sideways and would otherwise point along the row instead of at the
/// block directly below it.
fn arrowhead(side: Side) -> char {
    match side {
        Side::E => '▶',
        Side::W => '◀',
        Side::S => '▼',
        Side::N => '▲',
    }
}

/// The straight runs of one parent link.
///
/// Routed as a run back through the child's track, a hop across to the
/// parent's track, and a run into the parent's near border, where the
/// arrowhead goes.
fn draw_edge_runs(p: &mut Painter, layout: &Layout, edge: &Edge, cursor: &Cursor) {
    let (style, heavy) = edge_style(layout, edge, cursor);
    let st = strokes(layout.metrics.orient, heavy);
    let r = layout.route(edge);

    if r.stub {
        // Past the horizon: a short stub that visibly goes nowhere, rather
        // than an arrow into empty space.
        for (along, across) in r.cells() {
            p.at(along, across, st.stub, style);
        }
        return;
    }

    // Where both ends are in one track the arrow is a single straight run,
    // stated as such rather than as three pieces that happen to line up: the
    // crossing stroke laid over the middle of it would be a `│` across a `─`.
    if r.from_across == r.to_across {
        for along in r.head..=r.start {
            p.at(along, r.from_across, st.along, style);
        }
        return;
    }

    // Back through the child's track to the crossing point…
    for along in r.cross + 1..=r.start {
        p.at(along, r.from_across, st.along, style);
    }
    // …across, *between* the two turns: the cells they sit on are theirs, and
    // a stroke laid there first merges into the corner as an arm pointing at
    // nothing.
    let (lo, hi) = (
        r.from_across.min(r.to_across),
        r.from_across.max(r.to_across),
    );
    for across in lo + 1..hi {
        p.at(r.cross, across, st.across, style);
    }
    // …and on through the parent's track to the arrowhead.
    for along in r.head..r.cross {
        p.at(along, r.to_across, st.along, style);
    }
}

/// The corners and the arrowhead of one parent link.
///
/// The arrowhead is at the *parent* end because that is the direction the link
/// points: a commit names its parent, never the reverse, and drawing it the
/// other way would teach the graph backwards.
fn draw_edge_turns(p: &mut Painter, layout: &Layout, edge: &Edge, cursor: &Cursor) {
    let (style, heavy) = edge_style(layout, edge, cursor);
    let r = layout.route(edge);
    if r.stub {
        return;
    }

    let leaving = (r.cross, r.from_across);
    let arriving = (r.cross, r.to_across);
    if r.from_across != r.to_across {
        // Corners, so the run reads as one line rather than three.  The turn
        // always gets one, even when the arrow crosses at its very first cell:
        // the stroke it turns out of is the block sitting directly beside it,
        // and a bare stroke there reads as a line from nowhere.
        // The arms are named by the *neighbouring cell* in each direction,
        // never by the far end of the run: a merge's second parent crosses at
        // the arrow's very first cell, so the far end is the corner itself and
        // asking which side it lies on has no answer.
        p.at(
            leaving.0,
            leaving.1,
            corner(
                side(layout, leaving, (r.cross + 1, r.from_across)),
                side(layout, leaving, arriving),
                heavy,
            ),
            style,
        );
        if r.cross > r.head {
            p.at(
                arriving.0,
                arriving.1,
                corner(
                    side(layout, arriving, leaving),
                    side(layout, arriving, (r.cross - 1, r.to_across)),
                    heavy,
                ),
                style,
            );
        }
    }

    // Where the head is entered from: the run before it when there is one,
    // otherwise the crossing stroke itself.
    // The head sits one step forward in time from the parent's border, so one
    // step *back* from it is the block it names.
    let head = (r.head, r.to_across);
    let parent = (r.head.saturating_sub(1), r.to_across);
    p.put_over(head.0, head.1, arrowhead(side(layout, head, parent)), style);
}

/// A one-line description of what the cursor is on, for the message line.
pub fn describe_focus(dag: &Dag, focus: &Focus) -> String {
    match focus {
        Focus::Head => "HEAD".to_string(),
        Focus::Ref(name) => format!("branch {name}"),
        Focus::Commit(id) => describe_commit(dag, id),
    }
}

fn describe_commit(dag: &Dag, id: &Oid) -> String {
    match dag.get(id) {
        Some(c) => format!("{} {}", id.short(), c.summary),
        None => format!("{} (a commit the plan would create)", id.short()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{plan::Edit, Change, Commit, Head, Ref, WorkTree};
    use ratatui::{backend::TestBackend, Terminal};

    fn commit(id: &str, parents: &[&str], summary: &str) -> Commit {
        Commit {
            id: Oid::new(id),
            parents: parents.iter().map(|p| Oid::new(*p)).collect(),
            summary: summary.into(),
            author: "Christian".into(),
            when: 0,
            insertions: 293,
            deletions: 45,
        }
    }

    fn state() -> VcsState {
        let dag = Dag::new(
            vec![
                commit("aaaaaaa1", &["bbbbbbb2"], "Fix sigterm handling"),
                commit("bbbbbbb2", &[], "Initial commit"),
            ],
            vec![Ref {
                name: "main".into(),
                kind: RefKind::Local,
                target: Oid::new("aaaaaaa1"),
                upstream: None,
            }],
            Head { branch: Some("main".into()), target: Some(Oid::new("aaaaaaa1")) },
            WorkTree::new(vec![
                Change { path: "src/app.rs".into(), index: 'M', work: ' ' },
                Change { path: "src/ui.rs".into(), index: 'A', work: ' ' },
                Change { path: "notes.md".into(), index: ' ', work: 'M' },
                Change { path: "scratch.ipynb".into(), index: '?', work: '?' },
            ]),
            false,
        );
        VcsState::new(std::path::PathBuf::from("/tmp/r"), dag, 86_400 * 3)
    }

    /// `main`, and a second branch with a commit of its own hanging off the
    /// same root — two rows, and an arrow that has to change track between
    /// them.  The second branch needs a commit `main` does not contain: one
    /// merely *behind* the branch you are on owns no run and so no row (see
    /// `layout::a_branch_left_behind_does_not_own_the_trunk_beneath_it`).
    fn two_branches() -> VcsState {
        let dag = Dag::new(
            vec![
                commit("aaaaaaa1", &["bbbbbbb2"], "Fix sigterm handling"),
                commit("ccccccc3", &["bbbbbbb2"], "Try something else"),
                commit("bbbbbbb2", &[], "Initial commit"),
            ],
            vec![
                Ref { name: "main".into(), kind: RefKind::Local, target: Oid::new("aaaaaaa1"), upstream: None },
                Ref { name: "older".into(), kind: RefKind::Local, target: Oid::new("ccccccc3"), upstream: None },
            ],
            Head { branch: Some("main".into()), target: Some(Oid::new("aaaaaaa1")) },
            WorkTree::default(),
            false,
        );
        VcsState::new(std::path::PathBuf::from("/tmp/r"), dag, 86_400 * 3)
    }

    fn draw(state: &VcsState, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), state))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Everything the user asked to see on a block: hash, message, author,
    /// age, and the line counts.
    #[test]
    fn a_commit_block_carries_its_hash_message_author_age_and_diffstat() {
        let screen = draw(&state(), 90, 30).join("\n");
        assert!(screen.contains("aaaaaa"), "the abbreviated hash\n{screen}");
        // The summary wraps over the block's two text rows.
        assert!(screen.contains("Fix sigterm"), "the summary\n{screen}");
        assert!(screen.contains("handling"), "…all of it\n{screen}");
        assert!(screen.contains("Christian"), "the author\n{screen}");
        assert!(screen.contains("3d ago"), "the age\n{screen}");
        assert!(screen.contains("+293"), "insertions\n{screen}");
        assert!(screen.contains("-45"), "deletions\n{screen}");
    }

    #[test]
    fn head_is_drawn_as_its_own_block_naming_the_branch_and_the_work_tree() {
        let screen = draw(&state(), 90, 30).join("\n");
        assert!(screen.contains("HEAD"), "{screen}");
        assert!(screen.contains("● main"), "{screen}");
        assert!(screen.contains("2 staged"), "{screen}");
        assert!(screen.contains("1 unstaged"), "{screen}");
        // …and the untracked strays, which are the ones nobody remembers.
        assert!(screen.contains("1 untracked"), "{screen}");
    }

    /// Each branch's row says whose it is, at the viewport's left edge.  The
    /// label on a branch's tip is a screenful away on a long history, so
    /// without this nothing on screen names the branch you are reading.
    #[test]
    fn every_branch_row_carries_its_name_at_the_left_edge() {
        let state = two_branches();
        let lines = draw(&state, 90, 30);
        for name in ["main", "older"] {
            assert!(
                lines.iter().any(|l| l.starts_with(&format!("╾ {name}"))),
                "no row is named {name}\n{}",
                lines.join("\n")
            );
        }
    }

    /// …and it stays there once the graph is scrolled sideways, which is
    /// exactly when the question is worth asking.
    /// Vertical has no room for a name band per track, so every track is
    /// named at once in a row pinned above the graph — over its own columns,
    /// which is the only thing that says which column is which.
    #[test]
    fn every_branch_column_is_named_in_the_row_above_the_graph() {
        let mut state = two_branches();
        state.orient = Orientation::Vertical;
        let lines = draw(&state, 100, 40);
        let header = lines.first().expect("a row above the graph");
        assert!(header.contains("main"), "the header does not name main: {header}");
        assert!(header.contains("older"), "the header does not name older: {header}");

        // Each name sits over its own track's columns, not all in one place.
        let layout = state.layout(100);
        for track in 0..layout.track_count {
            let Some(name) = layout.lane_label(track) else { continue };
            // Character columns, not byte offsets: the tail glyph is three
            // bytes and one column.
            let byte = header.find(name).expect("the name is drawn");
            let at = header[..byte].chars().count();
            let want = layout.label_across(track) as usize;
            assert!(
                at >= want && at <= want + 3,
                "{name} is written at {at}, not over its own column at {want}"
            );
        }
        // …and the graph itself starts below that row.
        assert!(lines[1].contains('╭') || lines[1].contains('┏'), "{:?}", lines[1]);
    }

    #[test]
    fn a_row_keeps_its_name_when_the_graph_is_scrolled() {
        let mut state = state();
        state.scroll_along = 30;
        let lines = draw(&state, 90, 30);
        assert!(
            lines.iter().any(|l| l.starts_with("╾ main")),
            "{}",
            lines.join("\n")
        );
    }

    #[test]
    fn a_branch_label_is_drawn_on_its_commit() {
        let screen = draw(&state(), 90, 30).join("\n");
        assert!(screen.contains(" main "), "{screen}");
    }

    /// The exact character drawn at `(row, col)` of the graph.
    ///
    /// The corner tests need one cell rather than a line: a corner glyph on
    /// its own says nothing about *which way it faces*, and facing is the
    /// whole property under test.
    /// What is drawn at one *graph* cell, whichever way the graph is turned.
    ///
    /// Going through the layout's own mapping is the point: a test that read
    /// screen coordinates directly would be a second statement of the
    /// orientation, and would pass while the renderer and the navigation
    /// disagreed about where things are.
    /// The screen this module's tests draw into.  Big enough to hold the
    /// widest fixture either way up, so a cell being off it means the test
    /// asked for one that is genuinely not drawn.
    const TEST_W: u16 = 200;
    const TEST_H: u16 = 80;

    fn cell_at(state: &VcsState, along: u16, across: u16) -> Option<char> {
        let mut terminal = Terminal::new(TestBackend::new(TEST_W, TEST_H)).unwrap();
        terminal.draw(|f| render(f, f.area(), state)).unwrap();
        let layout = state.layout(TEST_W);
        let (x, y) = layout.screen(along, across);
        // Vertical reserves the top row for the branch names.
        let y = y + u16::from(layout.metrics.orient == Orientation::Vertical);
        if x >= TEST_W || y >= TEST_H {
            return None;
        }
        terminal.backend().buffer()[(x, y)].symbol().chars().next()
    }

    /// The sides of a cell an arrow glyph continues on.
    ///
    /// `None` for anything that is not part of an arrow, which is how the
    /// check below notices a gap rather than reading a block's border as a
    /// continuation of the line.  An arrowhead accepts a line from **any**
    /// side: several children of one commit converge on the same cell beside
    /// its border, and one glyph cannot point four ways.
    fn strokes_of(ch: char) -> Option<[bool; 4]> {
        match ch {
            '◀' | '▶' | '▲' | '▼' => Some([true; 4]),
            _ => line_strokes(ch).map(|(sides, _)| sides),
        }
    }

    /// One arrow's cells in the order it travels them.
    fn path(layout: &Layout, edge: &Edge) -> Vec<(u16, u16)> {
        let r = layout.route(edge);
        let mut path: Vec<(u16, u16)> = (r.cross + 1..=r.start)
            .rev()
            .map(|along| (along, r.from_across))
            .collect();
        let (from, to) = (r.from_across as i32, r.to_across as i32);
        let step = if to >= from { 1 } else { -1 };
        let mut across = from;
        loop {
            path.push((r.cross, across as u16));
            if across == to {
                break;
            }
            across += step;
        }
        path.extend((r.head..r.cross).rev().map(|along| (along, r.to_across)));
        path
    }

    /// A graph that turns every way an arrow can.
    ///
    /// Three branches, so three rows; `a` is a row of its own below `feature`
    /// and above `main`, so one arrow turns down into it and another turns up.
    /// `f` is a merge, whose second parent is the one arrow whose horizontal
    /// run reaches its target far enough away to need a corner at *both* ends.
    ///
    /// ```text
    ///   feature:      c ── d
    ///   base:     a
    ///   main:         e ── f   (f also follows c)
    /// ```
    fn forked() -> VcsState {
        let dag = Dag::new(
            vec![
                // `main`'s tip first: rows are handed out in the order the
                // tips are drawn, so this is what puts a branch *below* the
                // trunk and gives the fixture a turn in each direction.
                commit("f", &["e", "c"], "three"),
                commit("d", &["c"], "four"),
                commit("c", &["a"], "two"),
                commit("e", &["a"], "two again"),
                commit("b", &["a"], "aside"),
                commit("a", &[], "one"),
            ],
            vec![
                Ref { name: "feature".into(), kind: RefKind::Local, target: Oid::new("d"), upstream: None },
                // Its own commit, not one `main` already contains: a branch
                // behind the one you are on owns no run and so no row.
                Ref { name: "base".into(), kind: RefKind::Local, target: Oid::new("b"), upstream: None },
                Ref { name: "main".into(), kind: RefKind::Local, target: Oid::new("f"), upstream: None },
            ],
            Head { branch: Some("main".into()), target: Some(Oid::new("f")) },
            WorkTree::default(),
            false,
        );
        VcsState::new(std::path::PathBuf::from("/tmp/r"), dag, 0)
    }

    /// A corner faces the runs it joins.
    ///
    /// The arrow travels **right to left**, so the segment above a turn is to
    /// the corner's east and the segment below it is to the west — and a
    /// corner drawn the other way round is a line that appears to come from
    /// nowhere.  Checked cell by cell against the route the layout published,
    /// because the glyph alone does not say which way it faces.
    #[test]
    fn every_arrow_reads_as_one_unbroken_line() {
        for orient in [Orientation::Horizontal, Orientation::Vertical] {
            let mut state = forked();
            state.orient = orient;
            state.focus = None;
            let layout = state.layout(TEST_W);
            let mut turns = 0;

            for edge in &layout.edges {
                let r = layout.route(edge);
                if r.stub {
                    continue;
                }
                turns += usize::from(r.from_across != r.to_across);
                let cells = path(&layout, edge);
                for pair in cells.windows(2) {
                    let (a, b) = (pair[0], pair[1]);
                    let (ax, ay) = layout.screen(a.0, a.1);
                    let (bx, by) = layout.screen(b.0, b.1);
                    // [north, south, east, west]
                    let (from_a, from_b) = match (bx as i32 - ax as i32, by as i32 - ay as i32) {
                        (dx, _) if dx > 0 => (2, 3),
                        (dx, _) if dx < 0 => (3, 2),
                        (_, dy) if dy > 0 => (1, 0),
                        _ => (0, 1),
                    };
                    let (Some(ga), Some(gb)) = (cell_at(&state, a.0, a.1), cell_at(&state, b.0, b.1))
                    else {
                        continue;
                    };
                    let (sa, sb) = (
                        strokes_of(ga).unwrap_or_else(|| panic!("{orient:?}: {ga:?} is not part of an arrow")),
                        strokes_of(gb).unwrap_or_else(|| panic!("{orient:?}: {gb:?} is not part of an arrow")),
                    );
                    assert!(
                        sa[from_a] && sb[from_b],
                        "{orient:?}: the arrow {} -> {:?} breaks between {ga:?} and {gb:?}",
                        edge.child,
                        edge.parent
                    );
                }
            }
            assert!(turns >= 2, "{orient:?}: the fixture must turn");
        }
    }

    /// A focused arrow is drawn with the heavy box-drawing set, and its
    /// corners have to be heavy too.  A light rounded corner in the middle of
    /// a heavy run leaves a visible notch exactly where the corner is there to
    /// join two strokes.
    #[test]
    fn a_focused_arrow_has_corners_of_its_own_weight() {
        // The second branch's commit sits in its own row, so its arrow back to
        // the shared root actually turns a corner.
        for orient in [Orientation::Horizontal, Orientation::Vertical] {
            let mut state = two_branches();
            state.orient = orient;
            state.focus = Some(Focus::Commit(Oid::new("ccccccc3")));
            let layout = state.layout(TEST_W);
            let edge = layout
                .edges
                .iter()
                .find(|e| e.child == Oid::new("ccccccc3"))
                .expect("the arrow");
            let r = layout.route(edge);
            assert_ne!(r.from_across, r.to_across, "the fixture has to turn a corner");

            // Asked of the module's own weight table rather than a list
            // written out here: a corner where two arrows meet is a junction
            // glyph, and a second list would have to remember that.
            let weight = |state: &VcsState| {
                let turn = cell_at(state, r.cross, r.from_across).expect("the corner is drawn");
                line_strokes(turn)
                    .unwrap_or_else(|| panic!("{orient:?}: {turn:?} is not a line at all"))
                    .1
            };
            assert!(weight(&state), "{orient:?}: a heavy arrow turned a light corner");

            // …and an unfocused one keeps the light set end to end.
            state.focus = None;
            assert!(!weight(&state), "{orient:?}: a light arrow turned a heavy corner");
        }
    }

    /// The arrowhead points at the *parent*, which is to the **left**: a
    /// commit names its parent and never the reverse, and drawing it the other
    /// way teaches the graph backwards.
    #[test]
    fn arrows_point_back_from_a_commit_to_its_parent() {
        let lines = draw(&state(), 90, 30);
        let screen = lines.join("\n");
        assert!(screen.contains('◀'), "an arrowhead is drawn\n{screen}");
        let col = lines
            .iter()
            .find_map(|line| line.find('◀'))
            .expect("an arrowhead");
        // The border row carries both hashes: the parent's has to be to the
        // left of the arrowhead and the child's to its right.
        let border = lines.iter().find(|l| l.contains("bbbbbbb")).expect("the border row");
        assert!(border.find("bbbbbbb").unwrap() < col, "the parent is not to the left\n{screen}");
        assert!(border.find("aaaaaaa").unwrap() > col, "the child is not to the right\n{screen}");
    }

    /// A view that paints cells directly has to be clipped by construction —
    /// a terminal one row taller than a block must not panic or bleed.
    #[test]
    fn drawing_into_a_screen_far_too_small_clips_instead_of_panicking() {
        for (w, h) in [(1, 1), (10, 3), (30, 5), (200, 2)] {
            let _ = draw(&state(), w, h);
        }
    }

    /// The preview is the whole interaction: a plan that moves a branch has to
    /// draw the branch at its new home before anything is applied.
    #[test]
    fn a_planned_change_is_visible_before_it_is_applied() {
        let mut state = state();
        state
            .push_edit(Edit::MoveRef {
                name: "main".into(),
                new_target: Oid::new("bbbbbbb2"),
            })
            .unwrap();
        let lines = draw(&state, 90, 30);
        let screen = lines.join("\n");
        // The label is drawn on its block's own top border, and each branch
        // now has a row of its own — so "which block is it on" is answered by
        // finding the border row that carries the older commit's hash and
        // checking the label is on that row, after it.
        let border = lines
            .iter()
            .find(|l| l.contains("bbbbbbb"))
            .expect("the older commit's border row");
        let hash = border.find("bbbbbbb").expect("the hash");
        let label = border
            .find(" main ")
            .unwrap_or_else(|| panic!("main should now sit on the older commit\n{screen}"));
        assert!(hash < label, "the label is not on that block\n{screen}");
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("aaaaaaa") && l.contains(" main ")),
            "and no longer on the newer one\n{screen}"
        );
    }

    #[test]
    fn an_empty_repository_says_so_rather_than_drawing_nothing() {
        let dag = Dag::new(Vec::new(), Vec::new(), Head::default(), WorkTree::default(), false);
        let state = VcsState::new(std::path::PathBuf::from("/tmp/r"), dag, 0);
        assert!(draw(&state, 60, 10).join("\n").contains("no commits yet"));
    }

    #[test]
    fn the_focus_description_names_what_the_cursor_is_on() {
        let state = state();
        let dag = &state.dag;
        assert_eq!(describe_focus(dag, &Focus::Head), "HEAD");
        assert_eq!(describe_focus(dag, &Focus::Ref("main".into())), "branch main");
        assert!(describe_focus(dag, &Focus::Commit(Oid::new("aaaaaaa1")))
            .contains("Fix sigterm"));
    }
}



