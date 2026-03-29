//! Top-level build script for the forgetty binary crate.
//!
//! Sets RUNPATH on the final binary so it can find libghostty-vt.so
//! at runtime without ldconfig or LD_LIBRARY_PATH.
//!
//! The `cargo:rustc-link-arg` directive MUST come from the binary crate's
//! build.rs — Cargo does NOT propagate it from library crates.

fn main() {
    // Use --enable-new-dtags so we get RUNPATH (not RPATH).
    // RUNPATH allows LD_LIBRARY_PATH to override, which is desired.
    println!("cargo:rustc-link-arg=-Wl,--enable-new-dtags");

    // $ORIGIN/lib  — cargo run: binary at target/release/, .so at target/release/lib/
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/lib");

    // $ORIGIN/../lib — DEB: binary at /usr/bin/, .so at /usr/lib/
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");

    // $ORIGIN — flat deployment: binary and .so in same directory
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");

    // /usr/local/lib — install.sh compatibility
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/local/lib");
}
