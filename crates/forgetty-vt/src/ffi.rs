//! FFI bindings for libghostty-vt.
//!
//! This module will contain the C FFI bindings to libghostty-vt once
//! the library is integrated. Currently, Forgetty uses a built-in
//! VT parser based on the `vte` crate as an interim solution.
//!
//! The public API in `terminal.rs` and `screen.rs` is designed to be
//! backend-agnostic, so switching from the built-in parser to
//! libghostty-vt will not change the API surface.
