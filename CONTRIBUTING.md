# Contributing to Forgetty

Thank you for your interest in contributing to Forgetty! This guide will help
you get started, whether you're reporting a bug, suggesting a feature, or
submitting code.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Reporting Bugs](#reporting-bugs)
- [Suggesting Features](#suggesting-features)
- [Development Setup](#development-setup)
- [Code Style](#code-style)
- [Commit Messages](#commit-messages)
- [Pull Request Process](#pull-request-process)
- [License](#license)

## Code of Conduct

This project follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md).
By participating, you agree to uphold this code. Please report unacceptable
behavior to conduct@totemlabsforge.com.

## Reporting Bugs

Found a bug? We'd like to fix it. Please
[open an issue](https://github.com/totem-labs-forge/forgetty/issues/new?template=bug_report.md)
using the bug report template.

A good bug report includes:

1. **A clear title** that summarizes the problem.
2. **Steps to reproduce** the issue, as minimal as possible.
3. **Expected behavior** — what you thought would happen.
4. **Actual behavior** — what actually happened, including any error output.
5. **Environment details** — OS, Forgetty version (`forgetty --version`),
   terminal info, GPU driver version if rendering-related.

If you can include a short screen recording or screenshot, even better.

## Suggesting Features

Feature ideas are welcome. Please
[open an issue](https://github.com/totem-labs-forge/forgetty/issues/new?template=feature_request.md)
using the feature request template.

When proposing a feature:

- Explain the problem it solves or the use case it enables.
- Describe the behavior you'd like to see.
- Note any alternatives you've considered.
- If the feature is large, consider opening a discussion first.

## Development Setup

### Prerequisites

| Tool   | Version | Notes                                    |
|--------|---------|------------------------------------------|
| Rust   | 1.85+   | Install via [rustup](https://rustup.rs/) |
| Zig    | 0.13+   | Required for building libghostty-vt      |
| Git    | 2.x     | With submodule support                   |

**Linux additional packages:**

```sh
# Debian/Ubuntu
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev libfontconfig-dev

# Fedora
sudo dnf install libX11-devel libxkbcommon-devel wayland-devel fontconfig-devel

# Arch
sudo pacman -S libx11 libxkbcommon wayland fontconfig
```

### Building

```sh
# Clone the repository with submodules
git clone --recursive https://github.com/totem-labs-forge/forgetty.git
cd forgetty

# Build in debug mode (faster compilation)
cargo build

# Build in release mode (optimized)
cargo build --release

# Run the application
cargo run
```

### Running Tests

```sh
# Run all tests
cargo test --workspace

# Run tests for a specific crate
cargo test -p forgetty-render

# Run tests with output
cargo test --workspace -- --nocapture
```

### Useful Commands

```sh
# Check formatting
cargo fmt --all -- --check

# Run clippy lints
cargo clippy --workspace --all-targets -- -D warnings

# Run cargo-deny checks (license, advisories, duplicates)
cargo deny check

# Generate documentation
cargo doc --workspace --no-deps --open
```

## Code Style

We use `rustfmt` and `clippy` to maintain consistent code style. CI will reject
PRs that don't pass both.

### General guidelines

- **Format your code** with `cargo fmt --all` before committing.
- **Fix all clippy warnings** — run `cargo clippy --workspace --all-targets -- -D warnings`.
- **Write documentation** for all public items. Use `///` doc comments with
  examples where appropriate.
- **Keep functions focused.** If a function is doing too many things, break it
  up.
- **Prefer explicit error handling** over `.unwrap()` in library code. Binary
  crate entry points and tests may use `.unwrap()` or `?` as appropriate.
- **Use `thiserror`** for defining error types in library crates.
- **Name things clearly.** A longer, descriptive name is better than a short,
  ambiguous one.

See [docs/contributing/code-style.md](docs/contributing/code-style.md) for the
full code style guide.

## Commit Messages

We follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).

### Format

```
<type>(<scope>): <description>

[optional body]

[optional footer(s)]
```

### Types

| Type       | Description                                  |
|------------|----------------------------------------------|
| `feat`     | A new feature                                |
| `fix`      | A bug fix                                    |
| `docs`     | Documentation only changes                   |
| `style`    | Formatting, missing semicolons, etc.         |
| `refactor` | Code change that neither fixes nor adds      |
| `perf`     | Performance improvement                      |
| `test`     | Adding or updating tests                     |
| `build`    | Build system or external dependency changes  |
| `ci`       | CI configuration changes                     |
| `chore`    | Other changes that don't modify src or tests |

### Scope

Use the crate name without the `forgetty-` prefix when the change is
crate-specific:

```
feat(render): add subpixel glyph positioning
fix(pty): handle SIGWINCH on FreeBSD
docs(socket): document workspace.list RPC method
```

### Examples

```
feat(ui): add vertical tab bar with git branch display

Add a vertical tab bar that shows the git branch, working directory,
and running command for each session. The tab bar auto-hides when
only one tab is open.

Closes #42
```

```
fix(vt): correct wide character cursor positioning

Wide characters (CJK, emoji) were causing the cursor to be positioned
one cell too far to the right. This aligns cursor tracking with the
actual cell width reported by libghostty-vt.
```

## Pull Request Process

1. **Fork and branch.** Create a feature branch from `main`:
   ```sh
   git checkout -b feat/my-feature main
   ```

2. **Make your changes.** Keep the scope focused — one feature or fix per PR.

3. **Write tests.** New features should include tests. Bug fixes should include
   a regression test when feasible.

4. **Ensure CI passes locally:**
   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

5. **Push and open a PR.** Fill out the PR template completely. Link any related
   issues.

6. **Respond to review feedback.** We aim to review PRs within a few days.
   Please be responsive to comments — we'll work with you to get the PR merged.

### What we look for in review

- **Correctness** — Does it do what it claims?
- **Tests** — Are edge cases covered?
- **Documentation** — Are public APIs documented?
- **Performance** — Are there obvious performance pitfalls?
- **Style** — Does it follow the project's conventions?

### After merge

Your contribution will be included in the next release and credited in the
changelog. Thank you!

## License

By contributing to Forgetty, you agree that your contributions will be licensed
under the [MIT License](LICENSE). This is the same license that covers the
project, so your contributions are available under the same terms as the rest of
the codebase.
