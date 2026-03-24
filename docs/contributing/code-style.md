# Code Style Guide

This document expands on the guidelines in [CONTRIBUTING.md](../../CONTRIBUTING.md).

## Formatting

All Rust code must be formatted with `rustfmt`. The project's formatting rules
are defined in `.rustfmt.toml`:

- Maximum line width: 100 characters
- Edition: 2021
- Heuristics: `Max` (allows longer expressions before wrapping)

Run `cargo fmt --all` before every commit.

## Linting

All clippy warnings are treated as errors in CI. Run:

```sh
cargo clippy --workspace --all-targets
```

The `clippy.toml` at the workspace root configures project-specific lint
thresholds (e.g., `too-many-arguments-threshold = 8`).

## Naming Conventions

- **Crates:** `forgetty-<name>` (lowercase, hyphenated).
- **Modules:** `snake_case`.
- **Types:** `PascalCase`. Prefer descriptive names over abbreviations.
- **Functions:** `snake_case`. Start with a verb when possible (`create_tab`,
  `parse_escape`, `render_grid`).
- **Constants:** `SCREAMING_SNAKE_CASE`.
- **Trait names:** Use adjectives or capabilities (`Renderable`, `Scrollable`)
  rather than nouns.

## Error Handling

- Use `thiserror` for error types in library crates.
- Every crate should define its own error enum in an `error.rs` module.
- Avoid `.unwrap()` in library code. Use `?` with proper error context.
- Binary crate entry points may use `.unwrap()` for unrecoverable startup
  errors, but prefer `.expect("reason")` with a message.

## Documentation

- All `pub` items must have `///` doc comments.
- Include a one-line summary, then a blank line, then details if needed.
- Add `# Examples` sections for non-trivial public APIs.
- Use `#[doc(hidden)]` for public items that are implementation details.

## Imports

- Group imports in this order, separated by blank lines:
  1. `std` library
  2. External crates
  3. Workspace crates (`forgetty-*`)
  4. Current crate modules (`crate::`, `super::`)
- Prefer explicit imports over glob imports (`use module::*`).

## Unsafe Code

- Avoid `unsafe` unless absolutely necessary (e.g., FFI in `forgetty-vt`).
- Every `unsafe` block must have a `// SAFETY:` comment explaining why it is
  sound.
- Wrap unsafe FFI calls in safe Rust abstractions.

## Testing

- Unit tests go in a `#[cfg(test)] mod tests` block at the bottom of the
  source file.
- Integration tests go in the `tests/` directory at the workspace root.
- Name test functions descriptively: `test_cursor_moves_right_on_character_input`
  rather than `test_cursor_1`.
