# Building from Source

This guide covers building Forgetty from source on all supported platforms.

## Prerequisites

| Tool | Minimum Version | Purpose |
|------|-----------------|---------|
| Rust | 1.85+ | Core language toolchain |
| Zig  | 0.15.2+ | Compiles libghostty-vt (C/Zig library); also used for Android cross-compile |
| Git  | 2.x | Clone with submodule support |

### Installing Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

The project includes a `rust-toolchain.toml` that pins the stable channel with
`rustfmt` and `clippy` components. Rustup will install the correct toolchain
automatically.

### Installing Zig

Zig is required because libghostty-vt is written in Zig/C and the build script
(`crates/forgetty-vt/build.rs`) invokes the Zig compiler.

Download from [ziglang.org/download](https://ziglang.org/download/) or use a
package manager:

```sh
# macOS
brew install zig

# Arch Linux
sudo pacman -S zig

# Snap (Ubuntu/Debian)
snap install zig --classic --beta
```

## Linux

### System Dependencies

Forgetty requires several system libraries for windowing, input, and font
rendering.

**Debian / Ubuntu:**

```sh
sudo apt update
sudo apt install -y \
  build-essential \
  pkg-config \
  libx11-dev \
  libxkbcommon-dev \
  libwayland-dev \
  libfontconfig-dev \
  libfreetype-dev
```

**Fedora:**

```sh
sudo dnf install -y \
  gcc \
  pkg-config \
  libX11-devel \
  libxkbcommon-devel \
  wayland-devel \
  fontconfig-devel \
  freetype-devel
```

**Arch Linux:**

```sh
sudo pacman -S --needed \
  base-devel \
  libx11 \
  libxkbcommon \
  wayland \
  fontconfig \
  freetype2
```

### Build

```sh
git clone --recursive https://github.com/vikgmdev/forgetty.git
cd forgetty
cargo build --release
```

The binary is at `target/release/forgetty`.

## macOS

### System Dependencies

Install Xcode command-line tools:

```sh
xcode-select --install
```

No additional libraries are needed; macOS provides the required frameworks
(Metal, CoreText, AppKit) out of the box.

### Build

```sh
git clone --recursive https://github.com/vikgmdev/forgetty.git
cd forgetty
cargo build --release
```

The binary is at `target/release/forgetty`.

Both `x86_64-apple-darwin` and `aarch64-apple-darwin` (Apple Silicon) are
supported. Cargo builds for the host architecture by default. To cross-compile:

```sh
rustup target add aarch64-apple-darwin
cargo build --release --target aarch64-apple-darwin
```

## Windows

### System Dependencies

Install [Visual Studio Build Tools 2022](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
with the "Desktop development with C++" workload.

Zig can be installed via:

```powershell
winget install zig.zig
```

Or download directly from [ziglang.org/download](https://ziglang.org/download/).

### Build

```powershell
git clone --recursive https://github.com/vikgmdev/forgetty.git
cd forgetty
cargo build --release
```

The binary is at `target\release\forgetty.exe`.

## Android (cross-compiling forgetty-vt)

`forgetty-vt` cross-compiles for Android via Zig. This is required by the
`forgetty-android` companion app and produces `libghostty-vt.so` for each ABI.

### Prerequisites

In addition to Rust and Zig, you need:

```sh
# Android Rust targets
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android

# cargo-ndk (wraps cargo for Android cross-compilation)
cargo install cargo-ndk
```

Android NDK must be installed. The build detects it automatically from:
- `$ANDROID_NDK_HOME` — highest priority (direct NDK path)
- `$ANDROID_HOME/ndk/<latest>` or `$ANDROID_SDK_ROOT/ndk/<latest>`
- `~/Android/sdk/ndk/<latest>` (Linux default)

### Build

Run from the `forgetty-android` companion app's Rust directory:

```sh
cd ~/Forge/forgetty-android/rust

# Single ABI (fastest for iteration)
cargo ndk -t arm64-v8a build

# All ABIs (required for APK)
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 build
```

Zig is invoked automatically by `build.rs` with `-Dtarget=aarch64-linux-android`
(and equivalents for other ABIs). The resulting `libghostty-vt.so` is copied to
`forgetty-android/app/src/main/jniLibs/<abi>/` for APK bundling.

**Full cross-compilation design:** `docs/architecture/CROSS_COMPILE.md`

## Running Tests

```sh
# All workspace tests
cargo test --workspace

# A specific crate
cargo test -p forgetty-renderer

# With output visible
cargo test --workspace -- --nocapture
```

## Development Build

For day-to-day development, use a debug build (faster compile times):

```sh
cargo build
cargo run
```

## Useful Commands

```sh
# Check formatting
cargo fmt --all -- --check

# Run clippy lints
cargo clippy --workspace --all-targets

# Run cargo-deny (license and advisory checks)
cargo install cargo-deny
cargo deny check

# Generate and open API docs
cargo doc --workspace --no-deps --open
```

## Troubleshooting

### Zig not found

If you see an error about Zig during the build, make sure `zig` is on your
`PATH`. You can verify with `zig version`.

### Missing system libraries on Linux

The build will fail at the linking stage if system libraries are missing. The
error message usually indicates which `-l` flag failed. Install the
corresponding `-dev` / `-devel` package.

### GPU driver issues

Forgetty requires a GPU driver that supports at least one of: Vulkan, Metal,
or DirectX 12. On Linux, ensure your Vulkan ICD is installed (e.g.,
`mesa-vulkan-drivers` on Debian/Ubuntu).
