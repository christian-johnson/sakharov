//! Drawing the version-control view: the changes tree, the plan, and the
//! selected file's diff or conflicts.  Geometry comes from
//! [`crate::vcs::layout`] and nowhere else.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders},
    Frame,
};
use unicode_width::UnicodeWidthChar;

use crate::render_util::sanitize_source;
use crate::vcs::{
    diff::Staged,
    layout::{FileDiff, Layout},
    state::{Focus, VcsState},
    Mark,
};

const TAB_WIDTH: usize = 4;

/// The glyph and colour a tree row's mark is drawn with.
fn mark_style(mark: Mark) -> (&'static str, Color) {
    let th = crate::theme::active();
    match mark {
        Mark::Conflicted => ("!", th.error),
        Mark::Untracked => ("?", th.dim),
        Mark::Staged => ("●", th.git_added),
        Mark::Unstaged => ("○", th.git_modified),
        Mark::Partly => ("◐", th.warning),
    }
}

fn staged_style(staged: Staged) -> (&'static str, Color) {
    match staged {
        Staged::Yes => mark_style(Mark::Staged),
        Staged::No => mark_style(Mark::Unstaged),
        Staged::Partly => mark_style(Mark::Partly),
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &VcsState) {
    let layout = Layout::compute(area, state.plan.len());
    tree(frame, layout.tree, state);
    plan(frame, layout.plan, state);
    match (&state.conflicts, state.showing_conflict()) {
        (Some(conflicts), true) => crate::conflict_ui::render(frame, layout.main, conflicts),
        _ => main(frame, &layout, state),
    }
}

fn panel(frame: &mut Frame, rect: Rect, title: &str, focused: bool) -> Rect {
    let th = crate::theme::active();
    let block = Block::default()
        .title(format!(" {title} "))
        .title_style(match focused {
            true => Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
            false => Style::default().fg(th.dim),
        })
        .borders(Borders::ALL)
        .border_type(if focused { BorderType::Thick } else { BorderType::Rounded })
        .border_style(Style::default().fg(if focused { th.popup_border_focus } else { th.popup_border }));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

fn tree(frame: &mut Frame, rect: Rect, state: &VcsState) {
    if rect.width < 4 || rect.height < 3 {
        return;
    }
    let th = crate::theme::active();
    let repo = state.repo();
    let mut title = repo.head_label();
    if let Some(op) = &repo.operation {
        title.push_str(&format!(" · {} in progress", op.kind.describe()));
    }
    let inner = panel(frame, rect, &title, state.focus == Focus::Tree);
    let rows = state.rows();
    if rows.is_empty() {
        put(frame, inner, 0, 1, "Nothing changed.", Style::default().fg(th.dim));
        return;
    }
    let selected = state.selected_index(&rows);
    for (y, (i, row)) in rows.iter().enumerate().skip(state.tree_scroll).take(inner.height as usize).enumerate() {
        let (glyph, color) = mark_style(row.mark);
        let here = Some(i) == selected;
        let mut base = Style::default();
        if here {
            base = base.bg(th.cell_selection_bg);
            if state.focus == Focus::Tree {
                base = base.add_modifier(Modifier::BOLD);
            }
            fill(frame, inner, y as u16, base);
        }
        let fold = match (row.dir, row.collapsed) {
            (true, true) => "▸ ",
            (true, false) => "▾ ",
            _ => "",
        };
        let name = format!("{}{fold}{}{}", "  ".repeat(row.depth), row.name, if row.dir { "/" } else { "" });
        put(frame, inner, y as u16, 0, glyph, base.fg(color));
        let name_style = if row.dir { base.fg(th.dim) } else { base };
        put(frame, inner, y as u16, 2, &name, name_style);
    }
}

fn plan(frame: &mut Frame, rect: Rect, state: &VcsState) {
    if rect.width < 4 || rect.height < 3 {
        return;
    }
    let th = crate::theme::active();
    let n = state.plan.len();
    let title = match n {
        0 => "plan".to_string(),
        n => format!("plan ({n}) — g a applies"),
    };
    let inner = panel(frame, rect, &title, state.focus == Focus::Plan);
    if n == 0 {
        put(frame, inner, 0, 1, "Nothing planned yet.", Style::default().fg(th.dim));
        return;
    }
    for (y, (i, step)) in state.plan.steps().iter().enumerate().skip(state.plan_scroll).take(inner.height as usize).enumerate() {
        let mut style = Style::default().fg(th.vcs_pending);
        if i == state.plan_cursor && state.focus == Focus::Plan {
            style = style.bg(th.cell_selection_bg).add_modifier(Modifier::BOLD);
            fill(frame, inner, y as u16, style);
        }
        put(frame, inner, y as u16, 0, &format!("{:>2} {}", i + 1, step.label()), style);
    }
}

fn main(frame: &mut Frame, layout: &Layout, state: &VcsState) {
    let th = crate::theme::active();
    let focused = state.focus == Focus::Diff;
    let Some(diff) = &state.diff else {
        let row = state.selected_row();
        let title = row.as_ref().map_or(String::new(), |r| format!("{}/", r.path));
        let inner = panel(frame, layout.main, &title, false);
        let text = match row {
            Some(row) if row.dir => "Space stages or unstages everything in it, d discards it".to_string(),
            Some(_) => String::new(),
            None if state.repo().operation.is_some() => "Every conflict is resolved — g C continues".to_string(),
            None => "The work tree matches HEAD. Nothing to stage.".to_string(),
        };
        put(frame, inner, 0, 1, &text, Style::default().fg(th.dim));
        return;
    };
    let title = match diff.changes.get(state.change) {
        Some(c) => {
            let word = match c.staged {
                Staged::Yes => "staged",
                Staged::No => "not staged",
                Staged::Partly => "partly staged",
            };
            format!("{} — change {} of {}, {word}", diff.path, state.change + 1, diff.changes.len())
        }
        None => diff.path.clone(),
    };
    let inner = panel(frame, layout.main, &title, focused);
    if inner.height < 2 {
        return;
    }
    let split = layout.split();
    let header = Style::default().fg(th.dim).add_modifier(Modifier::BOLD);
    let half = inner.width / 2;
    if split {
        put(frame, inner, 0, 0, "HEAD", header);
        put(frame, inner, 0, half + 1, "working tree", header);
    } else {
        put(frame, inner, 0, 0, "HEAD → working tree", header);
    }
    let body = Rect { y: inner.y + 1, height: inner.height - 1, ..inner };
    if let Some(note) = &diff.note {
        put(frame, body, 0, 1, note, Style::default().fg(th.dim).add_modifier(Modifier::ITALIC));
        if diff.changes.is_empty() {
            return;
        }
    }
    rows(frame, body, diff, state, split, focused);
}

fn rows(frame: &mut Frame, body: Rect, diff: &FileDiff, state: &VcsState, split: bool, focused: bool) {
    let th = crate::theme::active();
    let all = diff.rows(split);
    let digits = all.iter().filter_map(|r| r.new.as_ref().or(r.old.as_ref())).map(|l| l.no).max().unwrap_or(1).to_string().len();
    for (y, row) in all.iter().skip(state.diff_scroll).take(body.height as usize).enumerate() {
        let y = y as u16;
        let current = row.change == Some(state.change);
        let bg = (current && focused).then_some(th.cell_selection_bg);
        if let Some(bg) = bg {
            fill(frame, body, y, Style::default().bg(bg));
        }
        let with_bg = |s: Style| match bg {
            Some(bg) => s.bg(bg),
            None => s,
        };
        let changed = row.change.is_some();
        // The staged mark sits on a change's first row.
        let first = row.change.and_then(|c| diff.span(c, split)).is_some_and(|(start, _)| {
            all.get(start).is_some_and(|r| std::ptr::eq(r, row))
        });
        if first {
            if let Some(info) = row.change.and_then(|c| diff.changes.get(c)) {
                let (glyph, color) = staged_style(info.staged);
                put(frame, body, y, 0, glyph, with_bg(Style::default().fg(color)));
            }
        }
        let removed = with_bg(Style::default().fg(th.error));
        let added = with_bg(Style::default().fg(th.git_added));
        let same = with_bg(Style::default());
        if split {
            let half = body.width / 2;
            let left = Rect { x: body.x + 1, width: half.saturating_sub(1), ..body };
            let right = Rect { x: body.x + half + 1, width: body.width - half - 1, ..body };
            line(frame, left, y, row.old.as_ref(), digits, if changed { removed } else { same });
            line(frame, right, y, row.new.as_ref(), digits, if changed { added } else { same });
        } else {
            let area = Rect { x: body.x + 1, width: body.width.saturating_sub(1), ..body };
            match (row.old.as_ref(), row.new.as_ref(), changed) {
                (Some(old), None, _) => line(frame, area, y, Some(old), digits, removed),
                (_, Some(new), true) => line(frame, area, y, Some(new), digits, added),
                (_, new, _) => line(frame, area, y, new, digits, same),
            }
        }
    }
}

fn line(frame: &mut Frame, rect: Rect, y: u16, line: Option<&crate::vcs::layout::Line>, digits: usize, style: Style) {
    let th = crate::theme::active();
    let Some(line) = line else { return };
    let number = format!("{:>digits$} ", line.no);
    let gutter = Style { fg: Some(th.line_numbers), ..style };
    put(frame, rect, y, 0, &number, gutter);
    put(frame, rect, y, number.len() as u16, &sanitize_source(&line.text, TAB_WIDTH), style);
}

/// Paint a whole row of `rect` with `style`.
fn fill(frame: &mut Frame, rect: Rect, y: u16, style: Style) {
    if y >= rect.height {
        return;
    }
    for x in rect.left()..rect.right() {
        frame.buffer_mut()[(x, rect.y + y)].set_char(' ').set_style(style);
    }
}

/// Write `text` at column `x` of row `y`, clipped to `rect`, by display width.
fn put(frame: &mut Frame, rect: Rect, y: u16, x: u16, text: &str, style: Style) {
    if y >= rect.height {
        return;
    }
    let mut col = rect.x + x;
    for c in sanitize_source(text, TAB_WIDTH).chars() {
        let w = c.width().unwrap_or(0) as u16;
        if w == 0 {
            continue;
        }
        if col + w > rect.right() {
            break;
        }
        frame.buffer_mut()[(col, rect.y + y)].set_char(c).set_style(style);
        col += w;
    }
}

/// What the status line says about the selection.
pub fn describe_selection(state: &VcsState) -> String {
    let Some(row) = state.selected_row() else { return String::new() };
    let word = match row.mark {
        Mark::Conflicted => "conflicted",
        Mark::Untracked => "untracked",
        Mark::Staged => "staged",
        Mark::Unstaged => "not staged",
        Mark::Partly => "partly staged",
    };
    format!("{}{} · {word}", row.path, if row.dir { "/" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{load, testrepo::TestRepo};
    use ratatui::{backend::TestBackend, Terminal};

    fn draw(state: &mut VcsState, w: u16, h: u16) -> Vec<String> {
        let area = Rect::new(0, 0, w, h);
        state.update_scroll(area);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, area, state)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
    }

    fn state() -> Option<(TestRepo, VcsState)> {
        let repo = TestRepo::new()?;
        repo.commit("src/a.rs", "fn a() {}\nfn b() {}\n", "base");
        repo.write("src/a.rs", "fn a() {}\nfn b() { changed }\n");
        repo.write("notes.txt", "scratch\n");
        let snap = load::read(repo.path()).ok()?;
        let state = VcsState::new(repo.root.clone(), snap);
        Some((repo, state))
    }

    #[test]
    fn the_tree_the_plan_and_the_side_by_side_diff_are_all_on_screen() {
        let Some((_repo, mut state)) = state() else { return };
        state.select("src/a.rs".into());
        let screen = draw(&mut state, 140, 24).join("\n");
        assert!(screen.contains("main"), "the branch heads the tree: {screen}");
        assert!(screen.contains("src/") && screen.contains("a.rs") && screen.contains("notes.txt"));
        assert!(screen.contains("Nothing planned yet."));
        assert!(screen.contains("HEAD") && screen.contains("working tree"));
        let row = draw(&mut state, 140, 24).into_iter().find(|r| r.contains("fn b() {}")).expect("the old line");
        assert!(row.contains("fn b() { changed }"), "old and new sit on one row: {row}");
    }

    #[test]
    fn a_tiny_screen_clips_rather_than_panicking() {
        let Some((_repo, mut state)) = state() else { return };
        state.select("src/a.rs".into());
        for (w, h) in [(10, 4), (30, 6), (4, 20), (1, 1), (70, 10)] {
            let _ = draw(&mut state, w, h);
        }
    }
}
