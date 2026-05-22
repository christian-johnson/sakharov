use ropey::Rope;

/// Keys used for labels, ordered ergonomically (home row first).
pub const JUMP_KEYS: &[char] = &[
    'a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l',
    'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p',
    'z', 'x', 'c', 'v', 'b', 'n', 'm',
];

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

/// Assign 2-char labels to positions (up to 27² = 729 targets).
pub fn generate_labels(positions: &[usize]) -> Vec<(usize, String)> {
    let k = JUMP_KEYS.len();
    positions
        .iter()
        .take(k * k)
        .enumerate()
        .map(|(i, &pos)| {
            let first = JUMP_KEYS[i / k];
            let second = JUMP_KEYS[i % k];
            (pos, format!("{first}{second}"))
        })
        .collect()
}
