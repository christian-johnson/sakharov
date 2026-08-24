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

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    Frame,
};

use crate::{
    theme,
    render_util::wrap_segments,
    vcs::{
        layout::{Block, BlockKind, Edge, Focus, Layout, BLOCK_H, GAP},
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
struct Painter<'a, 'b> {
    frame: &'a mut Frame<'b>,
    area: Rect,
    scroll_row: u16,
    scroll_col: u16,
}

impl Painter<'_, '_> {
    /// One cell, or nothing if it is scrolled off or past the edge.
    fn cell(&mut self, row: u16, col: u16, ch: char, style: Style) {
        let Some(row) = row.checked_sub(self.scroll_row) else { return };
        let Some(col) = col.checked_sub(self.scroll_col) else { return };
        if row >= self.area.height || col >= self.area.width {
            return;
        }
        self.frame.buffer_mut()[(self.area.x + col, self.area.y + row)]
            .set_char(ch)
            .set_style(style);
    }

    /// `text`, truncated to `max` columns.
    fn text(&mut self, row: u16, col: u16, text: &str, style: Style, max: u16) {
        for (i, ch) in text.chars().take(max as usize).enumerate() {
            self.cell(row, col + i as u16, ch, style);
        }
    }

    /// `text` at the viewport's own left edge: scrolled with the graph
    /// vertically, never horizontally.
    ///
    /// A row's branch name is the answer to "which branch am I looking at",
    /// and that question is at its sharpest a hundred commits along a history
    /// — exactly where a name written in graph coordinates has scrolled off.
    fn pinned_text(&mut self, row: u16, text: &str, style: Style) {
        let Some(row) = row.checked_sub(self.scroll_row) else { return };
        if row >= self.area.height {
            return;
        }
        for (i, ch) in text.chars().enumerate() {
            let col = i as u16;
            if col >= self.area.width {
                return;
            }
            self.frame.buffer_mut()[(self.area.x + col, self.area.y + row)]
                .set_char(ch)
                .set_style(style);
        }
    }
}

/// Draw the graph for `state` into `area`.
pub fn render(frame: &mut Frame, area: Rect, state: &VcsState) {
    let th = theme::active();
    let layout = state.layout(area.width);
    let mut p = Painter {
        frame,
        area,
        scroll_row: layout.label_row(state.scroll_track),
        scroll_col: state.scroll_col,
    };

    if state.dag.is_empty() && state.dag.head.branch.is_none() {
        p.scroll_row = 0;
        p.scroll_col = 0;
        p.text(
            0,
            2,
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
    // Last, so a lane's name wins over an arrow that happens to cross the row
    // it is written on.
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
    for track in 0..layout.track_count {
        let Some(name) = layout.lane_label(track) else { continue };
        let colour = layout
            .branch_tints
            .get(name)
            .map_or(th.vcs_branch, |&t| th.vcs_tint(t));
        p.pinned_text(
            layout.label_row(track),
            &format!("╾ {name} "),
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        );
    }
}

/// Say so when the walk stopped at the commit limit.
///
/// Without it the oldest block on screen looks like the repository's first
/// commit, and its missing parent arrow looks like a root — a picture that is
/// simply false about older history.  Written in the gap row under the oldest
/// block, because the columns to its left are the few the stub arrow needs.
fn draw_horizon(p: &mut Painter, state: &VcsState, layout: &Layout) {
    if !state.dag.truncated {
        return;
    }
    let Some(oldest) = state.dag.commits().last() else { return };
    let Some(block) = layout.block(&oldest.id) else { return };
    p.text(
        layout.track_row(block.track) + BLOCK_H,
        block.col,
        "⋯ older history not loaded",
        Style::default().fg(theme::active().dim),
        layout.block_width + GAP,
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

fn draw_block(p: &mut Painter, state: &VcsState, layout: &Layout, block: &Block, cursor: &Cursor) {
    let (style, heavy) = border_style(block, cursor);
    let ch = if heavy { &HEAVY } else { &LIGHT };
    let left = block.col;
    let top = layout.track_row(block.track);
    let right = left + layout.block_width - 1;
    let bottom = top + BLOCK_H - 1;

    // --- frame ---
    for col in left..=right {
        p.cell(top, col, ch.h, style);
        p.cell(bottom, col, ch.h, style);
    }
    for row in top + 1..bottom {
        p.cell(row, left, ch.v, style);
        p.cell(row, right, ch.v, style);
    }
    p.cell(top, left, ch.tl, style);
    p.cell(top, right, ch.tr, style);
    p.cell(bottom, left, ch.bl, style);
    p.cell(bottom, right, ch.br, style);

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
    let (left, top, inner) = (block.col, layout.track_row(block.track), layout.block_inner());
    let bold = Style::default().fg(th.vcs_head).add_modifier(Modifier::BOLD);
    p.text(top, left + 2, " HEAD ", bold, inner);

    let head = &state.dag.head;
    let where_ = match (&head.branch, &head.target) {
        (Some(branch), _) => format!("● {branch}"),
        (None, Some(oid)) => format!("● detached at {}", oid.short()),
        (None, None) => "● no commits yet".to_string(),
    };
    p.text(top + 1, left + 2, &where_, bold, inner.saturating_sub(1));

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
    p.text(top + 2, left + 2, &tracked, Style::default().fg(colour), inner.saturating_sub(1));
    p.text(
        top + 3,
        left + 2,
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
    let (left, top, inner) = (block.col, layout.track_row(block.track), layout.block_inner());
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
    p.text(top, left + 1, &hash, hash_style, inner);

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
        p.text(top, left + col, &format!(" {name} "), style, inner);
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
            top + 1 + row as u16,
            left + 2,
            line.trim_end(),
            Style::default(),
            text_width as u16,
        );
    }

    // --- author · age ---
    let Some(commit) = commit else {
        p.text(
            top + 1 + SUMMARY_ROWS as u16,
            left + 2,
            "created by :vc-apply",
            Style::default().fg(th.vcs_pending),
            inner.saturating_sub(1),
        );
        return;
    };
    let meta = format!("{} · {}", commit.author, relative_time(commit.when, state.now));
    p.text(
        top + 1 + SUMMARY_ROWS as u16,
        left + 2,
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
    p.text(bottom, col, &plus, Style::default().fg(th.git_added), width);
    p.text(
        bottom,
        col + plus.chars().count() as u16,
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

/// The straight runs of one parent link.
///
/// Routed as a horizontal run back through the child's track, a vertical hop
/// across to the parent's track, and a run into the parent's right-hand
/// border, where the arrowhead goes.
fn draw_edge_runs(p: &mut Painter, layout: &Layout, edge: &Edge, cursor: &Cursor) {
    let (style, heavy) = edge_style(layout, edge, cursor);
    let (v, h) = if heavy { ('┃', '━') } else { ('│', '─') };
    let r = layout.route(edge);

    if r.stub {
        // Past the horizon: a short stub that visibly goes nowhere, rather
        // than an arrow into empty space.
        for (row, col) in r.cells() {
            p.cell(row, col, '╌', style);
        }
        return;
    }

    // Back through the child's track to the crossing column…
    for col in r.cross + 1..=r.start {
        p.cell(r.from_row, col, h, style);
    }
    // …across…
    let (lo, hi) = (r.from_row.min(r.to_row), r.from_row.max(r.to_row));
    for row in lo..=hi {
        p.cell(row, r.cross, v, style);
    }
    // …and on through the parent's track to the arrowhead.  Where the two
    // tracks are the same all three reduce to one straight run.
    for col in r.head_col..r.cross {
        p.cell(r.to_row, col, h, style);
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

    if r.from_row != r.to_row {
        // Corners, so the run reads as one line rather than three.
        let down = r.to_row > r.from_row;
        // The turn always gets a corner, even when the arrow crosses in the
        // very first column: the stroke it turns out of is the block sitting
        // directly beside it, and a bare `│` there reads as a line from
        // nowhere.
        //
        // The run the arrow leaves along is to the corner's east (it started
        // at the child, which is further right), and the run it arrives along
        // is to the corner's west (it ends at the parent, further left).  The
        // vertical leaves the first corner in the direction of travel and
        // arrives at the second from the opposite one.
        p.cell(r.from_row, r.cross, corner(true, down, heavy), style);
        if r.cross > r.head_col {
            p.cell(r.to_row, r.cross, corner(false, !down, heavy), style);
        }
    }
    p.cell(r.to_row, r.head_col, '◀', style);
}

/// The corner where an arrow turns.
///
/// `east` says which side of the corner the horizontal run is on and `south`
/// which way the vertical leaves it — the two facts that decide which of the
/// four glyphs joins them, and the two that are easy to state backwards.  The
/// arrow runs *right to left*, so the segment at the top of a turn is to the
/// corner's **east** and the segment at the bottom is to its **west**; a corner
/// facing the wrong way draws a line that appears to come from nowhere.
///
/// The weight has to match the strokes it joins too: a focused arrow is drawn
/// with the heavy set, and a light rounded corner in the middle of it leaves a
/// visible notch where the two stroke widths fail to meet — which is exactly
/// what a corner is there to prevent.
fn corner(east: bool, south: bool, heavy: bool) -> char {
    match (east, south, heavy) {
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
        let mut state = state();
        // A second branch, one commit behind, so there are two rows to name.
        state.dag.refs.push(Ref {
            name: "older".into(),
            kind: RefKind::Local,
            target: Oid::new("bbbbbbb2"),
            upstream: None,
        });
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
    #[test]
    fn a_row_keeps_its_name_when_the_graph_is_scrolled() {
        let mut state = state();
        state.scroll_col = 30;
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
    fn cell_at(state: &VcsState, row: u16, col: u16) -> char {
        let (w, h) = (140u16, 40u16);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, f.area(), state)).unwrap();
        terminal.backend().buffer()[(col, row)]
            .symbol()
            .chars()
            .next()
            .expect("a cell")
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
                commit("d", &["c"], "four"),
                commit("f", &["e", "c"], "three"),
                commit("c", &["a"], "two"),
                commit("e", &["a"], "two again"),
                commit("a", &[], "one"),
            ],
            vec![
                Ref { name: "feature".into(), kind: RefKind::Local, target: Oid::new("d"), upstream: None },
                Ref { name: "base".into(), kind: RefKind::Local, target: Oid::new("a"), upstream: None },
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
    fn an_arrow_turns_its_corners_towards_the_runs_they_join() {
        let mut state = forked();
        state.focus = None;
        let layout = state.layout(140);
        let mut seen_down = false;
        let mut seen_up = false;

        for edge in &layout.edges {
            let r = layout.route(edge);
            if r.stub || r.from_row == r.to_row {
                continue;
            }
            let down = r.to_row > r.from_row;
            seen_down |= down;
            seen_up |= !down;

            // Leaving: the run is to the east, the vertical goes on downwards
            // (or upwards) from here.
            assert_eq!(
                cell_at(&state, r.from_row, r.cross),
                if down { '╭' } else { '╰' },
                "the arrow {} -> {:?} leaves through a corner facing the wrong way",
                edge.child,
                edge.parent
            );
            // Arriving: the run is to the west, and the vertical came from the
            // side the arrow travelled down.
            if r.cross > r.head_col {
                assert_eq!(
                    cell_at(&state, r.to_row, r.cross),
                    if down { '╯' } else { '╮' },
                    "the arrow {} -> {:?} arrives through a corner facing the wrong way",
                    edge.child,
                    edge.parent
                );
            }
        }
        assert!(seen_down && seen_up, "the fixture must turn both ways");
    }

    /// A focused arrow is drawn with the heavy box-drawing set, and its
    /// corners have to be heavy too.  A light rounded corner in the middle of
    /// a heavy run leaves a visible notch exactly where the corner is there to
    /// join two strokes.
    #[test]
    fn a_focused_arrow_has_corners_of_its_own_weight() {
        let mut state = state();
        // A branch on the older commit, so the two sit in different rows and
        // the arrow between them actually turns a corner.
        state.dag.refs.push(Ref {
            name: "older".into(),
            kind: RefKind::Local,
            target: Oid::new("bbbbbbb2"),
            upstream: None,
        });
        state.focus = Some(Focus::Commit(Oid::new("aaaaaaa1")));
        let layout = state.layout(140);
        let edge = layout
            .edges
            .iter()
            .find(|e| e.child == Oid::new("aaaaaaa1"))
            .expect("the arrow");
        let r = layout.route(edge);
        assert!(r.from_row < r.to_row, "the fixture has to turn a corner");
        assert_eq!(
            cell_at(&state, r.from_row, r.cross),
            '┏',
            "a heavy arrow turned a light corner"
        );

        // …and an unfocused one keeps the light set end to end.
        state.focus = None;
        assert_eq!(
            cell_at(&state, r.from_row, r.cross),
            '╭',
            "a light arrow turned a heavy corner"
        );
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



