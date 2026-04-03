# ADR 005: Windows Terminal Technical Comparison

## Status: Reference Document

## Context

Windows Terminal (github.com/microsoft/terminal) is the benchmark for terminal quality on Windows. When Forgetty ports to Windows (T-037), we need to understand WT's architecture to beat it.

## Windows Terminal Architecture

- **Language:** C++ (12.3M lines, 6+ years development)
- **UI:** WinUI 2 via XAML Islands in a Win32 host window
- **Rendering:** Direct3D 11 with custom HLSL shaders (Atlas Engine)
- **Text:** DirectWrite with ClearType subpixel AA
- **VT Parser:** Custom (no SIMD, character-by-character)
- **PTY:** ConPTY (their invention — pseudo-console over named pipes)
- **Settings:** JSON + full GUI editor
- **Platform:** Windows only, forever (deep Win32/COM/XAML dependencies)

## Three-Way Comparison

| Aspect | Windows Terminal | Ghostty | Forgetty |
|---|---|---|---|
| Language | C++ (12.3M lines) | Zig | Rust (~20K lines) |
| GPU API | Direct3D 11 | Metal/OpenGL | GTK4/Pango (Linux), wgpu (future Windows) |
| Text quality | DirectWrite ClearType | CoreText/FreeType | Pango/FreeType (Linux) |
| VT parser | Custom (no SIMD) | libghostty (SIMD) | libghostty-vt (SIMD) |
| Input latency | ~66ms (XAML overhead) | ~5ms | ~5ms |
| Themes | ~15 bundled | 200+ (CLI browser) | 486 with live preview |
| Session persistence | No | No | Planned |
| Workspaces | No | No | Planned |
| AI features | None | None | Planned |
| Profiles | Rich (icons, per-profile themes) | No | Planned |
| Command palette | Yes | Yes | Yes |
| Quake mode | Yes | No | Planned |
| Cross-platform | No (Windows only) | macOS + Linux | Linux + Windows + Android planned |

## Where WT Beats Us (and how to close the gap)

1. **Shell profiles with icons** — Rich profile system with auto-discovery (WSL distros self-register). Our T-M1-extra-006 must match this.
2. **Settings fragments** — Third-party tools drop JSON files to add profiles. Consider `~/.config/forgetty/profiles.d/*.toml`.
3. **Full actions/keybindings GUI editor** — Every action editable. Our T-M1-extra-007.
4. **Monarch/Peasant multi-window** — Sophisticated coordination between instances.
5. **Procedural box-drawing in shader** — Pixel-perfect. Not applicable to our Pango approach.

## Where We Beat WT

1. **Input latency** — 5ms vs 66ms (XAML overhead kills WT's responsiveness)
2. **Theme browser** — 486 themes with LIVE preview vs 15 static themes
3. **VT parser** — SIMD-optimized libghostty-vt vs conventional character-by-character
4. **Session persistence** — WT loses everything on close
5. **Workspaces** — WT has no concept of named workspaces
6. **AI features** — Notifications, smart copy, viewer — WT has nothing
7. **Cross-platform** — WT is locked to Windows forever
8. **Startup time** — Fast vs slow XAML initialization
9. **Config hot reload** — Instant vs partial restart required
10. **Codebase** — 20K lines Rust vs 12.3M lines C++

## Key Decisions for Forgetty's Windows Port (T-037)

1. **Use ConPTY** via `portable-pty` — VT output feeds into libghostty-vt (same pipeline as Linux)
2. **Use wgpu → DX12 or DirectWrite + winit** for rendering (not WT's XAML Islands approach)
3. **Target <10ms latency** — WT's 66ms is our opportunity
4. **Beat WT on missing features** — session persistence, workspaces, AI notifications, 486 themes
5. **Skip XAML Islands** — enormous complexity, even WT's team acknowledges pain points
6. **Match WT on profiles** — the one feature Windows users will demand
