use std::io::Write as _;

use anyhow::Result;
use base64::Engine as _;

/// Emit a Kitty graphics command to render `png_data` at terminal cell (col, row).
///
/// `rows` constrains the image height to that many terminal cell rows so it
/// cannot overflow into adjacent cells. Writes directly to stdout — call after
/// `terminal.draw()`. Errors are best-effort; callers should ignore them on
/// terminals without Kitty support.
pub fn render_image(col: u16, row: u16, rows: u16, png_data: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();

    // Move cursor to (col, row).
    write!(stdout, "\x1b[{};{}H", row + 1, col + 1)?;

    let encoded = base64::engine::general_purpose::STANDARD.encode(png_data);
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    let total = chunks.len();
    // r=N tells Kitty to scale the image to exactly N cell rows.
    let base_params = format!("a=T,f=100,r={rows},q=2");

    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i + 1 == total;
        if total == 1 {
            write!(stdout, "\x1b_G{base_params},m=0;{chunk}\x1b\\")?;
        } else if i == 0 {
            write!(stdout, "\x1b_G{base_params},m=1;{chunk}\x1b\\")?;
        } else if is_last {
            write!(stdout, "\x1b_Gm=0;{chunk}\x1b\\")?;
        } else {
            write!(stdout, "\x1b_Gm=1;{chunk}\x1b\\")?;
        }
    }

    stdout.flush()?;
    Ok(())
}

/// Delete all Kitty images. Call before each render to clear stale images.
pub fn clear_images() -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "\x1b_Ga=d\x1b\\")?;
    stdout.flush()?;
    Ok(())
}
