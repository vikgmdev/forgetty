# ADR-001: Use libghostty-vt for Terminal Emulation

## Status

Accepted

## Context

Forgetty needs a VT terminal emulation library to parse escape sequences and
maintain terminal state (cell grid, cursor, scrollback, selection). The two
primary candidates are:

1. **alacritty_terminal** — The terminal emulation core extracted from
   Alacritty. Mature, well-tested, pure Rust.
2. **libghostty-vt** — The terminal emulation core from Ghostty, exposed as a
   C library. Written in Zig with SIMD-accelerated parsing.

### Evaluation Criteria

| Criteria | alacritty_terminal | libghostty-vt |
|----------|--------------------|---------------|
| Parse throughput | Good (~800 MB/s) | Excellent (~2.5 GB/s with SIMD) |
| Incremental render API | No (full grid scan) | Yes (dirty row tracking) |
| Kitty graphics protocol | Partial | Full |
| Unicode correctness | Good | Excellent (ICU-grade) |
| WASM target support | Not tested | Designed for it (Zig cross-compiles) |
| External dependencies | Several Rust crates | Zero (self-contained Zig) |
| API stability | Unstable (no semver promise) | C ABI (stable across versions) |
| Build complexity | Simple (cargo) | Requires Zig toolchain |

### Key Factors

**Performance.** libghostty-vt's SIMD-accelerated parser is roughly 3x faster
than alacritty_terminal for bulk throughput. For an AI-agent-oriented terminal
where large amounts of output (build logs, test results) are common, this
matters.

**Incremental rendering.** libghostty-vt tracks which rows have changed since
the last read, enabling the renderer to update only dirty cells. alacritty_terminal
requires scanning the entire grid each frame to detect changes.

**Kitty graphics protocol.** Full support in libghostty-vt means we can
render inline images out of the box, which is important for the embedded
viewer feature and AI agent output.

**WASM / Android.** Zig's cross-compilation story makes it straightforward to
target WASM (for a future web version) and Android (via Vulkan). This aligns
with Forgetty's cross-platform goals.

**Build complexity.** The main downside is requiring a Zig compiler in the
build chain. This adds friction for contributors but is mitigated by clear
documentation and CI handling.

## Decision

Use **libghostty-vt** as the terminal emulation backend. The `forgetty-vt`
crate will contain:

- A `build.rs` that invokes the Zig build system to compile libghostty-vt.
- Rust FFI bindings generated from the C headers.
- A safe Rust wrapper API (`Terminal`, `Screen`, `Selection`).

libghostty-vt is included as a Git submodule to pin a specific version.

## Consequences

**Benefits:**
- Significantly faster parsing throughput.
- Dirty row tracking reduces GPU work per frame.
- Full Kitty graphics protocol support from day one.
- Path to WASM and Android targets.
- C ABI provides a stable interface across Ghostty releases.

**Costs:**
- Contributors must install Zig (0.13+) to build from source.
- The FFI boundary adds some complexity compared to a pure Rust library.
- Debugging terminal emulation issues may require reading Zig source.
- We depend on Ghostty's release cadence for bug fixes in the VT core.

**Mitigations:**
- Clear build docs with per-platform Zig installation instructions.
- The safe Rust wrapper in `forgetty-vt` isolates FFI unsafety.
- Pinning via Git submodule ensures reproducible builds.
