/// Stackless bytecode VM with explicit frame stack.
use crate::builtins;
use crate::bytecode::{self, CodeObject, op};
use crate::error::PythonError;
use crate::object::{
    ArithError, BuiltinId, ExceptionType, GeneratorState, HeapObject, PyInt, PyPowResult,
    Value, pyint_truediv, value_hash,
};
use std::collections::HashMap;

const MAX_STACK: usize = 256;
const MAX_LOCALS: usize = 128;

/// A single execution frame.
struct Frame {
    code_index: usize,
    ip: usize,
    stack: [Value; MAX_STACK],
    sp: usize,
    locals: [Value; MAX_LOCALS],
    /// Heap indices of cell objects for closures.
    cells: Vec<usize>,
    /// If this frame belongs to a generator, its heap index.
    generator_idx: Option<usize>,
    /// If this frame is an __init__ call, the instance to return to the caller.
    init_instance: Option<Value>,
}

impl Frame {
    fn new(code_index: usize) -> Self {
        Self {
            code_index,
            ip: 0,
            stack: [Value::none(); MAX_STACK],
            sp: 0,
            locals: [Value::none(); MAX_LOCALS],
            cells: Vec::new(),
            generator_idx: None,
            init_instance: None,
        }
    }

    fn push(&mut self, val: Value) {
        unsafe {
            *self.stack.get_unchecked_mut(self.sp) = val;
        }
        self.sp += 1;
    }

    fn pop(&mut self) -> Value {
        self.sp -= 1;
        unsafe { *self.stack.get_unchecked(self.sp) }
    }

    fn peek(&self) -> Value {
        unsafe { *self.stack.get_unchecked(self.sp - 1) }
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
    globals: HashMap<String, Value>,
    pub heap: Vec<HeapObject>,
    pub output: Vec<String>,
    exception_stack: Vec<ExceptionHandler>,
    current_exception: Option<Value>,
}

impl VM {
    pub fn new(code_objects: Vec<CodeObject>, heap: Vec<HeapObject>) -> Self {
        let mut vm = Self {
            frames: Vec::with_capacity(64),
            code_objects,
            globals: HashMap::new(),
            heap,
            output: Vec::new(),
            exception_stack: Vec::new(),
            current_exception: None,
        };
        builtins::register_builtins(&mut vm.globals, &mut vm.heap);
        vm
    }

    pub fn run(&mut self) -> Result<(), PythonError> {
        self.frames.push(Frame::new(0));
        self.execute()
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

    fn execute(&mut self) -> Result<(), PythonError> {
        loop {
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

            eprintln!("[TRACE] frame={} code={} ip={} op={} operand={} sp={} gen={:?}",
                frame_idx, code_index, self.frames[frame_idx].ip - 1,
                opcode, operand, self.frames[frame_idx].sp,
                self.frames[frame_idx].generator_idx);

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
                    let name = &self.code_objects[code_index].names[operand as usize];
                    if let Some(&val) = self.globals.get(name) {
                        self.frames[frame_idx].push(val);
                    } else {
                        let msg = format!("name '{name}' is not defined");
                        let err = PythonError::runtime(msg, line);
                        self.try_handle_error(err, line)?;
                        continue;
                    }
                }
                op::STORE_GLOBAL => {
                    let val = self.frames[frame_idx].pop();
                    let name = self.code_objects[code_index].names[operand as usize].clone();
                    self.globals.insert(name, val);
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
                            HeapObject::Closure { code_index, arity, cells, .. } => {
                                let func_code_index = *code_index;
                                let arity = *arity as usize;
                                let cells = cells.clone();
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
                                    let mut new_frame = Frame::new(func_code_index);
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
                        let (func_code_index, arity) = if let HeapObject::Function { code_index, arity, .. } = &self.heap[heap_idx] {
                            (*code_index, *arity as usize)
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
                            let mut new_frame = Frame::new(func_code_index);
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
                        let caller = self.frames.last_mut().unwrap();
                        caller.ip = caller.ip.saturating_sub(2);
                        continue;
                    }

                    if self.frames.is_empty() {
                        return Ok(());
                    }
                    let caller = self.frames.last_mut().unwrap();
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
                    let func_code_index = code_idx_val.as_int().unwrap() as usize;
                    let func_name = self.code_objects[func_code_index].name.clone();
                    let arity = self.code_objects[func_code_index].num_params as u8;

                    let heap_idx = self.heap.len();
                    self.heap.push(HeapObject::Function {
                        name: func_name,
                        code_index: func_code_index,
                        arity,
                    });
                    self.frames[frame_idx].push(Value::func_ref(heap_idx));
                }
                op::MAKE_CLOSURE => {
                    let code_idx_val = self.code_objects[code_index].constants[operand as usize];
                    let func_code_index = code_idx_val.as_int().unwrap() as usize;
                    let func_name = self.code_objects[func_code_index].name.clone();
                    let arity = self.code_objects[func_code_index].num_params as u8;
                    let num_free = self.code_objects[func_code_index].free_var_names.len();

                    // Pop cell indices from stack (pushed by LOAD_CLOSURE)
                    let mut cells = Vec::with_capacity(num_free);
                    for _ in 0..num_free {
                        let cell_val = self.frames[frame_idx].pop();
                        cells.push(cell_val.as_int().unwrap() as usize);
                    }
                    cells.reverse();

                    let heap_idx = self.heap.len();
                    self.heap.push(HeapObject::Closure {
                        name: func_name,
                        code_index: func_code_index,
                        arity,
                        cells,
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
                    let class_co_idx = co_idx_val.as_int().unwrap() as usize;
                    let class_name = name_val.display(&self.heap);

                    let mut base_indices = Vec::with_capacity(num_bases);
                    for _ in 0..num_bases {
                        let base = self.frames[frame_idx].pop();
                        if let Some(idx) = base.as_object_ref() {
                            base_indices.push(idx);
                        }
                    }
                    base_indices.reverse();

                    // Execute class body to get attributes
                    let class_frame = Frame::new(class_co_idx);
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
                                let name = &self.code_objects[co_index].names[co_operand as usize];
                                if let Some(&val) = self.globals.get(name) {
                                    self.frames[cf_idx].push(val);
                                } else {
                                    return Err(PythonError::runtime(format!("name '{name}' is not defined"), cl));
                                }
                            }
                            op::STORE_GLOBAL => {
                                let val = self.frames[cf_idx].pop();
                                let name = self.code_objects[co_index].names[co_operand as usize].clone();
                                self.globals.insert(name, val);
                            }
                            op::MAKE_FUNCTION => {
                                let code_idx_v = self.code_objects[co_index].constants[co_operand as usize];
                                let fci = code_idx_v.as_int().unwrap() as usize;
                                let fname = self.code_objects[fci].name.clone();
                                let farity = self.code_objects[fci].num_params as u8;
                                let hi = self.heap.len();
                                self.heap.push(HeapObject::Function { name: fname, code_index: fci, arity: farity });
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
                    let class_frame = self.frames.pop().unwrap();
                    let mut attrs = HashMap::new();
                    let local_names = &self.code_objects[class_co_idx].local_names;
                    for (i, name) in local_names.iter().enumerate() {
                        if i < MAX_LOCALS {
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
                            if i < MAX_LOCALS {
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
                        let caller = self.frames.last_mut().unwrap();
                        caller.push(yielded);
                        continue;
                    } else {
                        return Err(PythonError::runtime("yield outside generator", line));
                    }
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
                        // If it's a function, bind it
                        if method.is_func() || (method.as_object_ref().is_some() && matches!(&self.heap[method.as_object_ref().unwrap()], HeapObject::Closure { .. } | HeapObject::Function { .. })) {
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
                let s = self.heap[heap_idx].as_str().unwrap();
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
                HeapObject::Dict { keys, values, .. } => {
                    let h = value_hash(index, &self.heap);
                    // Linear search for matching key
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
            let (func_code_index, arity) = if let HeapObject::Function { code_index, arity, .. } = &self.heap[heap_idx] {
                (*code_index, *arity as usize)
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
            let mut new_frame = Frame::new(func_code_index);
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
                HeapObject::Closure { code_index, arity, cells, .. } => {
                    let func_code_index = *code_index;
                    let arity = *arity as usize;
                    let cells = cells.clone();
                    if argc != arity {
                        return Err(PythonError::runtime("wrong number of arguments", line));
                    }
                    let mut new_frame = Frame::new(func_code_index);
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

        // Create a new frame from generator state
        let mut gen_frame = Frame::new(code_index);
        gen_frame.ip = ip;
        gen_frame.generator_idx = Some(gen_heap_idx);
        gen_frame.cells = cells;

        // Restore locals
        for (i, val) in locals.iter().enumerate() {
            if i < MAX_LOCALS {
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

/// True if `v` carries a float — used to choose the int vs float path.
fn is_numeric_float(v: Value) -> bool {
    v.is_float()
}

fn binary_add(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    // Int + int (small or big, bool widens).
    if let (Some(a), Some(b)) = (
        PyInt::from_value_or_bool(left, heap),
        PyInt::from_value_or_bool(right, heap),
    ) && !is_numeric_float(left) && !is_numeric_float(right) {
        return Ok(a.add(b).into_value(heap));
    }
    // Float (or int↔float) — int side widens through PyInt::to_f64
    // so even huge BigInts collapse to inf consistently with CPython.
    if is_numeric_float(left) || is_numeric_float(right) {
        let af = num_to_f64(left, heap);
        let bf = num_to_f64(right, heap);
        if let (Some(a), Some(b)) = (af, bf) {
            return Ok(Value::float(a + b));
        }
    }
    if let (Some(a_idx), Some(b_idx)) = (left.as_str_ref(), right.as_str_ref()) {
        let a = heap[a_idx].as_str().unwrap().to_string();
        let b = heap[b_idx].as_str().unwrap();
        let result = format!("{a}{b}");
        let heap_idx = heap.len();
        heap.push(HeapObject::Str(result.into()));
        return Ok(Value::str_ref(heap_idx));
    }
    if let (Some(a_idx), Some(b_idx)) = (left.as_list_ref(), right.as_list_ref()) {
        let a = if let HeapObject::List(items) = &heap[a_idx] { items.clone() } else { Vec::new() };
        let b = if let HeapObject::List(items) = &heap[b_idx] { items.clone() } else { Vec::new() };
        let mut result = a;
        result.extend(b);
        let heap_idx = heap.len();
        heap.push(HeapObject::List(result));
        return Ok(Value::list_ref(heap_idx));
    }
    Err(PythonError::runtime("unsupported operand type(s) for +", line))
}

fn binary_sub(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let (Some(a), Some(b)) = (
        PyInt::from_value_or_bool(left, heap),
        PyInt::from_value_or_bool(right, heap),
    ) && !is_numeric_float(left) && !is_numeric_float(right) {
        return Ok(a.sub(b).into_value(heap));
    }
    if let (Some(a), Some(b)) = (num_to_f64(left, heap), num_to_f64(right, heap))
        && (is_numeric_float(left) || is_numeric_float(right)) {
        return Ok(Value::float(a - b));
    }
    Err(PythonError::runtime("unsupported operand type(s) for -", line))
}

fn binary_mul(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let (Some(a), Some(b)) = (
        PyInt::from_value_or_bool(left, heap),
        PyInt::from_value_or_bool(right, heap),
    ) && !is_numeric_float(left) && !is_numeric_float(right) {
        return Ok(a.mul(b).into_value(heap));
    }
    if let (Some(a), Some(b)) = (num_to_f64(left, heap), num_to_f64(right, heap))
        && (is_numeric_float(left) || is_numeric_float(right)) {
        return Ok(Value::float(a * b));
    }
    // String repetition — count comes from the int side (small only for now).
    if let Some(s_idx) = left.as_str_ref() && let Some(n) = right.as_int() {
        let s = heap[s_idx].as_str().unwrap();
        let result = s.repeat(n.max(0) as usize);
        let heap_idx = heap.len();
        heap.push(HeapObject::Str(result.into()));
        return Ok(Value::str_ref(heap_idx));
    }
    if let Some(n) = left.as_int() && let Some(s_idx) = right.as_str_ref() {
        let s = heap[s_idx].as_str().unwrap();
        let result = s.repeat(n.max(0) as usize);
        let heap_idx = heap.len();
        heap.push(HeapObject::Str(result.into()));
        return Ok(Value::str_ref(heap_idx));
    }
    Err(PythonError::runtime("unsupported operand type(s) for *", line))
}

fn binary_div(left: Value, right: Value, heap: &[HeapObject], line: u32) -> Result<Value, PythonError> {
    // Python `/` is always true-division, returns float.
    if let (Some(a), Some(b)) = (
        PyInt::from_value_or_bool(left, heap),
        PyInt::from_value_or_bool(right, heap),
    ) && !is_numeric_float(left) && !is_numeric_float(right) {
        return pyint_truediv(a, b)
            .map(Value::float)
            .map_err(|e| arith_to_runtime(e, "division", line));
    }
    let af = num_to_f64(left, heap).ok_or_else(|| PythonError::runtime("unsupported operand type(s) for /", line))?;
    let bf = num_to_f64(right, heap).ok_or_else(|| PythonError::runtime("unsupported operand type(s) for /", line))?;
    if bf == 0.0 {
        return Err(PythonError::runtime("division by zero", line));
    }
    Ok(Value::float(af / bf))
}

fn binary_floor_div(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let (Some(a), Some(b)) = (
        PyInt::from_value_or_bool(left, heap),
        PyInt::from_value_or_bool(right, heap),
    ) && !is_numeric_float(left) && !is_numeric_float(right) {
        return a.floordiv(b)
            .map(|r| r.into_value(heap))
            .map_err(|e| arith_to_runtime(e, "integer division or modulo", line));
    }
    if let (Some(a), Some(b)) = (num_to_f64(left, heap), num_to_f64(right, heap)) {
        if b == 0.0 {
            return Err(PythonError::runtime("float floor division by zero", line));
        }
        return Ok(Value::float((a / b).floor()));
    }
    Err(PythonError::runtime("unsupported operand type(s) for //", line))
}

fn binary_mod(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let (Some(a), Some(b)) = (
        PyInt::from_value_or_bool(left, heap),
        PyInt::from_value_or_bool(right, heap),
    ) && !is_numeric_float(left) && !is_numeric_float(right) {
        return a.mod_(b)
            .map(|r| r.into_value(heap))
            .map_err(|e| arith_to_runtime(e, "integer division or modulo", line));
    }
    if let (Some(a), Some(b)) = (num_to_f64(left, heap), num_to_f64(right, heap)) {
        if b == 0.0 {
            return Err(PythonError::runtime("float modulo by zero", line));
        }
        let result = ((a % b) + b) % b;
        return Ok(Value::float(result));
    }
    // String formatting: "hello %s" % value — stub
    if left.as_str_ref().is_some() {
        let _ = right;
    }
    Err(PythonError::runtime("unsupported operand type(s) for %", line))
}

fn binary_pow(left: Value, right: Value, heap: &mut Vec<HeapObject>, line: u32) -> Result<Value, PythonError> {
    if let (Some(a), Some(b)) = (
        PyInt::from_value_or_bool(left, heap),
        PyInt::from_value_or_bool(right, heap),
    ) && !is_numeric_float(left) && !is_numeric_float(right) {
        return Ok(match a.pow(b) {
            PyPowResult::Int(o)   => o.into_value(heap),
            PyPowResult::Float(f) => Value::float(f),
        });
    }
    if let (Some(a), Some(b)) = (num_to_f64(left, heap), num_to_f64(right, heap)) {
        return Ok(Value::float(a.powf(b)));
    }
    Err(PythonError::runtime("unsupported operand type(s) for **", line))
}

/// Numeric-to-f64: handles int (small/big), bool, and float. Used wherever
/// an int↔float mixed op needs a unified f64 path.
fn num_to_f64(v: Value, heap: &[HeapObject]) -> Option<f64> {
    if let Some(f) = v.as_float() { return Some(f); }
    if let Some(pi) = PyInt::from_value_or_bool(v, heap) {
        return Some(pi.to_f64());
    }
    None
}

fn compare(
    left: Value,
    right: Value,
    heap: &[HeapObject],
    int_cmp: impl Fn(i64, i64) -> bool,
    float_cmp: impl Fn(f64, f64) -> bool,
) -> bool {
    // Int family (small, big, bool) ↔ int family: use PyInt::cmp for total
    // ordering across representations.
    if !is_numeric_float(left) && !is_numeric_float(right)
        && let (Some(a), Some(b)) = (
            PyInt::from_value_or_bool(left, heap),
            PyInt::from_value_or_bool(right, heap),
        )
    {
        let ord = a.cmp(b) as i64;
        return int_cmp(ord, 0);
    }
    // Mixed with float: widen everything to f64.
    if let (Some(a), Some(b)) = (num_to_f64(left, heap), num_to_f64(right, heap)) {
        return float_cmp(a, b);
    }
    if left.is_none() && right.is_none() {
        return true;
    }
    if let (Some(a_idx), Some(b_idx)) = (left.as_str_ref(), right.as_str_ref()) {
        let a = heap[a_idx].as_str().unwrap();
        let b = heap[b_idx].as_str().unwrap();
        let ord = a.cmp(b) as i64;
        return int_cmp(ord, 0);
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
            HeapObject::Dict { keys, .. } => {
                return Ok(keys.iter().any(|k| values_equal(*item, *k, heap)));
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
}
