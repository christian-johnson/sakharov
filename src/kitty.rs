use std::io::Write as _;

use anyhow::Result;
use base64::Engine as _;

/// Upload PNG with a stable image `id` and display it at terminal cell (col, row).
///
/// After the first call for a given image, use `place_image` to reposition it
/// cheaply without re-transmitting pixel data.
pub fn upload_and_place(col: u16, row: u16, id: u32, rows: u16, png_data: &[u8]) -> Result<()> {
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
    let base_params = format!("a=T,f=100,i={id},r={rows},q=2");

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
/// pixel data is already cached in Kitty under `id`.
pub fn place_image(col: u16, row: u16, id: u32, rows: u16) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write!(
        stdout,
        "\x1b[{};{}H\x1b_Ga=p,i={id},r={rows},q=2\x1b\\",
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
