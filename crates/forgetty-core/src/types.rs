//! Fundamental types used throughout the Forgetty terminal emulator.
//!
//! These types represent coordinates, colors, dimensions, and identifiers
//! that are shared across all subsystems.

use serde::{de, Deserialize, Serialize};
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
///
/// Deserializes from either a hex string (`"#rrggbb"` or `"#rrggbbaa"`) or
/// a struct `{ r, g, b, a }`. Serializes as a struct for config.toml round-trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
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

    /// Parses a hex color string like `"#rrggbb"` or `"#rrggbbaa"`.
    ///
    /// Returns `None` if the string is not a valid hex color.
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#')?;
        match s.len() {
            6 => {
                let r = u8::from_str_radix(&s[0..2], 16).ok()?;
                let g = u8::from_str_radix(&s[2..4], 16).ok()?;
                let b = u8::from_str_radix(&s[4..6], 16).ok()?;
                Some(Self { r, g, b, a: 255 })
            }
            8 => {
                let r = u8::from_str_radix(&s[0..2], 16).ok()?;
                let g = u8::from_str_radix(&s[2..4], 16).ok()?;
                let b = u8::from_str_radix(&s[4..6], 16).ok()?;
                let a = u8::from_str_radix(&s[6..8], 16).ok()?;
                Some(Self { r, g, b, a })
            }
            _ => None,
        }
    }

    /// Converts to a normalized `[f32; 4]` suitable for GPU shaders.
    pub fn to_f32_array(self) -> [f32; 4] {
        [self.r as f32 / 255.0, self.g as f32 / 255.0, self.b as f32 / 255.0, self.a as f32 / 255.0]
    }
}

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        /// Visitor that accepts either a hex string or a `{ r, g, b, a }` map.
        struct RgbaVisitor;

        impl<'de> de::Visitor<'de> for RgbaVisitor {
            type Value = Rgba;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a hex color string like \"#rrggbb\" or a struct { r, g, b, a }")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Rgba, E> {
                Rgba::from_hex(v)
                    .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(v), &self))
            }

            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Rgba, M::Error> {
                /// Helper struct for the `{ r, g, b, a }` format.
                #[derive(Deserialize)]
                struct RgbaFields {
                    r: u8,
                    g: u8,
                    b: u8,
                    a: u8,
                }

                let fields = RgbaFields::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(Rgba::new(fields.r, fields.g, fields.b, fields.a))
            }
        }

        deserializer.deserialize_any(RgbaVisitor)
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
