//! Drawing the merge-conflict resolver.
//!
//! The whole view is one idea: **put the two versions side by side, and say
//! whose each one is**.  Everything here serves that.
//!
//! - Each pane's two header rows are the [`SideLabel`](crate::conflict::SideLabel)
//!   — the role in words ("On main — your branch", "Your commit, being
//!   replayed") over the hash, author and age.  Never "ours" and "theirs".
//! - The panes are **line-aligned**: a region is as tall as its tallest side
//!   ([`layout::spans_for`]), so the same hunk sits on the same screen rows in
//!   both, and the eye compares across rather than counting.
//! - A side that is **switched on** is drawn in the git-added colour with a
//!   filled mark; one switched off is dim with a hollow one.  The same switch
//!   language the staging view and the branch picker use, so "what is going
//!   in" reads down the column at a glance.
//!
//! Geometry comes from [`crate::conflict::layout`] and nowhere else, so what
//! is drawn and what the cursor thinks it is on cannot disagree.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, BorderType, Borders},
    Frame,
};

use crate::render_util::sanitize_source;
use crate::conflict::{
    hunk::{Region, Versions},
    layout::{self, Layout, Span},
    state::ConflictState,
    Choice, ConflictFile, Side,
};

/// Columns a tab is drawn as.  Four rather than eight: the panes are half a
/// terminal wide each, and indented code is what is in them.
const TAB_WIDTH: usize = 4;

/// Marks for a side that is in, and one that is out.
const MARK_IN: &str = "●";
const MARK_OUT: &str = "○";

pub fn render(frame: &mut Frame, area: Rect, state: &ConflictState) {
    let Some(file) = state.current() else {
        return empty(frame, area);
    };
    let l = layout::compute(area, file, state.show_base);

    pane(frame, state, file, &l, Some(Side::Left));
    pane(frame, state, file, &l, Some(Side::Right));
    if l.base.is_some() {
        pane(frame, state, file, &l, None);
    }
    if l.result.height > 0 {
        result(frame, state, file, &l);
    }
}

/// Nothing conflicted — which is worth saying rather than leaving a blank
/// screen that reads as a view that failed to load.
fn empty(frame: &mut Frame, area: Rect) {
    let th = crate::theme::active();
    let block = Block::default()
        .title(" merge conflicts ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.popup_border))
        .style(Style::default().bg(th.popup_bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    put_line(
        frame,
        inner,
        0,
        1,
        "Nothing is conflicted.",
        Style::default().fg(th.dim),
    );
}

/// One pane: the label header, then the region rows.
///
/// `side` is `None` for the ancestor pane, which has no switch and is never
/// focusable — it is not one of the things that can be taken.
fn pane(
    frame: &mut Frame,
    state: &ConflictState,
    file: &ConflictFile,
    l: &Layout,
    side: Option<Side>,
) {
    let th = crate::theme::active();
    let rect = match side {
        Some(Side::Left) => l.left,
        Some(Side::Right) => l.right,
        None => match l.base {
            Some(rect) => rect,
            None => return,
        },
    };
    if rect.width < 4 || rect.height < 3 {
        return;
    }

    let focused = side == Some(state.side);
    let title = match side {
        Some(side) => state.set.label(side).role.clone(),
        None => "The common ancestor".to_string(),
    };
    // The focused pane is marked by **weight**, not by hue: recolouring it
    // would collide with the on/off colour, which is the one thing the pane's
    // colour already carries.  Same rule the commit graph's cursor follows.
    let mut border = Style::default().fg(match focused {
        true => th.popup_border_focus,
        false => th.popup_border,
    });
    if focused {
        border = border.add_modifier(Modifier::BOLD);
    }
    let block = Block::default()
        .title(format!(" {title} "))
        .title_style(match focused {
            true => Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
            false => Style::default().fg(th.dim),
        })
        .borders(Borders::ALL)
        .border_type(match focused {
            true => BorderType::Thick,
            false => BorderType::Rounded,
        })
        .border_style(border);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Row 0: the attribution — hash, author, age.  This is the half of the
    // label that tells a stale topic branch from a colleague's push an hour
    // ago, which the name alone cannot.
    let now = crate::vcs::now_secs();
    let attribution = match side {
        Some(side) => state.set.label(side).attribution(now),
        None => "what both sides started from".to_string(),
    };
    put_line(frame, inner, 0, 0, &attribution, Style::default().fg(th.dim));
    // Row 1: the commit's own summary, when there is one.
    let summary = match side {
        Some(side) => state.set.label(side).summary.clone(),
        None => String::new(),
    };
    put_line(frame, inner, 1, 0, &summary, Style::default().fg(th.dim));

    let body = Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(2),
        ..inner
    };
    rows(frame, state, file, l, side, body);
}

/// The region rows of one pane, clipped to the pane's scroll window.
fn rows(
    frame: &mut Frame,
    state: &ConflictState,
    file: &ConflictFile,
    l: &Layout,
    side: Option<Side>,
    body: Rect,
) {
    let th = crate::theme::active();
    let visible = state.scroll..state.scroll + body.height as usize;

    for (region, span) in file.regions.iter().zip(&l.spans) {
        if span.end() <= visible.start || span.start >= visible.end {
            continue;
        }
        match region {
            Region::Agreed(text) => {
                // Context, drawn the same in every pane: it is what both sides
                // say, and colouring it would imply a choice about it.
                for (i, line) in Versions::lines(text).iter().enumerate() {
                    draw_row(
                        frame,
                        body,
                        span.start + i,
                        visible.start,
                        "  ",
                        line,
                        Style::default().fg(th.dim),
                        None,
                    );
                }
            }
            Region::Conflict(versions) => {
                conflict_rows(frame, state, file, versions, *span, side, body, visible.start);
            }
        }
    }
}

/// One conflicted region in one pane: its marker row, then its text.
#[allow(clippy::too_many_arguments)]
fn conflict_rows(
    frame: &mut Frame,
    state: &ConflictState,
    file: &ConflictFile,
    versions: &Versions,
    span: Span,
    side: Option<Side>,
    body: Rect,
    scroll: usize,
) {
    let th = crate::theme::active();
    let Some(nth) = span.nth else { return };
    let choice = file.choices.get(nth).cloned().unwrap_or_default();
    let taken = side.is_some_and(|s| choice.takes(s));
    let current = nth == state.nth;

    // The marker row: which conflict this is, and whether this side is in.
    let (mark, mark_style) = match (side, &choice) {
        (None, _) => (" ", Style::default().fg(th.dim)),
        (Some(_), Choice::Edited(_)) => ("✎", Style::default().fg(th.accent)),
        (Some(_), _) if taken => (MARK_IN, Style::default().fg(th.git_added)),
        _ => (MARK_OUT, Style::default().fg(th.dim)),
    };
    // The "here" marker only in the focused pane: the current hunk is already
    // tinted in both, and repeating the words on the other side is noise that
    // says nothing the highlight has not.  Here it doubles as the row-level
    // answer to "which pane am I in", which the border alone gives only at the
    // edge of the screen.
    let header = format!(
        "{mark} {}/{}{}",
        nth + 1,
        file.conflict_count(),
        match current && side == Some(state.side) {
            true => "  ◂ here",
            false => "",
        }
    );
    let mut header_style = mark_style;
    if current {
        header_style = header_style.add_modifier(Modifier::BOLD);
    }
    draw_row(frame, body, span.start, scroll, "", &header, header_style, None);

    // The text.  Taken text is the git-added colour; text that is not going in
    // is dim — so a column of dim rows is a column of things being dropped,
    // readable without checking a single mark.
    let text = match side {
        Some(Side::Left) => &versions.left,
        Some(Side::Right) => &versions.right,
        None => &versions.base,
    };
    let style = match (side, taken) {
        (None, _) => Style::default().fg(th.dim),
        (Some(_), true) => Style::default().fg(th.git_added),
        (Some(_), false) => Style::default().fg(th.dim),
    };
    let bg = current.then_some(th.cell_selection_bg);

    let lines = Versions::lines(text);
    if lines.is_empty() {
        // A side that contributes nothing still needs a row saying so, or an
        // empty pane reads as a drawing bug rather than as "this side deleted
        // it" — which is a real and useful answer.
        draw_row(
            frame,
            body,
            span.start + 1,
            scroll,
            "  ",
            match side {
                None => "(no common ancestor)",
                Some(_) => "(nothing — this side removes it)",
            },
            Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
            bg,
        );
        return;
    }
    for (i, line) in lines.iter().enumerate() {
        draw_row(frame, body, span.start + 1 + i, scroll, "  ", line, style, bg);
    }
}

/// The strip along the bottom: what would actually be written for the focused
/// hunk.
///
/// The answer to "so what does that give me", which two panes of alternatives
/// do not by themselves show — especially for *both*, whose result is a thing
/// neither pane contains.
fn result(frame: &mut Frame, state: &ConflictState, file: &ConflictFile, l: &Layout) {
    let th = crate::theme::active();
    let left = file.undecided_count();
    let title = match left {
        0 => format!(" result — every conflict answered, Enter writes {} ", file.path),
        n => format!(" result — {n} still unanswered "),
    };
    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(match left {
            0 => th.git_added,
            _ => th.dim,
        }))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.popup_border));
    let inner = block.inner(l.result);
    frame.render_widget(block, l.result);
    if inner.height == 0 {
        return;
    }

    let Some(Region::Conflict(versions)) = file
        .regions
        .iter()
        .filter(|r| r.is_conflict())
        .nth(state.nth)
    else {
        return;
    };
    let choice = file.choices.get(state.nth).cloned().unwrap_or_default();
    let text = match &choice {
        Choice::Edited(text) => text.clone(),
        Choice::Sides { left, right } => {
            let mut out = String::new();
            if *left {
                out.push_str(&versions.left);
            }
            if *right {
                out.push_str(&versions.right);
            }
            out
        }
    };

    let lines = Versions::lines(&text);
    if lines.is_empty() {
        put_line(
            frame,
            inner,
            0,
            1,
            match choice.is_decided() {
                true => "(this section is deleted)",
                false => "Space takes the focused version · a / b take one outright",
            },
            Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
        );
        return;
    }
    for (i, line) in lines.iter().take(inner.height as usize).enumerate() {
        put_line(frame, inner, i as u16, 1, line, Style::default().fg(th.git_added));
    }
}

// ---------------------------------------------------------------------------
// Cell writing
// ---------------------------------------------------------------------------

/// Draw one content row, translated by the scroll offset and clipped.
#[allow(clippy::too_many_arguments)]
fn draw_row(
    frame: &mut Frame,
    body: Rect,
    row: usize,
    scroll: usize,
    prefix: &str,
    text: &str,
    style: Style,
    bg: Option<ratatui::style::Color>,
) {
    if row < scroll {
        return;
    }
    let y = (row - scroll) as u16;
    if y >= body.height {
        return;
    }
    let style = match bg {
        Some(bg) => style.bg(bg),
        None => style,
    };
    if bg.is_some() {
        for x in body.left()..body.right() {
            frame.buffer_mut()[(x, body.y + y)].set_char(' ').set_style(style);
        }
    }
    for (x, c) in (body.left()..body.right()).zip(prefix.chars().chain(sanitize_source(text, TAB_WIDTH).chars())) {
        frame.buffer_mut()[(x, body.y + y)].set_char(c).set_style(style);
    }
}

/// Draw one line at a fixed row of `rect`, with no scrolling.
fn put_line(frame: &mut Frame, rect: Rect, row: u16, indent: u16, text: &str, style: Style) {
    if row >= rect.height {
        return;
    }
    for (x, c) in (rect.left() + indent..rect.right()).zip(sanitize_source(text, TAB_WIDTH).chars()) {
        frame.buffer_mut()[(x, rect.y + row)].set_char(c).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conflict::{load, state::ConflictState, testrepo::diverged, Choice, Side};
    use ratatui::{backend::TestBackend, Terminal};

    fn draw(state: &ConflictState, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| render(f, Rect { x: 0, y: 0, width: w, height: h }, state))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    fn resolving() -> Option<ConflictState> {
        let repo = diverged()?;
        let _ = crate::vcs::load::git(&repo.root, &["merge", "feature"]);
        let set = load::read(&repo.root).ok()?;
        Some(ConflictState::new(set))
    }

    fn rebasing() -> Option<ConflictState> {
        let repo = diverged()?;
        let _ = crate::vcs::load::git(&repo.root, &["checkout", "feature"]);
        let _ = crate::vcs::load::git(&repo.root, &["rebase", "main"]);
        let set = load::read(&repo.root).ok()?;
        Some(ConflictState::new(set))
    }

    /// The whole reason the view exists: both panes carry a name, a commit, an
    /// author and a date — never `HEAD`, never a bare hash, and never the two
    /// words that cause the confusion.
    #[test]
    fn each_pane_is_headed_by_who_its_version_belongs_to() {
        let Some(state) = resolving() else { return };
        let body = draw(&state, 130, 26).join("\n");

        assert!(body.contains("On main — your branch"), "{body}");
        assert!(body.contains("Merging in feature"), "{body}");
        assert!(body.contains("· Test · just now"), "no attribution row: {body}");
        assert!(body.contains("bump to 45") && body.contains("bump to 60"));
        for word in ["ours", "theirs", "Ours", "Theirs", "HEAD"] {
            assert!(!body.contains(word), "`{word}` is back in the labels");
        }
        // Both versions, side by side, on the same row — which is what makes
        // them comparable without counting lines.
        let row = draw(&state, 130, 26)
            .into_iter()
            .find(|r| r.contains("timeout = 45"))
            .expect("the left version is drawn");
        assert!(row.contains("timeout = 60"), "the two versions are not aligned: {row}");
    }

    /// In a replay the panes say outright that the right-hand one is your own
    /// commit — the inversion behind most conflicts resolved backwards.
    #[test]
    fn a_rebase_says_which_pane_is_your_own_work() {
        let Some(state) = rebasing() else { return };
        let body = draw(&state, 130, 26).join("\n");
        assert!(body.contains("landing here"), "{body}");
        assert!(body.contains("Your commit"), "{body}");
        assert!(body.contains("feature"), "it has to name the branch: {body}");
    }

    /// A taken side is marked filled and one that is out is hollow, so what is
    /// going into the file reads down the column without checking anything.
    #[test]
    fn the_marks_say_which_side_is_going_in() {
        let Some(mut state) = resolving() else { return };
        let before = draw(&state, 130, 26).join("\n");
        assert!(!before.contains(MARK_IN), "nothing is chosen yet");
        assert_eq!(before.matches(MARK_OUT).count(), 2, "one hollow mark per pane");

        if let Some(f) = state.current_mut() {
            f.choose(0, Choice::only(Side::Left));
        }
        let after = draw(&state, 130, 26).join("\n");
        assert_eq!(after.matches(MARK_IN).count(), 1, "exactly one side is in");
        assert_eq!(after.matches(MARK_OUT).count(), 1);
    }

    /// The result strip answers "so what does that give me" — which two panes
    /// of alternatives do not, least of all for *both*, whose result is a thing
    /// neither pane contains.
    #[test]
    fn the_result_strip_shows_what_would_be_written() {
        let Some(mut state) = resolving() else { return };
        if let Some(f) = state.current_mut() {
            f.choose(0, Choice::Sides { left: true, right: true });
        }
        let rows = draw(&state, 130, 26);
        let start = rows.iter().position(|r| r.contains("result —")).expect("the strip");
        let strip = rows[start..].join("\n");
        assert!(strip.contains("timeout = 45") && strip.contains("timeout = 60"));
        assert!(strip.contains("Enter writes"), "it has to say what to press");
    }

    /// The ancestor is a third pane, and it is never one of the things that
    /// can be taken — so it carries no switch.
    #[test]
    fn the_ancestor_pane_shows_the_original_and_offers_no_switch() {
        let Some(mut state) = resolving() else { return };
        state.show_base = true;
        let body = draw(&state, 130, 22).join("\n");
        assert!(body.contains("The common ancestor"), "{body}");
        assert!(body.contains("timeout = 30"), "the original value: {body}");
        // Two switch marks, not three: the ancestor has none.
        assert_eq!(body.matches(MARK_OUT).count(), 2);
    }

    /// A terminal too narrow to split stacks the panes, and everything is
    /// still there.
    #[test]
    fn a_narrow_terminal_stacks_the_panes_and_loses_nothing() {
        let Some(state) = resolving() else { return };
        let rows = draw(&state, 50, 30);
        let body = rows.join("\n");
        assert!(body.contains("On main"), "{body}");
        assert!(body.contains("Merging in feature"), "{body}");
        assert!(body.contains("timeout = 45") && body.contains("timeout = 60"));
        assert!(
            rows.iter().all(|r| r.chars().count() == 50),
            "something drew outside the terminal"
        );
    }

    /// A screen far too small must clip rather than panic — the resolver opens
    /// automatically when an apply conflicts, so it can arrive at any size.
    #[test]
    fn drawing_into_a_screen_far_too_small_clips_instead_of_panicking() {
        let Some(mut state) = resolving() else { return };
        for (w, h) in [(10, 4), (20, 6), (4, 20), (1, 1)] {
            state.show_base = w % 3 == 0;
            let _ = draw(&state, w, h);
        }
    }

}
