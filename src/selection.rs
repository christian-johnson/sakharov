/// A selection in the buffer, represented as anchor + head char indices.
///
/// In Normal mode, anchor == head (a "point" selection covering one char).
/// In Select mode, motions move head while anchor stays fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Stable end of the selection (char index in the rope).
    pub anchor: usize,
    /// Moving end of the selection / cursor position (char index in the rope).
    pub head: usize,
}

impl Selection {
    /// Create a new selection with explicit anchor and head.
    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    /// Create a point selection (anchor == head).
    pub fn point(pos: usize) -> Self {
        Self {
            anchor: pos,
            head: pos,
        }
    }

    /// Return the smaller of anchor and head.
    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    /// Return the larger of anchor and head.
    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

}
