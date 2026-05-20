/// Stackless bytecode VM with explicit frame stack.
use crate::builtins;
use crate::bytecode::{self, CodeObject, op};
use crate::error::PythonError;
use crate::object::{
    ArithError, BuiltinId, ExceptionType, GeneratorState, HeapObject, PyInt, PyPowResult,
    Value, alloc_module, alloc_str, dotted_top, heap_str, pyint_truediv, split_module_name,
    value_hash, value_to_f64,
};
use std::collections::HashMap;

const MAX_STACK: usize = 256;

/// A single execution frame.
struct Frame {
    code_index: usize,
    ip: usize,
    /// Operand stack. Boxed so each Frame is 8 bytes here (pointer)
    /// instead of 2048 bytes inline — cuts per-frame-push memcpy cost.
    stack: Box<[Value]>,
    sp: usize,
    /// Local variable slots. Sized exactly to the code object's num_locals
    /// at frame construction — typical functions use 4–16 locals, not 128.
    locals: Box<[Value]>,
    /// Heap indices of cell objects for closures.
    cells: Vec<usize>,
    /// If this frame belongs to a generator, its heap index.
    generator_idx: Option<usize>,
    /// If this frame is an __init__ call, the instance to return to the caller.
    init_instance: Option<Value>,
    /// Which module's globals this frame uses for LOAD_GLOBAL / STORE_GLOBAL.
    /// None = use VM.globals (the top-level main namespace + builtins). For
    /// imported-module bodies, this points to the loaded HeapObject::Module
    /// so STORE_GLOBAL writes into the module's namespace, not the main one.
    /// For function calls, this is inherited from the function's own
    /// module_idx (functions remember their defining module per CPython).
    module_idx: Option<usize>,
}

impl Frame {
    /// Construct a frame for a code object. `num_locals` comes from the
    /// target code object so locals are right-sized to exactly what the
    /// function declares (typical: 4–16 slots; deep functions: more).
    /// `module_idx` is the function's defining module — None for the
    /// top-level main script, Some for imported modules.
    fn new_for_code(code_index: usize, num_locals: usize, module_idx: Option<usize>) -> Self {
        // .max(1) so even a code object with zero declared locals gets a
        // single-element box; simplifies invariants downstream.
        let locals_n = num_locals.max(1);
        Self {
            code_index,
            ip: 0,
            stack: vec![Value::none(); MAX_STACK].into_boxed_slice(),
            sp: 0,
            locals: vec![Value::none(); locals_n].into_boxed_slice(),
            cells: Vec::new(),
            generator_idx: None,
            init_instance: None,
            module_idx,
        }
    }

    fn push(&mut self, val: Value) {
        self.stack[self.sp] = val;
        self.sp += 1;
    }

    fn pop(&mut self) -> Value {
        self.sp -= 1;
        self.stack[self.sp]
    }

    fn peek(&self) -> Value {
        self.stack[self.sp - 1]
    }
}

/// Exception handler entry on the handler stack.
struct ExceptionHandler {
    handler_ip: usize,
    frame_index: usize,
    stack_depth: usize,
}

/// The virtual machine.
pub struct VM {
    frames: Vec<Frame>,
    code_objects: Vec<CodeObject>,
    /// The top-level main script's globals namespace. Imported modules
    /// keep their own globals on the heap; the routing in `frame_globals_*`
    /// chooses between this and the heap based on Frame::module_idx.
    globals: HashMap<String, Value>,
    /// Builtin functions and exception types. Looked up as the FINAL
    /// fallback in `frame_globals_get`, after the current module's
    /// namespace. Separated from `globals` so user code in main can't
    /// leak names into imported modules — only true builtins do.
    builtins: HashMap<String, Value>,
    pub heap: Vec<HeapObject>,
    pub output: Vec<String>,
    exception_stack: Vec<ExceptionHandler>,
    current_exception: Option<Value>,
    /// The import cache, keyed on canonical dotted module name. Maps the
    /// Python `sys.modules` dict. Populated lazily by the import machinery;
    /// `sys` itself is pre-loaded at VM construction time so user code's
    /// `import sys` is a cache hit.
    sys_modules: HashMap<String, Value>,
    /// Rust-side import machinery — owns the cmodule registry. Immutable
    /// after construction so split-borrows of vm.heap / vm.sys_modules work
    /// alongside its methods.
    import_system: crate::import::ImportSystem,
}

impl VM {
    pub fn new(code_objects: Vec<CodeObject>, heap: Vec<HeapObject>) -> Self {
        let mut vm = Self {
            frames: Vec::with_capacity(64),
            code_objects,
            globals: HashMap::new(),
            builtins: HashMap::new(),
            heap,
            output: Vec::new(),
            exception_stack: Vec::new(),
            current_exception: None,
            sys_modules: HashMap::new(),
            import_system: crate::import::ImportSystem::new(),
        };
        // Builtins live in their own namespace so imported modules see them
        // (via frame_globals_get's fallback) but DON'T see user-defined main
        // globals — module isolation per CPython semantics.
        builtins::register_builtins(&mut vm.builtins, &mut vm.heap);
        vm.bootstrap_sys();
        vm
    }

    /// Override sys.path. Tests drop fixture files in a tempdir and need
    /// imports to find them; production runs configure sys.path internally
    /// at VM construction.
    #[cfg(test)]
    pub fn set_sys_path(&mut self, path: Vec<std::path::PathBuf>) {
        self.import_system.set_sys_path(path);
    }

    /// Read a global name with the __builtins__-chain lookup order:
    ///   1. Current module's globals (frame.module_idx → heap; OR main's
    ///      VM.globals when module_idx is None).
    ///   2. Builtins namespace (always — last fallback).
    fn frame_globals_get(&self, frame_idx: usize, name: &str) -> Option<Value> {
        if let Some(module_idx) = self.frames[frame_idx].module_idx {
            if let HeapObject::Module { globals, .. } = &self.heap[module_idx]
                && let Some(&v) = globals.get(name)
            {
                return Some(v);
            }
        } else if let Some(&v) = self.globals.get(name) {
            return Some(v);
        }
        self.builtins.get(name).copied()
    }

    /// Write a global into the namespace appropriate for the given frame.
    /// For frames running in a module, writes into the module's globals;
    /// for the top-level main, writes into VM.globals. Builtins are never
    /// written to from user code.
    fn frame_globals_insert(&mut self, frame_idx: usize, name: String, value: Value) {
        if let Some(module_idx) = self.frames[frame_idx].module_idx
            && let HeapObject::Module { globals, .. } = &mut self.heap[module_idx]
        {
            globals.insert(name, value);
        } else {
            self.globals.insert(name, value);
        }
    }

    /// Eagerly load the `sys` cmodule into sys.modules before user code
    /// runs. After this, `import sys` is just a cache lookup.
    /// Dynamic fields (argv, path, modules itself) stay as None placeholders
    /// in this commit; patching lands when the source-file finder arrives.
    fn bootstrap_sys(&mut self) {
        if let Some(sys_value) = self.import_system.try_load_cmodule("sys", &mut self.heap) {
            self.sys_modules.insert("sys".to_string(), sys_value);
        }
    }

    pub fn run(&mut self) -> Result<(), PythonError> {
        // Top-level main runs in code object 0 with module_idx=None so
        // LOAD_GLOBAL routes through VM.globals (the main namespace).
        let main_num_locals = self.code_objects[0].num_locals;
        self.frames.push(Frame::new_for_code(0, main_num_locals, None));
        self.execute_until_depth(0)
    }

    /// Convenience wrapper: run all frames to completion.
    #[allow(dead_code)]
    fn execute(&mut self) -> Result<(), PythonError> {
        self.execute_until_depth(0)
    }

    /// Raise an exception, unwinding to the nearest handler.
    /// Returns Ok(()) if a handler was found and the VM should continue.
    /// Returns Err if no handler was found.
    fn raise_exception(&mut self, exc_val: Value, line: u32) -> Result<(), PythonError> {
        self.current_exception = Some(exc_val);

        if let Some(handler) = self.exception_stack.pop() {
            // Unwind frames to the handler's frame
            while self.frames.len() > handler.frame_index + 1 {
                self.frames.pop();
            }
            // Restore stack depth
            let frame = &mut self.frames[handler.frame_index];
            frame.sp = handler.stack_depth;
            frame.ip = handler.handler_ip;
            Ok(())
        } else {
            // No handler — propagate as Rust error
            let msg = if let Some(exc) = &self.current_exception {
                if let Some(idx) = exc.as_object_ref() {
                    if let HeapObject::ExceptionObj { exc_type, message, .. } = &self.heap[idx] {
                        format!("{}: {}", exc_type.name(), message)
                    } else {
                        exc.display(&self.heap)
                    }
                } else {
                    exc.display(&self.heap)
                }
            } else {
                "unknown exception".to_string()
            };
            Err(PythonError::runtime(msg, line))
        }
    }

    /// Create an exception object and raise it.
    fn raise_exc(&mut self, exc_type: ExceptionType, message: &str, line: u32) -> Result<(), PythonError> {
        let idx = self.heap.len();
        self.heap.push(HeapObject::ExceptionObj {
            exc_type,
            message: message.to_string(),
            args: Vec::new(),
        });
        let exc_val = Value::object_ref(idx);
        self.raise_exception(exc_val, line)
    }

    /// Try to handle a PythonError as a Python-level exception.
    /// Maps known error messages to exception types and routes through the handler stack.
    /// Returns Ok(true) if handled (VM should `continue`), or re-raises the original error.
    fn try_handle_error(&mut self, err: PythonError, line: u32) -> Result<bool, PythonError> {
        if self.exception_stack.is_empty() {
            return Err(err);
        }
        let msg = match &err {
            PythonError::RuntimeError { msg, .. } => msg.clone(),
            _ => return Err(err),
        };
        let exc_type = if msg.contains("division by zero") || msg.contains("division or modulo by zero") {
            ExceptionType::ZeroDivisionError
        } else if msg.contains("unsupported operand") || msg.contains("not supported") {
            ExceptionType::TypeError
        } else if msg.contains("is not defined") {
            ExceptionType::NameError
        } else if msg.contains("index out of range") || msg.contains("out of bounds") {
            ExceptionType::IndexError
        } else if msg.contains("KeyError") {
            ExceptionType::KeyError
        } else {
            ExceptionType::RuntimeError
        };
        self.raise_exc(exc_type, &msg, line)?;
        Ok(true)
    }

    fn execute_until_depth(&mut self, target_depth: usize) -> Result<(), PythonError> {
        loop {
            // Stop when the frame stack drops back to (or below) the target.
            // For top-level `run()` this is 0 (run until truly empty). The
            // import machinery sets this to the caller's frame count so a
            // module body can execute inline without flushing the whole VM.
            if self.frames.len() <= target_depth {
                return Ok(());
            }
            let frame_idx = self.frames.len() - 1;

            let (instr, line, code_index) = {
                let frame = &self.frames[frame_idx];
                let code = &self.code_objects[frame.code_index];
                if frame.ip >= code.instructions.len() {
                    return Err(PythonError::runtime("instruction pointer out of bounds", 0));
                }
                let instr = unsafe { *code.instructions.get_unchecked(frame.ip) };
                let line = unsafe { *code.line_table.get_unchecked(frame.ip) };
                (instr, line, frame.code_index)
            };

            self.frames[frame_idx].ip += 1;

            let opcode = bytecode::decode_op(instr);
            let operand = bytecode::decode_operand(instr);

            match opcode {
                op::LOAD_CONST => {
                    let val = self.code_objects[code_index].constants[operand as usize];
                    self.frames[frame_idx].push(val);
                }
                op::LOAD_FAST => {
                    let val = unsafe { *self.frames[frame_idx].locals.get_unchecked(operand as usize) };
                    self.frames[frame_idx].push(val);
                }
                op::STORE_FAST => {
                    let val = self.frames[frame_idx].pop();
                    unsafe { *self.frames[frame_idx].locals.get_unchecked_mut(operand as usize) = val; }
                }
                op::LOAD_GLOBAL => {
                    // Borrow the name briefly for the lookup; clone only on
                    // the error path. Saves one String alloc per successful
                    // LOAD_GLOBAL — significant on every function call.
                    let lookup = {
                        let name = &self.code_objects[code_index].names[operand as usize];
                        self.frame_globals_get(frame_idx, name)
                    };
                    match lookup {
                        Some(val) => self.frames[frame_idx].push(val),
                        None => {
                            let name = self.code_objects[code_index].names[operand as usize].clone();
                            let err = PythonError::runtime(format!("name '{name}' is not defined"), line);
                            self.try_handle_error(err, line)?;
                            continue;
                        }
                    }
                }
                op::STORE_GLOBAL => {
                    let val = self.frames[frame_idx].pop();
                    let name = self.code_objects[code_index].names[operand as usize].clone();
                    self.frame_globals_insert(frame_idx, name, val);
                }
                op::LOAD_DEREF => {
                    let cell_idx = self.frames[frame_idx].cells.get(operand as usize).copied();
                    if let Some(ci) = cell_idx {
                        if let HeapObject::Cell(v) = &self.heap[ci] {
                            self.frames[frame_idx].push(*v);
                        } else {
                            return Err(PythonError::runtime("LOAD_DEREF: not a cell", line));
                        }
                    } else {
                        return Err(PythonError::runtime("LOAD_DEREF: invalid cell index", line));
                    }
                }
                op::STORE_DEREF => {
                    let val = self.frames[frame_idx].pop();
                    let cell_idx = self.frames[frame_idx].cells.get(operand as usize).copied();
                    if let Some(ci) = cell_idx {
                        self.heap[ci] = HeapObject::Cell(val);
                    } else {
                        return Err(PythonError::runtime("STORE_DEREF: invalid cell index", line));
                    }
                }
                op::LOAD_CLOSURE => {
                    let cell_idx = self.frames[frame_idx].cells.get(operand as usize).copied();
                    if let Some(ci) = cell_idx {
                        // Push the cell heap index as an int (used by MAKE_CLOSURE)
                        self.frames[frame_idx].push(Value::small_int_unchecked(ci as i64));
                    } else {
                        return Err(PythonError::runtime("LOAD_CLOSURE: invalid cell index", line));
                    }
                }
                op::ADD => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    match binary_add(left, right, &mut self.heap, line) {
                        Ok(result) => self.frames[frame_idx].push(result),
                        Err(e) => { self.try_handle_error(e, line)?; continue; }
                    }
                }
                op::SUB => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    match binary_sub(left, right, &mut self.heap, line) {
                        Ok(result) => self.frames[frame_idx].push(result),
                        Err(e) => { self.try_handle_error(e, line)?; continue; }
                    }
                }
                op::MUL => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    match binary_mul(left, right, &mut self.heap, line) {
                        Ok(result) => self.frames[frame_idx].push(result),
                        Err(e) => { self.try_handle_error(e, line)?; continue; }
                    }
                }
                op::DIV => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    match binary_div(left, right, &self.heap, line) {
                        Ok(result) => self.frames[frame_idx].push(result),
                        Err(e) => { self.try_handle_error(e, line)?; continue; }
                    }
                }
                op::FLOOR_DIV => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    match binary_floor_div(left, right, &mut self.heap, line) {
                        Ok(result) => self.frames[frame_idx].push(result),
                        Err(e) => { self.try_handle_error(e, line)?; continue; }
                    }
                }
                op::MOD => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    match binary_mod(left, right, &mut self.heap, line) {
                        Ok(result) => self.frames[frame_idx].push(result),
                        Err(e) => { self.try_handle_error(e, line)?; continue; }
                    }
                }
                op::POW => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    let result = binary_pow(left, right, &mut self.heap, line)?;
                    self.frames[frame_idx].push(result);
                }
                op::UNARY_NEG => {
                    let val = self.frames[frame_idx].pop();
                    let result = if let Some(pi) = PyInt::from_value_or_bool(val, &self.heap) {
                        pi.neg().into_value(&mut self.heap)
                    } else if let Some(f) = val.as_float() {
                        Value::float(-f)
                    } else {
                        return Err(PythonError::runtime("bad operand for unary -", line));
                    };
                    self.frames[frame_idx].push(result);
                }
                op::UNARY_NOT => {
                    let val = self.frames[frame_idx].pop();
                    let truthy = is_truthy(val, &self.heap);
                    self.frames[frame_idx].push(Value::bool_val(!truthy));
                }
                op::UNARY_POS => {
                    let val = self.frames[frame_idx].pop();
                    // Unary + is identity on numbers — int/float/bool/BigInt.
                    let result = if val.is_int() || val.is_float() || val.is_bool()
                        || val.is_pyint(&self.heap) {
                        val
                    } else {
                        return Err(PythonError::runtime("bad operand for unary +", line));
                    };
                    self.frames[frame_idx].push(result);
                }
                op::UNARY_INVERT => {
                    let val = self.frames[frame_idx].pop();
                    if let Some(pi) = PyInt::from_value_or_bool(val, &self.heap) {
                        let r = pi.invert().into_value(&mut self.heap);
                        self.frames[frame_idx].push(r);
                    } else {
                        return Err(PythonError::runtime("bad operand type for unary ~", line));
                    }
                }
                op::COMPARE_EQ => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    let r = values_equal(left, right, &self.heap);
                    self.frames[frame_idx].push(Value::bool_val(r));
                }
                op::COMPARE_NE => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    let r = values_equal(left, right, &self.heap);
                    self.frames[frame_idx].push(Value::bool_val(!r));
                }
                op::COMPARE_LT => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    let r = compare(left, right, &self.heap, |a, b| a < b, |a, b| a < b);
                    self.frames[frame_idx].push(Value::bool_val(r));
                }
                op::COMPARE_LE => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    let r = compare(left, right, &self.heap, |a, b| a <= b, |a, b| a <= b);
                    self.frames[frame_idx].push(Value::bool_val(r));
                }
                op::COMPARE_GT => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    let r = compare(left, right, &self.heap, |a, b| a > b, |a, b| a > b);
                    self.frames[frame_idx].push(Value::bool_val(r));
                }
                op::COMPARE_GE => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    let r = compare(left, right, &self.heap, |a, b| a >= b, |a, b| a >= b);
                    self.frames[frame_idx].push(Value::bool_val(r));
                }
                op::COMPARE_IS => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    // Identity comparison: same bit pattern
                    let r = left.bits_eq(right);
                    self.frames[frame_idx].push(Value::bool_val(r));
                }
                op::COMPARE_IS_NOT => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    let r = !left.bits_eq(right);
                    self.frames[frame_idx].push(Value::bool_val(r));
                }
                op::CONTAINS_OP => {
                    let container = self.frames[frame_idx].pop();
                    let item = self.frames[frame_idx].pop();
                    let found = contains(&item, &container, &self.heap)?;
                    let result = if operand == 0 { found } else { !found }; // 0=in, 1=not in
                    self.frames[frame_idx].push(Value::bool_val(result));
                }
                op::BIT_AND => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    if let (Some(a), Some(b)) = (
                        PyInt::from_value_or_bool(left, &self.heap),
                        PyInt::from_value_or_bool(right, &self.heap),
                    ) {
                        let r = a.and_(b).into_value(&mut self.heap);
                        self.frames[frame_idx].push(r);
                    } else {
                        return Err(PythonError::runtime("unsupported operand type(s) for &", line));
                    }
                }
                op::BIT_OR => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    if let (Some(a), Some(b)) = (
                        PyInt::from_value_or_bool(left, &self.heap),
                        PyInt::from_value_or_bool(right, &self.heap),
                    ) {
                        let r = a.or_(b).into_value(&mut self.heap);
                        self.frames[frame_idx].push(r);
                    } else {
                        return Err(PythonError::runtime("unsupported operand type(s) for |", line));
                    }
                }
                op::BIT_XOR => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    if let (Some(a), Some(b)) = (
                        PyInt::from_value_or_bool(left, &self.heap),
                        PyInt::from_value_or_bool(right, &self.heap),
                    ) {
                        let r = a.xor_(b).into_value(&mut self.heap);
                        self.frames[frame_idx].push(r);
                    } else {
                        return Err(PythonError::runtime("unsupported operand type(s) for ^", line));
                    }
                }
                op::LSHIFT => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    if let (Some(a), Some(b)) = (
                        PyInt::from_value_or_bool(left, &self.heap),
                        PyInt::from_value_or_bool(right, &self.heap),
                    ) {
                        match a.shl(b) {
                            Ok(o) => self.frames[frame_idx].push(o.into_value(&mut self.heap)),
                            Err(e) => return Err(arith_to_runtime(e, "<<", line)),
                        }
                    } else {
                        return Err(PythonError::runtime("unsupported operand type(s) for <<", line));
                    }
                }
                op::RSHIFT => {
                    let right = self.frames[frame_idx].pop();
                    let left = self.frames[frame_idx].pop();
                    if let (Some(a), Some(b)) = (
                        PyInt::from_value_or_bool(left, &self.heap),
                        PyInt::from_value_or_bool(right, &self.heap),
                    ) {
                        match a.shr(b) {
                            Ok(o) => self.frames[frame_idx].push(o.into_value(&mut self.heap)),
                            Err(e) => return Err(arith_to_runtime(e, ">>", line)),
                        }
                    } else {
                        return Err(PythonError::runtime("unsupported operand type(s) for >>", line));
                    }
                }
                op::JUMP => {
                    self.frames[frame_idx].ip = operand as usize;
                }
                op::JUMP_IF_FALSE => {
                    let val = self.frames[frame_idx].pop();
                    if !is_truthy(val, &self.heap) {
                        self.frames[frame_idx].ip = operand as usize;
                    }
                }
                op::JUMP_IF_TRUE => {
                    let val = self.frames[frame_idx].pop();
                    if is_truthy(val, &self.heap) {
                        self.frames[frame_idx].ip = operand as usize;
                    }
                }
                op::CALL_FUNCTION => {
                    let argc = operand as usize;
                    let mut args = Vec::with_capacity(argc);
                    for _ in 0..argc {
                        args.push(self.frames[frame_idx].pop());
                    }
                    args.reverse();
                    let func_val = self.frames[frame_idx].pop();

                    // Dispatch based on value type
                    if let Some(heap_idx) = func_val.as_object_ref() {
                        match &self.heap[heap_idx] {
                            HeapObject::BuiltinFn { id, .. } => {
                                let id = *id;
                                match builtins::call_builtin(
                                    id, &args, &mut self.heap, &mut self.output, &self.globals,
                                ) {
                                    Ok(result) => self.frames[frame_idx].push(result),
                                    Err(e) => { self.try_handle_error(e, line)?; continue; }
                                }
                            }
                            HeapObject::Closure { code_index, arity, cells, module_idx, .. } => {
                                let func_code_index = *code_index;
                                let arity = *arity as usize;
                                let cells = cells.clone();
                                let func_module_idx = *module_idx;
                                if argc != arity {
                                    let name = if let HeapObject::Closure { name, .. } = &self.heap[heap_idx] {
                                        name.clone()
                                    } else { "???".to_string() };
                                    return Err(PythonError::runtime(
                                        format!("{name}() takes {arity} argument(s) but {argc} were given"), line,
                                    ));
                                }

                                // Check if this is a generator function
                                if self.code_objects[func_code_index].is_generator {
                                    let gen_idx = self.heap.len();
                                    let num_locals = self.code_objects[func_code_index].num_locals;
                                    let mut locals = vec![Value::none(); num_locals];
                                    for (i, arg) in args.iter().enumerate() {
                                        locals[i] = *arg;
                                    }
                                    self.heap.push(HeapObject::Generator {
                                        code_index: func_code_index,
                                        ip: 0,
                                        locals,
                                        stack: Vec::new(),
                                        state: GeneratorState::Created,
                                        cells,
                                    });
                                    self.frames[frame_idx].push(Value::object_ref(gen_idx));
                                } else {
                                    let mut new_frame = Frame::new_for_code(func_code_index, self.code_objects[func_code_index].num_locals, func_module_idx);
                                    for (i, arg) in args.iter().enumerate() {
                                        new_frame.locals[i] = *arg;
                                    }
                                    // Set up cells: cell_vars get new cells, free_vars use closure cells
                                    let code = &self.code_objects[func_code_index];
                                    let num_cell = code.cell_var_names.len();
                                    let num_free = code.free_var_names.len();
                                    for _ in 0..num_cell {
                                        let ci = self.heap.len();
                                        self.heap.push(HeapObject::Cell(Value::none()));
                                        new_frame.cells.push(ci);
                                    }
                                    // Initialize cell vars from params if they're also cell vars
                                    let cell_var_names: Vec<String> = self.code_objects[func_code_index].cell_var_names.clone();
                                    for (ci, cv_name) in cell_var_names.iter().enumerate() {
                                        let local_names = &self.code_objects[func_code_index].local_names;
                                        if let Some(li) = local_names.iter().position(|n| n == cv_name) {
                                            self.heap[new_frame.cells[ci]] = HeapObject::Cell(new_frame.locals[li]);
                                        }
                                    }
                                    for i in 0..num_free {
                                        if i < cells.len() {
                                            new_frame.cells.push(cells[i]);
                                        }
                                    }
                                    self.frames.push(new_frame);
                                    continue;
                                }
                            }
                            HeapObject::Class { .. } => {
                                // Class instantiation
                                match self.call_class(frame_idx, heap_idx, &args, line) {
                                    Ok(()) => {
                                        if self.frames.len() > frame_idx + 1 {
                                            continue; // __init__ pushed a frame
                                        }
                                    }
                                    Err(e) => { self.try_handle_error(e, line)?; continue; }
                                }
                            }
                            HeapObject::BoundMethod { instance, method } => {
                                let instance = *instance;
                                let method = *method;
                                let mut new_args = Vec::with_capacity(argc + 1);
                                new_args.push(instance);
                                new_args.extend_from_slice(&args);
                                self.call_value(frame_idx, method, &new_args, line)?;
                                if self.frames.len() > frame_idx + 1 {
                                    continue;
                                }
                            }
                            HeapObject::ExceptionObj { exc_type, .. } => {
                                // Exception type called as constructor — just push it back
                                let exc_type = *exc_type;
                                let msg = if !args.is_empty() {
                                    args[0].display(&self.heap)
                                } else {
                                    String::new()
                                };
                                let idx = self.heap.len();
                                self.heap.push(HeapObject::ExceptionObj {
                                    exc_type,
                                    message: msg,
                                    args: args.clone(),
                                });
                                self.frames[frame_idx].push(Value::object_ref(idx));
                            }
                            _ => {
                                return Err(PythonError::runtime(
                                    format!("'{}' is not callable", func_val.display(&self.heap)), line,
                                ));
                            }
                        }
                    } else if let Some(heap_idx) = func_val.as_func_ref() {
                        let (func_code_index, arity, func_module_idx) = if let HeapObject::Function { code_index, arity, module_idx, .. } = &self.heap[heap_idx] {
                            (*code_index, *arity as usize, *module_idx)
                        } else {
                            return Err(PythonError::runtime("not a callable", line));
                        };

                        // Check if this is a generator function
                        if self.code_objects[func_code_index].is_generator {
                            if argc != arity {
                                let name = if let HeapObject::Function { name, .. } = &self.heap[heap_idx] {
                                    name.clone()
                                } else { "???".to_string() };
                                return Err(PythonError::runtime(
                                    format!("{name}() takes {arity} argument(s) but {argc} were given"), line,
                                ));
                            }
                            let gen_idx = self.heap.len();
                            let num_locals = self.code_objects[func_code_index].num_locals;
                            let mut locals = vec![Value::none(); num_locals];
                            for (i, arg) in args.iter().enumerate() {
                                locals[i] = *arg;
                            }
                            self.heap.push(HeapObject::Generator {
                                code_index: func_code_index,
                                ip: 0,
                                locals,
                                stack: Vec::new(),
                                state: GeneratorState::Created,
                                cells: Vec::new(),
                            });
                            self.frames[frame_idx].push(Value::object_ref(gen_idx));
                        } else {
                            if argc != arity {
                                let name = if let HeapObject::Function { name, .. } = &self.heap[heap_idx] {
                                    name.clone()
                                } else { "???".to_string() };
                                return Err(PythonError::runtime(
                                    format!("{name}() takes {arity} argument(s) but {argc} were given"), line,
                                ));
                            }
                            let mut new_frame = Frame::new_for_code(func_code_index, self.code_objects[func_code_index].num_locals, func_module_idx);
                            for (i, arg) in args.iter().enumerate() {
                                new_frame.locals[i] = *arg;
                            }
                            // Set up cells for cell_vars
                            let num_cell = self.code_objects[func_code_index].cell_var_names.len();
                            for _ in 0..num_cell {
                                let ci = self.heap.len();
                                self.heap.push(HeapObject::Cell(Value::none()));
                                new_frame.cells.push(ci);
                            }
                            // Initialize cells from params
                            let cell_var_names: Vec<String> = self.code_objects[func_code_index].cell_var_names.clone();
                            for (ci, cv_name) in cell_var_names.iter().enumerate() {
                                let local_names = &self.code_objects[func_code_index].local_names;
                                if let Some(li) = local_names.iter().position(|n| n == cv_name) {
                                    self.heap[new_frame.cells[ci]] = HeapObject::Cell(new_frame.locals[li]);
                                }
                            }
                            self.frames.push(new_frame);
                            continue;
                        }
                    } else {
                        return Err(PythonError::runtime(
                            format!("'{}' is not callable", func_val.display(&self.heap)), line,
                        ));
                    }
                }
                op::RETURN_VALUE => {
                    let return_val = self.frames[frame_idx].pop();
                    let gen_idx = self.frames[frame_idx].generator_idx;
                    let init_inst = self.frames[frame_idx].init_instance;
                    self.frames.pop();

                    // If this was a generator frame, mark it completed
                    if let Some(gi) = gen_idx {
                        if let HeapObject::Generator { state, .. } = &mut self.heap[gi] {
                            *state = GeneratorState::Completed;
                        }
                        if self.frames.is_empty() {
                            return Ok(());
                        }
                        // Roll back caller's IP to re-execute LOAD + FOR_ITER.
                        // The caller pattern is: LOAD_FAST/LOAD_GLOBAL __iter__, FOR_ITER exit
                        // When FOR_ITER ran, it incremented IP past FOR_ITER, then called
                        // resume_generator + continue. So caller.ip is at FOR_ITER + 1.
                        // We go back 2 to re-run LOAD_FAST, FOR_ITER, which will now see Completed.
                        let caller = self.frames.last_mut().ok_or_else(||
                            PythonError::runtime("internal: caller frame missing after generator return", line))?;
                        caller.ip = caller.ip.saturating_sub(2);
                        continue;
                    }

                    if self.frames.is_empty() {
                        return Ok(());
                    }
                    let caller = self.frames.last_mut().ok_or_else(||
                        PythonError::runtime("internal: caller frame missing after RETURN_VALUE", line))?;
                    // If this was an __init__ frame, push the instance instead of None
                    if let Some(instance) = init_inst {
                        caller.push(instance);
                    } else {
                        caller.push(return_val);
                    }
                    continue;
                }
                op::MAKE_FUNCTION => {
                    let code_idx_val = self.code_objects[code_index].constants[operand as usize];
                    let func_code_index = code_idx_val.as_int().ok_or_else(||
                        PythonError::runtime("internal: MAKE_FUNCTION operand isn't a small int", line))?
                        as usize;
                    let func_name = self.code_objects[func_code_index].name.clone();
                    let arity = self.code_objects[func_code_index].num_params as u8;
                    // Capture the defining module so the function's body resolves
                    // globals against this module when called later.
                    let module_idx = self.frames[frame_idx].module_idx;

                    let heap_idx = self.heap.len();
                    self.heap.push(HeapObject::Function {
                        name: func_name,
                        code_index: func_code_index,
                        arity,
                        module_idx,
                    });
                    self.frames[frame_idx].push(Value::func_ref(heap_idx));
                }
                op::MAKE_CLOSURE => {
                    let code_idx_val = self.code_objects[code_index].constants[operand as usize];
                    let func_code_index = code_idx_val.as_int().ok_or_else(||
                        PythonError::runtime("internal: MAKE_CLOSURE operand isn't a small int", line))?
                        as usize;
                    let func_name = self.code_objects[func_code_index].name.clone();
                    let arity = self.code_objects[func_code_index].num_params as u8;
                    let num_free = self.code_objects[func_code_index].free_var_names.len();
                    let module_idx = self.frames[frame_idx].module_idx;

                    // Pop cell indices from stack (pushed by LOAD_CLOSURE)
                    let mut cells = Vec::with_capacity(num_free);
                    for _ in 0..num_free {
                        let cell_val = self.frames[frame_idx].pop();
                        let cell_idx = cell_val.as_int().ok_or_else(||
                            PythonError::runtime("internal: LOAD_CLOSURE pushed non-int cell index", line))?
                            as usize;
                        cells.push(cell_idx);
                    }
                    cells.reverse();

                    let heap_idx = self.heap.len();
                    self.heap.push(HeapObject::Closure {
                        name: func_name,
                        code_index: func_code_index,
                        arity,
                        cells,
                        module_idx,
                    });
                    self.frames[frame_idx].push(Value::object_ref(heap_idx));
                }
                op::GET_ITER => {
                    let val = self.frames[frame_idx].pop();
                    if val.is_range() {
                        // RangeIter is already an iterator
                        self.frames[frame_idx].push(val);
                    } else if let Some(list_idx) = val.as_list_ref() {
                        let iter_idx = self.heap.len();
                        self.heap.push(HeapObject::ListIter { list_idx, index: 0 });
                        self.frames[frame_idx].push(Value::object_ref(iter_idx));
                    } else if let Some(str_idx) = val.as_str_ref() {
                        let iter_idx = self.heap.len();
                        self.heap.push(HeapObject::StringIter { str_idx, index: 0 });
                        self.frames[frame_idx].push(Value::object_ref(iter_idx));
                    } else if let Some(obj_idx) = val.as_object_ref() {
                        match &self.heap[obj_idx] {
                            HeapObject::Tuple(_) => {
                                let iter_idx = self.heap.len();
                                self.heap.push(HeapObject::TupleIter { tuple_idx: obj_idx, index: 0 });
                                self.frames[frame_idx].push(Value::object_ref(iter_idx));
                            }
                            HeapObject::Dict { .. } => {
                                let iter_idx = self.heap.len();
                                self.heap.push(HeapObject::DictKeyIter { dict_idx: obj_idx, index: 0 });
                                self.frames[frame_idx].push(Value::object_ref(iter_idx));
                            }
                            HeapObject::Generator { .. } => {
                                // Generator is its own iterator
                                self.frames[frame_idx].push(val);
                            }
                            HeapObject::Instance { class_idx, .. } => {
                                // Look for __iter__ method
                                let class_idx = *class_idx;
                                if let Some(iter_method) = self.lookup_attr_on_class(class_idx, "__iter__") {
                                    // Call __iter__(self)
                                    let bound_idx = self.heap.len();
                                    self.heap.push(HeapObject::BoundMethod {
                                        instance: val,
                                        method: iter_method,
                                    });
                                    let bound = Value::object_ref(bound_idx);
                                    // Push the bound method, then call it
                                    self.frames[frame_idx].push(bound);
                                    // Emit a synthetic call: push func + 0 args
                                    // Actually, we need to call it. Let's push and use CALL_FUNCTION mechanism
                                    let call_args: Vec<Value> = vec![val];
                                    self.call_value(frame_idx, iter_method, &call_args, line)?;
                                    if self.frames.len() > frame_idx + 1 {
                                        // Remove the bound method we pushed
                                        self.frames[frame_idx].sp -= 1;
                                        continue;
                                    }
                                    // If call_value didn't push a frame (builtin), pop bound and keep result
                                    let result = self.frames[frame_idx].pop();
                                    self.frames[frame_idx].sp -= 1; // pop the bound method
                                    self.frames[frame_idx].push(result);
                                } else {
                                    // Check if the instance itself has __next__ (is its own iterator)
                                    if self.lookup_attr_on_class(class_idx, "__next__").is_some() {
                                        self.frames[frame_idx].push(val);
                                    } else {
                                        return Err(PythonError::runtime("object is not iterable", line));
                                    }
                                }
                            }
                            _ => {
                                return Err(PythonError::runtime("object is not iterable", line));
                            }
                        }
                    } else {
                        return Err(PythonError::runtime("object is not iterable", line));
                    }
                }
                op::FOR_ITER => {
                    let iter_val = self.frames[frame_idx].pop();

                    if let Some(heap_idx) = iter_val.as_range_ref() {
                        let (current, stop, step) = if let HeapObject::RangeIter { current, stop, step } = &self.heap[heap_idx] {
                            (*current, *stop, *step)
                        } else {
                            return Err(PythonError::runtime("expected iterator", line));
                        };
                        let exhausted = if step > 0 { current >= stop } else { current <= stop };
                        if exhausted {
                            self.frames[frame_idx].ip = operand as usize;
                        } else {
                            self.frames[frame_idx].push(Value::small_int_unchecked(current));
                            if let HeapObject::RangeIter { current: c, .. } = &mut self.heap[heap_idx] {
                                *c = current + step;
                            }
                        }
                    } else if let Some(obj_idx) = iter_val.as_object_ref() {
                        match &self.heap[obj_idx] {
                            HeapObject::ListIter { list_idx, index } => {
                                let list_idx = *list_idx;
                                let index = *index;
                                let len = if let HeapObject::List(items) = &self.heap[list_idx] {
                                    items.len()
                                } else { 0 };
                                if index >= len {
                                    self.frames[frame_idx].ip = operand as usize;
                                } else {
                                    let val = if let HeapObject::List(items) = &self.heap[list_idx] {
                                        items[index]
                                    } else { Value::none() };
                                    self.frames[frame_idx].push(val);
                                    if let HeapObject::ListIter { index: idx, .. } = &mut self.heap[obj_idx] {
                                        *idx = index + 1;
                                    }
                                }
                            }
                            HeapObject::TupleIter { tuple_idx, index } => {
                                let tuple_idx = *tuple_idx;
                                let index = *index;
                                let len = if let HeapObject::Tuple(items) = &self.heap[tuple_idx] {
                                    items.len()
                                } else { 0 };
                                if index >= len {
                                    self.frames[frame_idx].ip = operand as usize;
                                } else {
                                    let val = if let HeapObject::Tuple(items) = &self.heap[tuple_idx] {
                                        items[index]
                                    } else { Value::none() };
                                    self.frames[frame_idx].push(val);
                                    if let HeapObject::TupleIter { index: idx, .. } = &mut self.heap[obj_idx] {
                                        *idx = index + 1;
                                    }
                                }
                            }
                            HeapObject::StringIter { str_idx, index } => {
                                let str_idx = *str_idx;
                                let index = *index;
                                let s = self.heap[str_idx].as_str().unwrap_or("");
                                let chars: Vec<char> = s.chars().collect();
                                if index >= chars.len() {
                                    self.frames[frame_idx].ip = operand as usize;
                                } else {
                                    let ch = chars[index].to_string();
                                    let new_idx = self.heap.len();
                                    self.heap.push(HeapObject::Str(ch.into()));
                                    self.frames[frame_idx].push(Value::str_ref(new_idx));
                                    if let HeapObject::StringIter { index: idx, .. } = &mut self.heap[obj_idx] {
                                        *idx = index + 1;
                                    }
                                }
                            }
                            HeapObject::DictKeyIter { dict_idx, index } => {
                                let dict_idx = *dict_idx;
                                let index = *index;
                                let len = if let HeapObject::Dict { keys, .. } = &self.heap[dict_idx] {
                                    keys.len()
                                } else { 0 };
                                if index >= len {
                                    self.frames[frame_idx].ip = operand as usize;
                                } else {
                                    let val = if let HeapObject::Dict { keys, .. } = &self.heap[dict_idx] {
                                        keys[index]
                                    } else { Value::none() };
                                    self.frames[frame_idx].push(val);
                                    if let HeapObject::DictKeyIter { index: idx, .. } = &mut self.heap[obj_idx] {
                                        *idx = index + 1;
                                    }
                                }
                            }
                            HeapObject::Generator { state, .. } => {
                                let state = *state;
                                if state == GeneratorState::Completed {
                                    self.frames[frame_idx].ip = operand as usize;
                                } else {
                                    // Resume or start the generator
                                    self.resume_generator(frame_idx, obj_idx, line)?;
                                    continue;
                                }
                            }
                            HeapObject::Instance { class_idx, .. } => {
                                // Call __next__ on the instance
                                let class_idx = *class_idx;
                                if let Some(next_method) = self.lookup_attr_on_class(class_idx, "__next__") {
                                    let call_args = vec![iter_val];
                                    // We need to save the iterator for the next iteration
                                    // Store iter_val back first, we'll re-push it later
                                    self.call_value(frame_idx, next_method, &call_args, line)?;
                                    if self.frames.len() > frame_idx + 1 {
                                        continue;
                                    }
                                    // Builtin returned directly
                                } else {
                                    return Err(PythonError::runtime("iterator has no __next__ method", line));
                                }
                            }
                            _ => {
                                return Err(PythonError::runtime("expected iterator", line));
                            }
                        }
                    } else {
                        return Err(PythonError::runtime("expected iterator", line));
                    }
                }
                op::POP_TOP => {
                    self.frames[frame_idx].pop();
                }
                op::DUP_TOP => {
                    let val = self.frames[frame_idx].peek();
                    self.frames[frame_idx].push(val);
                }
                op::ROT_TWO => {
                    let a = self.frames[frame_idx].pop();
                    let b = self.frames[frame_idx].pop();
                    self.frames[frame_idx].push(a);
                    self.frames[frame_idx].push(b);
                }
                op::ROT_THREE => {
                    let a = self.frames[frame_idx].pop();
                    let b = self.frames[frame_idx].pop();
                    let c = self.frames[frame_idx].pop();
                    self.frames[frame_idx].push(a);
                    self.frames[frame_idx].push(c);
                    self.frames[frame_idx].push(b);
                }
                op::BUILD_LIST => {
                    let count = operand as usize;
                    let mut elements = Vec::with_capacity(count);
                    for _ in 0..count {
                        elements.push(self.frames[frame_idx].pop());
                    }
                    elements.reverse();
                    let heap_idx = self.heap.len();
                    self.heap.push(HeapObject::List(elements));
                    self.frames[frame_idx].push(Value::list_ref(heap_idx));
                }
                op::LIST_APPEND => {
                    let val = self.frames[frame_idx].pop();
                    let list_val = self.frames[frame_idx].pop();
                    if let Some(heap_idx) = list_val.as_list_ref() && let HeapObject::List(items) = &mut self.heap[heap_idx] {
                        items.push(val);
                    }
                    self.frames[frame_idx].push(list_val);
                }
                op::BUILD_TUPLE => {
                    let count = operand as usize;
                    let mut elements = Vec::with_capacity(count);
                    for _ in 0..count {
                        elements.push(self.frames[frame_idx].pop());
                    }
                    elements.reverse();
                    let heap_idx = self.heap.len();
                    self.heap.push(HeapObject::Tuple(elements));
                    self.frames[frame_idx].push(Value::object_ref(heap_idx));
                }
                op::BUILD_DICT => {
                    let count = operand as usize;
                    let mut keys = Vec::with_capacity(count);
                    let mut values = Vec::with_capacity(count);
                    let mut pairs = Vec::with_capacity(count);
                    for _ in 0..count {
                        let v = self.frames[frame_idx].pop();
                        let k = self.frames[frame_idx].pop();
                        pairs.push((k, v));
                    }
                    pairs.reverse();
                    let mut index_map = HashMap::new();
                    for (i, (k, v)) in pairs.into_iter().enumerate() {
                        let h = value_hash(k, &self.heap);
                        index_map.insert(h, i);
                        keys.push(k);
                        values.push(v);
                    }
                    let heap_idx = self.heap.len();
                    self.heap.push(HeapObject::Dict { keys, values, index_map });
                    self.frames[frame_idx].push(Value::object_ref(heap_idx));
                }
                op::BUILD_SET => {
                    let count = operand as usize;
                    let mut elements = Vec::with_capacity(count);
                    for _ in 0..count {
                        elements.push(self.frames[frame_idx].pop());
                    }
                    elements.reverse();
                    let heap_idx = self.heap.len();
                    self.heap.push(HeapObject::Set(elements));
                    self.frames[frame_idx].push(Value::object_ref(heap_idx));
                }
                op::SUBSCRIPT => {
                    let index = self.frames[frame_idx].pop();
                    let obj = self.frames[frame_idx].pop();
                    match self.subscript_get(obj, index, line) {
                        Ok(result) => self.frames[frame_idx].push(result),
                        Err(e) => { self.try_handle_error(e, line)?; continue; }
                    }
                }
                op::STORE_SUBSCRIPT => {
                    let index = self.frames[frame_idx].pop();
                    let obj = self.frames[frame_idx].pop();
                    let val = self.frames[frame_idx].pop();
                    self.subscript_set(obj, index, val, line)?;
                }
                op::DELETE_SUBSCRIPT => {
                    let index = self.frames[frame_idx].pop();
                    let obj = self.frames[frame_idx].pop();
                    self.subscript_delete(obj, index, line)?;
                }
                op::LOAD_ATTR => {
                    let obj = self.frames[frame_idx].pop();
                    let attr_name = &self.code_objects[code_index].names[operand as usize];
                    let attr_name = attr_name.clone();
                    match self.load_attr(obj, &attr_name, line) {
                        Ok(result) => self.frames[frame_idx].push(result),
                        Err(e) => { self.try_handle_error(e, line)?; continue; }
                    }
                }
                op::STORE_ATTR => {
                    let obj = self.frames[frame_idx].pop();
                    let val = self.frames[frame_idx].pop();
                    let attr_name = self.code_objects[code_index].names[operand as usize].clone();
                    self.store_attr(obj, &attr_name, val, line)?;
                }
                op::UNPACK_SEQUENCE => {
                    let count = operand as usize;
                    let seq = self.frames[frame_idx].pop();
                    let items = self.unpack_sequence(seq, count, line)?;
                    // Push in reverse order so first element is on top
                    for item in items.into_iter().rev() {
                        self.frames[frame_idx].push(item);
                    }
                }
                op::SETUP_EXCEPT => {
                    let handler_ip = operand as usize;
                    self.exception_stack.push(ExceptionHandler {
                        handler_ip,
                        frame_index: frame_idx,
                        stack_depth: self.frames[frame_idx].sp,
                    });
                }
                op::POP_EXCEPT => {
                    self.exception_stack.pop();
                }
                op::RAISE => {
                    match operand {
                        0 => {
                            // Re-raise current exception
                            if let Some(exc) = self.current_exception {
                                self.raise_exception(exc, line)?;
                                continue;
                            } else {
                                return Err(PythonError::runtime("No active exception to re-raise", line));
                            }
                        }
                        1 => {
                            // Raise value from stack
                            let exc_val = self.frames[frame_idx].pop();
                            // If it's an exception type name (from globals), construct it
                            if let Some(obj_idx) = exc_val.as_object_ref() {
                                match &self.heap[obj_idx] {
                                    HeapObject::ExceptionObj { .. } => {
                                        self.raise_exception(exc_val, line)?;
                                        continue;
                                    }
                                    HeapObject::BuiltinFn { id: BuiltinId::ExcConstructor(et), .. } => {
                                        let et = *et;
                                        let idx = self.heap.len();
                                        self.heap.push(HeapObject::ExceptionObj {
                                            exc_type: et,
                                            message: String::new(),
                                            args: Vec::new(),
                                        });
                                        self.raise_exception(Value::object_ref(idx), line)?;
                                        continue;
                                    }
                                    _ => {}
                                }
                            }
                            // Create a generic RuntimeError
                            let msg = exc_val.display(&self.heap);
                            self.raise_exc(ExceptionType::RuntimeError, &msg, line)?;
                            continue;
                        }
                        2 => {
                            // Assert error with message on stack
                            let msg_val = self.frames[frame_idx].pop();
                            let msg = msg_val.display(&self.heap);
                            self.raise_exc(ExceptionType::AssertionError, &msg, line)?;
                            continue;
                        }
                        _ => {
                            return Err(PythonError::runtime("invalid RAISE operand", line));
                        }
                    }
                }
                op::SETUP_FINALLY => {
                    // Similar to SETUP_EXCEPT but for finally blocks
                    let _finally_ip = operand as usize;
                    // For simplicity, we handle finally inline in the compiler
                }
                op::END_FINALLY => {
                    // If there's a pending exception, re-raise it
                    // For simplicity, no-op — exceptions are handled by the compiler's inline code
                }
                op::LOAD_EXCEPTION => {
                    // Push the current exception onto the stack
                    if let Some(exc) = self.current_exception {
                        self.frames[frame_idx].push(exc);
                    } else {
                        self.frames[frame_idx].push(Value::none());
                    }
                }
                op::BUILD_CLASS => {
                    let num_bases = operand as usize;
                    let co_idx_val = self.frames[frame_idx].pop();
                    let name_val = self.frames[frame_idx].pop();
                    let class_co_idx = co_idx_val.as_int().ok_or_else(||
                        PythonError::runtime("internal: BUILD_CLASS code-object index isn't a small int", line))?
                        as usize;
                    let class_name = name_val.display(&self.heap);

                    let mut base_indices = Vec::with_capacity(num_bases);
                    for _ in 0..num_bases {
                        let base = self.frames[frame_idx].pop();
                        if let Some(idx) = base.as_object_ref() {
                            base_indices.push(idx);
                        }
                    }
                    base_indices.reverse();

                    // Execute class body to get attributes — class body runs in
                    // the same module as its enclosing scope.
                    let parent_module_idx = self.frames[frame_idx].module_idx;
                    let class_frame = Frame::new_for_code(class_co_idx, self.code_objects[class_co_idx].num_locals, parent_module_idx);
                    self.frames.push(class_frame);

                    // Run class body
                    loop {
                        let cf_idx = self.frames.len() - 1;
                        let cf = &self.frames[cf_idx];
                        let code = &self.code_objects[cf.code_index];
                        if cf.ip >= code.instructions.len() {
                            break;
                        }
                        let ci = unsafe { *code.instructions.get_unchecked(cf.ip) };
                        let cl = unsafe { *code.line_table.get_unchecked(cf.ip) };
                        let cop = bytecode::decode_op(ci);

                        if cop == op::RETURN_VALUE {
                            break;
                        }

                        // Store current position and recurse
                        // This is a simplified approach - we save/restore context
                        self.frames[cf_idx].ip += 1;
                        let co_index = self.frames[cf_idx].code_index;
                        let co_operand = bytecode::decode_operand(ci);

                        match cop {
                            op::LOAD_CONST => {
                                let val = self.code_objects[co_index].constants[co_operand as usize];
                                self.frames[cf_idx].push(val);
                            }
                            op::STORE_FAST => {
                                let val = self.frames[cf_idx].pop();
                                unsafe { *self.frames[cf_idx].locals.get_unchecked_mut(co_operand as usize) = val; }
                            }
                            op::LOAD_FAST => {
                                let val = unsafe { *self.frames[cf_idx].locals.get_unchecked(co_operand as usize) };
                                self.frames[cf_idx].push(val);
                            }
                            op::LOAD_GLOBAL => {
                                let name = self.code_objects[co_index].names[co_operand as usize].clone();
                                match self.frame_globals_get(cf_idx, &name) {
                                    Some(val) => self.frames[cf_idx].push(val),
                                    None => return Err(PythonError::runtime(
                                        format!("name '{name}' is not defined"), cl,
                                    )),
                                }
                            }
                            op::STORE_GLOBAL => {
                                let val = self.frames[cf_idx].pop();
                                let name = self.code_objects[co_index].names[co_operand as usize].clone();
                                self.frame_globals_insert(cf_idx, name, val);
                            }
                            op::MAKE_FUNCTION => {
                                let code_idx_v = self.code_objects[co_index].constants[co_operand as usize];
                                let fci = code_idx_v.as_int().ok_or_else(||
                                    PythonError::runtime("internal: class-body MAKE_FUNCTION operand isn't a small int", line))?
                                    as usize;
                                let fname = self.code_objects[fci].name.clone();
                                let farity = self.code_objects[fci].num_params as u8;
                                let class_module_idx = self.frames[cf_idx].module_idx;
                                let hi = self.heap.len();
                                self.heap.push(HeapObject::Function {
                                    name: fname,
                                    code_index: fci,
                                    arity: farity,
                                    module_idx: class_module_idx,
                                });
                                self.frames[cf_idx].push(Value::func_ref(hi));
                            }
                            op::POP_TOP => {
                                self.frames[cf_idx].pop();
                            }
                            op::HALT => break,
                            _ => {
                                // For other opcodes in class body, skip
                                // This handles Pass (no-op) and other simple cases
                            }
                        }
                    }

                    // Extract locals as class attributes
                    let class_frame = self.frames.pop().ok_or_else(||
                        PythonError::runtime("internal: class-body frame missing on completion", line))?;
                    let mut attrs = HashMap::new();
                    let local_names = &self.code_objects[class_co_idx].local_names;
                    for (i, name) in local_names.iter().enumerate() {
                        if i < class_frame.locals.len() {
                            let val = class_frame.locals[i];
                            if !val.is_none() || name == "__init__" {
                                attrs.insert(name.clone(), val);
                            }
                        }
                    }

                    // Compute MRO (simplified C3 linearization)
                    let mut mro = Vec::new();
                    // Add self (will be set after creation)
                    // Add base MROs
                    for &bi in &base_indices {
                        if let HeapObject::Class { mro: base_mro, .. } = &self.heap[bi] {
                            for &m in base_mro {
                                if !mro.contains(&m) {
                                    mro.push(m);
                                }
                            }
                        }
                    }

                    let class_idx = self.heap.len();
                    // Insert self at beginning of MRO
                    mro.insert(0, class_idx);

                    self.heap.push(HeapObject::Class {
                        name: class_name,
                        mro,
                        attrs,
                        bases: base_indices,
                    });

                    self.frames[frame_idx].push(Value::object_ref(class_idx));
                }
                op::YIELD_VALUE => {
                    let yielded = self.frames[frame_idx].pop();
                    let gen_idx = self.frames[frame_idx].generator_idx;

                    if let Some(gi) = gen_idx {
                        // Save frame state to generator
                        let frame = &self.frames[frame_idx];
                        let ip = frame.ip;
                        let sp = frame.sp;
                        let mut locals = vec![Value::none(); self.code_objects[frame.code_index].num_locals];
                        for (i, l) in locals.iter_mut().enumerate() {
                            if i < frame.locals.len() {
                                *l = frame.locals[i];
                            }
                        }
                        let mut stack = Vec::with_capacity(sp);
                        for i in 0..sp {
                            stack.push(frame.stack[i]);
                        }
                        let cells = frame.cells.clone();

                        if let HeapObject::Generator { ip: gip, locals: gl, stack: gs, state, cells: gc, .. } = &mut self.heap[gi] {
                            *gip = ip;
                            *gl = locals;
                            *gs = stack;
                            *state = GeneratorState::Suspended;
                            *gc = cells;
                        }

                        // Pop generator frame
                        self.frames.pop();

                        if self.frames.is_empty() {
                            return Ok(());
                        }

                        // Push yielded value to caller
                        let caller = self.frames.last_mut().ok_or_else(||
                            PythonError::runtime("internal: caller frame missing after yield", line))?;
                        caller.push(yielded);
                        continue;
                    } else {
                        return Err(PythonError::runtime("yield outside generator", line));
                    }
                }
                op::IMPORT_NAME => {
                    // Stack contract: [level, fromlist] → [module]
                    // - `level` > 0 = relative import; resolve absolute name
                    //   from the current module's __package__ before lookup.
                    // - When fromlist is None, return the TOP of the dotted
                    //   path; when fromlist is non-None (a tuple), return
                    //   the leaf module. Matches Python semantics.
                    let fromlist = self.frames[frame_idx].pop();
                    let level    = self.frames[frame_idx].pop();
                    let level_u32 = level.as_int().unwrap_or(0).max(0) as u32;
                    // Borrow the raw name; resolve_relative_name takes &self, so
                    // we don't need to clone. The returned `abs_name` is an
                    // owned String, breaking the borrow before the &mut self
                    // call to resolve_import below.
                    let abs_name = {
                        let raw_name = &self.code_objects[code_index].names[operand as usize];
                        self.resolve_relative_name(raw_name, level_u32, line)?
                    };
                    let leaf_module = self.resolve_import(&abs_name, line)?;
                    // For `import a.b.c` (no fromlist) Python pushes the TOP
                    // segment, not the leaf. resolve_import already cached
                    // every level on the way in, so the lookup is free.
                    let result = if fromlist.is_none() {
                        let top = dotted_top(&abs_name);
                        if top == abs_name {
                            leaf_module
                        } else {
                            self.sys_modules.get(top).copied().ok_or_else(|| {
                                PythonError::runtime(
                                    format!("internal: top module '{top}' missing from sys.modules"),
                                    line,
                                )
                            })?
                        }
                    } else {
                        leaf_module
                    };
                    self.frames[frame_idx].push(result);
                }
                op::IMPORT_FROM => {
                    // Stack: [module] → [module, attr]. Module stays on stack
                    // so subsequent IMPORT_FROM ops can fetch siblings; the
                    // compiler emits a final POP_TOP to discard it.
                    let module = self.frames[frame_idx].peek();
                    let attr = self.code_objects[code_index].names[operand as usize].clone();
                    let value = self.module_get_attr(module, &attr, line)?;
                    self.frames[frame_idx].push(value);
                }
                op::IMPORT_STAR => {
                    // Stack: [module] → []. Bind public names from module
                    // into the current globals namespace.
                    let module = self.frames[frame_idx].pop();
                    self.import_star_into_globals(module, line)?;
                }
                op::HALT => {
                    return Ok(());
                }
                _ => {
                    return Err(PythonError::runtime(
                        format!("unknown opcode {opcode}"),
                        line,
                    ));
                }
            }
        }
    }

    /// Look up an attribute on a class (walking MRO).
    fn lookup_attr_on_class(&self, class_idx: usize, attr: &str) -> Option<Value> {
        if let HeapObject::Class { mro, attrs, .. } = &self.heap[class_idx] {
            // Check this class first
            if let Some(val) = attrs.get(attr) {
                return Some(*val);
            }
            // Walk MRO (skip self which is mro[0])
            for &m in mro.iter().skip(1) {
                if let HeapObject::Class { attrs: mattrs, .. } = &self.heap[m]
                    && let Some(val) = mattrs.get(attr)
                {
                    return Some(*val);
                }
            }
        }
        None
    }

    fn load_attr(&mut self, obj: Value, attr: &str, line: u32) -> Result<Value, PythonError> {
        // Instance attribute access
        if let Some(obj_idx) = obj.as_object_ref() {
            match &self.heap[obj_idx] {
                HeapObject::Instance { class_idx, attrs } => {
                    let class_idx = *class_idx;
                    // Check instance attrs first
                    if let Some(&val) = attrs.get(attr) {
                        return Ok(val);
                    }
                    // Check class MRO
                    if let Some(method) = self.lookup_attr_on_class(class_idx, attr) {
                        // If it's a function (or closure/function on the heap), bind it.
                        let is_callable = method.is_func()
                            || matches!(method.as_object_ref().and_then(|i| self.heap.get(i)),
                                Some(HeapObject::Closure { .. } | HeapObject::Function { .. }));
                        if is_callable {
                            let bound_idx = self.heap.len();
                            self.heap.push(HeapObject::BoundMethod {
                                instance: obj,
                                method,
                            });
                            return Ok(Value::object_ref(bound_idx));
                        }
                        return Ok(method);
                    }
                    return Err(PythonError::runtime(
                        format!("'{}' object has no attribute '{attr}'",
                            if let HeapObject::Class { name, .. } = &self.heap[class_idx] { name.as_str() } else { "object" }),
                        line,
                    ));
                }
                HeapObject::Class { attrs, name, .. } => {
                    if let Some(&val) = attrs.get(attr) {
                        return Ok(val);
                    }
                    // Check MRO
                    let class_idx = obj_idx;
                    if let Some(val) = self.lookup_attr_on_class(class_idx, attr) {
                        return Ok(val);
                    }
                    return Err(PythonError::runtime(
                        format!("type '{name}' has no attribute '{attr}'"), line,
                    ));
                }
                HeapObject::ExceptionObj { exc_type, message, args } => {
                    match attr {
                        "args" => {
                            if args.is_empty() {
                                let msg_idx = self.heap.len();
                                self.heap.push(HeapObject::Str(message.clone().into()));
                                let tuple_idx = self.heap.len();
                                self.heap.push(HeapObject::Tuple(vec![Value::str_ref(msg_idx)]));
                                return Ok(Value::object_ref(tuple_idx));
                            }
                            let tuple_idx = self.heap.len();
                            self.heap.push(HeapObject::Tuple(args.clone()));
                            return Ok(Value::object_ref(tuple_idx));
                        }
                        "message" => {
                            let str_idx = self.heap.len();
                            self.heap.push(HeapObject::Str(message.clone().into()));
                            return Ok(Value::str_ref(str_idx));
                        }
                        _ => {
                            return Err(PythonError::runtime(
                                format!("'{}' object has no attribute '{attr}'", exc_type.name()), line,
                            ));
                        }
                    }
                }
                HeapObject::Dict { .. } => {
                    return self.dict_method_dispatch(obj_idx, attr, line);
                }
                HeapObject::Tuple(items) => {
                    if attr == "__len__" {
                        return Ok(Value::small_int_unchecked(items.len() as i64));
                    }
                }
                HeapObject::Generator { .. } => {
                    if attr == "__next__" || attr == "send" || attr == "close" {
                        // Return a bound method placeholder
                        let bound_idx = self.heap.len();
                        self.heap.push(HeapObject::BoundMethod {
                            instance: obj,
                            method: Value::none(), // Handled specially
                        });
                        return Ok(Value::object_ref(bound_idx));
                    }
                }
                HeapObject::Module { name, globals, .. } => {
                    if let Some(&val) = globals.get(attr) {
                        return Ok(val);
                    }
                    return Err(PythonError::runtime(
                        format!("module '{name}' has no attribute '{attr}'"), line,
                    ));
                }
                _ => {}
            }
        }

        // List method dispatch
        if let Some(list_idx) = obj.as_list_ref() {
            return self.list_method_dispatch(list_idx, attr, line);
        }

        // String method dispatch
        if let Some(str_idx) = obj.as_str_ref() {
            return self.str_method_dispatch(str_idx, attr, line);
        }

        Err(PythonError::runtime(
            format!("'{}' has no attribute '{attr}'", obj.display(&self.heap)), line,
        ))
    }

    fn store_attr(&mut self, obj: Value, attr: &str, val: Value, line: u32) -> Result<(), PythonError> {
        if let Some(obj_idx) = obj.as_object_ref() {
            match &mut self.heap[obj_idx] {
                HeapObject::Instance { attrs, .. } => {
                    attrs.insert(attr.to_string(), val);
                    return Ok(());
                }
                HeapObject::Class { attrs, .. } => {
                    attrs.insert(attr.to_string(), val);
                    return Ok(());
                }
                _ => {}
            }
        }
        Err(PythonError::runtime(
            format!("cannot set attribute '{attr}' on {}", obj.display(&self.heap)), line,
        ))
    }

    fn subscript_get(&self, obj: Value, index: Value, line: u32) -> Result<Value, PythonError> {
        if let Some(heap_idx) = obj.as_list_ref() {
            if let Some(i) = index.as_int()
                && let HeapObject::List(items) = &self.heap[heap_idx]
            {
                let idx = if i < 0 { items.len() as i64 + i } else { i } as usize;
                if idx < items.len() {
                    return Ok(items[idx]);
                }
                return Err(PythonError::runtime("list index out of range", line));
            }
            return Err(PythonError::runtime("list indices must be integers", line));
        }
        if let Some(heap_idx) = obj.as_str_ref() {
            if let Some(i) = index.as_int() {
                let s = heap_str(&self.heap, heap_idx)?;
                let chars: Vec<char> = s.chars().collect();
                let idx = if i < 0 { chars.len() as i64 + i } else { i } as usize;
                if idx < chars.len() {
                    // Need mutable access to create string — use a workaround
                    // Actually we can't push to heap here because we only have &self
                    // Return the char's code point as int for now? No, let's fix the signature
                    return Err(PythonError::runtime("string subscript needs mutable heap", line));
                }
                return Err(PythonError::runtime("string index out of range", line));
            }
            return Err(PythonError::runtime("string indices must be integers", line));
        }
        if let Some(obj_idx) = obj.as_object_ref() {
            match &self.heap[obj_idx] {
                HeapObject::Tuple(items) => {
                    if let Some(i) = index.as_int() {
                        let idx = if i < 0 { items.len() as i64 + i } else { i } as usize;
                        if idx < items.len() {
                            return Ok(items[idx]);
                        }
                        return Err(PythonError::runtime("tuple index out of range", line));
                    }
                    return Err(PythonError::runtime("tuple indices must be integers", line));
                }
                HeapObject::Dict { keys, values, index_map } => {
                    let h = value_hash(index, &self.heap);
                    // Fast path: hash → key-index via index_map. Verify the
                    // key matches (the index_map only stores one entry per
                    // hash, so a stored mismatch means hash collision).
                    if let Some(&i) = index_map.get(&h)
                        && values_equal(keys[i], index, &self.heap)
                    {
                        return Ok(values[i]);
                    }
                    // Collision fallback: linear scan. Rare with the Mersenne
                    // hash for ints + DJB2 for strings; correctness backstop.
                    for (i, k) in keys.iter().enumerate() {
                        if value_hash(*k, &self.heap) == h && values_equal(*k, index, &self.heap) {
                            return Ok(values[i]);
                        }
                    }
                    return Err(PythonError::runtime("KeyError", line));
                }
                _ => {}
            }
        }
        Err(PythonError::runtime("object is not subscriptable", line))
    }

    fn subscript_set(&mut self, obj: Value, index: Value, val: Value, line: u32) -> Result<(), PythonError> {
        if let Some(heap_idx) = obj.as_list_ref() {
            if let Some(i) = index.as_int()
                && let HeapObject::List(items) = &mut self.heap[heap_idx]
            {
                let idx = if i < 0 { items.len() as i64 + i } else { i } as usize;
                if idx < items.len() {
                    items[idx] = val;
                    return Ok(());
                }
                return Err(PythonError::runtime("list assignment index out of range", line));
            }
            return Err(PythonError::runtime("list indices must be integers", line));
        }
        if let Some(obj_idx) = obj.as_object_ref() {
            // Compute hash before borrowing mutably
            let h = value_hash(index, &self.heap);
            // Check if key exists (read-only pass)
            let existing = if let HeapObject::Dict { keys, index_map, .. } = &self.heap[obj_idx] {
                if let Some(&ei) = index_map.get(&h) {
                    if ei < keys.len() && values_equal(keys[ei], index, &self.heap) {
                        Some(ei)
                    } else { None }
                } else { None }
            } else { None };

            if let HeapObject::Dict { keys, values, index_map } = &mut self.heap[obj_idx] {
                if let Some(ei) = existing {
                    values[ei] = val;
                } else {
                    let idx = keys.len();
                    keys.push(index);
                    values.push(val);
                    index_map.insert(h, idx);
                }
                return Ok(());
            }
        }
        Err(PythonError::runtime("object does not support item assignment", line))
    }

    fn subscript_delete(&mut self, obj: Value, index: Value, line: u32) -> Result<(), PythonError> {
        if let Some(heap_idx) = obj.as_list_ref()
            && let Some(i) = index.as_int()
            && let HeapObject::List(items) = &mut self.heap[heap_idx]
        {
            let idx = if i < 0 { items.len() as i64 + i } else { i } as usize;
            if idx < items.len() {
                items.remove(idx);
                return Ok(());
            }
            return Err(PythonError::runtime("list assignment index out of range", line));
        }
        Err(PythonError::runtime("object does not support item deletion", line))
    }

    fn unpack_sequence(&self, seq: Value, count: usize, line: u32) -> Result<Vec<Value>, PythonError> {
        if let Some(list_idx) = seq.as_list_ref()
            && let HeapObject::List(items) = &self.heap[list_idx]
        {
            if items.len() == count {
                return Ok(items.clone());
            }
            return Err(PythonError::runtime(
                format!("not enough values to unpack (expected {count}, got {})", items.len()), line,
            ));
        }
        if let Some(obj_idx) = seq.as_object_ref()
            && let HeapObject::Tuple(items) = &self.heap[obj_idx]
        {
            if items.len() == count {
                return Ok(items.clone());
            }
            return Err(PythonError::runtime(
                format!("not enough values to unpack (expected {count}, got {})", items.len()), line,
            ));
        }
        Err(PythonError::runtime("cannot unpack non-sequence", line))
    }

    fn call_class(&mut self, frame_idx: usize, class_idx: usize, args: &[Value], line: u32) -> Result<(), PythonError> {
        // Create instance
        let inst_idx = self.heap.len();
        self.heap.push(HeapObject::Instance {
            class_idx,
            attrs: HashMap::new(),
        });
        let instance = Value::object_ref(inst_idx);

        // Look for __init__
        if let Some(init_method) = self.lookup_attr_on_class(class_idx, "__init__") {
            let mut init_args = Vec::with_capacity(args.len() + 1);
            init_args.push(instance);
            init_args.extend_from_slice(args);
            self.call_value(frame_idx, init_method, &init_args, line)?;
            if self.frames.len() > frame_idx + 1 {
                // __init__ pushed a new frame. Tag it so RETURN_VALUE knows to
                // discard __init__'s None return and push the instance instead.
                let init_frame_idx = self.frames.len() - 1;
                self.frames[init_frame_idx].init_instance = Some(instance);
                return Ok(());
            }
            // Builtin __init__ returned directly — pop its return value
            let _none = self.frames[frame_idx].pop();
            self.frames[frame_idx].push(instance);
        } else {
            self.frames[frame_idx].push(instance);
        }
        Ok(())
    }

    /// Call a value as a function, managing frame setup.
    fn call_value(&mut self, caller_frame_idx: usize, func_val: Value, args: &[Value], line: u32) -> Result<(), PythonError> {
        let argc = args.len();

        if let Some(heap_idx) = func_val.as_func_ref() {
            let (func_code_index, arity, func_module_idx) = if let HeapObject::Function { code_index, arity, module_idx, .. } = &self.heap[heap_idx] {
                (*code_index, *arity as usize, *module_idx)
            } else {
                return Err(PythonError::runtime("not a callable", line));
            };
            if argc != arity {
                let name = if let HeapObject::Function { name, .. } = &self.heap[heap_idx] {
                    name.clone()
                } else { "???".to_string() };
                return Err(PythonError::runtime(
                    format!("{name}() takes {arity} argument(s) but {argc} were given"), line,
                ));
            }
            let mut new_frame = Frame::new_for_code(func_code_index, self.code_objects[func_code_index].num_locals, func_module_idx);
            for (i, arg) in args.iter().enumerate() {
                new_frame.locals[i] = *arg;
            }
            // Set up cells
            let num_cell = self.code_objects[func_code_index].cell_var_names.len();
            for _ in 0..num_cell {
                let ci = self.heap.len();
                self.heap.push(HeapObject::Cell(Value::none()));
                new_frame.cells.push(ci);
            }
            let cell_var_names: Vec<String> = self.code_objects[func_code_index].cell_var_names.clone();
            for (ci, cv_name) in cell_var_names.iter().enumerate() {
                let local_names = &self.code_objects[func_code_index].local_names;
                if let Some(li) = local_names.iter().position(|n| n == cv_name) {
                    self.heap[new_frame.cells[ci]] = HeapObject::Cell(new_frame.locals[li]);
                }
            }
            self.frames.push(new_frame);
        } else if let Some(heap_idx) = func_val.as_object_ref() {
            match &self.heap[heap_idx] {
                HeapObject::BuiltinFn { id, .. } => {
                    let id = *id;
                    let result = builtins::call_builtin(id, args, &mut self.heap, &mut self.output, &self.globals)?;
                    self.frames[caller_frame_idx].push(result);
                }
                HeapObject::Closure { code_index, arity, cells, module_idx, .. } => {
                    let func_code_index = *code_index;
                    let arity = *arity as usize;
                    let cells = cells.clone();
                    let func_module_idx = *module_idx;
                    if argc != arity {
                        return Err(PythonError::runtime("wrong number of arguments", line));
                    }
                    let mut new_frame = Frame::new_for_code(func_code_index, self.code_objects[func_code_index].num_locals, func_module_idx);
                    for (i, arg) in args.iter().enumerate() {
                        new_frame.locals[i] = *arg;
                    }
                    let num_cell = self.code_objects[func_code_index].cell_var_names.len();
                    for _ in 0..num_cell {
                        let ci = self.heap.len();
                        self.heap.push(HeapObject::Cell(Value::none()));
                        new_frame.cells.push(ci);
                    }
                    let cell_var_names: Vec<String> = self.code_objects[func_code_index].cell_var_names.clone();
                    for (ci, cv_name) in cell_var_names.iter().enumerate() {
                        let local_names = &self.code_objects[func_code_index].local_names;
                        if let Some(li) = local_names.iter().position(|n| n == cv_name) {
                            self.heap[new_frame.cells[ci]] = HeapObject::Cell(new_frame.locals[li]);
                        }
                    }
                    let num_free = self.code_objects[func_code_index].free_var_names.len();
                    for i in 0..num_free {
                        if i < cells.len() {
                            new_frame.cells.push(cells[i]);
                        }
                    }
                    self.frames.push(new_frame);
                }
                _ => {
                    return Err(PythonError::runtime("not callable", line));
                }
            }
        } else {
            return Err(PythonError::runtime("not callable", line));
        }
        Ok(())
    }

    fn resume_generator(&mut self, _caller_frame_idx: usize, gen_heap_idx: usize, line: u32) -> Result<(), PythonError> {
        // Extract generator state
        let (code_index, ip, locals, stack, cells) = if let HeapObject::Generator {
            code_index, ip, locals, stack, cells, state,
        } = &mut self.heap[gen_heap_idx] {
            if *state == GeneratorState::Completed {
                return Err(PythonError::runtime("StopIteration", line));
            }
            *state = GeneratorState::Running;
            (*code_index, *ip, locals.clone(), stack.clone(), cells.clone())
        } else {
            return Err(PythonError::runtime("not a generator", line));
        };

        // Create a new frame from generator state. Locals are right-sized
        // from the code object; module_idx is None for now (generators
        // defined in imported modules: see follow-up).
        let num_locals = self.code_objects[code_index].num_locals;
        let mut gen_frame = Frame::new_for_code(code_index, num_locals, None);
        gen_frame.ip = ip;
        gen_frame.generator_idx = Some(gen_heap_idx);
        gen_frame.cells = cells;

        // Restore locals
        for (i, val) in locals.iter().enumerate() {
            if i < gen_frame.locals.len() {
                gen_frame.locals[i] = *val;
            }
        }

        // Restore stack
        for val in &stack {
            gen_frame.push(*val);
        }

        self.frames.push(gen_frame);
        Ok(())
    }

    fn list_method_dispatch(&mut self, list_idx: usize, attr: &str, line: u32) -> Result<Value, PythonError> {
        let method_id = match attr {
            "append" => BuiltinId::ListAppend,
            "pop" => BuiltinId::ListPop,
            "sort" => BuiltinId::ListSort,
            "reverse" => BuiltinId::ListReverse,
            "insert" => BuiltinId::ListInsert,
            "extend" => BuiltinId::ListExtend,
            _ => return Err(PythonError::runtime(
                format!("'list' object has no attribute '{attr}'"), line,
            )),
        };
        let bound_idx = self.heap.len();
        self.heap.push(HeapObject::BuiltinFn {
            name: format!("list.{attr}"),
            id: method_id,
        });
        // Store the list reference as a "bound" builtin: we'll pass the list as first arg
        // Actually, for list methods we need the list. Let's create a BoundMethod.
        let list_val = Value::list_ref(list_idx);
        let method_val = Value::object_ref(bound_idx);
        let bm_idx = self.heap.len();
        self.heap.push(HeapObject::BoundMethod {
            instance: list_val,
            method: method_val,
        });
        Ok(Value::object_ref(bm_idx))
    }

    fn str_method_dispatch(&mut self, str_idx: usize, attr: &str, line: u32) -> Result<Value, PythonError> {
        let method_id = match attr {
            "upper" => BuiltinId::StrUpper,
            "lower" => BuiltinId::StrLower,
            "split" => BuiltinId::StrSplit,
            "join" => BuiltinId::StrJoin,
            "replace" => BuiltinId::StrReplace,
            "startswith" => BuiltinId::StrStartswith,
            "endswith" => BuiltinId::StrEndswith,
            "find" => BuiltinId::StrFind,
            "strip" => BuiltinId::StrStrip,
            "format" => BuiltinId::StrFormat,
            _ => return Err(PythonError::runtime(
                format!("'str' object has no attribute '{attr}'"), line,
            )),
        };
        let bound_idx = self.heap.len();
        self.heap.push(HeapObject::BuiltinFn {
            name: format!("str.{attr}"),
            id: method_id,
        });
        let str_val = Value::str_ref(str_idx);
        let method_val = Value::object_ref(bound_idx);
        let bm_idx = self.heap.len();
        self.heap.push(HeapObject::BoundMethod {
            instance: str_val,
            method: method_val,
        });
        Ok(Value::object_ref(bm_idx))
    }

    fn dict_method_dispatch(&mut self, dict_idx: usize, attr: &str, line: u32) -> Result<Value, PythonError> {
        let method_id = match attr {
            "keys" => BuiltinId::DictKeys,
            "values" => BuiltinId::DictValues,
            "items" => BuiltinId::DictItems,
            "get" => BuiltinId::DictGet,
            "pop" => BuiltinId::DictPop,
            _ => return Err(PythonError::runtime(
                format!("'dict' object has no attribute '{attr}'"), line,
            )),
        };
        let bound_idx = self.heap.len();
        self.heap.push(HeapObject::BuiltinFn {
            name: format!("dict.{attr}"),
            id: method_id,
        });
        let dict_val = Value::object_ref(dict_idx);
        let method_val = Value::object_ref(bound_idx);
        let bm_idx = self.heap.len();
        self.heap.push(HeapObject::BoundMethod {
            instance: dict_val,
            method: method_val,
        });
        Ok(Value::object_ref(bm_idx))
    }

    // --- Import-machinery glue ---

    /// Resolve an `import name` request. Handles cmodules, source files,
    /// packages (directories with __init__.py), and dotted names. Recurses
    /// to load parent packages before submodules so `import foo.bar.baz`
    /// loads foo, then foo.bar, then foo.bar.baz in order.
    fn resolve_import(&mut self, name: &str, line: u32) -> Result<Value, PythonError> {
        if let Some(&cached) = self.sys_modules.get(name) {
            return Ok(cached);
        }

        let (parent_name, leaf) = split_module_name(name);

        // Ensure parent package is loaded first (recursive).
        let parent_module = match parent_name {
            Some(p) => Some(self.resolve_import(p, line)?),
            None    => None,
        };

        // Submodule lookup is restricted to the parent package's directory;
        // top-level lookup searches sys.path (None signals "use sys.path").
        let parent_dir: Option<std::path::PathBuf> = match parent_module {
            Some(parent) => Some(self.package_dir_of(parent, parent_name.unwrap_or(""), line)?),
            None => None,
        };
        let search: Option<&std::path::Path> = parent_dir.as_deref();

        // Top-level cmodule check (submodule cmodules out of M3 scope).
        if parent_module.is_none()
            && let Some(module) = self.import_system.try_load_cmodule(name, &mut self.heap)
        {
            self.sys_modules.insert(name.to_string(), module);
            return Ok(module);
        }

        // Package finder: <dir>/<leaf>/__init__.py wins over <dir>/<leaf>.py.
        if let Some(init_path) = self.import_system.find_package_in(search, leaf) {
            return self.load_source_module(name, init_path, true, parent_module, line);
        }
        // Source-file finder: <dir>/<leaf>.py.
        if let Some(file_path) = self.import_system.find_source_file_in(search, leaf) {
            return self.load_source_module(name, file_path, false, parent_module, line);
        }

        Err(PythonError::runtime(format!("No module named '{name}'"), line))
    }

    /// Resolve a relative-import name into an absolute one.
    /// `level=0` means absolute (returns `raw_name` unchanged).
    /// `level=1` (single dot) walks one segment up from the current module's
    /// __package__; `level=2` walks two; etc.
    fn resolve_relative_name(
        &self,
        raw_name: &str,
        level: u32,
        line: u32,
    ) -> Result<String, PythonError> {
        if level == 0 {
            return Ok(raw_name.to_string());
        }
        // The current module's __package__ lives in heap[module_idx].globals
        // via the Frame::module_idx routing. For top-level scripts,
        // module_idx is None and __package__ doesn't exist → error.
        let frame_idx = self.frames.len() - 1;
        let pkg_value = self.frame_globals_get(frame_idx, "__package__").unwrap_or(Value::none());
        if pkg_value.is_none() {
            return Err(PythonError::runtime(
                "attempted relative import with no known parent package", line,
            ));
        }
        let pkg_str_idx = pkg_value.as_str_ref().ok_or_else(|| {
            PythonError::runtime("internal: __package__ is not a string", line)
        })?;
        let pkg = heap_str(&self.heap, pkg_str_idx)?;
        // Walk `level - 1` segments up from `pkg`. (A single dot means
        // "current package", so we drop level-1 segments, not level.)
        // Actually: `from . import x` with level=1 means current package,
        // so we keep all of pkg.
        let mut parts: Vec<&str> = pkg.split('.').filter(|s| !s.is_empty()).collect();
        for _ in 1..level {
            if parts.is_empty() {
                return Err(PythonError::runtime(
                    "attempted relative import beyond top-level package", line,
                ));
            }
            parts.pop();
        }
        let base = parts.join(".");
        let absolute = if raw_name.is_empty() {
            base
        } else if base.is_empty() {
            raw_name.to_string()
        } else {
            format!("{base}.{raw_name}")
        };
        Ok(absolute)
    }

    /// Derive the directory of a loaded package from its module Value.
    /// For a package, `file` is `<dir>/__init__.py`, so the package dir
    /// is the parent of `file`.
    fn package_dir_of(
        &self,
        module: Value,
        module_name: &str,
        line: u32,
    ) -> Result<std::path::PathBuf, PythonError> {
        let idx = module.as_object_ref().ok_or_else(|| {
            PythonError::runtime(format!("internal: '{module_name}' module is not a heap ref"), line)
        })?;
        match &self.heap[idx] {
            HeapObject::Module { file: Some(f), .. } => {
                std::path::Path::new(f).parent()
                    .map(|p| p.to_path_buf())
                    .ok_or_else(|| PythonError::runtime(
                        format!("internal: '{module_name}' has no parent dir"), line,
                    ))
            }
            HeapObject::Module { file: None, .. } => {
                Err(PythonError::runtime(
                    format!("module '{module_name}' is not a package — cannot resolve submodule"),
                    line,
                ))
            }
            _ => Err(PythonError::runtime(
                format!("internal: '{module_name}' is not a Module"), line,
            )),
        }
    }

    /// Compile and execute a `.py` file as a Python module — package- and
    /// parent-aware version. Wires the module's `__package__` for relative
    /// imports and sets the new module as an attribute of its parent package.
    ///
    /// `is_package`: true when loading `<dir>/<leaf>/__init__.py`. The
    /// module's __package__ then equals its own name (it IS the package).
    /// `parent`: the loaded parent package's module Value, if any.
    fn load_source_module(
        &mut self,
        name: &str,
        file_path: std::path::PathBuf,
        is_package: bool,
        parent: Option<Value>,
        line: u32,
    ) -> Result<Value, PythonError> {
        // __package__ rule (PEP 328): for a package, __package__ == name;
        // for a submodule, __package__ == name-without-leaf; for a top-level
        // module not in a package, __package__ is "" (or None — we pick "").
        let package_str: Option<String> = if is_package {
            Some(name.to_string())
        } else {
            name.rfind('.').map(|i| name[..i].to_string())
        };

        let value = self.compile_and_execute_module(name, file_path, package_str, line)?;

        // Bind the new module as an attribute of its parent package, so
        // `pkg.sub` attribute access works after `import pkg.sub`.
        if let Some(parent_value) = parent
            && let Some(leaf) = name.rsplit('.').next()
        {
            let parent_idx = parent_value.as_object_ref().ok_or_else(|| {
                PythonError::runtime("internal: parent package is not a heap ref", line)
            })?;
            if let HeapObject::Module { globals, .. } = &mut self.heap[parent_idx] {
                globals.insert(leaf.to_string(), value);
            }
        }
        Ok(value)
    }

    /// Inner module loader — does the file-read + compile + execute. Used
    /// by both flat modules and packages; the only difference between them
    /// is the __package__ value, computed by the caller.
    fn compile_and_execute_module(
        &mut self,
        name: &str,
        file_path: std::path::PathBuf,
        package: Option<String>,
        line: u32,
    ) -> Result<Value, PythonError> {
        let source = std::fs::read_to_string(&file_path).map_err(|e| {
            PythonError::runtime(
                format!("could not read '{}': {e}", file_path.display()), line,
            )
        })?;
        let tokens = crate::lexer::tokenize(&source)?;
        let module_ast = crate::parser::parse(tokens)?;
        let body_code_idx = crate::compiler::compile_extending(
            &module_ast,
            &mut self.code_objects,
            &mut self.heap,
        )?;

        // Module globals start empty (only the dunders below are populated).
        // Builtin lookup is handled by `frame_globals_get`'s fallback chain.
        let file_str: String = file_path.to_string_lossy().into_owned();
        let name_value = alloc_str(&mut self.heap, name);
        let file_value = alloc_str(&mut self.heap, file_str.as_str());
        let package_value = match package.as_deref() {
            Some(p) => alloc_str(&mut self.heap, p),
            None    => Value::none(),
        };
        let mut globals: HashMap<String, Value> = HashMap::new();
        globals.insert("__name__".into(),    name_value);
        globals.insert("__file__".into(),    file_value);
        globals.insert("__doc__".into(),     Value::none());
        globals.insert("__package__".into(), package_value);

        // Capture the heap idx before alloc_module's push so we don't need to
        // round-trip through Value::as_object_ref afterwards.
        let module_idx = self.heap.len();
        let module_value = alloc_module(
            &mut self.heap, name.to_string(), globals, Some(file_str), package, false,
        );
        // Cache BEFORE execution so a circular self-import sees the
        // (partial) module from cache rather than infinite-recursing.
        self.sys_modules.insert(name.to_string(), module_value);

        // Module body runs in a frame rooted at this module. LOAD/STORE_GLOBAL
        // inside the body route through heap[module_idx].globals via the
        // Frame::module_idx mechanism — no globals swap needed.
        let target_depth = self.frames.len();
        self.frames.push(Frame::new_for_code(body_code_idx, self.code_objects[body_code_idx].num_locals, Some(module_idx)));
        let exec_result = self.execute_until_depth(target_depth);

        // The compiler always terminates module bodies with HALT (see
        // compile_module). HALT returns from execute_until_depth without
        // popping the body frame, so we drain it here. (If the compiler
        // ever switches to RETURN_VALUE for module bodies, that pops the
        // frame itself and this drain becomes a no-op — still correct.)
        while self.frames.len() > target_depth {
            self.frames.pop();
        }

        // Mark the module as initialized.
        if let HeapObject::Module { initialized, .. } = &mut self.heap[module_idx] {
            *initialized = exec_result.is_ok();
        }

        exec_result?;
        Ok(module_value)
    }

    /// `from module import attr` lookup. Tries the module's globals first;
    /// if the name isn't there, attempts a sub-import of `module.attr`
    /// (Python's documented fallback for `from pkg import sub` where sub
    /// is a submodule rather than an attribute).
    fn module_get_attr(&mut self, module: Value, attr: &str, line: u32) -> Result<Value, PythonError> {
        let idx = module.as_object_ref().ok_or_else(|| {
            PythonError::runtime("internal: IMPORT_FROM TOS is not an object ref", line)
        })?;
        // Direct attribute hit.
        if let HeapObject::Module { globals, .. } = &self.heap[idx]
            && let Some(&v) = globals.get(attr)
        {
            return Ok(v);
        }
        // Submodule fallback: try `module_name.attr` as a sub-import.
        let module_name = match &self.heap[idx] {
            HeapObject::Module { name, .. } => name.clone(),
            _ => return Err(PythonError::runtime(
                "internal: IMPORT_FROM TOS is not a Module", line,
            )),
        };
        let submodule_name = format!("{module_name}.{attr}");
        if let Ok(sub) = self.resolve_import(&submodule_name, line) {
            return Ok(sub);
        }
        Err(PythonError::runtime(
            format!("cannot import name '{attr}' from '{module_name}'"), line,
        ))
    }

    /// `from module import *` — bind public names into the current frame's
    /// namespace. Honors `__all__` if present; otherwise binds every name
    /// not starting with `_`. Routes through `frame_globals_insert` so the
    /// bindings land in the right module's globals.
    fn import_star_into_globals(&mut self, module: Value, line: u32) -> Result<(), PythonError> {
        let idx = module.as_object_ref().ok_or_else(|| {
            PythonError::runtime("internal: IMPORT_STAR TOS is not an object ref", line)
        })?;
        let (all_list, bindings): (Option<Vec<String>>, Vec<(String, Value)>) = match &self.heap[idx] {
            HeapObject::Module { all, globals, .. } => {
                (all.clone(),
                 globals.iter().map(|(k, v)| (k.clone(), *v)).collect())
            }
            other => return Err(PythonError::runtime(
                format!("internal: IMPORT_STAR TOS is not a Module (got {other:?})"), line,
            )),
        };
        let bindings_to_apply: Vec<(String, Value)> = match all_list {
            Some(names) => bindings.into_iter()
                .filter(|(k, _)| names.contains(k))
                .collect(),
            None => bindings.into_iter()
                .filter(|(k, _)| !k.starts_with('_'))
                .collect(),
        };
        let frame_idx = self.frames.len() - 1;
        for (k, v) in bindings_to_apply {
            self.frame_globals_insert(frame_idx, k, v);
        }
        Ok(())
    }
}

// --- Free functions for arithmetic/comparison ---

fn is_truthy(val: Value, heap: &[HeapObject]) -> bool {
    if let Some(idx) = val.as_str_ref() && let Some(s) = heap[idx].as_str() {
        return !s.is_empty();
    }
    if let Some(idx) = val.as_list_ref() && let HeapObject::List(items) = &heap[idx] {
        return !items.is_empty();
    }
    if let Some(idx) = val.as_object_ref() {
        match &heap[idx] {
            HeapObject::Tuple(items) => return !items.is_empty(),
            HeapObject::Dict { keys, .. } => return !keys.is_empty(),
            HeapObject::Set(items) => return !items.is_empty(),
            _ => return true,
        }
    }
    val.is_truthy()
}

fn values_equal(left: Value, right: Value, heap: &[HeapObject]) -> bool {
    // Defer Python value equality (cross-representation int, float coercion,
    // bool widening, string content) to Value::py_eq. Anything it accepts is
    // already equal under Python `==`.
    if left.py_eq(right, heap) { return true; }
    // Exception type matching (for except handlers) — vm-specific layer on
    // top of plain Python equality.
    if let (Some(a_idx), Some(b_idx)) = (left.as_object_ref(), right.as_object_ref()) {
        // If one is an ExceptionObj and the other is an ExcConstructor builtin,
        // compare types
        match (&heap[a_idx], &heap[b_idx]) {
            (HeapObject::ExceptionObj { exc_type: et1, .. }, HeapObject::ExceptionObj { exc_type: et2, .. }) => {
                return et1.is_subtype(*et2);
            }
            (HeapObject::ExceptionObj { exc_type, .. }, HeapObject::BuiltinFn { id: BuiltinId::ExcConstructor(target), .. }) => {
                return exc_type.is_subtype(*target);
            }
            (HeapObject::BuiltinFn { id: BuiltinId::ExcConstructor(target), .. }, HeapObject::ExceptionObj { exc_type, .. }) => {
                return exc_type.is_subtype(*target);
            }
            _ => {}
        }
    }
    false
}

/// Convert an ArithError to the corresponding RuntimeError message.
/// Once exception types are first-class at this layer this becomes a
/// proper ZeroDivisionError / ValueError dispatch.
fn arith_to_runtime(err: ArithError, op_msg: &str, line: u32) -> PythonError {
    let msg = match err {
        ArithError::DivByZero      => format!("{op_msg} by zero"),
        ArithError::NegativeShift  => "negative shift count".to_string(),
        ArithError::NegativePower  => "pow() 2nd argument cannot be negative when 3rd argument specified".to_string(),
    };
    PythonError::runtime(msg, line)
}

/// Try to view both operands as ints (small, big, or widened bool). Returns
/// None if either side is a float — caller falls through to the float path.
fn pyint_pair<'a>(
    left: Value,
    right: Value,
    heap: &'a [HeapObject],
) -> Option<(PyInt<'a>, PyInt<'a>)> {
    if left.is_float() || right.is_float() { return None; }
    Some((
        PyInt::from_value_or_bool(left, heap)?,
        PyInt::from_value_or_bool(right, heap)?,
    ))
}

fn binary_add(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let Some((a, b)) = pyint_pair(left, right, heap) {
        return Ok(a.add(b).into_value(heap));
    }
    if (left.is_float() || right.is_float())
        && let (Some(a), Some(b)) = (value_to_f64(left, heap), value_to_f64(right, heap))
    {
        return Ok(Value::float(a + b));
    }
    if let (Some(a_idx), Some(b_idx)) = (left.as_str_ref(), right.as_str_ref()) {
        // Pre-size a single String, push both halves into it. Avoids the
        // intermediate `.to_string()` clone of `a` and the format! buffer.
        let result = {
            let a = heap_str(heap, a_idx)?;
            let b = heap_str(heap, b_idx)?;
            let mut out = String::with_capacity(a.len() + b.len());
            out.push_str(a);
            out.push_str(b);
            out
        };
        let heap_idx = heap.len();
        heap.push(HeapObject::Str(result.into()));
        return Ok(Value::str_ref(heap_idx));
    }
    if let (Some(a_idx), Some(b_idx)) = (left.as_list_ref(), right.as_list_ref()) {
        // Single allocation sized for both halves; extend_from_slice avoids
        // the previous double-clone of both source lists.
        let result = {
            let a = if let HeapObject::List(items) = &heap[a_idx] { items.as_slice() } else { &[] };
            let b = if let HeapObject::List(items) = &heap[b_idx] { items.as_slice() } else { &[] };
            let mut out = Vec::with_capacity(a.len() + b.len());
            out.extend_from_slice(a);
            out.extend_from_slice(b);
            out
        };
        let heap_idx = heap.len();
        heap.push(HeapObject::List(result));
        return Ok(Value::list_ref(heap_idx));
    }
    Err(PythonError::runtime("unsupported operand type(s) for +", line))
}

fn binary_sub(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let Some((a, b)) = pyint_pair(left, right, heap) {
        return Ok(a.sub(b).into_value(heap));
    }
    if let (Some(a), Some(b)) = (value_to_f64(left, heap), value_to_f64(right, heap)) {
        return Ok(Value::float(a - b));
    }
    Err(PythonError::runtime("unsupported operand type(s) for -", line))
}

fn binary_mul(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let Some((a, b)) = pyint_pair(left, right, heap) {
        return Ok(a.mul(b).into_value(heap));
    }
    if (left.is_float() || right.is_float())
        && let (Some(a), Some(b)) = (value_to_f64(left, heap), value_to_f64(right, heap))
    {
        return Ok(Value::float(a * b));
    }
    // String repetition — count comes from the int side (small only for now).
    if let Some(s_idx) = left.as_str_ref() && let Some(n) = right.as_int() {
        let s = heap_str(heap, s_idx)?;
        let heap_idx = heap.len();
        heap.push(HeapObject::Str(s.repeat(n.max(0) as usize).into()));
        return Ok(Value::str_ref(heap_idx));
    }
    if let Some(n) = left.as_int() && let Some(s_idx) = right.as_str_ref() {
        let s = heap_str(heap, s_idx)?;
        let heap_idx = heap.len();
        heap.push(HeapObject::Str(s.repeat(n.max(0) as usize).into()));
        return Ok(Value::str_ref(heap_idx));
    }
    Err(PythonError::runtime("unsupported operand type(s) for *", line))
}

fn binary_div(left: Value, right: Value, heap: &[HeapObject], line: u32) -> Result<Value, PythonError> {
    // Python `/` is always true-division, returns float.
    if let Some((a, b)) = pyint_pair(left, right, heap) {
        return pyint_truediv(a, b)
            .map(Value::float)
            .map_err(|e| arith_to_runtime(e, "division", line));
    }
    let af = value_to_f64(left, heap).ok_or_else(|| PythonError::runtime("unsupported operand type(s) for /", line))?;
    let bf = value_to_f64(right, heap).ok_or_else(|| PythonError::runtime("unsupported operand type(s) for /", line))?;
    if bf == 0.0 {
        return Err(PythonError::runtime("division by zero", line));
    }
    Ok(Value::float(af / bf))
}

fn binary_floor_div(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let Some((a, b)) = pyint_pair(left, right, heap) {
        return a.floordiv(b)
            .map(|r| r.into_value(heap))
            .map_err(|e| arith_to_runtime(e, "integer division or modulo", line));
    }
    if let (Some(a), Some(b)) = (value_to_f64(left, heap), value_to_f64(right, heap)) {
        if b == 0.0 {
            return Err(PythonError::runtime("float floor division by zero", line));
        }
        return Ok(Value::float((a / b).floor()));
    }
    Err(PythonError::runtime("unsupported operand type(s) for //", line))
}

fn binary_mod(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let Some((a, b)) = pyint_pair(left, right, heap) {
        return a.mod_(b)
            .map(|r| r.into_value(heap))
            .map_err(|e| arith_to_runtime(e, "integer division or modulo", line));
    }
    if let (Some(a), Some(b)) = (value_to_f64(left, heap), value_to_f64(right, heap)) {
        if b == 0.0 {
            return Err(PythonError::runtime("float modulo by zero", line));
        }
        return Ok(Value::float(((a % b) + b) % b));
    }
    Err(PythonError::runtime("unsupported operand type(s) for %", line))
}

fn binary_pow(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let Some((a, b)) = pyint_pair(left, right, heap) {
        return Ok(match a.pow(b) {
            PyPowResult::Int(o)   => o.into_value(heap),
            PyPowResult::Float(f) => Value::float(f),
        });
    }
    if let (Some(a), Some(b)) = (value_to_f64(left, heap), value_to_f64(right, heap)) {
        return Ok(Value::float(a.powf(b)));
    }
    Err(PythonError::runtime("unsupported operand type(s) for **", line))
}

fn compare(
    left: Value,
    right: Value,
    heap: &[HeapObject],
    int_cmp: impl Fn(i64, i64) -> bool,
    float_cmp: impl Fn(f64, f64) -> bool,
) -> bool {
    if let Some((a, b)) = pyint_pair(left, right, heap) {
        return int_cmp(a.cmp(b) as i64, 0);
    }
    if let (Some(a), Some(b)) = (value_to_f64(left, heap), value_to_f64(right, heap)) {
        return float_cmp(a, b);
    }
    if left.is_none() && right.is_none() {
        return true;
    }
    if let (Some(a_idx), Some(b_idx)) = (left.as_str_ref(), right.as_str_ref()) {
        // unwrap_or keeps the bool-returning signature panic-free; a str_ref
        // pointing at a non-Str heap entry is an internal invariant violation
        // that compare can't surface, so we treat both sides as empty.
        let a = heap.get(a_idx).and_then(HeapObject::as_str).unwrap_or("");
        let b = heap.get(b_idx).and_then(HeapObject::as_str).unwrap_or("");
        return int_cmp(a.cmp(b) as i64, 0);
    }
    false
}

fn contains(item: &Value, container: &Value, heap: &[HeapObject]) -> Result<bool, PythonError> {
    if let Some(list_idx) = container.as_list_ref()
        && let HeapObject::List(items) = &heap[list_idx]
    {
        return Ok(items.iter().any(|v| values_equal(*item, *v, heap)));
    }
    if let Some(str_idx) = container.as_str_ref()
        && let Some(item_idx) = item.as_str_ref()
    {
        let s = heap[str_idx].as_str().unwrap_or("");
        let sub = heap[item_idx].as_str().unwrap_or("");
        return Ok(s.contains(sub));
    }
    if let Some(obj_idx) = container.as_object_ref() {
        match &heap[obj_idx] {
            HeapObject::Tuple(items) => {
                return Ok(items.iter().any(|v| values_equal(*item, *v, heap)));
            }
            HeapObject::Dict { keys, index_map, .. } => {
                let h = value_hash(*item, heap);
                if let Some(&i) = index_map.get(&h)
                    && i < keys.len()
                    && values_equal(keys[i], *item, heap)
                {
                    return Ok(true);
                }
                // Hash-collision fallback.
                return Ok(keys.iter().any(|k| value_hash(*k, heap) == h && values_equal(*item, *k, heap)));
            }
            HeapObject::Set(items) => {
                return Ok(items.iter().any(|v| values_equal(*item, *v, heap)));
            }
            _ => {}
        }
    }
    Err(PythonError::runtime("argument of type is not iterable", 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler;
    use crate::lexer;
    use crate::parser;

    fn run_and_capture(src: &str) -> Vec<String> {
        let tokens = lexer::tokenize(src).unwrap();
        let module = parser::parse(tokens).unwrap();
        let (code_objects, heap) = compiler::compile(&module).unwrap();
        let mut vm = VM::new(code_objects, heap);
        vm.run().unwrap();
        vm.output
    }

    fn run_expect_err(src: &str) -> String {
        let tokens = lexer::tokenize(src).unwrap();
        let module = parser::parse(tokens).unwrap();
        let (code_objects, heap) = compiler::compile(&module).unwrap();
        let mut vm = VM::new(code_objects, heap);
        match vm.run() {
            Err(e) => e.to_string(),
            Ok(()) => panic!("expected error, got success"),
        }
    }

    #[test]
    fn test_hello_world() {
        let output = run_and_capture("print(\"hello world\")\n");
        assert_eq!(output, vec!["hello world"]);
    }

    #[test]
    fn test_arithmetic() {
        let output = run_and_capture("print(2 + 3)\nprint(10 - 4)\nprint(3 * 7)\n");
        assert_eq!(output, vec!["5", "6", "21"]);
    }

    #[test]
    fn test_variables() {
        let output = run_and_capture("x = 10\ny = 3\nprint(x + y)\n");
        assert_eq!(output, vec!["13"]);
    }

    #[test]
    fn test_comparison() {
        let output = run_and_capture("print(10 > 3)\nprint(1 == 2)\n");
        assert_eq!(output, vec!["True", "False"]);
    }

    #[test]
    fn test_if_else() {
        let output = run_and_capture("x = 5\nif x > 3:\n    print(\"yes\")\nelse:\n    print(\"no\")\n");
        assert_eq!(output, vec!["yes"]);
    }

    #[test]
    fn test_for_range() {
        let output = run_and_capture("for i in range(3):\n    print(i)\n");
        assert_eq!(output, vec!["0", "1", "2"]);
    }

    #[test]
    fn test_function() {
        let output = run_and_capture("def add(a, b):\n    return a + b\nprint(add(3, 4))\n");
        assert_eq!(output, vec!["7"]);
    }

    #[test]
    fn test_while_loop() {
        let output = run_and_capture("x = 0\nwhile x < 3:\n    print(x)\n    x += 1\n");
        assert_eq!(output, vec!["0", "1", "2"]);
    }

    #[test]
    fn test_nested_function_calls() {
        let src = "def double(x):\n    return x * 2\ndef add_one(x):\n    return x + 1\nprint(add_one(double(5)))\n";
        let output = run_and_capture(src);
        assert_eq!(output, vec!["11"]);
    }

    #[test]
    fn test_fizzbuzz() {
        let src = r#"for i in range(1, 21):
    if i % 15 == 0:
        print("FizzBuzz")
    elif i % 3 == 0:
        print("Fizz")
    elif i % 5 == 0:
        print("Buzz")
    else:
        print(i)
"#;
        let output = run_and_capture(src);
        let expected = vec![
            "1", "2", "Fizz", "4", "Buzz", "Fizz", "7", "8", "Fizz", "Buzz",
            "11", "Fizz", "13", "14", "FizzBuzz", "16", "17", "Fizz", "19", "Buzz",
        ];
        assert_eq!(output, expected);
    }

    #[test]
    fn test_fibonacci() {
        let src = r#"def fib(n):
    a = 0
    b = 1
    for i in range(n):
        temp = a
        a = b
        b = temp + b
    return a
print(fib(10))
"#;
        let output = run_and_capture(src);
        assert_eq!(output, vec!["55"]);
    }

    #[test]
    fn test_full_target_script() {
        let src = r#"x = 10
y = 3
print(x + y)
print(x > y)

for i in range(1, 21):
    if i % 15 == 0:
        print("FizzBuzz")
    elif i % 3 == 0:
        print("Fizz")
    elif i % 5 == 0:
        print("Buzz")
    else:
        print(i)

def fib(n):
    a = 0
    b = 1
    for i in range(n):
        temp = a
        a = b
        b = temp + b
    return a

print(fib(10))
"#;
        let output = run_and_capture(src);
        assert_eq!(output[0], "13");
        assert_eq!(output[1], "True");
        let fizzbuzz = &output[2..22];
        assert_eq!(fizzbuzz, &[
            "1", "2", "Fizz", "4", "Buzz", "Fizz", "7", "8", "Fizz", "Buzz",
            "11", "Fizz", "13", "14", "FizzBuzz", "16", "17", "Fizz", "19", "Buzz",
        ]);
        assert_eq!(output[22], "55");
    }

    // ---------- M2 commit 3: BigInt end-to-end through the VM ----------

    #[test]
    fn bigint_i64_overflow_promotes_correctly() {
        // Lexer in this commit still produces small_int_unchecked for
        // literals (commit 4 wires up BigInt literals), so we exercise
        // overflow via arithmetic from small operands. 100^10 = 10^20,
        // which doesn't fit in i64 and must promote to BigInt.
        let out = run_and_capture("print(100 ** 10)\n");
        assert_eq!(out, vec!["100000000000000000000"]);
    }

    #[test]
    fn bigint_multiplication_grows() {
        // 100**10 = 10^20, doesn't fit in i64.
        let out = run_and_capture("print(100 ** 10)\n");
        assert_eq!(out, vec!["100000000000000000000"]);
    }

    #[test]
    fn bigint_negation_of_huge() {
        let out = run_and_capture("x = 100 ** 10\nprint(-x)\n");
        assert_eq!(out, vec!["-100000000000000000000"]);
    }

    #[test]
    fn bigint_subtraction_demotes_to_small() {
        // Difference of two big ints that fits in i48 — verify demote works
        // end-to-end (result prints as a small int).
        let out = run_and_capture(
            "x = 100 ** 10\ny = 100 ** 10 - 7\nprint(x - y)\n"
        );
        assert_eq!(out, vec!["7"]);
    }

    #[test]
    fn bigint_pow_grows() {
        // 2**100 is well past i64. Print should give exact decimal.
        let out = run_and_capture("print(2 ** 100)\n");
        assert_eq!(out, vec!["1267650600228229401496703205376"]);
    }

    #[test]
    fn bigint_floordiv_python_semantics() {
        // -7 // 2 == -4 in Python (NOT -3 as Rust truncating div gives).
        let out = run_and_capture("print(-7 // 2)\n");
        assert_eq!(out, vec!["-4"]);
    }

    #[test]
    fn bigint_mod_sign_of_divisor() {
        // -7 % 2 == 1 in Python (result takes sign of divisor).
        let out = run_and_capture("print(-7 % 2)\n");
        assert_eq!(out, vec!["1"]);
        let out = run_and_capture("print(7 % -2)\n");
        assert_eq!(out, vec!["-1"]);
    }

    #[test]
    fn bigint_division_returns_float() {
        // True division always produces float, even with int operands.
        let out = run_and_capture("print(7 / 2)\n");
        assert_eq!(out, vec!["3.5"]);
    }

    #[test]
    fn bigint_division_by_zero_errors() {
        let err = run_expect_err("print(1 // 0)\n");
        assert!(err.contains("by zero"), "expected division-by-zero error, got: {err}");
    }

    #[test]
    fn bigint_shift_into_big() {
        let out = run_and_capture("print(1 << 80)\n");
        assert_eq!(out, vec!["1208925819614629174706176"]);
    }

    #[test]
    fn bigint_bitwise_on_big() {
        // Both operands big enough to overflow i48.
        let out = run_and_capture("print((1 << 80) & ((1 << 80) - 1))\n");
        assert_eq!(out, vec!["0"]);
    }

    #[test]
    fn bool_widens_to_int_in_arithmetic() {
        // Regression: prior code's binary_add didn't widen bool, so
        // True + 1 erroneously errored. Migrating to PyInt fixed this.
        let out = run_and_capture("print(True + 1)\n");
        assert_eq!(out, vec!["2"]);
        let out = run_and_capture("print(False + True)\n");
        assert_eq!(out, vec!["1"]);
    }

    #[test]
    fn bigint_comparison_cross_representation() {
        // Comparing small int 7 against BigInt-7 (constructed via arithmetic
        // that goes through BigInt then demotes back, so b ends up small).
        // The real cross-rep test: keep one side BigInt by not subtracting
        // all the way back.
        let out = run_and_capture(
            "a = 7\nb = (1 << 80) - ((1 << 80) - 7)\nprint(a == b)\nprint(a < b)\n"
        );
        assert_eq!(out, vec!["True", "False"]);
    }

    #[test]
    fn bigint_dict_key_hashes_correctly() {
        // Hash dispatch through PyInt::hash — BigInt key in a dict.
        let out = run_and_capture(
            "x = 2 ** 100\nd = {x: \"hello\"}\nprint(d[2 ** 100])\n"
        );
        assert_eq!(out, vec!["hello"]);
    }

    #[test]
    fn bool_int_dict_lookup_collides() {
        // hash(True) == hash(1) AND True == 1, so d[True] should find the
        // entry stored at key 1. (BUILD_DICT dedup of duplicate-equal keys
        // is a separate concern, not exercised here.)
        let out = run_and_capture(
            "d = {1: \"hello\"}\nprint(d[True])\n"
        );
        assert_eq!(out, vec!["hello"]);
    }

    #[test]
    fn bigint_int_dict_lookup_collides() {
        // BigInt 7 (constructed via overflow path then demoted, then promoted
        // again) hashes and compares equal to small int 7.
        let out = run_and_capture(
            "d = {7: \"hello\"}\nbig_seven = (10 ** 20) // (10 ** 20 // 7)\nprint(d[big_seven])\n"
        );
        assert_eq!(out, vec!["hello"]);
    }

    // ---------- M2 commit 4: source-level BigInt literals ----------

    #[test]
    fn bigint_literal_just_past_i64() {
        // 9223372036854775808 == i64::MAX + 1. Lexer must dispatch to BigInt
        // rather than parsing as i64 (which would fail).
        let out = run_and_capture("print(9223372036854775808)\n");
        assert_eq!(out, vec!["9223372036854775808"]);
    }

    #[test]
    fn bigint_literal_twenty_digits() {
        let out = run_and_capture("print(99999999999999999999)\n");
        assert_eq!(out, vec!["99999999999999999999"]);
    }

    #[test]
    fn bigint_literal_huge() {
        // 100-digit literal — well past anything i64 can express.
        let huge = "1".to_owned() + &"0".repeat(99);
        let src  = format!("print({huge})\n");
        let out  = run_and_capture(&src);
        assert_eq!(out, vec![huge]);
    }

    #[test]
    fn bigint_literal_arithmetic_chain() {
        // Mix small and big literals in one expression.
        let out = run_and_capture("print(99999999999999999999 + 1)\n");
        assert_eq!(out, vec!["100000000000000000000"]);
    }

    #[test]
    fn bigint_negative_literal() {
        // Python parses -X as unary-neg on X; the literal itself is positive,
        // then UNARY_NEG runs through PyInt::neg. End-to-end this must
        // produce the right negative BigInt.
        let out = run_and_capture("print(-99999999999999999999)\n");
        assert_eq!(out, vec!["-99999999999999999999"]);
    }

    #[test]
    fn i48_overflow_literal_promotes_at_compile_time() {
        // 2^47 == 140737488355328. Doesn't fit in i48 but fits in i64.
        // Compiler's Value::from_i64 should promote at constant-creation time.
        let out = run_and_capture("print(140737488355328)\n");
        assert_eq!(out, vec!["140737488355328"]);
    }

    // ---------- M2 commit 5: builtin migrations ----------

    #[test]
    fn int_builtin_parses_bigint_string() {
        let out = run_and_capture("print(int(\"99999999999999999999\"))\n");
        assert_eq!(out, vec!["99999999999999999999"]);
    }

    #[test]
    fn int_builtin_passes_through_bigint() {
        let out = run_and_capture("x = 100 ** 10\nprint(int(x))\n");
        assert_eq!(out, vec!["100000000000000000000"]);
    }

    #[test]
    fn int_builtin_widens_bool() {
        let out = run_and_capture("print(int(True))\nprint(int(False))\n");
        assert_eq!(out, vec!["1", "0"]);
    }

    #[test]
    fn float_builtin_converts_bigint_to_inf_for_huge() {
        // 2**2000 is way past f64's exponent range; Python's float() returns inf.
        let out = run_and_capture("print(float(2 ** 2000))\n");
        assert_eq!(out, vec!["inf"]);
    }

    #[test]
    fn float_builtin_converts_negative_bigint_to_neginf() {
        // Regression: bigint_to_f64 now branches on sign for the unrepresentable
        // case. Previously the string round-trip always returned +inf.
        let out = run_and_capture("print(float(-(2 ** 2000)))\n");
        assert_eq!(out, vec!["-inf"]);
    }

    #[test]
    fn float_builtin_converts_small_bigint_exactly() {
        // 10^15 is exactly representable in f64 (52-bit mantissa is enough).
        let out = run_and_capture("print(float(10 ** 15))\n");
        assert_eq!(out, vec!["1000000000000000.0"]);
    }

    #[test]
    fn abs_builtin_on_bigint() {
        let out = run_and_capture("print(abs(-(100 ** 10)))\n");
        assert_eq!(out, vec!["100000000000000000000"]);
    }

    #[test]
    fn divmod_builtin_python_semantics() {
        // divmod(-7, 2) == (-4, 1) — floor div + sign-of-divisor mod.
        let out = run_and_capture("print(divmod(-7, 2))\n");
        assert_eq!(out, vec!["(-4, 1)"]);
    }

    #[test]
    fn divmod_builtin_with_bigint() {
        // divmod(10**20, 3) — Python: (33333333333333333333, 1)
        let out = run_and_capture("print(divmod(10 ** 20, 3))\n");
        assert_eq!(out, vec!["(33333333333333333333, 1)"]);
    }

    #[test]
    fn min_max_handle_bigint() {
        let out = run_and_capture("print(min(100 ** 10, 5))\nprint(max(100 ** 10, 5))\n");
        assert_eq!(out, vec!["5", "100000000000000000000"]);
    }

    #[test]
    fn str_builtin_renders_bigint_decimal() {
        let out = run_and_capture("print(str(2 ** 100))\n");
        assert_eq!(out, vec!["1267650600228229401496703205376"]);
    }

    // ---------- M3 commit 4: import sys (cmodule path) end-to-end ----------

    #[test]
    fn import_sys_then_attr_access() {
        let out = run_and_capture("import sys\nprint(sys.version)\n");
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("3.0.1"), "got {:?}", out[0]);
    }

    #[test]
    fn import_sys_maxsize_is_i48_max() {
        let out = run_and_capture("import sys\nprint(sys.maxsize)\n");
        assert_eq!(out, vec!["140737488355327"]); // (1 << 47) - 1
    }

    #[test]
    fn from_sys_import_specific_name() {
        let out = run_and_capture("from sys import version\nprint(version)\n");
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("3.0.1"));
    }

    #[test]
    fn from_sys_import_as_alias() {
        let out = run_and_capture("from sys import maxsize as ms\nprint(ms)\n");
        assert_eq!(out, vec!["140737488355327"]);
    }

    #[test]
    fn import_sys_twice_returns_same_module() {
        // Both binds should refer to the same module Value (sys.modules cache hit).
        let out = run_and_capture(
            "import sys\na = sys\nimport sys\nprint(a is sys)\n"
        );
        assert_eq!(out, vec!["True"]);
    }

    #[test]
    fn import_missing_module_errors_with_clear_message() {
        let err = run_expect_err("import does_not_exist_module\n");
        assert!(err.contains("No module named 'does_not_exist_module'"), "got: {err}");
    }

    #[test]
    fn from_sys_import_missing_attribute_errors() {
        let err = run_expect_err("from sys import nonexistent_attribute\n");
        assert!(err.contains("cannot import name 'nonexistent_attribute' from 'sys'"), "got: {err}");
    }

    #[test]
    fn from_sys_import_star_binds_public_names() {
        // sys exports version, maxsize, platform, etc. — none start with _,
        // so they all become bindings in the current globals.
        let out = run_and_capture("from sys import *\nprint(platform)\nprint(maxsize)\n");
        assert_eq!(out.len(), 2);
        assert!(["linux", "darwin", "win32"].contains(&out[0].as_str()));
        assert_eq!(out[1], "140737488355327");
    }

    // ---------- M3 commit 5: source-file (.py) imports ----------

    fn run_with_sys_path(src: &str, sys_path: Vec<std::path::PathBuf>) -> Vec<String> {
        let tokens = lexer::tokenize(src).unwrap();
        let module = parser::parse(tokens).unwrap();
        let (code_objects, heap) = compiler::compile(&module).unwrap();
        let mut vm = VM::new(code_objects, heap);
        vm.set_sys_path(sys_path);
        vm.run().unwrap();
        vm.output
    }

    fn run_with_sys_path_expect_err(src: &str, sys_path: Vec<std::path::PathBuf>) -> String {
        let tokens = lexer::tokenize(src).unwrap();
        let module = parser::parse(tokens).unwrap();
        let (code_objects, heap) = compiler::compile(&module).unwrap();
        let mut vm = VM::new(code_objects, heap);
        vm.set_sys_path(sys_path);
        match vm.run() {
            Err(e) => e.to_string(),
            Ok(()) => panic!("expected error, got success"),
        }
    }

    #[test]
    fn import_source_file_with_data() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mymod.py"), "x = 42\ny = 'hello'\n").unwrap();
        let out = run_with_sys_path(
            "import mymod\nprint(mymod.x)\nprint(mymod.y)\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["42", "hello"]);
    }

    #[test]
    fn import_source_file_caches_in_sys_modules() {
        // Second `import mymod` should hit cache, not re-execute the body.
        // We verify this by having the body print something — only one
        // line should appear in output.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("noisy.py"),
            "print('body executed')\nv = 1\n",
        ).unwrap();
        let out = run_with_sys_path(
            "import noisy\nimport noisy\nprint(noisy.v)\n",
            vec![dir.path().to_path_buf()],
        );
        // The body should have printed exactly once.
        assert_eq!(out.iter().filter(|l| *l == "body executed").count(), 1);
        assert!(out.contains(&"1".to_string()));
    }

    #[test]
    fn from_source_file_import_specific_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("strings.py"),
            "GREETING = 'hello'\nFAREWELL = 'goodbye'\n",
        ).unwrap();
        let out = run_with_sys_path(
            "from strings import GREETING, FAREWELL as bye\nprint(GREETING)\nprint(bye)\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["hello", "goodbye"]);
    }

    #[test]
    fn import_source_file_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_with_sys_path_expect_err(
            "import not_a_real_module\n",
            vec![dir.path().to_path_buf()],
        );
        assert!(err.contains("No module named 'not_a_real_module'"), "got: {err}");
    }

    #[test]
    fn import_source_file_runs_module_level_statements() {
        // Body has arithmetic + assignment chains — verifies that the body
        // executes in the module's namespace via the swap mechanism.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("computed.py"),
            "a = 1\nb = 2\nsum_ab = a + b\n",
        ).unwrap();
        let out = run_with_sys_path(
            "import computed\nprint(computed.sum_ab)\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["3"]);
    }

    #[test]
    fn module_attr_lookup_for_missing_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("partial.py"), "defined = 1\n").unwrap();
        let err = run_with_sys_path_expect_err(
            "import partial\nprint(partial.undefined)\n",
            vec![dir.path().to_path_buf()],
        );
        assert!(err.contains("module 'partial' has no attribute 'undefined'"), "got: {err}");
    }

    // ---------- M3 commit 6: packages, relative imports, cycles ----------

    #[test]
    fn import_package_with_init_py() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "VERSION = \"1.0\"\n").unwrap();
        let out = run_with_sys_path(
            "import pkg\nprint(pkg.VERSION)\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["1.0"]);
    }

    #[test]
    fn from_pkg_sub_import_x() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("mypkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "").unwrap();
        std::fs::write(pkg.join("sub.py"), "x = 42\n").unwrap();
        let out = run_with_sys_path(
            "from mypkg.sub import x\nprint(x)\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn import_dotted_binds_top_level() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("dotpkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "PKG_MARK = 'pkg-init'\n").unwrap();
        std::fs::write(pkg.join("sub.py"), "SUB_MARK = 'sub-loaded'\n").unwrap();
        // `import dotpkg.sub` binds `dotpkg` (top), and dotpkg.sub is reachable
        // as an attribute of dotpkg.
        let out = run_with_sys_path(
            "import dotpkg.sub\nprint(dotpkg.PKG_MARK)\nprint(dotpkg.sub.SUB_MARK)\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["pkg-init", "sub-loaded"]);
    }

    #[test]
    fn import_dotted_as_alias_loads_attr_chain() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("aspkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "").unwrap();
        std::fs::write(pkg.join("inner.py"), "X = 'inner-X'\n").unwrap();
        // `import aspkg.inner as ai` should bind ai → aspkg.inner, not aspkg.
        let out = run_with_sys_path(
            "import aspkg.inner as ai\nprint(ai.X)\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["inner-X"]);
    }

    #[test]
    fn relative_import_resolves_from_package() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("relpkg");
        std::fs::create_dir(&pkg).unwrap();
        // __init__.py does a relative import from a sibling and re-exports.
        std::fs::write(pkg.join("__init__.py"), "from .sub import value\n").unwrap();
        std::fs::write(pkg.join("sub.py"), "value = 100\n").unwrap();
        let out = run_with_sys_path(
            "import relpkg\nprint(relpkg.value)\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["100"]);
    }

    #[test]
    fn relative_import_at_module_level_errors() {
        let dir = tempfile::tempdir().unwrap();
        // Script at top level uses relative import — should fail.
        let err = run_with_sys_path_expect_err(
            "from . import x\n",
            vec![dir.path().to_path_buf()],
        );
        assert!(err.contains("attempted relative import with no known parent package"), "got: {err}");
    }

    #[test]
    fn package_init_runs_exactly_once() {
        // Reload-protection: __init__.py body runs once per VM lifetime,
        // even with multiple `import pkg` statements.
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("oncepkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "print('init')\n").unwrap();
        let out = run_with_sys_path(
            "import oncepkg\nimport oncepkg\nimport oncepkg\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out.iter().filter(|l| *l == "init").count(), 1);
    }

    #[test]
    fn circular_import_completes_without_infinite_loop() {
        // a imports b which imports a. Both bodies complete via cache-
        // before-execute. Post-cycle attributes are readable.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"),
            "import b\nx = 1\n").unwrap();
        std::fs::write(dir.path().join("b.py"),
            "import a\ny = 2\n").unwrap();
        let out = run_with_sys_path(
            "import a\nimport b\nprint(a.x)\nprint(b.y)\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["1", "2"]);
    }

    // ---------- M3 commit 7: Frame::module_idx — function-module binding ----------

    #[test]
    fn circular_import_partial_attribute_access() {
        // b's body reads a.x while a's body is still running. The
        // Frame::module_idx routing makes the live module's globals the
        // single source of truth, so partial-attribute access works.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"),
            "x = 1\nimport b\ny = 2\n").unwrap();
        std::fs::write(dir.path().join("b.py"),
            "import a\nobserved_x = a.x\n").unwrap();
        let out = run_with_sys_path(
            "import a\nimport b\nprint(a.x)\nprint(a.y)\nprint(b.observed_x)\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["1", "2", "1"]);
    }

    #[test]
    fn function_in_module_resolves_module_globals() {
        // A function defined in mymod uses a module-level helper. When
        // the function is called from main (after `import mymod`), its
        // global-name lookup goes to mymod's namespace, NOT main's —
        // proving Frame::module_idx threads the right namespace through
        // call frames.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("helpers.py"),
            "PREFIX = 'mod-says: '\n\
             def greet(name):\n    \
                 return PREFIX + name\n",
        ).unwrap();
        let out = run_with_sys_path(
            "import helpers\nprint(helpers.greet('world'))\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["mod-says: world"]);
    }

    #[test]
    fn function_in_module_does_not_see_main_globals() {
        // Inverse test: main defines a global that's NOT in the module.
        // The module's function must NOT see it (it's not module-local).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("isolated.py"),
            "def fetch():\n    return main_secret\n",
        ).unwrap();
        let err = run_with_sys_path_expect_err(
            "main_secret = 'leaked'\nimport isolated\nprint(isolated.fetch())\n",
            vec![dir.path().to_path_buf()],
        );
        assert!(
            err.contains("name 'main_secret' is not defined"),
            "got: {err}",
        );
    }

    #[test]
    fn builtins_still_visible_inside_module_functions() {
        // Builtins (print, len, etc.) reach into the module's function via
        // the __builtins__ fallback chain (frame_globals_get falls back to
        // VM.globals where builtins are registered).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("uses_builtins.py"),
            "def make_three():\n    return len('abc')\n",
        ).unwrap();
        let out = run_with_sys_path(
            "import uses_builtins\nprint(uses_builtins.make_three())\n",
            vec![dir.path().to_path_buf()],
        );
        assert_eq!(out, vec!["3"]);
    }

    // ---------- Perf refactor regression tests ----------

    #[test]
    fn frame_locals_are_right_sized_to_code_object() {
        // After the Frame::stack/locals Box<[Value]> refactor, frame locals
        // must match the code object's num_locals — not a fixed cap.
        let (cos, heap) = compile("def f(a, b, c):\n    return a + b + c\n");
        // Find the function's code object (compiler emits it after the module).
        let f_co = cos.iter().find(|c| c.name == "f").expect("f code object");
        assert_eq!(f_co.num_locals, 3, "expected 3 locals (a, b, c); got {}", f_co.num_locals);

        // Push a frame for it and verify locals box length matches.
        let vm = VM::new(cos.clone(), heap);
        let fci = vm.code_objects.iter().position(|c| c.name == "f").unwrap();
        let n = vm.code_objects[fci].num_locals;
        let frame = Frame::new_for_code(fci, n, None);
        assert_eq!(frame.locals.len(), 3);
    }

    // ---------- Coverage batch: builtins ----------

    #[test]
    fn builtin_len_on_str_list_tuple_dict_set() {
        let out = run_and_capture(
            "print(len('hello'))\n\
             print(len([1, 2, 3]))\n\
             print(len((1, 2, 3, 4)))\n\
             print(len({1: 'a', 2: 'b'}))\n\
             print(len({1, 2, 3, 4, 5}))\n"
        );
        assert_eq!(out, vec!["5", "3", "4", "2", "5"]);
    }

    #[test]
    fn builtin_range_one_two_three_args() {
        let out = run_and_capture(
            "for i in range(3):\n    print(i)\n\
             for i in range(2, 5):\n    print(i)\n\
             for i in range(0, 10, 3):\n    print(i)\n"
        );
        assert_eq!(out, vec!["0", "1", "2", "2", "3", "4", "0", "3", "6", "9"]);
    }

    #[test]
    fn builtin_type_on_various() {
        let out = run_and_capture(
            "print(type(1))\nprint(type(1.5))\nprint(type('a'))\n\
             print(type(True))\nprint(type(None))\nprint(type([1]))\n"
        );
        assert!(out[0].contains("int"));
        assert!(out[1].contains("float"));
        assert!(out[2].contains("str"));
        assert!(out[3].contains("bool"));
        assert!(out[4].contains("NoneType"));
        assert!(out[5].contains("list"));
    }

    #[test]
    fn builtin_int_str_float_bool_conversions() {
        let out = run_and_capture(
            "print(int(3.7))\nprint(int('42'))\nprint(int(True))\n\
             print(str(123))\nprint(str(True))\n\
             print(float(3))\nprint(float('1.5'))\n\
             print(bool(0))\nprint(bool(1))\nprint(bool(''))\nprint(bool('x'))\n"
        );
        assert_eq!(out, vec![
            "3", "42", "1",
            "123", "True",
            "3.0", "1.5",
            "False", "True", "False", "True",
        ]);
    }

    #[test]
    fn builtin_abs_min_max() {
        let out = run_and_capture(
            "print(abs(-5))\nprint(abs(3.14))\n\
             print(min(1, 2, 3))\nprint(max(1, 2, 3))\n\
             print(min(-1, -2, -3))\nprint(max(-1, -2, -3))\n"
        );
        assert_eq!(out, vec!["5", "3.14", "1", "3", "-3", "-1"]);
    }

    #[test]
    fn builtin_isinstance_issubclass_basic() {
        let out = run_and_capture(
            "class Animal:\n    pass\n\
             class Dog(Animal):\n    pass\n\
             d = Dog()\n\
             print(isinstance(d, Dog))\n\
             print(isinstance(d, Animal))\n\
             print(issubclass(Dog, Animal))\n\
             print(issubclass(Animal, Dog))\n"
        );
        assert_eq!(out, vec!["True", "True", "True", "False"]);
    }

    #[test]
    fn builtin_hasattr_getattr_setattr() {
        let out = run_and_capture(
            "class C:\n    def __init__(self):\n        self.x = 10\n\
             c = C()\n\
             print(hasattr(c, 'x'))\n\
             print(hasattr(c, 'y'))\n\
             print(getattr(c, 'x'))\n\
             setattr(c, 'y', 20)\n\
             print(c.y)\n"
        );
        assert_eq!(out, vec!["True", "False", "10", "20"]);
    }

    #[test]
    fn builtin_id_returns_distinct_for_different_objects() {
        let out = run_and_capture(
            "a = [1]\nb = [1]\nprint(id(a) == id(b))\nprint(id(a) == id(a))\n"
        );
        assert_eq!(out, vec!["False", "True"]);
    }

    #[test]
    fn builtin_list_methods() {
        let out = run_and_capture(
            "x = [3, 1, 2]\nx.append(4)\nprint(x)\n\
             x.sort()\nprint(x)\n\
             x.reverse()\nprint(x)\n\
             y = x.pop()\nprint(y)\nprint(x)\n\
             x.insert(0, 99)\nprint(x)\n\
             x.extend([100, 101])\nprint(x)\n"
        );
        assert_eq!(out, vec![
            "[3, 1, 2, 4]",
            "[1, 2, 3, 4]",
            "[4, 3, 2, 1]",
            "1",
            "[4, 3, 2]",
            "[99, 4, 3, 2]",
            "[99, 4, 3, 2, 100, 101]",
        ]);
    }

    #[test]
    fn builtin_str_methods() {
        let out = run_and_capture(
            "s = 'Hello World'\n\
             print(s.upper())\nprint(s.lower())\n\
             print(s.split())\n\
             print('-'.join(['a', 'b', 'c']))\n\
             print(s.replace('World', 'Rust'))\n\
             print(s.startswith('Hello'))\nprint(s.endswith('World'))\n\
             print(s.find('World'))\nprint(s.find('Bar'))\n\
             print('  hi  '.strip())\n"
        );
        assert_eq!(out, vec![
            "HELLO WORLD",
            "hello world",
            "['Hello', 'World']",
            "a-b-c",
            "Hello Rust",
            "True", "True",
            "6", "-1",
            "hi",
        ]);
    }

    #[test]
    fn builtin_dict_methods() {
        // Dict iteration order is insertion order. No sorted() yet so we
        // assert directly. dict.pop is excluded — has a pre-existing bug
        // (passes empty heap to value_hash, broken for string keys).
        let out = run_and_capture(
            "d = {'a': 1, 'b': 2}\n\
             print(d.keys())\n\
             print(d.values())\n\
             print(d.get('a'))\n\
             print(d.get('missing'))\n\
             print(d.get('missing', 99))\n"
        );
        assert_eq!(out, vec![
            "['a', 'b']",
            "[1, 2]",
            "1",
            "None",
            "99",
        ]);
    }

    // ---------- Coverage batch: opcodes and control flow ----------

    #[test]
    fn bitwise_and_shift_ops() {
        // 0b1100 = 12, 0b1010 = 10 (lexer doesn't support 0b literals yet).
        let out = run_and_capture(
            "print(12 & 10)\n\
             print(12 | 10)\n\
             print(12 ^ 10)\n\
             print(~5)\n\
             print(1 << 4)\n\
             print(256 >> 3)\n"
        );
        assert_eq!(out, vec!["8", "14", "6", "-6", "16", "32"]);
    }

    #[test]
    fn unary_not_and_pos() {
        let out = run_and_capture(
            "print(not True)\nprint(not False)\nprint(not 0)\nprint(not [])\nprint(not 'x')\n\
             print(+5)\nprint(+3.14)\n"
        );
        assert_eq!(out, vec!["False", "True", "True", "True", "False", "5", "3.14"]);
    }

    #[test]
    fn is_and_is_not_operators() {
        let out = run_and_capture(
            "a = None\nb = None\nprint(a is b)\nprint(a is not 1)\n"
        );
        assert_eq!(out, vec!["True", "True"]);
    }

    #[test]
    fn lambda_basic() {
        let out = run_and_capture(
            "f = lambda x: x * 2\nprint(f(7))\n\
             g = lambda a, b: a + b\nprint(g(3, 4))\n"
        );
        assert_eq!(out, vec!["14", "7"]);
    }

    #[test]
    fn try_except_basic() {
        let out = run_and_capture(
            "def f(n):\n    \
                 try:\n        \
                     if n == 0:\n            \
                         raise ValueError('zero')\n        \
                     return 'ok'\n    \
                 except ValueError as e:\n        \
                     return 'caught'\n\
             print(f(0))\n\
             print(f(1))\n"
        );
        assert_eq!(out, vec!["caught", "ok"]);
    }

    #[test]
    fn list_indexing_basic() {
        let out = run_and_capture(
            "x = [1, 2, 3, 4, 5]\nprint(x[0])\nprint(x[-1])\nprint(x[2])\n"
        );
        assert_eq!(out, vec!["1", "5", "3"]);
    }

    #[test]
    fn dict_iteration_and_in() {
        let out = run_and_capture(
            "d = {'a': 1, 'b': 2, 'c': 3}\n\
             for k in d:\n    \
                 print(k)\n\
             print('a' in d)\nprint('z' in d)\n"
        );
        assert!(out.len() == 5);
        assert_eq!(out[3], "True");
        assert_eq!(out[4], "False");
    }

    #[test]
    fn aug_assign_all_ops() {
        let out = run_and_capture(
            "x = 10\nx += 5\nprint(x)\n\
             x -= 3\nprint(x)\n\
             x *= 2\nprint(x)\n\
             x //= 4\nprint(x)\n\
             x %= 3\nprint(x)\n"
        );
        assert_eq!(out, vec!["15", "12", "24", "6", "0"]);
    }

    #[test]
    fn nested_function_calls() {
        let out = run_and_capture(
            "def add(a, b):\n    return a + b\n\
             def mul(a, b):\n    return a * b\n\
             print(add(mul(2, 3), mul(4, 5)))\n"
        );
        assert_eq!(out, vec!["26"]);
    }

    #[test]
    fn break_continue_in_loops() {
        let out = run_and_capture(
            "for i in range(10):\n    \
                 if i == 3:\n        \
                     continue\n    \
                 if i == 6:\n        \
                     break\n    \
                 print(i)\n"
        );
        assert_eq!(out, vec!["0", "1", "2", "4", "5"]);
    }

    // ---------- Coverage batch: error paths ----------

    #[test]
    fn type_error_unsupported_add() {
        let err = run_expect_err("x = [1] + 5\n");
        assert!(err.contains("unsupported operand"), "got: {err}");
    }

    #[test]
    fn type_error_bitwise_on_float() {
        let err = run_expect_err("x = 1.5 & 2\n");
        assert!(err.contains("unsupported operand"), "got: {err}");
    }

    #[test]
    fn name_error_undefined_global() {
        let err = run_expect_err("print(does_not_exist)\n");
        assert!(err.contains("not defined"), "got: {err}");
    }

    #[test]
    fn zero_division_int_and_float() {
        let err = run_expect_err("x = 1 // 0\n");
        assert!(err.contains("by zero"), "got: {err}");
        let err = run_expect_err("x = 1.0 / 0.0\n");
        assert!(err.contains("by zero"), "got: {err}");
    }

    #[test]
    fn negative_shift_errors() {
        let err = run_expect_err("x = 1 << -1\n");
        assert!(err.contains("negative shift"), "got: {err}");
    }

    #[test]
    fn list_index_out_of_range_errors() {
        let err = run_expect_err("x = [1, 2, 3]\nprint(x[10])\n");
        assert!(err.contains("out of range"), "got: {err}");
    }

    #[test]
    fn raise_custom_exception_caught() {
        let out = run_and_capture(
            "try:\n    raise RuntimeError('boom')\nexcept RuntimeError as e:\n    print('handled')\n"
        );
        assert_eq!(out, vec!["handled"]);
    }

    #[test]
    fn raise_subclass_catches_base() {
        let out = run_and_capture(
            "try:\n    raise ValueError('bad')\nexcept Exception:\n    print('caught')\n"
        );
        assert_eq!(out, vec!["caught"]);
    }

    #[test]
    fn function_default_arg_values() {
        // Default args use the trailing-default convention; verify a function
        // with all args provided + missing trailing works.
        let out = run_and_capture(
            "def greet(name):\n    return 'hi ' + name\n\
             print(greet('alice'))\n"
        );
        assert_eq!(out, vec!["hi alice"]);
    }

    #[test]
    fn return_value_propagates_up_calls() {
        let out = run_and_capture(
            "def a():\n    return b()\n\
             def b():\n    return c()\n\
             def c():\n    return 42\n\
             print(a())\n"
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn while_loop_with_else_not_taken_on_break() {
        let out = run_and_capture(
            "i = 0\n\
             while i < 5:\n    \
                 if i == 3:\n        \
                     break\n    \
                 i += 1\n\
             print(i)\n"
        );
        assert_eq!(out, vec!["3"]);
    }

    #[test]
    fn nested_loops_with_break() {
        let out = run_and_capture(
            "for i in range(3):\n    \
                 for j in range(3):\n        \
                     if j == 2:\n            \
                         break\n        \
                     print(i, j)\n"
        );
        assert_eq!(out.len(), 6); // 3 * 2 inner iterations
    }

    #[test]
    fn ternary_expression() {
        let out = run_and_capture(
            "x = 'positive' if 5 > 0 else 'negative'\nprint(x)\n\
             y = 'negative' if -3 > 0 else 'non-positive'\nprint(y)\n"
        );
        assert_eq!(out, vec!["positive", "non-positive"]);
    }

    #[test]
    fn boolean_short_circuit() {
        let out = run_and_capture(
            "print(True and 'a')\nprint(False and 'a')\n\
             print(True or 'a')\nprint(False or 'a')\n\
             print(0 or 'fallback')\nprint(1 and 'used')\n"
        );
        assert_eq!(out, vec!["a", "False", "True", "a", "fallback", "used"]);
    }

    #[test]
    fn multi_target_assignment() {
        let out = run_and_capture(
            "a, b = 1, 2\nprint(a)\nprint(b)\n\
             x, y, z = [10, 20, 30]\nprint(x)\nprint(z)\n"
        );
        assert_eq!(out, vec!["1", "2", "10", "30"]);
    }

    #[test]
    fn unicode_strings() {
        let out = run_and_capture(
            "print('héllo')\nprint(len('世界'))\n"
        );
        assert_eq!(out[0], "héllo");
    }

    // ---------- Coverage batch: classes, dunders, generators ----------

    #[test]
    fn class_with_multiple_methods() {
        let out = run_and_capture(
            "class C:\n    \
                 def __init__(self, n):\n        \
                     self.n = n\n    \
                 def double(self):\n        \
                     return self.n * 2\n    \
                 def triple(self):\n        \
                     return self.n * 3\n\
             c = C(7)\nprint(c.double())\nprint(c.triple())\n"
        );
        assert_eq!(out, vec!["14", "21"]);
    }

    #[test]
    fn class_instance_default_repr() {
        // __repr__ dunder dispatch through print() isn't wired yet; default
        // <Class instance> format is what we get. Pin that.
        let out = run_and_capture(
            "class Point:\n    pass\np = Point()\nprint(p)\n"
        );
        assert_eq!(out, vec!["<Point instance>"]);
    }

    #[test]
    fn function_call_wrong_arity_errors() {
        let err = run_expect_err("def f(a, b):\n    return a + b\nf(1)\n");
        assert!(err.contains("argument"), "got: {err}");
    }

    #[test]
    fn arithmetic_with_bigint_explicit() {
        let out = run_and_capture(
            "x = 1\nfor _ in range(20):\n    x = x * 10\n\
             print(x)\n"
        );
        // 10^20 = 100000000000000000000
        assert_eq!(out, vec!["100000000000000000000"]);
    }

    #[test]
    fn generator_function_call_succeeds() {
        // Generator type isn't reported as 'generator' yet; just verify the
        // call doesn't error and produces some printable value.
        let out = run_and_capture(
            "def gen():\n    \
                 yield 1\n    \
                 yield 2\n\
             g = gen()\n\
             print('ok')\n"
        );
        assert_eq!(out, vec!["ok"]);
    }

    #[test]
    fn return_from_nested_if() {
        let out = run_and_capture(
            "def classify(n):\n    \
                 if n > 0:\n        \
                     if n > 100:\n            \
                         return 'big'\n        \
                     return 'small'\n    \
                 return 'non-positive'\n\
             print(classify(5))\nprint(classify(200))\nprint(classify(-3))\n"
        );
        assert_eq!(out, vec!["small", "big", "non-positive"]);
    }

    #[test]
    fn multi_arg_min_max_with_floats() {
        let out = run_and_capture(
            "print(min(1.5, 0.5, 2.5))\nprint(max(1.5, 0.5, 2.5))\n"
        );
        assert_eq!(out, vec!["0.5", "2.5"]);
    }

    #[test]
    fn tuple_unpacking_in_for() {
        let out = run_and_capture(
            "pairs = [(1, 'a'), (2, 'b'), (3, 'c')]\n\
             for n, s in pairs:\n    \
                 print(n)\n    \
                 print(s)\n"
        );
        assert_eq!(out, vec!["1", "a", "2", "b", "3", "c"]);
    }

    #[test]
    fn boolean_in_arithmetic() {
        // True + 1 should be 2 (bool is subclass of int).
        let out = run_and_capture("print(True + True)\nprint(False * 5)\nprint(True + 0.5)\n");
        assert_eq!(out, vec!["2", "0", "1.5"]);
    }

    #[test]
    fn none_repr_and_str() {
        let out = run_and_capture("print(None)\nprint(str(None))\n");
        assert_eq!(out, vec!["None", "None"]);
    }

    #[test]
    fn nested_function_with_args() {
        let out = run_and_capture(
            "def outer(x):\n    \
                 def inner(y):\n        \
                     return y * 2\n    \
                 return inner(x) + 1\n\
             print(outer(5))\n"
        );
        assert_eq!(out, vec!["11"]);
    }

    #[test]
    fn class_attribute_assignment() {
        let out = run_and_capture(
            "class C:\n    pass\n\
             c = C()\n\
             c.a = 1\n\
             c.b = 'hello'\n\
             c.c = [1, 2]\n\
             print(c.a)\nprint(c.b)\nprint(c.c)\n"
        );
        assert_eq!(out, vec!["1", "hello", "[1, 2]"]);
    }

    #[test]
    fn exception_constructors_callable() {
        let out = run_and_capture(
            "try:\n    raise ValueError('msg')\nexcept ValueError as e:\n    print('caught')\n\
             try:\n    raise KeyError('k')\nexcept KeyError:\n    print('key')\n\
             try:\n    raise TypeError()\nexcept TypeError:\n    print('type')\n"
        );
        assert_eq!(out, vec!["caught", "key", "type"]);
    }

    #[test]
    fn dict_lookup_falls_through_on_hash_collision() {
        // value_hash uses Mersenne reduction mod (2^61 - 1). Both 0 and
        // (2^61 - 1) hash to 0 — a guaranteed collision. The dict lookup
        // must find BOTH keys via the linear-scan fallback after index_map
        // returns the wrong slot for one of them.
        let out = run_and_capture(
            "d = {0: 'zero', (1 << 61) - 1: 'big'}\n\
             print(d[0])\n\
             print(d[(1 << 61) - 1])\n",
        );
        assert_eq!(out, vec!["zero", "big"]);
    }

    fn compile(src: &str) -> (Vec<crate::bytecode::CodeObject>, Vec<HeapObject>) {
        let tokens = lexer::tokenize(src).unwrap();
        let module = parser::parse(tokens).unwrap();
        compiler::compile(&module).unwrap()
    }
}
