//! Import machinery — the finder/loader chain that resolves Python `import`
//! statements at runtime. Owns the cmodule registry, sys.path, and the
//! file-system finders.
//!
//! Packages (`__init__.py`), relative imports, and circular-import
//! survival arrive in M3 commit 6. This commit handles flat single-file
//! imports only.
//!
//! Designed so VM opcode arms call into this module rather than embedding
//! import logic inline. `sys.modules` lives on `VM` (so split borrows work);
//! this module owns only the cmodule registry + sys.path.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::cmodules;
use crate::object::{CModule, HeapObject, Value};

pub struct ImportSystem {
    /// Registered Rust-backed cmodules, keyed by `cmod.name()`.
    cmodules: HashMap<&'static str, Box<dyn CModule>>,
    /// Search path for `.py` source modules. Iterated in order; first hit
    /// wins. Initialized at VM construction to `[cwd]` plus (in a future
    /// commit) the vendored CPython 3.0.1 stdlib directory.
    pub sys_path: Vec<PathBuf>,
}

impl ImportSystem {
    pub fn new() -> Self {
        let mut cmodules = HashMap::new();
        for m in cmodules::registry() {
            cmodules.insert(m.name(), m);
        }
        let sys_path = vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))];
        Self { cmodules, sys_path }
    }

    /// Replace sys.path with the given entries. Called by the VM at
    /// bootstrap if the runner knows the script's directory.
    pub fn set_sys_path(&mut self, path: Vec<PathBuf>) {
        self.sys_path = path;
    }

    /// Try to build a HeapObject::Module from a registered cmodule. Returns
    /// None if no cmodule with this name is registered (caller falls through
    /// to the source-file finder).
    pub fn try_load_cmodule(
        &self,
        name: &str,
        heap: &mut Vec<HeapObject>,
    ) -> Option<Value> {
        let cmod = self.cmodules.get(name)?;
        let globals = cmod.build_globals(heap);
        let module_idx = heap.len();
        heap.push(HeapObject::Module {
            name: name.to_string(),
            globals,
            file: None,
            package: None,
            initialized: true,
            all: None,
        });
        Some(Value::object_ref(module_idx))
    }

    /// True if a cmodule with this name is registered.
    pub fn has_cmodule(&self, name: &str) -> bool {
        self.cmodules.contains_key(name)
    }

    /// Locate `<name>.py` on sys.path. Returns the file path on the first
    /// hit; None if no entry contains a matching file. Flat-modules-only:
    /// dotted names ("foo.bar") are not yet resolved (packages land in
    /// commit 6).
    pub fn find_source_file(&self, name: &str) -> Option<PathBuf> {
        if name.contains('.') {
            return None; // dotted names need package support
        }
        for dir in &self.sys_path {
            let candidate = dir.join(format!("{name}.py"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_loads_sys() {
        let sys = ImportSystem::new();
        assert!(sys.has_cmodule("sys"));
        assert!(!sys.has_cmodule("does_not_exist"));
    }

    #[test]
    fn try_load_cmodule_builds_module() {
        let sys = ImportSystem::new();
        let mut heap = Vec::new();
        let v = sys.try_load_cmodule("sys", &mut heap).expect("sys cmodule loads");
        // Module variant lives in heap.
        let idx = v.as_object_ref().expect("module is heap ref");
        match &heap[idx] {
            HeapObject::Module { name, file, package, initialized, globals, .. } => {
                assert_eq!(name, "sys");
                assert!(file.is_none());        // cmodule
                assert!(package.is_none());     // top-level
                assert!(*initialized);          // cmodules are init'd at build time
                assert!(globals.contains_key("version"));
                assert!(globals.contains_key("maxsize"));
            }
            other => panic!("expected Module, got {other:?}"),
        }
    }

    #[test]
    fn try_load_cmodule_returns_none_for_missing() {
        let sys = ImportSystem::new();
        let mut heap = Vec::new();
        assert!(sys.try_load_cmodule("nonexistent", &mut heap).is_none());
    }
}
