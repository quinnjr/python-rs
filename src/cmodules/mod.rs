//! Rust-implemented Python modules ("cmodules"). Each cmodule lives in its
//! own file in this directory and exposes itself via `object::CModule`.
//!
//! The import machinery indexes this registry at VM startup; cmodules win
//! precedence over `.py` files of the same name on `sys.path`, matching
//! CPython's behavior where built-ins outrank filesystem modules.
//!
//! Adding a new cmodule = one `mod foo;` line below + one entry in
//! `registry()`. No edits to import machinery needed.

mod sys;

use crate::object::CModule;

/// Full set of cmodules built into this VM. Called once at VM startup.
pub fn registry() -> Vec<Box<dyn CModule>> {
    vec![
        Box::new(sys::Sys),
    ]
}
