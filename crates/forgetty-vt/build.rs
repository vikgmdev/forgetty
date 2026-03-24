/// Build script for forgetty-vt.
///
/// In a future phase this will invoke the Zig build system to compile
/// the Ghostty-derived VT parser into a static library and link it.
fn main() {
    // TODO: Phase 2 — integrate Zig-based VT parser build
    //
    // The plan is to:
    //   1. Invoke `zig build` on the vendored VT parser source.
    //   2. Use the `cc` crate to link the resulting static library.
    //   3. Generate Rust FFI bindings via the types in `src/ffi.rs`.
    //
    // For now, this is a no-op so the crate compiles as a stub.
    println!(
        "cargo:warning=forgetty-vt: Zig VT parser integration is not yet implemented (Phase 2)"
    );
}
