//! Renderer for the version-control graph.
//!
//! Draws from a [`vcs::layout::Layout`] and nothing else.  The navigation in
//! `vcs::state` walks the same `Layout`, so what is under the cursor and what
//! is on screen cannot disagree — the same discipline `table_ui` has with
//! `table::layout`, and for the same reason.
//!
//! Everything is drawn into a cell buffer directly rather than through
//! ratatui widgets: blocks overlap arrows, arrows cross lanes, and the whole
//! picture is a coordinate grid rather than a stack of rectangles.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    Frame,
};

use crate::{
    theme,
    vcs::{
        layout::{Block, BlockKind, Edge, Focus, Layout},
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
}

/// Draw the graph for `state` into `area`.
pub fn render(frame: &mut Frame, area: Rect, state: &VcsState) {
    let th = theme::active();
    let layout = state.layout(area.width);
    let mut p = Painter {
        frame,
        area,
        scroll_row: state.scroll_row,
        scroll_col: layout.lane_col(state.scroll_lane),
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
    for edge in &layout.edges {
        draw_edge(&mut p, &layout, edge, &cursor);
    }
    for block in &layout.blocks {
        draw_block(&mut p, state, &layout, block, &cursor);
    }
    draw_horizon(&mut p, state, &layout);
}

/// Say so when the walk stopped at the commit limit.
///
/// Without it the oldest block on screen looks like the repository's first
/// commit, and its missing parent arrow looks like a root — a picture that is
/// simply false about older history.
fn draw_horizon(p: &mut Painter, state: &VcsState, layout: &Layout) {
    if !state.dag.truncated {
        return;
    }
    let Some(oldest) = state.dag.commits().last() else { return };
    let Some(block) = layout.block(&oldest.id) else { return };
    p.text(
        block.row + block.height + 1,
        layout.lane_col(block.lane) + 2,
        "⋮ older history not loaded",
        Style::default().fg(theme::active().dim),
        layout.lane_width,
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
        // By branch, not by lane: a branch that is merely *ahead* of another
        // shares its column, correctly, and colouring by column then painted
        // the whole history one colour.
        BlockKind::Commit => th.vcs_tint(block.tint),
    };
    (cursor.style(&this, base), cursor.mark(&this).is_some())
}

fn draw_block(p: &mut Painter, state: &VcsState, layout: &Layout, block: &Block, cursor: &Cursor) {
    let (style, heavy) = border_style(block, cursor);
    let ch = if heavy { &HEAVY } else { &LIGHT };
    let left = layout.lane_col(block.lane);
    let width = layout.lane_width;
    let right = left + width - 1;
    let bottom = block.row + block.height - 1;

    // --- frame ---
    for col in left..=right {
        p.cell(block.row, col, ch.h, style);
        p.cell(bottom, col, ch.h, style);
    }
    for row in block.row + 1..bottom {
        p.cell(row, left, ch.v, style);
        p.cell(row, right, ch.v, style);
    }
    p.cell(block.row, left, ch.tl, style);
    p.cell(block.row, right, ch.tr, style);
    p.cell(bottom, left, ch.bl, style);
    p.cell(bottom, right, ch.br, style);

    match block.kind {
        BlockKind::Head => draw_head_contents(p, state, layout, block),
        _ => draw_commit_contents(p, state, layout, block, cursor),
    }
}

/// HEAD: which branch you are on, and what is uncommitted.
fn draw_head_contents(p: &mut Painter, state: &VcsState, layout: &Layout, block: &Block) {
    let th = theme::active();
    let (left, inner) = (layout.lane_col(block.lane), layout.block_inner());
    let bold = Style::default().fg(th.vcs_head).add_modifier(Modifier::BOLD);
    p.text(block.row, left + 2, " HEAD ", bold, inner);

    let head = &state.dag.head;
    let where_ = match (&head.branch, &head.target) {
        (Some(branch), _) => format!("● {branch}"),
        (None, Some(oid)) => format!("● detached at {}", oid.short()),
        (None, None) => "● no commits yet".to_string(),
    };
    p.text(block.row + 1, left + 2, &where_, bold, inner);

    // The work tree, right-aligned in the same row: it belongs to "where you
    // are" rather than to any commit, and it is what blocks an apply.
    let work = &state.dag.work;
    let mut parts = Vec::new();
    if work.staged > 0 {
        parts.push((format!("{} staged", work.staged), th.git_added));
    }
    if work.unstaged > 0 {
        parts.push((format!("{} unstaged", work.unstaged), th.git_modified));
    }
    if work.conflicted > 0 {
        parts.push((format!("{} conflicted", work.conflicted), th.error));
    }
    if work.untracked > 0 {
        parts.push((format!("{} untracked", work.untracked), th.dim));
    }
    if parts.is_empty() {
        parts.push(("clean".to_string(), th.dim));
    }
    let total: u16 = parts.iter().map(|(t, _)| t.chars().count() as u16 + 2).sum::<u16>();
    let mut col = left + 1 + inner.saturating_sub(total);
    for (text, colour) in parts {
        p.text(block.row + 1, col, &text, Style::default().fg(colour), inner);
        col += text.chars().count() as u16 + 2;
    }
}

/// A commit: hash and refs on the top border, summary, then author, age and
/// the line counts.
fn draw_commit_contents(
    p: &mut Painter,
    state: &VcsState,
    layout: &Layout,
    block: &Block,
    cursor: &Cursor,
) {
    let th = theme::active();
    let (left, inner) = (layout.lane_col(block.lane), layout.block_inner());
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
    p.text(block.row, left + 1, &hash, hash_style, inner);

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
        p.text(block.row, left + col, &format!(" {name} "), style, inner);
    }

    // --- summary ---
    let summary = pending
        .as_ref()
        .map(|p| p.summary.clone())
        .or_else(|| commit.map(|c| c.summary.clone()))
        .unwrap_or_else(|| "(not loaded)".to_string());
    p.text(block.row + 1, left + 2, &summary, Style::default(), inner.saturating_sub(1));

    // --- author · age, and the line counts on the right ---
    let Some(commit) = commit else {
        p.text(
            block.row + 2,
            left + 2,
            "will be created by :vc-apply",
            Style::default().fg(th.vcs_pending),
            inner.saturating_sub(1),
        );
        return;
    };
    let meta = format!("{} · {}", commit.author, relative_time(commit.when, state.now));
    p.text(block.row + 2, left + 2, &meta, Style::default().fg(th.dim), inner.saturating_sub(1));

    let plus = format!("+{}", commit.insertions);
    let minus = format!("-{}", commit.deletions);
    let width = plus.chars().count() as u16 + minus.chars().count() as u16 + 1;
    let col = left + 1 + inner.saturating_sub(width);
    p.text(block.row + 2, col, &plus, Style::default().fg(th.git_added), width);
    p.text(
        block.row + 2,
        col + plus.chars().count() as u16 + 1,
        &minus,
        Style::default().fg(th.error),
        width,
    );
}

/// Draw one parent link.
///
/// Routed as a vertical drop in the child's lane, a horizontal run across to
/// the parent's lane, and a vertical drop into the parent's top border, where
/// the arrowhead goes.  The arrowhead is at the *parent* end because that is
/// the direction the link points: a commit names its parent, never the
/// reverse, and drawing it the other way would teach the graph backwards.
fn draw_edge(p: &mut Painter, layout: &Layout, edge: &Edge, cursor: &Cursor) {
    let th = theme::active();
    let this = Focus::Edge { child: edge.child.clone(), slot: edge.slot };
    // An arrow belongs to the commit it leaves, so it takes that commit's
    // colour and a branch reads as one colour from tip to base.  HEAD's arrow
    // is not a parent link and keeps the neutral edge colour.
    let base = match layout.block(&edge.child) {
        Some(block) if block.kind != BlockKind::Head => th.vcs_tint(block.tint),
        _ => th.vcs_edge,
    };
    let style = cursor.style(&this, base);
    // A focused arrow is drawn heavy rather than in another colour, the same
    // way a focused block gets a heavy border: an arrow is a thin line, and
    // bold alone is not enough to find it.
    let heavy = cursor.mark(&this).is_some();
    let (v, h) = if heavy { ('┃', '━') } else { ('│', '─') };

    let r = layout.route(edge);

    if r.stub {
        // Past the horizon: a short stub that visibly goes nowhere, rather
        // than an arrow into empty space.
        for (row, col) in r.cells() {
            p.cell(row, col, '╎', style);
        }
        return;
    }

    // Down the child's lane to the crossing row…
    for row in r.start..r.cross {
        p.cell(row, r.from_col, v, style);
    }
    // …across…
    let (lo, hi) = (r.from_col.min(r.to_col), r.from_col.max(r.to_col));
    for col in lo..=hi {
        p.cell(r.cross, col, h, style);
    }
    // …and down the parent's lane to the arrowhead.  Where the two lanes are
    // the same all three reduce to one straight drop.
    for row in r.cross + 1..r.head_row {
        p.cell(row, r.to_col, v, style);
    }

    if r.from_col != r.to_col {
        // Corners, so the run reads as one line rather than three.
        let right = r.to_col > r.from_col;
        // The turn always gets a corner, even when the arrow crosses on its
        // very first row: the stroke it turns out of is the block sitting
        // directly above, and a bare `─` there reads as a line from nowhere.
        p.cell(r.cross, r.from_col, if right { '╰' } else { '╯' }, style);
        if r.cross < r.head_row {
            p.cell(r.cross, r.to_col, if right { '╮' } else { '╭' }, style);
        }
    }
    p.cell(r.head_row, r.to_col, '▼', style);
}

/// A one-line description of what the cursor is on, for the message line.
pub fn describe_focus(dag: &Dag, focus: &Focus) -> String {
    match focus {
        Focus::Head => "HEAD".to_string(),
        Focus::Ref(name) => format!("branch {name}"),
        Focus::Commit(id) => describe_commit(dag, id),
        Focus::Edge { child, slot } => {
            let parent = dag
                .get(child)
                .and_then(|c| c.parents.get(*slot))
                .map_or_else(|| "nothing".to_string(), |p| p.short().to_string());
            format!("the link from {} to {parent}", child.short())
        }
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
    use crate::vcs::{plan::Edit, Commit, Head, Ref, WorkTree};
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
            WorkTree { staged: 2, unstaged: 1, ..Default::default() },
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
        assert!(screen.contains("Fix sigterm handling"), "the summary\n{screen}");
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
    }

    #[test]
    fn a_branch_label_is_drawn_on_its_commit() {
        let screen = draw(&state(), 90, 30).join("\n");
        assert!(screen.contains(" main "), "{screen}");
    }

    /// The arrowhead points at the *parent*: a commit names its parent and
    /// never the reverse, and drawing it the other way teaches the graph
    /// backwards.
    #[test]
    fn arrows_point_from_a_commit_down_to_its_parent() {
        let screen = draw(&state(), 90, 30).join("\n");
        assert!(screen.contains('▼'), "an arrowhead is drawn\n{screen}");
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
        // The label is drawn on its block's own top border, so "which block is
        // it on" is answered by which border row carries both the hash and the
        // name — not by proximity, which would also match HEAD's `● main`.
        let on_older = lines
            .iter()
            .any(|l| l.contains("bbbbbbb") && l.contains(" main "));
        let on_newer = lines
            .iter()
            .any(|l| l.contains("aaaaaaa") && l.contains(" main "));
        assert!(on_older, "main should now sit on the older commit\n{screen}");
        assert!(!on_newer, "and no longer on the newer one\n{screen}");
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
        assert_eq!(
            describe_focus(dag, &Focus::Edge { child: Oid::new("aaaaaaa1"), slot: 0 }),
            "the link from aaaaaaa to bbbbbbb"
        );
    }
}
