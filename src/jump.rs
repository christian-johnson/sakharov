use ropey::Rope;

/// Collect char-index positions of every jump target in the visible row range.
///
/// A jump target is defined as the first non-whitespace character after a run
/// of whitespace (or at column 0 of a line), mirroring the `w` word-motion
/// targets so the jump map feels natural.
pub fn visible_word_starts(rope: &Rope, scroll_row: usize, visible_rows: usize) -> Vec<usize> {
    let total_lines = rope.len_lines();
    let end_row = (scroll_row + visible_rows).min(total_lines);
    let mut positions = Vec::new();

    for line_idx in scroll_row..end_row {
        let line_start = rope.line_to_char(line_idx);
        let line = rope.line(line_idx);
        let len = line.len_chars();
        let mut prev_ws = true; // treat start of line as "after whitespace"

        for off in 0..len {
            let c = line.char(off);
            let is_ws = matches!(c, ' ' | '\t' | '\n' | '\r');
            if !is_ws && prev_ws {
                positions.push(line_start + off);
            }
            prev_ws = is_ws;
        }
    }
    positions
}

/// Assign 2-char labels to positions using the given key alphabet.
/// Supports up to `keys.len()²` targets (26 keys → 676 targets).
pub fn generate_labels(positions: &[usize], keys: &[char]) -> Vec<(usize, String)> {
    if keys.is_empty() {
        return vec![];
    }
    let k = keys.len();
    positions
        .iter()
        .take(k * k)
        .enumerate()
        .map(|(i, &pos)| {
            let first = keys[i / k];
            let second = keys[i % k];
            (pos, format!("{first}{second}"))
        })
        .collect()
}
