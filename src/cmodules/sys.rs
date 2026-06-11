//! The `sys` cmodule — exposes interpreter-level state to Python code.
//!
//! Most stdio/argv/path/modules fields are `None` placeholders here; the
//! VM patches them in after the cmodule is registered, since they depend
//! on runtime state (CLI args, executable path, the sys.modules dict
//! itself) that isn't available at `build_globals` time.
//!
//! Real `stdout`/`stderr`/`stdin` file objects come when `_io` lands (M3.1).

use std::collections::HashMap;

use crate::object::{CModule, HeapObject, Value, alloc_str, alloc_tuple};

pub struct Sys;

impl CModule for Sys {
    fn name(&self) -> &'static str {
        "sys"
    }

    fn build_globals(&self, heap: &mut Vec<HeapObject>) -> HashMap<String, Value> {
        let mut g = HashMap::new();

        g.insert(
            "version".into(),
            alloc_str(heap, "3.0.1 (python-rs, compatibility target)"),
        );

        let releaselevel = alloc_str(heap, "final");
        let version_info_items = vec![
            Value::small_int_unchecked(3),
            Value::small_int_unchecked(0),
            Value::small_int_unchecked(1),
            releaselevel,
            Value::small_int_unchecked(0),
        ];
        g.insert("version_info".into(), alloc_tuple(heap, version_info_items));

        g.insert("platform".into(), alloc_str(heap, platform_string()));
        // sys.maxsize — largest positive integer supported by the platform's
        // Py_ssize_t. Matches our i48 small-int max.
        g.insert(
            "maxsize".into(),
            Value::small_int_unchecked((1i64 << 47) - 1),
        );
        // sys.hash_info — partial; just modulus for now, matches our hash impl.
        let hash_info_items = vec![
            Value::small_int_unchecked(64),
            Value::small_int_unchecked((1i64 << 61) - 1), // modulus
            Value::small_int_unchecked(314_159),          // inf hash (placeholder)
            Value::small_int_unchecked(0),                // nan hash
        ];
        g.insert("hash_info".into(), alloc_tuple(heap, hash_info_items));
        g.insert(
            "byteorder".into(),
            alloc_str(
                heap,
                if cfg!(target_endian = "little") {
                    "little"
                } else {
                    "big"
                },
            ),
        );

        // Patched by the VM after registration:
        g.insert("argv".into(), Value::none());
        g.insert("path".into(), Value::none());
        g.insert("modules".into(), Value::none());
        g.insert("stdout".into(), Value::none());
        g.insert("stderr".into(), Value::none());
        g.insert("stdin".into(), Value::none());
        // sys.executable — VM patches with std::env::current_exe() at bootstrap.
        g.insert("executable".into(), Value::none());

        g
    }
}

fn platform_string() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sys_module_name_is_sys() {
        assert_eq!(Sys.name(), "sys");
    }

    #[test]
    fn sys_module_exports_expected_keys() {
        let mut heap = Vec::new();
        let g = Sys.build_globals(&mut heap);
        for key in [
            "version",
            "version_info",
            "platform",
            "maxsize",
            "hash_info",
            "byteorder",
            "argv",
            "path",
            "modules",
            "stdout",
            "stderr",
            "stdin",
            "executable",
        ] {
            assert!(g.contains_key(key), "missing key: {key}");
        }
    }

    #[test]
    fn sys_module_version_is_3_0_1() {
        let mut heap = Vec::new();
        let g = Sys.build_globals(&mut heap);
        let version = g.get("version").expect("version key");
        let idx = version.as_str_ref().expect("version is str");
        let s = heap[idx].as_str().expect("Str heap entry");
        assert!(s.starts_with("3.0.1"), "expected '3.0.1...' got {s:?}");
    }

    #[test]
    fn sys_module_maxsize_matches_i48_max() {
        let mut heap = Vec::new();
        let g = Sys.build_globals(&mut heap);
        let maxsize = g.get("maxsize").expect("maxsize key");
        assert_eq!(maxsize.as_int(), Some((1i64 << 47) - 1));
    }

    #[test]
    fn sys_module_argv_starts_as_none() {
        // argv is a None placeholder patched by the VM at bootstrap. Verifying
        // the placeholder contract so the bootstrap step has a stable target.
        let mut heap = Vec::new();
        let g = Sys.build_globals(&mut heap);
        assert!(g.get("argv").expect("argv key").is_none());
    }

    #[test]
    fn registry_includes_sys() {
        let modules = crate::cmodules::registry();
        assert!(modules.iter().any(|m| m.name() == "sys"));
    }
}
