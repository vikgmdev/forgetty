# Configuration Reference

Forgetty is configured via a TOML file located at:

| Platform | Path |
|----------|------|
| Linux    | `~/.config/forgetty/config.toml` |
| macOS    | `~/Library/Application Support/forgetty/config.toml` |
| Windows  | `%APPDATA%\forgetty\config.toml` |

A default configuration file is created automatically on first launch. Changes
are applied live -- Forgetty watches the config file and reloads on save.

## Full Schema

```toml
# ~/.config/forgetty/config.toml

# ──────────────────────────────────────────────
# Font
# ──────────────────────────────────────────────

[font]
# Font family for regular text. Forgetty uses the system font fallback
# chain for missing glyphs.
family = "JetBrains Mono"

# Font size in points.
size = 13.0

# Line height as a multiplier of the font's default line height.
# 1.0 = default, 1.2 = 20% extra spacing.
line_height = 1.0

# Enable font ligatures (e.g., -> becomes an arrow in supported fonts).
ligatures = true

# ──────────────────────────────────────────────
# Window
# ──────────────────────────────────────────────

[window]
# Initial window width and height in pixels.
width = 1200
height = 800

# Window opacity (0.0 = fully transparent, 1.0 = fully opaque).
# Requires a compositor that supports transparency.
opacity = 1.0

# Window padding in pixels (space between the terminal grid and window edge).
padding = 8

# Enable window decorations (title bar, borders).
# Set to false for a borderless window.
decorations = true

# ──────────────────────────────────────────────
# Theme
# ──────────────────────────────────────────────

[theme]
# Name of the built-in theme to use, or a path to a custom theme TOML file.
# Built-in themes: "default-dark", "default-light"
name = "default-dark"

# ──────────────────────────────────────────────
# Cursor
# ──────────────────────────────────────────────

[cursor]
# Cursor style: "block", "beam", or "underline".
style = "block"

# Whether the cursor blinks.
blink = true

# Blink interval in milliseconds.
blink_interval = 500

# ──────────────────────────────────────────────
# Scrollback
# ──────────────────────────────────────────────

[scrollback]
# Maximum number of lines to keep in the scrollback buffer.
lines = 10000

# ──────────────────────────────────────────────
# Shell
# ──────────────────────────────────────────────

[shell]
# Path to the shell executable. If empty, Forgetty uses the system
# default ($SHELL on Unix, %COMSPEC% on Windows).
program = ""

# Additional arguments passed to the shell.
args = []

# Working directory for new tabs. If empty, uses the current
# working directory of the focused pane (or $HOME for the first tab).
working_directory = ""

# ──────────────────────────────────────────────
# Tabs
# ──────────────────────────────────────────────

[tabs]
# Tab bar position: "left" (vertical) or "top" (horizontal).
position = "left"

# Width of the vertical tab bar in pixels (only used when position = "left").
width = 200

# Show the git branch in the tab title.
show_git_branch = true

# Show the current working directory in the tab title.
show_cwd = true

# Show the running command in the tab title.
show_command = true

# ──────────────────────────────────────────────
# Keybindings
# ──────────────────────────────────────────────

# Keybindings are defined as key = action pairs. Modifier keys are
# joined with "+". Examples: "ctrl+shift+t", "cmd+d", "alt+1".

[keybindings]
new_tab = "ctrl+shift+t"
close_tab = "ctrl+shift+w"
next_tab = "ctrl+tab"
prev_tab = "ctrl+shift+tab"
split_vertical = "ctrl+shift+d"
split_horizontal = "ctrl+shift+e"
close_pane = "ctrl+shift+x"
focus_up = "ctrl+shift+up"
focus_down = "ctrl+shift+down"
focus_left = "ctrl+shift+left"
focus_right = "ctrl+shift+right"
copy = "ctrl+shift+c"
paste = "ctrl+shift+v"
command_palette = "ctrl+shift+p"
zoom_in = "ctrl+="
zoom_out = "ctrl+-"
zoom_reset = "ctrl+0"

# ──────────────────────────────────────────────
# Notifications
# ──────────────────────────────────────────────

[notifications]
# Enable AI agent attention notifications. When a background pane's
# output matches a notification pattern, the pane's tab shows a
# notification ring.
enabled = true

# Patterns that trigger a notification (regex). Applied to each
# new line of terminal output.
patterns = [
  "\\? ",           # Interactive prompt (y/n)
  "error\\[",       # Rust compiler error
  "Error:",         # General error
  "FAILED",         # Test failure
  "Press .* to continue",
]

# ──────────────────────────────────────────────
# Socket API
# ──────────────────────────────────────────────

[socket]
# Enable the JSON-RPC socket API.
enabled = true

# Path to the Unix domain socket. Use "auto" to place it in the
# XDG runtime directory (Linux) or a temporary directory.
path = "auto"

# ──────────────────────────────────────────────
# Workspace
# ──────────────────────────────────────────────

[workspace]
# Automatically save and restore sessions on restart.
auto_restore = true

# Directory where workspace session files are stored.
# Default: platform config directory / "sessions"
session_dir = ""
```

## Theme Files

Theme files follow the same format as `resources/themes/default-dark.toml`.
Place custom themes anywhere on disk and reference them by path:

```toml
[theme]
name = "/home/user/.config/forgetty/themes/my-theme.toml"
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `FORGETTY_CONFIG` | Override the config file path |
| `FORGETTY_LOG` | Set the log level (`trace`, `debug`, `info`, `warn`, `error`) |
| `FORGETTY_SOCKET` | Override the socket path |
