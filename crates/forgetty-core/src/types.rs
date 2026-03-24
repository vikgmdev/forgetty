//! Fundamental types used throughout the Forgetty terminal emulator.
//!
//! These types represent coordinates, colors, dimensions, and identifiers
//! that are shared across all subsystems.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A coordinate within the terminal cell grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellCoord {
    /// Row index (0-based, top to bottom).
    pub row: usize,
    /// Column index (0-based, left to right).
    pub col: usize,
}

impl CellCoord {
    /// Creates a new cell coordinate.
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// An RGBA color value with 8-bit channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    /// Creates a new RGBA color.
    pub fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Creates an opaque RGB color (alpha = 255).
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Converts to a normalized `[f32; 4]` suitable for GPU shaders.
    pub fn to_f32_array(self) -> [f32; 4] {
        [self.r as f32 / 255.0, self.g as f32 / 255.0, self.b as f32 / 255.0, self.a as f32 / 255.0]
    }
}

/// A size in pixels or cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    /// Creates a new size.
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// A unique identifier for a terminal pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneId(pub Uuid);

impl PaneId {
    /// Generates a new random pane ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PaneId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
