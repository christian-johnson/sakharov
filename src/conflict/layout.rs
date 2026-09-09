//! THE geometry model for the resolver — the view's `table::layout`.
//!
//! Pane rectangles, how tall each region is, and which rows are on screen all
//! come from here, so the renderer and the scroll math cannot disagree about
//! what the cursor is on.  A resolver whose highlight and whose cursor point
//! at different hunks is worse than no resolver: every keypress then answers a
//! question about something other than what is lit up.

use ratatui::layout::Rect;

use super::{hunk::Region, ConflictFile};

/// Narrowest a side pane may be before stacking is better than splitting.
///
/// Below this a pane holds a couple of words per line, and two columns of
/// hard-wrapped fragments are harder to compare than the same text one above
/// the other.
pub const MIN_PANE: u16 = 28;

/// Rows a region's header costs (the `▸ 2/5` marker line).
const HEADER_ROWS: usize = 1;

/// How the panes are arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrangement {
    /// Side by side — the default, and what makes a conflict comparable at a
    /// glance.
    Columns,
    /// One above the other, on a terminal too narrow to give each side a
    /// readable column.  The same decision `table::layout` makes about column
    /// widths, made in one place for the same reason.
    Stacked,
}

/// Where everything goes.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The two version panes, plus the base when it is shown.
    pub left: Rect,
    pub right: Rect,
    pub base: Option<Rect>,
    /// The merged-result strip along the bottom.
    pub result: Rect,
    /// Each region's row span within the panes' scrolling content, in
    /// `regions` order.  A conflicted region and an agreed one are both here:
    /// the agreed text is context, and scrolling past it is how you read the
    /// conflict in place rather than as an extract.
    pub spans: Vec<Span>,
    /// Total content rows.
    pub total: usize,
}

/// One region's place in the scrolling content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub height: usize,
    /// `Some(n)` when this is the `n`th **conflicted** region — which is the
    /// index into `ConflictFile::choices`, and the only numbering the user
    /// ever sees.
    pub nth: Option<usize>,
}

impl Span {
    pub fn end(self) -> usize {
        self.start + self.height
    }

}

/// Rows the result strip takes, including its border.
const RESULT_ROWS: u16 = 6;

/// Lay `file` out into `area`.
///
/// `with_base` is the `3` toggle.  It is off by default because it costs a
/// third of the width on every conflict, including the many where both sides
/// are plainly different intents and the ancestor adds nothing.
pub fn compute(area: Rect, file: &ConflictFile, with_base: bool) -> Layout {
    let panes = panes(with_base);
    let arrangement = arrangement(area.width, with_base);

    // The result strip only earns its rows when there is height to spare; on a
    // short terminal the conflict itself is what must be readable.
    let result_rows = if area.height > RESULT_ROWS * 2 { RESULT_ROWS } else { 0 };
    let body = Rect { height: area.height.saturating_sub(result_rows), ..area };
    let result = Rect {
        x: area.x,
        y: area.y + body.height,
        width: area.width,
        height: result_rows,
    };

    let (left, base, right) = match arrangement {
        Arrangement::Columns => {
            let each = body.width / panes;
            let cut = |i: u16| Rect {
                x: body.x + each * i,
                // The last pane takes the remainder, so the panes always fill
                // the width exactly rather than leaving a ragged column.
                width: if i == panes - 1 { body.width - each * i } else { each },
                ..body
            };
            match with_base {
                true => (cut(0), Some(cut(1)), cut(2)),
                false => (cut(0), None, cut(1)),
            }
        }
        Arrangement::Stacked => {
            let each = body.height / panes;
            let cut = |i: u16| Rect {
                y: body.y + each * i,
                height: if i == panes - 1 { body.height - each * i } else { each },
                ..body
            };
            match with_base {
                true => (cut(0), Some(cut(1)), cut(2)),
                false => (cut(0), None, cut(1)),
            }
        }
    };

    let (spans, total) = spans_for(file, with_base);
    Layout { left, right, base, result, spans, total }
}

/// How many panes are on screen.
fn panes(with_base: bool) -> u16 {
    if with_base {
        3
    } else {
        2
    }
}

/// Columns or stacked, for a given width.
///
/// **The** rule, asked by [`compute`] and by the motion keys — which have to
/// know whether `h`/`l` choose a pane or walk the file, and would otherwise be
/// a second copy of this comparison that could disagree with what is drawn.
pub fn arrangement(width: u16, with_base: bool) -> Arrangement {
    match width < MIN_PANE * panes(with_base) {
        true => Arrangement::Stacked,
        false => Arrangement::Columns,
    }
}

/// Every region's row span, and the total height.
///
/// Shared by the renderer and the scroll math — which is the whole point of
/// this module.  A region is as tall as its tallest side, so the two panes
/// stay **line-aligned**: the left version and the right version of the same
/// hunk are always on the same screen row, which is what makes them
/// comparable without counting lines.
pub fn spans_for(file: &ConflictFile, with_base: bool) -> (Vec<Span>, usize) {
    let mut spans = Vec::with_capacity(file.regions.len());
    let mut row = 0;
    let mut nth = 0;
    for region in &file.regions {
        let (height, index) = match region {
            Region::Agreed(text) => (super::hunk::Versions::lines(text).len(), None),
            Region::Conflict(v) => {
                let n = nth;
                nth += 1;
                (v.height(with_base) + HEADER_ROWS, Some(n))
            }
        };
        spans.push(Span { start: row, height, nth: index });
        row += height;
    }
    (spans, row)
}

impl Layout {
    /// The span of the `nth` conflicted region.
    pub fn span_of(&self, nth: usize) -> Option<Span> {
        self.spans.iter().copied().find(|s| s.nth == Some(nth))
    }

    /// How many content rows a pane can show.
    pub fn pane_rows(&self) -> usize {
        // Minus the two border rows and the two header rows every pane draws.
        (self.left.height as usize).saturating_sub(4)
    }

    /// Scroll offset that brings the `nth` conflict into view, given where the
    /// panes are scrolled now.
    ///
    /// Nudges by the minimum needed rather than centring: a conflict two rows
    /// below the fold should scroll two rows, or reading a file top to bottom
    /// makes the text jump under you at every hunk.  A hunk taller than the
    /// pane is pinned to its top, since its start is the part with the header
    /// saying which hunk it is.
    pub fn scroll_to(&self, nth: usize, current: usize) -> usize {
        let Some(span) = self.span_of(nth) else { return current };
        let rows = self.pane_rows().max(1);
        if span.start < current {
            return span.start;
        }
        if span.end() > current + rows {
            return span.end().saturating_sub(rows).min(span.start);
        }
        current
    }
}

#[cfg(test)]
mod tests {
    use super::super::{hunk::Versions, Choice};
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect { x: 0, y: 0, width, height }
    }

    fn file(regions: Vec<Region>) -> ConflictFile {
        let conflicts = regions.iter().filter(|r| r.is_conflict()).count();
        ConflictFile {
            path: "f.txt".into(),
            base: None,
            left: None,
            right: None,
            regions,
            choices: vec![Choice::default(); conflicts],
            history: Vec::new(),
            resolved: false,
        }
    }

    fn sample() -> ConflictFile {
        file(vec![
            Region::Agreed("a\nb\n".into()),
            Region::Conflict(Versions {
                base: "1\n".into(),
                left: "1\n2\n".into(),
                right: "1\n".into(),
            }),
            Region::Agreed("z\n".into()),
        ])
    }

    /// A region is as tall as its tallest side, so the same hunk sits on the
    /// same rows in both panes.  Line-alignment is what makes two versions
    /// comparable without counting.
    #[test]
    fn a_region_is_as_tall_as_its_tallest_side() {
        let (spans, total) = spans_for(&sample(), false);
        assert_eq!(spans[0], Span { start: 0, height: 2, nth: None });
        // Two lines on the left, one on the right, plus the header row.
        assert_eq!(spans[1], Span { start: 2, height: 3, nth: Some(0) });
        assert_eq!(spans[2], Span { start: 5, height: 1, nth: None });
        assert_eq!(total, 6);
    }

    /// Showing the ancestor can only make a region taller, never shift the
    /// sides out of alignment with each other.
    #[test]
    fn showing_the_base_grows_a_region_rather_than_realigning_it() {
        let f = file(vec![Region::Conflict(Versions {
            base: "1\n2\n3\n".into(),
            left: "1\n".into(),
            right: "1\n".into(),
        })]);
        let (without, _) = spans_for(&f, false);
        let (with, _) = spans_for(&f, true);
        assert!(with[0].height > without[0].height);
        assert_eq!(with[0].start, without[0].start);
    }

    /// Panes fill the width exactly — a ragged right-hand column would leave
    /// the pane border floating a cell short of the edge.
    #[test]
    fn the_panes_always_fill_the_width() {
        for width in [80, 81, 100, 137] {
            for with_base in [false, true] {
                let l = compute(area(width, 40), &sample(), with_base);
                let last = l.right;
                assert_eq!(last.x + last.width, width, "width={width} base={with_base}");
                assert_eq!(l.left.x, 0);
            }
        }
    }

    /// Below two readable columns the panes stack instead of splitting: two
    /// columns of hard-wrapped fragments are harder to compare than the same
    /// text one above the other.
    #[test]
    fn a_terminal_too_narrow_to_split_stacks_the_panes_instead() {
        assert_eq!(arrangement(MIN_PANE * 2, false), Arrangement::Columns);
        let wide = compute(area(MIN_PANE * 2, 40), &sample(), false);
        assert_eq!(wide.left.y, wide.right.y, "columns share a top edge");

        assert_eq!(arrangement(MIN_PANE * 2 - 1, false), Arrangement::Stacked);
        let narrow = compute(area(MIN_PANE * 2 - 1, 40), &sample(), false);
        assert_eq!(narrow.left.x, narrow.right.x, "stacked panes share a left edge");
        assert!(narrow.right.y > narrow.left.y);

        // Showing the base needs a third of the width again, so the same
        // terminal that fits two columns may not fit three.
        assert_eq!(arrangement(MIN_PANE * 2, true), Arrangement::Stacked);
    }

    /// Scrolling nudges by the minimum: reading a file top to bottom must not
    /// make the text jump at every hunk.
    #[test]
    fn scrolling_to_a_conflict_moves_the_least_it_can() {
        let tall = file(vec![
            Region::Agreed("x\n".repeat(50)),
            Region::Conflict(Versions { base: "b\n".into(), left: "l\n".into(), right: "r\n".into() }),
        ]);
        let l = compute(area(120, 24), &tall, false);
        let rows = l.pane_rows();

        // Already visible: nothing moves.
        assert_eq!(l.scroll_to(0, 50), 50);
        // Below the fold: scrolled just far enough to show its end.
        let scrolled = l.scroll_to(0, 0);
        let span = l.span_of(0).unwrap();
        assert_eq!(scrolled, span.end() - rows);
        assert!(
            span.start >= scrolled && span.end() <= scrolled + rows,
            "the conflict has to sit inside the visible window"
        );
        // Above: pinned to its start, which is where its header is.
        assert_eq!(l.scroll_to(0, 60), span.start);
    }

    /// A conflict taller than the pane is pinned to its top, not its bottom:
    /// the top is where the header saying which hunk it is lives.
    #[test]
    fn a_conflict_taller_than_the_pane_is_pinned_to_its_top() {
        let huge = file(vec![Region::Conflict(Versions {
            base: String::new(),
            left: "l\n".repeat(100),
            right: "r\n".into(),
        })]);
        let l = compute(area(120, 24), &huge, false);
        assert_eq!(l.scroll_to(0, 0), 0);
    }

    /// A terminal too short for the result strip drops it rather than
    /// squeezing the conflict itself out of readability.
    #[test]
    fn a_short_terminal_drops_the_result_strip() {
        let short = compute(area(120, 10), &sample(), false);
        assert_eq!(short.result.height, 0);
        assert_eq!(short.left.height, 10, "the panes take the whole body");
    }
}
