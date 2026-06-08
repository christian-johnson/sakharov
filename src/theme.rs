use crate::mode::Mode;
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
        // --- Markdown markup (see highlight::MD_* constants) ---
        crate::highlight::MD_HEADING_1 => Style::default().fg(Color::LightMagenta).add_modifier(Modifier::BOLD),
        crate::highlight::MD_HEADING_2 => Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD),
        crate::highlight::MD_HEADING_3 => Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
        crate::highlight::MD_HEADING_4 => Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
        crate::highlight::MD_HEADING_5 => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        crate::highlight::MD_HEADING_6 => Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD),
        crate::highlight::MD_BOLD => Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        crate::highlight::MD_ITALIC => Style::default().add_modifier(Modifier::ITALIC),
        crate::highlight::MD_RAW => Style::default().fg(Color::Green),
        crate::highlight::MD_LINK => Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
        crate::highlight::MD_QUOTE => Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        crate::highlight::MD_LIST => Style::default().fg(Color::Yellow),
        _ => Style::default(),
    }
}

/// Style for selected text.
pub fn selection_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::Blue)
}

/// Resolve the color for a given mode, consulting per-mode overrides from
/// `colors` first and falling back to built-in ANSI colors.
pub fn mode_color(mode: &Mode, colors: &crate::config::ModeColorsConfig) -> Color {
    let hex = match mode {
        Mode::Normal                                                        => &colors.normal,
        Mode::Insert                                                        => &colors.insert,
        Mode::Select                                                        => &colors.select,
        Mode::Command | Mode::Prompt { .. }                                 => &colors.command,
        Mode::Goto { .. } | Mode::FindChar { .. } | Mode::Search { .. }    => &colors.goto,
        Mode::Jump { .. }                                                   => &colors.jump,
        Mode::Fold                                                          => &colors.fold,
    };
    if !hex.is_empty() {
        if let Some(c) = crate::config::parse_hex_color(hex) {
            return c;
        }
    }
    match mode {
        Mode::Normal                                                        => Color::Blue,
        Mode::Insert                                                        => Color::Green,
        Mode::Select                                                        => Color::Yellow,
        Mode::Command | Mode::Prompt { .. }                                 => Color::Cyan,
        Mode::Goto { .. } | Mode::FindChar { .. } | Mode::Search { .. }    => Color::Magenta,
        Mode::Jump { .. }                                                   => Color::Rgb(255, 160, 0),
        Mode::Fold                                                          => Color::Rgb(255, 160, 50),
    }
}

/// Style for the cursor block — background is the mode color, foreground is
/// always black (the cursor cell inverts the character's own colors).
pub fn cursor_style(mode: &Mode, colors: &crate::config::ModeColorsConfig) -> Style {
    Style::default().fg(Color::Black).bg(mode_color(mode, colors))
}

use std::collections::HashMap;
use std::sync::Mutex;

static COLOR_CACHE: Mutex<Option<HashMap<u8, String>>> = Mutex::new(None);

/// Convert a ratatui color to its ANSI color index.
pub fn color_to_ansi_index(color: Color) -> Option<u8> {
    match color {
        Color::Black => Some(0),
        Color::Red => Some(1),
        Color::Green => Some(2),
        Color::Yellow => Some(3),
        Color::Blue => Some(4),
        Color::Magenta => Some(5),
        Color::Cyan => Some(6),
        Color::Gray => Some(7),
        Color::DarkGray => Some(8),
        Color::LightRed => Some(9),
        Color::LightGreen => Some(10),
        Color::LightYellow => Some(11),
        Color::LightBlue => Some(12),
        Color::LightMagenta => Some(13),
        Color::LightCyan => Some(14),
        Color::White => Some(15),
        Color::Indexed(i) => Some(i),
        _ => None,
    }
}

/// Helper for non-blocking poll on stdin.
#[cfg(unix)]
fn wait_for_stdin(timeout_ms: i32) -> bool {
    let mut poll_fd = libc::pollfd {
        fd: 0, // stdin
        events: libc::POLLIN,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
    ret > 0 && (poll_fd.revents & libc::POLLIN) != 0
}

#[cfg(unix)]
fn read_all_stdin(timeout_ms: i32) -> Vec<u8> {
    use std::io::Read;
    let mut response = Vec::new();
    let mut buf = [0u8; 256];
    
    // Wait for the first byte to arrive
    if wait_for_stdin(timeout_ms) {
        let mut stdin = std::io::stdin();
        if let Ok(n) = stdin.read(&mut buf) {
            response.extend_from_slice(&buf[..n]);
            
            // Read any subsequent bytes that are immediately available
            while wait_for_stdin(5) {
                if let Ok(n) = stdin.read(&mut buf) {
                    response.extend_from_slice(&buf[..n]);
                } else {
                    break;
                }
                if response.len() > 1024 {
                    break;
                }
            }
        }
    }
    response
}

#[cfg(unix)]
fn parse_channel(hex_str: &str) -> Option<u8> {
    if hex_str.is_empty() {
        return None;
    }
    let hex_to_parse = if hex_str.len() >= 2 {
        &hex_str[..2]
    } else {
        return u8::from_str_radix(&format!("{}{}", hex_str, hex_str), 16).ok();
    };
    u8::from_str_radix(hex_to_parse, 16).ok()
}

#[cfg(unix)]
fn parse_all_responses(data: &[u8], cache: &mut HashMap<u8, String>) {
    let s = match String::from_utf8(data.to_vec()) {
        Ok(s) => s,
        Err(_) => return,
    };
    
    // Split by "\x1b]" to handle multiple OSC sequences
    for part in s.split("\x1b]") {
        if part.is_empty() {
            continue;
        }
        
        let content = if part.ends_with('\x07') {
            &part[..part.len() - 1]
        } else if part.ends_with("\x1b\\") {
            &part[..part.len() - 2]
        } else {
            part.trim_end_matches(|c: char| c.is_control() || c == '\\')
        };
        
        if !content.starts_with("4;") {
            continue;
        }
        let content = &content[2..];
        
        let mut parts = content.split(';');
        let index_str = match parts.next() {
            Some(idx) => idx,
            None => continue,
        };
        let index: u8 = match index_str.parse() {
            Ok(idx) => idx,
            Err(_) => continue,
        };
        
        let rgb_part = match parts.next() {
            Some(p) => p,
            None => continue,
        };
        
        let rgb_val = match rgb_part.strip_prefix("rgb:") {
            Some(val) => val,
            None => continue,
        };
        
        let mut rgb_hex = rgb_val.split('/');
        let r_hex = match rgb_hex.next() {
            Some(h) => h,
            None => continue,
        };
        let g_hex = match rgb_hex.next() {
            Some(h) => h,
            None => continue,
        };
        let b_hex = match rgb_hex.next() {
            Some(h) => h,
            None => continue,
        };
        
        let r = match parse_channel(r_hex) {
            Some(val) => val,
            None => continue,
        };
        let g = match parse_channel(g_hex) {
            Some(val) => val,
            None => continue,
        };
        let b = match parse_channel(b_hex) {
            Some(val) => val,
            None => continue,
        };
        
        cache.insert(index, format!("#{:02x}{:02x}{:02x}", r, g, b));
    }
}

/// Query the terminal theme color palette on startup and cache the values.
pub fn initialize_color_cache() {
    #[cfg(unix)]
    {
        if let Ok(term) = std::env::var("TERM") {
            if term == "dumb" {
                return;
            }
        }

        // Flush stdin first to clear any pending user keystrokes
        unsafe {
            libc::tcflush(0, libc::TCIFLUSH);
        }

        use std::io::Write;
        let mut stdout = std::io::stdout();
        // Query green (2), yellow (3), blue (4), magenta (5), cyan (6)
        let _ = write!(
            stdout,
            "\x1b]4;2;?\x07\x1b]4;3;?\x07\x1b]4;4;?\x07\x1b]4;5;?\x07\x1b]4;6;?\x07"
        );
        let _ = stdout.flush();

        let data = read_all_stdin(50);
        if !data.is_empty() {
            let mut cache = HashMap::new();
            parse_all_responses(&data, &mut cache);
            if !cache.is_empty() {
                if let Ok(mut guard) = COLOR_CACHE.lock() {
                    *guard = Some(cache);
                }
            }
        }
    }
}

/// Convert a ratatui color to its OSC-compatible color string.
pub fn color_to_osc_spec(color: Color) -> Option<String> {
    if let Some(index) = color_to_ansi_index(color) {
        if let Ok(guard) = COLOR_CACHE.lock() {
            if let Some(ref cache) = *guard {
                if let Some(hex) = cache.get(&index) {
                    return Some(hex.clone());
                }
            }
        }
    }
    match color {
        Color::Reset => None,
        Color::Black => Some("color0".to_string()),
        Color::Red => Some("color1".to_string()),
        Color::Green => Some("color2".to_string()),
        Color::Yellow => Some("color3".to_string()),
        Color::Blue => Some("color4".to_string()),
        Color::Magenta => Some("color5".to_string()),
        Color::Cyan => Some("color6".to_string()),
        Color::Gray => Some("color7".to_string()),
        Color::DarkGray => Some("color8".to_string()),
        Color::LightRed => Some("color9".to_string()),
        Color::LightGreen => Some("color10".to_string()),
        Color::LightYellow => Some("color11".to_string()),
        Color::LightBlue => Some("color12".to_string()),
        Color::LightMagenta => Some("color13".to_string()),
        Color::LightCyan => Some("color14".to_string()),
        Color::White => Some("color15".to_string()),
        Color::Indexed(i) => Some(format!("color{}", i)),
        Color::Rgb(r, g, b) => Some(format!("#{:02x}{:02x}{:02x}", r, g, b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_channel() {
        assert_eq!(parse_channel("2626"), Some(0x26));
        assert_eq!(parse_channel("8b8b"), Some(0x8b));
        assert_eq!(parse_channel("ff"), Some(0xff));
        assert_eq!(parse_channel("f"), Some(0xff));
        assert_eq!(parse_channel(""), None);
    }

    #[test]
    fn test_parse_all_responses() {
        let mut cache = HashMap::new();
        // Test single response with BEL terminator
        let data1 = b"\x1b]4;2;rgb:5050/adad/7070\x07";
        parse_all_responses(data1, &mut cache);
        assert_eq!(cache.get(&2), Some(&"#50ad70".to_string()));

        // Test multiple responses back-to-back with ST (\x1b\\) and BEL mixed
        let data2 = b"\x1b]4;4;rgb:2626/8b8b/e2e2\x1b\\\x1b]4;6;rgb:1111/2222/3333\x07";
        parse_all_responses(data2, &mut cache);
        assert_eq!(cache.get(&4), Some(&"#268be2".to_string()));
        assert_eq!(cache.get(&6), Some(&"#112233".to_string()));
    }

    #[test]
    fn test_color_to_ansi_index() {
        assert_eq!(color_to_ansi_index(Color::Blue), Some(4));
        assert_eq!(color_to_ansi_index(Color::Green), Some(2));
        assert_eq!(color_to_ansi_index(Color::Indexed(42)), Some(42));
        assert_eq!(color_to_ansi_index(Color::Rgb(1, 2, 3)), None);
    }
}


