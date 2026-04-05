# Cross-Compilation: forgetty-vt and libghostty-vt

> **Scope:** This document covers cross-compiling the `forgetty-vt` crate
> (and its underlying `libghostty-vt` C/Zig library) for targets other than the
> build host. The pattern generalises to any target Zig supports.
>
> **Related:**
> - `crates/forgetty-vt/build.rs` — the implementation
> - Android guide: `forgetty-android/docs/architecture/CROSS_COMPILE.md`

---

## Why Zig for cross-compilation?

`libghostty-vt` is a C/Zig library (Ghostty's VT engine) compiled by Zig via
`build.rs`. Zig has first-class, zero-dependency cross-compilation built in: a
single `zig` binary can target Linux, macOS, Windows, Android, WASM, and musl
without installing per-target SDKs (except Android NDK for bionic targets).

This means the same `forgetty-vt` crate compiles for:

| Target | Zig triple | Notes |
|--------|------------|-------|
| Linux x86_64 (host) | *(no `-Dtarget`)* | Host toolchain |
| Linux aarch64 | `aarch64-linux-gnu` | Cross from x86_64 Linux |
| Linux musl | `x86_64-linux-musl` | Static binary |
| Android arm64 | `aarch64-linux-android` | Needs Android NDK |
| Android armv7 | `arm-linux-androideabi` | Needs Android NDK |
| Android x86_64 | `x86_64-linux-android` | Needs Android NDK |
| macOS arm64 | `aarch64-macos` | From macOS host or with SDK |
| Windows x86_64 | `x86_64-windows-gnu` | MinGW-compatible |

---

## How it works

### `build.rs` target detection

`build.rs` is compiled and run on the **host** machine, but cargo sets
`CARGO_CFG_TARGET_OS` and `CARGO_CFG_TARGET_ARCH` to reflect the **compilation
target** (not the host). This is the correct way to branch on target platform
inside a build script:

```rust
// CORRECT — reads the TARGET being compiled for
let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

// WRONG — this checks the HOST OS, not the target
// #[cfg(target_os = "linux")]   ← DON'T do this in build.rs
```

### Passing the Zig target triple

When building for a non-host target, `build.rs` passes `-Dtarget=<zig-triple>`
to the Zig build command. Zig then cross-compiles `libghostty-vt.so` (or
`.dylib` / `.dll`) for that target:

```
zig build -Demit-lib-vt=true --release=fast -p <out> -Dtarget=aarch64-linux-android
```

For host builds (e.g. native Linux desktop), no `-Dtarget` is passed and Zig
uses the host toolchain.

### Link directives

Each target needs different system library link directives. Since `#[cfg]` in
`build.rs` reflects the HOST, we use runtime env-var checks instead:

```rust
match target_os.as_str() {
    "android" => {
        // Bionic libc — c++_shared (LLVM libc++) instead of stdc++
        println!("cargo:rustc-link-lib=dylib=c");
        println!("cargo:rustc-link-lib=dylib=m");
        println!("cargo:rustc-link-lib=dylib=c++_shared");
    }
    "linux" => {
        println!("cargo:rustc-link-lib=dylib=c");
        println!("cargo:rustc-link-lib=dylib=m");
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
    "macos" => {
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=dylib=c++");
    }
    "windows" | _ => { /* MSVC links C++ runtime automatically */ }
}
```

### `links = "ghostty_vt"` metadata

`forgetty-vt/Cargo.toml` has `links = "ghostty_vt"`. This does two things:

1. Tells cargo only ONE crate in the build graph links this native library
   (prevents duplicate-linking errors when multiple crates depend on `forgetty-vt`).
2. Enables the `DEP_GHOSTTY_VT_*` metadata mechanism: any build script in a
   crate that depends on `forgetty-vt` can read metadata emitted by
   `forgetty-vt/build.rs`.

Currently `build.rs` emits:
```
cargo:LIB_DIR=<path-to-ghostty-install/lib>
```

Available downstream as `DEP_GHOSTTY_VT_LIB_DIR`. Used by `forgetty-android`'s
`build.rs` to copy `libghostty-vt.so` into the APK's `jniLibs/` directory.

---

## Adding a new target

To cross-compile `forgetty-vt` for a new target:

1. Find the Zig target triple: `zig targets | python3 -c "import json,sys; d=json.load(sys.stdin); [print(t) for t in d['libc'] if 'musl' in t]"`
2. Add the case to `android_zig_triple()` in `build.rs` (rename the function
   to `zig_triple_for()` if extending beyond Android).
3. Handle any target-specific link directives in the `match target_os` block.
4. If the output `.so` needs to land in a special location (like jniLibs for
   Android), emit `cargo:KEY=VALUE` from `forgetty-vt/build.rs` and read
   `DEP_GHOSTTY_VT_KEY` in the consuming crate's `build.rs`.

---

## Android specifics

Android requires the NDK sysroot for bionic libc headers and libraries.
Ghostty's build system includes a `pkg/android-ndk` package that auto-detects
the NDK path from environment variables in priority order:

1. `ANDROID_NDK_HOME` — direct NDK path (e.g. `~/Android/sdk/ndk/27.2.12479018`)
2. `ANDROID_HOME` or `ANDROID_SDK_ROOT` — SDK root; picks the latest NDK version
3. `~/Android/sdk` (Linux) / `~/Library/Android/Sdk` (macOS) — default SDK path

When building via `cargo ndk`, the `ANDROID_NDK_HOME` variable is set
automatically. When building manually, set it before invoking cargo:

```sh
export ANDROID_NDK_HOME=~/Android/sdk/ndk/27.2.12479018
cargo build --target aarch64-linux-android
```

Full Android build guide: `forgetty-android/docs/architecture/CROSS_COMPILE.md`

---

## Troubleshooting

### `target_os = "linux"` vs `"android"`

Android's `CARGO_CFG_TARGET_OS` is `"android"`, NOT `"linux"`, even though
Android is Linux-based. This matters for:

- Link directives in `build.rs` (don't put Android under the `"linux"` arm)
- Third-party crates that use `#[cfg(target_os = "linux")]` — they won't compile
  on Android without a patch (see `termios-0.2.2` in the Android project)

### `termios` and other crates that don't support Android

The `termios 0.2.2` crate hard-codes `#[cfg(target_os = "linux")]` and doesn't
recognise Android. The fix is a `[patch.crates-io]` override in the consuming
project's `Cargo.toml` that adds `any(target_os = "linux", target_os = "android")`
to its `os/mod.rs`. See `forgetty-android/rust/patches/termios/`.

### `cargo:rustc-link-lib` not found on cross-compile

If the linker can't find `c++_shared` or similar, ensure the NDK sysroot is on
the library search path. `cargo ndk` handles this automatically. For manual
builds, check that `ANDROID_NDK_HOME` points to the correct NDK version.
