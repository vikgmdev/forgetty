//! Build script for forgetty-vt.
//!
//! Compiles libghostty-vt from the ghostty submodule using Zig,
//! then links the resulting shared library. After building, copies
//! the .so and soname symlink to `target/<profile>/lib/` so the
//! binary can find it via RUNPATH at runtime.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ghostty_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("ghostty");

    // Check if the ghostty submodule exists
    if !ghostty_dir.join("build.zig").exists() {
        println!(
            "cargo:warning=ghostty submodule not found at {}. \
             Run: git submodule update --init --recursive",
            ghostty_dir.display()
        );
        return;
    }

    // Find Zig compiler
    let zig = find_zig();

    let install_prefix = out_dir.join("ghostty-install");

    // Build libghostty-vt as a shared library
    let status = Command::new(&zig)
        .current_dir(&ghostty_dir)
        .args([
            "build",
            "-Demit-lib-vt=true",
            "--release=fast",
            "-p",
            &format!("{}", install_prefix.display()),
        ])
        .status()
        .expect("Failed to run zig build. Is Zig installed? Get it from https://ziglang.org");

    if !status.success() {
        panic!("zig build failed with exit code: {:?}", status.code());
    }

    // Tell cargo where to find the library.
    // Use the shared library (.so) because it bundles simdutf and all dependencies.
    // The static library has undefined simdutf symbols that aren't separately available.
    let lib_dir = install_prefix.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=ghostty-vt");

    // NOTE: RPATH is now set by the top-level build.rs (binary crate).
    // Library crate build.rs cannot propagate rustc-link-arg to the final binary.

    // Link system dependencies that libghostty-vt needs.
    // The library uses simdutf (C++) for SIMD-optimized UTF-8 processing,
    // so we need the C++ standard library.
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-lib=dylib=c");
        println!("cargo:rustc-link-lib=dylib=m");
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }

    #[cfg(target_os = "macos")]
    {
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=dylib=c++");
    }

    #[cfg(target_os = "windows")]
    {
        // MSVC links C++ runtime automatically
    }

    // ── Post-build: copy .so to target/<profile>/lib/ ──────────────────
    // This makes the .so available next to the binary so $ORIGIN/lib works
    // for both `cargo run` and portable tarball deployment.
    #[cfg(unix)]
    copy_so_to_target_lib(&out_dir, &lib_dir);

    // Rerun if ghostty source changes
    println!("cargo:rerun-if-changed=ghostty/src");
    println!("cargo:rerun-if-changed=ghostty/build.zig");
    println!("cargo:rerun-if-changed=build.rs");

    // Export the include path for bindgen or manual reference
    let include_dir = ghostty_dir.join("include");
    println!("cargo:include={}", include_dir.display());
}

/// Copy the shared library and soname symlink to `target/<profile>/lib/`.
///
/// Discovers the target directory by walking up from `$OUT_DIR` to find
/// the `target/<profile>/` directory. Uses copy-then-rename for atomicity
/// to avoid races with parallel builds.
#[cfg(unix)]
fn copy_so_to_target_lib(out_dir: &std::path::Path, zig_lib_dir: &std::path::Path) {
    use std::fs;
    use std::os::unix::fs::symlink;

    let so_file = "libghostty-vt.so.0.1.0";
    let so_soname = "libghostty-vt.so.0";

    let src = zig_lib_dir.join(so_file);
    if !src.exists() {
        println!(
            "cargo:warning=Post-build: {} not found at {}, skipping copy",
            so_file,
            zig_lib_dir.display()
        );
        return;
    }

    // Walk up from OUT_DIR to find target/<profile>/
    // OUT_DIR is typically: target/<profile>/build/<crate>-<hash>/out
    let target_profile_dir = match find_target_profile_dir(out_dir) {
        Some(d) => d,
        None => {
            println!(
                "cargo:warning=Post-build: could not determine target/<profile>/ from OUT_DIR={}",
                out_dir.display()
            );
            return;
        }
    };

    let dest_dir = target_profile_dir.join("lib");
    fs::create_dir_all(&dest_dir).unwrap_or_else(|e| {
        panic!("Failed to create {}: {}", dest_dir.display(), e);
    });

    let dest_file = dest_dir.join(so_file);
    let dest_link = dest_dir.join(so_soname);

    // Copy with atomic rename: write to .tmp then rename
    let tmp_file = dest_dir.join(format!("{}.tmp", so_file));
    if let Err(e) = fs::copy(&src, &tmp_file) {
        println!(
            "cargo:warning=Post-build: failed to copy {} to {}: {}",
            src.display(),
            tmp_file.display(),
            e
        );
        return;
    }
    if let Err(e) = fs::rename(&tmp_file, &dest_file) {
        println!(
            "cargo:warning=Post-build: failed to rename {} to {}: {}",
            tmp_file.display(),
            dest_file.display(),
            e
        );
        return;
    }

    // Create/replace soname symlink
    let _ = fs::remove_file(&dest_link);
    if let Err(e) = symlink(so_file, &dest_link) {
        println!(
            "cargo:warning=Post-build: failed to create symlink {} -> {}: {}",
            dest_link.display(),
            so_file,
            e
        );
        return;
    }

    // DO NOT add dest_dir as a rerun-if-changed trigger — that would cause
    // an infinite rebuild loop since we write into it every build.

    eprintln!("Post-build: copied {} and symlink {} to {}", so_file, so_soname, dest_dir.display());
}

/// Walk up from `out_dir` to find the `target/<profile>/` directory.
///
/// OUT_DIR layout: `<workspace>/target/<profile>/build/<crate>-<hash>/out`
/// We walk up looking for a parent that contains a `build/` child (the "build"
/// directory is always directly under `target/<profile>/`).
#[cfg(unix)]
fn find_target_profile_dir(out_dir: &std::path::Path) -> Option<PathBuf> {
    let mut dir = out_dir.to_path_buf();
    // Walk up at most 10 levels to avoid infinite loops
    for _ in 0..10 {
        dir = dir.parent()?.to_path_buf();
        // target/<profile>/ contains a "build" subdirectory and is NOT named "build" itself
        if dir.join("build").is_dir() && dir.file_name().map(|n| n != "build").unwrap_or(false) {
            return Some(dir);
        }
    }
    None
}

/// Find the Zig compiler binary.
fn find_zig() -> String {
    // Check common locations
    for candidate in
        &["zig", "/usr/local/bin/zig", "/home/vick/.local/zig/zig", "/home/vick/.local/bin/zig"]
    {
        if Command::new(candidate)
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return candidate.to_string();
        }
    }

    // Check ZIG_PATH env var
    if let Ok(path) = env::var("ZIG_PATH") {
        return path;
    }

    panic!(
        "Zig compiler not found. Install Zig 0.15+ from https://ziglang.org \
         or set the ZIG_PATH environment variable."
    );
}
