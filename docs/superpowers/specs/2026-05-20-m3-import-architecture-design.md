# M3 — Import System Architecture

**Date:** 2026-05-20
**Status:** Approved, ready for implementation planning
**Milestone:** M3 in `2026-05-19-cpython-3.0-compatibility-design.md` (architecture-only slice)
**Prereq:** M2 (big-int representation) complete on `develop`

## Summary

Introduce a Python import system to python-rs from scratch. Currently the interpreter has no `import` keyword, no `Stmt::Import` AST node, no `HeapObject::Module`, no `sys.modules`, no IMPORT_* opcodes. After M3:

- `import foo`, `from foo import bar`, `from foo import *`, `from . import sub`, and `import foo as f` all work.
- Both `.py` files on `sys.path` AND Rust-implemented "cmodules" are resolved by the same finder chain. Cmodules win on name collisions.
- Packages (directories with `__init__.py`) resolve correctly, including submodule loading from the package directory.
- Relative imports resolve from the importing module's `__package__`.
- Circular imports return half-built modules (Python's documented behavior).
- One cmodule (`sys`) is implemented end-to-end as proof of the integration path. The other ~17 Tier-1/Tier-2 cmodules each get their own short follow-up spec (M3.1–M3.8).

## Goals

1. **`import` works.** Both styles (`import x`, `from x import y`), both targets (.py and cmodule), packages, relative imports.
2. **Cmodule integration is one-trait-impl plus one registry entry** — adding `_io` later means writing `src/cmodules/io.rs` implementing `CModule` and adding a line to `src/cmodules/mod.rs::registry()`. No edits to import machinery.
3. **The architecture is exercised end-to-end** by `sys` — not just specified in prose, actually wired through compiler → opcode → import machinery → cmodule trait → `HeapObject::Module`.
4. **Failure modes are explicit.** Every plausible import failure has a documented behavior (Section 6 catalog).

## Non-Goals

- The other 17 cmodules. Each is its own spec (M3.1–M3.8).
- `unittest` / `test.regrtest` running end-to-end. Comes after the bulk of M3.x cmodules land.
- `.pyc` bytecode caching.
- `sys.meta_path` user-extensible finder chain.
- Threading / GIL / multi-interpreter isolation of `sys.modules`.
- `importlib` as a fully-featured Python API. We provide enough for `import` statements, not the introspection surface.
- `PYTHONPATH` env var and `-I`/`-S` CLI flags (deferred).

## Architecture overview

```
Python source:    import foo.bar
                       |
Lexer:            Import, Ident, Dot, Ident, Newline
                       |
Parser:           Stmt::Import { names: [{ name: "foo.bar", asname: None }] }
                       |
Compiler:         LOAD_CONST 0          (level)
                  LOAD_CONST None       (fromlist)
                  IMPORT_NAME idx       (operand = code.names index of "foo.bar")
                  STORE_NAME  idx_foo   (binds the TOP of the dotted path)
                       |
VM IMPORT_NAME:   Pop level + fromlist, read name, call ImportSystem::find_and_load.
                       |
Import machinery: 1. sys.modules cache check.
                  2. Recursively load parent package ("foo" before "foo.bar").
                  3. Try finders in order: CModule → Package (__init__.py) → SourceFile.
                  4. On hit: insert into sys.modules BEFORE executing the module body
                     (cycle survival). Push a frame with module.globals as the frame's
                     globals. Run module body. Mark initialized.
                  5. For "foo.bar", bind "bar" as attr of "foo".
                  6. Return the *top* of the dotted path (Python semantic).
                       |
VM continues:     STORE_NAME idx_foo  ← uses the returned top-of-path
```

For `from foo.bar import baz, qux`:

```
LOAD_CONST 0
LOAD_CONST ("baz", "qux")    # fromlist tuple
IMPORT_NAME idx_foo_bar      # pushes foo.bar module
IMPORT_FROM idx_baz          # peeks TOS, fetches .baz (or sub-imports), pushes
STORE_NAME  idx_baz
IMPORT_FROM idx_qux
STORE_NAME  idx_qux
POP_TOP                      # discard the module
```

For `from foo import *`:

```
LOAD_CONST 0
LOAD_CONST ("*",)            # marker fromlist
IMPORT_NAME idx_foo
IMPORT_STAR 0                # walks __all__ or public names, binds each into globals
```

### Three-component split

| Component | Lives in | Responsibility |
|---|---|---|
| Bytecode dispatch | `src/vm.rs` (opcode arms) | Pop operands, call the import machinery, push result. Thin. |
| Import machinery | **new** `src/import.rs` | Finder/loader chain. Owns sys.modules, sys.path, cmodule registry. |
| cmodule trait | `src/object.rs` next to HeapObject | Defines `trait CModule`, used by per-cmodule files under `src/cmodules/`. |

Each component testable in isolation. `vm.rs` doesn't know how a module was found; `import.rs` doesn't know about specific opcodes.

## Module representation

New `HeapObject` variant:

```rust
HeapObject::Module {
    name: String,                         // canonical dotted name; e.g. "foo.bar"
    globals: HashMap<String, Value>,      // module namespace (also serves as __dict__)
    file: Option<String>,                 // source path; None for cmodules
    package: Option<String>,              // parent package's dotted name (for relative imports)
    initialized: bool,                    // false during body execution; true after
    all: Option<Vec<String>>,             // lazy cache of __all__ for star-imports
}
```

- `name` is the `sys.modules` key. `import foo as f` binds `f` in caller scope; `module.__name__` stays `"foo"`.
- `globals` is the module's namespace. `__name__`/`__file__`/`__package__`/`__doc__` are populated at construction as regular dict entries — no special-casing.
- `package` carries the containing package's dotted name for relative-import resolution. `foo/bar.py` has `package = Some("foo")`; top-level `mymodule.py` has `package = None`.
- `initialized = false` during module-body execution; subsequent re-entrant imports return the partially-built module from cache (matches CPython circular-import semantics).
- `all` is filled lazily on first star-import or stays `None`.

### `sys.modules` placement

Lives on the VM, **not** in the heap:

```rust
pub struct VM {
    // ...existing...
    pub sys_modules: HashMap<String, Value>,
}
```

A `HeapObject::Dict` representation would force every import lookup through dict-protocol dispatch and thread `&mut heap` through every cache access. A plain Rust `HashMap` is faster and simpler. Userland access via `sys.modules` is rare in the 3.0 test surface; when needed, the `sys` cmodule will synthesize a dict view on attribute access (post-M3).

### Module attribute access

`foo.bar` (Value::object_ref to HeapObject::Module, then attribute `bar`) reads `module.globals.get("bar")`. Missing → `AttributeError`. No special-casing for dunders.

### Module construction always populates dunders

```rust
fn new_module(name: String, file: Option<String>, package: Option<String>, heap: &mut Vec<HeapObject>) -> Value {
    let mut globals = HashMap::new();
    globals.insert("__name__".into(),    alloc_str(heap, &name));
    globals.insert("__file__".into(),    file.as_deref().map(|f| alloc_str(heap, f)).unwrap_or(Value::none()));
    globals.insert("__package__".into(), package.as_deref().map(|p| alloc_str(heap, p)).unwrap_or(Value::none()));
    globals.insert("__doc__".into(),     Value::none());
    let idx = heap.len();
    heap.push(HeapObject::Module { name, globals, file, package, initialized: false, all: None });
    Value::object_ref(idx)
}
```

### Frame-globals refactor (prerequisite)

The existing `Frame` struct treats `globals` as a HashMap owned by the top-level `<module>` code object. M3 needs each frame to carry a *reference* to its module's globals, so when executing `foo.py`'s body, name lookups read/write `foo`'s globals (not the top-level main globals). This is a small refactor touching every frame-creation site. Lands as the first sub-commit of M3 implementation; verify all existing tests pass before any import code merges.

## Lexer, AST, bytecode, compiler additions

### Lexer (`src/lexer.rs`)

Two new keywords. `as` already lexed from `except ... as e` — reuse the existing token, do not duplicate.

```rust
"import" => TokenKind::Import,
"from"   => TokenKind::From,
```

Dotted module names (`foo.bar`) use the existing Dot token from attribute access. No new token kinds needed for dots.

### AST (`src/ast.rs`)

```rust
pub enum Stmt {
    // ...existing...
    Import {
        names: Vec<ImportAlias>,
        line: u32,
    },
    ImportFrom {
        module: Option<String>,      // None for `from . import x`
        names: Vec<ImportAlias>,     // empty when is_star
        level: u32,                  // leading-dot count: 0 absolute, 1 `.`, 2 `..`
        is_star: bool,
        line: u32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportAlias {
    pub name: String,                // dotted for Import; simple for ImportFrom
    pub asname: Option<String>,
}
```

### Bytecode (`src/bytecode.rs`)

```rust
pub mod op {
    // ...existing...
    /// stack: [level, fromlist] → [module]
    /// operand = index into code.names — the dotted module name.
    pub const IMPORT_NAME: u8 = ...;
    /// stack: [module] → [module, attr_or_submodule]
    /// operand = index into code.names — the name to fetch from TOS module.
    pub const IMPORT_FROM: u8 = ...;
    /// stack: [module] → []
    /// operand unused. Binds public names into current frame globals.
    pub const IMPORT_STAR: u8 = ...;
}
```

### Parser (`src/parser.rs`)

Standard Python grammar:

- `import a, b.c, d as e` → `Stmt::Import { names: [a, b.c, d-as-e] }`
- `from foo.bar import baz, qux as q` → `Stmt::ImportFrom { module: Some("foo.bar"), names: [baz, qux-as-q], level: 0, is_star: false }`
- `from . import x` → `module: None, level: 1, is_star: false`
- `from ..pkg import y` → `module: Some("pkg"), level: 2`
- `from foo import *` → `is_star: true, names: []`

### Compiler (`src/compiler.rs`)

For `import foo.bar as fb`:

```rust
self.emit_load_const(Value::small_int_unchecked(0), line);     // level
self.emit_load_const(Value::none(), line);                      // fromlist=None
self.emit(op::IMPORT_NAME, self.add_name("foo.bar"), line);
self.emit(op::STORE_NAME, self.add_name("fb"), line);
```

For `from foo.bar import baz, qux`:

```rust
self.emit_load_const(Value::small_int_unchecked(0), line);
let fromlist = self.materialize_tuple_const(&["baz", "qux"]);
self.emit_load_const(fromlist, line);
self.emit(op::IMPORT_NAME, self.add_name("foo.bar"), line);
self.emit(op::IMPORT_FROM, self.add_name("baz"), line);
self.emit(op::STORE_NAME, self.add_name("baz"), line);
self.emit(op::IMPORT_FROM, self.add_name("qux"), line);
self.emit(op::STORE_NAME, self.add_name("qux"), line);
self.emit(op::POP_TOP, 0, line);
```

For `from foo import *`:

```rust
self.emit_load_const(Value::small_int_unchecked(0), line);
self.emit_load_const(self.materialize_tuple_const(&["*"]), line);
self.emit(op::IMPORT_NAME, self.add_name("foo"), line);
self.emit(op::IMPORT_STAR, 0, line);
```

`materialize_tuple_const` is a new compiler helper: packs `&[&str]` into `HeapObject::Tuple(Vec<Value>)`, allocates into the compiler's heap, returns the `object_ref` Value for the constants pool. Lives next to `add_const`. Justified beyond imports — future MAKE_FUNCTION default-args work will want it.

Level and fromlist are compile-time constants; the actual import work is purely runtime. Matches CPython's design and keeps IMPORT_NAME's opcode arm a thin dispatch.

## The cmodule trait and registry

### Trait (`src/object.rs` near `HeapObject`)

```rust
pub trait CModule {
    /// Fully-qualified Python import name. Top-level only in M3 ("sys").
    fn name(&self) -> &'static str;

    /// Build the module's namespace. Called once per VM run on first import.
    /// MUST NOT cache the heap reference — heap may reallocate between calls.
    fn build_globals(&self, heap: &mut Vec<HeapObject>) -> HashMap<String, Value>;
}
```

Small on purpose: a cmodule is name + namespace producer. Everything else (caching, exposing as `HeapObject::Module`, dispatching `import sys`) lives in the import machinery; the cmodule doesn't see any of it.

### Registry (`src/cmodules/mod.rs` — new directory)

```rust
mod sys;     // M3 ships this one
// future:
// mod io;     // _io
// mod codecs; // _codecs
// ...

pub fn registry() -> Vec<Box<dyn CModule>> {
    vec![
        Box::new(sys::Sys),
        // Box::new(io::Io),
    ]
}
```

Adding a cmodule = one `mod foo;` + one `vec![]` entry. No other code changes.

### Import-machinery indexing

```rust
// src/import.rs
pub struct ImportSystem {
    cmodules: HashMap<&'static str, Box<dyn CModule>>,
    sys_path: Vec<PathBuf>,
}

impl ImportSystem {
    pub fn new(sys_path: Vec<PathBuf>) -> Self {
        let mut cmodules = HashMap::new();
        for m in crate::cmodules::registry() {
            cmodules.insert(m.name(), m);
        }
        Self { cmodules, sys_path }
    }
}
```

`O(1)` cmodule lookup. Cmodules take precedence over `.py` files of the same name — matches CPython behavior where built-ins outrank filesystem modules.

### The `sys` cmodule

The complete M3 contents — deliberately minimal. Most stdio/argv fields are `None` placeholders that the VM patches after registration (they depend on runtime state).

```rust
// src/cmodules/sys.rs
pub struct Sys;

impl CModule for Sys {
    fn name(&self) -> &'static str { "sys" }

    fn build_globals(&self, heap: &mut Vec<HeapObject>) -> HashMap<String, Value> {
        let mut g = HashMap::new();
        g.insert("version".into(),      alloc_str(heap, "3.0.1 (python-rs, compatibility target)"));
        g.insert("version_info".into(), alloc_tuple(heap, vec![
            Value::small_int_unchecked(3), Value::small_int_unchecked(0),
            Value::small_int_unchecked(1), alloc_str(heap, "final"),
            Value::small_int_unchecked(0),
        ]));
        g.insert("platform".into(), alloc_str(heap, platform_string()));
        g.insert("maxsize".into(),  Value::small_int_unchecked((1i64 << 47) - 1));
        // VM patches these after registration:
        g.insert("argv".into(),    Value::none());
        g.insert("path".into(),    Value::none());
        g.insert("modules".into(), Value::none());
        g.insert("stdout".into(),  Value::none());
        g.insert("stderr".into(),  Value::none());
        g.insert("stdin".into(),   Value::none());
        g
    }
}

fn platform_string() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux", "macos" => "darwin", "windows" => "win32", other => other,
    }
}
```

Real stdio comes when `_io` lands in M3.1. `sys.modules` synthesis as a Python dict comes when needed by an actual test file.

## `sys.path`, finder/loader chain, bootstrap

### `sys.path` at startup

```rust
sys_path = vec![
    script_dir_or_empty,                // entry 0: dir of script being run, or ""
    stdlib_dir,                         // entry 1: resolved from one of:
                                        //   a. $PYTHONRSHOME/Lib
                                        //   b. <exe-dir>/../vendor/cpython-3.0/Lib (dev build)
                                        //   c. <exe-dir>/Lib (installed)
];
```

Existence-probed at startup; if no `Lib/` is found, `sys.path[1]` is omitted and stdlib imports surface a clear ImportError. `PYTHONPATH`, `-I`, `-S` deferred.

### Finder/loader chain (`src/import.rs::ImportSystem::find_and_load`)

```rust
pub fn find_and_load(
    &mut self,
    name: &str,                     // absolute resolved name, e.g. "foo.bar"
    heap: &mut Vec<HeapObject>,
    vm: &mut VM,
) -> Result<Value, PythonError> {
    if let Some(&cached) = vm.sys_modules.get(name) {
        return Ok(cached);
    }

    let (parent_name, leaf_name) = split_dotted(name);
    let parent_pkg = if let Some(p) = parent_name {
        Some(self.find_and_load(p, heap, vm)?)
    } else {
        None
    };

    let search_dirs: Vec<PathBuf> = match &parent_pkg {
        Some(parent_value) => vec![package_dir_of(parent_value, heap)?],
        None => self.sys_path.clone(),
    };

    for finder in &[FinderKind::CModule, FinderKind::Package, FinderKind::SourceFile] {
        for dir in &search_dirs {
            if let Some(loaded) = finder.try_load(name, leaf_name, dir, heap, vm, self)? {
                vm.sys_modules.insert(name.to_string(), loaded);
                if let Some(parent_value) = &parent_pkg {
                    set_module_attr(parent_value, leaf_name, loaded, heap)?;
                }
                return Ok(loaded);
            }
        }
    }

    Err(PythonError::runtime(format!("No module named '{name}'"), 0))
}
```

The three finders:

- **CModule** — consults the cmodule registry. M3 has no submodule cmodules, so only relevant when `parent_pkg.is_none()`. Hit → construct `HeapObject::Module`, call `cmod.build_globals(heap)`, return.
- **Package** — checks `<dir>/<leaf>/__init__.py`. Hit → construct module with `file = Some(...)`, `package = Some(name)`, **cache first**, execute `__init__.py` in module's globals, mark `initialized = true`.
- **SourceFile** — checks `<dir>/<leaf>.py`. Same load-then-execute flow; `package` is the parent's name (or `None` for top-level), and the module isn't a package itself.

### Cache-before-execute (load-bearing)

`sys.modules` insertion happens **before** body execution. This is what makes circular imports work: if `a.py` imports `b` and `b.py` re-imports `a`, the second import returns the partially-initialized `a` from cache with whatever was bound before the `import b` line. Documented Python behavior; `test_import.py` exercises it.

### Module-body execution

Loaders push a `Frame` whose `globals` references the module's `globals` HashMap. The frame runs to completion via the existing VM dispatch loop. When `RETURN_VALUE` fires on a module-level frame, the import machinery resumes, marks the module `initialized = true`, and the recursion unwinds.

### Bootstrap order at VM startup

```rust
// src/vm.rs::VM::new (extended)
fn new(...) -> Self {
    let mut vm = Self { /* ... */ sys_modules: HashMap::new(), ... };
    vm.import_system = ImportSystem::new(/* sys.path */);

    // Eagerly load `sys` so we can patch its dynamic fields BEFORE any user code.
    let sys_value = vm.import_system.find_and_load("sys", &mut vm.heap, &mut vm)?;
    vm.patch_sys_dynamic_fields(sys_value);  // sets sys.argv, sys.path, sys.modules

    vm
}
```

After bootstrap, `import sys` from user code is a cache hit — no double-init.

### Relative import resolution

For `from .. import x` inside a module with `package = Some("foo.bar.baz")`:

1. `level = 2`, `module = None`.
2. Walk `level` segments up from `package`: 2 up from `foo.bar.baz` is `foo`.
3. Resolved absolute name = `"foo"`. Hand to `find_and_load`.
4. If `module` is `Some("sub")` instead of `None`, resolved name is `"foo.sub"`.

Errors:
- Relative import at module level (no `package`): `ImportError: attempted relative import with no known parent package`.
- Depth exceeds `package` segments: `ImportError: attempted relative import beyond top-level package`.

### Failure-mode catalog

| Situation | Behavior |
|---|---|
| `import missing` | `ImportError: No module named 'missing'` |
| `import foo` where `foo.py` has a `SyntaxError` | `SyntaxError` with file:line, not `ImportError` |
| `import foo` where `foo.py` raises at load | exception propagates; `foo` NOT cached in `sys.modules` |
| Circular: `a` imports `b` which re-imports `a` | second import returns half-built `a` from cache (matches CPython) |
| Relative depth too high | `ImportError` with clear message |
| `from foo import bar` where `bar` is neither attr nor submodule | `ImportError: cannot import name 'bar' from 'foo'` |

### `ImportError` exception type

```rust
pub enum ExceptionType {
    // ...existing...
    ImportError,
}
```

Plus the matching `name()` and `from_name()` cases.

## Tests

Rust unit tests in `src/import.rs` and `src/cmodules/sys.rs` under `#[cfg(test)]`:

| Test | Verifies |
|---|---|
| `import_cache_returns_same_value` | `import sys` twice returns bit-identical Value |
| `import_missing_raises_importerror` | clear error message on missing module |
| `cmodule_takes_precedence_over_file` | sys.py on sys.path doesn't shadow the cmodule |
| `package_init_runs_once` | `pkg/__init__.py` body executes exactly once across multiple imports |
| `submodule_load_searches_only_package_dir` | submodules don't fall back to sys.path |
| `relative_import_resolves_from_package` | `from .sub import x` inside a package works |
| `relative_import_at_module_level_errors` | top-level `from . import x` raises ImportError |
| `relative_import_too_deep_errors` | depth > parent count raises ImportError |
| `circular_import_returns_partial_module` | exercises cache-before-execute |
| `from_import_star_uses_dunder_all` | `__all__` controls star-import binding |
| `from_import_star_falls_back_to_public_names` | without `__all__`, underscore-prefix excluded |
| `module_attr_access_after_partial_init` | mid-execution module raises AttributeError on undefined names |
| `import_from_with_submodule_triggers_subimport` | `from foo import bar` falls back to `import foo.bar` |
| `from_import_as_binds_alias` | `from foo import bar as b` binds `b`, not `bar` |

Integration tests via Python scripts under `tests/`:

| Script | Asserts |
|---|---|
| `tests/import_sys.py` | `sys.version`, `sys.platform`, `sys.maxsize` produce expected three lines |
| `tests/import_pkg.py` + `tests/pkg/__init__.py` + `tests/pkg/sub.py` | `from pkg.sub import x` end-to-end |
| `tests/import_relative.py` + `tests/pkg2/...` | relative-import in a package body |
| `tests/import_circular.py` + `tests/circa/a.py` + `tests/circa/b.py` | partial-module on cycle |

`compat-report.json` after M3:
- `test_grammar`, `test_syntax` should newly pass — they exercise only the language, but were blocked because every `Lib/test/*` file `import test.test_support` (which needs the import system + several cmodules).
- Most other test files still fail with clear "cmodule X not yet implemented" `ImportError`s.
- The harness can finally produce a non-zero pass count.

## Follow-up specs

Per-cmodule, each its own short design doc:

| Order | Spec | Unlocks |
|---|---|---|
| M3.1 | `_io` + `_codecs` + `_warnings` | real `print()`, stdlib import warnings |
| M3.2 | `posix` + `errno` | `os.path`, `os.environ`, file-system ops |
| M3.3 | `_string` + `_collections` + `_functools` + `_heapq` + `itertools` | small accelerators most stdlib imports |
| M3.4 | `_struct` + `math` + `_random` | numerics + struct packing |
| M3.5 | `_sre` (regex via `regex` crate) | `re` — unblocks many `test_*` |
| M3.6 | `_thread` (stub) | tests that gate on `import _thread` succeeding |
| M3.7 | `_pickle` | `test_pickle`, `test_copy` |
| M3.8 | `unittest` + `test.regrtest` triage | regrtest invocable; closes M3 |

Each follow-up spec is small (50–200 lines), self-contained, and does not need to re-design the import system.

## Risks

| Risk | Mitigation |
|---|---|
| `Frame::globals` refactor touches every frame-creation site | Land as the first sub-commit; verify all existing tests pass before any import code merges |
| Circular-import semantics differ from CPython in subtle ways (e.g., generator-in-module-body) | Pin simple cases as tests; triage divergences when running `test_import.py` |
| `sys.path` stdlib resolution varies by `cargo run` vs install layout | Eager existence probing at startup; clear error if `Lib/` missing; `PYTHONRSHOME` env-var override |
| Module-body execution while a frame is on the stack interacts with stackless dispatch | Module loading uses the same frame-push mechanism as function calls — no new VM control flow |
| Vendored stdlib uses `from __future__ import ...` (2→3 transition leftover) | Recognize `__future__` as an always-empty module; the import no-ops |
| Star imports walking globals could be slow for large modules | Lazy `__all__` cache on Module struct (already in design); rare in practice |
| Reading `.py` at runtime requires file I/O before `_io` exists | Import machinery uses `std::fs::read_to_string` directly — independent of Python's `_io` |

## Open questions (deferred)

- `.pyc` cache format / write semantics — post-compat optimization.
- `sys.meta_path` user-extensible finder hooks — never in scope.
- Lazy module loading (`importlib.util.LazyLoader`) — doesn't exist in 3.0.1; never.
- Module-attribute access performance — current `HashMap<String, Value>` is fine; switch to indexed slots if benchmarks ever say so.

## Immediate next step

Implementation plan via the `writing-plans` skill, then begin with the `Frame::globals` refactor as commit 1.
