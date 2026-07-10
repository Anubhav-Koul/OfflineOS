//! The widget window's click-through mask.
//!
//! The character window is transparent, undecorated, and always on top. Clicks
//! must land on the character and the chat panel but pass through the empty
//! desktop around them (CLAUDE.md Phase 3: per-pixel hit testing). WebView2
//! offers no per-pixel hit test of its own, so the split is:
//!
//!   - the **UI** knows what is solid — it rasterizes the chat panel's DOM rect
//!     and the character's alpha silhouette into this coarse grid and pushes it
//!     over IPC whenever the layout or the silhouette changes;
//!   - **Rust** knows where the cursor is — a poller reads the global cursor
//!     (the webview receives no mouse events at all while the window is
//!     click-through, so it cannot self-report) and toggles
//!     `set_ignore_cursor_events` by testing this mask.
//!
//! The mask is deliberately coarse (the UI uses 8-px cells and dilates by one
//! cell) — the goal is "clicks near the character land, clicks on empty desktop
//! pass", not pixel-perfect edges.

use serde::Deserialize;

/// A row-major bitset over the widget window, in logical pixels.
///
/// Bit `row * cols + col` is set when that cell contains something clickable.
/// Bits are packed LSB-first within each byte, matching the UI's encoder.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HitMask {
    /// Cell edge length, logical pixels. Never zero (validated on receipt).
    pub cell: u32,
    /// Grid width in cells.
    pub cols: u32,
    /// Grid height in cells.
    pub rows: u32,
    /// The packed bits, `ceil(cols * rows / 8)` bytes.
    pub bits: Vec<u8>,
}

impl HitMask {
    /// Whether the mask is internally consistent. A mask that fails this is
    /// rejected at the IPC boundary rather than stored.
    pub fn is_valid(&self) -> bool {
        let cells = (self.cols as usize).saturating_mul(self.rows as usize);
        self.cell > 0 && cells > 0 && self.bits.len() >= cells.div_ceil(8)
    }

    /// Whether the point at window-local logical coordinates is solid.
    ///
    /// Points outside the grid are not solid: the window edge beyond the last
    /// cell has nothing to click.
    pub fn is_solid(&self, x: f64, y: f64) -> bool {
        if x < 0.0 || y < 0.0 || self.cell == 0 {
            return false;
        }
        let col = (x / f64::from(self.cell)) as u32;
        let row = (y / f64::from(self.cell)) as u32;
        if col >= self.cols || row >= self.rows {
            return false;
        }
        let index = (row as usize) * (self.cols as usize) + (col as usize);
        match self.bits.get(index / 8) {
            Some(byte) => byte & (1 << (index % 8)) != 0,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3×2 grid of 10-px cells with only cell (col 1, row 0) solid.
    fn mask() -> HitMask {
        HitMask {
            cell: 10,
            cols: 3,
            rows: 2,
            bits: vec![0b0000_0010],
        }
    }

    #[test]
    fn a_solid_cell_hits_and_its_neighbours_do_not() {
        let mask = mask();
        assert!(mask.is_solid(15.0, 5.0), "the centre of cell (1,0)");
        assert!(mask.is_solid(10.0, 0.0), "the cell's own top-left corner");
        assert!(!mask.is_solid(5.0, 5.0), "cell (0,0) is empty");
        assert!(!mask.is_solid(25.0, 15.0), "cell (2,1) is empty");
    }

    #[test]
    fn points_outside_the_grid_are_not_solid() {
        let mask = mask();
        assert!(!mask.is_solid(-1.0, 5.0));
        assert!(!mask.is_solid(5.0, -0.1));
        assert!(!mask.is_solid(30.0, 5.0), "just past the last column");
        assert!(!mask.is_solid(5.0, 20.0), "just past the last row");
    }

    #[test]
    fn bit_order_is_lsb_first_row_major() {
        // Cells 0..=8 of a 3×3 grid, with bits 0, 4, 8 set: (0,0), (1,1), (2,2).
        let mask = HitMask {
            cell: 1,
            cols: 3,
            rows: 3,
            bits: vec![0b0001_0001, 0b0000_0001],
        };
        assert!(mask.is_solid(0.0, 0.0));
        assert!(mask.is_solid(1.0, 1.0));
        assert!(mask.is_solid(2.0, 2.0));
        assert!(!mask.is_solid(1.0, 0.0));
        assert!(!mask.is_solid(2.0, 1.0));
    }

    #[test]
    fn validation_rejects_zero_dimensions_and_short_bitsets() {
        assert!(mask().is_valid());
        for broken in [
            HitMask { cell: 0, ..mask() },
            HitMask { cols: 0, ..mask() },
            HitMask { rows: 0, ..mask() },
            HitMask {
                bits: vec![],
                ..mask()
            },
        ] {
            assert!(!broken.is_valid(), "{broken:?}");
        }
    }

    #[test]
    fn a_short_bitset_reads_as_empty_rather_than_panicking() {
        // Defence in depth: even if an invalid mask slipped past validation,
        // lookups past the buffer are false, never a panic in the poller.
        let mask = HitMask {
            cell: 10,
            cols: 100,
            rows: 100,
            bits: vec![0xFF],
        };
        assert!(mask.is_solid(0.0, 0.0));
        assert!(!mask.is_solid(999.0, 999.0));
    }
}
