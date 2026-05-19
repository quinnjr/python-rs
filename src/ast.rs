//! AST node types for the Python subset.

/// A parsed module (top-level).
#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub body: Vec<Stmt>,
}

/// Assignment target — supports name, attribute, subscript, tuple unpacking.
#[derive(Debug, Clone, PartialEq)]
pub enum AssignTarget {
    Name(String),
    Attribute { value: Box<Expr>, attr: String },
    Subscript { value: Box<Expr>, index: Box<Expr> },
    Tuple(Vec<AssignTarget>),
}

/// An except handler clause.
#[derive(Debug, Clone, PartialEq)]
pub struct ExceptHandler {
    pub exc_type: Option<Expr>,
    pub name: Option<String>,
    pub body: Vec<Stmt>,
    pub line: u32,
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Stmt {
    Assign {
        target: AssignTarget,
        value: Expr,
        line: u32,
    },
    AugAssign {
        target: AssignTarget,
        op: BinOp,
        value: Expr,
        line: u32,
    },
    ExprStmt {
        expr: Expr,
        line: u32,
    },
    If {
        condition: Expr,
        body: Vec<Stmt>,
        elif_clauses: Vec<(Expr, Vec<Stmt>)>,
        else_body: Vec<Stmt>,
        line: u32,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
        line: u32,
    },
    For {
        target: AssignTarget,
        iter: Expr,
        body: Vec<Stmt>,
        line: u32,
    },
    FunctionDef {
        name: String,
        params: Vec<String>,
        body: Vec<Stmt>,
        decorators: Vec<Expr>,
        line: u32,
    },
    Return {
        value: Option<Expr>,
        line: u32,
    },
    Pass {
        line: u32,
    },
    Break {
        line: u32,
    },
    Continue {
        line: u32,
    },
    ClassDef {
        name: String,
        bases: Vec<Expr>,
        body: Vec<Stmt>,
        decorators: Vec<Expr>,
        line: u32,
    },
    Try {
        body: Vec<Stmt>,
        handlers: Vec<ExceptHandler>,
        else_body: Vec<Stmt>,
        finally_body: Vec<Stmt>,
        line: u32,
    },
    Raise {
        exc: Option<Expr>,
        line: u32,
    },
    Assert {
        test: Expr,
        msg: Option<Expr>,
        line: u32,
    },
    Delete {
        target: AssignTarget,
        line: u32,
    },
    GlobalDecl {
        names: Vec<String>,
        line: u32,
    },
    NonlocalDecl {
        names: Vec<String>,
        line: u32,
    },
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Expr {
    IntLit {
        value: i64,
        line: u32,
    },
    FloatLit {
        value: f64,
        line: u32,
    },
    StringLit {
        value: String,
        line: u32,
    },
    BoolLit {
        value: bool,
        line: u32,
    },
    NoneLit {
        line: u32,
    },
    Name {
        id: String,
        line: u32,
    },
    BinOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
        line: u32,
    },
    UnaryOp {
        op: UnaryOp,
        operand: Box<Expr>,
        line: u32,
    },
    Compare {
        left: Box<Expr>,
        ops: Vec<CmpOp>,
        comparators: Vec<Expr>,
        line: u32,
    },
    BoolOp {
        op: BoolOpKind,
        left: Box<Expr>,
        right: Box<Expr>,
        line: u32,
    },
    Call {
        func: Box<Expr>,
        args: Vec<Expr>,
        line: u32,
    },
    Subscript {
        value: Box<Expr>,
        index: Box<Expr>,
        line: u32,
    },
    List {
        elements: Vec<Expr>,
        line: u32,
    },
    Attribute {
        value: Box<Expr>,
        attr: String,
        line: u32,
    },
    Tuple {
        elements: Vec<Expr>,
        line: u32,
    },
    Dict {
        keys: Vec<Expr>,
        values: Vec<Expr>,
        line: u32,
    },
    Set {
        elements: Vec<Expr>,
        line: u32,
    },
    Lambda {
        params: Vec<String>,
        body: Box<Expr>,
        line: u32,
    },
    IfExpr {
        body: Box<Expr>,
        test: Box<Expr>,
        orelse: Box<Expr>,
        line: u32,
    },
    Yield {
        value: Option<Box<Expr>>,
        line: u32,
    },
    Starred {
        value: Box<Expr>,
        line: u32,
    },
}

impl Expr {
    /// Get the line number of this expression.
    #[allow(dead_code)]
    pub fn line(&self) -> u32 {
        match self {
            Self::IntLit { line, .. }
            | Self::FloatLit { line, .. }
            | Self::StringLit { line, .. }
            | Self::BoolLit { line, .. }
            | Self::NoneLit { line }
            | Self::Name { line, .. }
            | Self::BinOp { line, .. }
            | Self::UnaryOp { line, .. }
            | Self::Compare { line, .. }
            | Self::BoolOp { line, .. }
            | Self::Call { line, .. }
            | Self::Subscript { line, .. }
            | Self::List { line, .. }
            | Self::Attribute { line, .. }
            | Self::Tuple { line, .. }
            | Self::Dict { line, .. }
            | Self::Set { line, .. }
            | Self::Lambda { line, .. }
            | Self::IfExpr { line, .. }
            | Self::Yield { line, .. }
            | Self::Starred { line, .. } => *line,
        }
    }
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    LShift,
    RShift,
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp {
    Neg,
    Not,
    Invert,
    Pos,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Is,
    IsNot,
    In,
    NotIn,
}

/// Boolean operators (short-circuit).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BoolOpKind {
    And,
    Or,
}
