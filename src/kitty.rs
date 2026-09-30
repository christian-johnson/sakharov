use std::io::Write as _;

use anyhow::Result;
use base64::Engine as _;

/// Which terminal graphics backend is in use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GraphicsTerminal {
    /// Kitty terminal — full Kitty graphics protocol support.
    Kitty,
    /// Ghostty — implements the Kitty graphics protocol (and the Kitty
    /// keyboard protocol).
    Ghostty,
    /// WezTerm — supports the Kitty graphics protocol.
    WezTerm,
    /// Terminal without known Kitty graphics support — images are suppressed.
    Other,
}

impl GraphicsTerminal {
    pub fn detect() -> Self {
        Self::detect_from(|k| std::env::var(k).ok())
    }

    /// Detection against an arbitrary env lookup (unit-testable — `std::env`
    /// mutation is process-global and racy under the parallel test runner).
    fn detect_from(env: impl Fn(&str) -> Option<String>) -> Self {
        let term = env("TERM").unwrap_or_default();
        let term_program = env("TERM_PROGRAM").unwrap_or_default();
        if env("KITTY_WINDOW_ID").is_some() || term.contains("kitty") {
            return GraphicsTerminal::Kitty;
        }
        // TERM=xterm-ghostty propagates over ssh; the other two only hold locally.
        if term.contains("ghostty")
            || term_program.eq_ignore_ascii_case("ghostty")
            || env("GHOSTTY_RESOURCES_DIR").is_some()
        {
            return GraphicsTerminal::Ghostty;
        }
        if term_program.eq_ignore_ascii_case("wezterm") || env("WEZTERM_UNIX_SOCKET").is_some() {
            return GraphicsTerminal::WezTerm;
        }
        GraphicsTerminal::Other
    }

    pub fn supports_graphics(self) -> bool {
        !matches!(self, GraphicsTerminal::Other)
    }

    /// Terminals that draw an image wherever its Unicode placeholder cells
    /// are (see [`placeholder_symbol`]), so moving one is just writing text.
    pub fn supports_placeholders(self) -> bool {
        matches!(self, GraphicsTerminal::Kitty | GraphicsTerminal::Ghostty)
    }

    /// Terminals known to implement the Kitty *keyboard* protocol, used to
    /// force-enable it when the support query goes unanswered (the reply can
    /// be lost in startup output on some setups). WezTerm is deliberately
    /// excluded: it only speaks the protocol when the user opts in via
    /// `enable_kitty_keyboard`, so the query is authoritative there.
    pub fn implements_kitty_keyboard(self) -> bool {
        matches!(self, GraphicsTerminal::Kitty | GraphicsTerminal::Ghostty)
    }
}

/// Vertical source-rectangle crop, in image pixels: `(y_px, h_px)` — display
/// only the horizontal band starting `y_px` from the top, `h_px` tall.  Used
/// to clip images at the viewport edge instead of squashing them.
pub type ImageCrop = (u32, u32);

/// A request to render a PNG image via the Kitty graphics protocol.
///
/// Produced by a renderer (which knows *where* on screen an image goes) and
/// flushed by the run loop after `terminal.draw()` — ratatui owns the screen
/// during the draw, so pixel data cannot be written until it has finished.
/// Lives here rather than with any one renderer because every view that can
/// show a raster emits these.
pub struct ImageRequest {
    pub col: u16,
    pub row: u16,
    pub rows: u16,
    /// Explicit column width passed as `c=` in the protocol.  Required for
    /// WezTerm, which doesn't auto-compute width from aspect ratio like Kitty.
    pub cols: u16,
    /// Vertical source-rectangle crop `(y_px, h_px)` when the image is clipped
    /// at the viewport edge — the visible band is shown at its natural scale
    /// instead of squashing the whole image into the remaining rows.
    pub crop: Option<ImageCrop>,
    /// Image rows clipped off above the viewport (`rows` are shown below them).
    pub skip_rows: u16,
    /// Rows the whole image occupies when nothing is clipped.
    pub full_rows: u16,
    /// Shared reference to the raw PNG bytes — cloning this is O(1).
    pub png_data: std::sync::Arc<Vec<u8>>,
}

/// The character a cell holds to show one cell of a virtual placement.
const PLACEHOLDER: char = '\u{10EEEE}';

/// Combining marks that number a placeholder's row and column, in order —
/// kitty's `rowcolumn-diacritics.txt`, which the protocol fixes.
const DIACRITICS: [char; 297] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065A}', '\u{065B}', '\u{065D}', '\u{065E}', '\u{06D6}', '\u{06D7}', '\u{06D8}',
    '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}', '\u{06E0}', '\u{06E1}', '\u{06E2}',
    '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EB}', '\u{06EC}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073A}', '\u{073D}', '\u{073F}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074A}', '\u{07EB}', '\u{07EC}', '\u{07ED}', '\u{07EE}',
    '\u{07EF}', '\u{07F0}', '\u{07F1}', '\u{07F3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081B}', '\u{081C}', '\u{081D}', '\u{081E}', '\u{081F}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082A}', '\u{082B}', '\u{082C}',
    '\u{082D}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0F82}', '\u{0F83}', '\u{0F86}', '\u{0F87}',
    '\u{135D}', '\u{135E}', '\u{135F}', '\u{17DD}', '\u{193A}', '\u{1A17}', '\u{1A75}', '\u{1A76}',
    '\u{1A77}', '\u{1A78}', '\u{1A79}', '\u{1A7A}', '\u{1A7B}', '\u{1A7C}', '\u{1B6B}', '\u{1B6D}',
    '\u{1B6E}', '\u{1B6F}', '\u{1B70}', '\u{1B71}', '\u{1B72}', '\u{1B73}', '\u{1CD0}', '\u{1CD1}',
    '\u{1CD2}', '\u{1CDA}', '\u{1CDB}', '\u{1CE0}', '\u{1DC0}', '\u{1DC1}', '\u{1DC3}', '\u{1DC4}',
    '\u{1DC5}', '\u{1DC6}', '\u{1DC7}', '\u{1DC8}', '\u{1DC9}', '\u{1DCB}', '\u{1DCC}', '\u{1DD1}',
    '\u{1DD2}', '\u{1DD3}', '\u{1DD4}', '\u{1DD5}', '\u{1DD6}', '\u{1DD7}', '\u{1DD8}', '\u{1DD9}',
    '\u{1DDA}', '\u{1DDB}', '\u{1DDC}', '\u{1DDD}', '\u{1DDE}', '\u{1DDF}', '\u{1DE0}', '\u{1DE1}',
    '\u{1DE2}', '\u{1DE3}', '\u{1DE4}', '\u{1DE5}', '\u{1DE6}', '\u{1DFE}', '\u{20D0}', '\u{20D1}',
    '\u{20D4}', '\u{20D5}', '\u{20D6}', '\u{20D7}', '\u{20DB}', '\u{20DC}', '\u{20E1}', '\u{20E7}',
    '\u{20E9}', '\u{20F0}', '\u{2CEF}', '\u{2CF0}', '\u{2CF1}', '\u{2DE0}', '\u{2DE1}', '\u{2DE2}',
    '\u{2DE3}', '\u{2DE4}', '\u{2DE5}', '\u{2DE6}', '\u{2DE7}', '\u{2DE8}', '\u{2DE9}', '\u{2DEA}',
    '\u{2DEB}', '\u{2DEC}', '\u{2DED}', '\u{2DEE}', '\u{2DEF}', '\u{2DF0}', '\u{2DF1}', '\u{2DF2}',
    '\u{2DF3}', '\u{2DF4}', '\u{2DF5}', '\u{2DF6}', '\u{2DF7}', '\u{2DF8}', '\u{2DF9}', '\u{2DFA}',
    '\u{2DFB}', '\u{2DFC}', '\u{2DFD}', '\u{2DFE}', '\u{2DFF}', '\u{A66F}', '\u{A67C}', '\u{A67D}',
    '\u{A6F0}', '\u{A6F1}', '\u{A8E0}', '\u{A8E1}', '\u{A8E2}', '\u{A8E3}', '\u{A8E4}', '\u{A8E5}',
    '\u{A8E6}', '\u{A8E7}', '\u{A8E8}', '\u{A8E9}', '\u{A8EA}', '\u{A8EB}', '\u{A8EC}', '\u{A8ED}',
    '\u{A8EE}', '\u{A8EF}', '\u{A8F0}', '\u{A8F1}', '\u{AAB0}', '\u{AAB2}', '\u{AAB3}', '\u{AAB7}',
    '\u{AAB8}', '\u{AABE}', '\u{AABF}', '\u{AAC1}', '\u{FE20}', '\u{FE21}', '\u{FE22}', '\u{FE23}',
    '\u{FE24}', '\u{FE25}', '\u{FE26}', '\u{10A0F}', '\u{10A38}', '\u{1D185}', '\u{1D186}', '\u{1D187}',
    '\u{1D188}', '\u{1D189}', '\u{1D1AA}', '\u{1D1AB}', '\u{1D1AC}', '\u{1D1AD}', '\u{1D242}', '\u{1D243}',
    '\u{1D244}',
];

/// The cell symbol that shows cell `(row, col)` of an image placed with
/// [`upload_virtual`]; `None` past the 297 rows/columns the protocol can number.
pub fn placeholder_symbol(row: u16, col: u16) -> Option<String> {
    let row = *DIACRITICS.get(row as usize)?;
    let col = *DIACRITICS.get(col as usize)?;
    Some([PLACEHOLDER, row, col].iter().collect())
}

/// The foreground colour that tells the terminal which image a placeholder
/// cell belongs to: the id's low 24 bits, as RGB.
pub fn placeholder_color(id: u32) -> ratatui::style::Color {
    ratatui::style::Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
}

/// The largest image id a placeholder's colour can carry.
pub const MAX_PLACEHOLDER_ID: u32 = 0x00FF_FFFF;

fn crop_params(crop: Option<ImageCrop>) -> String {
    match crop {
        Some((y, h)) if h > 0 => format!(",y={y},h={h}"),
        _ => String::new(),
    }
}

/// Send `png_data` in the protocol's 4096-byte base64 chunks; `params` are
/// the control keys of the first chunk.
fn transmit(out: &mut impl std::io::Write, params: &str, png_data: &[u8]) -> std::io::Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(png_data);
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    let total = chunks.len();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < total);
        if i == 0 {
            write!(out, "\x1b_G{params},m={more};{chunk}\x1b\\")?;
        } else {
            write!(out, "\x1b_Gm={more};{chunk}\x1b\\")?;
        }
    }
    Ok(())
}

/// Upload PNG with a stable image `id` and display it at terminal cell (col, row).
///
/// `cols` is passed as `c=` so WezTerm (which doesn't auto-scale width) renders
/// the image at the correct width.  After the first call for a given image,
/// use `place_image` to reposition it cheaply without re-transmitting pixel data.
pub fn upload_and_place(
    col: u16,
    row: u16,
    id: u32,
    rows: u16,
    cols: u16,
    crop: Option<ImageCrop>,
    png_data: &[u8],
) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "\x1b[{};{}H", row + 1, col + 1)?;
    // q=2 suppresses all OK/error responses so they don't pollute stdin.
    let params = format!("a=T,f=100,i={id},r={rows},c={cols}{},q=2", crop_params(crop));
    transmit(&mut stdout, &params, png_data)?;
    stdout.flush()?;
    Ok(())
}

/// Upload PNG as image `id` and give it a *virtual* placement `cols` × `rows`
/// cells big: nothing is drawn until cells hold its [`placeholder_symbol`]s
/// in its [`placeholder_color`].  Neither command moves the cursor.
pub fn upload_virtual(id: u32, rows: u16, cols: u16, png_data: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    transmit(&mut stdout, &format!("a=t,f=100,i={id},q=2"), png_data)?;
    write!(stdout, "\x1b_Ga=p,U=1,i={id},r={rows},c={cols},q=2\x1b\\")?;
    Ok(())
}

/// Delete image `id` — its placements and its pixel data.
pub fn free_image(id: u32) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
    Ok(())
}

/// Re-display a previously-uploaded image at (col, row).  Only ~30 bytes —
/// pixel data is already cached in the terminal under `id`.
pub fn place_image(
    col: u16,
    row: u16,
    id: u32,
    rows: u16,
    cols: u16,
    crop: Option<ImageCrop>,
) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write!(
        stdout,
        "\x1b[{};{}H\x1b_Ga=p,i={id},r={rows},c={cols}{},q=2\x1b\\",
        row + 1,
        col + 1,
        crop_params(crop),
    )?;
    stdout.flush()?;
    Ok(())
}

/// Delete all visible Kitty image placements.  q=2 suppresses the terminal's
/// OK response so it never appears in stdin as a spurious key event.
pub fn clear_images() -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "\x1b_Ga=d,q=2\x1b\\")?;
    stdout.flush()?;
    Ok(())
}

/// Delete placements for specific image IDs, then (with `catch_all`) send a
/// catch-all delete — more reliable than clear_images() alone on terminals
/// with partial a=d support, but it takes every other placement down too.
pub fn delete_images(ids: &[u32], catch_all: bool) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    for &id in ids {
        write!(stdout, "\x1b_Ga=d,i={id},q=2\x1b\\")?;
    }
    if catch_all {
        write!(stdout, "\x1b_Ga=d,q=2\x1b\\")?;
    }
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detects_ghostty_kitty_and_wezterm() {
        assert_eq!(
            GraphicsTerminal::detect_from(env_of(&[("TERM", "xterm-ghostty")])),
            GraphicsTerminal::Ghostty
        );
        assert_eq!(
            GraphicsTerminal::detect_from(env_of(&[("TERM_PROGRAM", "ghostty")])),
            GraphicsTerminal::Ghostty
        );
        assert_eq!(
            GraphicsTerminal::detect_from(env_of(&[("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty")])),
            GraphicsTerminal::Ghostty
        );
        assert_eq!(
            GraphicsTerminal::detect_from(env_of(&[("TERM", "xterm-kitty")])),
            GraphicsTerminal::Kitty
        );
        assert_eq!(
            GraphicsTerminal::detect_from(env_of(&[("TERM_PROGRAM", "WezTerm")])),
            GraphicsTerminal::WezTerm
        );
        assert_eq!(
            GraphicsTerminal::detect_from(env_of(&[("TERM", "xterm-256color")])),
            GraphicsTerminal::Other
        );
    }

    /// ratatui skips the cells a wide symbol covers, so a placeholder must
    /// measure exactly one column or the rest of its row is never written.
    #[test]
    fn every_placeholder_symbol_is_one_column_wide() {
        use unicode_width::UnicodeWidthStr;
        for i in 0..DIACRITICS.len() as u16 {
            let symbol = placeholder_symbol(i, i).unwrap();
            assert_eq!(symbol.width(), 1, "diacritic {i}");
            assert_eq!(symbol.chars().count(), 3);
        }
        assert_eq!(placeholder_symbol(0, 1).unwrap(), "\u{10EEEE}\u{0305}\u{030D}");
        assert!(placeholder_symbol(297, 0).is_none());
        assert_eq!(placeholder_color(0x01_02_03), ratatui::style::Color::Rgb(1, 2, 3));
    }

    #[test]
    fn graphics_and_keyboard_support_by_terminal() {
        assert!(GraphicsTerminal::Ghostty.supports_graphics());
        assert!(GraphicsTerminal::Kitty.supports_graphics());
        assert!(GraphicsTerminal::WezTerm.supports_graphics());
        assert!(!GraphicsTerminal::Other.supports_graphics());

        assert!(GraphicsTerminal::Ghostty.implements_kitty_keyboard());
        assert!(GraphicsTerminal::Kitty.implements_kitty_keyboard());
        assert!(!GraphicsTerminal::WezTerm.implements_kitty_keyboard());
        assert!(!GraphicsTerminal::Other.implements_kitty_keyboard());
    }
}
