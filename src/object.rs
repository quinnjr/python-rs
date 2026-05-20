//! NaN-boxed value representation and heap objects.
//!
//! Layout: every Value is a u64. IEEE 754 doubles are stored as-is.
//! Non-float values use the NaN space: when bits match the tag pattern
//! `(bits & 0x7FFC_0000_0000_0000) == 0x7FFC_0000_0000_0000`, the value
//! is tagged. The tag is encoded in sign bit + bits 49:48 (3 bits total).
//!
//! Tags:
//!   0 = Int(i48)       — sign-extended 48-bit integer
//!   1 = Bool           — payload 0 or 1
//!   2 = None           — singleton
//!   3 = Str(heap idx)  — index into heap
//!   4 = List(heap idx)
//!   5 = Function(heap idx)
//!   6 = RangeIter(heap idx)
//!   7 = Object(heap idx) — all other heap types (dict, tuple, class, instance, etc.)

use std::collections::HashMap;
use std::fmt;

use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_traits::{Signed, ToPrimitive};

/// Quiet NaN with tag bits set — base for all tagged values.
const QNAN: u64 = 0x7FFC_0000_0000_0000;
/// Mask for the 48-bit payload.
const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// Mask for tag bits 49:48 (within the NaN space).
const TAG_BITS_MASK: u64 = 0x0003_0000_0000_0000;

/// Tag values (3 bits: sign + bits 49:48).
const TAG_INT: u64 = 0;       // sign=0, bits=00
const TAG_BOOL: u64 = 1;      // sign=0, bits=01
const TAG_NONE: u64 = 2;      // sign=0, bits=10
const TAG_STR: u64 = 3;       // sign=0, bits=11
const TAG_LIST: u64 = 4;      // sign=1, bits=00
const TAG_FUNC: u64 = 5;      // sign=1, bits=01
const TAG_RANGE: u64 = 6;     // sign=1, bits=10
const TAG_OBJECT: u64 = 7;    // sign=1, bits=11 — generalized object tag

/// A NaN-boxed Python value — 8 bytes, Copy.
///
/// `PartialEq` is intentionally not derived: bit equality is not Python
/// equality. Use `bits_eq` for raw bit comparison or `py_eq` (or the
/// `values_equal` helper in vm.rs, which layers exception-subtype
/// semantics on top) for Python `==`.
#[derive(Clone, Copy)]
pub struct Value(u64);

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Each branch's type predicate guarantees the matching accessor
        // returns Some; pattern-match on the Option so we never unwrap.
        if let Some(v) = self.as_float() {
            write!(f, "Value(float={v})")
        } else if let Some(v) = self.as_int() {
            write!(f, "Value(int={v})")
        } else if let Some(v) = self.as_bool() {
            write!(f, "Value(bool={v})")
        } else if self.is_none() {
            write!(f, "Value(None)")
        } else {
            write!(f, "Value(tagged=0x{:016X})", self.0)
        }
    }
}

impl Value {
    /// Create a float value.
    pub fn float(v: f64) -> Self {
        let bits = v.to_bits();
        if (bits & QNAN) == QNAN {
            Self(0x7FF8_0000_0000_0000)
        } else {
            Self(bits)
        }
    }

    /// Construct a small int directly. Caller is responsible for keeping
    /// `v` in the i48 range; values outside `[-2^47, 2^47)` will silently
    /// wrap. Use `Value::from_i64` if overflow is possible.
    pub fn small_int_unchecked(v: i64) -> Self {
        let payload = (v as u64) & PAYLOAD_MASK;
        Self(make_tagged(TAG_INT, payload))
    }

    /// Construct an int from i64. Stays a small int (TAG_INT, no
    /// allocation) when the value fits in i48; promotes to a heap
    /// BigInt otherwise. This is the canonical "I have an i64, give
    /// me the right Value" entry point.
    pub fn from_i64(v: i64, heap: &mut Vec<HeapObject>) -> Self {
        if fits_in_i48(v) {
            Self::small_int_unchecked(v)
        } else {
            let idx = heap.len();
            heap.push(HeapObject::BigInt(BigInt::from(v)));
            Self::object_ref(idx)
        }
    }

    /// Construct an int from a BigInt. Demotes to a small int when the
    /// magnitude fits in i48; otherwise allocates a HeapObject::BigInt.
    pub fn from_bigint(v: BigInt, heap: &mut Vec<HeapObject>) -> Self {
        if let Some(i) = bigint_to_i48(&v) {
            Self::small_int_unchecked(i)
        } else {
            let idx = heap.len();
            heap.push(HeapObject::BigInt(v));
            Self::object_ref(idx)
        }
    }

    /// Create a boolean value.
    pub fn bool_val(v: bool) -> Self {
        Self(make_tagged(TAG_BOOL, v as u64))
    }

    /// Create the None singleton.
    pub fn none() -> Self {
        Self(make_tagged(TAG_NONE, 0))
    }

    /// Create a string reference (heap index).
    pub fn str_ref(heap_idx: usize) -> Self {
        Self(make_tagged(TAG_STR, heap_idx as u64))
    }

    /// Create a list reference (heap index).
    pub fn list_ref(heap_idx: usize) -> Self {
        Self(make_tagged(TAG_LIST, heap_idx as u64))
    }

    /// Create a function reference (heap index).
    pub fn func_ref(heap_idx: usize) -> Self {
        Self(make_tagged(TAG_FUNC, heap_idx as u64))
    }

    /// Create a range iterator reference (heap index).
    pub fn range_ref(heap_idx: usize) -> Self {
        Self(make_tagged(TAG_RANGE, heap_idx as u64))
    }

    /// Create an object reference (heap index) — tag 7, covers all other heap types.
    pub fn object_ref(heap_idx: usize) -> Self {
        Self(make_tagged(TAG_OBJECT, heap_idx as u64))
    }

    /// Backward-compat alias for object_ref.
    #[allow(dead_code)]
    pub fn builtin_ref(heap_idx: usize) -> Self {
        Self::object_ref(heap_idx)
    }

    /// Check if this is a float (not a tagged NaN value).
    pub fn is_float(&self) -> bool {
        (self.0 & QNAN) != QNAN
    }

    /// Check if this is a tagged value.
    fn is_tagged(&self) -> bool {
        (self.0 & QNAN) == QNAN
    }

    /// Extract the 3-bit tag from a tagged value.
    fn tag(&self) -> u64 {
        debug_assert!(self.is_tagged());
        let sign_bit = (self.0 >> 63) << 2;
        let mid = (self.0 & TAG_BITS_MASK) >> 48;
        sign_bit | mid
    }

    /// Check if this value has the given tag.
    fn has_tag(&self, t: u64) -> bool {
        self.is_tagged() && self.tag() == t
    }

    /// Extract the 48-bit payload.
    fn payload(&self) -> u64 {
        self.0 & PAYLOAD_MASK
    }

    pub fn is_int(&self) -> bool { self.has_tag(TAG_INT) }
    pub fn is_bool(&self) -> bool { self.has_tag(TAG_BOOL) }
    pub fn is_none(&self) -> bool { self.has_tag(TAG_NONE) }
    pub fn is_str(&self) -> bool { self.has_tag(TAG_STR) }
    pub fn is_list(&self) -> bool { self.has_tag(TAG_LIST) }
    pub fn is_func(&self) -> bool { self.has_tag(TAG_FUNC) }
    pub fn is_range(&self) -> bool { self.has_tag(TAG_RANGE) }
    pub fn is_object(&self) -> bool { self.has_tag(TAG_OBJECT) }
    /// Backward-compat alias.
    #[allow(dead_code)]
    pub fn is_builtin(&self) -> bool { self.is_object() }

    pub fn as_float(&self) -> Option<f64> {
        if self.is_float() { Some(f64::from_bits(self.0)) } else { None }
    }

    pub fn as_int(&self) -> Option<i64> {
        if self.is_int() {
            let raw = self.payload();
            let shifted = (raw as i64) << 16;
            Some(shifted >> 16)
        } else {
            None
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if self.is_bool() { Some(self.payload() != 0) } else { None }
    }

    pub fn as_str_ref(&self) -> Option<usize> {
        if self.is_str() { Some(self.payload() as usize) } else { None }
    }

    pub fn as_list_ref(&self) -> Option<usize> {
        if self.is_list() { Some(self.payload() as usize) } else { None }
    }

    pub fn as_func_ref(&self) -> Option<usize> {
        if self.is_func() { Some(self.payload() as usize) } else { None }
    }

    pub fn as_range_ref(&self) -> Option<usize> {
        if self.is_range() { Some(self.payload() as usize) } else { None }
    }

    pub fn as_object_ref(&self) -> Option<usize> {
        if self.is_object() { Some(self.payload() as usize) } else { None }
    }

    /// Backward-compat alias.
    #[allow(dead_code)]
    pub fn as_builtin_ref(&self) -> Option<usize> {
        self.as_object_ref()
    }

    /// Get the raw bits (for id() builtin).
    pub fn display_bits(self) -> u64 {
        self.0
    }

    /// Bit-level equality — true iff the two Values have identical u64
    /// representations. Replaces the dropped `PartialEq` derive for the
    /// (rare) cases where bit equality is actually what's wanted.
    pub fn bits_eq(self, other: Value) -> bool {
        self.0 == other.0
    }

    /// True iff this Value represents a Python int (small or big).
    /// Returns true for TAG_INT and for TAG_OBJECT pointing at a
    /// `HeapObject::BigInt`. Returns false for bool (bool is its own
    /// tag; callers that want bool widening go through PyInt's
    /// `from_value_or_bool`).
    pub fn is_pyint(&self, heap: &[HeapObject]) -> bool {
        if self.is_int() { return true; }
        if let Some(idx) = self.as_object_ref()
            && matches!(heap[idx], HeapObject::BigInt(_)) {
            return true;
        }
        false
    }

    /// Python value equality. Handles cross-representation int equality
    /// (small `7` vs heap `BigInt::from(7)`), int↔float coercion, bool↔int
    /// coercion (Python treats `True == 1`), and string-content equality.
    /// Does NOT layer exception subtype semantics — vm.rs's `values_equal`
    /// adds that on top for except-handler matching.
    pub fn py_eq(self, other: Value, heap: &[HeapObject]) -> bool {
        if self.bits_eq(other) { return true; }

        // None — short-circuit; None equals only None.
        if self.is_none() || other.is_none() {
            return self.is_none() && other.is_none();
        }

        // Numeric (int, bool, BigInt) ↔ float coercion. If either side is a
        // float, coerce both to f64 and compare. Loses precision for huge
        // BigInts (collapses to inf), matching CPython's documented behavior.
        if self.is_float() || other.is_float() {
            if let (Some(a), Some(b)) = (value_to_f64(self, heap), value_to_f64(other, heap)) {
                return a == b;
            }
            return false;
        }

        // Int family: small int, big int, or bool — all compare against each
        // other as numbers. `True == 1` is required Python semantics.
        let self_intish  = self.is_pyint(heap) || self.is_bool();
        let other_intish = other.is_pyint(heap) || other.is_bool();
        if self_intish && other_intish {
            return pyint_values_eq(self, other, heap);
        }

        // String ↔ string (content).
        if let (Some(a_idx), Some(b_idx)) = (self.as_str_ref(), other.as_str_ref()) {
            let a = heap[a_idx].as_str().unwrap_or("");
            let b = heap[b_idx].as_str().unwrap_or("");
            return a == b;
        }

        false
    }

    /// Get a numeric value as f64 (works for int and float).
    pub fn to_f64(self) -> Option<f64> {
        if let Some(f) = self.as_float() {
            Some(f)
        } else {
            self.as_int().map(|i| i as f64)
        }
    }

    /// Python truthiness (basic — doesn't check __bool__/__len__).
    pub fn is_truthy(&self) -> bool {
        if let Some(b) = self.as_bool() {
            b
        } else if let Some(i) = self.as_int() {
            i != 0
        } else if let Some(f) = self.as_float() {
            f != 0.0
        } else {
            !self.is_none()
        }
    }

    /// Display this value using the heap for string/list lookup.
    pub fn display(&self, heap: &[HeapObject]) -> String {
        if let Some(f) = self.as_float() {
            format_float(f)
        } else if let Some(i) = self.as_int() {
            i.to_string()
        } else if let Some(b) = self.as_bool() {
            if b { "True".to_string() } else { "False".to_string() }
        } else if self.is_none() {
            "None".to_string()
        } else if let Some(idx) = self.as_str_ref() {
            heap[idx].as_str().unwrap_or("???").to_string()
        } else if let Some(idx) = self.as_list_ref() {
            if let HeapObject::List(items) = &heap[idx] {
                let parts: Vec<String> = items.iter().map(|v| v.repr(heap)).collect();
                format!("[{}]", parts.join(", "))
            } else {
                "[???]".to_string()
            }
        } else if let Some(idx) = self.as_func_ref() {
            if let HeapObject::Function { name, .. } = &heap[idx] {
                format!("<function {name}>")
            } else {
                "<function>".to_string()
            }
        } else if let Some(idx) = self.as_object_ref() {
            display_object(idx, heap)
        } else {
            format!("<object 0x{:016X}>", self.0)
        }
    }

    /// Python repr (strings get quotes).
    pub fn repr(&self, heap: &[HeapObject]) -> String {
        if let Some(idx) = self.as_str_ref() {
            let s = heap[idx].as_str().unwrap_or("???");
            format!("'{s}'")
        } else {
            self.display(heap)
        }
    }
}

fn display_object(idx: usize, heap: &[HeapObject]) -> String {
    match &heap[idx] {
        HeapObject::BuiltinFn { name, .. } => format!("<built-in function {name}>"),
        HeapObject::Tuple(items) => {
            let parts: Vec<String> = items.iter().map(|v| v.repr(heap)).collect();
            if items.len() == 1 {
                format!("({},)", parts[0])
            } else {
                format!("({})", parts.join(", "))
            }
        }
        HeapObject::Dict { keys, values, .. } => {
            let parts: Vec<String> = keys.iter().zip(values.iter())
                .map(|(k, v)| format!("{}: {}", k.repr(heap), v.repr(heap)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        HeapObject::Class { name, .. } => format!("<class '{name}'>"),
        HeapObject::Instance { class_idx, .. } => {
            if let HeapObject::Class { name, .. } = &heap[*class_idx] {
                format!("<{name} instance>")
            } else {
                "<instance>".to_string()
            }
        }
        HeapObject::BoundMethod { .. } => "<bound method>".to_string(),
        HeapObject::Generator { .. } => "<generator object>".to_string(),
        HeapObject::Cell(v) => format!("<cell: {}>", v.display(heap)),
        HeapObject::Closure { name, .. } => format!("<function {name}>"),
        HeapObject::ExceptionObj { exc_type, message, .. } => {
            format!("{exc_type:?}({message})")
        }
        HeapObject::BigInt(b) => b.to_string(),
        HeapObject::Module { name, file, .. } => match file {
            Some(path) => format!("<module '{name}' from '{path}'>"),
            None       => format!("<module '{name}' (built-in)>"),
        },
        HeapObject::ListIter { .. } => "<list_iterator>".to_string(),
        HeapObject::Set(items) => {
            if items.is_empty() {
                "set()".to_string()
            } else {
                let parts: Vec<String> = items.iter().map(|v| v.repr(heap)).collect();
                format!("{{{}}}", parts.join(", "))
            }
        }
        _ => format!("<object@{idx}>"),
    }
}

/// Format a float like Python does.
fn format_float(f: f64) -> String {
    if f.is_infinite() {
        if f > 0.0 { "inf".to_string() } else { "-inf".to_string() }
    } else if f.is_nan() {
        "nan".to_string()
    } else if f == f.trunc() && f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// Inclusive lower bound of the i48 small-int range.
const I48_MIN: i64 = -(1 << 47);
/// Exclusive upper bound of the i48 small-int range.
const I48_MAX_PLUS_ONE: i64 = 1 << 47;

/// True iff `v` fits in the sign-extended i48 small-int range.
#[inline]
pub fn fits_in_i48(v: i64) -> bool {
    (I48_MIN..I48_MAX_PLUS_ONE).contains(&v)
}

/// If `v` fits in i48, return it as i64; otherwise None.
pub fn bigint_to_i48(v: &BigInt) -> Option<i64> {
    let i = i64::try_from(v).ok()?;
    if fits_in_i48(i) { Some(i) } else { None }
}

/// Cross-representation int equality: handles small↔small (covered by
/// bits already), small↔big, big↔small, big↔big. Bool widens to int 0/1.
fn pyint_values_eq(a: Value, b: Value, heap: &[HeapObject]) -> bool {
    let av = pyint_as_bigint_or_i64(a, heap);
    let bv = pyint_as_bigint_or_i64(b, heap);
    match (av, bv) {
        (Some(Either3::Small(x)), Some(Either3::Small(y))) => x == y,
        (Some(Either3::Big(x)),   Some(Either3::Big(y)))   => x == y,
        (Some(Either3::Small(x)), Some(Either3::Big(y)))
            | (Some(Either3::Big(y)),  Some(Either3::Small(x))) => &BigInt::from(x) == y,
        _ => false,
    }
}

enum Either3<'a> {
    Small(i64),
    Big(&'a BigInt),
}

fn pyint_as_bigint_or_i64<'a>(v: Value, heap: &'a [HeapObject]) -> Option<Either3<'a>> {
    if let Some(i) = v.as_int() {
        return Some(Either3::Small(i));
    }
    if let Some(b) = v.as_bool() {
        return Some(Either3::Small(b as i64));
    }
    if let Some(idx) = v.as_object_ref()
        && let HeapObject::BigInt(b) = &heap[idx] {
        return Some(Either3::Big(b));
    }
    None
}

/// f64 view of a numeric value (int, bool, BigInt, or float). Used by `py_eq`.
/// f64 view of any numeric Value (int, bool, BigInt, or float). The single
/// chokepoint for "give me an f64 for this thing" — int↔float coercion in
/// arithmetic, hashing, and comparison all go through this.
pub fn value_to_f64(v: Value, heap: &[HeapObject]) -> Option<f64> {
    if let Some(f) = v.as_float() { return Some(f); }
    if let Some(i) = v.as_int() { return Some(i as f64); }
    if let Some(b) = v.as_bool() { return Some(if b { 1.0 } else { 0.0 }); }
    if let Some(idx) = v.as_object_ref()
        && let HeapObject::BigInt(big) = &heap[idx] {
        return bigint_to_f64(big);
    }
    None
}

/// Convert a BigInt to f64. For huge magnitudes this returns `±inf` (Python's
/// documented behavior for unrepresentable ints in float context). For ints
/// outside f64's exact range it loses precision, consistent with CPython.
fn bigint_to_f64(b: &BigInt) -> Option<f64> {
    Some(b.to_f64().unwrap_or_else(|| {
        if b.sign() == Sign::Minus { f64::NEG_INFINITY } else { f64::INFINITY }
    }))
}

// =====================================================================
// PyInt — unified small/big int arithmetic surface (M2 commit 2)
// =====================================================================

/// Borrowed view of a Python int — small (i64, always within i48) or big
/// (heap BigInt). The single chokepoint for every arithmetic operation
/// involving ints. Constructed from a Value via `from_value`; arithmetic
/// methods return `PyIntOwned`, which the caller converts back into a
/// Value via `PyIntOwned::into_value`.
#[derive(Debug, Clone, Copy)]
pub enum PyInt<'a> {
    Small(i64),
    Big(&'a BigInt),
}

/// Owned form returned by arithmetic. Invariant: `Small(i)` always holds
/// a value in the i48 range — `demote()` enforces this on every result.
#[derive(Debug, Clone)]
pub enum PyIntOwned {
    Small(i64),
    Big(BigInt),
}

/// `pow` result: integer base raised to integer exp can be either int
/// (non-negative exp) or float (negative exp, e.g., `2 ** -3 == 0.125`).
#[derive(Debug)]
pub enum PyPowResult {
    Int(PyIntOwned),
    Float(f64),
}

/// Arithmetic errors at the int-op level. Each maps to a specific
/// Python exception at the VM boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithError {
    DivByZero,
    NegativeShift,
    /// pow_mod with negative exponent and no modulus. Not yet
    /// constructible from any opcode; reserved for the three-arg
    /// pow path that lands when the VM gets a fused opcode for it.
    #[allow(dead_code)]
    NegativePower,
}

impl<'a> PyInt<'a> {
    /// Construct from a Value. Returns None if `v` is not an int.
    /// Does NOT treat bool as int — use `from_value_or_bool` for that.
    pub fn from_value(v: Value, heap: &'a [HeapObject]) -> Option<Self> {
        if let Some(i) = v.as_int() { return Some(PyInt::Small(i)); }
        if let Some(idx) = v.as_object_ref()
            && let HeapObject::BigInt(b) = &heap[idx] {
            return Some(PyInt::Big(b));
        }
        None
    }

    /// Construct from a Value, widening bool to int (True→1, False→0).
    /// Use at arithmetic call sites — Python treats `True + 1 == 2`.
    pub fn from_value_or_bool(v: Value, heap: &'a [HeapObject]) -> Option<Self> {
        if let Some(b) = v.as_bool() { return Some(PyInt::Small(b as i64)); }
        Self::from_value(v, heap)
    }

    /// Allocate an owned BigInt from this view. Cheap clone for Big;
    /// constructs from i64 for Small.
    fn to_owned_bigint(self) -> BigInt {
        match self {
            PyInt::Small(i) => BigInt::from(i),
            PyInt::Big(b) => b.clone(),
        }
    }

    /// f64 view. Loses precision for ints beyond f64's exact range; for
    /// magnitudes beyond f64's exponent range, collapses to ±inf, matching
    /// CPython's documented `float(huge_int)` behavior.
    pub fn to_f64(self) -> f64 {
        match self {
            PyInt::Small(i) => i as f64,
            PyInt::Big(b) => bigint_to_f64(b).unwrap_or(f64::INFINITY),
        }
    }

    pub fn add(self, other: Self) -> PyIntOwned {
        if let (PyInt::Small(a), PyInt::Small(b)) = (self, other)
            && let Some(r) = a.checked_add(b)
        {
            return PyIntOwned::Small(r).demote();
        }
        PyIntOwned::Big(self.to_owned_bigint() + other.to_owned_bigint()).demote()
    }

    pub fn sub(self, other: Self) -> PyIntOwned {
        if let (PyInt::Small(a), PyInt::Small(b)) = (self, other)
            && let Some(r) = a.checked_sub(b)
        {
            return PyIntOwned::Small(r).demote();
        }
        PyIntOwned::Big(self.to_owned_bigint() - other.to_owned_bigint()).demote()
    }

    pub fn mul(self, other: Self) -> PyIntOwned {
        if let (PyInt::Small(a), PyInt::Small(b)) = (self, other)
            && let Some(r) = a.checked_mul(b)
        {
            return PyIntOwned::Small(r).demote();
        }
        PyIntOwned::Big(self.to_owned_bigint() * other.to_owned_bigint()).demote()
    }

    pub fn floordiv(self, other: Self) -> Result<PyIntOwned, ArithError> {
        check_nonzero(other)?;
        // Avoid i64::MIN / -1 overflow by falling through to BigInt.
        if let (PyInt::Small(a), PyInt::Small(b)) = (self, other)
            && !(a == i64::MIN && b == -1)
        {
            return Ok(PyIntOwned::Small(floor_div_i64(a, b)).demote());
        }
        let a = self.to_owned_bigint();
        let b = other.to_owned_bigint();
        Ok(PyIntOwned::Big(a.div_floor(&b)).demote())
    }

    pub fn mod_(self, other: Self) -> Result<PyIntOwned, ArithError> {
        check_nonzero(other)?;
        if let (PyInt::Small(a), PyInt::Small(b)) = (self, other)
            && !(a == i64::MIN && b == -1)
        {
            return Ok(PyIntOwned::Small(floor_mod_i64(a, b)).demote());
        }
        let a = self.to_owned_bigint();
        let b = other.to_owned_bigint();
        Ok(PyIntOwned::Big(a.mod_floor(&b)).demote())
    }

    pub fn divmod(self, other: Self) -> Result<(PyIntOwned, PyIntOwned), ArithError> {
        check_nonzero(other)?;
        if let (PyInt::Small(a), PyInt::Small(b)) = (self, other)
            && !(a == i64::MIN && b == -1)
        {
            let q = floor_div_i64(a, b);
            let r = floor_mod_i64(a, b);
            return Ok((PyIntOwned::Small(q).demote(), PyIntOwned::Small(r).demote()));
        }
        let a = self.to_owned_bigint();
        let b = other.to_owned_bigint();
        let (q, r) = a.div_mod_floor(&b);
        Ok((PyIntOwned::Big(q).demote(), PyIntOwned::Big(r).demote()))
    }

    /// `pow(self, exp)` — negative exp returns float; otherwise int.
    /// For huge exponents this can be slow; that's an inherent cost of
    /// arbitrary-precision arithmetic.
    pub fn pow(self, exp: Self) -> PyPowResult {
        // Negative exponent → float.
        if matches!(exp, PyInt::Small(e) if e < 0)
            || matches!(exp, PyInt::Big(b) if b.sign() == Sign::Minus)
        {
            let base_f = self.to_f64();
            let exp_f  = exp.to_f64();
            return PyPowResult::Float(base_f.powf(exp_f));
        }

        // Non-negative integer exponent. Extract as u32 if it fits; otherwise
        // the operation is effectively infeasible (would produce a >4-billion-
        // bit result), so we fall back to f64 powf.
        let exp_u32 = match exp {
            PyInt::Small(e) => u32::try_from(e).ok(),
            PyInt::Big(b)   => u32::try_from(b).ok(),
        };
        match exp_u32 {
            Some(e) => {
                let base = self.to_owned_bigint();
                PyPowResult::Int(PyIntOwned::Big(base.pow(e)).demote())
            }
            None => {
                // Exponent too large for any plausible computation; defer to f64.
                PyPowResult::Float(self.to_f64().powf(exp.to_f64()))
            }
        }
    }

    /// Three-arg pow: `pow(self, exp, modulus)`. Negative `exp` requires a
    /// modular inverse which is out of M2 scope — we error on it.
    /// Not yet wired into any opcode; reserved for the three-arg pow path.
    #[allow(dead_code)]
    pub fn pow_mod(self, exp: Self, modulus: Self) -> Result<PyIntOwned, ArithError> {
        check_nonzero(modulus)?;
        if matches!(exp, PyInt::Small(e) if e < 0) {
            return Err(ArithError::NegativePower);
        }
        if let PyInt::Big(b) = exp
            && b.sign() == Sign::Minus { return Err(ArithError::NegativePower); }

        let base = self.to_owned_bigint();
        let e = exp.to_owned_bigint();
        let m = modulus.to_owned_bigint();
        Ok(PyIntOwned::Big(base.modpow(&e, &m)).demote())
    }

    pub fn neg(self) -> PyIntOwned {
        match self {
            PyInt::Small(i) => {
                // i64::MIN.checked_neg() returns None; spills to BigInt.
                match i.checked_neg() {
                    Some(r) => PyIntOwned::Small(r).demote(),
                    None    => PyIntOwned::Big(-BigInt::from(i)).demote(),
                }
            }
            PyInt::Big(b) => PyIntOwned::Big(-b).demote(),
        }
    }

    pub fn abs(self) -> PyIntOwned {
        match self {
            PyInt::Small(i) => match i.checked_abs() {
                Some(r) => PyIntOwned::Small(r).demote(),
                None    => PyIntOwned::Big(BigInt::from(i).abs()).demote(),
            },
            PyInt::Big(b) => PyIntOwned::Big(b.abs()).demote(),
        }
    }

    pub fn and_(self, other: Self) -> PyIntOwned {
        if let (PyInt::Small(a), PyInt::Small(b)) = (self, other) {
            return PyIntOwned::Small(a & b).demote();
        }
        PyIntOwned::Big(self.to_owned_bigint() & other.to_owned_bigint()).demote()
    }

    pub fn or_(self, other: Self) -> PyIntOwned {
        if let (PyInt::Small(a), PyInt::Small(b)) = (self, other) {
            return PyIntOwned::Small(a | b).demote();
        }
        PyIntOwned::Big(self.to_owned_bigint() | other.to_owned_bigint()).demote()
    }

    pub fn xor_(self, other: Self) -> PyIntOwned {
        if let (PyInt::Small(a), PyInt::Small(b)) = (self, other) {
            return PyIntOwned::Small(a ^ b).demote();
        }
        PyIntOwned::Big(self.to_owned_bigint() ^ other.to_owned_bigint()).demote()
    }

    pub fn invert(self) -> PyIntOwned {
        // ~x == -x - 1 in Python's two's-complement int model.
        match self {
            PyInt::Small(i) => PyIntOwned::Small(!i).demote(),
            PyInt::Big(b)   => PyIntOwned::Big(!b.clone()).demote(),
        }
    }

    pub fn shl(self, other: Self) -> Result<PyIntOwned, ArithError> {
        let shift = pyint_to_shift_amount(other)?;
        // i128 fast path: avoids arithmetic-shift round-trip issues with
        // negative i64 values near the sign bit, and demote() will catch
        // anything that exceeds i48 and bounce it to BigInt.
        if let PyInt::Small(a) = self
            && shift <= 63
        {
            let r: i128 = (a as i128) << shift;
            if let Ok(small) = i64::try_from(r) {
                return Ok(PyIntOwned::Small(small).demote());
            }
        }
        Ok(PyIntOwned::Big(self.to_owned_bigint() << shift).demote())
    }

    pub fn shr(self, other: Self) -> Result<PyIntOwned, ArithError> {
        let shift = pyint_to_shift_amount(other)?;
        if let PyInt::Small(a) = self {
            // Python int shr is arithmetic (sign-extending). For shifts >= 64,
            // the result is 0 for non-negative, -1 for negative.
            let s = shift.min(63) as u32;
            return Ok(PyIntOwned::Small(a >> s).demote());
        }
        Ok(PyIntOwned::Big(self.to_owned_bigint() >> shift).demote())
    }

    pub fn cmp(self, other: Self) -> std::cmp::Ordering {
        match (self, other) {
            (PyInt::Small(a), PyInt::Small(b)) => a.cmp(&b),
            (PyInt::Big(a), PyInt::Big(b))     => a.cmp(b),
            (PyInt::Small(a), PyInt::Big(b))   => BigInt::from(a).cmp(b),
            (PyInt::Big(a), PyInt::Small(b))   => a.cmp(&BigInt::from(b)),
        }
    }

    /// Convenience int-vs-int equality. Most call sites use
    /// `Value::py_eq` directly because they don't pre-convert to PyInt;
    /// this is here for completeness and for future builtins like
    /// `operator.eq`.
    #[allow(dead_code)]
    pub fn eq(self, other: Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }

    /// CPython-compatible int hash. Returns i64 so the `-2` sentinel
    /// substitution is expressible. Optimized Mersenne reduction —
    /// allocation-free, division-free.
    #[inline]
    pub fn hash(&self) -> i64 {
        match self {
            PyInt::Small(i) => hash_small_i64(*i),
            PyInt::Big(b)   => hash_bigint(b),
        }
    }
}

impl PyIntOwned {
    /// Upholds the demote invariant: small magnitudes always Small.
    pub fn demote(self) -> Self {
        match self {
            PyIntOwned::Small(i) if !fits_in_i48(i) => PyIntOwned::Big(BigInt::from(i)),
            PyIntOwned::Big(b) => match bigint_to_i48(&b) {
                Some(i) => PyIntOwned::Small(i),
                None    => PyIntOwned::Big(b),
            },
            small => small,
        }
    }

    /// Convert back into a Value, allocating a HeapObject::BigInt if the
    /// result didn't fit in i48. Always upholds the demote invariant.
    pub fn into_value(self, heap: &mut Vec<HeapObject>) -> Value {
        match self.demote() {
            PyIntOwned::Small(i) => Value::small_int_unchecked(i),
            PyIntOwned::Big(b) => {
                let idx = heap.len();
                heap.push(HeapObject::BigInt(b));
                Value::object_ref(idx)
            }
        }
    }

}

// ---------- shared helpers ----------

/// Returns `DivByZero` if `d` is zero. Shared between division-like ops.
#[inline]
fn check_nonzero(d: PyInt<'_>) -> Result<(), ArithError> {
    match d {
        PyInt::Small(0) => Err(ArithError::DivByZero),
        PyInt::Big(b) if b.sign() == Sign::NoSign => Err(ArithError::DivByZero),
        _ => Ok(()),
    }
}

#[inline]
fn floor_div_i64(a: i64, b: i64) -> i64 {
    let q = a / b;
    let r = a % b;
    if (r != 0) && ((r < 0) != (b < 0)) { q - 1 } else { q }
}

#[inline]
fn floor_mod_i64(a: i64, b: i64) -> i64 {
    let r = a % b;
    if (r != 0) && ((r < 0) != (b < 0)) { r + b } else { r }
}

fn pyint_to_shift_amount(p: PyInt<'_>) -> Result<usize, ArithError> {
    match p {
        PyInt::Small(s) => {
            if s < 0 { Err(ArithError::NegativeShift) }
            else { Ok(s as usize) }
        }
        PyInt::Big(b) => {
            if b.sign() == Sign::Minus { return Err(ArithError::NegativeShift); }
            usize::try_from(b).map_err(|_| ArithError::NegativeShift)
        }
    }
}

/// Truediv lives outside PyInt because it always returns float.
/// Zero is checked on the int representation first so a non-zero BigInt
/// that underflows to 0.0 doesn't get reported as division-by-zero.
pub fn pyint_truediv(a: PyInt<'_>, b: PyInt<'_>) -> Result<f64, ArithError> {
    check_nonzero(b)?;
    let bf = b.to_f64();
    // Defensive: even after check_nonzero, a non-zero BigInt can in
    // principle underflow to 0.0 in f64. Keep this guard.
    if bf == 0.0 { return Err(ArithError::DivByZero); }
    Ok(a.to_f64() / bf)
}

// ---------- Mersenne hash (allocation-free, division-free) ----------

const PYHASH_BITS:    u32 = 61;
const PYHASH_MODULUS: u64 = (1u64 << PYHASH_BITS) - 1; // 2^61 - 1

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

#[inline(always)]
fn hash_small_i64(i: i64) -> i64 {
    let abs    = i.unsigned_abs();
    let h      = mod_mersenne_u64(abs) as i64;
    let signed = if i < 0 { -h } else { h };
    signed - ((signed == -1) as i64) // -1 → -2, branchless
}

#[inline(never)]
#[cold]
fn hash_bigint(b: &BigInt) -> i64 {
    let mut h:    u64 = 0;
    let mut pow8: u64 = 1; // 8^i mod P
    for limb in b.iter_u64_digits() {
        let limb_mod = mod_mersenne_u64(limb);
        let term     = mod_mersenne_u128(limb_mod as u128 * pow8 as u128);
        h            = mod_mersenne_u64(h + term);
        pow8         = mod_mersenne_u64(pow8 << 3);
    }
    let signed = if b.sign() == Sign::Minus { -(h as i64) } else { h as i64 };
    signed - ((signed == -1) as i64)
}

/// Construct a tagged NaN-boxed value from a 3-bit tag and 48-bit payload.
fn make_tagged(tag: u64, payload: u64) -> u64 {
    let sign = ((tag >> 2) & 1) << 63;
    let mid = (tag & 0b011) << 48;
    QNAN | sign | mid | (payload & PAYLOAD_MASK)
}

/// Heap-allocated Python objects.
#[derive(Debug, Clone)]
pub enum HeapObject {
    /// Immutable string.
    Str(Box<str>),
    /// Mutable list.
    List(Vec<Value>),
    /// User-defined function.
    Function {
        name: String,
        code_index: usize,
        arity: u8,
        /// Defining module's heap index — globals lookups inside the
        /// function body route through this module. None for functions
        /// defined in the top-level main script (use VM.globals).
        module_idx: Option<usize>,
    },
    /// Range iterator state.
    RangeIter {
        current: i64,
        stop: i64,
        step: i64,
    },
    /// Built-in function.
    BuiltinFn {
        name: String,
        id: BuiltinId,
    },
    /// Tuple (immutable sequence).
    Tuple(Vec<Value>),
    /// Dictionary.
    Dict {
        keys: Vec<Value>,
        values: Vec<Value>,
        index_map: HashMap<u64, usize>,
    },
    /// Set.
    Set(Vec<Value>),
    /// Class object.
    Class {
        name: String,
        mro: Vec<usize>,
        attrs: HashMap<String, Value>,
        #[allow(dead_code)]
        bases: Vec<usize>,
    },
    /// Instance of a class.
    Instance {
        class_idx: usize,
        attrs: HashMap<String, Value>,
    },
    /// Bound method (instance + function).
    BoundMethod {
        instance: Value,
        method: Value,
    },
    /// Generator object.
    Generator {
        code_index: usize,
        ip: usize,
        locals: Vec<Value>,
        stack: Vec<Value>,
        state: GeneratorState,
        cells: Vec<usize>,
    },
    /// Cell for closures.
    Cell(Value),
    /// Closure (function + captured cells).
    Closure {
        name: String,
        code_index: usize,
        arity: u8,
        cells: Vec<usize>,
        /// Defining module's heap index — see Function::module_idx.
        module_idx: Option<usize>,
    },
    /// Exception object.
    ExceptionObj {
        exc_type: ExceptionType,
        message: String,
        args: Vec<Value>,
    },
    /// List iterator.
    ListIter {
        list_idx: usize,
        index: usize,
    },
    /// Tuple iterator.
    TupleIter {
        tuple_idx: usize,
        index: usize,
    },
    /// String iterator.
    StringIter {
        str_idx: usize,
        index: usize,
    },
    /// Dict key iterator.
    DictKeyIter {
        dict_idx: usize,
        index: usize,
    },
    /// Arbitrary-precision integer. Only present when a value has
    /// overflowed the i48 small-int range, or when a source literal
    /// exceeds i64. Reached via TAG_OBJECT.
    BigInt(BigInt),
    /// A Python module — either loaded from a .py file or built by a
    /// Rust cmodule. The single representation for both kinds.
    ///
    /// Reached via TAG_OBJECT. Inserted into VM.sys_modules under its
    /// canonical dotted name as the import system loads it.
    Module {
        /// Canonical dotted name, e.g. "foo.bar". Used as sys.modules key.
        name: String,
        /// Module namespace. Also serves as __dict__. Pre-populated with
        /// __name__, __file__, __package__, __doc__ at construction.
        globals: HashMap<String, Value>,
        /// Source path for .py-loaded modules; None for cmodules and
        /// for `__future__` / similar pseudo-modules.
        file: Option<String>,
        /// Parent package's dotted name. Kept in sync with the module's
        /// `__package__` global entry at construction time; the live
        /// relative-import resolver reads `__package__` from globals via
        /// `frame_globals_get`, but this struct field is retained for
        /// future introspection APIs (e.g., a Python-visible `module.__package__`
        /// that doesn't go through dict lookup).
        #[allow(dead_code)]
        package: Option<String>,
        /// false during module-body execution; true after the body
        /// returns. A re-entrant import (circular case) returns the
        /// partially-initialized module from cache regardless.
        initialized: bool,
        /// Lazy cache of __all__ for `from foo import *`. Populated on
        /// first star-import; None means "not yet probed."
        all: Option<Vec<String>>,
    },
}

impl HeapObject {
    /// Get as string slice if this is a Str variant.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// A Rust-implemented Python module. Each cmodule lives in its own file
/// under `src/cmodules/` and exposes itself via this trait. The import
/// machinery indexes the registry by `name()`; `build_globals()` is called
/// exactly once per VM run when the module is first imported.
pub trait CModule {
    /// Fully-qualified Python import name. M3 supports top-level only:
    /// "sys", "_io", "math". Dotted submodule cmodules are out of scope.
    fn name(&self) -> &'static str;

    /// Build the module's namespace. Called the first time the module is
    /// imported. Receives a mutable heap so the module can allocate
    /// strings, tuples, functions, etc.
    ///
    /// Implementations MUST NOT cache the `heap` reference — the heap may
    /// reallocate between calls. Build every Value during this call, drop
    /// the reference, return the populated globals.
    fn build_globals(&self, heap: &mut Vec<HeapObject>) -> HashMap<String, Value>;
}

/// Fetch a `&str` from heap at the given index. Used wherever we've
/// already verified the Value carries a str ref via `as_str_ref()` and
/// want the actual string content. Returns an internal RuntimeError if
/// the heap entry isn't a Str (which should never happen with a valid
/// str ref but we surface it as an error rather than panicking).
pub fn heap_str(heap: &[HeapObject], idx: usize) -> Result<&str, crate::error::PythonError> {
    heap.get(idx).and_then(HeapObject::as_str).ok_or_else(|| {
        crate::error::PythonError::runtime(
            format!("internal: heap index {idx} does not point to a Str"), 0,
        )
    })
}

/// Generator execution state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GeneratorState {
    Created,
    Suspended,
    Running,
    Completed,
}

/// Python exception types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionType {
    BaseException,
    Exception,
    TypeError,
    ValueError,
    NameError,
    AttributeError,
    IndexError,
    KeyError,
    ZeroDivisionError,
    StopIteration,
    RuntimeError,
    NotImplementedError,
    AssertionError,
    OverflowError,
    ImportError,
}

impl ExceptionType {
    /// Check if self is a subtype of target.
    pub fn is_subtype(self, target: Self) -> bool {
        if self == target { return true; }
        match target {
            Self::BaseException => true,
            Self::Exception => self != Self::BaseException,
            _ => false,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::BaseException => "BaseException",
            Self::Exception => "Exception",
            Self::TypeError => "TypeError",
            Self::ValueError => "ValueError",
            Self::NameError => "NameError",
            Self::AttributeError => "AttributeError",
            Self::IndexError => "IndexError",
            Self::KeyError => "KeyError",
            Self::ZeroDivisionError => "ZeroDivisionError",
            Self::StopIteration => "StopIteration",
            Self::RuntimeError => "RuntimeError",
            Self::NotImplementedError => "NotImplementedError",
            Self::AssertionError => "AssertionError",
            Self::OverflowError => "OverflowError",
            Self::ImportError => "ImportError",
        }
    }

    #[allow(dead_code)]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "BaseException" => Some(Self::BaseException),
            "Exception" => Some(Self::Exception),
            "TypeError" => Some(Self::TypeError),
            "ValueError" => Some(Self::ValueError),
            "NameError" => Some(Self::NameError),
            "AttributeError" => Some(Self::AttributeError),
            "IndexError" => Some(Self::IndexError),
            "KeyError" => Some(Self::KeyError),
            "ZeroDivisionError" => Some(Self::ZeroDivisionError),
            "StopIteration" => Some(Self::StopIteration),
            "RuntimeError" => Some(Self::RuntimeError),
            "NotImplementedError" => Some(Self::NotImplementedError),
            "AssertionError" | "AssertError" => Some(Self::AssertionError),
            "OverflowError" => Some(Self::OverflowError),
            "ImportError" | "ModuleNotFoundError" => Some(Self::ImportError),
            _ => None,
        }
    }
}

/// Identifies which built-in function to dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinId {
    Print,
    Range,
    Len,
    Type,
    Int,
    Str,
    Bool,
    Float,
    Abs,
    Divmod,
    Min,
    Max,
    Isinstance,
    Issubclass,
    Super,
    Hasattr,
    Getattr,
    Setattr,
    Id,
    Iter,
    Next,
    // List methods
    ListAppend,
    ListPop,
    ListSort,
    ListReverse,
    ListInsert,
    ListExtend,
    // String methods
    StrUpper,
    StrLower,
    StrSplit,
    StrJoin,
    StrReplace,
    StrStartswith,
    StrEndswith,
    StrFind,
    StrStrip,
    StrFormat,
    // Dict methods
    DictKeys,
    DictValues,
    DictItems,
    DictGet,
    DictPop,
    // Exception constructors
    ExcConstructor(ExceptionType),
}

/// Compute a hash for a Value, used in dict key lookup.
pub fn value_hash(v: Value, heap: &[HeapObject]) -> u64 {
    // Int / bool / BigInt — go through PyInt::hash for CPython-compatible
    // semantics (Mersenne reduction with -1 → -2 sentinel). Bool widens
    // so `hash(True) == hash(1) == 1` as Python requires.
    if let Some(pi) = PyInt::from_value_or_bool(v, heap) {
        return pi.hash() as u64;
    }
    if v.is_none() {
        return 0x_DEAD_CAFE;
    }
    if let Some(idx) = v.as_str_ref() {
        let s = heap[idx].as_str().unwrap_or("");
        let mut h: u64 = 5381;
        for b in s.bytes() {
            h = h.wrapping_mul(33).wrapping_add(b as u64);
        }
        return h;
    }
    // Identity hash for other types
    v.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_roundtrip() {
        for v in [0, 1, -1, 42, -42, 100_000, -100_000, (1 << 47) - 1, -(1 << 47)] {
            let val = Value::small_int_unchecked(v);
            assert!(val.is_int(), "expected int for {v}");
            assert_eq!(val.as_int(), Some(v), "roundtrip failed for {v}");
        }
    }

    #[test]
    fn float_roundtrip() {
        for v in [0.0, 1.5, -3.14, f64::INFINITY, f64::NEG_INFINITY] {
            let val = Value::float(v);
            assert!(val.is_float(), "expected float for {v}");
            assert_eq!(val.as_float(), Some(v));
        }
    }

    #[test]
    fn nan_becomes_canonical() {
        let val = Value::float(f64::NAN);
        assert!(val.is_float());
        assert!(val.as_float().unwrap().is_nan());
    }

    #[test]
    fn bool_roundtrip() {
        assert_eq!(Value::bool_val(true).as_bool(), Some(true));
        assert_eq!(Value::bool_val(false).as_bool(), Some(false));
        assert!(Value::bool_val(true).is_bool());
    }

    #[test]
    fn none_works() {
        let v = Value::none();
        assert!(v.is_none());
        assert!(!v.is_int());
        assert!(!v.is_float());
    }

    #[test]
    fn heap_refs() {
        let v = Value::str_ref(42);
        assert!(v.is_str());
        assert_eq!(v.as_str_ref(), Some(42));

        let v = Value::list_ref(7);
        assert!(v.is_list());
        assert_eq!(v.as_list_ref(), Some(7));

        let v = Value::func_ref(3);
        assert!(v.is_func());
        assert_eq!(v.as_func_ref(), Some(3));
    }

    #[test]
    fn truthiness() {
        assert!(Value::bool_val(true).is_truthy());
        assert!(!Value::bool_val(false).is_truthy());
        assert!(Value::small_int_unchecked(1).is_truthy());
        assert!(!Value::small_int_unchecked(0).is_truthy());
        assert!(Value::float(1.0).is_truthy());
        assert!(!Value::float(0.0).is_truthy());
        assert!(!Value::none().is_truthy());
    }

    #[test]
    fn display_values() {
        let heap = vec![HeapObject::Str("hello".into())];
        assert_eq!(Value::small_int_unchecked(42).display(&heap), "42");
        assert_eq!(Value::float(3.14).display(&heap), "3.14");
        assert_eq!(Value::bool_val(true).display(&heap), "True");
        assert_eq!(Value::none().display(&heap), "None");
        assert_eq!(Value::str_ref(0).display(&heap), "hello");
    }

    #[test]
    fn tags_dont_collide() {
        let values = vec![
            Value::small_int_unchecked(0),
            Value::bool_val(false),
            Value::none(),
            Value::str_ref(0),
            Value::list_ref(0),
            Value::func_ref(0),
            Value::range_ref(0),
            Value::object_ref(0),
        ];
        for (i, v) in values.iter().enumerate() {
            let checks = [
                v.is_int(),
                v.is_bool(),
                v.is_none(),
                v.is_str(),
                v.is_list(),
                v.is_func(),
                v.is_range(),
                v.is_object(),
            ];
            for (j, &check) in checks.iter().enumerate() {
                if i == j {
                    assert!(check, "tag {i} should match check {j}");
                } else {
                    assert!(!check, "tag {i} should NOT match check {j}");
                }
            }
        }
    }

    #[test]
    fn object_ref_backward_compat() {
        let v = Value::builtin_ref(5);
        assert!(v.is_object());
        assert!(v.is_builtin()); // alias
        assert_eq!(v.as_object_ref(), Some(5));
        assert_eq!(v.as_builtin_ref(), Some(5));
    }

    // ---------- M2 commit 1: BigInt scaffolding ----------

    #[test]
    fn fits_in_i48_boundaries() {
        assert!(fits_in_i48(0));
        assert!(fits_in_i48(1));
        assert!(fits_in_i48(-1));
        assert!(fits_in_i48((1 << 47) - 1));
        assert!(fits_in_i48(-(1 << 47)));
        assert!(!fits_in_i48(1 << 47));
        assert!(!fits_in_i48(-(1 << 47) - 1));
        assert!(!fits_in_i48(i64::MAX));
        assert!(!fits_in_i48(i64::MIN));
    }

    #[test]
    fn from_i64_stays_small_for_i48_range() {
        let mut heap = Vec::new();
        let before_len = heap.len();
        let v = Value::from_i64(42, &mut heap);
        assert!(v.is_int(), "small int should use TAG_INT");
        assert_eq!(v.as_int(), Some(42));
        assert_eq!(heap.len(), before_len, "no heap allocation for small int");
    }

    #[test]
    fn from_i64_promotes_to_bigint_for_overflow() {
        let mut heap = Vec::new();
        let v = Value::from_i64(i64::MAX, &mut heap);
        assert!(!v.is_int(), "i64::MAX does not fit in i48, must not be TAG_INT");
        assert!(v.is_object());
        assert_eq!(heap.len(), 1, "one BigInt allocated");
        assert!(matches!(heap[0], HeapObject::BigInt(_)));
        assert!(v.is_pyint(&heap));
    }

    #[test]
    fn from_bigint_demotes_when_fits() {
        let mut heap = Vec::new();
        let v = Value::from_bigint(BigInt::from(7), &mut heap);
        assert!(v.is_int(), "small magnitude must demote to TAG_INT");
        assert_eq!(v.as_int(), Some(7));
        assert!(heap.is_empty(), "no heap allocation when demoted");
    }

    #[test]
    fn from_bigint_keeps_big_when_outside_i48() {
        let mut heap = Vec::new();
        let big = BigInt::from(1u64) << 80;
        let v = Value::from_bigint(big, &mut heap);
        assert!(v.is_object());
        assert_eq!(heap.len(), 1);
        assert!(matches!(&heap[0], HeapObject::BigInt(b) if b == &(BigInt::from(1u64) << 80)));
    }

    #[test]
    fn bits_eq_distinguishes_representations() {
        let mut heap = Vec::new();
        let small_7 = Value::from_i64(7, &mut heap);
        // Manually fabricate a BigInt(7) Value to test cross-representation eq.
        let idx = heap.len();
        heap.push(HeapObject::BigInt(BigInt::from(7)));
        let big_7 = Value::object_ref(idx);

        // bits differ: small int 7 has TAG_INT, big_7 has TAG_OBJECT + idx
        assert!(!small_7.bits_eq(big_7));
        // but Python value equality holds
        assert!(small_7.py_eq(big_7, &heap));
        assert!(big_7.py_eq(small_7, &heap));
    }

    #[test]
    fn py_eq_handles_int_float_coercion() {
        let heap: Vec<HeapObject> = Vec::new();
        let int_3 = Value::small_int_unchecked(3);
        let float_3 = Value::float(3.0);
        assert!(int_3.py_eq(float_3, &heap));
        assert!(float_3.py_eq(int_3, &heap));

        let float_3_5 = Value::float(3.5);
        assert!(!int_3.py_eq(float_3_5, &heap));
    }

    #[test]
    fn py_eq_handles_bool_int_equality() {
        let heap: Vec<HeapObject> = Vec::new();
        // Python: True == 1, False == 0
        assert!(Value::bool_val(true).py_eq(Value::small_int_unchecked(1), &heap));
        assert!(Value::bool_val(false).py_eq(Value::small_int_unchecked(0), &heap));
        assert!(!Value::bool_val(true).py_eq(Value::small_int_unchecked(2), &heap));
    }

    #[test]
    fn bigint_displays_decimal() {
        let big: BigInt = BigInt::from(1u64) << 100;
        let expected = big.to_string();
        let mut heap = Vec::new();
        let v = Value::from_bigint(big, &mut heap);
        assert_eq!(v.display(&heap), expected);
    }

    #[test]
    fn is_pyint_recognizes_both_forms() {
        let mut heap = Vec::new();
        let small = Value::small_int_unchecked(42);
        let big = Value::from_bigint(BigInt::from(1u64) << 100, &mut heap);
        assert!(small.is_pyint(&heap));
        assert!(big.is_pyint(&heap));
        assert!(!Value::bool_val(true).is_pyint(&heap));
        assert!(!Value::float(1.0).is_pyint(&heap));
        assert!(!Value::none().is_pyint(&heap));
    }

    // ---------- M3 commit 1: Module + ImportError ----------

    #[test]
    fn module_heap_variant_display_for_source_file() {
        let m = HeapObject::Module {
            name: "mymodule".into(),
            globals: HashMap::new(),
            file: Some("/tmp/mymodule.py".into()),
            package: None,
            initialized: true,
            all: None,
        };
        let heap = vec![m];
        let v = Value::object_ref(0);
        assert_eq!(v.display(&heap), "<module 'mymodule' from '/tmp/mymodule.py'>");
    }

    #[test]
    fn module_heap_variant_display_for_cmodule() {
        let m = HeapObject::Module {
            name: "sys".into(),
            globals: HashMap::new(),
            file: None,
            package: None,
            initialized: true,
            all: None,
        };
        let heap = vec![m];
        let v = Value::object_ref(0);
        assert_eq!(v.display(&heap), "<module 'sys' (built-in)>");
    }

    #[test]
    fn import_error_exception_type_round_trip() {
        assert_eq!(ExceptionType::ImportError.name(), "ImportError");
        assert_eq!(ExceptionType::from_name("ImportError"), Some(ExceptionType::ImportError));
        // ModuleNotFoundError (3.6+) aliases to ImportError in our 3.0.1 target.
        assert_eq!(ExceptionType::from_name("ModuleNotFoundError"), Some(ExceptionType::ImportError));
        assert_eq!(ExceptionType::from_name("NotAnException"), None);
    }

    #[test]
    fn import_error_is_subtype_of_exception() {
        assert!(ExceptionType::ImportError.is_subtype(ExceptionType::Exception));
        assert!(ExceptionType::ImportError.is_subtype(ExceptionType::BaseException));
        assert!(!ExceptionType::ImportError.is_subtype(ExceptionType::ValueError));
    }

    // ---------- M2 commit 2: PyInt arithmetic ----------

    fn small(i: i64) -> PyInt<'static> { PyInt::Small(i) }

    fn as_i64(p: &PyIntOwned) -> Option<i64> {
        match p {
            PyIntOwned::Small(i) => Some(*i),
            _ => None,
        }
    }

    fn as_big(p: &PyIntOwned) -> Option<&BigInt> {
        match p {
            PyIntOwned::Big(b) => Some(b),
            _ => None,
        }
    }

    #[test]
    fn pyint_add_small_no_overflow() {
        assert_eq!(as_i64(&small(3).add(small(4))), Some(7));
        assert_eq!(as_i64(&small(-5).add(small(2))), Some(-3));
    }

    #[test]
    fn pyint_add_promotes_to_bigint_on_i64_overflow() {
        let r = small(i64::MAX).add(small(1));
        let b = as_big(&r).expect("expected Big");
        assert_eq!(b, &(BigInt::from(i64::MAX) + 1));
    }

    #[test]
    fn pyint_add_promotes_to_bigint_on_i48_overflow() {
        // 2^46 + 2^46 = 2^47 — fits in i64 but NOT in i48; must Big.
        let r = small(1i64 << 46).add(small(1i64 << 46));
        assert!(as_big(&r).is_some(), "i48 overflow must produce Big");
    }

    #[test]
    fn pyint_sub_demotes_on_result_fit() {
        // BigInt - BigInt where result fits in i48 → demoted to Small.
        let big = BigInt::from(1u64) << 100;
        let r = PyInt::Big(&big).sub(PyInt::Big(&big));
        assert_eq!(as_i64(&r), Some(0));
    }

    #[test]
    fn pyint_mul_overflow() {
        let r = small(i64::MAX).mul(small(2));
        let b = as_big(&r).expect("Big");
        assert_eq!(b, &(BigInt::from(i64::MAX) * 2));
    }

    #[test]
    fn pyint_floordiv_python_semantics() {
        // Python: -7 // 2 == -4 (not -3 as Rust gives).
        assert_eq!(as_i64(&small(-7).floordiv(small(2)).unwrap()), Some(-4));
        assert_eq!(as_i64(&small(7).floordiv(small(-2)).unwrap()), Some(-4));
        assert_eq!(as_i64(&small(7).floordiv(small(2)).unwrap()), Some(3));
        assert_eq!(as_i64(&small(-7).floordiv(small(-2)).unwrap()), Some(3));
    }

    #[test]
    fn pyint_mod_python_semantics() {
        // Python: result has same sign as divisor.
        assert_eq!(as_i64(&small(-7).mod_(small(2)).unwrap()), Some(1));
        assert_eq!(as_i64(&small(7).mod_(small(-2)).unwrap()), Some(-1));
        assert_eq!(as_i64(&small(7).mod_(small(2)).unwrap()), Some(1));
    }

    #[test]
    fn pyint_div_by_zero() {
        assert_eq!(small(1).floordiv(small(0)).unwrap_err(), ArithError::DivByZero);
        assert_eq!(small(1).mod_(small(0)).unwrap_err(), ArithError::DivByZero);
        assert_eq!(small(1).divmod(small(0)).unwrap_err(), ArithError::DivByZero);
    }

    #[test]
    fn pyint_divmod_pair() {
        let (q, r) = small(-7).divmod(small(2)).unwrap();
        assert_eq!(as_i64(&q), Some(-4));
        assert_eq!(as_i64(&r), Some(1));
    }

    #[test]
    fn pyint_pow_positive_exp() {
        match small(2).pow(small(10)) {
            PyPowResult::Int(o) => assert_eq!(as_i64(&o), Some(1024)),
            other => panic!("expected Int, got {other:?}"),
        }
    }

    #[test]
    fn pyint_pow_big_result() {
        // 2**100 doesn't fit in i64, must be Big.
        match small(2).pow(small(100)) {
            PyPowResult::Int(o) => {
                let b = as_big(&o).expect("Big");
                assert_eq!(b, &(BigInt::from(1u64) << 100));
            }
            other => panic!("expected Int, got {other:?}"),
        }
    }

    #[test]
    fn pyint_pow_negative_exp_returns_float() {
        match small(2).pow(small(-3)) {
            PyPowResult::Float(f) => assert!((f - 0.125).abs() < 1e-10),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn pyint_pow_mod() {
        // 3**10 mod 7 = 59049 mod 7 = 4
        let r = small(3).pow_mod(small(10), small(7)).unwrap();
        assert_eq!(as_i64(&r), Some(4));
    }

    #[test]
    fn pyint_pow_mod_negative_exp_errors() {
        assert_eq!(small(2).pow_mod(small(-1), small(7)).unwrap_err(), ArithError::NegativePower);
    }

    #[test]
    fn pyint_neg() {
        assert_eq!(as_i64(&small(5).neg()), Some(-5));
        assert_eq!(as_i64(&small(-5).neg()), Some(5));
        // i64::MIN.neg() overflows i64; PyInt promotes to Big and computes correctly.
        let r = small(i64::MIN).neg();
        let b = as_big(&r).expect("Big");
        assert_eq!(b, &-BigInt::from(i64::MIN));
    }

    #[test]
    fn pyint_abs() {
        assert_eq!(as_i64(&small(5).abs()), Some(5));
        assert_eq!(as_i64(&small(-5).abs()), Some(5));
    }

    #[test]
    fn pyint_bitwise() {
        assert_eq!(as_i64(&small(0b1100).and_(small(0b1010))), Some(0b1000));
        assert_eq!(as_i64(&small(0b1100).or_(small(0b1010))), Some(0b1110));
        assert_eq!(as_i64(&small(0b1100).xor_(small(0b1010))), Some(0b0110));
        assert_eq!(as_i64(&small(5).invert()), Some(-6)); // ~5 == -6
    }

    #[test]
    fn pyint_shifts() {
        assert_eq!(as_i64(&small(1).shl(small(10)).unwrap()), Some(1024));
        assert_eq!(as_i64(&small(1024).shr(small(10)).unwrap()), Some(1));
        // 1 << 100 → BigInt
        let r = small(1).shl(small(100)).unwrap();
        let b = as_big(&r).expect("Big");
        assert_eq!(b, &(BigInt::from(1u64) << 100));
    }

    #[test]
    fn pyint_shift_negative_errors() {
        assert_eq!(small(1).shl(small(-1)).unwrap_err(), ArithError::NegativeShift);
        assert_eq!(small(1).shr(small(-1)).unwrap_err(), ArithError::NegativeShift);
    }

    #[test]
    fn pyint_shl_negative_msb_promotes() {
        // Regression: the old `(r >> s) == a` arithmetic-shift round-trip
        // check could falsely succeed for negative i64 values near the
        // sign bit. The current i128 fast path promotes correctly.
        // i64::MIN << 1 overflows i64; must become BigInt.
        let r = small(i64::MIN).shl(small(1)).unwrap();
        assert!(as_big(&r).is_some(), "i64::MIN << 1 must promote to BigInt");
        assert_eq!(as_big(&r).unwrap(), &(BigInt::from(i64::MIN) << 1));

        // Negative small int with a shift that keeps it in i48: stays small.
        let r = small(-1).shl(small(2)).unwrap();
        assert_eq!(as_i64(&r), Some(-4));

        // Negative shifted past i48 boundary: BigInt.
        let r = small(-1).shl(small(48)).unwrap();
        assert!(as_big(&r).is_some(), "-(1<<48) must promote to BigInt");
    }

    #[test]
    fn pyint_cmp_and_eq_cross_representation() {
        let big_7 = BigInt::from(7);
        assert!(small(7).eq(PyInt::Big(&big_7)));
        assert!(PyInt::Big(&big_7).eq(small(7)));
        assert!(small(5).cmp(small(7)) == std::cmp::Ordering::Less);
        assert!(PyInt::Big(&big_7).cmp(small(5)) == std::cmp::Ordering::Greater);
    }

    #[test]
    fn pyint_to_f64() {
        assert_eq!(small(3).to_f64(), 3.0);
        assert_eq!(small(-7).to_f64(), -7.0);
        let huge = BigInt::from(2u64).pow(2000); // way past f64 range
        assert!(PyInt::Big(&huge).to_f64().is_infinite());
    }

    #[test]
    fn pyint_truediv_basic() {
        assert_eq!(pyint_truediv(small(7), small(2)).unwrap(), 3.5);
        assert_eq!(pyint_truediv(small(1), small(0)).unwrap_err(), ArithError::DivByZero);
    }

    #[test]
    fn pyint_from_value_or_bool_widens() {
        let heap: Vec<HeapObject> = Vec::new();
        let pi = PyInt::from_value_or_bool(Value::bool_val(true), &heap).unwrap();
        assert!(matches!(pi, PyInt::Small(1)));
        let pi = PyInt::from_value_or_bool(Value::bool_val(false), &heap).unwrap();
        assert!(matches!(pi, PyInt::Small(0)));
    }

    #[test]
    fn pyint_owned_into_value_demotes() {
        let mut heap = Vec::new();
        // BigInt that fits in i48 → small int Value, no heap entry.
        let v = PyIntOwned::Big(BigInt::from(42)).into_value(&mut heap);
        assert!(v.is_int());
        assert_eq!(v.as_int(), Some(42));
        assert!(heap.is_empty());
    }

    #[test]
    fn pyint_owned_into_value_keeps_big() {
        let mut heap = Vec::new();
        let big: BigInt = BigInt::from(1u64) << 100;
        let v = PyIntOwned::Big(big.clone()).into_value(&mut heap);
        assert!(v.is_object());
        assert_eq!(heap.len(), 1);
        assert!(matches!(&heap[0], HeapObject::BigInt(b) if b == &big));
    }

    // ---------- Hash: pinned test corpus (Mersenne reduction) ----------

    #[test]
    fn hash_small_int_corpus() {
        // From Section 4 spec table.
        assert_eq!(small(0).hash(), 0);
        assert_eq!(small(1).hash(), 1);
        assert_eq!(small(-1).hash(), -2); // sentinel substitution
        assert_eq!(small((1i64 << 47) - 1).hash(), (1i64 << 47) - 1); // i48 max
        assert_eq!(small(-(1i64 << 47)).hash(), -(1i64 << 47)); // i48 min
    }

    #[test]
    fn hash_bigint_corpus() {
        // 2^61 - 1 == the modulus → 0
        let p_minus_1 = (BigInt::from(1u64) << 61) - 1;
        assert_eq!(PyInt::Big(&p_minus_1).hash(), 0);

        // 2^61 → 1
        let p_plus_1 = BigInt::from(1u64) << 61;
        assert_eq!(PyInt::Big(&p_plus_1).hash(), 1);

        // -(2^61) → -2 (|x| mod P = 1, sign flip → -1, sentinel → -2)
        let positive_p_plus_1: BigInt = BigInt::from(1u64) << 61;
        let neg_p_plus_1: BigInt = -positive_p_plus_1;
        assert_eq!(PyInt::Big(&neg_p_plus_1).hash(), -2);

        // 2^62 + 1 → 3 ((2P + 1) ≡ 0 + 2 + 1 ≡ 3 mod P)
        let two_p_plus_1: BigInt = (BigInt::from(1u64) << 62) + 1;
        assert_eq!(PyInt::Big(&two_p_plus_1).hash(), 3);
    }

    #[test]
    fn hash_never_returns_minus_one() {
        // Any input that mathematically maps to -1 must come out as -2.
        for x in [-1i64, -((1u64 << 61) as i64 + 1)] {
            let h = small(x).hash();
            assert_ne!(h, -1, "hash({x}) returned the forbidden -1");
        }
    }

    #[test]
    fn mod_mersenne_u64_correctness() {
        // Spot-check against straightforward % to confirm the optimization.
        for x in [0u64, 1, PYHASH_MODULUS - 1, PYHASH_MODULUS, PYHASH_MODULUS + 1,
                  u64::MAX, 1u64 << 61, (1u64 << 61) + 7] {
            assert_eq!(mod_mersenne_u64(x), x % PYHASH_MODULUS, "x={x}");
        }
    }
}
