//! GPU-accelerated terminal renderer using wgpu.
//!
//! This crate handles all rendering for the Forgetty terminal, including:
//! - Text glyph rasterization and atlas management via cosmic-text/glyphon
//! - Terminal grid cell rendering via custom wgpu shaders
//! - Cursor rendering with multiple styles (block, bar, underline)
//! - Selection highlight overlay
//! - Damage tracking for efficient partial redraws
//! - Inline image rendering (iTerm2/Sixel protocols)

pub mod atlas;
pub mod color;
pub mod context;
pub mod cursor;
pub mod damage;
pub mod grid;
pub mod images;
pub mod selection;

// TODO: Phase 3 — re-export key types once renderer is implemented
