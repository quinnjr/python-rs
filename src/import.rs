//! Import machinery — the finder/loader chain that resolves Python `import`
//! statements at runtime. Owns the cmodule registry and (in later commits)
//! the sys.path finder for `.py` files and packages.
//!
//! Designed so VM opcode arms call into this module rather than embedding
//! import logic inline. `sys.modules` lives on `VM` (so split borrows work);
//! this module owns only the immutable-after-construction registry and path.

use std::collections::HashMap;

use crate::cmodules;
use crate::object::{CModule, HeapObject, Value};

/// The import system: cmodule registry + sys.path. Stored on the VM as a
/// field; methods take `&self` because the registry is immutable after
/// construction, so callers can split-borrow `vm.heap` alongside.
pub struct ImportSystem {
    /// Registered Rust-backed cmodules, keyed by `cmod.name()`.
    cmodules: HashMap<&'static str, Box<dyn CModule>>,
    // sys_path lands in commit 5 when the source-file finder arrives.
}

impl ImportSystem {
    pub fn new() -> Self {
        let mut cmodules = HashMap::new();
        for m in cmodules::registry() {
            cmodules.insert(m.name(), m);
        }
        Self { cmodules }
    }

    /// Try to build a HeapObject::Module from a registered cmodule. Returns
    /// None if no cmodule with this name is registered (caller falls through
    /// to the source-file finder when that lands).
    ///
    /// On a hit, allocates the module into `heap` and returns its `object_ref`
    /// Value. The caller is responsible for inserting it into `sys.modules`.
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

    /// True if a cmodule with this name is registered. Cheap predicate
    /// for the finder chain.
    pub fn has_cmodule(&self, name: &str) -> bool {
        self.cmodules.contains_key(name)
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
