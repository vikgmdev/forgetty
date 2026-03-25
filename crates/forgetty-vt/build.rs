//! Build script for forgetty-vt.
//!
//! Compiles libghostty-vt from the ghostty submodule using Zig,
//! then links the resulting static library.

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

    // Build libghostty-vt as a static library
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

    // Set rpath so the binary can find the shared library at runtime
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());

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

    // Rerun if ghostty source changes
    println!("cargo:rerun-if-changed=ghostty/src");
    println!("cargo:rerun-if-changed=ghostty/build.zig");
    println!("cargo:rerun-if-changed=build.rs");

    // Export the include path for bindgen or manual reference
    let include_dir = ghostty_dir.join("include");
    println!("cargo:include={}", include_dir.display());
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
