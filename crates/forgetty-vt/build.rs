//! Build script for forgetty-vt.
//!
//! Compiles libghostty-vt from the ghostty submodule using Zig,
//! then links the resulting shared library. After building, copies
//! the .so and soname symlink to `target/<profile>/lib/` so the
//! binary can find it via RUNPATH at runtime.
//!
//! # Android cross-compilation (AND-007)
//!
//! When `CARGO_CFG_TARGET_OS=android`, this script:
//!   1. Passes `-Dtarget=<zig-android-triple>` to the Zig build so Ghostty's
//!      build system uses the correct Android sysroot via its `android_ndk`
//!      package (which reads `ANDROID_NDK_HOME` / `ANDROID_HOME` /
//!      `ANDROID_SDK_ROOT` from the environment — set automatically by cargo-ndk).
//!   2. Links against Android's bionic C++ runtime (`c++_shared`) instead of
//!      the desktop GNU `stdc++`.
//!   3. Skips the RUNPATH copy step (Android's dynamic linker does not use
//!      RUNPATH; jniLibs packaging is handled by forgetty-android's build.rs).
//!   4. Emits `cargo:LIB_DIR=<path>` so the dependent forgetty-android build.rs
//!      can copy libghostty-vt.so into the APK's jniLibs directory.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ghostty_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("ghostty");

    // Read target OS/arch from cargo (these reflect the COMPILATION TARGET, not the host).
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let is_android = target_os == "android";

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
    let install_prefix_str = format!("{}", install_prefix.display());

    // Build libghostty-vt as a shared library.
    // For Android, pass the Zig target triple so Ghostty's android_ndk build
    // package sets up the correct bionic sysroot via ANDROID_NDK_HOME.
    let mut cmd = Command::new(&zig);
    cmd.current_dir(&ghostty_dir).args([
        "build",
        "-Demit-lib-vt=true",
        "--release=fast",
        "-p",
        &install_prefix_str,
    ]);

    if is_android {
        let zig_triple = android_zig_triple(&target_arch);
        cmd.arg(format!("-Dtarget={zig_triple}"));
    }

    let status = cmd
        .status()
        .expect("Failed to run zig build. Is Zig installed? Get it from https://ziglang.org");

    if !status.success() {
        panic!("zig build failed with exit code: {:?}", status.code());
    }

    // Android: patch libghostty-vt.so soname from libghostty-vt.so.0 → libghostty-vt.so.
    //
    // Zig uses Linux versioned-soname convention (libghostty-vt.so.0) even when
    // targeting Android. Android's dynamic linker resolves NEEDED by *filename*,
    // not soname, so it looks for a file literally named "libghostty-vt.so.0" —
    // which doesn't exist in the APK (only "libghostty-vt.so" is packaged).
    //
    // Patching the soname here (before Rust links against the library) ensures
    // that libforgetty_android.so bakes in NEEDED=libghostty-vt.so, which the
    // Android linker resolves correctly from jniLibs.
    if is_android {
        let lib_dir_early = install_prefix.join("lib");
        patch_android_soname(&lib_dir_early);
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
    // Use runtime target-OS detection (CARGO_CFG_TARGET_OS) rather than
    // #[cfg(target_os)] which reflects the HOST OS, not the compilation target.
    if is_android {
        // Android bionic: libc and libm are always available as system libs.
        // libc++_shared must be bundled in the APK (cargo-ndk handles this when
        // ANDROID_NDK_HOME is set and c++_shared is found in the NDK sysroot).
        println!("cargo:rustc-link-lib=dylib=c");
        println!("cargo:rustc-link-lib=dylib=m");
        println!("cargo:rustc-link-lib=dylib=c++_shared");
    } else {
        match target_os.as_str() {
            "linux" => {
                println!("cargo:rustc-link-lib=dylib=c");
                println!("cargo:rustc-link-lib=dylib=m");
                println!("cargo:rustc-link-lib=dylib=stdc++");
            }
            "macos" => {
                println!("cargo:rustc-link-lib=framework=Foundation");
                println!("cargo:rustc-link-lib=dylib=c++");
            }
            "windows" | _ => {
                // Windows: MSVC links C++ runtime automatically.
            }
        }
    }

    // ── Post-build: copy .so to target/<profile>/lib/ (desktop only) ─────────
    // On Android the dynamic linker resolves .so files from the APK's lib/<abi>/
    // directory — RUNPATH is not used. The forgetty-android build.rs copies
    // libghostty-vt.so into jniLibs using the LIB_DIR metadata below.
    if !is_android {
        #[cfg(unix)]
        copy_so_to_target_lib(&out_dir, &lib_dir);
    }

    // Emit the lib dir path as build metadata so forgetty-android's build.rs
    // can pick it up via DEP_GHOSTTY_VT_LIB_DIR and copy libghostty-vt.so into
    // the APK's jniLibs/<abi>/ directory.
    println!("cargo:LIB_DIR={}", lib_dir.display());

    // Rerun if ghostty source changes
    println!("cargo:rerun-if-changed=ghostty/src");
    println!("cargo:rerun-if-changed=ghostty/build.zig");
    println!("cargo:rerun-if-changed=build.rs");

    // Export the include path for bindgen or manual reference
    let include_dir = ghostty_dir.join("include");
    println!("cargo:include={}", include_dir.display());
}

/// Map a Rust target architecture name to the corresponding Zig Android target triple.
///
/// Zig uses `arm-linux-androideabi` for 32-bit ARM (not `armv7a`), matching the
/// NDK triple used by `android_ndk.addPaths()` in Ghostty's build system.
fn android_zig_triple(arch: &str) -> &'static str {
    match arch {
        "aarch64" => "aarch64-linux-android",
        "arm" => "arm-linux-androideabi",
        "x86_64" => "x86_64-linux-android",
        "x86" => "x86-linux-android",
        _ => panic!("Unsupported Android architecture for Zig cross-compile: {arch}"),
    }
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

/// Patch libghostty-vt.so soname from `libghostty-vt.so.0` to `libghostty-vt.so` for Android.
///
/// Zig sets a Linux-style versioned soname even when cross-compiling for Android.
/// Android's dynamic linker resolves NEEDED entries by filename, so the library file
/// must be named exactly what NEEDED says. Since we package only `libghostty-vt.so`
/// (not `libghostty-vt.so.0`) in jniLibs, the soname must match.
///
/// Tries patchelf first. Falls back to a direct binary patch (safe: replaces a known
/// fixed-length null-terminated string in .dynstr with a shorter one + null padding).
fn patch_android_soname(lib_dir: &std::path::Path) {
    use std::fs;

    let so_path = lib_dir.join("libghostty-vt.so");
    if !so_path.exists() {
        println!("cargo:warning=patch_android_soname: {} not found, skipping", so_path.display());
        return;
    }

    // Try patchelf first.
    let patchelf_ok = Command::new("patchelf")
        .args(["--set-soname", "libghostty-vt.so"])
        .arg(&so_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if patchelf_ok {
        println!(
            "cargo:warning=Patched libghostty-vt.so soname → libghostty-vt.so (Android, patchelf)"
        );
        return;
    }

    // Fallback: binary patch. Safe because:
    // - "libghostty-vt.so.0\0" (20 bytes) → "libghostty-vt.so\0\0\0\0" (20 bytes)
    // - Extra nulls are inert in .dynstr (empty string entries).
    // - We only patch the first occurrence; it's always in .dynstr.
    let mut data = match fs::read(&so_path) {
        Ok(d) => d,
        Err(e) => {
            println!(
                "cargo:warning=patch_android_soname: failed to read {}: {e}",
                so_path.display()
            );
            return;
        }
    };

    // "libghostty-vt.so.0\0" = 18 chars + null = 19 bytes
    // "libghostty-vt.so\0\0\0" = 16 chars + null + 2 padding = 19 bytes
    let old: &[u8] = b"libghostty-vt.so.0\0";
    let new: &[u8] = b"libghostty-vt.so\0\0\0";
    debug_assert_eq!(old.len(), new.len());

    if let Some(pos) = data.windows(old.len()).position(|w| w == old) {
        data[pos..pos + old.len()].copy_from_slice(new);
        match fs::write(&so_path, &data) {
            Ok(()) => println!(
                "cargo:warning=Patched libghostty-vt.so soname → libghostty-vt.so (Android, binary patch)"
            ),
            Err(e) => println!(
                "cargo:warning=patch_android_soname: write failed: {e}. \
                 Install patchelf for a reliable fix: apt install patchelf"
            ),
        }
    } else {
        println!(
            "cargo:warning=patch_android_soname: soname string not found in {}. \
             Already patched, or Zig changed its output format. \
             Verify with: readelf -d {} | grep SONAME",
            so_path.display(),
            so_path.display()
        );
    }
}

/// Find the Zig compiler binary.
fn find_zig() -> String {
    // Check common locations
    for candidate in
        &["zig", "/usr/local/bin/zig", "zig", "zig"]
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
