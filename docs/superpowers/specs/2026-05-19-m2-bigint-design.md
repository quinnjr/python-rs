# M2 — Big-Int Value Representation

**Date:** 2026-05-19
**Status:** Approved, ready for implementation planning
**Milestone:** M2 in `2026-05-19-cpython-3.0-compatibility-design.md`
**Prereq:** M0 (Phase 2 object model) complete on `develop`

## Summary

Add arbitrary-precision integer support to `python-rs`. Small ints continue to ride in the NaN-box payload as i48; integers that overflow promote to a heap `num_bigint::BigInt`. A single `PyInt<'a>` wrapper presents a unified arithmetic surface to every call site. The CPython-compatible int hash (Mersenne-reduction, allocation-free) lands as part of the same milestone.

This unblocks every Python 3.0.1 `Lib/test/` file that exercises large integers, hash semantics on ints, or dict/set keys with int keys — which is most of the core-language test surface.

## Goals

1. **Arbitrary precision.** Any Python integer expressible in CPython 3.0.1 is representable, computed, and stringified correctly under python-rs.
2. **Small-int fast path stays fast.** Programs that never overflow i48 pay zero overhead from M2 — no extra branches in the dispatch loop, no heap allocations, no widened tokens at the lexer.
3. **Hash matches CPython byte-for-byte.** Required for `test_hash`, `test_dict`, `test_set`, and any dict/set keyed on ints. Algorithm is documented Mersenne reduction; we never read CPython source for it.
4. **Single point of arithmetic logic.** The `PyInt` wrapper is the only place that knows about small/big representation. Every arithmetic opcode and every int-touching builtin goes through it.
5. **Migration is reviewable.** Six commits, each leaving the tree test-green.

## Non-Goals

- Big-int optimizations beyond what `num-bigint` provides out of the box (no GMP, no fast Schönhage-Strassen, no SIMD).
- `__hash__` protocol dispatch on user-defined classes — only the int hash matters here.
- BigInt pickling — comes when `_pickle` lands in M7.
- `OverflowError` on float→int conversions of `inf`/`nan` — falls out of general type-error path, not a deliberate M2 feature.
- A new NaN-box scheme. The on-disk `Value(u64)` layout does not change.
- Threading / send across threads for BigInt — same single-threaded model as the rest of the interpreter.

## Value representation

The on-disk shape of `Value` does not change. The existing 8-tag NaN-boxing scheme (3-bit tag + 48-bit payload) is preserved. Only the `HeapObject` enum grows and the `Value` constructor surface tightens.

### HeapObject addition

```rust
pub enum HeapObject {
    // ...existing variants...
    /// Arbitrary-precision integer. Reached via TAG_OBJECT. Only present
    /// when a value has overflowed the i48 small-int range, or when a
    /// source literal exceeds i64.
    BigInt(BigInt),
}
```

No new tag. No tag-space rearrangement. `is_int(v, heap)` becomes "tag is TAG_INT, or tag is TAG_OBJECT and heap entry is `BigInt`."

### Load-bearing invariants

1. A value represents an int iff `is_int_tag(v)` holds, or (`is_object_tag(v)` and `heap[idx] == BigInt(_)`).
2. **Demotion invariant:** any `Value` whose mathematical value fits in `[-2^47, 2^47)` *must* be tagged TAG_INT. Constructing a TAG_OBJECT → BigInt with a small magnitude is a bug. Enforced by every construction site routing through `Value::from_bigint` or `PyIntOwned::demote`.
3. `bool` keeps its own tag. Arithmetic widens bool to small-int at the operation site (`PyInt::from_value_or_bool`).

### Constructor surface

```rust
impl Value {
    /// Construct an int from i64. Stays in TAG_INT (no allocation) when
    /// the value fits in i48; promotes to a heap BigInt otherwise. This
    /// is the canonical "I have an i64, give me the right Value" entry.
    pub fn from_i64(v: i64, heap: &mut Vec<HeapObject>) -> Self;

    /// Construct an int from a BigInt; demotes to small int if the
    /// value fits in i48, otherwise allocates a HeapObject::BigInt.
    pub fn from_bigint(v: BigInt, heap: &mut Vec<HeapObject>) -> Self;

    /// Bit-equal comparison (for `id()`-like uses).
    pub fn bits_eq(self, other: Self) -> bool;

    /// Python value equality. Required because two equal ints can live
    /// in different representations (small `7` vs heap BigInt-7).
    pub fn py_eq(self, other: Self, heap: &[HeapObject]) -> bool;
}
```

The existing `Value::int(i64)` is renamed `Value::small_int_unchecked(i48)` and kept for internal use where the caller has already proven the i48 range. The `#[derive(PartialEq)]` on `Value` is removed in favor of explicit `bits_eq` / `py_eq`. Call sites that used `==` migrate to whichever they actually meant.

### Why TAG_OBJECT rather than a dedicated tag

Consistent with how every other heap-resident type (`Dict`, `Tuple`, `Set`, `Class`, `Instance`, iterators, `Cell`, `Closure`, etc.) already routes through TAG_OBJECT. No new tag plumbing. The extra branch in int type checks is uncontroversial — BigInt operations are heap-allocating and intrinsically slow, the branch cost is in the noise.

## `PyInt` wrapper and arithmetic API

The single chokepoint for every int-touching arithmetic operation. Lives in `src/object.rs`.

### Types

```rust
pub enum PyInt<'a> {
    Small(i64),
    Big(&'a BigInt),
}

pub enum PyIntOwned {
    Small(i64),
    Big(BigInt),
}

pub enum PyPowResult {
    Int(PyIntOwned),
    Float(f64),     // e.g., 2**-3
}

pub enum ArithError {
    DivByZero,
    NegativeShift,
    NegativePower,  // pow_mod with negative exp and no mod
}
```

`PyInt<'a>` is a borrowed view used in arithmetic. `PyIntOwned` is the operation result; the VM converts it back via `Value::from_pyint_owned(result, heap)`. The split prevents borrow-checker friction when the heap is held mutably during a binary op.

### Construction

```rust
impl<'a> PyInt<'a> {
    /// Returns Some(PyInt) if v is an int (small or big). Bool is NOT
    /// treated as int here — callers explicitly widen via the variant below.
    pub fn from_value(v: Value, heap: &'a [HeapObject]) -> Option<Self>;

    /// Widen bool to int (True→1, False→0). Use at every arithmetic
    /// call site, since Python treats `True + 1 == 2`.
    pub fn from_value_or_bool(v: Value, heap: &'a [HeapObject]) -> Option<Self>;
}
```

### Arithmetic methods — every method returns `PyIntOwned` (or a Result thereof)

| Method | Behavior |
|---|---|
| `add(other)`, `sub(other)`, `mul(other)` | i64 `checked_*` fast path; on overflow promote both to BigInt and retry; result demotes via `PyIntOwned::demote()` |
| `floordiv(other) -> Result<PyIntOwned, ArithError>` | Python floor division (sign of divisor); `ArithError::DivByZero` for 0 |
| `mod_(other) -> Result<PyIntOwned, ArithError>` | Python modulo (sign of divisor); pairs with `floordiv` |
| `divmod(other) -> Result<(PyIntOwned, PyIntOwned), ArithError>` | Fused op for the `divmod` opcode |
| `pow(other) -> PyPowResult`, `pow_mod(other, mod_) -> Result<PyIntOwned, ArithError>` | Negative exponent in `pow` returns `Float`; three-arg `pow` always Int |
| `neg() -> PyIntOwned`, `abs() -> PyIntOwned` | Unary |
| `and_(other)`, `or_(other)`, `xor_(other)` | Bitwise — Python's infinite-width two's-complement semantics for negatives |
| `shl(other) -> Result<PyIntOwned, ArithError>`, `shr(other) -> Result<PyIntOwned, ArithError>` | Errors on negative shift |
| `invert() -> PyIntOwned` | `~x == -x - 1` |
| `cmp(other) -> Ordering`, `eq(other) -> bool` | Total int-vs-int ordering and equality (handles small-7 vs Big-7) |
| `hash() -> i64` | CPython-compatible (see next section) |
| `to_f64() -> f64` | For mixed int/float ops; may lose precision; huge BigInt → `inf` |

### Methods that don't live on `PyInt`

- `pyint_truediv(a: PyInt, b: PyInt) -> Result<f64, ArithError>` — always returns float, free function next to the wrapper.
- Mixed int/float comparison and arithmetic — handled at the VM dispatch site, using `PyInt::to_f64` on the int side.

### Demote-in-one-place

```rust
impl PyIntOwned {
    /// The single point that upholds the demotion invariant.
    /// Returns Small if the value fits in i48, otherwise Big.
    fn demote(self) -> Self;
}
```

Every arithmetic method calls `.demote()` on its result. `Value::from_bigint` and `Value::from_pyint_owned` both call it too. There is exactly one place that checks "does this fit in i48," and it's `demote`.

## Hash algorithm — optimized

CPython uses `hash(x) = sign(x) · (|x| mod (2^61 − 1))` with `hash(-1) → -2`. We match it byte-for-byte. The modulus `P = 2^61 − 1` is a Mersenne prime, which enables division-free reduction.

### Primitive — Mersenne reduction

For `M = 2^n − 1`, the identity `2^n ≡ 1 (mod M)` gives `x ≡ (x >> n) + (x & M) (mod M)`. Applied with `n = 61`:

```rust
const PYHASH_BITS:    u32 = 61;
const PYHASH_MODULUS: u64 = (1 << PYHASH_BITS) - 1;

#[inline(always)]
const fn mod_mersenne_u64(x: u64) -> u64 {
    let r = (x & PYHASH_MODULUS) + (x >> PYHASH_BITS);
    if r >= PYHASH_MODULUS { r - PYHASH_MODULUS } else { r }
}

#[inline(always)]
const fn mod_mersenne_u128(mut x: u128) -> u64 {
    x = (x & PYHASH_MODULUS as u128) + (x >> PYHASH_BITS);
    x = (x & PYHASH_MODULUS as u128) + (x >> PYHASH_BITS);
    let r = x as u64;
    if r >= PYHASH_MODULUS { r - PYHASH_MODULUS } else { r }
}
```

The `if r >= …` compiles to a single conditional-move; no actual branch in the small-int hash.

### Hot path

```rust
#[inline(always)]
fn hash_small_i64(i: i64) -> i64 {
    let abs    = i.unsigned_abs();
    let h      = mod_mersenne_u64(abs) as i64;
    let signed = if i < 0 { -h } else { h };
    signed - ((signed == -1) as i64)            // -1 → -2, branchless
}
```

~10 cycles total. No division, no allocation, no branches in the body.

### Cold path

Walk u64 limbs of the BigInt magnitude, accumulating `h ≡ Σᵢ Lᵢ · 2^(64i) (mod P)`. Since `2^64 ≡ 8 (mod P)`, the running scale factor multiplies by 8 each limb, reducible by the same Mersenne trick.

```rust
#[inline(never)]
#[cold]
fn hash_bigint(b: &BigInt) -> i64 {
    let mut h:    u64 = 0;
    let mut pow8: u64 = 1;
    for limb in b.iter_u64_digits() {
        let limb_mod = mod_mersenne_u64(limb);
        let term     = mod_mersenne_u128(limb_mod as u128 * pow8 as u128);
        h            = mod_mersenne_u64(h + term);
        pow8         = mod_mersenne_u64(pow8 << 3);
    }
    let signed = if b.sign() == Sign::Minus { -(h as i64) } else { h as i64 };
    signed - ((signed == -1) as i64)
}
```

`iter_u64_digits` borrows the magnitude — no allocation. A 1-limb BigInt (the common immediate-overflow case) hashes in ~25 cycles. A 64-limb (4096-bit) int hashes well under a microsecond.

### Dispatch

```rust
impl PyInt<'_> {
    #[inline]
    pub fn hash(&self) -> i64 {
        match self {
            PyInt::Small(i) => hash_small_i64(*i),
            PyInt::Big(b)   => hash_bigint(b),
        }
    }
}
```

`#[inline(always)]` keeps the small-int hash inlined into the dict-lookup hot path; `#[cold]` on `hash_bigint` keeps it out of icache contention.

### `value_hash` becomes a thin dispatcher

For ints (small or big), call `PyInt::hash`; for bool, hash the int 0 or 1 through the same path; for None/str/etc., existing per-type algorithms unchanged. Bool hashing is therefore identical to int hashing — `hash(True) == hash(1) == 1`, required so `{True: 'a', 1: 'b'}` is a one-key dict.

### Pinned test corpus

Split by which hash path is exercised. All inputs and outputs are determined entirely by the algorithm above.

**Small-int path (`hash_small_i64`, i48 range):**

| Input | Expected `hash()` | Why |
|---|---|---|
| `0` | `0` | identity |
| `1` | `1` | identity |
| `-1` | `-2` | sentinel substitution |
| `2^47 − 1` (i48 max) | `2^47 − 1` | small-int boundary, hits no wraparound |
| `-(2^47)` (i48 min) | `-(2^47)` | small-int boundary, negative side |
| `True`, `False` | `1`, `0` | bool inherits int hash |

**Big-int path (`hash_bigint`, one or more u64 limbs):**

| Input | Expected `hash()` | Why |
|---|---|---|
| `2^61 − 1` (BigInt, one limb) | `0` | exactly the modulus — checks the `>=` branch |
| `2^61` (BigInt, one limb) | `1` | first wraparound |
| `-(2^61)` (BigInt, one limb) | `-2` | sentinel substitution through BigInt path: `|x| mod P = 1`, sign flip → `-1` → `-2` |
| `2^62 + 1` (BigInt, one limb) | `3` | `(2·P + 1) ≡ 0 + 2 + 1 ≡ 3 (mod P)` |
| `BigInt::from(10).pow(100)` (multi-limb) | computed once, pinned in test | end-to-end multi-limb sanity |

## Literal pipeline

Three optimizations turn the literal path from "correct" into "barely visible in profiles":

### Single opcode, pre-materialized constants

`LOAD_CONST` (or a new `LOAD_CONST_INT` if a typed opcode helps cache locality) pushes a pre-built `Value`. The code object holds two const stages:

```rust
pub struct CodeObject {
    pub consts: Vec<ConstValue>,    // pre-materialization
    // ...
}

pub enum ConstValue {
    SmallInt(i64),
    BigInt(Box<BigInt>),    // boxed — keeps the enum to ~16 bytes
    Float(f64),
    Str(Box<str>),
    None,
    Bool(bool),
    // ...other existing const types
}

pub struct LoadedCode {
    pub consts: Box<[Value]>,       // post-materialization, ready to push
    // ...
}
```

At VM startup (or on first load of a code object), the VM walks each `CodeObject::consts` and produces `LoadedCode::consts` — BigInt constants get allocated into the heap exactly once. After load, `OpCode::LoadConst` is one indexed read + one push. Zero per-execution allocation for BigInt literals.

### Lexer digit-count dispatch — no double-parse

```rust
fn lex_int_literal(text: &str, base: u32) -> IntLiteral {
    let digits = text.bytes().filter(|b| *b != b'_').count();
    let max    = max_i64_digits_for(base);  // 19 for base 10

    if digits < max {
        IntLiteral::Small(i64::from_str_radix(&strip_underscores(text), base).unwrap())
    } else if digits == max {
        match i64::from_str_radix(&strip_underscores(text), base) {
            Ok(i)  => IntLiteral::Small(i),
            Err(_) => IntLiteral::Big(Box::new(BigInt::from_str_radix(&strip_underscores(text), base).unwrap())),
        }
    } else {
        IntLiteral::Big(Box::new(BigInt::from_str_radix(&strip_underscores(text), base).unwrap()))
    }
}
```

The vast majority of literals (≤ 18 digits in base 10) take the fast branch with zero BigInt-related work. `strip_underscores` returns `Cow<'_, str>` to avoid allocation when the source has no underscores.

### Compile-time deduplication

```rust
struct ConstPoolBuilder {
    consts:       Vec<ConstValue>,
    small_lookup: HashMap<i64, u32>,
    big_lookup:   HashMap<BigInt, u32>,
    // ...
}
```

`0`, `1`, `-1`, and any other repeated literals occupy one pool slot. Smaller pool, better L1 residency, branch predictor and prefetcher rewarded.

### Layout choices that compound

- `Box<BigInt>` keeps `ConstValue` at ~16 bytes (raw `BigInt` is 40+). Tokens and AST nodes stay small.
- `LoadedCode::consts` is `Box<[Value]>` not `Vec<Value>` — saves the capacity field per code object, signals immutability.
- Pool index is `u32` (65k+ headroom).

## Migration plan

Six commits, each leaves the tree test-green.

| # | Commit | What lands |
|---|---|---|
| 1 | `HeapObject::BigInt` + constructors | New variant, `Value::from_i64` / `from_bigint`, `PartialEq` derive replaced with `bits_eq` / `py_eq`. No arithmetic changed yet. |
| 2 | `PyInt` wrapper + standalone tests | Full arithmetic surface from Section 3, hash from Section 4. Library code only; not wired into VM. Unit tests in `object.rs`. |
| 3 | VM arithmetic opcodes migrated | Every `as_int().unwrap()` arithmetic site replaced with `PyInt::from_value(...).method(...)`. New Python integration tests under `tests/`. May be split per opcode family if review surface is too large. |
| 4 | Lexer/parser big-literal support | `IntLiteral` enum, digit-count dispatch, `ConstValue::BigInt`, code-object load-time materialization, compile-time dedup. End-to-end `print(2**1000)` works. |
| 5 | Builtins + string conversion | `int()`, `str()`, `repr()`, `abs()`, `divmod()`, `float(big_int)`, `bool(big_int)`. All user-visible int paths respect arbitrary precision. |
| 6 | Cleanup + dashboard update | Audit for residual `as_int().unwrap()` in arithmetic contexts, clippy/fmt clean, roadmap dashboard updated, optional `v0.2.0-m2-bigint` tag. |

### Phase 2 coordination

- M2 work happens on `feature/phase-2-bigint` (or `m2-bigint`), branched off `develop` at a Phase-2-stable point.
- M2 only touches `Value`, `HeapObject`, int arithmetic, hash, the constant pool, and lexer/compiler int paths. Phase 2 (classes, dunders, closures, generators, exceptions) is a disjoint surface.
- Merge order: when M2 is complete, fast-forward into `develop`; any in-flight Phase 2 work rebases. Conflicts limited to `HeapObject` enum additions (mechanical merge) and possibly the `Value` impl block.
- The spec's M0-gates-M2 sequencing is honored — M2 commit 1 doesn't land until Phase 2 is complete on `develop`.

### Invariant across all commits

After each commit, `cargo test` and every Python script in `tests/` pass green. Each commit is a safe rollback point.

## Risks

| Risk | Mitigation |
|---|---|
| Commit 1's `PartialEq` removal breaks call sites we haven't found | `grep '== Value\|Value::.* == ' src/` audit included in commit 1; tests catch the rest |
| Commit 3 is large enough to be unreviewable in one diff | Split per opcode family (3a–3e) if needed; each sub-commit still test-green |
| `tests/target_phase2.py` has integer expectations that worked by i64 accident | Re-verify and update after commit 3 |
| `iter_u64_digits` API changes between `num-bigint` versions | Pin `num-bigint = "0.4"` in `Cargo.toml`; revisit only on deliberate upgrade |
| BigInt clone in `ConstValue::BigInt(Box<BigInt>)` if a code object is materialized multiple times | Materialization is one-shot per code object per VM run; multiple VM instances are out of scope for now |
| Hash algorithm mismatch with CPython on edge cases we didn't anticipate | Pinned-value tests catch any drift; once `Lib/test/test_hash.py` runs under M3+, that's the final authority |

## Open questions (deferred)

- Whether `LOAD_CONST` should split into typed variants (`LOAD_CONST_INT`, `LOAD_CONST_FLOAT`, ...) for icache locality — deferred to optimization passes once benchmarks exist.
- Whether to intern BigInt constants across code objects with weak `Rc`-based sharing — out of M2 scope; current model is one allocation per literal per code object.
- Whether to add a global "small int cache" (CPython has `[-5, 256]` interned) — irrelevant under NaN-boxing; small ints aren't heap objects.
- `__hash__` for user-defined classes — covered by Phase 2's dunder dispatch work, not M2.

## Immediate next step

Implementation plan via `writing-plans` skill, then begin commit 1 once M0 (Phase 2) is complete on `develop`.
