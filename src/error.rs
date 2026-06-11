/// Error types for the Python interpreter.
use std::fmt;

/// Top-level error type for all interpreter phases.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum PythonError {
    LexError { msg: String, line: u32 },
    ParseError { msg: String, line: u32 },
    CompileError { msg: String, line: u32 },
    RuntimeError { msg: String, line: u32 },
}

impl PythonError {
    /// Create a lex error.
    pub fn lex(msg: impl Into<String>, line: u32) -> Self {
        Self::LexError {
            msg: msg.into(),
            line,
        }
    }

    /// Create a parse error.
    pub fn parse(msg: impl Into<String>, line: u32) -> Self {
        Self::ParseError {
            msg: msg.into(),
            line,
        }
    }

    /// Create a compile error.
    pub fn compile(msg: impl Into<String>, line: u32) -> Self {
        Self::CompileError {
            msg: msg.into(),
            line,
        }
    }

    /// Create a runtime error.
    pub fn runtime(msg: impl Into<String>, line: u32) -> Self {
        Self::RuntimeError {
            msg: msg.into(),
            line,
        }
    }

    /// Get the line number of the error.
    pub fn line(&self) -> u32 {
        match self {
            Self::LexError { line, .. }
            | Self::ParseError { line, .. }
            | Self::CompileError { line, .. }
            | Self::RuntimeError { line, .. } => *line,
        }
    }
}

impl fmt::Display for PythonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LexError { msg, line } => write!(f, "LexError at line {line}: {msg}"),
            Self::ParseError { msg, line } => write!(f, "ParseError at line {line}: {msg}"),
            Self::CompileError { msg, line } => write!(f, "CompileError at line {line}: {msg}"),
            Self::RuntimeError { msg, line } => write!(f, "RuntimeError at line {line}: {msg}"),
        }
    }
}

impl std::error::Error for PythonError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let e = PythonError::lex("unexpected character", 5);
        assert_eq!(e.to_string(), "LexError at line 5: unexpected character");
        assert_eq!(e.line(), 5);
    }

    #[test]
    fn error_variants() {
        assert_eq!(PythonError::parse("bad syntax", 1).line(), 1);
        assert_eq!(PythonError::compile("unknown var", 10).line(), 10);
        assert_eq!(PythonError::runtime("division by zero", 3).line(), 3);
    }
}
