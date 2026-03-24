# Architecture

This document describes Forgetty's internal architecture, crate dependency
graph, and data flow.

## High-Level Overview

Forgetty is a Cargo workspace composed of 10 crates, each with a focused
responsibility. The binary crate (`forgetty`) ties everything together.

```
                        ┌──────────────┐
                        │   forgetty   │  (binary crate)
                        │  CLI + App   │
                        └──────┬───────┘
                               │
            ┌──────────────────┼──────────────────┐
            │                  │                  │
     ┌──────▼──────┐   ┌──────▼──────┐   ┌───────▼───────┐
     │ forgetty-ui │   │  forgetty-  │   │   forgetty-   │
     │  Windows,   │   │  workspace  │   │    socket     │
     │ Tabs, Panes │   │  Sessions   │   │  JSON-RPC API │
     └──────┬──────┘   └──────┬──────┘   └───────────────┘
            │                 │
     ┌──────▼──────┐   ┌─────▼───────┐
     │  forgetty-  │   │  forgetty-  │
     │  renderer   │   │   watcher   │
     │ wgpu + GPU  │   │ File notify │
     └──────┬──────┘   └─────────────┘
            │
     ┌──────▼──────┐   ┌─────────────┐
     │ forgetty-vt │   │  forgetty-  │
     │ libghostty  │   │   viewer    │
     │  VT parser  │   │  MD/Images  │
     └──────┬──────┘   └─────────────┘
            │
     ┌──────▼──────┐
     │ forgetty-   │
     │    pty      │
     │ PTY spawn   │
     └─────────────┘

     ┌─────────────┐   ┌─────────────┐
     │  forgetty-  │   │  forgetty-  │
     │   config    │   │    core     │
     │ TOML loader │   │ Shared types│
     └─────────────┘   └─────────────┘
```

## Data Flow

The terminal data pipeline flows from the shell process through several
transformation stages before reaching the screen:

```
  Shell Process (bash, zsh, fish, ...)
       │
       │  raw bytes (stdout + stderr)
       ▼
  ┌─────────────┐
  │ forgetty-   │  Spawns the child process, manages the PTY file
  │    pty      │  descriptor, handles SIGWINCH for resize.
  └──────┬──────┘
         │  byte stream
         ▼
  ┌─────────────┐
  │ forgetty-vt │  Feeds bytes into libghostty-vt's SIMD-accelerated
  │  VT parser  │  parser. Produces terminal state: a grid of styled
  └──────┬──────┘  cells, cursor position, scrollback, selection.
         │
         │  terminal state (cells, cursor, dirty rows)
         ▼
  ┌─────────────┐
  │  forgetty-  │  Reads the terminal grid. Rasterizes glyphs via
  │  renderer   │  glyphon/cosmic-text into a glyph atlas. Builds
  └──────┬──────┘  GPU vertex buffers for dirty cells only (damage
         │         tracking). Submits draw calls via wgpu.
         │
         │  wgpu render pass
         ▼
  ┌─────────────┐
  │     GPU     │  Composites cell backgrounds, glyph textures,
  │  (Vulkan /  │  cursor, selection highlights, and images into
  │  Metal /    │  the final framebuffer.
  │  DX12)      │
  └─────────────┘
```

### Input Flow (reverse direction)

```
  Keyboard / Mouse event
       │
       ▼
  ┌─────────────┐
  │ forgetty-ui │  winit event loop captures input. Keybinding
  │   input     │  layer checks for terminal-level bindings
  └──────┬──────┘  (copy, paste, new tab, split, etc.).
         │
         │  unhandled keys → escape sequences
         ▼
  ┌─────────────┐
  │ forgetty-   │  Writes escape sequences to the PTY master fd.
  │    pty      │  The child process reads them as stdin.
  └─────────────┘
```

## Crate Descriptions

### `forgetty` (binary)

The entry point. Parses CLI arguments with `clap`, loads configuration,
initializes the event loop, and wires all subsystems together. Contains the
top-level application state machine.

### `forgetty-core`

Shared types and utilities used across the workspace: color types, geometry
primitives (`Size`, `Point`, `Rect`), error definitions, and the central
`Event` enum that flows through the event bus.

### `forgetty-config`

Loads and validates `~/.config/forgetty/config.toml`. Provides typed access
to all configuration values with defaults. Handles live-reload: watches the
config file for changes and emits update events.

### `forgetty-vt`

Rust FFI bindings to [libghostty-vt](https://github.com/ghostty-org/ghostty).
Contains a `build.rs` that invokes Zig to compile the C library, then
generates Rust bindings. Exposes a safe `Terminal` API over the raw FFI:
creating terminals, feeding input, reading cell grids, handling selection,
and querying terminal state.

### `forgetty-pty`

Cross-platform PTY abstraction. On Unix, uses `posix_openpt` / `forkpty`.
On Windows, uses ConPTY. Provides a `Multiplexer` that manages multiple PTY
sessions and routes I/O through async channels.

### `forgetty-renderer`

The GPU rendering pipeline. Uses `wgpu` for cross-platform GPU access and
`glyphon` (backed by `cosmic-text`) for text shaping and rasterization.
Key subsystems:

- **Glyph atlas** — Caches rasterized glyphs in a GPU texture atlas.
- **Grid renderer** — Converts terminal cells into GPU vertex buffers.
- **Damage tracking** — Only re-renders rows that changed since the last frame.
- **Cursor renderer** — Draws block, beam, and underline cursor styles.
- **Selection renderer** — Highlights selected text regions.
- **Image renderer** — Renders inline images (Kitty graphics protocol).

### `forgetty-ui`

Window management and user interaction. Built on `winit` for cross-platform
windowing. Manages the tab bar (vertical, showing git branch and CWD), pane
tree (splits), keybinding dispatch, clipboard operations, and notification
overlays.

### `forgetty-viewer`

Embedded content viewer for markdown files and images. Uses `wry` (WebView)
to render markdown (converted via `pulldown-cmark`) with syntax highlighting.
Supports inline image display. Integrates with `forgetty-watcher` for
auto-refresh on file save.

### `forgetty-watcher`

Thin wrapper around the `notify` crate for filesystem watching. Watches
config files, viewed markdown/image files, and project directories for
changes. Emits debounced events into the main event bus.

### `forgetty-workspace`

Workspace and session management. Groups tabs into named workspaces.
Persists workspace layouts (tab arrangement, split geometry, working
directories, scroll positions) to disk. Restores full sessions on restart.

### `forgetty-socket`

JSON-RPC server over Unix domain sockets (named pipes on Windows). Exposes
the terminal's functionality to external tools: creating tabs, sending input,
reading output, listing workspaces, and more. Enables AI agents and editor
integrations to drive the terminal programmatically.

## Threading Model

Forgetty uses a hybrid async/sync architecture:

- **Main thread** — Runs the `winit` event loop, handles windowing events,
  and drives the renderer. All GPU work happens here.
- **PTY I/O tasks** — Each PTY session spawns a Tokio task for reading output
  and another for writing input. Data flows through `tokio::sync` channels.
- **VT parsing** — Happens on the main thread, triggered by PTY read events.
  libghostty-vt's SIMD parser is fast enough that this doesn't block rendering.
- **Socket server** — Runs as a Tokio task, accepting connections and
  dispatching JSON-RPC calls to the main event bus.
- **File watchers** — Run on a background thread managed by the `notify` crate.
