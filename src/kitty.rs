use std::io::Write as _;

use anyhow::Result;
use base64::Engine as _;

/// Which terminal graphics backend is in use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GraphicsTerminal {
    /// Kitty terminal — full Kitty graphics protocol support.
    Kitty,
    /// WezTerm — supports the Kitty graphics protocol.
    WezTerm,
    /// Terminal without known Kitty graphics support — images are suppressed.
    Other,
}

impl GraphicsTerminal {
    pub fn detect() -> Self {
        if std::env::var("KITTY_WINDOW_ID").is_ok()
            || std::env::var("TERM")
                .map(|t| t.contains("kitty"))
                .unwrap_or(false)
        {
            return GraphicsTerminal::Kitty;
        }
        if std::env::var("TERM_PROGRAM")
            .map(|t| t.eq_ignore_ascii_case("wezterm"))
            .unwrap_or(false)
            || std::env::var("WEZTERM_UNIX_SOCKET").is_ok()
        {
            return GraphicsTerminal::WezTerm;
        }
        GraphicsTerminal::Other
    }

    pub fn supports_graphics(self) -> bool {
        matches!(self, GraphicsTerminal::Kitty | GraphicsTerminal::WezTerm)
    }
}

/// Upload PNG with a stable image `id` and display it at terminal cell (col, row).
///
/// `cols` is passed as `c=` so WezTerm (which doesn't auto-scale width) renders
/// the image at the correct width.  After the first call for a given image,
/// use `place_image` to reposition it cheaply without re-transmitting pixel data.
pub fn upload_and_place(col: u16, row: u16, id: u32, rows: u16, cols: u16, png_data: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "\x1b[{};{}H", row + 1, col + 1)?;

    let encoded = base64::engine::general_purpose::STANDARD.encode(png_data);
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    let total = chunks.len();
    // q=2 suppresses all OK/error responses so they don't pollute stdin.
    let base_params = format!("a=T,f=100,i={id},r={rows},c={cols},q=2");

    for (i, chunk) in chunks.iter().enumerate() {
        if total == 1 {
            write!(stdout, "\x1b_G{base_params},m=0;{chunk}\x1b\\")?;
        } else if i == 0 {
            write!(stdout, "\x1b_G{base_params},m=1;{chunk}\x1b\\")?;
        } else if i + 1 == total {
            write!(stdout, "\x1b_Gm=0;{chunk}\x1b\\")?;
        } else {
            write!(stdout, "\x1b_Gm=1;{chunk}\x1b\\")?;
        }
    }
    stdout.flush()?;
    Ok(())
}

/// Re-display a previously-uploaded image at (col, row).  Only ~30 bytes —
/// pixel data is already cached in the terminal under `id`.
pub fn place_image(col: u16, row: u16, id: u32, rows: u16, cols: u16) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write!(
        stdout,
        "\x1b[{};{}H\x1b_Ga=p,i={id},r={rows},c={cols},q=2\x1b\\",
        row + 1,
        col + 1
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
