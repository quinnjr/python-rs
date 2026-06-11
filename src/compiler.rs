/// Compiles AST into bytecode.
use crate::ast::*;
use crate::bytecode::{self, CodeObject, encode, op};
use crate::error::PythonError;
use crate::object::{HeapObject, Value};
use std::collections::{HashMap, HashSet};

/// Compile a module AST into code objects and initial heap objects.
pub fn compile(module: &Module) -> Result<(Vec<CodeObject>, Vec<HeapObject>), PythonError> {
    let mut compiler = Compiler::new();
    compiler.compile_module(module)?;
    Ok((compiler.code_objects, compiler.heap))
}

/// Compile a module AST and APPEND its code objects + heap entries to
/// existing vectors. Used by the import system at runtime to compile
/// `.py` files into an already-running VM's storage. Returns the
/// `code_index` of the new module's top-level code object so the VM
/// knows where to start executing the module body.
pub fn compile_extending(
    module: &Module,
    code_objects: &mut Vec<CodeObject>,
    heap: &mut Vec<HeapObject>,
) -> Result<usize, PythonError> {
    let existing_n = code_objects.len();
    let mut compiler = Compiler {
        code_objects: std::mem::take(code_objects),
        heap: std::mem::take(heap),
        code_stack: Vec::new(),
        loop_stack: Vec::new(),
        scope_info: Vec::new(),
        const_index: (0..existing_n).map(|_| HashMap::new()).collect(),
    };
    compiler.compile_module(module)?;
    *code_objects = compiler.code_objects;
    *heap = compiler.heap;
    Ok(existing_n)
}

struct Compiler {
    code_objects: Vec<CodeObject>,
    heap: Vec<HeapObject>,
    code_stack: Vec<usize>,
    loop_stack: Vec<LoopContext>,
    /// Scope info: (globals, nonlocals, cell_vars, free_vars) per code object index
    scope_info: Vec<ScopeInfo>,
    /// Per-code-object constant dedup index, keyed on Value's bit pattern.
    /// Parallels `code_objects`; the i-th map covers the i-th code object's
    /// constants pool. Turns add_const from O(N) linear scan into O(1) lookup.
    const_index: Vec<HashMap<u64, u32>>,
}

struct LoopContext {
    break_patches: Vec<usize>,
    continue_target: usize,
}

/// Backpatch handles returned by `emit_iter_loop_open` and consumed by
/// `emit_iter_loop_close`. Carries the loop-back target and the FOR_ITER
/// exit-jump offset that needs patching to land after the loop body.
struct IterLoopFrame {
    loop_start: usize,
    for_iter_off: usize,
}

/// What kind of comprehension is being compiled, with the per-iteration
/// element expressions bundled. `Copy` because every variant is just
/// shared references into the AST.
#[derive(Clone, Copy)]
enum CompKind<'a> {
    List(&'a Expr),
    Set(&'a Expr),
    Dict(&'a Expr, &'a Expr),
}

#[derive(Clone, Default)]
struct ScopeInfo {
    globals: HashSet<String>,
    nonlocals: HashSet<String>,
    cell_vars: HashSet<String>,
    free_vars: HashSet<String>,
    locals: HashSet<String>,
    /// Parent scope's index in `scope_info`. None for the module-level
    /// scope (root). Used to walk up the enclosing-scope chain when
    /// resolving implicit free-variable captures.
    parent: Option<usize>,
}

impl Compiler {
    fn new() -> Self {
        Self {
            code_objects: Vec::new(),
            heap: Vec::new(),
            code_stack: Vec::new(),
            loop_stack: Vec::new(),
            scope_info: Vec::new(),
            const_index: Vec::new(),
        }
    }

    fn current_code(&mut self) -> &mut CodeObject {
        // code_stack is initialized with [0] (module scope) and only
        // shrinks back to that after function bodies finish; last() can't
        // be None during compilation. unwrap_or(&0) falls back to module
        // scope to keep the path panic-free if the invariant is ever broken.
        let idx = *self.code_stack.last().unwrap_or(&0);
        &mut self.code_objects[idx]
    }

    fn current_code_index(&self) -> usize {
        *self.code_stack.last().unwrap_or(&0)
    }

    fn emit(&mut self, opcode: u8, operand: u32, line: u32) {
        let code = self.current_code();
        code.instructions.push(encode(opcode, operand));
        code.line_table.push(line);
    }

    fn current_offset(&self) -> usize {
        let idx = *self.code_stack.last().unwrap_or(&0);
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
        let co_idx = self.current_code_index();
        let key = val.display_bits();
        if let Some(&existing) = self.const_index[co_idx].get(&key) {
            return existing;
        }
        let code = &mut self.code_objects[co_idx];
        let idx = code.constants.len() as u32;
        code.constants.push(val);
        self.const_index[co_idx].insert(key, idx);
        idx
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
        let idx = *self.code_stack.last().unwrap_or(&0);
        let code = &self.code_objects[idx];
        code.local_names
            .iter()
            .position(|n| n == name)
            .map(|i| i as u32)
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
            !self.scope_info[co_idx].cell_vars.is_empty()
                || !self.scope_info[co_idx].free_vars.is_empty()
        } else {
            false
        }
    }

    fn compile_module(&mut self, module: &Module) -> Result<(), PythonError> {
        // First pass: analyze scopes for closures
        self.analyze_all_scopes(module);

        let co_idx = self.code_objects.len();
        self.code_objects.push(CodeObject::new("<module>"));
        self.const_index.push(HashMap::new());
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
        // For module level — root scope with no parent.
        self.scope_info.push(ScopeInfo::default());
        self.analyze_scope_stmts(&module.body, 0);
    }

    fn analyze_scope_stmts(&mut self, stmts: &[Stmt], parent_scope: usize) {
        for stmt in stmts {
            match stmt {
                Stmt::FunctionDef { body, params, .. } => {
                    let co_idx = self.scope_info.len();
                    let mut scope = ScopeInfo {
                        parent: Some(parent_scope),
                        ..ScopeInfo::default()
                    };
                    collect_declarations(body, &mut scope);
                    collect_locals(body, &mut scope);
                    for p in params {
                        scope.locals.insert(p.clone());
                    }
                    self.scope_info.push(scope);
                    self.analyze_scope_stmts(body, co_idx);
                    self.compute_free_vars(co_idx, body);
                }
                Stmt::ClassDef { body, .. } => {
                    let co_idx = self.scope_info.len();
                    let mut scope = ScopeInfo {
                        parent: Some(parent_scope),
                        ..ScopeInfo::default()
                    };
                    collect_declarations(body, &mut scope);
                    collect_locals(body, &mut scope);
                    self.scope_info.push(scope);
                    self.analyze_scope_stmts(body, co_idx);
                    self.compute_free_vars(co_idx, body);
                }
                // FunctionDef can be nested inside any control-flow body;
                // recurse so its scope analysis still runs with the
                // enclosing function's scope as parent.
                Stmt::If {
                    body,
                    elif_clauses,
                    else_body,
                    ..
                } => {
                    self.analyze_scope_stmts(body, parent_scope);
                    for (_, b) in elif_clauses {
                        self.analyze_scope_stmts(b, parent_scope);
                    }
                    self.analyze_scope_stmts(else_body, parent_scope);
                }
                Stmt::While { body, .. } | Stmt::For { body, .. } => {
                    self.analyze_scope_stmts(body, parent_scope);
                }
                Stmt::Try {
                    body,
                    handlers,
                    else_body,
                    finally_body,
                    ..
                } => {
                    self.analyze_scope_stmts(body, parent_scope);
                    for h in handlers {
                        self.analyze_scope_stmts(&h.body, parent_scope);
                    }
                    self.analyze_scope_stmts(else_body, parent_scope);
                    self.analyze_scope_stmts(finally_body, parent_scope);
                }
                _ => {}
            }
        }
    }

    fn compute_free_vars(&mut self, scope_idx: usize, body: &[Stmt]) {
        // 1. Explicit `nonlocal` declarations always create cell/free pairs.
        let nonlocals: Vec<String> = self.scope_info[scope_idx]
            .nonlocals
            .iter()
            .cloned()
            .collect();
        for name in &nonlocals {
            self.scope_info[scope_idx].free_vars.insert(name.clone());
            // Find the nearest enclosing scope that owns this name and
            // mark it as a cell variable there.
            let mut cur = self.scope_info[scope_idx].parent;
            while let Some(parent_idx) = cur {
                if self.scope_info[parent_idx].locals.contains(name) {
                    self.scope_info[parent_idx].cell_vars.insert(name.clone());
                    break;
                }
                cur = self.scope_info[parent_idx].parent;
            }
        }

        // 2. Implicit closure capture — walk the body for Name references
        //    that are NOT locals/globals/nonlocals/params in THIS scope but ARE
        //    locals in some enclosing scope (other than the module root, where
        //    they would resolve via LOAD_GLOBAL instead).
        let mut referenced = HashSet::new();
        collect_name_refs(body, &mut referenced);
        let scope = &self.scope_info[scope_idx];
        let local_or_declared: HashSet<String> = scope
            .locals
            .iter()
            .chain(scope.globals.iter())
            .chain(scope.nonlocals.iter())
            .cloned()
            .collect();
        let candidates: Vec<String> = referenced
            .iter()
            .filter(|n| !local_or_declared.contains(*n))
            .cloned()
            .collect();
        for name in candidates {
            // Any function scope strictly between this one and the owner
            // must ALSO carry the name as a free var so MAKE_CLOSURE can
            // thread it through. `chain` collects those pass-throughs.
            let mut cur = self.scope_info[scope_idx].parent;
            let mut chain: Vec<usize> = Vec::new();
            while let Some(parent_idx) = cur {
                if parent_idx == 0 {
                    break;
                }
                if self.scope_info[parent_idx].locals.contains(&name) {
                    self.scope_info[parent_idx].cell_vars.insert(name.clone());
                    self.scope_info[scope_idx].free_vars.insert(name.clone());
                    for inter in &chain {
                        self.scope_info[*inter].free_vars.insert(name.clone());
                    }
                    break;
                }
                chain.push(parent_idx);
                cur = self.scope_info[parent_idx].parent;
            }
        }
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), PythonError> {
        match stmt {
            Stmt::Assign {
                target,
                value,
                line,
            } => {
                self.compile_expr(value)?;
                self.compile_store_target(target, *line)?;
            }
            Stmt::AugAssign {
                target,
                op,
                value,
                line,
            } => {
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
            Stmt::If {
                condition,
                body,
                elif_clauses,
                else_body,
                line,
            } => {
                self.compile_if(condition, body, elif_clauses, else_body, *line)?;
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
                self.compile_while(condition, body, *line)?;
            }
            Stmt::For {
                target,
                iter,
                body,
                line,
            } => {
                self.compile_for(target, iter, body, *line)?;
            }
            Stmt::FunctionDef {
                name,
                params,
                body,
                decorators,
                line,
            } => {
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
            Stmt::ClassDef {
                name,
                bases,
                body,
                decorators: _,
                line,
            } => {
                self.compile_class_def(name, bases, body, *line)?;
            }
            Stmt::Try {
                body,
                handlers,
                else_body,
                finally_body,
                line,
            } => {
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
            Stmt::Import { names, line } => self.compile_import(names, *line)?,
            Stmt::ImportFrom {
                module,
                names,
                level,
                is_star,
                line,
            } => {
                self.compile_import_from(module.as_deref(), names, *level, *is_star, *line)?;
            }
        }
        Ok(())
    }

    /// `import foo, bar.baz as bb` → per-alias IMPORT_NAME + STORE_NAME.
    fn compile_import(&mut self, names: &[ImportAlias], line: u32) -> Result<(), PythonError> {
        for alias in names {
            // Stack: [level=0, fromlist=None]
            let zero = self.add_const(Value::small_int_unchecked(0));
            self.emit(op::LOAD_CONST, zero, line);
            let none = self.add_const(Value::none());
            self.emit(op::LOAD_CONST, none, line);
            // IMPORT_NAME with fromlist=None pushes the TOP of the dotted
            // path: `import foo.bar` pushes `foo`.
            let name_idx = self.add_name(&alias.name);
            self.emit(op::IMPORT_NAME, name_idx, line);
            match &alias.asname {
                None => {
                    // `import foo.bar` — bind the top-level name (`foo`).
                    let top = alias.name.split('.').next().unwrap_or("");
                    self.store_name(top, line);
                }
                Some(asname) => {
                    // `import foo.bar.baz as fbb` — walk from the top down
                    // via LOAD_ATTR for each segment after the first, then
                    // bind the deepest as the alias.
                    let parts: Vec<&str> = alias.name.split('.').collect();
                    for part in parts.iter().skip(1) {
                        let part_idx = self.add_name(part);
                        self.emit(op::LOAD_ATTR, part_idx, line);
                    }
                    self.store_name(asname, line);
                }
            }
        }
        Ok(())
    }

    /// `from foo.bar import baz, qux as q` / `from . import x` / `from foo import *`.
    fn compile_import_from(
        &mut self,
        module: Option<&str>,
        names: &[ImportAlias],
        level: u32,
        is_star: bool,
        line: u32,
    ) -> Result<(), PythonError> {
        // Stack: [level, fromlist]
        let level_idx = self.add_const(Value::small_int_unchecked(level as i64));
        self.emit(op::LOAD_CONST, level_idx, line);

        let fromlist = if is_star {
            self.materialize_tuple_const(&["*"])
        } else {
            let strs: Vec<&str> = names.iter().map(|a| a.name.as_str()).collect();
            self.materialize_tuple_const(&strs)
        };
        let fromlist_idx = self.add_const(fromlist);
        self.emit(op::LOAD_CONST, fromlist_idx, line);

        let module_name = module.unwrap_or("");
        let module_name_idx = self.add_name(module_name);
        self.emit(op::IMPORT_NAME, module_name_idx, line);

        if is_star {
            self.emit(op::IMPORT_STAR, 0, line);
        } else {
            for alias in names {
                let attr_idx = self.add_name(&alias.name);
                self.emit(op::IMPORT_FROM, attr_idx, line);
                let bind_as = alias.asname.as_ref().unwrap_or(&alias.name);
                self.store_name(bind_as, line);
            }
            // Discard the module Value still on TOS.
            self.emit(op::POP_TOP, 0, line);
        }
        Ok(())
    }

    /// Emit STORE_FAST (in a function scope) or STORE_GLOBAL (at module
    /// level) for a name. Shared between import statements and `import_from`.
    fn store_name(&mut self, name: &str, line: u32) {
        if self.is_module_level() {
            let idx = self.add_name(name);
            self.emit(op::STORE_GLOBAL, idx, line);
        } else {
            let idx = self.add_local(name);
            self.emit(op::STORE_FAST, idx, line);
        }
    }

    /// Materialize a tuple of strings as a constant Value, allocating the
    /// strings + tuple into the compiler's heap. Returns the Value ready to
    /// stash in the constants pool. Used by import_from for the fromlist.
    fn materialize_tuple_const(&mut self, strs: &[&str]) -> Value {
        let mut items = Vec::with_capacity(strs.len());
        for s in strs {
            let str_idx = self.heap.len();
            self.heap.push(HeapObject::Str((*s).into()));
            items.push(Value::str_ref(str_idx));
        }
        let tuple_idx = self.heap.len();
        self.heap.push(HeapObject::Tuple(items));
        Value::object_ref(tuple_idx)
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
                return Err(PythonError::compile(
                    "cannot augmented-assign to tuple",
                    line,
                ));
            }
        }
        Ok(())
    }

    fn compile_store_target(
        &mut self,
        target: &AssignTarget,
        line: u32,
    ) -> Result<(), PythonError> {
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

    fn compile_while(
        &mut self,
        condition: &Expr,
        body: &[Stmt],
        line: u32,
    ) -> Result<(), PythonError> {
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

        let ctx = self.loop_stack.pop().ok_or_else(|| {
            PythonError::compile("internal: while-loop context missing on pop", line)
        })?;
        for bp in ctx.break_patches {
            self.patch_jump(bp);
        }

        Ok(())
    }

    fn compile_for(
        &mut self,
        target: &AssignTarget,
        iter: &Expr,
        body: &[Stmt],
        line: u32,
    ) -> Result<(), PythonError> {
        let iter_name = format!("__iter_{}__", target_name(target));
        let frame = self.emit_iter_loop_open(iter, target, &iter_name, line)?;

        self.loop_stack.push(LoopContext {
            break_patches: Vec::new(),
            continue_target: frame.loop_start,
        });

        for stmt in body {
            self.compile_stmt(stmt)?;
        }

        self.emit_iter_loop_close(&frame, line);

        let ctx = self.loop_stack.pop().ok_or_else(|| {
            PythonError::compile("internal: for-loop context missing on pop", line)
        })?;
        for bp in ctx.break_patches {
            self.patch_jump(bp);
        }

        Ok(())
    }

    /// Compile the for/if/recurse part of a comprehension. The container
    /// (list/set/dict) is already on the stack and stays there across
    /// every iteration. `clauses[clause_idx..]` are the remaining
    /// for-clauses (outermost at `clause_idx`); when exhausted we emit
    /// the element append.
    fn compile_comprehension(
        &mut self,
        clauses: &[ComprehensionClause],
        clause_idx: usize,
        kind: CompKind,
        line: u32,
    ) -> Result<(), PythonError> {
        if clause_idx == clauses.len() {
            // Innermost body: emit the element(s) and the *_ADD opcode.
            // The container is below the element(s) on the stack — every
            // *_ADD op pops them, mutates, and pushes the container back
            // so it stays TOS for the next iteration.
            match kind {
                CompKind::List(elt) => {
                    self.compile_expr(elt)?;
                    self.emit(op::LIST_APPEND, 0, line);
                }
                CompKind::Set(elt) => {
                    self.compile_expr(elt)?;
                    self.emit(op::SET_ADD, 0, line);
                }
                CompKind::Dict(key, value) => {
                    // MAP_ADD pops in order [key, value], so push value first.
                    self.compile_expr(value)?;
                    self.compile_expr(key)?;
                    self.emit(op::MAP_ADD, 0, line);
                }
            }
            return Ok(());
        }

        let clause = &clauses[clause_idx];
        // Unique-per-comprehension name; the bytecode offset already
        // distinguishes nested comprehensions in the same scope.
        let iter_name = format!("__comp_iter_{}__", self.current_offset());
        let frame = self.emit_iter_loop_open(&clause.iter, &clause.target, &iter_name, line)?;

        let mut cond_jumps = Vec::new();
        for cond in &clause.conditions {
            self.compile_expr(cond)?;
            let off = self.current_offset();
            self.emit(op::JUMP_IF_FALSE, 0, line);
            cond_jumps.push(off);
        }

        self.compile_comprehension(clauses, clause_idx + 1, kind, line)?;

        // Filter misses land here so the loop-back JUMP carries them
        // forward to the next iteration without escaping outer clauses.
        for off in cond_jumps {
            self.patch_jump(off);
        }
        self.emit_iter_loop_close(&frame, line);

        Ok(())
    }

    /// Emits the iterator-store + loop-start + load + FOR_ITER + target-
    /// store prologue shared by `compile_for` and `compile_comprehension`.
    /// `iter_name` is the temporary slot used to keep the iterator alive
    /// across iterations (FOR_ITER pops it each loop).
    fn emit_iter_loop_open(
        &mut self,
        iter: &Expr,
        target: &AssignTarget,
        iter_name: &str,
        line: u32,
    ) -> Result<IterLoopFrame, PythonError> {
        self.compile_expr(iter)?;
        self.emit(op::GET_ITER, 0, line);
        let is_module = self.is_module_level();
        if is_module {
            let name_idx = self.add_name(iter_name);
            self.emit(op::STORE_GLOBAL, name_idx, line);
        } else {
            let local_idx = self.add_local(iter_name);
            self.emit(op::STORE_FAST, local_idx, line);
        }

        let loop_start = self.current_offset();
        if is_module {
            let name_idx = self.add_name(iter_name);
            self.emit(op::LOAD_GLOBAL, name_idx, line);
        } else {
            let local_idx = self.find_local(iter_name).ok_or_else(|| {
                PythonError::compile(format!("internal: iter local '{iter_name}' missing"), line)
            })?;
            self.emit(op::LOAD_FAST, local_idx, line);
        }

        let for_iter_off = self.current_offset();
        self.emit(op::FOR_ITER, 0, line);
        self.compile_store_target(target, line)?;
        Ok(IterLoopFrame {
            loop_start,
            for_iter_off,
        })
    }

    /// Pair to `emit_iter_loop_open`: emits the loop-back JUMP and patches
    /// the FOR_ITER exit to land immediately after it.
    fn emit_iter_loop_close(&mut self, frame: &IterLoopFrame, line: u32) {
        self.emit(op::JUMP, frame.loop_start as u32, line);
        self.patch_jump(frame.for_iter_off);
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
        self.const_index.push(HashMap::new());
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
        self.const_index.push(HashMap::new());
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
                Stmt::If {
                    body,
                    elif_clauses,
                    else_body,
                    ..
                } => {
                    self.prescan_locals(co, body);
                    for (_, elif_body) in elif_clauses {
                        self.prescan_locals(co, elif_body);
                    }
                    self.prescan_locals(co, else_body);
                }
                Stmt::While { body, .. } => {
                    self.prescan_locals(co, body);
                }
                Stmt::Try {
                    body,
                    handlers,
                    else_body,
                    finally_body,
                    ..
                } => {
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
                Stmt::FunctionDef { name, .. } | Stmt::ClassDef { name, .. }
                    if !co.local_names.contains(name) =>
                {
                    co.local_names.push(name.clone());
                    co.num_locals = co.local_names.len();
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
            Expr::BinOp {
                left,
                op: binop,
                right,
                line,
            } => {
                self.compile_expr(left)?;
                self.compile_expr(right)?;
                let opcode = self.binop_to_opcode(binop);
                self.emit(opcode, 0, *line);
            }
            Expr::UnaryOp {
                op: unop,
                operand,
                line,
            } => {
                self.compile_expr(operand)?;
                let opcode = match unop {
                    UnaryOp::Neg => op::UNARY_NEG,
                    UnaryOp::Not => op::UNARY_NOT,
                    UnaryOp::Pos => op::UNARY_POS,
                    UnaryOp::Invert => op::UNARY_INVERT,
                };
                self.emit(opcode, 0, *line);
            }
            Expr::Compare {
                left,
                ops,
                comparators,
                line,
            } => {
                self.compile_comparison(left, ops, comparators, *line)?;
            }
            Expr::BoolOp {
                op: boolop,
                left,
                right,
                line,
            } => {
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
                if let Expr::Slice {
                    start, stop, step, ..
                } = index.as_ref()
                {
                    let none_const = self.add_const(Value::none());
                    let emit_part =
                        |co: &mut Self, part: &Option<Box<Expr>>| -> Result<(), PythonError> {
                            match part {
                                Some(e) => co.compile_expr(e)?,
                                None => co.emit(op::LOAD_CONST, none_const, *line),
                            }
                            Ok(())
                        };
                    emit_part(self, start)?;
                    emit_part(self, stop)?;
                    emit_part(self, step)?;
                    self.emit(op::SLICE_SUBSCRIPT, 0, *line);
                } else {
                    self.compile_expr(index)?;
                    self.emit(op::SUBSCRIPT, 0, *line);
                }
            }
            Expr::Slice { line, .. } => {
                return Err(PythonError::compile(
                    "slice syntax is only valid inside `[]`",
                    *line,
                ));
            }
            Expr::ListComp { elt, clauses, line } => {
                self.emit(op::BUILD_LIST, 0, *line);
                self.compile_comprehension(clauses, 0, CompKind::List(elt), *line)?;
            }
            Expr::SetComp { elt, clauses, line } => {
                self.emit(op::BUILD_SET, 0, *line);
                self.compile_comprehension(clauses, 0, CompKind::Set(elt), *line)?;
            }
            Expr::DictComp {
                key,
                value,
                clauses,
                line,
            } => {
                self.emit(op::BUILD_DICT, 0, *line);
                self.compile_comprehension(clauses, 0, CompKind::Dict(key, value), *line)?;
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
                self.const_index.push(HashMap::new());
                self.code_stack.push(func_co_idx);

                self.compile_expr(body)?;
                self.emit(op::RETURN_VALUE, 0, *line);

                self.code_stack.pop();

                let func_idx_const = self.add_const(Value::small_int_unchecked(func_co_idx as i64));
                self.emit(op::MAKE_FUNCTION, func_idx_const, *line);
            }
            Expr::IfExpr {
                body,
                test,
                orelse,
                line,
            } => {
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
        AssignTarget::Name(name) if !co.local_names.contains(name) => {
            co.local_names.push(name.clone());
            co.num_locals = co.local_names.len();
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
            Stmt::Assign { target, value, .. } | Stmt::AugAssign { target, value, .. } => {
                collect_target_names(target, scope);
                collect_comp_targets_in_expr(value, scope);
            }
            Stmt::ExprStmt { expr, .. } => collect_comp_targets_in_expr(expr, scope),
            Stmt::For {
                target, iter, body, ..
            } => {
                collect_target_names(target, scope);
                collect_comp_targets_in_expr(iter, scope);
                collect_locals(body, scope);
            }
            Stmt::If {
                condition,
                body,
                elif_clauses,
                else_body,
                ..
            } => {
                collect_comp_targets_in_expr(condition, scope);
                collect_locals(body, scope);
                for (c, b) in elif_clauses {
                    collect_comp_targets_in_expr(c, scope);
                    collect_locals(b, scope);
                }
                collect_locals(else_body, scope);
            }
            Stmt::While {
                condition, body, ..
            } => {
                collect_comp_targets_in_expr(condition, scope);
                collect_locals(body, scope);
            }
            Stmt::FunctionDef { name, .. } => {
                scope.locals.insert(name.clone());
            }
            Stmt::ClassDef { name, .. } => {
                scope.locals.insert(name.clone());
            }
            Stmt::Return { value: Some(v), .. } | Stmt::Raise { exc: Some(v), .. } => {
                collect_comp_targets_in_expr(v, scope);
            }
            _ => {}
        }
    }
}

/// Walk an expression collecting any comprehension's loop-target names.
/// Comprehensions share the enclosing scope (we do not emit a hidden
/// function frame), so their targets are assignments at this level and
/// must be recorded as locals for closure analysis to see them.
fn collect_comp_targets_in_expr(expr: &Expr, scope: &mut ScopeInfo) {
    match expr {
        Expr::ListComp { elt, clauses, .. } | Expr::SetComp { elt, clauses, .. } => {
            for c in clauses {
                collect_target_names(&c.target, scope);
                collect_comp_targets_in_expr(&c.iter, scope);
                for cond in &c.conditions {
                    collect_comp_targets_in_expr(cond, scope);
                }
            }
            collect_comp_targets_in_expr(elt, scope);
        }
        Expr::DictComp {
            key,
            value,
            clauses,
            ..
        } => {
            for c in clauses {
                collect_target_names(&c.target, scope);
                collect_comp_targets_in_expr(&c.iter, scope);
                for cond in &c.conditions {
                    collect_comp_targets_in_expr(cond, scope);
                }
            }
            collect_comp_targets_in_expr(key, scope);
            collect_comp_targets_in_expr(value, scope);
        }
        Expr::BinOp { left, right, .. } | Expr::BoolOp { left, right, .. } => {
            collect_comp_targets_in_expr(left, scope);
            collect_comp_targets_in_expr(right, scope);
        }
        Expr::UnaryOp { operand, .. } => collect_comp_targets_in_expr(operand, scope),
        Expr::Compare {
            left, comparators, ..
        } => {
            collect_comp_targets_in_expr(left, scope);
            for c in comparators {
                collect_comp_targets_in_expr(c, scope);
            }
        }
        Expr::Call { func, args, .. } => {
            collect_comp_targets_in_expr(func, scope);
            for a in args {
                collect_comp_targets_in_expr(a, scope);
            }
        }
        Expr::Subscript { value, index, .. } => {
            collect_comp_targets_in_expr(value, scope);
            collect_comp_targets_in_expr(index, scope);
        }
        Expr::Attribute { value, .. } | Expr::Starred { value, .. } => {
            collect_comp_targets_in_expr(value, scope);
        }
        Expr::List { elements, .. } | Expr::Tuple { elements, .. } | Expr::Set { elements, .. } => {
            for e in elements {
                collect_comp_targets_in_expr(e, scope);
            }
        }
        Expr::Dict { keys, values, .. } => {
            for k in keys {
                collect_comp_targets_in_expr(k, scope);
            }
            for v in values {
                collect_comp_targets_in_expr(v, scope);
            }
        }
        Expr::IfExpr {
            body, test, orelse, ..
        } => {
            collect_comp_targets_in_expr(body, scope);
            collect_comp_targets_in_expr(test, scope);
            collect_comp_targets_in_expr(orelse, scope);
        }
        Expr::Slice {
            start, stop, step, ..
        } => {
            if let Some(s) = start {
                collect_comp_targets_in_expr(s, scope);
            }
            if let Some(s) = stop {
                collect_comp_targets_in_expr(s, scope);
            }
            if let Some(s) = step {
                collect_comp_targets_in_expr(s, scope);
            }
        }
        Expr::Yield { value: Some(v), .. } => collect_comp_targets_in_expr(v, scope),
        // Lambda has its own scope; do not descend.
        _ => {}
    }
}

fn collect_target_names(target: &AssignTarget, scope: &mut ScopeInfo) {
    match target {
        AssignTarget::Name(name)
            if !scope.globals.contains(name) && !scope.nonlocals.contains(name) =>
        {
            scope.locals.insert(name.clone());
        }
        AssignTarget::Tuple(targets) => {
            for t in targets {
                collect_target_names(t, scope);
            }
        }
        _ => {}
    }
}

/// Walk a function body collecting every Name reference. Used to detect
/// implicit free-variable captures: any name referenced here that's not
/// local to this scope but is local in an enclosing scope becomes a
/// closure cell.
///
/// Does NOT recurse into nested function/class bodies — their captures
/// are computed by their own `compute_free_vars` pass.
fn collect_name_refs(stmts: &[Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            Stmt::Assign { value, .. }
            | Stmt::AugAssign { value, .. }
            | Stmt::ExprStmt { expr: value, .. } => expr_collect_names(value, out),
            Stmt::If {
                condition,
                body,
                elif_clauses,
                else_body,
                ..
            } => {
                expr_collect_names(condition, out);
                collect_name_refs(body, out);
                for (c, b) in elif_clauses {
                    expr_collect_names(c, out);
                    collect_name_refs(b, out);
                }
                collect_name_refs(else_body, out);
            }
            Stmt::While {
                condition, body, ..
            } => {
                expr_collect_names(condition, out);
                collect_name_refs(body, out);
            }
            Stmt::For { iter, body, .. } => {
                expr_collect_names(iter, out);
                collect_name_refs(body, out);
            }
            Stmt::Return { value: Some(v), .. } => expr_collect_names(v, out),
            Stmt::Raise { exc: Some(v), .. } => expr_collect_names(v, out),
            Stmt::Assert { test, msg, .. } => {
                expr_collect_names(test, out);
                if let Some(m) = msg {
                    expr_collect_names(m, out);
                }
            }
            Stmt::Try {
                body,
                handlers,
                else_body,
                finally_body,
                ..
            } => {
                collect_name_refs(body, out);
                for h in handlers {
                    if let Some(t) = &h.exc_type {
                        expr_collect_names(t, out);
                    }
                    collect_name_refs(&h.body, out);
                }
                collect_name_refs(else_body, out);
                collect_name_refs(finally_body, out);
            }
            // Nested function/class — params + bodies are their own scope;
            // skip recursion. The nested function's own compute_free_vars
            // will detect what IT captures.
            _ => {}
        }
    }
}

fn expr_collect_names(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Name { id, .. } => {
            out.insert(id.clone());
        }
        Expr::BinOp { left, right, .. } => {
            expr_collect_names(left, out);
            expr_collect_names(right, out);
        }
        Expr::UnaryOp { operand, .. } => expr_collect_names(operand, out),
        Expr::Compare {
            left, comparators, ..
        } => {
            expr_collect_names(left, out);
            for c in comparators {
                expr_collect_names(c, out);
            }
        }
        Expr::BoolOp { left, right, .. } => {
            expr_collect_names(left, out);
            expr_collect_names(right, out);
        }
        Expr::Call { func, args, .. } => {
            expr_collect_names(func, out);
            for a in args {
                expr_collect_names(a, out);
            }
        }
        Expr::Subscript { value, index, .. } => {
            expr_collect_names(value, out);
            expr_collect_names(index, out);
        }
        Expr::Attribute { value, .. } => expr_collect_names(value, out),
        Expr::List { elements, .. } | Expr::Tuple { elements, .. } | Expr::Set { elements, .. } => {
            for e in elements {
                expr_collect_names(e, out);
            }
        }
        Expr::Dict { keys, values, .. } => {
            for k in keys {
                expr_collect_names(k, out);
            }
            for v in values {
                expr_collect_names(v, out);
            }
        }
        Expr::IfExpr {
            body, test, orelse, ..
        } => {
            expr_collect_names(body, out);
            expr_collect_names(test, out);
            expr_collect_names(orelse, out);
        }
        Expr::Yield { value: Some(v), .. } => expr_collect_names(v, out),
        Expr::Starred { value, .. } => expr_collect_names(value, out),
        Expr::Slice {
            start, stop, step, ..
        } => {
            if let Some(s) = start {
                expr_collect_names(s, out);
            }
            if let Some(s) = stop {
                expr_collect_names(s, out);
            }
            if let Some(s) = step {
                expr_collect_names(s, out);
            }
        }
        // Comprehensions share the enclosing scope (no hidden function
        // frame yet), so referenced names inside leak out for closure
        // analysis. Targets are intentionally NOT walked: they're bound
        // by the comp's STORE_FAST, not by capture.
        Expr::ListComp { elt, clauses, .. } | Expr::SetComp { elt, clauses, .. } => {
            expr_collect_names(elt, out);
            for c in clauses {
                expr_collect_names(&c.iter, out);
                for cond in &c.conditions {
                    expr_collect_names(cond, out);
                }
            }
        }
        Expr::DictComp {
            key,
            value,
            clauses,
            ..
        } => {
            expr_collect_names(key, out);
            expr_collect_names(value, out);
            for c in clauses {
                expr_collect_names(&c.iter, out);
                for cond in &c.conditions {
                    expr_collect_names(cond, out);
                }
            }
        }
        // Lambdas have their own scope; skip recursion.
        _ => {}
    }
}

/// Check if a function body contains yield expressions.
fn contains_yield(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        match stmt {
            Stmt::ExprStmt { expr, .. }
            | Stmt::Assign { value: expr, .. }
            | Stmt::Return {
                value: Some(expr), ..
            } if expr_contains_yield(expr) => {
                return true;
            }
            Stmt::If {
                body,
                elif_clauses,
                else_body,
                ..
            } => {
                if contains_yield(body) || contains_yield(else_body) {
                    return true;
                }
                for (_, b) in elif_clauses {
                    if contains_yield(b) {
                        return true;
                    }
                }
            }
            Stmt::While { body, .. } | Stmt::For { body, .. } if contains_yield(body) => {
                return true;
            }
            Stmt::Try {
                body,
                handlers,
                else_body,
                finally_body,
                ..
            } => {
                if contains_yield(body) || contains_yield(else_body) || contains_yield(finally_body)
                {
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
        Expr::BinOp { left, right, .. } => expr_contains_yield(left) || expr_contains_yield(right),
        Expr::UnaryOp { operand, .. } => expr_contains_yield(operand),
        Expr::BoolOp { left, right, .. } => expr_contains_yield(left) || expr_contains_yield(right),
        Expr::Compare {
            left, comparators, ..
        } => expr_contains_yield(left) || comparators.iter().any(expr_contains_yield),
        Expr::IfExpr {
            body, test, orelse, ..
        } => expr_contains_yield(body) || expr_contains_yield(test) || expr_contains_yield(orelse),
        Expr::Attribute { value, .. }
        | Expr::Subscript { value, .. }
        | Expr::Starred { value, .. } => expr_contains_yield(value),
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
        let ops: Vec<u8> = cos[0]
            .instructions
            .iter()
            .map(|i| bytecode::decode_op(*i))
            .collect();
        assert!(ops.contains(&op::GET_ITER));
        assert!(ops.contains(&op::FOR_ITER));
    }

    #[test]
    fn const_pool_deduplicates_repeated_literals() {
        // Three occurrences of the literal 0 must collapse to one constant.
        // Verifies the HashMap-backed dedup in add_const works end-to-end.
        let (cos, _) = compile_src("x = 0\ny = 0\nz = 0\n");
        let zeros = cos[0]
            .constants
            .iter()
            .filter(|c| c.as_int() == Some(0))
            .count();
        assert_eq!(
            zeros, 1,
            "expected the constant 0 to appear exactly once in the pool"
        );
    }

    #[test]
    fn const_pool_keeps_distinct_literals_distinct() {
        // Counterpart to the dedup test: a hash-key collision that lost the
        // value comparison would erroneously dedupe these into one entry.
        let (cos, _) = compile_src("x = 0\ny = 1\nz = 2\n");
        let mut ints: Vec<i64> = cos[0].constants.iter().filter_map(|c| c.as_int()).collect();
        ints.sort();
        assert_eq!(
            ints,
            vec![0, 1, 2],
            "distinct int literals must not be deduped"
        );
    }

    #[test]
    fn compile_class() {
        let (cos, _) = compile_src("class Foo:\n    pass\n");
        assert!(cos.len() >= 2);
    }

    #[test]
    fn compile_try_except() {
        let (cos, _) = compile_src("try:\n    pass\nexcept:\n    pass\n");
        let ops: Vec<u8> = cos[0]
            .instructions
            .iter()
            .map(|i| bytecode::decode_op(*i))
            .collect();
        assert!(ops.contains(&op::SETUP_EXCEPT));
    }

    #[test]
    fn compile_generator() {
        let (cos, _) = compile_src("def gen():\n    yield 1\n");
        assert!(cos[1].is_generator);
    }

    // ---------- M3 commit 3: compile import statements ----------

    #[test]
    fn compile_import_emits_import_name_and_store() {
        let (cos, _) = compile_src("import foo\n");
        let ops: Vec<u8> = cos[0]
            .instructions
            .iter()
            .map(|i| bytecode::decode_op(*i))
            .collect();
        assert!(ops.contains(&op::IMPORT_NAME));
        assert!(ops.contains(&op::STORE_GLOBAL));
    }

    #[test]
    fn compile_from_import_emits_from_and_pop() {
        let (cos, _) = compile_src("from foo import a, b\n");
        let ops: Vec<u8> = cos[0]
            .instructions
            .iter()
            .map(|i| bytecode::decode_op(*i))
            .collect();
        assert!(ops.contains(&op::IMPORT_NAME));
        assert_eq!(ops.iter().filter(|&&o| o == op::IMPORT_FROM).count(), 2);
        assert!(ops.contains(&op::POP_TOP));
    }

    #[test]
    fn compile_from_import_star_emits_import_star() {
        let (cos, _) = compile_src("from foo import *\n");
        let ops: Vec<u8> = cos[0]
            .instructions
            .iter()
            .map(|i| bytecode::decode_op(*i))
            .collect();
        assert!(ops.contains(&op::IMPORT_STAR));
        // Star imports do NOT emit a trailing POP_TOP — IMPORT_STAR consumes the module.
        assert!(!ops.contains(&op::IMPORT_FROM));
    }

    #[test]
    fn compile_import_dotted_binds_top_level() {
        // `import foo.bar` should bind `foo`, not `foo.bar`, in the current scope.
        let (cos, _) = compile_src("import foo.bar\n");
        assert!(
            cos[0].names.contains(&"foo.bar".to_string()),
            "expected 'foo.bar' in names for IMPORT_NAME operand"
        );
        assert!(
            cos[0].names.contains(&"foo".to_string()),
            "expected 'foo' in names for STORE_GLOBAL bind"
        );
    }
}
