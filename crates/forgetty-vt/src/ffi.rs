//! FFI bindings to the Zig-based VT parser.
//!
//! This module will contain the raw C FFI declarations for the VT parser
//! compiled from Zig, as well as safe Rust wrappers around them.

// TODO: Phase 2 — define extern "C" function declarations for the Zig VT parser
//
// Expected FFI surface:
//   - `vt_parser_new() -> *mut VtParser`
//   - `vt_parser_feed(parser: *mut VtParser, data: *const u8, len: usize)`
//   - `vt_parser_destroy(parser: *mut VtParser)`
//   - Screen query functions for rows, cols, cell contents, attributes, etc.
