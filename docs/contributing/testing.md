# Testing Guide

## Running Tests

```sh
# Run all workspace tests
cargo test --workspace

# Run tests for a single crate
cargo test -p forgetty-vt

# Run a specific test by name
cargo test -p forgetty-renderer test_glyph_cache_eviction

# Show stdout/stderr from tests
cargo test --workspace -- --nocapture
```

## Test Organization

### Unit Tests

Unit tests live alongside the code they test, inside a `#[cfg(test)]` module
at the bottom of each source file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_character() {
        // ...
    }
}
```

### Integration Tests

Integration tests live in `tests/` at the workspace root. These test
cross-crate interactions and end-to-end behavior.

```
tests/
  integration/    # Cross-crate integration tests
  fixtures/       # Shared test data (escape sequences, config files, etc.)
```

### Test Fixtures

Test fixtures (sample config files, VT escape sequences, etc.) go in
`tests/fixtures/`. Reference them in tests with:

```rust
let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests/fixtures/sample.toml");
```

## What to Test

- **VT parser** — Correctness of escape sequence handling. Verify cursor
  movement, styling, scrolling, and edge cases (malformed sequences, very
  long lines).
- **Renderer** — Glyph atlas allocation, damage tracking, cell-to-vertex
  conversion. GPU rendering is hard to unit test; focus on the data pipeline
  before the draw call.
- **Config** — Loading, defaults, validation, and error messages for invalid
  config.
- **PTY** — Process spawning, I/O routing, resize handling.
- **Socket API** — Request/response serialization, method dispatch, error
  codes.

## Writing Good Tests

1. **One assertion per concept.** A test should verify one behavior. Multiple
   assertions are fine if they validate the same concept.
2. **Descriptive names.** `test_scrollback_discards_oldest_lines_when_full`
   is better than `test_scrollback`.
3. **Arrange-Act-Assert.** Structure tests clearly: set up state, perform
   the action, check the result.
4. **Test edge cases.** Empty input, maximum values, Unicode boundary
   characters, concurrent access.

## Benchmarks

Performance-critical code (VT parsing, rendering) should have benchmarks in
`benches/`. We use Criterion for benchmarking:

```sh
cargo bench -p forgetty-renderer
```
