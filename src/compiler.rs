/// Compiles AST into bytecode.
use crate::ast::*;
use crate::bytecode::{self, CodeObject, encode, op};
use crate::error::PythonError;
use crate::object::{HeapObject, Value};
use std::collections::HashSet;

/// Compile a module AST into code objects and initial heap objects.
pub fn compile(module: &Module) -> Result<(Vec<CodeObject>, Vec<HeapObject>), PythonError> {
    let mut compiler = Compiler::new();
    compiler.compile_module(module)?;
    Ok((compiler.code_objects, compiler.heap))
}

struct Compiler {
    code_objects: Vec<CodeObject>,
    heap: Vec<HeapObject>,
    code_stack: Vec<usize>,
    loop_stack: Vec<LoopContext>,
    /// Scope info: (globals, nonlocals, cell_vars, free_vars) per code object index
    scope_info: Vec<ScopeInfo>,
}

struct LoopContext {
    break_patches: Vec<usize>,
    continue_target: usize,
}

#[derive(Clone, Default)]
struct ScopeInfo {
    globals: HashSet<String>,
    nonlocals: HashSet<String>,
    cell_vars: HashSet<String>,
    free_vars: HashSet<String>,
    locals: HashSet<String>,
}

impl Compiler {
    fn new() -> Self {
        Self {
            code_objects: Vec::new(),
            heap: Vec::new(),
            code_stack: Vec::new(),
            loop_stack: Vec::new(),
            scope_info: Vec::new(),
        }
    }

    fn current_code(&mut self) -> &mut CodeObject {
        let idx = *self.code_stack.last().unwrap();
        &mut self.code_objects[idx]
    }

    fn current_code_index(&self) -> usize {
        *self.code_stack.last().unwrap()
    }

    fn emit(&mut self, opcode: u8, operand: u32, line: u32) {
        let code = self.current_code();
        code.instructions.push(encode(opcode, operand));
        code.line_table.push(line);
    }

    fn current_offset(&self) -> usize {
        let idx = *self.code_stack.last().unwrap();
        self.code_objects[idx].instructions.len()
    }

    fn patch_jump(&mut self, instr_idx: usize) {
        let target = self.current_offset() as u32;
        let code = self.current_code();
        let old = code.instructions[instr_idx];
        let opcode = bytecode::decode_op(old);
        code.instructions[instr_idx] = encode(opcode, target);
    }

    fn add_const(&mut self, val: Value) -> u32 {
        let code = self.current_code();
        for (i, c) in code.constants.iter().enumerate() {
            if c.bits_eq(val) {
                return i as u32;
            }
        }
        let idx = code.constants.len();
        code.constants.push(val);
        idx as u32
    }

    fn add_name(&mut self, name: &str) -> u32 {
        let code = self.current_code();
        for (i, n) in code.names.iter().enumerate() {
            if n == name {
                return i as u32;
            }
        }
        let idx = code.names.len();
        code.names.push(name.to_string());
        idx as u32
    }

    fn add_local(&mut self, name: &str) -> u32 {
        let code = self.current_code();
        for (i, n) in code.local_names.iter().enumerate() {
            if n == name {
                return i as u32;
            }
        }
        let idx = code.local_names.len();
        code.local_names.push(name.to_string());
        code.num_locals = code.local_names.len();
        idx as u32
    }

    fn find_local(&self, name: &str) -> Option<u32> {
        let idx = *self.code_stack.last().unwrap();
        let code = &self.code_objects[idx];
        code.local_names.iter().position(|n| n == name).map(|i| i as u32)
    }

    fn is_module_level(&self) -> bool {
        self.code_stack.len() == 1
    }

    fn add_string_const(&mut self, s: &str) -> u32 {
        let heap_idx = self.heap.len();
        self.heap.push(HeapObject::Str(s.into()));
        let val = Value::str_ref(heap_idx);
        self.add_const(val)
    }

    /// Check if a name is a cell variable in the current scope.
    #[allow(dead_code)]
    fn is_cell_var(&self, name: &str) -> bool {
        let co_idx = self.current_code_index();
        if co_idx < self.scope_info.len() {
            self.scope_info[co_idx].cell_vars.contains(name)
        } else {
            false
        }
    }

    /// Check if a name is a free variable in the current scope.
    #[allow(dead_code)]
    fn is_free_var(&self, name: &str) -> bool {
        let co_idx = self.current_code_index();
        if co_idx < self.scope_info.len() {
            self.scope_info[co_idx].free_vars.contains(name)
        } else {
            false
        }
    }

    /// Check if a name is declared global in the current scope.
    fn is_global_decl(&self, name: &str) -> bool {
        let co_idx = self.current_code_index();
        if co_idx < self.scope_info.len() {
            self.scope_info[co_idx].globals.contains(name)
        } else {
            false
        }
    }

    /// Find the deref index for a cell or free variable.
    fn find_deref(&self, name: &str) -> Option<u32> {
        let co_idx = self.current_code_index();
        let code = &self.code_objects[co_idx];
        // Cell vars come first, then free vars
        for (i, n) in code.cell_var_names.iter().enumerate() {
            if n == name {
                return Some(i as u32);
            }
        }
        let offset = code.cell_var_names.len();
        for (i, n) in code.free_var_names.iter().enumerate() {
            if n == name {
                return Some((offset + i) as u32);
            }
        }
        None
    }

    #[allow(dead_code)]
    fn has_closures(&self) -> bool {
        let co_idx = self.current_code_index();
        if co_idx < self.scope_info.len() {
            !self.scope_info[co_idx].cell_vars.is_empty() || !self.scope_info[co_idx].free_vars.is_empty()
        } else {
            false
        }
    }

    fn compile_module(&mut self, module: &Module) -> Result<(), PythonError> {
        // First pass: analyze scopes for closures
        self.analyze_all_scopes(module);

        let co_idx = self.code_objects.len();
        self.code_objects.push(CodeObject::new("<module>"));
        self.code_stack.push(co_idx);

        for stmt in &module.body {
            self.compile_stmt(stmt)?;
        }

        self.emit(op::HALT, 0, 0);
        self.code_stack.pop();
        Ok(())
    }

    /// Simple scope analysis: find cell_vars and free_vars for closures.
    fn analyze_all_scopes(&mut self, module: &Module) {
        // For module level
        self.scope_info.push(ScopeInfo::default());
        // We do a simple analysis: scan for nested functions and their variable usage
        self.analyze_scope_stmts(&module.body, 0);
    }

    fn analyze_scope_stmts(&mut self, stmts: &[Stmt], _parent_scope: usize) {
        for stmt in stmts {
            if let Stmt::FunctionDef { body, .. } | Stmt::ClassDef { body, .. } = stmt {
                let co_idx = self.scope_info.len();
                let mut scope = ScopeInfo::default();

                // Collect globals and nonlocals
                collect_declarations(body, &mut scope);
                // Collect assignment targets as locals
                collect_locals(body, &mut scope);

                // For function defs, add params as locals
                if let Stmt::FunctionDef { params, .. } = stmt {
                    for p in params {
                        scope.locals.insert(p.clone());
                    }
                }

                self.scope_info.push(scope);

                // Recurse into body
                self.analyze_scope_stmts(body, co_idx);

                // After analyzing children, compute free_vars and cell_vars
                self.compute_free_vars(co_idx);
            }
        }
    }

    fn compute_free_vars(&mut self, scope_idx: usize) {
        // Find variables referenced in inner scopes that are local to this scope
        // This is simplified: we just look at nonlocal declarations
        let scope = &self.scope_info[scope_idx];
        let nonlocals: Vec<String> = scope.nonlocals.iter().cloned().collect();

        // Mark nonlocals as free vars in this scope
        for name in &nonlocals {
            self.scope_info[scope_idx].free_vars.insert(name.clone());
        }

        // Find the enclosing scope that has these as locals and mark them as cell vars
        // For simplicity, check all previous scopes
        for name in &nonlocals {
            for i in (0..scope_idx).rev() {
                if self.scope_info[i].locals.contains(name) {
                    self.scope_info[i].cell_vars.insert(name.clone());
                    break;
                }
            }
        }

        // Also detect closures: inner function references outer locals
        // Scan body of functions at this scope level for references to locals
        // This is a simplified version - we just handle explicit nonlocal declarations
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), PythonError> {
        match stmt {
            Stmt::Assign { target, value, line } => {
                self.compile_expr(value)?;
                self.compile_store_target(target, *line)?;
            }
            Stmt::AugAssign { target, op, value, line } => {
                self.compile_load_target(target, *line)?;
                self.compile_expr(value)?;
                let binop = self.binop_to_opcode(op);
                self.emit(binop, 0, *line);
                self.compile_store_target(target, *line)?;
            }
            Stmt::ExprStmt { expr, line } => {
                self.compile_expr(expr)?;
                self.emit(op::POP_TOP, 0, *line);
            }
            Stmt::If { condition, body, elif_clauses, else_body, line } => {
                self.compile_if(condition, body, elif_clauses, else_body, *line)?;
            }
            Stmt::While { condition, body, line } => {
                self.compile_while(condition, body, *line)?;
            }
            Stmt::For { target, iter, body, line } => {
                self.compile_for(target, iter, body, *line)?;
            }
            Stmt::FunctionDef { name, params, body, decorators, line } => {
                self.compile_function_def(name, params, body, decorators, *line)?;
            }
            Stmt::Return { value, line } => {
                if let Some(val) = value {
                    self.compile_expr(val)?;
                } else {
                    let none_idx = self.add_const(Value::none());
                    self.emit(op::LOAD_CONST, none_idx, *line);
                }
                self.emit(op::RETURN_VALUE, 0, *line);
            }
            Stmt::Pass { .. } => {}
            Stmt::Break { line } => {
                let offset = self.current_offset();
                self.emit(op::JUMP, 0, *line);
                if let Some(ctx) = self.loop_stack.last_mut() {
                    ctx.break_patches.push(offset);
                }
            }
            Stmt::Continue { line } => {
                if let Some(ctx) = self.loop_stack.last() {
                    let target = ctx.continue_target as u32;
                    self.emit(op::JUMP, target, *line);
                }
            }
            Stmt::ClassDef { name, bases, body, decorators: _, line } => {
                self.compile_class_def(name, bases, body, *line)?;
            }
            Stmt::Try { body, handlers, else_body, finally_body, line } => {
                self.compile_try(body, handlers, else_body, finally_body, *line)?;
            }
            Stmt::Raise { exc, line } => {
                if let Some(exc_expr) = exc {
                    self.compile_expr(exc_expr)?;
                    self.emit(op::RAISE, 1, *line);
                } else {
                    self.emit(op::RAISE, 0, *line);
                }
            }
            Stmt::Assert { test, msg, line } => {
                self.compile_expr(test)?;
                let jump_ok = self.current_offset();
                self.emit(op::JUMP_IF_TRUE, 0, *line);
                // Assertion failed — raise
                if let Some(m) = msg {
                    self.compile_expr(m)?;
                } else {
                    let s = self.add_string_const("assertion failed");
                    self.emit(op::LOAD_CONST, s, *line);
                }
                self.emit(op::RAISE, 2, *line); // operand 2 = assertion error with message on stack
                self.patch_jump(jump_ok);
            }
            Stmt::Delete { .. } => {
                // Simplified: no-op for now
            }
            Stmt::GlobalDecl { .. } | Stmt::NonlocalDecl { .. } => {
                // Declarations are handled by scope analysis, no runtime code needed
            }
        }
        Ok(())
    }

    fn binop_to_opcode(&self, binop: &BinOp) -> u8 {
        match binop {
            BinOp::Add => op::ADD,
            BinOp::Sub => op::SUB,
            BinOp::Mul => op::MUL,
            BinOp::Div => op::DIV,
            BinOp::FloorDiv => op::FLOOR_DIV,
            BinOp::Mod => op::MOD,
            BinOp::Pow => op::POW,
            BinOp::BitAnd => op::BIT_AND,
            BinOp::BitOr => op::BIT_OR,
            BinOp::BitXor => op::BIT_XOR,
            BinOp::LShift => op::LSHIFT,
            BinOp::RShift => op::RSHIFT,
        }
    }

    fn compile_load_target(&mut self, target: &AssignTarget, line: u32) -> Result<(), PythonError> {
        match target {
            AssignTarget::Name(name) => {
                self.compile_load_name(name, line);
            }
            AssignTarget::Attribute { value, attr, .. } => {
                self.compile_expr(value)?;
                let name_idx = self.add_name(attr);
                self.emit(op::LOAD_ATTR, name_idx, line);
            }
            AssignTarget::Subscript { value, index, .. } => {
                self.compile_expr(value)?;
                self.compile_expr(index)?;
                self.emit(op::SUBSCRIPT, 0, line);
            }
            AssignTarget::Tuple(_) => {
                return Err(PythonError::compile("cannot augmented-assign to tuple", line));
            }
        }
        Ok(())
    }

    fn compile_store_target(&mut self, target: &AssignTarget, line: u32) -> Result<(), PythonError> {
        match target {
            AssignTarget::Name(name) => {
                self.compile_store_name(name, line);
            }
            AssignTarget::Attribute { value, attr, .. } => {
                self.compile_expr(value)?;
                let name_idx = self.add_name(attr);
                self.emit(op::STORE_ATTR, name_idx, line);
            }
            AssignTarget::Subscript { value, index, .. } => {
                self.compile_expr(value)?;
                self.compile_expr(index)?;
                self.emit(op::STORE_SUBSCRIPT, 0, line);
            }
            AssignTarget::Tuple(targets) => {
                self.emit(op::UNPACK_SEQUENCE, targets.len() as u32, line);
                for t in targets {
                    self.compile_store_target(t, line)?;
                }
            }
        }
        Ok(())
    }

    fn compile_load_name(&mut self, name: &str, line: u32) {
        if !self.is_module_level() {
            // Check deref (cell/free vars)
            if let Some(deref_idx) = self.find_deref(name) {
                self.emit(op::LOAD_DEREF, deref_idx, line);
                return;
            }
            if self.is_global_decl(name) {
                let name_idx = self.add_name(name);
                self.emit(op::LOAD_GLOBAL, name_idx, line);
                return;
            }
            if let Some(local_idx) = self.find_local(name) {
                self.emit(op::LOAD_FAST, local_idx, line);
                return;
            }
        }
        let name_idx = self.add_name(name);
        self.emit(op::LOAD_GLOBAL, name_idx, line);
    }

    fn compile_store_name(&mut self, name: &str, line: u32) {
        if !self.is_module_level() {
            // Check deref (cell/free vars)
            if let Some(deref_idx) = self.find_deref(name) {
                self.emit(op::STORE_DEREF, deref_idx, line);
                return;
            }
            if self.is_global_decl(name) {
                let name_idx = self.add_name(name);
                self.emit(op::STORE_GLOBAL, name_idx, line);
                return;
            }
            let local_idx = self.add_local(name);
            self.emit(op::STORE_FAST, local_idx, line);
            return;
        }
        let name_idx = self.add_name(name);
        self.emit(op::STORE_GLOBAL, name_idx, line);
    }

    fn compile_if(
        &mut self,
        condition: &Expr,
        body: &[Stmt],
        elif_clauses: &[(Expr, Vec<Stmt>)],
        else_body: &[Stmt],
        line: u32,
    ) -> Result<(), PythonError> {
        self.compile_expr(condition)?;
        let jump_false = self.current_offset();
        self.emit(op::JUMP_IF_FALSE, 0, line);

        for stmt in body {
            self.compile_stmt(stmt)?;
        }

        let mut end_jumps = Vec::new();
        let has_more = !elif_clauses.is_empty() || !else_body.is_empty();
        if has_more {
            let end_jump = self.current_offset();
            self.emit(op::JUMP, 0, line);
            end_jumps.push(end_jump);
        }
        self.patch_jump(jump_false);

        for (elif_cond, elif_body) in elif_clauses {
            self.compile_expr(elif_cond)?;
            let elif_jump_false = self.current_offset();
            self.emit(op::JUMP_IF_FALSE, 0, line);

            for stmt in elif_body {
                self.compile_stmt(stmt)?;
            }

            let end_jump = self.current_offset();
            self.emit(op::JUMP, 0, line);
            end_jumps.push(end_jump);
            self.patch_jump(elif_jump_false);
        }

        for stmt in else_body {
            self.compile_stmt(stmt)?;
        }

        for ej in end_jumps {
            self.patch_jump(ej);
        }

        Ok(())
    }

    fn compile_while(&mut self, condition: &Expr, body: &[Stmt], line: u32) -> Result<(), PythonError> {
        let loop_start = self.current_offset();

        self.loop_stack.push(LoopContext {
            break_patches: Vec::new(),
            continue_target: loop_start,
        });

        self.compile_expr(condition)?;
        let exit_jump = self.current_offset();
        self.emit(op::JUMP_IF_FALSE, 0, line);

        for stmt in body {
            self.compile_stmt(stmt)?;
        }

        self.emit(op::JUMP, loop_start as u32, line);
        self.patch_jump(exit_jump);

        let ctx = self.loop_stack.pop().unwrap();
        for bp in ctx.break_patches {
            self.patch_jump(bp);
        }

        Ok(())
    }

    fn compile_for(&mut self, target: &AssignTarget, iter: &Expr, body: &[Stmt], line: u32) -> Result<(), PythonError> {
        self.compile_expr(iter)?;
        self.emit(op::GET_ITER, 0, line);

        // Store iterator in a temporary
        let iter_name = format!("__iter_{}__", target_name(target));
        if self.is_module_level() {
            let name_idx = self.add_name(&iter_name);
            self.emit(op::STORE_GLOBAL, name_idx, line);
        } else {
            let local_idx = self.add_local(&iter_name);
            self.emit(op::STORE_FAST, local_idx, line);
        }

        let loop_start = self.current_offset();

        self.loop_stack.push(LoopContext {
            break_patches: Vec::new(),
            continue_target: loop_start,
        });

        // Load iterator and call FOR_ITER
        if self.is_module_level() {
            let name_idx = self.add_name(&iter_name);
            self.emit(op::LOAD_GLOBAL, name_idx, line);
        } else {
            let local_idx = self.find_local(&iter_name).unwrap();
            self.emit(op::LOAD_FAST, local_idx, line);
        }

        let for_iter = self.current_offset();
        self.emit(op::FOR_ITER, 0, line);

        // Store current value in target variable(s)
        self.compile_store_target(target, line)?;

        for stmt in body {
            self.compile_stmt(stmt)?;
        }

        self.emit(op::JUMP, loop_start as u32, line);
        self.patch_jump(for_iter);

        let ctx = self.loop_stack.pop().unwrap();
        for bp in ctx.break_patches {
            self.patch_jump(bp);
        }

        Ok(())
    }

    fn compile_function_def(
        &mut self,
        name: &str,
        params: &[String],
        body: &[Stmt],
        _decorators: &[Expr],
        line: u32,
    ) -> Result<(), PythonError> {
        let func_co_idx = self.code_objects.len();
        let mut func_co = CodeObject::new(name);
        func_co.num_params = params.len();

        for p in params {
            func_co.local_names.push(p.clone());
        }
        func_co.num_locals = params.len();

        // Check if this is a generator (contains yield)
        if contains_yield(body) {
            func_co.is_generator = true;
        }

        // Set up cell/free var names from scope info
        if func_co_idx < self.scope_info.len() {
            let scope = &self.scope_info[func_co_idx];
            func_co.cell_var_names = scope.cell_vars.iter().cloned().collect();
            func_co.free_var_names = scope.free_vars.iter().cloned().collect();
        }

        self.prescan_locals(&mut func_co, body);
        self.code_objects.push(func_co);
        self.code_stack.push(func_co_idx);

        for stmt in body {
            self.compile_stmt(stmt)?;
        }

        // Implicit return None
        let none_idx = self.add_const(Value::none());
        self.emit(op::LOAD_CONST, none_idx, line);
        self.emit(op::RETURN_VALUE, 0, line);

        self.code_stack.pop();

        // Check if this function has free vars (needs closure)
        let has_free_vars = func_co_idx < self.scope_info.len()
            && !self.scope_info[func_co_idx].free_vars.is_empty();

        if has_free_vars {
            // Emit LOAD_CLOSURE for each free var, then MAKE_CLOSURE
            let free_vars: Vec<String> = self.code_objects[func_co_idx].free_var_names.clone();
            let num_free = free_vars.len();
            for fv in &free_vars {
                // Find the cell in the enclosing scope
                if let Some(deref_idx) = self.find_deref(fv) {
                    self.emit(op::LOAD_CLOSURE, deref_idx, line);
                } else {
                    // Fall back to loading as a local
                    let name_idx = self.add_name(fv);
                    self.emit(op::LOAD_GLOBAL, name_idx, line);
                }
            }
            let func_idx_const = self.add_const(Value::small_int_unchecked(func_co_idx as i64));
            self.emit(op::MAKE_CLOSURE, func_idx_const, line);
            // Operand tells how many free vars were pushed
            // Actually we encode num_free in a second way: use BUILD_TUPLE first
            // Simpler: MAKE_CLOSURE pops num_free_vars cells then creates closure
            // Let's encode the count via the code object's free_var_names.len()
            // The VM will read it from there.
            // But we need to emit the count... let's put it as a separate const
            let _count = num_free; // VM reads from code object
        } else {
            let func_idx_const = self.add_const(Value::small_int_unchecked(func_co_idx as i64));
            self.emit(op::MAKE_FUNCTION, func_idx_const, line);
        }

        // Store the function
        if self.is_module_level() {
            let name_idx = self.add_name(name);
            self.emit(op::STORE_GLOBAL, name_idx, line);
        } else {
            let local_idx = self.add_local(name);
            self.emit(op::STORE_FAST, local_idx, line);
        }

        Ok(())
    }

    fn compile_class_def(
        &mut self,
        name: &str,
        bases: &[Expr],
        body: &[Stmt],
        line: u32,
    ) -> Result<(), PythonError> {
        // Compile class body as a separate code object
        let class_co_idx = self.code_objects.len();
        let mut class_co = CodeObject::new(name);
        class_co.num_params = 0;
        self.code_objects.push(class_co);
        // Add scope info if needed
        while self.scope_info.len() <= class_co_idx {
            self.scope_info.push(ScopeInfo::default());
        }

        self.code_stack.push(class_co_idx);

        for stmt in body {
            self.compile_stmt(stmt)?;
        }

        // Return None (class body doesn't return a value, locals become attrs)
        let none_idx = self.add_const(Value::none());
        self.emit(op::LOAD_CONST, none_idx, line);
        self.emit(op::RETURN_VALUE, 0, line);

        self.code_stack.pop();

        // Push bases
        for base in bases {
            self.compile_expr(base)?;
        }

        // Push class name as string
        let name_const = self.add_string_const(name);
        self.emit(op::LOAD_CONST, name_const, line);

        // Push code index
        let co_idx_const = self.add_const(Value::small_int_unchecked(class_co_idx as i64));
        self.emit(op::LOAD_CONST, co_idx_const, line);

        // BUILD_CLASS: operand = number of bases
        self.emit(op::BUILD_CLASS, bases.len() as u32, line);

        // Store the class
        if self.is_module_level() {
            let name_idx = self.add_name(name);
            self.emit(op::STORE_GLOBAL, name_idx, line);
        } else {
            let local_idx = self.add_local(name);
            self.emit(op::STORE_FAST, local_idx, line);
        }

        Ok(())
    }

    fn compile_try(
        &mut self,
        body: &[Stmt],
        handlers: &[ExceptHandler],
        else_body: &[Stmt],
        finally_body: &[Stmt],
        line: u32,
    ) -> Result<(), PythonError> {
        let has_finally = !finally_body.is_empty();

        // Set up finally handler if present
        let finally_setup = if has_finally {
            let idx = self.current_offset();
            self.emit(op::SETUP_FINALLY, 0, line);
            Some(idx)
        } else {
            None
        };

        // Set up except handler
        let except_setup = self.current_offset();
        self.emit(op::SETUP_EXCEPT, 0, line);

        // Compile try body
        for stmt in body {
            self.compile_stmt(stmt)?;
        }

        // Pop except handler (no exception occurred)
        self.emit(op::POP_EXCEPT, 0, line);

        // Compile else body
        for stmt in else_body {
            self.compile_stmt(stmt)?;
        }

        // Jump past handlers
        let jump_past_handlers = self.current_offset();
        self.emit(op::JUMP, 0, line);

        // Patch SETUP_EXCEPT to point here
        self.patch_jump(except_setup);

        // Compile handlers
        let mut handler_end_jumps = Vec::new();
        for handler in handlers {
            if let Some(exc_type) = &handler.exc_type {
                // Load exception, check type
                self.emit(op::LOAD_EXCEPTION, 0, handler.line);
                self.compile_expr(exc_type)?;
                self.emit(op::COMPARE_EQ, 0, handler.line); // simplified: will be exception type matching in VM
                let skip_handler = self.current_offset();
                self.emit(op::JUMP_IF_FALSE, 0, handler.line);

                // Bind exception to name if present
                if let Some(name) = &handler.name {
                    self.emit(op::LOAD_EXCEPTION, 0, handler.line);
                    self.compile_store_name(name, handler.line);
                }

                for stmt in &handler.body {
                    self.compile_stmt(stmt)?;
                }

                let end_jump = self.current_offset();
                self.emit(op::JUMP, 0, handler.line);
                handler_end_jumps.push(end_jump);
                self.patch_jump(skip_handler);
            } else {
                // Bare except
                if let Some(name) = &handler.name {
                    self.emit(op::LOAD_EXCEPTION, 0, handler.line);
                    self.compile_store_name(name, handler.line);
                }

                for stmt in &handler.body {
                    self.compile_stmt(stmt)?;
                }

                let end_jump = self.current_offset();
                self.emit(op::JUMP, 0, handler.line);
                handler_end_jumps.push(end_jump);
            }
        }

        // If no handler matched, re-raise
        self.emit(op::RAISE, 0, line);

        // Patch all handler end jumps
        for ej in handler_end_jumps {
            self.patch_jump(ej);
        }
        self.patch_jump(jump_past_handlers);

        // Compile finally body
        if has_finally {
            if let Some(fi) = finally_setup {
                self.patch_jump(fi);
            }
            for stmt in finally_body {
                self.compile_stmt(stmt)?;
            }
            self.emit(op::END_FINALLY, 0, line);
        }

        Ok(())
    }

    fn prescan_locals(&self, co: &mut CodeObject, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::Assign { target, .. } | Stmt::AugAssign { target, .. } => {
                    add_target_locals(co, target);
                }
                Stmt::For { target, body, .. } => {
                    add_target_locals(co, target);
                    let iter_name = format!("__iter_{}__", target_name(target));
                    if !co.local_names.contains(&iter_name) {
                        co.local_names.push(iter_name);
                        co.num_locals = co.local_names.len();
                    }
                    self.prescan_locals(co, body);
                }
                Stmt::If { body, elif_clauses, else_body, .. } => {
                    self.prescan_locals(co, body);
                    for (_, elif_body) in elif_clauses {
                        self.prescan_locals(co, elif_body);
                    }
                    self.prescan_locals(co, else_body);
                }
                Stmt::While { body, .. } => {
                    self.prescan_locals(co, body);
                }
                Stmt::Try { body, handlers, else_body, finally_body, .. } => {
                    self.prescan_locals(co, body);
                    for h in handlers {
                        if let Some(name) = &h.name
                            && !co.local_names.contains(name)
                        {
                            co.local_names.push(name.clone());
                            co.num_locals = co.local_names.len();
                        }
                        self.prescan_locals(co, &h.body);
                    }
                    self.prescan_locals(co, else_body);
                    self.prescan_locals(co, finally_body);
                }
                Stmt::FunctionDef { name, .. } => {
                    if !co.local_names.contains(name) {
                        co.local_names.push(name.clone());
                        co.num_locals = co.local_names.len();
                    }
                }
                Stmt::ClassDef { name, .. } => {
                    if !co.local_names.contains(name) {
                        co.local_names.push(name.clone());
                        co.num_locals = co.local_names.len();
                    }
                }
                _ => {}
            }
        }
    }

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), PythonError> {
        match expr {
            Expr::IntLit { value, line } => {
                // Source-level i64 literal — promote to BigInt if outside i48.
                let v = Value::from_i64(*value, &mut self.heap);
                let idx = self.add_const(v);
                self.emit(op::LOAD_CONST, idx, *line);
            }
            Expr::BigIntLit { value, line } => {
                // Materialize the BigInt into the heap once at compile time.
                // Value::from_bigint demotes to a small int if (after lex/parse
                // simplification) the value somehow fits in i48 — defensive.
                let v = Value::from_bigint((**value).clone(), &mut self.heap);
                let idx = self.add_const(v);
                self.emit(op::LOAD_CONST, idx, *line);
            }
            Expr::FloatLit { value, line } => {
                let idx = self.add_const(Value::float(*value));
                self.emit(op::LOAD_CONST, idx, *line);
            }
            Expr::StringLit { value, line } => {
                let idx = self.add_string_const(value);
                self.emit(op::LOAD_CONST, idx, *line);
            }
            Expr::BoolLit { value, line } => {
                let idx = self.add_const(Value::bool_val(*value));
                self.emit(op::LOAD_CONST, idx, *line);
            }
            Expr::NoneLit { line } => {
                let idx = self.add_const(Value::none());
                self.emit(op::LOAD_CONST, idx, *line);
            }
            Expr::Name { id, line } => {
                self.compile_load_name(id, *line);
            }
            Expr::BinOp { left, op: binop, right, line } => {
                self.compile_expr(left)?;
                self.compile_expr(right)?;
                let opcode = self.binop_to_opcode(binop);
                self.emit(opcode, 0, *line);
            }
            Expr::UnaryOp { op: unop, operand, line } => {
                self.compile_expr(operand)?;
                let opcode = match unop {
                    UnaryOp::Neg => op::UNARY_NEG,
                    UnaryOp::Not => op::UNARY_NOT,
                    UnaryOp::Pos => op::UNARY_POS,
                    UnaryOp::Invert => op::UNARY_INVERT,
                };
                self.emit(opcode, 0, *line);
            }
            Expr::Compare { left, ops, comparators, line } => {
                self.compile_comparison(left, ops, comparators, *line)?;
            }
            Expr::BoolOp { op: boolop, left, right, line } => {
                self.compile_expr(left)?;
                match boolop {
                    BoolOpKind::And => {
                        self.emit(op::DUP_TOP, 0, *line);
                        let jump = self.current_offset();
                        self.emit(op::JUMP_IF_FALSE, 0, *line);
                        self.emit(op::POP_TOP, 0, *line);
                        self.compile_expr(right)?;
                        self.patch_jump(jump);
                    }
                    BoolOpKind::Or => {
                        self.emit(op::DUP_TOP, 0, *line);
                        let jump = self.current_offset();
                        self.emit(op::JUMP_IF_TRUE, 0, *line);
                        self.emit(op::POP_TOP, 0, *line);
                        self.compile_expr(right)?;
                        self.patch_jump(jump);
                    }
                }
            }
            Expr::Call { func, args, line } => {
                self.compile_expr(func)?;
                let argc = args.len();
                for arg in args {
                    self.compile_expr(arg)?;
                }
                self.emit(op::CALL_FUNCTION, argc as u32, *line);
            }
            Expr::Subscript { value, index, line } => {
                self.compile_expr(value)?;
                self.compile_expr(index)?;
                self.emit(op::SUBSCRIPT, 0, *line);
            }
            Expr::List { elements, line } => {
                for elem in elements {
                    self.compile_expr(elem)?;
                }
                self.emit(op::BUILD_LIST, elements.len() as u32, *line);
            }
            Expr::Attribute { value, attr, line } => {
                self.compile_expr(value)?;
                let name_idx = self.add_name(attr);
                self.emit(op::LOAD_ATTR, name_idx, *line);
            }
            Expr::Tuple { elements, line } => {
                for elem in elements {
                    self.compile_expr(elem)?;
                }
                self.emit(op::BUILD_TUPLE, elements.len() as u32, *line);
            }
            Expr::Dict { keys, values, line } => {
                for (k, v) in keys.iter().zip(values.iter()) {
                    self.compile_expr(k)?;
                    self.compile_expr(v)?;
                }
                self.emit(op::BUILD_DICT, keys.len() as u32, *line);
            }
            Expr::Set { elements, line } => {
                for elem in elements {
                    self.compile_expr(elem)?;
                }
                self.emit(op::BUILD_SET, elements.len() as u32, *line);
            }
            Expr::Lambda { params, body, line } => {
                // Compile as anonymous function
                let lambda_name = "<lambda>";
                let func_co_idx = self.code_objects.len();
                let mut func_co = CodeObject::new(lambda_name);
                func_co.num_params = params.len();
                for p in params {
                    func_co.local_names.push(p.clone());
                }
                func_co.num_locals = params.len();
                self.code_objects.push(func_co);
                self.code_stack.push(func_co_idx);

                self.compile_expr(body)?;
                self.emit(op::RETURN_VALUE, 0, *line);

                self.code_stack.pop();

                let func_idx_const = self.add_const(Value::small_int_unchecked(func_co_idx as i64));
                self.emit(op::MAKE_FUNCTION, func_idx_const, *line);
            }
            Expr::IfExpr { body, test, orelse, line } => {
                self.compile_expr(test)?;
                let jump_false = self.current_offset();
                self.emit(op::JUMP_IF_FALSE, 0, *line);
                self.compile_expr(body)?;
                let jump_end = self.current_offset();
                self.emit(op::JUMP, 0, *line);
                self.patch_jump(jump_false);
                self.compile_expr(orelse)?;
                self.patch_jump(jump_end);
            }
            Expr::Yield { value, line } => {
                if let Some(val) = value {
                    self.compile_expr(val)?;
                } else {
                    let none_idx = self.add_const(Value::none());
                    self.emit(op::LOAD_CONST, none_idx, *line);
                }
                self.emit(op::YIELD_VALUE, 0, *line);
            }
            Expr::Starred { value, line: _ } => {
                // For now just compile the inner expression
                self.compile_expr(value)?;
            }
        }
        Ok(())
    }

    fn compile_comparison(
        &mut self,
        left: &Expr,
        ops: &[CmpOp],
        comparators: &[Expr],
        line: u32,
    ) -> Result<(), PythonError> {
        if ops.len() == 1 {
            self.compile_expr(left)?;
            self.compile_expr(&comparators[0])?;
            self.emit_cmp_op(&ops[0], line);
        } else {
            self.compile_expr(left)?;
            let mut end_jumps = Vec::new();

            for (i, (cmp_op, comparator)) in ops.iter().zip(comparators.iter()).enumerate() {
                if i < ops.len() - 1 {
                    self.emit(op::DUP_TOP, 0, line);
                }
                self.compile_expr(comparator)?;
                if i < ops.len() - 1 {
                    // placeholder for intermediate
                }
                self.emit_cmp_op(cmp_op, line);

                if i < ops.len() - 1 {
                    let jump = self.current_offset();
                    self.emit(op::JUMP_IF_FALSE, 0, line);
                    end_jumps.push(jump);
                }
            }

            for ej in end_jumps {
                self.patch_jump(ej);
            }
        }
        Ok(())
    }

    fn emit_cmp_op(&mut self, cmp_op: &CmpOp, line: u32) {
        match cmp_op {
            CmpOp::Eq => self.emit(op::COMPARE_EQ, 0, line),
            CmpOp::NotEq => self.emit(op::COMPARE_NE, 0, line),
            CmpOp::Lt => self.emit(op::COMPARE_LT, 0, line),
            CmpOp::LtEq => self.emit(op::COMPARE_LE, 0, line),
            CmpOp::Gt => self.emit(op::COMPARE_GT, 0, line),
            CmpOp::GtEq => self.emit(op::COMPARE_GE, 0, line),
            CmpOp::Is => self.emit(op::COMPARE_IS, 0, line),
            CmpOp::IsNot => self.emit(op::COMPARE_IS_NOT, 0, line),
            CmpOp::In => self.emit(op::CONTAINS_OP, 0, line),
            CmpOp::NotIn => self.emit(op::CONTAINS_OP, 1, line),
        }
    }
}

/// Extract a name from an assignment target (for iterator naming).
fn target_name(target: &AssignTarget) -> String {
    match target {
        AssignTarget::Name(n) => n.clone(),
        AssignTarget::Tuple(targets) => {
            if let Some(first) = targets.first() {
                target_name(first)
            } else {
                "tuple".to_string()
            }
        }
        _ => "target".to_string(),
    }
}

fn add_target_locals(co: &mut CodeObject, target: &AssignTarget) {
    match target {
        AssignTarget::Name(name) => {
            if !co.local_names.contains(name) {
                co.local_names.push(name.clone());
                co.num_locals = co.local_names.len();
            }
        }
        AssignTarget::Tuple(targets) => {
            for t in targets {
                add_target_locals(co, t);
            }
        }
        _ => {}
    }
}

fn collect_declarations(stmts: &[Stmt], scope: &mut ScopeInfo) {
    for stmt in stmts {
        match stmt {
            Stmt::GlobalDecl { names, .. } => {
                for n in names {
                    scope.globals.insert(n.clone());
                }
            }
            Stmt::NonlocalDecl { names, .. } => {
                for n in names {
                    scope.nonlocals.insert(n.clone());
                }
            }
            _ => {}
        }
    }
}

fn collect_locals(stmts: &[Stmt], scope: &mut ScopeInfo) {
    for stmt in stmts {
        match stmt {
            Stmt::Assign { target, .. } | Stmt::AugAssign { target, .. } => {
                collect_target_names(target, scope);
            }
            Stmt::For { target, body, .. } => {
                collect_target_names(target, scope);
                collect_locals(body, scope);
            }
            Stmt::If { body, elif_clauses, else_body, .. } => {
                collect_locals(body, scope);
                for (_, b) in elif_clauses {
                    collect_locals(b, scope);
                }
                collect_locals(else_body, scope);
            }
            Stmt::While { body, .. } => {
                collect_locals(body, scope);
            }
            Stmt::FunctionDef { name, .. } => {
                scope.locals.insert(name.clone());
            }
            Stmt::ClassDef { name, .. } => {
                scope.locals.insert(name.clone());
            }
            _ => {}
        }
    }
}

fn collect_target_names(target: &AssignTarget, scope: &mut ScopeInfo) {
    match target {
        AssignTarget::Name(name) => {
            if !scope.globals.contains(name) && !scope.nonlocals.contains(name) {
                scope.locals.insert(name.clone());
            }
        }
        AssignTarget::Tuple(targets) => {
            for t in targets {
                collect_target_names(t, scope);
            }
        }
        _ => {}
    }
}

/// Check if a function body contains yield expressions.
fn contains_yield(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        match stmt {
            Stmt::ExprStmt { expr, .. } | Stmt::Assign { value: expr, .. } => {
                if expr_contains_yield(expr) {
                    return true;
                }
            }
            Stmt::Return { value: Some(expr), .. } => {
                if expr_contains_yield(expr) {
                    return true;
                }
            }
            Stmt::If { body, elif_clauses, else_body, .. } => {
                if contains_yield(body) || contains_yield(else_body) {
                    return true;
                }
                for (_, b) in elif_clauses {
                    if contains_yield(b) {
                        return true;
                    }
                }
            }
            Stmt::While { body, .. } | Stmt::For { body, .. } => {
                if contains_yield(body) {
                    return true;
                }
            }
            Stmt::Try { body, handlers, else_body, finally_body, .. } => {
                if contains_yield(body) || contains_yield(else_body) || contains_yield(finally_body) {
                    return true;
                }
                for h in handlers {
                    if contains_yield(&h.body) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn expr_contains_yield(expr: &Expr) -> bool {
    match expr {
        Expr::Yield { .. } => true,
        Expr::Call { func, args, .. } => {
            expr_contains_yield(func) || args.iter().any(expr_contains_yield)
        }
        Expr::BinOp { left, right, .. } => {
            expr_contains_yield(left) || expr_contains_yield(right)
        }
        Expr::UnaryOp { operand, .. } => expr_contains_yield(operand),
        Expr::BoolOp { left, right, .. } => {
            expr_contains_yield(left) || expr_contains_yield(right)
        }
        Expr::Compare { left, comparators, .. } => {
            expr_contains_yield(left) || comparators.iter().any(expr_contains_yield)
        }
        Expr::IfExpr { body, test, orelse, .. } => {
            expr_contains_yield(body) || expr_contains_yield(test) || expr_contains_yield(orelse)
        }
        Expr::Attribute { value, .. } | Expr::Subscript { value, .. } | Expr::Starred { value, .. } => {
            expr_contains_yield(value)
        }
        Expr::Tuple { elements, .. } | Expr::List { elements, .. } | Expr::Set { elements, .. } => {
            elements.iter().any(expr_contains_yield)
        }
        Expr::Dict { keys, values, .. } => {
            keys.iter().any(expr_contains_yield) || values.iter().any(expr_contains_yield)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;
    use crate::parser::parse;

    fn compile_src(src: &str) -> (Vec<CodeObject>, Vec<HeapObject>) {
        let tokens = tokenize(src).unwrap();
        let module = parse(tokens).unwrap();
        compile(&module).unwrap()
    }

    #[test]
    fn compile_simple_assignment() {
        let (cos, _) = compile_src("x = 42\n");
        assert_eq!(cos.len(), 1);
        assert!(cos[0].instructions.len() >= 2);
    }

    #[test]
    fn compile_function() {
        let (cos, _) = compile_src("def foo(x):\n    return x\n");
        assert_eq!(cos.len(), 2);
        assert_eq!(cos[1].name, "foo");
        assert_eq!(cos[1].num_params, 1);
    }

    #[test]
    fn compile_for_loop() {
        let (cos, _) = compile_src("for i in range(10):\n    print(i)\n");
        assert_eq!(cos.len(), 1);
        let ops: Vec<u8> = cos[0].instructions.iter().map(|i| bytecode::decode_op(*i)).collect();
        assert!(ops.contains(&op::GET_ITER));
        assert!(ops.contains(&op::FOR_ITER));
    }

    #[test]
    fn compile_class() {
        let (cos, _) = compile_src("class Foo:\n    pass\n");
        assert!(cos.len() >= 2);
    }

    #[test]
    fn compile_try_except() {
        let (cos, _) = compile_src("try:\n    pass\nexcept:\n    pass\n");
        let ops: Vec<u8> = cos[0].instructions.iter().map(|i| bytecode::decode_op(*i)).collect();
        assert!(ops.contains(&op::SETUP_EXCEPT));
    }

    #[test]
    fn compile_generator() {
        let (cos, _) = compile_src("def gen():\n    yield 1\n");
        assert!(cos[1].is_generator);
    }
}
