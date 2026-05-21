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
    /// `import foo, bar.baz as bb, qux`
    Import {
        names: Vec<ImportAlias>,
        line: u32,
    },
    /// `from foo.bar import baz, qux as q`, `from . import x`, `from foo import *`
    ImportFrom {
        /// Module path after the leading dots. None for `from . import x`.
        module: Option<String>,
        /// Imported names. Empty when `is_star` is true.
        names: Vec<ImportAlias>,
        /// Leading-dot count: 0 absolute, 1 single `.`, 2 `..`, etc.
        level: u32,
        /// True for `from foo import *`.
        is_star: bool,
        line: u32,
    },
}

/// `foo as bar` in an import list. `asname = None` for plain `import foo`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportAlias {
    /// Dotted name for `Stmt::Import` (e.g. "foo.bar"); simple name for
    /// `Stmt::ImportFrom` (e.g. "baz").
    pub name: String,
    pub asname: Option<String>,
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Expr {
    IntLit {
        value: i64,
        line: u32,
    },
    /// Integer literal that overflowed i64 at lex time. Boxed to keep
    /// the Expr enum compact; only constructed when actually needed.
    BigIntLit {
        value: Box<num_bigint::BigInt>,
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
    /// `start:stop:step` inside a `[]` subscript. All three fields are
    /// optional (omitted means the default — 0/len/1 depending on side).
    /// Slices only appear as the immediate `index` of an `Expr::Subscript`.
    Slice {
        start: Option<Box<Expr>>,
        stop: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
        line: u32,
    },
    /// `[elt for x in iter if cond ...]`
    ListComp {
        elt: Box<Expr>,
        clauses: Vec<ComprehensionClause>,
        line: u32,
    },
    /// `{elt for x in iter if cond ...}`
    SetComp {
        elt: Box<Expr>,
        clauses: Vec<ComprehensionClause>,
        line: u32,
    },
    /// `{k: v for x in iter if cond ...}`
    DictComp {
        key: Box<Expr>,
        value: Box<Expr>,
        clauses: Vec<ComprehensionClause>,
        line: u32,
    },
}

/// One `for target in iter [if cond]*` clause of a comprehension.
#[derive(Debug, Clone, PartialEq)]
pub struct ComprehensionClause {
    pub target: AssignTarget,
    pub iter: Expr,
    pub conditions: Vec<Expr>,
}

impl Expr {
    /// Get the line number of this expression.
    #[allow(dead_code)]
    pub fn line(&self) -> u32 {
        match self {
            Self::IntLit { line, .. }
            | Self::BigIntLit { line, .. }
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
            | Self::Starred { line, .. }
            | Self::Slice { line, .. }
            | Self::ListComp { line, .. }
            | Self::SetComp { line, .. }
            | Self::DictComp { line, .. } => *line,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expr_line_returns_stored_line() {
        let cases: Vec<(Expr, u32)> = vec![
            (Expr::IntLit { value: 1, line: 5 }, 5),
            (
                Expr::FloatLit {
                    value: 1.0,
                    line: 7,
                },
                7,
            ),
            (
                Expr::StringLit {
                    value: "x".into(),
                    line: 9,
                },
                9,
            ),
            (
                Expr::BoolLit {
                    value: true,
                    line: 11,
                },
                11,
            ),
            (Expr::NoneLit { line: 13 }, 13),
            (
                Expr::Name {
                    id: "x".into(),
                    line: 15,
                },
                15,
            ),
            (
                Expr::Starred {
                    value: Box::new(Expr::Name {
                        id: "x".into(),
                        line: 17,
                    }),
                    line: 17,
                },
                17,
            ),
        ];
        for (expr, expected) in cases {
            assert_eq!(expr.line(), expected, "wrong line for {expr:?}");
        }
    }

    #[test]
    fn binop_variants_constructible() {
        // Smoke test for the operator enums — proves Debug + Clone + PartialEq impls exist.
        let ops = [BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::Div];
        for o in ops {
            let cloned = o;
            assert_eq!(o, cloned);
            let _ = format!("{o:?}");
        }
    }

    #[test]
    fn cmp_and_boolop_variants_constructible() {
        let cmps = [CmpOp::Eq, CmpOp::Lt, CmpOp::Gt, CmpOp::In, CmpOp::NotIn];
        for c in cmps {
            let _ = format!("{c:?}");
            assert_eq!(c, c);
        }
        let bools = [BoolOpKind::And, BoolOpKind::Or];
        for b in bools {
            let _ = format!("{b:?}");
            assert_eq!(b, b);
        }
    }

    #[test]
    fn module_struct_constructible() {
        let m = Module { body: vec![] };
        let cloned = m.clone();
        assert_eq!(m, cloned);
        let _ = format!("{m:?}");
    }

    #[test]
    fn assign_target_variants() {
        let targets = vec![
            AssignTarget::Name("x".into()),
            AssignTarget::Tuple(vec![
                AssignTarget::Name("a".into()),
                AssignTarget::Name("b".into()),
            ]),
        ];
        for t in &targets {
            let _ = format!("{t:?}");
            assert_eq!(*t, t.clone());
        }
    }

    #[test]
    fn import_alias_constructible() {
        let a = ImportAlias {
            name: "foo.bar".into(),
            asname: Some("fb".into()),
        };
        assert_eq!(a, a.clone());
        let _ = format!("{a:?}");
    }
}
