# ADR 004: wgpu Instead of Ghostty's Renderer

## Status: Accepted

## Context

Forgetty uses libghostty-vt for terminal emulation (the same engine as Ghostty). The question: why not also use Ghostty's GPU renderer?

## Ghostty's Rendering Architecture

Ghostty uses **platform-specific, compile-time selected renderers**:

| Platform | GPU API | Font System | Shader Language |
|----------|---------|-------------|-----------------|
| macOS/iOS | Metal | CoreText | MSL |
| Linux | OpenGL 4.3 | Fontconfig + FreeType | GLSL 4.3 |
| Windows | OpenGL 4.3 | FreeType (no discovery) | GLSL 4.3 |
| Browser | WebGL (stubbed) | Canvas API | - |

**Key limitations:**
- **No Vulkan, No DX12** — Ghostty explicitly does not support modern GPU APIs on Windows/Linux
- **Metal is Apple-exclusive** — The macOS renderer cannot run on any other platform
- **Separate shader codebases** — MSL and GLSL are incompatible languages requiring parallel maintenance
- **OpenGL threading issues** — GTK forces single-threaded rendering on Linux
- **Compile-time backend selection** — Each platform needs a different build configuration
- **Windows font discovery missing** — Font paths are hardcoded, no Windows font API integration
- **Browser support stubbed** — WebGL.zig is 3 lines of code

## Forgetty's wgpu Architecture

Forgetty uses **wgpu** — a single unified GPU API that runs on all platforms:

| Platform | GPU Backend(s) | Font System | Shader Language |
|----------|---------------|-------------|-----------------|
| Linux | Vulkan, OpenGL | cosmic-text (Rust) | WGSL → SPIR-V |
| macOS | Metal | cosmic-text (Rust) | WGSL → MSL |
| Windows | DX12, Vulkan | cosmic-text (Rust) | WGSL → HLSL |
| Android | Vulkan, GLES | cosmic-text (Rust) | WGSL → SPIR-V |
| Browser | WebGPU | cosmic-text (Rust) | WGSL (native) |

**Key advantages:**
- **Runtime backend selection** — wgpu picks the best available API automatically
- **One shader language** — WGSL, transpiled to MSL/HLSL/GLSL/SPIR-V via Naga
- **Vulkan + DX12 support** — Modern GPU APIs on Windows and Linux
- **True WebGPU** — Browser support via the same codebase (all major browsers ship WebGPU)
- **Android via Vulkan** — 99%+ of modern Android devices
- **Portable font rendering** — cosmic-text + glyphon are pure Rust, same on all platforms

## Why Not Fork Ghostty's Renderer

1. **Language mismatch** — Ghostty's renderer is Zig. Forgetty is Rust. Maintaining Zig rendering code alongside Rust UI code creates unnecessary complexity.

2. **Platform coupling** — Ghostty's Metal renderer is deeply integrated with Cocoa/AppKit. The OpenGL renderer is integrated with GTK. Extracting either for use with winit would require significant rework.

3. **No Windows/Android path** — Ghostty has no Vulkan or DX12 backend. Building for Windows or Android would require writing a new renderer anyway.

4. **Font system coupling** — Ghostty's font rendering uses CoreText (macOS) and FreeType (Linux) with platform-specific atlas management. Forgetty uses glyphon/cosmic-text which is platform-independent.

5. **Shader duplication** — Maintaining MSL + GLSL in parallel is error-prone. WGSL compiles to both automatically.

## Why libghostty-vt + wgpu is the Right Architecture

We get the best of both worlds:
- **libghostty-vt**: Battle-tested VT emulation, SIMD-optimized parsing, Kitty protocol, Unicode — proven by millions of Ghostty users
- **wgpu**: Cross-platform GPU rendering on 5+ platforms from a single codebase

This is exactly what the libghostty project intended. From Mitchell Hashimoto: *"libghostty has no opinion about the renderer or GUI framework used; it's even standalone WASM-compatible."*

Ghostling (official reference) uses Raylib. cmux uses Metal via full libghostty. Forgetty uses wgpu. All valid approaches — we chose the one that maximizes cross-platform reach.

## Decision

Use libghostty-vt for terminal emulation + wgpu for rendering. Write all rendering code in Rust with WGSL shaders. This enables Linux, macOS, Windows, Android, and browser support from a single codebase.
