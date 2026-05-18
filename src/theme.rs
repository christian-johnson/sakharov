use ratatui::style::{Color, Modifier, Style};

/// Map a highlight name index (from `HIGHLIGHT_NAMES`) to a ratatui `Style`.
///
/// The indices correspond to the order in `highlight::HIGHLIGHT_NAMES`.
pub fn style_for_highlight(index: usize) -> Style {
    // Names in order (must match highlight::HIGHLIGHT_NAMES):
    // 0  attribute
    // 1  comment
    // 2  constant
    // 3  constant.builtin
    // 4  constructor
    // 5  function
    // 6  function.builtin
    // 7  function.method
    // 8  keyword
    // 9  label
    // 10 namespace
    // 11 number
    // 12 operator
    // 13 property
    // 14 punctuation
    // 15 punctuation.bracket
    // 16 punctuation.delimiter
    // 17 string
    // 18 string.special
    // 19 tag
    // 20 type
    // 21 type.builtin
    // 22 variable
    // 23 variable.builtin
    // 24 variable.parameter
    match index {
        0 => Style::default().fg(Color::Yellow),                                         // attribute
        1 => Style::default().fg(Color::DarkGray),                                       // comment
        2 => Style::default().fg(Color::Yellow),                                         // constant
        3 => Style::default().fg(Color::Yellow),                                         // constant.builtin
        4 => Style::default().fg(Color::Cyan),                                           // constructor
        5 => Style::default().fg(Color::Blue),                                           // function
        6 => Style::default().fg(Color::Cyan),                                           // function.builtin
        7 => Style::default().fg(Color::Blue),                                           // function.method
        8 => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),           // keyword
        9 => Style::default().fg(Color::White),                                          // label
        10 => Style::default().fg(Color::Cyan),                                          // namespace
        11 => Style::default().fg(Color::Yellow),                                        // number
        12 => Style::default().fg(Color::White),                                         // operator
        13 => Style::default().fg(Color::White),                                         // property
        14 => Style::default().fg(Color::Gray),                                          // punctuation
        15 => Style::default().fg(Color::Gray),                                          // punctuation.bracket
        16 => Style::default().fg(Color::Gray),                                          // punctuation.delimiter
        17 => Style::default().fg(Color::Green),                                         // string
        18 => Style::default().fg(Color::Green),                                         // string.special
        19 => Style::default().fg(Color::Red),                                           // tag
        20 => Style::default().fg(Color::Cyan),                                          // type
        21 => Style::default().fg(Color::Cyan),                                          // type.builtin
        22 => Style::default().fg(Color::White),                                         // variable
        23 => Style::default().fg(Color::Red),                                           // variable.builtin
        24 => Style::default().fg(Color::White),                                         // variable.parameter
        _ => Style::default(),
    }
}

/// Style for selected text.
pub fn selection_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::Blue)
}

/// Style for the cursor block.
pub fn cursor_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::White)
}

/// Style for the cursor in Insert mode (slightly different shade to distinguish).
pub fn cursor_insert_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::Cyan)
}
