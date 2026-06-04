//! A "boiling" Braille spinner shown in the status bar while a background task
//! runs (a notebook cell executing, an in-flight LSP request).
//!
//! Rather than cycling through a fixed list of frames, the spinner models the
//! 8 dots of a single Braille cell (U+2800 + bitmask) and randomly flips one
//! dot per tick.  The result shimmers organically instead of looping visibly.
//! A guard keeps the lit-dot count within `[MIN_DOTS, MAX_DOTS]` so it never
//! collapses to blank (looks stalled) or fills solid (looks frozen).

use std::time::{Duration, Instant};

/// Minimum / maximum lit dots — keeps the animation lively, never empty/solid.
const MIN_DOTS: u32 = 2;
const MAX_DOTS: u32 = 6;
/// Minimum delay between dot flips.  The render loop ticks ~60fps, but the
/// spinner only mutates this often so the motion reads as deliberate.
const FLIP_INTERVAL: Duration = Duration::from_millis(90);
/// A pleasant non-empty starting pattern (dots 2 and 5 → a centred pair).
const SEED_BITS: u8 = 0b0001_0010;

pub struct Spinner {
    /// Current Braille dot bitmask (bit `n` ⇒ dot `n+1`).
    bits: u8,
    /// True while a background task is in progress.
    active: bool,
    /// Timestamp of the last dot flip (throttles the animation).
    last_flip: Instant,
    /// xorshift64 state for cheap dependency-free randomness.
    rng: u64,
}

impl Default for Spinner {
    fn default() -> Self {
        // Seed the RNG from the wall clock so each session shimmers differently.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15)
            | 1; // xorshift must never be seeded with 0
        Self {
            bits: SEED_BITS,
            active: false,
            last_flip: Instant::now(),
            rng: seed,
        }
    }
}

impl Spinner {
    fn next_rng(&mut self) -> u64 {
        // xorshift64
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Flip one dot, respecting the lit-count guard rails so the glyph stays
    /// in the visually-interesting middle band.
    fn flip_one(&mut self) {
        let count = self.bits.count_ones();
        let r = self.next_rng();
        if count <= MIN_DOTS {
            // Too sparse — turn a currently-off dot on.
            let off = !self.bits;
            if off != 0 {
                self.bits |= nth_set_bit(off, (r % off.count_ones() as u64) as u32);
            }
        } else if count >= MAX_DOTS {
            // Too dense — turn a currently-on dot off.
            self.bits &= !nth_set_bit(self.bits, (r % self.bits.count_ones() as u64) as u32);
        } else {
            // Free to toggle any single dot.
            self.bits ^= 1u8 << (r % 8);
        }
    }

    /// Advance the spinner.  Call once per frame from the run loop.
    ///
    /// `active` reflects whether any background task is currently running; when
    /// it falls to `false` the spinner goes dormant (and `glyph()`/`is_active()`
    /// report inactive).  On the rising edge it reseeds to a non-blank pattern.
    pub fn update(&mut self, active: bool) {
        if !active {
            self.active = false;
            return;
        }
        let now = Instant::now();
        if !self.active {
            self.active = true;
            self.last_flip = now;
            if self.bits.count_ones() < MIN_DOTS {
                self.bits = SEED_BITS;
            }
            return;
        }
        if now.duration_since(self.last_flip) >= FLIP_INTERVAL {
            self.last_flip = now;
            self.flip_one();
        }
    }

    /// The current Braille glyph, or `None` when dormant.
    pub fn glyph(&self) -> Option<char> {
        if !self.active {
            return None;
        }
        char::from_u32(0x2800 + self.bits as u32)
    }
}

/// Return a mask with only the `n`-th set bit of `value` set (0-indexed from LSB).
fn nth_set_bit(value: u8, n: u32) -> u8 {
    let mut remaining = n;
    let mut bits = value;
    while bits != 0 {
        let lowest = bits & bits.wrapping_neg(); // isolate lowest set bit
        if remaining == 0 {
            return lowest;
        }
        remaining -= 1;
        bits &= bits - 1; // clear lowest set bit
    }
    0
}
