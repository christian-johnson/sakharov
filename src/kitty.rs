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

fn crop_params(crop: Option<ImageCrop>) -> String {
    match crop {
        Some((y, h)) if h > 0 => format!(",y={y},h={h}"),
        _ => String::new(),
    }
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

    let encoded = base64::engine::general_purpose::STANDARD.encode(png_data);
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    let total = chunks.len();
    // q=2 suppresses all OK/error responses so they don't pollute stdin.
    let base_params = format!("a=T,f=100,i={id},r={rows},c={cols}{},q=2", crop_params(crop));

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

/// Delete placements for specific image IDs, then send a catch-all delete.
/// More reliable than clear_images() alone on terminals with partial a=d support.
pub fn delete_images(ids: &[u32]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    for &id in ids {
        write!(stdout, "\x1b_Ga=d,i={id},q=2\x1b\\")?;
    }
    write!(stdout, "\x1b_Ga=d,q=2\x1b\\")?;
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
