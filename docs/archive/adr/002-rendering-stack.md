# ADR-002: Rendering Stack (wgpu + glyphon)

## Status

Accepted

## Context

Forgetty needs a GPU-accelerated rendering pipeline for the terminal grid,
cursor, selection, and inline images. The renderer must work on Linux (Vulkan),
macOS (Metal), and Windows (DX12/Vulkan), with a path to Android (Vulkan).

### Options Considered

1. **wgpu + glyphon** — wgpu for GPU abstraction, glyphon (backed by
   cosmic-text) for text shaping and glyph rasterization.
2. **raw Vulkan/Metal/DX12** — Direct API usage per platform.
3. **OpenGL / glow** — OpenGL via the `glow` crate.
4. **Skia (via skia-safe)** — 2D rendering library with GPU backend.

### Evaluation

| Criteria | wgpu + glyphon | Raw APIs | OpenGL | Skia |
|----------|---------------|----------|--------|------|
| Cross-platform | All 4 platforms | Per-platform code | Deprecated on macOS | All, heavy |
| Build complexity | Moderate | Very high | Low | Very high (C++ dep) |
| Text quality | Excellent (cosmic-text) | Manual | Manual | Excellent |
| Ecosystem proof | Alacritty (wgpu planned), Zed (GPUI/wgpu) | N/A | Alacritty (current) | Chrome, Flutter |
| Binary size | ~5 MB | Minimal | ~2 MB | ~20 MB |
| Android support | Yes (Vulkan) | Yes (Vulkan) | Partial | Yes |

### Key Factors

**Cross-platform from a single codebase.** wgpu translates a single set of
API calls into Vulkan, Metal, DX12, or OpenGL (fallback) behind the scenes.
This eliminates per-platform rendering code.

**Text rendering quality.** glyphon leverages cosmic-text for text shaping,
which handles complex scripts, ligatures, and fallback fonts correctly. This
is essential for a terminal that displays code in many languages.

**Proven in the ecosystem.** Zed uses wgpu for its editor rendering (GPUI).
Alacritty has been exploring a wgpu migration. The crate is actively maintained
by the wgpu team and Mozilla contributors.

**Android via Vulkan.** wgpu's Vulkan backend works on Android, which aligns
with the planned mobile target.

## Decision

Use **wgpu** for GPU abstraction and **glyphon** (with cosmic-text) for text
shaping and glyph rasterization. The rendering pipeline in `forgetty-renderer`
will:

1. Use cosmic-text to shape text runs and rasterize glyphs.
2. Cache rasterized glyphs in a GPU texture atlas.
3. Build per-cell vertex buffers with cell background color, glyph texture
   coordinates, and cursor/selection overlays.
4. Submit a single render pass per frame via wgpu.
5. Use damage tracking from `forgetty-vt` to minimize per-frame work.

Custom WGSL shaders (`crates/forgetty-renderer/src/shaders/cell.wgsl`) handle
cell compositing on the GPU.

## Consequences

**Benefits:**
- Single rendering codebase for Linux, macOS, Windows, and Android.
- High-quality text rendering with ligatures and Unicode support.
- Damage-tracked rendering keeps GPU utilization low during idle periods.
- Well-maintained dependencies with strong community support.

**Costs:**
- wgpu adds ~5 MB to the binary size.
- Shader debugging requires familiarity with WGSL and wgpu's error reporting.
- Users with very old GPUs (no Vulkan/Metal/DX12) fall back to OpenGL,
  which may have rendering differences.

**Mitigations:**
- Binary size is acceptable for a desktop application.
- wgpu's validation layer catches most shader errors at development time.
- OpenGL fallback covers older hardware adequately for a terminal use case.
