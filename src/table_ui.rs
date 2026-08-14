//! Renderer for the tabular data view.
//!
//! Every cell is exactly one row tall and no wider than its column: values are
//! flattened and truncated by [`layout::fit_cell`], never wrapped.  That is the
//! whole trick behind keeping a table of long free-text values readable — a
//! single paragraph-length value can't push the rest of the grid off screen, and
//! the truncated text stays reachable through the cell peek / cell buffer.
//!
//! All geometry comes from [`crate::table::layout`], which the scroll math in
//! `exec::table` also uses; the two must agree column-for-column and row-for-row.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    config::TableConfig,
    exec::table::Session,
    table::{layout, summary, TableSource},
    theme,
};

/// Draw the grid for `session` into `area`.
pub fn render(frame: &mut Frame, area: Rect, session: &Session, cfg: &TableConfig) {
    let th = theme::active();
    let source = session.source();
    let state = &session.state;

    if source.columns().is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "  (no columns — the file has no header row to read)",
            Style::default().fg(th.dim),
        )));
        frame.render_widget(msg, area);
        return;
    }

    let geom = layout::compute(source, state.scroll_col, area.width, cfg);
    let header_rows = layout::header_rows(cfg);
    let rows_visible = layout::visible_rows(area.height, cfg);

    // --- header ---
    frame.render_widget(
        Paragraph::new(header_line(&geom, source, area.width)),
        Rect { height: layout::NAME_ROWS.min(area.height), ..area },
    );
    // Second header row: each numeric column's distribution, drawn from the
    // summary the session has already computed (never computed here — the
    // renderer must not scan the data).
    if cfg.column_sparkline && area.height > layout::NAME_ROWS {
        frame.render_widget(
            Paragraph::new(sparkline_line(&geom, session, area.width)),
            Rect { y: area.y + layout::NAME_ROWS, height: 1, ..area },
        );
    }

    // --- data rows ---
    let total = source.loaded_rows();
    for screen_row in 0..rows_visible {
        let row = state.scroll_row + screen_row;
        if row >= total {
            break;
        }
        let y = area.y + header_rows + screen_row as u16;
        if y >= area.y + area.height {
            break;
        }
        let line = data_line(&geom, source, state, cfg, row, area.width);
        frame.render_widget(
            Paragraph::new(line),
            Rect { y, height: 1, ..area },
        );
    }
}

/// The column-name header, on its own background so it reads as a fixed row
/// even when the grid scrolls under it.
fn header_line<'a>(geom: &layout::Layout, source: &'a dyn TableSource, width: u16) -> Line<'a> {
    let th = theme::active();
    let style = Style::default()
        .bg(th.table_header_bg)
        .fg(th.table_header)
        .add_modifier(Modifier::BOLD);

    let mut spans = vec![Span::styled(" ".repeat(geom.gutter as usize), style)];
    let cols = source.columns();
    for vis in &geom.columns {
        let col = cols.get(vis.idx);
        let name = col.map(|c| c.name.as_str()).unwrap_or("");
        // A numeric column's header is right-aligned like its values, so the
        // label sits over the digits instead of drifting left of them.
        let right_align = col.is_some_and(|c| c.ty.is_numeric());
        let (text, truncated) = layout::fit_cell(name, vis.width as usize);
        push_padded(&mut spans, text, truncated, vis.width, right_align, style, style);
        spans.push(Span::styled(" ".repeat(layout::COL_GAP as usize), style));
    }
    pad_to_width(&mut spans, width, style);
    Line::from(spans)
}

/// The distribution row under the column names.
///
/// Only numeric columns have anything to draw; a text column's slot is blank
/// rather than filled with something meaningless (the order of category counts
/// is arbitrary, so a "distribution" of them would be a shape the data doesn't
/// have).  A column whose summary hasn't been computed yet is also blank — it
/// arrives on the next frame.
fn sparkline_line<'a>(geom: &layout::Layout, session: &'a Session, width: u16) -> Line<'a> {
    let th = theme::active();
    let bg = Style::default().bg(th.table_header_bg);
    let bar = bg.fg(th.table_sparkline);

    let mut spans = vec![Span::styled(" ".repeat(geom.gutter as usize), bg)];
    for vis in &geom.columns {
        let glyphs = session
            .summary(vis.idx)
            .map(|s| summary::sparkline(&s.hist, vis.width as usize))
            .unwrap_or_default();
        let pad = (vis.width as usize).saturating_sub(layout::display_width(&glyphs));
        spans.push(Span::styled(glyphs, bar));
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), bg));
        }
        spans.push(Span::styled(" ".repeat(layout::COL_GAP as usize), bg));
    }
    pad_to_width(&mut spans, width, bg);
    Line::from(spans)
}

/// One data row: row number in the gutter, then each visible cell.
fn data_line<'a>(
    geom: &layout::Layout,
    source: &'a dyn TableSource,
    state: &crate::table::TableState,
    cfg: &TableConfig,
    row: usize,
    width: u16,
) -> Line<'a> {
    let th = theme::active();
    let on_cursor_row = row == state.cursor_row;
    // Three distinguishable levels: plain row, cursor row, cursor cell.
    let row_style = if on_cursor_row {
        Style::default().bg(th.table_row_bg)
    } else {
        Style::default()
    };

    let mut spans = Vec::new();
    if geom.gutter > 0 {
        let label = format!(
            "{:>width$} ",
            row + 1,
            width = geom.gutter.saturating_sub(1) as usize
        );
        let style = if on_cursor_row {
            row_style.fg(th.table_header).add_modifier(Modifier::BOLD)
        } else {
            row_style.fg(th.table_grid)
        };
        spans.push(Span::styled(label, style));
    }

    let cols = source.columns();
    for vis in &geom.columns {
        let on_cursor = on_cursor_row && vis.idx == state.cursor_col;
        let raw = source.cell(row, vis.idx).unwrap_or("");
        let numeric = cols.get(vis.idx).is_some_and(|c| c.ty.is_numeric());

        // An empty value is drawn as the configured stand-in, dimmed — an empty
        // cell and a cell containing whitespace should not look identical.
        let (raw, empty) = if raw.is_empty() {
            (cfg.null_display.as_str(), true)
        } else {
            (raw, false)
        };

        let mut style = row_style;
        if on_cursor {
            style = style.bg(th.table_cursor_bg).fg(theme::contrast_fg(th.table_cursor_bg));
        } else if empty {
            style = style.fg(th.table_null);
        } else if numeric {
            style = style.fg(th.table_numeric);
        } else if let Some(fg) = th.foreground {
            style = style.fg(fg);
        }
        // The ellipsis keeps its own colour unless the cursor cell has taken the
        // whole cell's foreground (where a dim marker would vanish).
        let ellipsis_style = if on_cursor {
            style
        } else {
            style.fg(th.table_truncation)
        };

        let (text, truncated) = layout::fit_cell(raw, vis.width as usize);
        push_padded(
            &mut spans,
            text,
            truncated,
            vis.width,
            numeric && !empty,
            style,
            ellipsis_style,
        );
        spans.push(Span::styled(" ".repeat(layout::COL_GAP as usize), row_style));
    }
    // Extend the cursor-row tint to the right edge so the row reads as one band.
    pad_to_width(&mut spans, width, row_style);
    Line::from(spans)
}

/// Push `text` padded to exactly `width` display columns.
///
/// `right_align` puts the padding first (numeric columns, so digits line up).
/// A truncated value's final `…` is pushed as its own span so it can be
/// coloured differently from the value.
fn push_padded(
    spans: &mut Vec<Span<'static>>,
    text: String,
    truncated: bool,
    width: u16,
    right_align: bool,
    style: Style,
    ellipsis_style: Style,
) {
    let pad = (width as usize).saturating_sub(layout::display_width(&text));
    if right_align && pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), style));
    }
    if truncated {
        // Split the marker off the end (it is always the last char).
        let mut body = text;
        body.pop();
        spans.push(Span::styled(body, style));
        spans.push(Span::styled(layout::ELLIPSIS.to_string(), ellipsis_style));
    } else {
        spans.push(Span::styled(text, style));
    }
    if !right_align && pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), style));
    }
}

/// Fill the rest of the line with `style`, so a row's background reaches the
/// right edge instead of stopping at the last column.
fn pad_to_width(spans: &mut Vec<Span<'static>>, width: u16, style: Style) {
    let used: usize = spans.iter().map(|s| layout::display_width(&s.content)).sum();
    if (width as usize) > used {
        spans.push(Span::styled(" ".repeat(width as usize - used), style));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn session(text: &str) -> Session {
        let src = crate::table::csv::CsvSource::from_reader(
            text.as_bytes(),
            b',',
            &TableConfig::default(),
        )
        .unwrap();
        Session::new(crate::source::SourceId::of(std::path::Path::new("t.csv")), Box::new(src))
    }

    /// Render into a fixed-size test backend and return the screen as lines.
    fn draw(session: &Session, cfg: &TableConfig, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), session, cfg))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The default config with the distribution row **off**: these tests are
    /// about where the grid draws its cells, and the sparkline row is covered
    /// separately (`the_sparkline_row_sits_under_the_names_and_costs_a_row`).
    fn cfg() -> TableConfig {
        TableConfig {
            row_numbers: true,
            min_col_width: 3,
            max_col_width: 10,
            column_sparkline: false,
            ..Default::default()
        }
    }

    #[test]
    fn draws_header_then_rows_with_row_numbers() {
        let s = session("name,n\nada,1\ngrace,2\n");
        let lines = draw(&s, &cfg(), 30, 4);
        // `name` is text (left-aligned); `n` is numeric, so its header is
        // right-aligned over the digits below it.
        assert_eq!(lines[0].trim(), "name    n");
        assert!(lines[1].starts_with("1 ada"), "got {:?}", lines[1]);
        assert!(lines[2].starts_with("2 grace"), "got {:?}", lines[2]);
    }

    #[test]
    fn a_long_value_is_truncated_and_never_spills_onto_another_row() {
        let s = session("note,n\n\"a very long note that would otherwise wrap\",1\n");
        let lines = draw(&s, &cfg(), 30, 5);
        // Row 1 holds the whole (truncated) value; row 2 is empty, not a
        // continuation — one logical row is always one screen row.
        assert!(lines[1].contains('…'), "expected truncation in {:?}", lines[1]);
        assert!(!lines[1].contains("otherwise"));
        assert_eq!(lines[2], "", "the value must not wrap onto the next row");
    }

    #[test]
    fn a_multiline_value_stays_on_one_row() {
        let s = session("note\n\"first\nsecond\"\n");
        let lines = draw(&s, &cfg(), 30, 4);
        assert!(lines[1].contains('↵'), "got {:?}", lines[1]);
        assert!(!lines[2].contains("second"));
    }

    #[test]
    fn numeric_columns_are_right_aligned() {
        let s = session("n\n1\n1000\n");
        let cfg = TableConfig {
            row_numbers: false,
            min_col_width: 4,
            ..cfg()
        };
        let lines = draw(&s, &cfg, 12, 4);
        // Digits line up at the column's right edge.
        assert_eq!(lines[1], "   1");
        assert_eq!(lines[2], "1000");
    }

    #[test]
    fn scrolled_rows_start_from_the_scroll_anchor() {
        let mut s = session("n\n0\n1\n2\n3\n4\n5\n");
        s.state.scroll_row = 4;
        s.state.cursor_row = 4;
        let lines = draw(&s, &cfg(), 12, 3);
        // Gutter shows the absolute row number (5), and the numeric value 4 is
        // right-aligned within its 3-column width.
        assert_eq!(lines[1], "5   4");
    }

    #[test]
    fn rendering_stops_at_the_last_row_without_panicking() {
        // A viewport taller than the data must not read past the end.
        let s = session("n\n1\n");
        let lines = draw(&s, &cfg(), 12, 10);
        assert!(lines[2..].iter().all(|l| l.is_empty()));
    }

    #[test]
    fn a_table_with_no_columns_says_so() {
        let s = session("");
        let lines = draw(&s, &cfg(), 40, 3);
        assert!(lines[0].contains("no columns"), "got {:?}", lines[0]);
    }

    #[test]
    fn the_sparkline_row_sits_under_the_names_and_costs_a_row() {
        let s = session("city,n\noslo,1\nlima,5\nbern,9\n");
        let on = TableConfig { column_sparkline: true, ..cfg() };
        // The summary cache is filled by the exec layer, not the renderer.
        let mut s = s;
        s.ensure_summaries_for_test([0, 1]);

        let lines = draw(&s, &on, 30, 6);
        assert!(lines[0].contains("city"), "names stay on the first row: {:?}", lines[0]);
        // Second row: block glyphs for the numeric column only. A text column
        // has no distribution, so its slot is blank rather than misleading.
        let bars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        assert!(
            lines[1].chars().any(|c| bars.contains(&c)),
            "expected distribution glyphs, got {:?}",
            lines[1],
        );
        assert!(!lines[1].contains("oslo"), "data must not start here: {:?}", lines[1]);
        // Data begins one row lower than it would without the sparkline.
        assert!(lines[2].starts_with("1 oslo"), "got {:?}", lines[2]);

        // ...and with it off, the same table draws data immediately under the names.
        let lines = draw(&s, &cfg(), 30, 6);
        assert!(lines[1].starts_with("1 oslo"), "got {:?}", lines[1]);
    }

    #[test]
    fn a_column_without_a_computed_summary_draws_a_blank_slot() {
        // The renderer never scans the data: a summary that hasn't been computed
        // yet is simply absent this frame, and must not panic or fabricate one.
        let s = session("n\n1\n2\n");
        let on = TableConfig { column_sparkline: true, ..cfg() };
        let lines = draw(&s, &on, 20, 5);
        assert_eq!(lines[1], "", "no summary cached, so nothing to draw");
        assert!(lines[2].starts_with("1"), "data still begins after the header");
    }

    #[test]
    fn every_drawn_row_fits_the_viewport_width() {
        // Guards the padding math: a row that overruns the width would push
        // ratatui into truncating mid-cell and misalign the grid.
        let s = session("a,b,c\nxx,yyyy,zzzzzz\n");
        for w in 6u16..40 {
            let mut terminal = Terminal::new(TestBackend::new(w, 3)).unwrap();
            terminal
                .draw(|f| render(f, f.area(), &s, &cfg()))
                .unwrap();
        }
    }
}

