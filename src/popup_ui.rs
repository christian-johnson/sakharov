use ratatui::{
    buffer::Buffer as RatBuffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, Widget},
    Frame,
};

use crate::popup::{match_positions, KeyHintsState, Popup, PopupAnchor, PopupContent, PopupSize};

/// Render a popup on top of the current frame.
pub fn render(
    frame: &mut Frame,
    popup: &Popup,
    cursor_screen: Option<(u16, u16)>,
    ui_config: &crate::config::UiConfig,
) {
    let term = frame.area();
    if term.width < 10 || term.height < 4 {
        return;
    }

    let popup_width = compute_width(popup, term.width);

    let popup_height = compute_height(popup, ui_config);

    let popup_rect = compute_rect(popup, term, popup_width, popup_height, cursor_screen);

    // Draw shadow (offset 1,1, clipped to terminal).
    let shadow_x = popup_rect.x.saturating_add(1).min(term.right().saturating_sub(1));
    let shadow_y = popup_rect.y.saturating_add(1).min(term.bottom().saturating_sub(1));
    let shadow_w = popup_rect
        .width
        .min(term.right().saturating_sub(shadow_x));
    let shadow_h = popup_rect
        .height
        .min(term.bottom().saturating_sub(shadow_y));
    if shadow_w > 0 && shadow_h > 0 {
        let shadow_rect = Rect {
            x: shadow_x,
            y: shadow_y,
            width: shadow_w,
            height: shadow_h,
        };
        frame.render_widget(FilledRect { style: Style::default().bg(Color::Rgb(8, 8, 8)) }, shadow_rect);
    }

    match &popup.content {
        PopupContent::List(state) => {
            render_list_popup(frame, popup, state, popup_rect);
        }
        PopupContent::Text(state) => {
            render_text_popup(frame, popup, state, popup_rect);
        }
        PopupContent::KeyHints(state) => {
            render_key_hints_popup(frame, state, popup_rect);
        }
    }
}

// ---------------------------------------------------------------------------
// Width / height / rect helpers
// ---------------------------------------------------------------------------

fn compute_width(popup: &Popup, term_width: u16) -> u16 {
    match popup.width {
        PopupSize::Fixed(n) => n.min(term_width),
        PopupSize::FractionOfScreen(f) => {
            let w = (term_width as f32 * f) as u16;
            w.max(20).min(term_width)
        }
        PopupSize::Auto => {
            // Compute from content.
            let natural = match &popup.content {
                PopupContent::List(s) => s
                    .items
                    .iter()
                    .map(|item| {
                        let mut w = item.label.len() + 2; // prefix
                        if let Some(ref d) = item.detail {
                            w += d.len() + 2;
                        }
                        if let Some(ref k) = item.kind {
                            w += k.len() + 1;
                        }
                        w
                    })
                    .max()
                    .unwrap_or(20),
                PopupContent::Text(s) => {
                    s.lines.iter().map(|l| l.len()).max().unwrap_or(20) + 4
                }
                PopupContent::KeyHints(s) => key_hints_natural_width(s),
            } as u16;
            natural.max(20).min(term_width.saturating_sub(4))
        }
    }
}

fn compute_height(popup: &Popup, ui_config: &crate::config::UiConfig) -> u16 {
    match &popup.content {
        PopupContent::List(s) => {
            let filtered = s.filtered_indices().len();
            let items_shown = filtered.min(ui_config.completion_list_height as usize) as u16;
            // 2 borders + 1 filter row + 1 separator + items
            (2 + 1 + 1 + items_shown).min(22).max(5)
        }
        PopupContent::Text(s) => {
            let lines_shown = s.lines.len().min(ui_config.doc_popup_height as usize) as u16;
            (2 + lines_shown).max(4)
        }
        PopupContent::KeyHints(s) => (s.hints.len() as u16 + 2).max(3),
    }
}

fn compute_rect(
    popup: &Popup,
    term: Rect,
    pw: u16,
    ph: u16,
    cursor_screen: Option<(u16, u16)>,
) -> Rect {
    let (x, y, w, h) = match popup.anchor {
        PopupAnchor::Center => {
            let x = term.x + (term.width.saturating_sub(pw)) / 2;
            let y = term.y + (term.height.saturating_sub(ph)) / 2;
            (x, y, pw, ph)
        }
        PopupAnchor::CursorBelow => {
            let (cx, cy) = cursor_screen.unwrap_or((term.x, term.y));
            let x = cx.min(term.right().saturating_sub(pw));
            let y_below = cy.saturating_add(1);
            let y = if y_below + ph > term.bottom() {
                cy.saturating_sub(ph)
            } else {
                y_below
            };
            (x, y, pw, ph)
        }
        PopupAnchor::BottomRight => {
            // Sit just above the 2-row status/command bar, flush to the right margin.
            let margin_right: u16 = 2;
            let margin_bottom: u16 = 3; // clears status bar + command line
            let x = term.right().saturating_sub(pw + margin_right);
            let y = term.bottom().saturating_sub(ph + margin_bottom);
            (x, y, pw, ph)
        }
        PopupAnchor::BottomStrip => {
            let y = term.bottom().saturating_sub(ph);
            (term.x, y, term.width, ph)
        }
    };

    // Clamp to terminal area.
    let x = x.max(term.x).min(term.right().saturating_sub(1));
    let y = y.max(term.y).min(term.bottom().saturating_sub(1));
    let w = w.min(term.right().saturating_sub(x));
    let h = h.min(term.bottom().saturating_sub(y));

    Rect { x, y, width: w.max(1), height: h.max(1) }
}

// ---------------------------------------------------------------------------
// List popup
// ---------------------------------------------------------------------------

fn render_list_popup(
    frame: &mut Frame,
    popup: &Popup,
    state: &crate::popup::ListState,
    rect: Rect,
) {
    // Draw the border block.
    let block = build_block(popup);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let buf = frame.buffer_mut();

    // Row 0: filter input "  > {filter}" or navigation hint
    let filter_y = inner.top();
    {
        let mut x = inner.left();
        // Clear row background
        for col in inner.left()..inner.right() {
            buf[(col, filter_y)]
                .set_char(' ')
                .set_style(Style::default().bg(Color::Rgb(28, 28, 40)));
        }
        if state.navigating {
            // Navigation mode: show a hint instead of the cursor
            let hint = format!("  j/k navigate · i to type · esc to close · {}", state.filter);
            for c in hint.chars() {
                if x >= inner.right() { break; }
                buf[(x, filter_y)]
                    .set_char(c)
                    .set_style(Style::default().fg(Color::DarkGray).bg(Color::Rgb(28, 28, 40)));
                x += 1;
            }
        } else {
            let prefix = "> ";
            // Draw prefix
            for c in prefix.chars() {
                if x >= inner.right() { break; }
                buf[(x, filter_y)]
                    .set_char(c)
                    .set_style(Style::default().fg(Color::DarkGray).bg(Color::Rgb(28, 28, 40)));
                x += 1;
            }
            // Draw filter text
            for c in state.filter.chars() {
                if x >= inner.right() { break; }
                buf[(x, filter_y)]
                    .set_char(c)
                    .set_style(Style::default().fg(Color::White).bg(Color::Rgb(28, 28, 40)));
                x += 1;
            }
            // Cursor block
            if x < inner.right() {
                buf[(x, filter_y)]
                    .set_char(' ')
                    .set_style(
                        Style::default()
                            .fg(Color::Rgb(28, 28, 40))
                            .bg(Color::White)
                            .add_modifier(Modifier::REVERSED),
                    );
            }
        }
    }

    if inner.height < 2 {
        return;
    }

    // Row 1: separator
    let sep_y = inner.top() + 1;
    {
        let sep_style = Style::default().fg(Color::DarkGray).bg(Color::Rgb(28, 28, 40));
        for col in inner.left()..inner.right() {
            buf[(col, sep_y)].set_char('\u{2500}').set_style(sep_style);
        }
    }

    if inner.height < 3 {
        return;
    }

    // Items area starts at row 2.
    let items_top = inner.top() + 2;
    let items_height = inner.height.saturating_sub(2);
    let visible_rows = items_height as usize;

    let indices = state.filtered_indices();
    let total_filtered = indices.len();

    // Determine scroll offset so selected item is visible.
    let scroll = if state.selected >= visible_rows {
        state.selected - visible_rows + 1
    } else {
        0
    };

    // Reserve 1 col for scrollbar if needed.
    let scrollbar_width: u16 = if total_filtered > visible_rows { 1 } else { 0 };
    let item_width = inner.width.saturating_sub(scrollbar_width) as usize;

    for row in 0..visible_rows {
        let item_row = scroll + row;
        let y = items_top + row as u16;

        // Clear row
        for col in inner.left()..inner.right() {
            buf[(col, y)]
                .set_char(' ')
                .set_style(Style::default().bg(Color::Rgb(28, 28, 40)));
        }

        let Some(&item_idx) = indices.get(item_row) else {
            continue;
        };

        let item = &state.items[item_idx];
        let is_selected = item_row == state.selected;

        let row_bg = if is_selected {
            Color::Rgb(60, 60, 100)
        } else {
            Color::Rgb(28, 28, 40)
        };
        let row_fg = if is_selected {
            Color::White
        } else {
            Color::Rgb(180, 180, 180)
        };

        let base_style = Style::default().fg(row_fg).bg(row_bg);

        // Fill row with bg color
        for col in inner.left()..inner.right() {
            buf[(col, y)].set_char(' ').set_style(base_style);
        }

        let mut x = inner.left();

        // Prefix
        let prefix = if is_selected { "\u{25b8} " } else { "  " };
        for c in prefix.chars() {
            if x >= inner.right().saturating_sub(scrollbar_width) {
                break;
            }
            buf[(x, y)].set_char(c).set_style(base_style);
            x += 1;
        }

        // Optional kind badge
        if let Some(ref kind) = item.kind {
            let badge_style = Style::default().fg(Color::Cyan).bg(row_bg);
            for c in kind.chars() {
                if x >= inner.right().saturating_sub(scrollbar_width) {
                    break;
                }
                buf[(x, y)].set_char(c).set_style(badge_style);
                x += 1;
            }
            // Space after badge
            if x < inner.right().saturating_sub(scrollbar_width) {
                buf[(x, y)].set_char(' ').set_style(base_style);
                x += 1;
            }
        }

        // Label — highlight chars that matched the filter.
        let label_start = x;
        let matched: std::collections::HashSet<usize> = if state.filter.is_empty() {
            Default::default()
        } else {
            match_positions(&item.label, &state.filter)
                .unwrap_or_default()
                .into_iter()
                .collect()
        };
        let match_style = if is_selected {
            Style::default()
                .fg(Color::Rgb(255, 225, 80))
                .bg(row_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Rgb(255, 175, 0))
                .bg(row_bg)
                .add_modifier(Modifier::BOLD)
        };
        for (char_idx, c) in item.label.chars().enumerate() {
            if x >= inner.right().saturating_sub(scrollbar_width) {
                break;
            }
            let style = if matched.contains(&char_idx) { match_style } else { base_style };
            buf[(x, y)].set_char(c).set_style(style);
            x += 1;
        }
        let label_end = x;

        // Detail (right-aligned)
        if let Some(ref detail) = item.detail {
            let detail_style = Style::default().fg(Color::Rgb(100, 100, 100)).bg(row_bg);
            let max_detail_right = inner.right().saturating_sub(scrollbar_width);
            // Available space between label end and right edge
            let avail = max_detail_right.saturating_sub(label_end + 2);
            if avail > 0 && !detail.is_empty() {
                let detail_chars: Vec<char> = detail.chars().collect();
                let show_len = (avail as usize).min(detail_chars.len());
                let start_x = max_detail_right.saturating_sub(show_len as u16);
                let draw_start = start_x.max(label_end + 2);
                let skip = show_len.saturating_sub((max_detail_right.saturating_sub(draw_start)) as usize);
                let mut dx = draw_start;
                for c in detail_chars.iter().skip(skip) {
                    if dx >= max_detail_right {
                        break;
                    }
                    buf[(dx, y)].set_char(*c).set_style(detail_style);
                    dx += 1;
                }
            }
        }
        let _ = (label_start, item_width);
    }

    // Scrollbar
    if scrollbar_width > 0 && total_filtered > 0 {
        let sb_x = inner.right() - 1;
        let track_h = items_height as usize;
        let thumb_h = ((track_h * visible_rows) / total_filtered).max(1).min(track_h);
        let thumb_top = (scroll * track_h) / total_filtered;
        for row in 0..track_h {
            let sy = items_top + row as u16;
            let in_thumb = row >= thumb_top && row < thumb_top + thumb_h;
            let sc = if in_thumb { '\u{2588}' } else { '\u{2502}' };
            let sb_style = Style::default()
                .fg(Color::Rgb(100, 100, 180))
                .bg(Color::Rgb(28, 28, 40));
            buf[(sb_x, sy)].set_char(sc).set_style(sb_style);
        }
    }
}

// ---------------------------------------------------------------------------
// Text popup
// ---------------------------------------------------------------------------

fn render_text_popup(
    frame: &mut Frame,
    popup: &Popup,
    state: &crate::popup::TextState,
    rect: Rect,
) {
    let total_lines = state.lines.len();
    let block = build_block(popup);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let buf = frame.buffer_mut();
    let visible_rows = inner.height as usize;

    for row in 0..visible_rows {
        let line_idx = state.scroll + row;
        let y = inner.top() + row as u16;

        // Clear row
        for col in inner.left()..inner.right() {
            buf[(col, y)]
                .set_char(' ')
                .set_style(Style::default().bg(Color::Rgb(28, 28, 40)));
        }

        let Some(line) = state.lines.get(line_idx) else {
            continue;
        };

        let mut x = inner.left();
        for c in line.chars() {
            if x >= inner.right() {
                break;
            }
            buf[(x, y)]
                .set_char(c)
                .set_style(Style::default().fg(Color::Rgb(200, 200, 200)).bg(Color::Rgb(28, 28, 40)));
            x += 1;
        }
    }

    // Scroll percentage in bottom-right of border.
    if total_lines > 0 && rect.height >= 2 {
        let pct = (state.scroll * 100) / total_lines.max(1);
        let pct_str = format!(" {pct}% ");
        let py = rect.bottom() - 1;
        let px_end = rect.right().saturating_sub(1);
        let px_start = px_end.saturating_sub(pct_str.len() as u16);
        let pct_style = Style::default().fg(Color::DarkGray).bg(Color::Rgb(28, 28, 40));
        let mut px = px_start;
        for c in pct_str.chars() {
            if px >= px_end {
                break;
            }
            buf[(px, py)].set_char(c).set_style(pct_style);
            px += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// KeyHints popup (BottomRight bordered window)
// ---------------------------------------------------------------------------

fn render_key_hints_popup(
    frame: &mut Frame,
    state: &KeyHintsState,
    rect: Rect,
) {
    // Build a block with the prefix as the title (e.g. " g ")
    let block = Block::default()
        .title(format!(" {} ", state.prefix))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(100, 100, 180)))
        .style(Style::default().bg(Color::Rgb(28, 28, 40)));

    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Compute the max key label width for column alignment.
    let max_key_w = state.hints.iter().map(|(k, _)| k.len()).max().unwrap_or(1);

    let key_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default()
        .fg(Color::Rgb(180, 180, 180));
    let sep_style = Style::default()
        .fg(Color::DarkGray);

    for (row, (key, desc)) in state.hints.iter().enumerate() {
        if row as u16 >= inner.height {
            break;
        }
        let y = inner.top() + row as u16;
        let mut x = inner.left();

        // 1-char left padding
        if x < inner.right() {
            frame.buffer_mut()[(x, y)].set_char(' ').set_style(Style::default());
            x += 1;
        }

        // Key (padded to max_key_w)
        for c in key.chars() {
            if x >= inner.right() { break; }
            frame.buffer_mut()[(x, y)].set_char(c).set_style(key_style);
            x += 1;
        }
        // Pad key column
        for _ in key.len()..max_key_w {
            if x >= inner.right() { break; }
            frame.buffer_mut()[(x, y)].set_char(' ').set_style(Style::default());
            x += 1;
        }

        // Separator "  →  "
        for c in "  →  ".chars() {
            if x >= inner.right() { break; }
            frame.buffer_mut()[(x, y)].set_char(c).set_style(sep_style);
            x += 1;
        }

        // Description (truncated to remaining width)
        for c in desc.chars() {
            if x >= inner.right() { break; }
            frame.buffer_mut()[(x, y)].set_char(c).set_style(desc_style);
            x += 1;
        }
    }
}

/// Natural width of a KeyHints popup: wide enough to fit the widest row.
fn key_hints_natural_width(s: &KeyHintsState) -> usize {
    let max_key = s.hints.iter().map(|(k, _)| k.len()).max().unwrap_or(1);
    let max_desc = s.hints.iter().map(|(_, d)| d.len()).max().unwrap_or(0);
    // 1 (left pad) + max_key + 5 (separator "  →  ") + max_desc + 1 (right pad) + 2 (borders)
    1 + max_key + 5 + max_desc + 1 + 2
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn build_block(popup: &Popup) -> Block<'static> {
    let border_style = Style::default().fg(Color::Rgb(100, 100, 180));
    let bg_style = Style::default().bg(Color::Rgb(28, 28, 40));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(bg_style);
    if let Some(ref title) = popup.title {
        block.title(format!(" {title} "))
    } else {
        block
    }
}

// ---------------------------------------------------------------------------
// FilledRect widget — fills an area with a background colour
// ---------------------------------------------------------------------------

struct FilledRect {
    style: Style,
}

impl Widget for FilledRect {
    fn render(self, area: Rect, buf: &mut RatBuffer) {
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].set_char(' ').set_style(self.style);
            }
        }
    }
}
