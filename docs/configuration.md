# Configuration Reference

Forgetty is configured via a TOML file at `~/.config/forgetty/config.toml`. The file is created automatically on first launch with sensible defaults.

**Hot reload:** Forgetty watches the config file and applies changes instantly to all panes. No restart needed. If you save a malformed config, the previous working config is preserved.

You can also edit font, theme, and size visually using the appearance sidebar (`Ctrl+,`).

## Config file location

| Platform | Path |
|----------|------|
| Linux | `$XDG_CONFIG_HOME/forgetty/config.toml` (default: `~/.config/forgetty/config.toml`) |

Override with `--config-file <PATH>` on the command line.

## Full example

```toml
font_family = "JetBrains Mono"
font_size = 14.0
theme = "Catppuccin Mocha"
shell = "/bin/zsh"
scrollback_lines = 10000
cursor_style = "block"
bell_mode = "visual"
```

All fields are optional. Omitted fields use defaults.

## Fields

### `font_family`

**Type:** String
**Default:** `"monospace"`

Font family name for terminal text. Uses Fontconfig for font discovery — any font installed on your system works. Monospace fonts are recommended.

```toml
font_family = "JetBrains Mono"
font_family = "Fira Code"
font_family = "monospace"       # system default monospace
```

### `font_size`

**Type:** Float
**Default:** `12.0`

Font size in points. Runtime zoom (`Ctrl+=`/`Ctrl+-`) adjusts this per-pane without changing the config.

```toml
font_size = 14.0
```

Minimum: 6.0. Maximum: 72.0.

### `theme`

**Type:** String
**Default:** `"0x96f"`

Name of the color theme. Forgetty ships with 486 built-in themes (all from [iTerm2-Color-Schemes](https://github.com/mbadolato/iTerm2-Color-Schemes) plus a default dark theme).

```toml
theme = "Catppuccin Mocha"
theme = "Dracula"
theme = "Solarized Dark"
theme = "Tokyo Night"
theme = "Nord"
theme = "Gruvbox Dark"
```

Browse all themes interactively with the appearance sidebar (`Ctrl+,`). Arrow keys preview themes live — Enter to apply, Escape to revert.

**Custom themes:** Drop a `.toml` file in `~/.config/forgetty/themes/` and reference it by name. Custom themes override bundled themes with the same name.

**Theme aliases:** Common aliases are built in — "Solarized Dark" maps to "Solarized Dark - Patched", "Tokyo Night" maps to "tokyonight", etc.

### `shell`

**Type:** String (optional)
**Default:** not set (auto-detect)

Shell to launch in new terminals. When not set, Forgetty uses the `$SHELL` environment variable, falls back to `/etc/passwd`, then `/bin/sh`.

```toml
shell = "/bin/zsh"
shell = "/usr/bin/fish"
```

### `scrollback_lines`

**Type:** Integer
**Default:** `10000`

Maximum number of lines retained in the scrollback buffer per pane.

```toml
scrollback_lines = 50000
scrollback_lines = 0            # disable scrollback
```

### `cursor_style`

**Type:** Enum
**Default:** `"block"`

Cursor shape. Applications can override this via DECSCUSR escape sequences (e.g., vim uses bar cursor in insert mode).

| Value | Description |
|-------|-------------|
| `"block"` | Filled block cursor |
| `"bar"` | Thin vertical bar |
| `"underline"` | Horizontal underline |
| `"block_hollow"` | Unfilled block outline |

```toml
cursor_style = "bar"
```

### `bell_mode`

**Type:** Enum
**Default:** `"visual"`

How the terminal responds to the BEL character (0x07).

| Value | Description |
|-------|-------------|
| `"visual"` | Brief white flash overlay on the pane |
| `"audio"` | System beep via the desktop |
| `"both"` | Flash and beep |
| `"none"` | Ignore bells silently |

```toml
bell_mode = "none"
```

### `keybindings`

**Type:** Map of action name to key combination
**Default:** empty (uses built-in defaults)

Custom keybindings using GTK4 accelerator format.

```toml
[keybindings]
# Custom keybindings are not yet supported. Planned for a future release.
# Default shortcuts are listed in the README and viewable via F1 in-app.
```

## Custom theme format

Create a `.toml` file in `~/.config/forgetty/themes/`:

```toml
name = "My Theme"

[colors]
foreground = "#cdd6f4"
background = "#1e1e2e"
cursor = "#f5e0dc"
selection = "#585b70"

[colors.ansi]
black = "#45475a"
red = "#f38ba8"
green = "#a6e3a1"
yellow = "#f9e2af"
blue = "#89b4fa"
magenta = "#f5c2e7"
cyan = "#94e2d5"
white = "#bac2de"

[colors.bright]
black = "#585b70"
red = "#f38ba8"
green = "#a6e3a1"
yellow = "#f9e2af"
blue = "#89b4fa"
magenta = "#f5c2e7"
cyan = "#94e2d5"
white = "#cdd6f4"
```

All color values are `#rrggbb` hex strings. The `cursor` and `selection` fields are optional — they fall back to theme defaults if omitted.

## Environment variables

Forgetty sets these on every spawned shell:

| Variable | Value | Notes |
|----------|-------|-------|
| `TERM` | `xterm-256color` | Terminal type for 256-color support |
| `COLORTERM` | `truecolor` | Indicates 24-bit true color support |
| `TERM_PROGRAM` | `forgetty` | Terminal program identifier |
| `TERM_PROGRAM_VERSION` | `0.1.0-beta` | Version from build time |

## CLI flags

```
forgetty [OPTIONS]

Options:
  --working-directory <PATH>   Start in this directory
  -e, --execute <CMD> [ARGS]   Run a command instead of default shell
  --config-file <PATH>         Use alternate config file
  --class <CLASS>              Set WM_CLASS for window manager rules
  -V, --version                Show version
  -h, --help                   Show help
```

Flags like `--working-directory` and `--config-file` must come before `-e`:

```sh
forgetty --working-directory ~/projects -e nvim server.log
```
