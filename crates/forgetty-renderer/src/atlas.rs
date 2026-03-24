//! Glyph atlas for efficient text rendering.
//!
//! Manages a texture atlas that caches rasterized glyphs. Glyphs are
//! rasterized on demand using cosmic-text and packed into the atlas
//! texture for efficient GPU rendering.

// TODO: Phase 3 — implement GlyphAtlas
//
// pub struct GlyphAtlas {
//     texture: wgpu::Texture,
//     entries: HashMap<GlyphKey, AtlasEntry>,
//     packer: AtlasPacker,
// }
