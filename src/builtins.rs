/// Built-in functions: print, range, len, type, int, str, bool, isinstance, super, etc.
use crate::error::PythonError;
use crate::object::{BuiltinId, ExceptionType, HeapObject, PyInt, Value, heap_str, value_hash};
use num_bigint::BigInt;
use std::collections::HashMap;

/// Register all built-in functions into globals.
pub fn register_builtins(globals: &mut HashMap<String, Value>, heap: &mut Vec<HeapObject>) {
    let builtins: Vec<(&str, BuiltinId)> = vec![
        ("print", BuiltinId::Print),
        ("range", BuiltinId::Range),
        ("len", BuiltinId::Len),
        ("type", BuiltinId::Type),
        ("int", BuiltinId::Int),
        ("str", BuiltinId::Str),
        ("bool", BuiltinId::Bool),
        ("float", BuiltinId::Float),
        ("abs", BuiltinId::Abs),
        ("divmod", BuiltinId::Divmod),
        ("min", BuiltinId::Min),
        ("max", BuiltinId::Max),
        ("isinstance", BuiltinId::Isinstance),
        ("issubclass", BuiltinId::Issubclass),
        ("super", BuiltinId::Super),
        ("hasattr", BuiltinId::Hasattr),
        ("getattr", BuiltinId::Getattr),
        ("setattr", BuiltinId::Setattr),
        ("id", BuiltinId::Id),
        ("iter", BuiltinId::Iter),
        ("next", BuiltinId::Next),
    ];

    for (name, id) in builtins {
        let heap_idx = heap.len();
        heap.push(HeapObject::BuiltinFn {
            name: name.to_string(),
            id,
        });
        globals.insert(name.to_string(), Value::object_ref(heap_idx));
    }

    // Register exception type constructors
    let exc_types = [
        ("BaseException", ExceptionType::BaseException),
        ("Exception", ExceptionType::Exception),
        ("TypeError", ExceptionType::TypeError),
        ("ValueError", ExceptionType::ValueError),
        ("NameError", ExceptionType::NameError),
        ("AttributeError", ExceptionType::AttributeError),
        ("IndexError", ExceptionType::IndexError),
        ("KeyError", ExceptionType::KeyError),
        ("ZeroDivisionError", ExceptionType::ZeroDivisionError),
        ("StopIteration", ExceptionType::StopIteration),
        ("RuntimeError", ExceptionType::RuntimeError),
        ("NotImplementedError", ExceptionType::NotImplementedError),
        ("AssertionError", ExceptionType::AssertionError),
        ("OverflowError", ExceptionType::OverflowError),
        ("ImportError", ExceptionType::ImportError),
    ];

    for (name, et) in exc_types {
        let heap_idx = heap.len();
        heap.push(HeapObject::BuiltinFn {
            name: name.to_string(),
            id: BuiltinId::ExcConstructor(et),
        });
        globals.insert(name.to_string(), Value::object_ref(heap_idx));
    }
}

/// Dispatch a built-in function call.
pub fn call_builtin(
    id: BuiltinId,
    args: &[Value],
    heap: &mut Vec<HeapObject>,
    output: &mut Vec<String>,
    globals: &HashMap<String, Value>,
) -> Result<Value, PythonError> {
    match id {
        BuiltinId::Print => builtin_print(args, heap, output),
        BuiltinId::Range => builtin_range(args, heap),
        BuiltinId::Len => builtin_len(args, heap),
        BuiltinId::Type => builtin_type(args, heap),
        BuiltinId::Int => builtin_int(args, heap),
        BuiltinId::Str => builtin_str(args, heap),
        BuiltinId::Bool => builtin_bool(args, heap),
        BuiltinId::Float => builtin_float(args, heap),
        BuiltinId::Abs => builtin_abs(args, heap),
        BuiltinId::Divmod => builtin_divmod(args, heap),
        BuiltinId::Min => builtin_min(args, heap),
        BuiltinId::Max => builtin_max(args, heap),
        BuiltinId::Isinstance => builtin_isinstance(args, heap),
        BuiltinId::Issubclass => builtin_issubclass(args, heap),
        BuiltinId::Super => builtin_super(args, globals),
        BuiltinId::Hasattr => builtin_hasattr(args, heap),
        BuiltinId::Getattr => builtin_getattr(args, heap),
        BuiltinId::Setattr => builtin_setattr(args, heap),
        BuiltinId::Id => builtin_id(args),
        BuiltinId::Iter => Ok(Value::none()), // handled in VM
        BuiltinId::Next => Ok(Value::none()), // handled in VM
        BuiltinId::ExcConstructor(et) => builtin_exc_constructor(et, args, heap),
        // List methods
        BuiltinId::ListAppend => builtin_list_append(args, heap),
        BuiltinId::ListPop => builtin_list_pop(args, heap),
        BuiltinId::ListSort => builtin_list_sort(args, heap),
        BuiltinId::ListReverse => builtin_list_reverse(args, heap),
        BuiltinId::ListInsert => builtin_list_insert(args, heap),
        BuiltinId::ListExtend => builtin_list_extend(args, heap),
        // String methods
        BuiltinId::StrUpper => builtin_str_upper(args, heap),
        BuiltinId::StrLower => builtin_str_lower(args, heap),
        BuiltinId::StrSplit => builtin_str_split(args, heap),
        BuiltinId::StrJoin => builtin_str_join(args, heap),
        BuiltinId::StrReplace => builtin_str_replace(args, heap),
        BuiltinId::StrStartswith => builtin_str_startswith(args, heap),
        BuiltinId::StrEndswith => builtin_str_endswith(args, heap),
        BuiltinId::StrFind => builtin_str_find(args, heap),
        BuiltinId::StrStrip => builtin_str_strip(args, heap),
        BuiltinId::StrFormat => builtin_str_format(args, heap),
        // Dict methods
        BuiltinId::DictKeys => builtin_dict_keys(args, heap),
        BuiltinId::DictValues => builtin_dict_values(args, heap),
        BuiltinId::DictItems => builtin_dict_items(args, heap),
        BuiltinId::DictGet => builtin_dict_get(args, heap),
        BuiltinId::DictPop => builtin_dict_pop(args, heap),
    }
}

fn builtin_print(args: &[Value], heap: &[HeapObject], output: &mut Vec<String>) -> Result<Value, PythonError> {
    // Build the line directly instead of collecting into Vec<String> + join;
    // saves one Vec allocation per print() call.
    let mut line = String::new();
    for (i, v) in args.iter().enumerate() {
        if i > 0 { line.push(' '); }
        line.push_str(&v.display(heap));
    }
    output.push(line);
    Ok(Value::none())
}

fn builtin_range(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let (start, stop, step) = match args.len() {
        1 => {
            let stop = args[0].as_int().ok_or_else(|| PythonError::runtime("range() integer expected", 0))?;
            (0, stop, 1)
        }
        2 => {
            let start = args[0].as_int().ok_or_else(|| PythonError::runtime("range() integer expected", 0))?;
            let stop = args[1].as_int().ok_or_else(|| PythonError::runtime("range() integer expected", 0))?;
            (start, stop, 1)
        }
        3 => {
            let start = args[0].as_int().ok_or_else(|| PythonError::runtime("range() integer expected", 0))?;
            let stop = args[1].as_int().ok_or_else(|| PythonError::runtime("range() integer expected", 0))?;
            let step = args[2].as_int().ok_or_else(|| PythonError::runtime("range() integer expected", 0))?;
            if step == 0 {
                return Err(PythonError::runtime("range() arg 3 must not be zero", 0));
            }
            (start, stop, step)
        }
        _ => return Err(PythonError::runtime("range() takes 1 to 3 arguments", 0)),
    };

    let heap_idx = heap.len();
    heap.push(HeapObject::RangeIter {
        current: start,
        stop,
        step,
    });
    Ok(Value::range_ref(heap_idx))
}

fn builtin_len(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    if args.len() != 1 {
        return Err(PythonError::runtime("len() takes exactly one argument", 0));
    }
    let val = args[0];
    if let Some(idx) = val.as_str_ref() {
        let s = heap_str(heap, idx)?;
        Ok(Value::small_int_unchecked(s.len() as i64))
    } else if let Some(idx) = val.as_list_ref() {
        if let HeapObject::List(items) = &heap[idx] {
            Ok(Value::small_int_unchecked(items.len() as i64))
        } else {
            Err(PythonError::runtime("object has no len()", 0))
        }
    } else if let Some(idx) = val.as_object_ref() {
        match &heap[idx] {
            HeapObject::Tuple(items) => Ok(Value::small_int_unchecked(items.len() as i64)),
            HeapObject::Dict { keys, .. } => Ok(Value::small_int_unchecked(keys.len() as i64)),
            HeapObject::Set(items) => Ok(Value::small_int_unchecked(items.len() as i64)),
            _ => Err(PythonError::runtime("object has no len()", 0)),
        }
    } else {
        Err(PythonError::runtime("object has no len()", 0))
    }
}

fn builtin_type(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    if args.len() != 1 {
        return Err(PythonError::runtime("type() takes exactly one argument", 0));
    }
    let val = args[0];
    let type_name = if val.is_int() {
        "<class 'int'>"
    } else if val.is_float() {
        "<class 'float'>"
    } else if val.is_bool() {
        "<class 'bool'>"
    } else if val.is_none() {
        "<class 'NoneType'>"
    } else if val.is_str() {
        "<class 'str'>"
    } else if val.is_list() {
        "<class 'list'>"
    } else if val.is_func() {
        "<class 'function'>"
    } else if let Some(idx) = val.as_object_ref() {
        match &heap[idx] {
            HeapObject::Instance { class_idx, .. } => {
                if let HeapObject::Class { name, .. } = &heap[*class_idx] {
                    let s = format!("<class '{name}'>");
                    let heap_idx = heap.len();
                    heap.push(HeapObject::Str(s.into()));
                    return Ok(Value::str_ref(heap_idx));
                }
                "<class 'object'>"
            }
            HeapObject::Class { .. } => "<class 'type'>",
            HeapObject::Tuple(_) => "<class 'tuple'>",
            HeapObject::Dict { .. } => "<class 'dict'>",
            HeapObject::Set(_) => "<class 'set'>",
            _ => "<class 'object'>"
        }
    } else {
        "<class 'object'>"
    };
    let heap_idx = heap.len();
    heap.push(HeapObject::Str(type_name.into()));
    Ok(Value::str_ref(heap_idx))
}

fn builtin_int(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    if args.is_empty() {
        return Ok(Value::small_int_unchecked(0));
    }
    if args.len() != 1 {
        return Err(PythonError::runtime("int() takes at most one argument", 0));
    }
    let val = args[0];
    // Already an int (small or big) — pass through unchanged. Bool widens.
    if val.is_pyint(heap) {
        return Ok(val);
    }
    if let Some(b) = val.as_bool() {
        return Ok(Value::small_int_unchecked(b as i64));
    }
    if let Some(f) = val.as_float() {
        // float → int truncates toward zero. For huge floats this can produce
        // a BigInt; for inf/nan, Python raises (we surface as a runtime error).
        if !f.is_finite() {
            return Err(PythonError::runtime(
                "cannot convert float infinity/NaN to integer", 0,
            ));
        }
        // Floats up to ~9e18 fit in i64; beyond that route through BigInt.
        if f.abs() < (i64::MAX as f64) {
            return Ok(Value::from_i64(f.trunc() as i64, heap));
        }
        // Use the string round-trip — slow but correct for the rare big-float case.
        let truncated = f.trunc();
        let s = format!("{truncated:.0}");
        let b: BigInt = s.parse().map_err(|_| {
            PythonError::runtime("cannot convert float to integer", 0)
        })?;
        return Ok(Value::from_bigint(b, heap));
    }
    if let Some(idx) = val.as_str_ref() {
        let s = heap_str(heap, idx)?.trim();
        // Try i64 first, fall back to BigInt — same dispatch pattern as the lexer.
        if let Ok(i) = s.parse::<i64>() {
            return Ok(Value::from_i64(i, heap));
        }
        let b: BigInt = s.parse().map_err(|_| {
            PythonError::runtime(format!("invalid literal for int() with base 10: '{s}'"), 0)
        })?;
        return Ok(Value::from_bigint(b, heap));
    }
    Err(PythonError::runtime("int() argument must be a string or number", 0))
}

fn builtin_str(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    if args.is_empty() {
        let heap_idx = heap.len();
        heap.push(HeapObject::Str("".into()));
        return Ok(Value::str_ref(heap_idx));
    }
    if args.len() != 1 {
        return Err(PythonError::runtime("str() takes at most one argument", 0));
    }
    let s = args[0].display(heap);
    let heap_idx = heap.len();
    heap.push(HeapObject::Str(s.into()));
    Ok(Value::str_ref(heap_idx))
}

fn builtin_bool(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    if args.is_empty() {
        return Ok(Value::bool_val(false));
    }
    if args.len() != 1 {
        return Err(PythonError::runtime("bool() takes at most one argument", 0));
    }
    // Check heap-based truthiness
    let val = args[0];
    if let Some(idx) = val.as_str_ref() && let Some(s) = heap[idx].as_str() {
        return Ok(Value::bool_val(!s.is_empty()));
    }
    if let Some(idx) = val.as_list_ref() && let HeapObject::List(items) = &heap[idx] {
        return Ok(Value::bool_val(!items.is_empty()));
    }
    Ok(Value::bool_val(val.is_truthy()))
}

fn builtin_float(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    if args.is_empty() {
        return Ok(Value::float(0.0));
    }
    if args.len() != 1 {
        return Err(PythonError::runtime("float() takes at most one argument", 0));
    }
    let val = args[0];
    if let Some(f) = val.as_float() {
        return Ok(Value::float(f));
    }
    // Int (small or big) or bool — go through PyInt::to_f64 (BigInt that
    // overflows f64's exponent collapses to inf, matching CPython).
    if let Some(pi) = PyInt::from_value_or_bool(val, heap) {
        return Ok(Value::float(pi.to_f64()));
    }
    if let Some(idx) = val.as_str_ref() {
        let s = heap_str(heap, idx)?;
        let f: f64 = s.trim().parse().map_err(|_| {
            PythonError::runtime(format!("could not convert string to float: '{s}'"), 0)
        })?;
        return Ok(Value::float(f));
    }
    Err(PythonError::runtime("float() argument must be a string or number", 0))
}

fn builtin_abs(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    if args.len() != 1 {
        return Err(PythonError::runtime("abs() takes exactly one argument", 0));
    }
    let val = args[0];
    if let Some(pi) = PyInt::from_value_or_bool(val, heap) {
        return Ok(pi.abs().into_value(heap));
    }
    if let Some(f) = val.as_float() {
        return Ok(Value::float(f.abs()));
    }
    Err(PythonError::runtime("bad operand type for abs()", 0))
}

fn builtin_divmod(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    if args.len() != 2 {
        return Err(PythonError::runtime("divmod() takes exactly two arguments", 0));
    }
    let a = PyInt::from_value_or_bool(args[0], heap);
    let b = PyInt::from_value_or_bool(args[1], heap);
    match (a, b) {
        (Some(ai), Some(bi)) => {
            let (q, r) = ai.divmod(bi)
                .map_err(|_| PythonError::runtime("integer division or modulo by zero", 0))?;
            let qv = q.into_value(heap);
            let rv = r.into_value(heap);
            let tuple_idx = heap.len();
            heap.push(HeapObject::Tuple(vec![qv, rv]));
            Ok(Value::object_ref(tuple_idx))
        }
        _ => Err(PythonError::runtime("divmod() requires two integer arguments", 0)),
    }
}

fn builtin_min(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    minmax(args, heap, MinMax::Min)
}

fn builtin_max(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    minmax(args, heap, MinMax::Max)
}

#[derive(Clone, Copy)]
enum MinMax { Min, Max }

impl MinMax {
    fn name(self) -> &'static str {
        match self { Self::Min => "min", Self::Max => "max" }
    }
    fn want(self) -> std::cmp::Ordering {
        match self { Self::Min => std::cmp::Ordering::Less, Self::Max => std::cmp::Ordering::Greater }
    }
}

fn minmax(args: &[Value], heap: &[HeapObject], kind: MinMax) -> Result<Value, PythonError> {
    if args.len() < 2 {
        return Err(PythonError::runtime(format!("{}() requires at least 2 arguments", kind.name()), 0));
    }
    let mut result = args[0];
    for arg in &args[1..] {
        let replaces = if let (Some(a), Some(b)) = (
            PyInt::from_value_or_bool(*arg, heap),
            PyInt::from_value_or_bool(result, heap),
        ) {
            a.cmp(b) == kind.want()
        } else if let (Some(a), Some(b)) = (arg.to_f64(), result.to_f64()) {
            // partial_cmp returns None for NaN, so NaN never replaces — same
            // behavior as the prior `a < b` / `a > b` checks.
            a.partial_cmp(&b) == Some(kind.want())
        } else {
            false
        };
        if replaces { result = *arg; }
    }
    Ok(result)
}

fn builtin_isinstance(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    if args.len() != 2 {
        return Err(PythonError::runtime("isinstance() takes exactly 2 arguments", 0));
    }
    let obj = args[0];
    let type_val = args[1];

    // Get the class of the object
    if let Some(obj_idx) = obj.as_object_ref()
        && let HeapObject::Instance { class_idx, .. } = &heap[obj_idx]
    {
        let obj_class = *class_idx;
        // Get the target class
        if let Some(type_idx) = type_val.as_object_ref()
            && let HeapObject::Class { .. } = &heap[type_idx]
        {
            // Check if obj_class is type_idx or a subclass
            if let HeapObject::Class { mro, .. } = &heap[obj_class] {
                return Ok(Value::bool_val(mro.contains(&type_idx)));
            }
        }
    }
    // Check primitive types
    if let Some(type_idx) = type_val.as_object_ref() {
        if let HeapObject::BuiltinFn { id: BuiltinId::Int, .. } = &heap[type_idx] {
            return Ok(Value::bool_val(obj.is_int()));
        }
        if let HeapObject::BuiltinFn { id: BuiltinId::Str, .. } = &heap[type_idx] {
            return Ok(Value::bool_val(obj.is_str()));
        }
        if let HeapObject::BuiltinFn { id: BuiltinId::Bool, .. } = &heap[type_idx] {
            return Ok(Value::bool_val(obj.is_bool()));
        }
        if let HeapObject::BuiltinFn { id: BuiltinId::Float, .. } = &heap[type_idx] {
            return Ok(Value::bool_val(obj.is_float()));
        }
    }
    Ok(Value::bool_val(false))
}

fn builtin_issubclass(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    if args.len() != 2 {
        return Err(PythonError::runtime("issubclass() takes exactly 2 arguments", 0));
    }
    if let (Some(cls_idx), Some(base_idx)) = (args[0].as_object_ref(), args[1].as_object_ref())
        && let HeapObject::Class { mro, .. } = &heap[cls_idx]
    {
        return Ok(Value::bool_val(mro.contains(&base_idx)));
    }
    Ok(Value::bool_val(false))
}

fn builtin_super(_args: &[Value], _globals: &HashMap<String, Value>) -> Result<Value, PythonError> {
    // super() is handled specially in the VM during CALL_FUNCTION
    // Here we just return None as a placeholder — actual super resolution happens
    // when attributes are accessed on the super result
    Ok(Value::none())
}

fn builtin_hasattr(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    if args.len() != 2 {
        return Err(PythonError::runtime("hasattr() takes exactly 2 arguments", 0));
    }
    let obj = args[0];
    let attr_name = args[1].display(heap);
    if let Some(obj_idx) = obj.as_object_ref() {
        match &heap[obj_idx] {
            HeapObject::Instance { class_idx, attrs } => {
                if attrs.contains_key(&attr_name) {
                    return Ok(Value::bool_val(true));
                }
                // Check class attrs
                if let HeapObject::Class { attrs: cattrs, mro, .. } = &heap[*class_idx] {
                    if cattrs.contains_key(&attr_name) {
                        return Ok(Value::bool_val(true));
                    }
                    for &m in mro.iter().skip(1) {
                        if let HeapObject::Class { attrs: ma, .. } = &heap[m]
                            && ma.contains_key(&attr_name)
                        {
                            return Ok(Value::bool_val(true));
                        }
                    }
                }
            }
            HeapObject::Class { attrs, .. } => {
                return Ok(Value::bool_val(attrs.contains_key(&attr_name)));
            }
            _ => {}
        }
    }
    Ok(Value::bool_val(false))
}

fn builtin_getattr(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    if args.len() < 2 || args.len() > 3 {
        return Err(PythonError::runtime("getattr() takes 2 to 3 arguments", 0));
    }
    let obj = args[0];
    let attr_name = args[1].display(heap);
    let default = if args.len() == 3 { Some(args[2]) } else { None };

    if let Some(obj_idx) = obj.as_object_ref()
        && let HeapObject::Instance { attrs, .. } = &heap[obj_idx]
        && let Some(&val) = attrs.get(&attr_name)
    {
        return Ok(val);
    }
    if let Some(d) = default {
        Ok(d)
    } else {
        Err(PythonError::runtime(format!("object has no attribute '{attr_name}'"), 0))
    }
}

fn builtin_setattr(args: &[Value], heap: &mut [HeapObject]) -> Result<Value, PythonError> {
    if args.len() != 3 {
        return Err(PythonError::runtime("setattr() takes exactly 3 arguments", 0));
    }
    let obj = args[0];
    let attr_name = args[1].display(heap);
    let val = args[2];
    if let Some(obj_idx) = obj.as_object_ref()
        && let HeapObject::Instance { attrs, .. } = &mut heap[obj_idx]
    {
        attrs.insert(attr_name, val);
        return Ok(Value::none());
    }
    Err(PythonError::runtime("setattr: object does not support attribute assignment", 0))
}

fn builtin_id(args: &[Value]) -> Result<Value, PythonError> {
    if args.len() != 1 {
        return Err(PythonError::runtime("id() takes exactly one argument", 0));
    }
    // Return the raw bit pattern as an int (unique id)
    Ok(Value::small_int_unchecked(args[0].display_bits() as i64))
}

fn builtin_exc_constructor(et: ExceptionType, args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let msg = if !args.is_empty() {
        args[0].display(heap)
    } else {
        String::new()
    };
    let idx = heap.len();
    heap.push(HeapObject::ExceptionObj {
        exc_type: et,
        message: msg,
        args: args.to_vec(),
    });
    Ok(Value::object_ref(idx))
}

// --- List methods ---

fn builtin_list_append(args: &[Value], heap: &mut [HeapObject]) -> Result<Value, PythonError> {
    if args.len() != 2 {
        return Err(PythonError::runtime("append() takes exactly one argument", 0));
    }
    let list = args[0];
    let val = args[1];
    if let Some(idx) = list.as_list_ref()
        && let HeapObject::List(items) = &mut heap[idx]
    {
        items.push(val);
        return Ok(Value::none());
    }
    Err(PythonError::runtime("append: not a list", 0))
}

fn builtin_list_pop(args: &[Value], heap: &mut [HeapObject]) -> Result<Value, PythonError> {
    let list = args[0];
    if let Some(idx) = list.as_list_ref()
        && let HeapObject::List(items) = &mut heap[idx]
    {
        if args.len() == 2 {
            let i = args[1].as_int().ok_or_else(|| PythonError::runtime("pop: index must be integer", 0))?;
            let i = if i < 0 { items.len() as i64 + i } else { i } as usize;
            if i < items.len() {
                return Ok(items.remove(i));
            }
            return Err(PythonError::runtime("pop index out of range", 0));
        }
        return items.pop()
            .ok_or_else(|| PythonError::runtime("pop from empty list", 0));
    }
    Err(PythonError::runtime("pop: not a list", 0))
}

fn builtin_list_sort(args: &[Value], heap: &mut [HeapObject]) -> Result<Value, PythonError> {
    let list = args[0];
    if let Some(idx) = list.as_list_ref()
        && let HeapObject::List(items) = &mut heap[idx]
    {
        items.sort_by(|a, b| {
            if let (Some(ai), Some(bi)) = (a.as_int(), b.as_int()) {
                ai.cmp(&bi)
            } else if let (Some(af), Some(bf)) = (a.to_f64(), b.to_f64()) {
                af.partial_cmp(&bf).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                std::cmp::Ordering::Equal
            }
        });
        return Ok(Value::none());
    }
    Err(PythonError::runtime("sort: not a list", 0))
}

fn builtin_list_reverse(args: &[Value], heap: &mut [HeapObject]) -> Result<Value, PythonError> {
    let list = args[0];
    if let Some(idx) = list.as_list_ref()
        && let HeapObject::List(items) = &mut heap[idx]
    {
        items.reverse();
        return Ok(Value::none());
    }
    Err(PythonError::runtime("reverse: not a list", 0))
}

fn builtin_list_insert(args: &[Value], heap: &mut [HeapObject]) -> Result<Value, PythonError> {
    if args.len() != 3 {
        return Err(PythonError::runtime("insert() takes exactly 2 arguments", 0));
    }
    let list = args[0];
    let i = args[1].as_int().ok_or_else(|| PythonError::runtime("insert: index must be integer", 0))?;
    let val = args[2];
    if let Some(idx) = list.as_list_ref()
        && let HeapObject::List(items) = &mut heap[idx]
    {
        let i = if i < 0 { (items.len() as i64 + i).max(0) } else { i } as usize;
        let i = i.min(items.len());
        items.insert(i, val);
        return Ok(Value::none());
    }
    Err(PythonError::runtime("insert: not a list", 0))
}

fn builtin_list_extend(args: &[Value], heap: &mut [HeapObject]) -> Result<Value, PythonError> {
    if args.len() != 2 {
        return Err(PythonError::runtime("extend() takes exactly one argument", 0));
    }
    let list = args[0];
    let other = args[1];
    // Collect items from other first
    let other_items = if let Some(idx) = other.as_list_ref()
        && let HeapObject::List(items) = &heap[idx]
    {
        items.clone()
    } else {
        return Err(PythonError::runtime("extend: argument is not iterable", 0));
    };
    if let Some(idx) = list.as_list_ref()
        && let HeapObject::List(items) = &mut heap[idx]
    {
        items.extend(other_items);
        return Ok(Value::none());
    }
    Err(PythonError::runtime("extend: not a list", 0))
}

// --- String methods ---

fn builtin_str_upper(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let s = get_self_str(args, heap)?;
    let result = s.to_uppercase();
    let idx = heap.len();
    heap.push(HeapObject::Str(result.into()));
    Ok(Value::str_ref(idx))
}

fn builtin_str_lower(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let s = get_self_str(args, heap)?;
    let result = s.to_lowercase();
    let idx = heap.len();
    heap.push(HeapObject::Str(result.into()));
    Ok(Value::str_ref(idx))
}

fn builtin_str_split(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let s = get_self_str(args, heap)?;
    let parts: Vec<&str> = if args.len() >= 2 {
        let sep = args[1].display(heap);
        s.split(&sep).collect()
    } else {
        s.split_whitespace().collect()
    };
    let mut items = Vec::new();
    for part in parts {
        let idx = heap.len();
        heap.push(HeapObject::Str(part.into()));
        items.push(Value::str_ref(idx));
    }
    let list_idx = heap.len();
    heap.push(HeapObject::List(items));
    Ok(Value::list_ref(list_idx))
}

fn builtin_str_join(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let sep = get_self_str(args, heap)?;
    if args.len() < 2 {
        return Err(PythonError::runtime("join() takes exactly one argument", 0));
    }
    let iterable = args[1];
    let mut parts = Vec::new();
    if let Some(idx) = iterable.as_list_ref()
        && let HeapObject::List(items) = &heap[idx]
    {
        for item in items {
            parts.push(item.display(heap));
        }
    }
    let result = parts.join(&sep);
    let idx = heap.len();
    heap.push(HeapObject::Str(result.into()));
    Ok(Value::str_ref(idx))
}

fn builtin_str_replace(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let s = get_self_str(args, heap)?;
    if args.len() < 3 {
        return Err(PythonError::runtime("replace() takes at least 2 arguments", 0));
    }
    let old = args[1].display(heap);
    let new = args[2].display(heap);
    let result = s.replace(&old, &new);
    let idx = heap.len();
    heap.push(HeapObject::Str(result.into()));
    Ok(Value::str_ref(idx))
}

fn builtin_str_startswith(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    let s = get_self_str(args, heap)?;
    if args.len() < 2 {
        return Err(PythonError::runtime("startswith() takes at least 1 argument", 0));
    }
    let prefix = args[1].display(heap);
    Ok(Value::bool_val(s.starts_with(&prefix)))
}

fn builtin_str_endswith(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    let s = get_self_str(args, heap)?;
    if args.len() < 2 {
        return Err(PythonError::runtime("endswith() takes at least 1 argument", 0));
    }
    let suffix = args[1].display(heap);
    Ok(Value::bool_val(s.ends_with(&suffix)))
}

fn builtin_str_find(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    let s = get_self_str(args, heap)?;
    if args.len() < 2 {
        return Err(PythonError::runtime("find() takes at least 1 argument", 0));
    }
    let sub = args[1].display(heap);
    let result = s.find(&sub).map(|i| i as i64).unwrap_or(-1);
    Ok(Value::small_int_unchecked(result))
}

fn builtin_str_strip(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let s = get_self_str(args, heap)?;
    let result = s.trim().to_string();
    let idx = heap.len();
    heap.push(HeapObject::Str(result.into()));
    Ok(Value::str_ref(idx))
}

fn builtin_str_format(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    // Simplified format: just return self for now
    let s = get_self_str(args, heap)?;
    let idx = heap.len();
    heap.push(HeapObject::Str(s.into()));
    Ok(Value::str_ref(idx))
}

fn get_self_str(args: &[Value], heap: &[HeapObject]) -> Result<String, PythonError> {
    if args.is_empty() {
        return Err(PythonError::runtime("method called with no self", 0));
    }
    let self_val = args[0];
    if let Some(idx) = self_val.as_str_ref() {
        Ok(heap[idx].as_str().unwrap_or("").to_string())
    } else {
        Ok(self_val.display(heap))
    }
}

// --- Dict methods ---

fn builtin_dict_keys(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let dict = args[0];
    if let Some(idx) = dict.as_object_ref()
        && let HeapObject::Dict { keys, .. } = &heap[idx]
    {
        let items = keys.clone();
        let list_idx = heap.len();
        heap.push(HeapObject::List(items));
        return Ok(Value::list_ref(list_idx));
    }
    Err(PythonError::runtime("keys: not a dict", 0))
}

fn builtin_dict_values(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let dict = args[0];
    if let Some(idx) = dict.as_object_ref()
        && let HeapObject::Dict { values, .. } = &heap[idx]
    {
        let items = values.clone();
        let list_idx = heap.len();
        heap.push(HeapObject::List(items));
        return Ok(Value::list_ref(list_idx));
    }
    Err(PythonError::runtime("values: not a dict", 0))
}

fn builtin_dict_items(args: &[Value], heap: &mut Vec<HeapObject>) -> Result<Value, PythonError> {
    let dict = args[0];
    if let Some(idx) = dict.as_object_ref() {
        // Collect pairs first to avoid borrow conflict
        let pairs: Vec<(Value, Value)> = if let HeapObject::Dict { keys, values, .. } = &heap[idx] {
            keys.iter().copied().zip(values.iter().copied()).collect()
        } else {
            return Err(PythonError::runtime("items: not a dict", 0));
        };
        let mut items = Vec::new();
        for (k, v) in pairs {
            let tuple_idx = heap.len();
            heap.push(HeapObject::Tuple(vec![k, v]));
            items.push(Value::object_ref(tuple_idx));
        }
        let list_idx = heap.len();
        heap.push(HeapObject::List(items));
        return Ok(Value::list_ref(list_idx));
    }
    Err(PythonError::runtime("items: not a dict", 0))
}

fn builtin_dict_get(args: &[Value], heap: &[HeapObject]) -> Result<Value, PythonError> {
    if args.len() < 2 {
        return Err(PythonError::runtime("get() takes at least 1 argument", 0));
    }
    let dict = args[0];
    let key = args[1];
    let default = if args.len() >= 3 { args[2] } else { Value::none() };

    if let Some(idx) = dict.as_object_ref()
        && let HeapObject::Dict { keys, values, index_map } = &heap[idx]
    {
        let h = value_hash(key, heap);
        if let Some(&i) = index_map.get(&h)
            && i < keys.len()
        {
            return Ok(values[i]);
        }
        // Hash-collision fallback: linear scan with full equality check.
        for (i, k) in keys.iter().enumerate() {
            if value_hash(*k, heap) == h {
                return Ok(values[i]);
            }
        }
        return Ok(default);
    }
    Err(PythonError::runtime("get: not a dict", 0))
}

fn builtin_dict_pop(args: &[Value], heap: &mut [HeapObject]) -> Result<Value, PythonError> {
    if args.len() < 2 {
        return Err(PythonError::runtime("pop() takes at least 1 argument", 0));
    }
    let dict = args[0];
    let key = args[1];
    let default = if args.len() >= 3 { Some(args[2]) } else { None };

    if let Some(idx) = dict.as_object_ref()
        && let HeapObject::Dict { keys, values, index_map } = &mut heap[idx]
    {
        let h = value_hash(key, &[]);  // simplified hash
        if let Some(&i) = index_map.get(&h)
            && i < keys.len()
        {
            let val = values[i];
            keys.remove(i);
            values.remove(i);
            index_map.remove(&h);
            // Reindex
            let new_map: HashMap<u64, usize> = keys.iter().enumerate()
                .map(|(i, k)| (value_hash(*k, &[]), i))
                .collect();
            *index_map = new_map;
            return Ok(val);
        }
        if let Some(d) = default {
            return Ok(d);
        }
        return Err(PythonError::runtime("KeyError", 0));
    }
    Err(PythonError::runtime("pop: not a dict", 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_builtins() {
        let mut globals = HashMap::new();
        let mut heap = Vec::new();
        register_builtins(&mut globals, &mut heap);
        assert!(globals.contains_key("print"));
        assert!(globals.contains_key("range"));
        assert!(globals.contains_key("len"));
        assert!(globals.contains_key("isinstance"));
        assert!(globals.contains_key("ValueError"));
    }

    #[test]
    fn test_print() {
        let heap = vec![HeapObject::Str("hello".into())];
        let mut output = Vec::new();
        let result = builtin_print(&[Value::small_int_unchecked(42)], &heap, &mut output);
        assert!(result.is_ok());
        assert_eq!(output, vec!["42"]);
    }

    #[test]
    fn test_range() {
        let mut heap = Vec::new();
        let result = builtin_range(&[Value::small_int_unchecked(5)], &mut heap).unwrap();
        assert!(result.is_range());
        if let HeapObject::RangeIter { current, stop, step } = &heap[result.as_range_ref().unwrap()] {
            assert_eq!(*current, 0);
            assert_eq!(*stop, 5);
            assert_eq!(*step, 1);
        }
    }

    #[test]
    fn test_range_with_start_stop() {
        let mut heap = Vec::new();
        let result = builtin_range(&[Value::small_int_unchecked(1), Value::small_int_unchecked(10)], &mut heap).unwrap();
        assert!(result.is_range());
        if let HeapObject::RangeIter { current, stop, .. } = &heap[result.as_range_ref().unwrap()] {
            assert_eq!(*current, 1);
            assert_eq!(*stop, 10);
        }
    }
}
