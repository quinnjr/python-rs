# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project context

`python-rs` is a from-scratch Rust port of CPython, written primarily through AI-assisted coding as an academic / hobby experiment. It is **not** a production interpreter. The README's framing of the project applies here too — disclose AI assistance, do not deploy.

## Authoritative roadmap

The active design document is:

> `docs/superpowers/specs/2026-05-19-cpython-3.0-compatibility-design.md`

It defines the milestones M0–M10 and the strategy for getting there. Key facts that you will not derive from reading code alone:

- **North-star milestone is CPython 3.0.1 compatibility**, not Pandas. Pass `vendor/cpython-3.0/Lib/test/` first; Pandas, NumPy, and `cpyext` resume after M10.
- **`int` is arbitrary precision.** Python 3 unified `int` and `long` — implementations of integer values must NaN-box small values *and* spill to `num_bigint::BigInt` on overflow. There is no "i64 only" path.
- **External crates are allowed liberally** where they reduce work or improve correctness (e.g., `regex`, `encoding_rs`, `flate2`, `num-bigint`, `chrono`). Each crate gets a one-line justification in `Cargo.toml`.
- **Stdlib strategy is "vendor, not reimplement":** the pure-Python stdlib at `vendor/cpython-3.0/Lib/` is run as-is. We only write Rust for the language and the minimum C-module replacements that the vendored stdlib depends on.

**Current state:** M0 (Phase 2 object model — classes, dunders, closures, generators, exceptions) in progress on `develop`. M1 (vendoring + harness) complete. M2 (big-int representation) is next and gets its own design doc before implementation.

## Repo layout (the parts not obvious from `ls`)

- `src/` — Rust source. Currently **flat** (no `src/types/`, no `src/cmodules/` yet — the nested layout described in the spec is aspirational). The pipeline is `lexer → parser → compiler → vm`, all wired in `src/main.rs`.
- `vendor/cpython-3.0/` — verbatim CPython 3.0.1 stdlib + `Lib/test/`. **PSF-2.0 licensed. Do not edit files in place** — all local modifications live as numbered patches in `vendor/cpython-3.0/patches/` (currently empty).
- `tests/` — both our own Python test scripts (`*.py`) at the top level, and the CPython compat infrastructure (`tests/cpython-3.0/skip.toml`, `tests/cpython-3.0/compat-report.json`).
- `docs/3.0-compat-progress.md` — auto-generated headline `X / N tests passing` dashboard. Currently 0/325.
- `docs/superpowers/specs/` — design documents (currently one: the 3.0.1 compat spec).
- `scripts/run-cpython-tests.sh` — invokes CPython's own `regrtest.py` *under* python-rs. Will fail until milestone M3 lands the import system.

## Common commands

```sh
# Build
cargo build                          # debug
cargo build --release                # always use release for any timing or compat run

# Run a Python file
cargo run --release -- path/to/script.py

# Tests
cargo test                           # all Rust unit + integration tests
cargo test <name>                    # filter by test name substring
cargo test -- --nocapture            # show println! output during tests

# Lint / format
cargo clippy -- -D warnings          # required clean before commit
cargo fmt                            # required before commit
cargo fmt --check                    # CI check

# CPython compatibility suite (fails until M3 — that is expected)
scripts/run-cpython-tests.sh         # full Lib/test/ run
scripts/run-cpython-tests.sh --fast  # tier-1 smoke list (~10 files)
scripts/run-cpython-tests.sh test_grammar  # single test file
```

## Architecture in one paragraph

A Python source string flows through `lexer::tokenize` → `parser::parse` (recursive descent, full 3.11+ grammar — over-built for 3.0 but harmless) → `compiler::compile` (emits bytecode and a heap of code objects) → `VM::new(...).run()`. The VM is **stackless**: Python function calls do not map to Rust stack frames; instead a `Vec<Frame>` lives on the heap and the dispatch loop switches frames by updating an index. There is no recursive `execute()` in Rust. Output is captured into `vm.output` rather than written directly to stdout — `main.rs::run` is what prints it. Values use **NaN-boxing** (`src/object.rs`) so small ints/floats/bools/None ride inline in 8 bytes; heap objects are `Rc`-managed. Bytecode is **our own format** (specialized opcodes, register-style where it pays off), not CPython's — we never load `.pyc` files from CPython.

## Architectural constraints (still load-bearing)

- **Stackless is non-negotiable.** `CALL_FUNCTION` pushes a frame and continues the dispatch loop; `RETURN_VALUE` pops and continues. Anything that introduces Rust recursion through Python code paths is wrong.
- **`unsafe` allowed only in:** `src/object.rs` (NaN-boxing), `src/vm.rs` (hot-loop stack access), and `src/cpyext/` once it exists. Every `unsafe` block needs a `// SAFETY:` comment. Nowhere else.
- **Bytecode independence.** Our bytecode is not CPython 3.0's. Tests that introspect bytecode (`dis`, `co_code`) go on the skip list under `cpython_internal`.
- **Edition 2024**, Rust stable.

## Clean-room implementation policy

**This project is a clean-room reimplementation of CPython, not a translation of it.** Two rules enforce that:

1. **Rust interpreter and C-module replacements are written clean-room.** When writing or extending any Rust code in `src/` (the lexer, parser, compiler, VM, object model, and the Rust modules under `src/cmodules/` that replace CPython's C extensions), do **not** read CPython's own source for the same component — i.e., do not look at CPython's `Modules/`, `Python/`, `Objects/`, `Parser/`, or `Include/`. Work from the **Python language reference, the data model docs, the relevant PEPs, and the behavioral test cases**. Once the Rust implementation passes its unit tests and the relevant `Lib/test/` files, you may then consult CPython source for performance ideas, edge-case verification, or comparison — but only after the implementation already works.

2. **Python stdlib modules we reimplement ourselves are written clean-room.** Where we choose to replace a vendored `Lib/` module with our own Python or Rust implementation (rare — the whole point of vendoring is to avoid this), the new implementation is written from the documented behavior and tests, not by transcribing or paraphrasing CPython's `Lib/<module>.py`. The vendored copy is the runtime; it is not a reference to copy from.

**What is explicitly fine:**
- Running the vendored CPython `Lib/` code as-is. That is the whole point of vendoring.
- Reading vendored `Lib/test/` test files to understand expected behavior — tests *are* the spec.
- Reading the Python language reference, data model docs, PEPs, and Python's own `Doc/` if needed.
- Consulting CPython source *after* a clean-room implementation passes its tests, to compare or improve.

**Why:** This is the intellectual point of the project. Transcribing CPython into Rust teaches nothing; reimplementing from spec exposes every place CPython's behavior is under-documented, surprising, or load-bearing. The test suite is the contract; the source is the suspect.

**How to apply:** If you find yourself wanting to read `Modules/_io.c` or `Objects/listobject.c` while writing the equivalent Rust, stop and consult docs/PEPs/tests instead. Note the question and the source you considered in the PR description so the boundary is auditable. Same rule for `Lib/<module>.py` when implementing our own replacement for that module.

## License model

Aggregate. Our code (everything outside `vendor/`) is **MIT OR Apache-2.0**. The vendored CPython 3.0.1 tree retains **PSF License v2**. Any patches applied to vendored files are derivative works and stay under PSF-2.0. `.gitattributes` marks `vendor/cpython-3.0/` as `linguist-vendored`. `Cargo.toml`'s `license` field describes our contribution only — do not change it to a compound expression.

## Branch and commit conventions

- `main` — stable / published releases.
- `develop` — integration branch; current active work.
- `feature/<name>` — short-lived branches that fast-forward into `develop`.
- Commits are typically conventional-style prose (no Conventional Commits prefix required) and end with a `Co-Authored-By:` trailer when AI-assisted. Phase work is committed in coherent chunks per milestone, not per-file.

## When in doubt

1. Read the active spec under `docs/superpowers/specs/` for current direction.
2. Check `docs/3.0-compat-progress.md` for the latest pass/fail headline.
3. The vendored CPython 3.0.1 `Lib/test/` files are the behavioral spec for everything language-level.
