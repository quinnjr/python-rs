/// Recursive-descent parser with Pratt precedence climbing.
use crate::ast::*;
use crate::error::PythonError;
use crate::lexer::{Token, TokenKind};

/// Parse a token stream into an AST module.
pub fn parse(tokens: Vec<Token>) -> Result<Module, PythonError> {
    let mut parser = Parser::new(tokens);
    let body = parser.parse_module()?;
    Ok(Module { body })
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> &TokenKind {
        if self.pos < self.tokens.len() {
            &self.tokens[self.pos].kind
        } else {
            &TokenKind::Eof
        }
    }

    fn peek_line(&self) -> u32 {
        if self.pos < self.tokens.len() {
            self.tokens[self.pos].line
        } else {
            0
        }
    }

    fn peek_nth(&self, n: usize) -> &TokenKind {
        let idx = self.pos + n;
        if idx < self.tokens.len() {
            &self.tokens[idx].kind
        } else {
            &TokenKind::Eof
        }
    }

    fn advance(&mut self) -> &Token {
        let tok = &self.tokens[self.pos];
        self.pos += 1;
        tok
    }

    fn expect(&mut self, expected: &TokenKind) -> Result<(), PythonError> {
        if self.peek() == expected {
            self.advance();
            Ok(())
        } else {
            Err(PythonError::parse(
                format!("expected {expected:?}, got {:?}", self.peek()),
                self.peek_line(),
            ))
        }
    }

    fn eat_newlines(&mut self) {
        while self.peek() == &TokenKind::Newline {
            self.advance();
        }
    }

    fn parse_module(&mut self) -> Result<Vec<Stmt>, PythonError> {
        let mut stmts = Vec::new();
        self.eat_newlines();
        while self.peek() != &TokenKind::Eof {
            stmts.push(self.parse_stmt()?);
            self.eat_newlines();
        }
        Ok(stmts)
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, PythonError> {
        self.expect(&TokenKind::Newline)?;
        self.expect(&TokenKind::Indent)?;
        let mut stmts = Vec::new();
        while self.peek() != &TokenKind::Dedent && self.peek() != &TokenKind::Eof {
            stmts.push(self.parse_stmt()?);
            self.eat_newlines();
        }
        if self.peek() == &TokenKind::Dedent {
            self.advance();
        }
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, PythonError> {
        // Check for decorators before def/class
        if self.peek() == &TokenKind::At {
            return self.parse_decorated();
        }
        match self.peek().clone() {
            TokenKind::If => self.parse_if(),
            TokenKind::While => self.parse_while(),
            TokenKind::For => self.parse_for(),
            TokenKind::Def => self.parse_function_def(Vec::new()),
            TokenKind::Return => self.parse_return(),
            TokenKind::Pass => self.parse_pass(),
            TokenKind::Break => self.parse_break(),
            TokenKind::Continue => self.parse_continue(),
            TokenKind::Class => self.parse_class_def(Vec::new()),
            TokenKind::Try => self.parse_try(),
            TokenKind::Raise => self.parse_raise(),
            TokenKind::Assert => self.parse_assert(),
            TokenKind::Del => self.parse_delete(),
            TokenKind::Global => self.parse_global(),
            TokenKind::Nonlocal => self.parse_nonlocal(),
            TokenKind::Import => self.parse_import(),
            TokenKind::From => self.parse_from_import(),
            _ => self.parse_assign_or_expr(),
        }
    }

    fn parse_decorated(&mut self) -> Result<Stmt, PythonError> {
        let mut decorators = Vec::new();
        while self.peek() == &TokenKind::At {
            self.advance(); // consume '@'
            let dec = self.parse_expr()?;
            decorators.push(dec);
            if self.peek() == &TokenKind::Newline {
                self.advance();
            }
        }
        match self.peek().clone() {
            TokenKind::Def => self.parse_function_def(decorators),
            TokenKind::Class => self.parse_class_def(decorators),
            _ => Err(PythonError::parse(
                "expected 'def' or 'class' after decorator",
                self.peek_line(),
            )),
        }
    }

    fn parse_if(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'if'
        let condition = self.parse_expr()?;
        self.expect(&TokenKind::Colon)?;
        let body = self.parse_block()?;

        let mut elif_clauses = Vec::new();
        let mut else_body = Vec::new();

        loop {
            self.eat_newlines();
            if self.peek() == &TokenKind::Elif {
                self.advance();
                let elif_cond = self.parse_expr()?;
                self.expect(&TokenKind::Colon)?;
                let elif_body = self.parse_block()?;
                elif_clauses.push((elif_cond, elif_body));
            } else if self.peek() == &TokenKind::Else {
                self.advance();
                self.expect(&TokenKind::Colon)?;
                else_body = self.parse_block()?;
                break;
            } else {
                break;
            }
        }

        Ok(Stmt::If {
            condition,
            body,
            elif_clauses,
            else_body,
            line,
        })
    }

    fn parse_while(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance();
        let condition = self.parse_expr()?;
        self.expect(&TokenKind::Colon)?;
        let body = self.parse_block()?;
        Ok(Stmt::While {
            condition,
            body,
            line,
        })
    }

    fn parse_for(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'for'
        let target = self.parse_assign_target_for()?;
        self.expect(&TokenKind::In)?;
        let iter = self.parse_expr()?;
        self.expect(&TokenKind::Colon)?;
        let body = self.parse_block()?;
        Ok(Stmt::For {
            target,
            iter,
            body,
            line,
        })
    }

    /// Parse assignment target in for-loop context (allows tuple unpacking).
    fn parse_assign_target_for(&mut self) -> Result<AssignTarget, PythonError> {
        let first = self.parse_single_target()?;
        if self.peek() == &TokenKind::Comma {
            let mut targets = vec![first];
            while self.peek() == &TokenKind::Comma {
                self.advance();
                if self.peek() == &TokenKind::In {
                    break;
                }
                targets.push(self.parse_single_target()?);
            }
            Ok(AssignTarget::Tuple(targets))
        } else {
            Ok(first)
        }
    }

    fn parse_single_target(&mut self) -> Result<AssignTarget, PythonError> {
        match self.peek().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                Ok(AssignTarget::Name(name))
            }
            TokenKind::LParen => {
                self.advance();
                let target = self.parse_assign_target_for()?;
                self.expect(&TokenKind::RParen)?;
                Ok(target)
            }
            _ => Err(PythonError::parse(
                format!(
                    "expected identifier in assignment target, got {:?}",
                    self.peek()
                ),
                self.peek_line(),
            )),
        }
    }

    fn parse_function_def(&mut self, decorators: Vec<Expr>) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'def'
        let name = match self.peek().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                name
            }
            _ => {
                return Err(PythonError::parse(
                    "expected function name",
                    self.peek_line(),
                ));
            }
        };
        self.expect(&TokenKind::LParen)?;
        let mut params = Vec::new();
        while self.peek() != &TokenKind::RParen {
            if !params.is_empty() {
                self.expect(&TokenKind::Comma)?;
                if self.peek() == &TokenKind::RParen {
                    break;
                }
            }
            match self.peek().clone() {
                TokenKind::Ident(p) => {
                    self.advance();
                    params.push(p);
                }
                _ => {
                    return Err(PythonError::parse(
                        "expected parameter name",
                        self.peek_line(),
                    ));
                }
            }
        }
        self.expect(&TokenKind::RParen)?;
        self.expect(&TokenKind::Colon)?;
        let body = self.parse_block()?;
        Ok(Stmt::FunctionDef {
            name,
            params,
            body,
            decorators,
            line,
        })
    }

    fn parse_class_def(&mut self, decorators: Vec<Expr>) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'class'
        let name = match self.peek().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                name
            }
            _ => return Err(PythonError::parse("expected class name", self.peek_line())),
        };

        let mut bases = Vec::new();
        if self.peek() == &TokenKind::LParen {
            self.advance();
            while self.peek() != &TokenKind::RParen {
                if !bases.is_empty() {
                    self.expect(&TokenKind::Comma)?;
                    if self.peek() == &TokenKind::RParen {
                        break;
                    }
                }
                bases.push(self.parse_expr()?);
            }
            self.expect(&TokenKind::RParen)?;
        }

        self.expect(&TokenKind::Colon)?;
        let body = self.parse_block()?;
        Ok(Stmt::ClassDef {
            name,
            bases,
            body,
            decorators,
            line,
        })
    }

    fn parse_try(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'try'
        self.expect(&TokenKind::Colon)?;
        let body = self.parse_block()?;

        let mut handlers = Vec::new();
        let mut else_body = Vec::new();
        let mut finally_body = Vec::new();

        self.eat_newlines();
        while self.peek() == &TokenKind::Except {
            let handler_line = self.peek_line();
            self.advance(); // consume 'except'

            let (exc_type, name) = if self.peek() == &TokenKind::Colon {
                (None, None)
            } else {
                let exc = self.parse_expr()?;
                let name = if self.peek() == &TokenKind::As {
                    self.advance();
                    match self.peek().clone() {
                        TokenKind::Ident(n) => {
                            self.advance();
                            Some(n)
                        }
                        _ => {
                            return Err(PythonError::parse(
                                "expected name after 'as'",
                                self.peek_line(),
                            ));
                        }
                    }
                } else {
                    None
                };
                (Some(exc), name)
            };

            self.expect(&TokenKind::Colon)?;
            let handler_body = self.parse_block()?;
            handlers.push(ExceptHandler {
                exc_type,
                name,
                body: handler_body,
                line: handler_line,
            });
            self.eat_newlines();
        }

        if self.peek() == &TokenKind::Else {
            self.advance();
            self.expect(&TokenKind::Colon)?;
            else_body = self.parse_block()?;
            self.eat_newlines();
        }

        if self.peek() == &TokenKind::Finally {
            self.advance();
            self.expect(&TokenKind::Colon)?;
            finally_body = self.parse_block()?;
        }

        Ok(Stmt::Try {
            body,
            handlers,
            else_body,
            finally_body,
            line,
        })
    }

    fn parse_raise(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'raise'
        let exc = if self.peek() == &TokenKind::Newline || self.peek() == &TokenKind::Eof {
            None
        } else {
            Some(self.parse_expr()?)
        };
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::Raise { exc, line })
    }

    fn parse_assert(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'assert'
        let test = self.parse_expr()?;
        let msg = if self.peek() == &TokenKind::Comma {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::Assert { test, msg, line })
    }

    fn parse_delete(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'del'
        let expr = self.parse_expr()?;
        let target = expr_to_target(expr, line)?;
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::Delete { target, line })
    }

    fn parse_global(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'global'
        let mut names = Vec::new();
        loop {
            match self.peek().clone() {
                TokenKind::Ident(n) => {
                    self.advance();
                    names.push(n);
                }
                _ => {
                    return Err(PythonError::parse(
                        "expected name after 'global'",
                        self.peek_line(),
                    ));
                }
            }
            if self.peek() != &TokenKind::Comma {
                break;
            }
            self.advance();
        }
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::GlobalDecl { names, line })
    }

    fn parse_nonlocal(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'nonlocal'
        let mut names = Vec::new();
        loop {
            match self.peek().clone() {
                TokenKind::Ident(n) => {
                    self.advance();
                    names.push(n);
                }
                _ => {
                    return Err(PythonError::parse(
                        "expected name after 'nonlocal'",
                        self.peek_line(),
                    ));
                }
            }
            if self.peek() != &TokenKind::Comma {
                break;
            }
            self.advance();
        }
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::NonlocalDecl { names, line })
    }

    /// Parse `import foo`, `import foo.bar`, `import a, b as c`.
    fn parse_import(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'import'
        let mut names = Vec::new();
        loop {
            let dotted = self.parse_dotted_name()?;
            let asname = if self.peek() == &TokenKind::As {
                self.advance();
                Some(self.expect_ident("expected name after 'as'")?)
            } else {
                None
            };
            names.push(ImportAlias {
                name: dotted,
                asname,
            });
            if self.peek() != &TokenKind::Comma {
                break;
            }
            self.advance();
        }
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::Import { names, line })
    }

    /// Parse `from foo import bar`, `from foo.bar import baz, qux as q`,
    /// `from . import x`, `from ..pkg import y`, `from foo import *`.
    fn parse_from_import(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'from'

        // Leading dots → relative-import level.
        let mut level: u32 = 0;
        while self.peek() == &TokenKind::Dot {
            self.advance();
            level += 1;
        }
        // Ellipsis (...) counts as three dots in Python's grammar.
        while self.peek() == &TokenKind::Ellipsis {
            self.advance();
            level += 3;
        }

        // Module name (optional when level > 0: `from . import x`).
        let module = match self.peek() {
            TokenKind::Ident(_) => Some(self.parse_dotted_name()?),
            _ if level > 0 => None,
            other => {
                return Err(PythonError::parse(
                    format!("expected module name after 'from', got {other:?}"),
                    line,
                ));
            }
        };

        if self.peek() != &TokenKind::Import {
            return Err(PythonError::parse(
                "expected 'import' after 'from <module>'",
                self.peek_line(),
            ));
        }
        self.advance(); // consume 'import'

        // Star import?
        if self.peek() == &TokenKind::Star {
            self.advance();
            if self.peek() == &TokenKind::Newline {
                self.advance();
            }
            return Ok(Stmt::ImportFrom {
                module,
                names: Vec::new(),
                level,
                is_star: true,
                line,
            });
        }

        // Optional parenthesized name list — Python allows `from foo import (a, b, c,)`.
        let parenthesized = self.peek() == &TokenKind::LParen;
        if parenthesized {
            self.advance();
        }

        let mut names = Vec::new();
        loop {
            let name = self.expect_ident("expected name in import list")?;
            let asname = if self.peek() == &TokenKind::As {
                self.advance();
                Some(self.expect_ident("expected name after 'as'")?)
            } else {
                None
            };
            names.push(ImportAlias { name, asname });
            if self.peek() != &TokenKind::Comma {
                break;
            }
            self.advance();
            // Trailing comma allowed inside parens.
            if parenthesized && self.peek() == &TokenKind::RParen {
                break;
            }
        }
        if parenthesized {
            if self.peek() != &TokenKind::RParen {
                return Err(PythonError::parse(
                    "expected ')' closing import list",
                    self.peek_line(),
                ));
            }
            self.advance();
        }
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::ImportFrom {
            module,
            names,
            level,
            is_star: false,
            line,
        })
    }

    /// Parse a dotted name like `foo` or `foo.bar.baz`. Caller must
    /// know the first token is an Ident.
    fn parse_dotted_name(&mut self) -> Result<String, PythonError> {
        let mut parts = vec![self.expect_ident("expected name")?];
        while self.peek() == &TokenKind::Dot {
            // Look ahead: only consume the dot if an Ident follows. Stops at
            // an import-list comma in cases like `from foo. import x` (which
            // is actually invalid syntax — but the lookahead keeps us from
            // greedily consuming a trailing dot in malformed input).
            let saved_pos = self.pos;
            self.advance(); // consume '.'
            match self.peek() {
                TokenKind::Ident(_) => parts.push(self.expect_ident("expected name after '.'")?),
                _ => {
                    self.pos = saved_pos;
                    break;
                }
            }
        }
        Ok(parts.join("."))
    }

    fn expect_ident(&mut self, err_msg: &'static str) -> Result<String, PythonError> {
        match self.peek().clone() {
            TokenKind::Ident(n) => {
                self.advance();
                Ok(n)
            }
            _ => Err(PythonError::parse(err_msg, self.peek_line())),
        }
    }

    fn parse_return(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance();
        let value = if self.peek() == &TokenKind::Newline || self.peek() == &TokenKind::Eof {
            None
        } else {
            Some(self.parse_expr()?)
        };
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::Return { value, line })
    }

    fn parse_pass(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance();
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::Pass { line })
    }

    fn parse_break(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance();
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::Break { line })
    }

    fn parse_continue(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        self.advance();
        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::Continue { line })
    }

    fn parse_assign_or_expr(&mut self) -> Result<Stmt, PythonError> {
        let line = self.peek_line();
        let expr = self.parse_expr_maybe_tuple()?;

        match self.peek() {
            TokenKind::Assign => {
                self.advance();
                let value = self.parse_expr_maybe_tuple()?;
                if self.peek() == &TokenKind::Newline {
                    self.advance();
                }
                let target = expr_to_target(expr, line)?;
                return Ok(Stmt::Assign {
                    target,
                    value,
                    line,
                });
            }
            TokenKind::PlusAssign
            | TokenKind::MinusAssign
            | TokenKind::StarAssign
            | TokenKind::SlashAssign
            | TokenKind::DoubleSlashAssign
            | TokenKind::PercentAssign
            | TokenKind::DoubleStarAssign
            | TokenKind::AmpersandAssign
            | TokenKind::PipeAssign
            | TokenKind::CaretAssign
            | TokenKind::LShiftAssign
            | TokenKind::RShiftAssign => {
                let op = match self.peek() {
                    TokenKind::PlusAssign => BinOp::Add,
                    TokenKind::MinusAssign => BinOp::Sub,
                    TokenKind::StarAssign => BinOp::Mul,
                    TokenKind::SlashAssign => BinOp::Div,
                    TokenKind::DoubleSlashAssign => BinOp::FloorDiv,
                    TokenKind::PercentAssign => BinOp::Mod,
                    TokenKind::DoubleStarAssign => BinOp::Pow,
                    TokenKind::AmpersandAssign => BinOp::BitAnd,
                    TokenKind::PipeAssign => BinOp::BitOr,
                    TokenKind::CaretAssign => BinOp::BitXor,
                    TokenKind::LShiftAssign => BinOp::LShift,
                    TokenKind::RShiftAssign => BinOp::RShift,
                    _ => unreachable!(),
                };
                self.advance();
                let value = self.parse_expr_maybe_tuple()?;
                if self.peek() == &TokenKind::Newline {
                    self.advance();
                }
                let target = expr_to_target(expr, line)?;
                return Ok(Stmt::AugAssign {
                    target,
                    op,
                    value,
                    line,
                });
            }
            _ => {}
        }

        if self.peek() == &TokenKind::Newline {
            self.advance();
        }
        Ok(Stmt::ExprStmt { expr, line })
    }

    /// Parse an expression, allowing tuple creation with commas.
    fn parse_expr_maybe_tuple(&mut self) -> Result<Expr, PythonError> {
        let line = self.peek_line();
        let first = self.parse_expr()?;
        if self.peek() == &TokenKind::Comma {
            let mut elements = vec![first];
            while self.peek() == &TokenKind::Comma {
                self.advance();
                // Allow trailing comma before newline/colon/rparen/rbracket/assign
                if matches!(
                    self.peek(),
                    TokenKind::Newline
                        | TokenKind::Colon
                        | TokenKind::RParen
                        | TokenKind::RBracket
                        | TokenKind::Eof
                        | TokenKind::Assign
                        | TokenKind::PlusAssign
                        | TokenKind::MinusAssign
                        | TokenKind::StarAssign
                ) {
                    break;
                }
                elements.push(self.parse_expr()?);
            }
            Ok(Expr::Tuple { elements, line })
        } else {
            Ok(first)
        }
    }

    // Expression parsing — full precedence tower
    // lambda < ternary < or < and < not < comparisons < | < ^ < & < shifts < add/sub < mul/div/mod < unary < power < postfix

    fn parse_expr(&mut self) -> Result<Expr, PythonError> {
        if self.peek() == &TokenKind::Lambda {
            return self.parse_lambda();
        }
        if self.peek() == &TokenKind::Yield {
            return self.parse_yield_expr();
        }
        self.parse_ternary()
    }

    fn parse_lambda(&mut self) -> Result<Expr, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'lambda'
        let mut params = Vec::new();
        while self.peek() != &TokenKind::Colon {
            if !params.is_empty() {
                self.expect(&TokenKind::Comma)?;
            }
            match self.peek().clone() {
                TokenKind::Ident(p) => {
                    self.advance();
                    params.push(p);
                }
                _ => {
                    return Err(PythonError::parse(
                        "expected parameter name in lambda",
                        self.peek_line(),
                    ));
                }
            }
        }
        self.expect(&TokenKind::Colon)?;
        let body = self.parse_expr()?;
        Ok(Expr::Lambda {
            params,
            body: Box::new(body),
            line,
        })
    }

    fn parse_yield_expr(&mut self) -> Result<Expr, PythonError> {
        let line = self.peek_line();
        self.advance(); // consume 'yield'
        let value = if matches!(
            self.peek(),
            TokenKind::Newline
                | TokenKind::Eof
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
                | TokenKind::Comma
                | TokenKind::Semicolon
        ) {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        Ok(Expr::Yield { value, line })
    }

    fn parse_ternary(&mut self) -> Result<Expr, PythonError> {
        let body = self.parse_or()?;
        if self.peek() == &TokenKind::If {
            let line = self.peek_line();
            self.advance(); // consume 'if'
            let test = self.parse_or()?;
            self.expect(&TokenKind::Else)?;
            let orelse = self.parse_expr()?;
            Ok(Expr::IfExpr {
                body: Box::new(body),
                test: Box::new(test),
                orelse: Box::new(orelse),
                line,
            })
        } else {
            Ok(body)
        }
    }

    fn parse_or(&mut self) -> Result<Expr, PythonError> {
        let mut left = self.parse_and()?;
        while self.peek() == &TokenKind::Or {
            let line = self.peek_line();
            self.advance();
            let right = self.parse_and()?;
            left = Expr::BoolOp {
                op: BoolOpKind::Or,
                left: Box::new(left),
                right: Box::new(right),
                line,
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, PythonError> {
        let mut left = self.parse_not()?;
        while self.peek() == &TokenKind::And {
            let line = self.peek_line();
            self.advance();
            let right = self.parse_not()?;
            left = Expr::BoolOp {
                op: BoolOpKind::And,
                left: Box::new(left),
                right: Box::new(right),
                line,
            };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, PythonError> {
        if self.peek() == &TokenKind::Not {
            let line = self.peek_line();
            self.advance();
            let operand = self.parse_not()?;
            Ok(Expr::UnaryOp {
                op: UnaryOp::Not,
                operand: Box::new(operand),
                line,
            })
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Expr, PythonError> {
        let left = self.parse_bitor()?;
        let line = self.peek_line();

        let mut ops = Vec::new();
        let mut comparators = Vec::new();

        loop {
            let op = match self.peek() {
                TokenKind::Eq => CmpOp::Eq,
                TokenKind::NotEq => CmpOp::NotEq,
                TokenKind::Lt => CmpOp::Lt,
                TokenKind::LtEq => CmpOp::LtEq,
                TokenKind::Gt => CmpOp::Gt,
                TokenKind::GtEq => CmpOp::GtEq,
                TokenKind::Is => {
                    self.advance();
                    if self.peek() == &TokenKind::Not {
                        self.advance();
                        ops.push(CmpOp::IsNot);
                        comparators.push(self.parse_bitor()?);
                        continue;
                    } else {
                        ops.push(CmpOp::Is);
                        comparators.push(self.parse_bitor()?);
                        continue;
                    }
                }
                TokenKind::In => {
                    self.advance();
                    ops.push(CmpOp::In);
                    comparators.push(self.parse_bitor()?);
                    continue;
                }
                TokenKind::Not => {
                    // 'not in'
                    if self.peek_nth(1) == &TokenKind::In {
                        self.advance(); // consume 'not'
                        self.advance(); // consume 'in'
                        ops.push(CmpOp::NotIn);
                        comparators.push(self.parse_bitor()?);
                        continue;
                    }
                    break;
                }
                _ => break,
            };
            self.advance();
            ops.push(op);
            comparators.push(self.parse_bitor()?);
        }

        if ops.is_empty() {
            Ok(left)
        } else {
            Ok(Expr::Compare {
                left: Box::new(left),
                ops,
                comparators,
                line,
            })
        }
    }

    fn parse_bitor(&mut self) -> Result<Expr, PythonError> {
        let mut left = self.parse_bitxor()?;
        while self.peek() == &TokenKind::Pipe {
            let line = self.peek_line();
            self.advance();
            let right = self.parse_bitxor()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op: BinOp::BitOr,
                right: Box::new(right),
                line,
            };
        }
        Ok(left)
    }

    fn parse_bitxor(&mut self) -> Result<Expr, PythonError> {
        let mut left = self.parse_bitand()?;
        while self.peek() == &TokenKind::Caret {
            let line = self.peek_line();
            self.advance();
            let right = self.parse_bitand()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op: BinOp::BitXor,
                right: Box::new(right),
                line,
            };
        }
        Ok(left)
    }

    fn parse_bitand(&mut self) -> Result<Expr, PythonError> {
        let mut left = self.parse_shift()?;
        while self.peek() == &TokenKind::Ampersand {
            let line = self.peek_line();
            self.advance();
            let right = self.parse_shift()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op: BinOp::BitAnd,
                right: Box::new(right),
                line,
            };
        }
        Ok(left)
    }

    fn parse_shift(&mut self) -> Result<Expr, PythonError> {
        let mut left = self.parse_addition()?;
        loop {
            let op = match self.peek() {
                TokenKind::LShift => BinOp::LShift,
                TokenKind::RShift => BinOp::RShift,
                _ => break,
            };
            let line = self.peek_line();
            self.advance();
            let right = self.parse_addition()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
                line,
            };
        }
        Ok(left)
    }

    fn parse_addition(&mut self) -> Result<Expr, PythonError> {
        let mut left = self.parse_multiplication()?;
        loop {
            let op = match self.peek() {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => break,
            };
            let line = self.peek_line();
            self.advance();
            let right = self.parse_multiplication()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
                line,
            };
        }
        Ok(left)
    }

    fn parse_multiplication(&mut self) -> Result<Expr, PythonError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::DoubleSlash => BinOp::FloorDiv,
                TokenKind::Percent => BinOp::Mod,
                _ => break,
            };
            let line = self.peek_line();
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
                line,
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, PythonError> {
        match self.peek() {
            TokenKind::Minus => {
                let line = self.peek_line();
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::UnaryOp {
                    op: UnaryOp::Neg,
                    operand: Box::new(operand),
                    line,
                })
            }
            TokenKind::Plus => {
                let line = self.peek_line();
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::UnaryOp {
                    op: UnaryOp::Pos,
                    operand: Box::new(operand),
                    line,
                })
            }
            TokenKind::Tilde => {
                let line = self.peek_line();
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::UnaryOp {
                    op: UnaryOp::Invert,
                    operand: Box::new(operand),
                    line,
                })
            }
            _ => self.parse_power(),
        }
    }

    fn parse_power(&mut self) -> Result<Expr, PythonError> {
        let base = self.parse_postfix()?;
        if self.peek() == &TokenKind::DoubleStar {
            let line = self.peek_line();
            self.advance();
            let exp = self.parse_unary()?; // right-associative
            Ok(Expr::BinOp {
                left: Box::new(base),
                op: BinOp::Pow,
                right: Box::new(exp),
                line,
            })
        } else {
            Ok(base)
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, PythonError> {
        let mut expr = self.parse_atom()?;

        loop {
            match self.peek() {
                TokenKind::LParen => {
                    let line = self.peek_line();
                    self.advance();
                    let mut args = Vec::new();
                    while self.peek() != &TokenKind::RParen {
                        if !args.is_empty() {
                            self.expect(&TokenKind::Comma)?;
                            if self.peek() == &TokenKind::RParen {
                                break;
                            }
                        }
                        args.push(self.parse_expr()?);
                    }
                    self.expect(&TokenKind::RParen)?;
                    expr = Expr::Call {
                        func: Box::new(expr),
                        args,
                        line,
                    };
                }
                TokenKind::LBracket => {
                    let line = self.peek_line();
                    self.advance();
                    let index = self.parse_subscript_index(line)?;
                    self.expect(&TokenKind::RBracket)?;
                    expr = Expr::Subscript {
                        value: Box::new(expr),
                        index: Box::new(index),
                        line,
                    };
                }
                TokenKind::Dot => {
                    let line = self.peek_line();
                    self.advance();
                    match self.peek().clone() {
                        TokenKind::Ident(attr) => {
                            self.advance();
                            expr = Expr::Attribute {
                                value: Box::new(expr),
                                attr,
                                line,
                            };
                        }
                        _ => {
                            return Err(PythonError::parse(
                                "expected attribute name after '.'",
                                self.peek_line(),
                            ));
                        }
                    }
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    /// Parse the `for ... in ... [if ...]*` chain of a comprehension. The
    /// initial element/key/value has already been consumed by the caller;
    /// this expects to start AT the first `for` keyword and to stop right
    /// before the closing bracket. Multi-`for` chains parse left to right
    /// — outermost first — matching Python semantics.
    fn parse_comprehension_clauses(
        &mut self,
    ) -> Result<Vec<crate::ast::ComprehensionClause>, PythonError> {
        let mut out = Vec::new();
        while self.peek() == &TokenKind::For {
            self.advance(); // 'for'
            // Use the same target parser the regular `for` stmt uses so
            // tuple-unpacking like `for x, y in pairs` works; it also
            // stops cleanly at `in` rather than consuming it as a
            // comparison operator (which `parse_or` would).
            let target = self.parse_assign_target_for()?;
            self.expect(&TokenKind::In)?;
            // Parse the iterable. Use parse_or to avoid swallowing later
            // `if` clauses — bare `if cond` after the iterable is the
            // filter, not a ternary on the iter.
            let iter = self.parse_or()?;
            let mut conditions = Vec::new();
            while self.peek() == &TokenKind::If {
                self.advance();
                conditions.push(self.parse_or()?);
            }
            out.push(crate::ast::ComprehensionClause {
                target,
                iter,
                conditions,
            });
        }
        Ok(out)
    }

    /// Parse what's between `[` and `]` in a subscript. Returns either a
    /// plain expression or an `Expr::Slice` if a `:` appears at the top
    /// level. Slice components are optional in all three positions:
    /// `a[:]`, `a[1:]`, `a[:5]`, `a[1:5:2]`, `a[::-1]`, etc.
    fn parse_subscript_index(&mut self, line: u32) -> Result<Expr, PythonError> {
        let is_sep = |t: &TokenKind| matches!(t, TokenKind::Colon | TokenKind::RBracket);

        let start = if is_sep(self.peek()) {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };

        if !matches!(self.peek(), TokenKind::Colon) {
            // No `:` → plain index. `start` is Some because the only way to
            // reach this branch with start=None is `[]`, which would have
            // peeked `]` as a separator AND failed the colon check — empty
            // subscripts aren't legal syntax and we reject them explicitly.
            let start_expr = start.ok_or_else(|| PythonError::parse("empty subscript", line))?;
            return Ok(*start_expr);
        }
        self.advance();

        let stop = if is_sep(self.peek()) {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };

        let step = if matches!(self.peek(), TokenKind::Colon) {
            self.advance();
            if matches!(self.peek(), TokenKind::RBracket) {
                None
            } else {
                Some(Box::new(self.parse_expr()?))
            }
        } else {
            None
        };

        Ok(Expr::Slice {
            start,
            stop,
            step,
            line,
        })
    }

    fn parse_atom(&mut self) -> Result<Expr, PythonError> {
        let line = self.peek_line();
        match self.peek().clone() {
            TokenKind::IntLit(v) => {
                self.advance();
                Ok(Expr::IntLit { value: v, line })
            }
            TokenKind::BigIntLit(b) => {
                self.advance();
                Ok(Expr::BigIntLit { value: b, line })
            }
            TokenKind::FloatLit(v) => {
                self.advance();
                Ok(Expr::FloatLit { value: v, line })
            }
            TokenKind::StringLit(s) => {
                self.advance();
                // Handle implicit string concatenation
                let mut result = s;
                while let TokenKind::StringLit(s2) = self.peek().clone() {
                    self.advance();
                    result.push_str(&s2);
                }
                Ok(Expr::StringLit {
                    value: result,
                    line,
                })
            }
            TokenKind::True => {
                self.advance();
                Ok(Expr::BoolLit { value: true, line })
            }
            TokenKind::False => {
                self.advance();
                Ok(Expr::BoolLit { value: false, line })
            }
            TokenKind::None => {
                self.advance();
                Ok(Expr::NoneLit { line })
            }
            TokenKind::Ident(name) => {
                self.advance();
                Ok(Expr::Name { id: name, line })
            }
            TokenKind::LParen => {
                self.advance();
                // Empty tuple
                if self.peek() == &TokenKind::RParen {
                    self.advance();
                    return Ok(Expr::Tuple {
                        elements: Vec::new(),
                        line,
                    });
                }
                let first = self.parse_expr()?;
                // Check for tuple: (a,) or (a, b, ...)
                if self.peek() == &TokenKind::Comma {
                    let mut elements = vec![first];
                    while self.peek() == &TokenKind::Comma {
                        self.advance();
                        if self.peek() == &TokenKind::RParen {
                            break;
                        }
                        elements.push(self.parse_expr()?);
                    }
                    self.expect(&TokenKind::RParen)?;
                    return Ok(Expr::Tuple { elements, line });
                }
                self.expect(&TokenKind::RParen)?;
                Ok(first)
            }
            TokenKind::LBracket => {
                self.advance();
                if self.peek() == &TokenKind::RBracket {
                    self.advance();
                    return Ok(Expr::List {
                        elements: Vec::new(),
                        line,
                    });
                }
                // Parse first element; if `for` follows, it's a list
                // comprehension. Otherwise a normal list literal.
                let first = self.parse_expr()?;
                if self.peek() == &TokenKind::For {
                    let clauses = self.parse_comprehension_clauses()?;
                    self.expect(&TokenKind::RBracket)?;
                    return Ok(Expr::ListComp {
                        elt: Box::new(first),
                        clauses,
                        line,
                    });
                }
                let mut elements = vec![first];
                while self.peek() == &TokenKind::Comma {
                    self.advance();
                    if self.peek() == &TokenKind::RBracket {
                        break;
                    }
                    elements.push(self.parse_expr()?);
                }
                self.expect(&TokenKind::RBracket)?;
                Ok(Expr::List { elements, line })
            }
            TokenKind::LBrace => {
                self.advance();
                // Empty dict
                if self.peek() == &TokenKind::RBrace {
                    self.advance();
                    return Ok(Expr::Dict {
                        keys: Vec::new(),
                        values: Vec::new(),
                        line,
                    });
                }
                // Parse first element to determine dict-vs-set, and for
                // each, comprehension-vs-literal.
                let first = self.parse_expr()?;
                if self.peek() == &TokenKind::Colon {
                    // Dict — peek past `:` to see if it's a comp or literal.
                    self.advance();
                    let first_val = self.parse_expr()?;
                    if self.peek() == &TokenKind::For {
                        let clauses = self.parse_comprehension_clauses()?;
                        self.expect(&TokenKind::RBrace)?;
                        return Ok(Expr::DictComp {
                            key: Box::new(first),
                            value: Box::new(first_val),
                            clauses,
                            line,
                        });
                    }
                    let mut keys = vec![first];
                    let mut values = vec![first_val];
                    while self.peek() == &TokenKind::Comma {
                        self.advance();
                        if self.peek() == &TokenKind::RBrace {
                            break;
                        }
                        keys.push(self.parse_expr()?);
                        self.expect(&TokenKind::Colon)?;
                        values.push(self.parse_expr()?);
                    }
                    self.expect(&TokenKind::RBrace)?;
                    Ok(Expr::Dict { keys, values, line })
                } else if self.peek() == &TokenKind::For {
                    // Set comprehension.
                    let clauses = self.parse_comprehension_clauses()?;
                    self.expect(&TokenKind::RBrace)?;
                    Ok(Expr::SetComp {
                        elt: Box::new(first),
                        clauses,
                        line,
                    })
                } else {
                    // Set literal
                    let mut elements = vec![first];
                    while self.peek() == &TokenKind::Comma {
                        self.advance();
                        if self.peek() == &TokenKind::RBrace {
                            break;
                        }
                        elements.push(self.parse_expr()?);
                    }
                    self.expect(&TokenKind::RBrace)?;
                    Ok(Expr::Set { elements, line })
                }
            }
            TokenKind::Star => {
                self.advance();
                let value = self.parse_expr()?;
                Ok(Expr::Starred {
                    value: Box::new(value),
                    line,
                })
            }
            _ => Err(PythonError::parse(
                format!("unexpected token {:?}", self.peek()),
                line,
            )),
        }
    }
}

/// Convert an expression to an assignment target.
fn expr_to_target(expr: Expr, line: u32) -> Result<AssignTarget, PythonError> {
    match expr {
        Expr::Name { id, .. } => Ok(AssignTarget::Name(id)),
        Expr::Attribute { value, attr, .. } => Ok(AssignTarget::Attribute { value, attr }),
        Expr::Subscript { value, index, .. } => Ok(AssignTarget::Subscript { value, index }),
        Expr::Tuple { elements, .. } => {
            let targets: Result<Vec<_>, _> = elements
                .into_iter()
                .map(|e| expr_to_target(e, line))
                .collect();
            Ok(AssignTarget::Tuple(targets?))
        }
        _ => Err(PythonError::parse("invalid assignment target", line)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse_str(src: &str) -> Module {
        let tokens = tokenize(src).unwrap();
        parse(tokens).unwrap()
    }

    #[test]
    fn parse_assignment() {
        let m = parse_str("x = 10\n");
        assert_eq!(m.body.len(), 1);
        matches!(&m.body[0], Stmt::Assign { target: AssignTarget::Name(n), .. } if n == "x");
    }

    #[test]
    fn parse_if_elif_else() {
        let m = parse_str("if x:\n    pass\nelif y:\n    pass\nelse:\n    pass\n");
        assert_eq!(m.body.len(), 1);
        if let Stmt::If {
            elif_clauses,
            else_body,
            ..
        } = &m.body[0]
        {
            assert_eq!(elif_clauses.len(), 1);
            assert_eq!(else_body.len(), 1);
        } else {
            panic!("expected If");
        }
    }

    #[test]
    fn parse_for_loop() {
        let m = parse_str("for i in range(10):\n    print(i)\n");
        assert_eq!(m.body.len(), 1);
        matches!(&m.body[0], Stmt::For { target: AssignTarget::Name(n), .. } if n == "i");
    }

    #[test]
    fn parse_function_def() {
        let m = parse_str("def foo(a, b):\n    return a + b\n");
        assert_eq!(m.body.len(), 1);
        if let Stmt::FunctionDef {
            name, params, body, ..
        } = &m.body[0]
        {
            assert_eq!(name, "foo");
            assert_eq!(params, &["a", "b"]);
            assert_eq!(body.len(), 1);
        } else {
            panic!("expected FunctionDef");
        }
    }

    #[test]
    fn parse_class_def() {
        let m = parse_str("class Foo(Bar):\n    pass\n");
        if let Stmt::ClassDef { name, bases, .. } = &m.body[0] {
            assert_eq!(name, "Foo");
            assert_eq!(bases.len(), 1);
        } else {
            panic!("expected ClassDef");
        }
    }

    #[test]
    fn parse_try_except() {
        let m = parse_str("try:\n    pass\nexcept ValueError as e:\n    pass\n");
        if let Stmt::Try { handlers, .. } = &m.body[0] {
            assert_eq!(handlers.len(), 1);
            assert_eq!(handlers[0].name, Some("e".into()));
        } else {
            panic!("expected Try");
        }
    }

    #[test]
    fn parse_dict_literal() {
        let m = parse_str("x = {'a': 1, 'b': 2}\n");
        if let Stmt::Assign {
            value: Expr::Dict { keys, values, .. },
            ..
        } = &m.body[0]
        {
            assert_eq!(keys.len(), 2);
            assert_eq!(values.len(), 2);
        } else {
            panic!("expected Dict");
        }
    }

    #[test]
    fn parse_attribute() {
        let m = parse_str("x = foo.bar\n");
        if let Stmt::Assign {
            value: Expr::Attribute { attr, .. },
            ..
        } = &m.body[0]
        {
            assert_eq!(attr, "bar");
        } else {
            panic!("expected Attribute");
        }
    }

    #[test]
    fn parse_tuple_unpacking() {
        let m = parse_str("a, b = 1, 2\n");
        if let Stmt::Assign {
            target: AssignTarget::Tuple(targets),
            ..
        } = &m.body[0]
        {
            assert_eq!(targets.len(), 2);
        } else {
            panic!("expected tuple assignment");
        }
    }

    #[test]
    fn parse_lambda() {
        let m = parse_str("f = lambda x: x + 1\n");
        if let Stmt::Assign {
            value: Expr::Lambda { params, .. },
            ..
        } = &m.body[0]
        {
            assert_eq!(params, &["x"]);
        } else {
            panic!("expected Lambda");
        }
    }

    #[test]
    fn parse_ternary_expr() {
        let m = parse_str("x = a if b else c\n");
        if let Stmt::Assign {
            value: Expr::IfExpr { .. },
            ..
        } = &m.body[0]
        {
            // ok
        } else {
            panic!("expected IfExpr");
        }
    }

    // ---------- M3 commit 3: import statement parsing ----------

    #[test]
    fn parse_import_single_name() {
        let m = parse_str("import foo\n");
        match &m.body[0] {
            Stmt::Import { names, .. } => {
                assert_eq!(names.len(), 1);
                assert_eq!(names[0].name, "foo");
                assert_eq!(names[0].asname, None);
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn parse_import_dotted_name() {
        let m = parse_str("import foo.bar.baz\n");
        match &m.body[0] {
            Stmt::Import { names, .. } => assert_eq!(names[0].name, "foo.bar.baz"),
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn parse_import_with_alias() {
        let m = parse_str("import foo.bar as fb\n");
        match &m.body[0] {
            Stmt::Import { names, .. } => {
                assert_eq!(names[0].name, "foo.bar");
                assert_eq!(names[0].asname.as_deref(), Some("fb"));
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn parse_import_multiple() {
        let m = parse_str("import a, b.c, d as e\n");
        match &m.body[0] {
            Stmt::Import { names, .. } => {
                assert_eq!(names.len(), 3);
                assert_eq!(names[0].name, "a");
                assert_eq!(names[1].name, "b.c");
                assert_eq!(names[2].name, "d");
                assert_eq!(names[2].asname.as_deref(), Some("e"));
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn parse_from_import_single() {
        let m = parse_str("from foo import bar\n");
        match &m.body[0] {
            Stmt::ImportFrom {
                module,
                names,
                level,
                is_star,
                ..
            } => {
                assert_eq!(module.as_deref(), Some("foo"));
                assert_eq!(*level, 0);
                assert!(!is_star);
                assert_eq!(names[0].name, "bar");
            }
            other => panic!("expected ImportFrom, got {other:?}"),
        }
    }

    #[test]
    fn parse_from_import_multiple_with_alias() {
        let m = parse_str("from foo.bar import baz, qux as q\n");
        match &m.body[0] {
            Stmt::ImportFrom { module, names, .. } => {
                assert_eq!(module.as_deref(), Some("foo.bar"));
                assert_eq!(names.len(), 2);
                assert_eq!(names[1].asname.as_deref(), Some("q"));
            }
            other => panic!("expected ImportFrom, got {other:?}"),
        }
    }

    #[test]
    fn parse_from_import_star() {
        let m = parse_str("from foo import *\n");
        match &m.body[0] {
            Stmt::ImportFrom {
                module,
                names,
                is_star,
                ..
            } => {
                assert_eq!(module.as_deref(), Some("foo"));
                assert!(*is_star);
                assert!(names.is_empty());
            }
            other => panic!("expected ImportFrom, got {other:?}"),
        }
    }

    #[test]
    fn parse_from_import_relative_single_dot() {
        let m = parse_str("from . import x\n");
        match &m.body[0] {
            Stmt::ImportFrom { module, level, .. } => {
                assert_eq!(*level, 1);
                assert_eq!(module.as_deref(), None);
            }
            other => panic!("expected ImportFrom, got {other:?}"),
        }
    }

    #[test]
    fn parse_from_import_relative_double_dot_with_module() {
        let m = parse_str("from ..pkg import y\n");
        match &m.body[0] {
            Stmt::ImportFrom { module, level, .. } => {
                assert_eq!(*level, 2);
                assert_eq!(module.as_deref(), Some("pkg"));
            }
            other => panic!("expected ImportFrom, got {other:?}"),
        }
    }

    #[test]
    fn parse_from_import_parenthesized() {
        let m = parse_str("from foo import (a, b, c,)\n");
        match &m.body[0] {
            Stmt::ImportFrom { names, .. } => assert_eq!(names.len(), 3),
            other => panic!("expected ImportFrom, got {other:?}"),
        }
    }
}
