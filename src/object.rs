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
#[derive(Clone, Copy, PartialEq)]
pub struct Value(u64);

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_float() {
            write!(f, "Value(float={})", self.as_float().unwrap())
        } else if self.is_int() {
            write!(f, "Value(int={})", self.as_int().unwrap())
        } else if self.is_bool() {
            write!(f, "Value(bool={})", self.as_bool().unwrap())
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

    /// Create an integer value (i48 range).
    pub fn int(v: i64) -> Self {
        let payload = (v as u64) & PAYLOAD_MASK;
        Self(make_tagged(TAG_INT, payload))
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
    if let Some(i) = v.as_int() {
        i as u64
    } else if let Some(b) = v.as_bool() {
        b as u64
    } else if v.is_none() {
        0x_DEAD_CAFE
    } else if let Some(idx) = v.as_str_ref() {
        let s = heap[idx].as_str().unwrap_or("");
        let mut h: u64 = 5381;
        for b in s.bytes() {
            h = h.wrapping_mul(33).wrapping_add(b as u64);
        }
        h
    } else {
        // Identity hash for other types
        v.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_roundtrip() {
        for v in [0, 1, -1, 42, -42, 100_000, -100_000, (1 << 47) - 1, -(1 << 47)] {
            let val = Value::int(v);
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
        assert!(Value::int(1).is_truthy());
        assert!(!Value::int(0).is_truthy());
        assert!(Value::float(1.0).is_truthy());
        assert!(!Value::float(0.0).is_truthy());
        assert!(!Value::none().is_truthy());
    }

    #[test]
    fn display_values() {
        let heap = vec![HeapObject::Str("hello".into())];
        assert_eq!(Value::int(42).display(&heap), "42");
        assert_eq!(Value::float(3.14).display(&heap), "3.14");
        assert_eq!(Value::bool_val(true).display(&heap), "True");
        assert_eq!(Value::none().display(&heap), "None");
        assert_eq!(Value::str_ref(0).display(&heap), "hello");
    }

    #[test]
    fn tags_dont_collide() {
        let values = vec![
            Value::int(0),
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
}
