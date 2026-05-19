/// Python tokenizer with INDENT/DEDENT tracking.
use crate::error::PythonError;

/// Token kind.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Literals
    IntLit(i64),
    FloatLit(f64),
    StringLit(String),

    // Identifier
    Ident(String),

    // Keywords
    If,
    Elif,
    Else,
    For,
    In,
    While,
    Def,
    Return,
    And,
    Or,
    Not,
    True,
    False,
    None,
    Pass,
    Break,
    Continue,
    Class,
    Try,
    Except,
    Finally,
    Raise,
    As,
    With,
    Assert,
    Del,
    Global,
    Nonlocal,
    Lambda,
    Yield,
    Is,
    From,

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    DoubleSlash,
    Percent,
    DoubleStar,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    Assign,
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    DoubleSlashAssign,
    PercentAssign,
    DoubleStarAssign,
    Ampersand,
    Pipe,
    Caret,
    Tilde,
    LShift,
    RShift,
    AmpersandAssign,
    PipeAssign,
    CaretAssign,
    LShiftAssign,
    RShiftAssign,
    At,

    // Delimiters
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Dot,
    Semicolon,
    Ellipsis,
    Arrow,

    // Structure
    Newline,
    Indent,
    Dedent,
    Eof,
}

/// A token with position info.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub line: u32,
    pub col: u32,
}

/// Tokenize Python source code.
pub fn tokenize(source: &str) -> Result<Vec<Token>, PythonError> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = source.chars().collect();
    let len = chars.len();
    let mut pos = 0;
    let mut line: u32 = 1;
    let mut col: u32 = 1;
    let mut indent_stack: Vec<usize> = vec![0];
    let mut paren_depth: u32 = 0;
    let mut at_line_start = true;

    while pos < len {
        // Handle beginning of line: compute indentation
        if at_line_start && paren_depth == 0 {
            let mut indent = 0;
            while pos < len && chars[pos] == ' ' {
                indent += 1;
                pos += 1;
                col += 1;
            }
            // Skip blank lines and comment-only lines
            if pos < len && (chars[pos] == '\n' || chars[pos] == '#') {
                if chars[pos] == '#' {
                    while pos < len && chars[pos] != '\n' {
                        pos += 1;
                    }
                }
                if pos < len && chars[pos] == '\n' {
                    pos += 1;
                    line += 1;
                    col = 1;
                }
                continue;
            }
            if pos >= len {
                break;
            }

            let current_indent = *indent_stack.last().unwrap();
            if indent > current_indent {
                indent_stack.push(indent);
                tokens.push(Token { kind: TokenKind::Indent, line, col: 1 });
            } else {
                while indent < *indent_stack.last().unwrap() {
                    indent_stack.pop();
                    tokens.push(Token { kind: TokenKind::Dedent, line, col: 1 });
                }
                if indent != *indent_stack.last().unwrap() {
                    return Err(PythonError::lex("inconsistent indentation", line));
                }
            }
            at_line_start = false;
        }

        // Skip spaces (not at line start)
        if pos < len && chars[pos] == ' ' {
            pos += 1;
            col += 1;
            continue;
        }

        // Tab handling (treat as space)
        if pos < len && chars[pos] == '\t' {
            pos += 1;
            col += 1;
            continue;
        }

        // Carriage return
        if pos < len && chars[pos] == '\r' {
            pos += 1;
            continue;
        }

        // Comments
        if pos < len && chars[pos] == '#' {
            while pos < len && chars[pos] != '\n' {
                pos += 1;
            }
            continue;
        }

        // Newline
        if pos < len && chars[pos] == '\n' {
            if paren_depth == 0 {
                // Only emit newline if the last token wasn't already a newline
                if let Some(last) = tokens.last() && last.kind != TokenKind::Newline {
                    tokens.push(Token { kind: TokenKind::Newline, line, col });
                }
            }
            pos += 1;
            line += 1;
            col = 1;
            at_line_start = true;
            continue;
        }

        if pos >= len {
            break;
        }

        let start_col = col;
        let ch = chars[pos];

        // String literals
        if ch == '\'' || ch == '"' {
            let quote = ch;
            pos += 1;
            col += 1;
            let mut s = String::new();
            while pos < len && chars[pos] != quote {
                if chars[pos] == '\\' && pos + 1 < len {
                    pos += 1;
                    col += 1;
                    match chars[pos] {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        '\\' => s.push('\\'),
                        '\'' => s.push('\''),
                        '"' => s.push('"'),
                        _ => {
                            s.push('\\');
                            s.push(chars[pos]);
                        }
                    }
                } else if chars[pos] == '\n' {
                    return Err(PythonError::lex("unterminated string literal", line));
                } else {
                    s.push(chars[pos]);
                }
                pos += 1;
                col += 1;
            }
            if pos >= len {
                return Err(PythonError::lex("unterminated string literal", line));
            }
            pos += 1; // skip closing quote
            col += 1;
            tokens.push(Token { kind: TokenKind::StringLit(s), line, col: start_col });
            continue;
        }

        // Numbers
        if ch.is_ascii_digit() {
            let start = pos;
            let mut is_float = false;
            while pos < len && chars[pos].is_ascii_digit() {
                pos += 1;
                col += 1;
            }
            if pos < len && chars[pos] == '.' && (pos + 1 >= len || chars[pos + 1] != '.') {
                is_float = true;
                pos += 1;
                col += 1;
                while pos < len && chars[pos].is_ascii_digit() {
                    pos += 1;
                    col += 1;
                }
            }
            let text: String = chars[start..pos].iter().collect();
            if is_float {
                let f: f64 = text.parse().map_err(|_| PythonError::lex("invalid float", line))?;
                tokens.push(Token { kind: TokenKind::FloatLit(f), line, col: start_col });
            } else {
                let i: i64 = text.parse().map_err(|_| PythonError::lex("invalid integer", line))?;
                tokens.push(Token { kind: TokenKind::IntLit(i), line, col: start_col });
            }
            continue;
        }

        // Identifiers and keywords
        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = pos;
            while pos < len && (chars[pos].is_ascii_alphanumeric() || chars[pos] == '_') {
                pos += 1;
                col += 1;
            }
            let word: String = chars[start..pos].iter().collect();
            let kind = match word.as_str() {
                "if" => TokenKind::If,
                "elif" => TokenKind::Elif,
                "else" => TokenKind::Else,
                "for" => TokenKind::For,
                "in" => TokenKind::In,
                "while" => TokenKind::While,
                "def" => TokenKind::Def,
                "return" => TokenKind::Return,
                "and" => TokenKind::And,
                "or" => TokenKind::Or,
                "not" => TokenKind::Not,
                "True" => TokenKind::True,
                "False" => TokenKind::False,
                "None" => TokenKind::None,
                "pass" => TokenKind::Pass,
                "break" => TokenKind::Break,
                "continue" => TokenKind::Continue,
                "class" => TokenKind::Class,
                "try" => TokenKind::Try,
                "except" => TokenKind::Except,
                "finally" => TokenKind::Finally,
                "raise" => TokenKind::Raise,
                "as" => TokenKind::As,
                "with" => TokenKind::With,
                "assert" => TokenKind::Assert,
                "del" => TokenKind::Del,
                "global" => TokenKind::Global,
                "nonlocal" => TokenKind::Nonlocal,
                "lambda" => TokenKind::Lambda,
                "yield" => TokenKind::Yield,
                "is" => TokenKind::Is,
                "from" => TokenKind::From,
                _ => TokenKind::Ident(word),
            };
            tokens.push(Token { kind, line, col: start_col });
            continue;
        }

        // Three-character tokens
        if pos + 2 < len {
            let three: String = chars[pos..pos + 3].iter().collect();
            let kind3 = match three.as_str() {
                "..." => Some(TokenKind::Ellipsis),
                "<<=" => Some(TokenKind::LShiftAssign),
                ">>=" => Some(TokenKind::RShiftAssign),
                "**=" => {
                    pos += 3;
                    col += 3;
                    tokens.push(Token { kind: TokenKind::DoubleStarAssign, line, col: start_col });
                    continue;
                }
                "//=" => {
                    pos += 3;
                    col += 3;
                    tokens.push(Token { kind: TokenKind::DoubleSlashAssign, line, col: start_col });
                    continue;
                }
                _ => Option::None,
            };
            if let Some(k) = kind3 {
                pos += 3;
                col += 3;
                tokens.push(Token { kind: k, line, col: start_col });
                continue;
            }
        }

        // Two-character operators
        if pos + 1 < len {
            let two: String = chars[pos..pos + 2].iter().collect();
            let kind = match two.as_str() {
                "==" => Some(TokenKind::Eq),
                "!=" => Some(TokenKind::NotEq),
                "<=" => Some(TokenKind::LtEq),
                ">=" => Some(TokenKind::GtEq),
                "//" => Some(TokenKind::DoubleSlash),
                "**" => Some(TokenKind::DoubleStar),
                "+=" => Some(TokenKind::PlusAssign),
                "-=" => Some(TokenKind::MinusAssign),
                "*=" => Some(TokenKind::StarAssign),
                "/=" => Some(TokenKind::SlashAssign),
                "%=" => Some(TokenKind::PercentAssign),
                "<<" => Some(TokenKind::LShift),
                ">>" => Some(TokenKind::RShift),
                "&=" => Some(TokenKind::AmpersandAssign),
                "|=" => Some(TokenKind::PipeAssign),
                "^=" => Some(TokenKind::CaretAssign),
                "->" => Some(TokenKind::Arrow),
                _ => Option::None,
            };
            if let Some(k) = kind {
                pos += 2;
                col += 2;
                tokens.push(Token { kind: k, line, col: start_col });
                continue;
            }
        }

        // Single-character operators and delimiters
        let kind = match ch {
            '+' => Some(TokenKind::Plus),
            '-' => Some(TokenKind::Minus),
            '*' => Some(TokenKind::Star),
            '/' => Some(TokenKind::Slash),
            '%' => Some(TokenKind::Percent),
            '<' => Some(TokenKind::Lt),
            '>' => Some(TokenKind::Gt),
            '=' => Some(TokenKind::Assign),
            '&' => Some(TokenKind::Ampersand),
            '|' => Some(TokenKind::Pipe),
            '^' => Some(TokenKind::Caret),
            '~' => Some(TokenKind::Tilde),
            '@' => Some(TokenKind::At),
            '(' => {
                paren_depth += 1;
                Some(TokenKind::LParen)
            }
            ')' => {
                paren_depth = paren_depth.saturating_sub(1);
                Some(TokenKind::RParen)
            }
            '[' => {
                paren_depth += 1;
                Some(TokenKind::LBracket)
            }
            ']' => {
                paren_depth = paren_depth.saturating_sub(1);
                Some(TokenKind::RBracket)
            }
            '{' => {
                paren_depth += 1;
                Some(TokenKind::LBrace)
            }
            '}' => {
                paren_depth = paren_depth.saturating_sub(1);
                Some(TokenKind::RBrace)
            }
            ':' => Some(TokenKind::Colon),
            ',' => Some(TokenKind::Comma),
            '.' => Some(TokenKind::Dot),
            ';' => Some(TokenKind::Semicolon),
            _ => Option::None,
        };

        if let Some(k) = kind {
            pos += 1;
            col += 1;
            tokens.push(Token { kind: k, line, col: start_col });
            continue;
        }

        return Err(PythonError::lex(format!("unexpected character '{ch}'"), line));
    }

    // Emit final newline if needed
    if let Some(last) = tokens.last() && last.kind != TokenKind::Newline && last.kind != TokenKind::Dedent {
        tokens.push(Token { kind: TokenKind::Newline, line, col });
    }

    // Close any remaining indentation
    while indent_stack.len() > 1 {
        indent_stack.pop();
        tokens.push(Token { kind: TokenKind::Dedent, line, col: 1 });
    }

    tokens.push(Token { kind: TokenKind::Eof, line, col });
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<TokenKind> {
        tokenize(source).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn simple_assignment() {
        let k = kinds("x = 10\n");
        assert_eq!(k, vec![
            TokenKind::Ident("x".into()),
            TokenKind::Assign,
            TokenKind::IntLit(10),
            TokenKind::Newline,
            TokenKind::Eof,
        ]);
    }

    #[test]
    fn indent_dedent() {
        let src = "if True:\n    x = 1\ny = 2\n";
        let k = kinds(src);
        assert_eq!(k, vec![
            TokenKind::If,
            TokenKind::True,
            TokenKind::Colon,
            TokenKind::Newline,
            TokenKind::Indent,
            TokenKind::Ident("x".into()),
            TokenKind::Assign,
            TokenKind::IntLit(1),
            TokenKind::Newline,
            TokenKind::Dedent,
            TokenKind::Ident("y".into()),
            TokenKind::Assign,
            TokenKind::IntLit(2),
            TokenKind::Newline,
            TokenKind::Eof,
        ]);
    }

    #[test]
    fn string_escapes() {
        let k = kinds("'hello\\nworld'\n");
        assert_eq!(k[0], TokenKind::StringLit("hello\nworld".into()));
    }

    #[test]
    fn operators() {
        let k = kinds("a + b * c // d ** e\n");
        assert!(k.contains(&TokenKind::Plus));
        assert!(k.contains(&TokenKind::Star));
        assert!(k.contains(&TokenKind::DoubleSlash));
        assert!(k.contains(&TokenKind::DoubleStar));
    }

    #[test]
    fn comparison_operators() {
        let k = kinds("a == b != c <= d >= e\n");
        assert!(k.contains(&TokenKind::Eq));
        assert!(k.contains(&TokenKind::NotEq));
        assert!(k.contains(&TokenKind::LtEq));
        assert!(k.contains(&TokenKind::GtEq));
    }

    #[test]
    fn keywords() {
        let k = kinds("if elif else for in while def return and or not True False None pass break continue\n");
        assert!(k.contains(&TokenKind::If));
        assert!(k.contains(&TokenKind::Elif));
        assert!(k.contains(&TokenKind::Else));
        assert!(k.contains(&TokenKind::For));
        assert!(k.contains(&TokenKind::In));
        assert!(k.contains(&TokenKind::While));
        assert!(k.contains(&TokenKind::Def));
        assert!(k.contains(&TokenKind::Return));
        assert!(k.contains(&TokenKind::And));
        assert!(k.contains(&TokenKind::Or));
        assert!(k.contains(&TokenKind::Not));
        assert!(k.contains(&TokenKind::True));
        assert!(k.contains(&TokenKind::False));
        assert!(k.contains(&TokenKind::None));
        assert!(k.contains(&TokenKind::Pass));
        assert!(k.contains(&TokenKind::Break));
        assert!(k.contains(&TokenKind::Continue));
    }

    #[test]
    fn paren_line_continuation() {
        let k = kinds("f(1,\n  2)\n");
        // No Newline between 1, and 2 due to paren depth
        assert!(!k[..k.len() - 1].windows(2).any(|w|
            w[0] == TokenKind::IntLit(1) && w[1] == TokenKind::Newline
        ));
    }

    #[test]
    fn float_literal() {
        let k = kinds("3.14\n");
        assert_eq!(k[0], TokenKind::FloatLit(3.14));
    }

    #[test]
    fn augmented_assign() {
        let k = kinds("x += 1\n");
        assert_eq!(k[1], TokenKind::PlusAssign);
    }

    #[test]
    fn new_keywords() {
        let k = kinds("class try except finally raise as with assert del global nonlocal lambda yield is from\n");
        assert!(k.contains(&TokenKind::Class));
        assert!(k.contains(&TokenKind::Try));
        assert!(k.contains(&TokenKind::Except));
        assert!(k.contains(&TokenKind::Finally));
        assert!(k.contains(&TokenKind::Raise));
        assert!(k.contains(&TokenKind::As));
        assert!(k.contains(&TokenKind::With));
        assert!(k.contains(&TokenKind::Assert));
        assert!(k.contains(&TokenKind::Del));
        assert!(k.contains(&TokenKind::Global));
        assert!(k.contains(&TokenKind::Nonlocal));
        assert!(k.contains(&TokenKind::Lambda));
        assert!(k.contains(&TokenKind::Yield));
        assert!(k.contains(&TokenKind::Is));
        assert!(k.contains(&TokenKind::From));
    }

    #[test]
    fn bitwise_operators() {
        let k = kinds("a & b | c ^ d ~ e << f >> g\n");
        assert!(k.contains(&TokenKind::Ampersand));
        assert!(k.contains(&TokenKind::Pipe));
        assert!(k.contains(&TokenKind::Caret));
        assert!(k.contains(&TokenKind::Tilde));
        assert!(k.contains(&TokenKind::LShift));
        assert!(k.contains(&TokenKind::RShift));
    }

    #[test]
    fn braces_and_semicolons() {
        let k = kinds("{1: 2}; ...\n");
        assert!(k.contains(&TokenKind::LBrace));
        assert!(k.contains(&TokenKind::RBrace));
        assert!(k.contains(&TokenKind::Semicolon));
        assert!(k.contains(&TokenKind::Ellipsis));
    }
}
