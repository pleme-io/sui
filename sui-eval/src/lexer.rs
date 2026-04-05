//! Nix language lexer — tokenizes source into a stream of tokens.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LexError {
    #[error("unexpected character '{ch}' at position {pos}")]
    UnexpectedChar { ch: char, pos: usize },
    #[error("unterminated string at position {pos}")]
    UnterminatedString { pos: usize },
    #[error("unterminated multiline string at position {pos}")]
    UnterminatedIndString { pos: usize },
    #[error("unterminated path at position {pos}")]
    UnterminatedPath { pos: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Literals
    Int(i64),
    Float(f64),
    /// String literal (already unescaped, interpolations resolved).
    Str(String),
    /// Indented (multiline) string.
    IndStr(String),
    /// Path literal.
    Path(String),
    /// Search path (<nixpkgs>).
    SearchPath(String),
    /// URI literal.
    Uri(String),

    // Identifiers and keywords
    Ident(String),
    If,
    Then,
    Else,
    Let,
    In,
    Rec,
    With,
    Assert,
    Inherit,
    OrKw, // `or` keyword (not ||)

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Eq,       // ==
    Neq,      // !=
    Lt,
    Gt,
    Le,       // <=
    Ge,       // >=
    And,      // &&
    Or,       // ||
    Not,      // !
    Impl,     // ->
    Update,   // //
    Concat,   // ++
    Question, // ?

    // Delimiters
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,

    // Punctuation
    Dot,
    Comma,
    Colon,
    Semi,
    Assign, // =
    At,     // @
    Ellipsis, // ...

    // String interpolation
    DollarBrace, // ${
    /// Interpolation content placeholder — parser handles nesting.
    InterpolStart,

    Eof,
}

pub struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.input.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let ch = self.peek()?;
        self.pos += 1;
        Some(ch)
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            // Whitespace
            while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                self.advance();
            }
            // Line comment
            if self.peek() == Some(b'#') {
                while self.peek().is_some_and(|c| c != b'\n') {
                    self.advance();
                }
                continue;
            }
            // Block comment
            if self.peek() == Some(b'/') && self.peek2() == Some(b'*') {
                self.advance();
                self.advance();
                let mut depth = 1;
                while depth > 0 {
                    match self.advance() {
                        Some(b'/') if self.peek() == Some(b'*') => {
                            self.advance();
                            depth += 1;
                        }
                        Some(b'*') if self.peek() == Some(b'/') => {
                            self.advance();
                            depth -= 1;
                        }
                        None => break,
                        _ => {}
                    }
                }
                continue;
            }
            break;
        }
    }

    fn read_string(&mut self) -> Result<Token, LexError> {
        let start = self.pos;
        self.advance(); // consume opening "
        let mut s = String::new();
        loop {
            match self.advance() {
                Some(b'"') => return Ok(Token::Str(s)),
                Some(b'\\') => match self.advance() {
                    Some(b'n') => s.push('\n'),
                    Some(b'r') => s.push('\r'),
                    Some(b't') => s.push('\t'),
                    Some(b'\\') => s.push('\\'),
                    Some(b'"') => s.push('"'),
                    Some(b'$') => s.push('$'),
                    Some(c) => {
                        s.push('\\');
                        s.push(c as char);
                    }
                    None => return Err(LexError::UnterminatedString { pos: start }),
                },
                Some(b'$') if self.peek() == Some(b'{') => {
                    // String interpolation — for now, collect as literal.
                    // Full interpolation parsing happens at the parser level.
                    s.push('$');
                    s.push('{');
                    self.advance();
                    // Simplified: skip until matching }
                    let mut depth = 1;
                    while depth > 0 {
                        match self.advance() {
                            Some(b'{') => depth += 1,
                            Some(b'}') => depth -= 1,
                            Some(c) => s.push(c as char),
                            None => return Err(LexError::UnterminatedString { pos: start }),
                        }
                    }
                    s.push('}');
                }
                Some(c) => {
                    // Handle multi-byte UTF-8 sequences
                    if c & 0x80 == 0 {
                        s.push(c as char);
                    } else {
                        // Multi-byte UTF-8: collect all continuation bytes
                        let byte_count = if c & 0xE0 == 0xC0 { 2 }
                            else if c & 0xF0 == 0xE0 { 3 }
                            else { 4 };
                        let mut bytes = vec![c];
                        for _ in 1..byte_count {
                            if let Some(b) = self.advance() {
                                bytes.push(b);
                            }
                        }
                        if let Ok(utf8) = std::str::from_utf8(&bytes) {
                            s.push_str(utf8);
                        } else {
                            // Invalid UTF-8, push replacement char
                            s.push('\u{FFFD}');
                        }
                    }
                }
                None => return Err(LexError::UnterminatedString { pos: start }),
            }
        }
    }

    fn read_indented_string(&mut self) -> Result<Token, LexError> {
        let start = self.pos;
        self.advance(); // first '
        self.advance(); // second '
        let mut s = String::new();
        loop {
            match self.advance() {
                Some(b'\'') if self.peek() == Some(b'\'') => {
                    // Check for escape sequences
                    if self.input.get(self.pos + 1) == Some(&b'\'') {
                        // ''' = escaped single quote
                        self.advance(); // second '
                        self.advance(); // third '
                        s.push('\'');
                    } else {
                        // '' = end of string
                        self.advance();
                        return Ok(Token::IndStr(s));
                    }
                }
                Some(c) => s.push(c as char),
                None => return Err(LexError::UnterminatedIndString { pos: start }),
            }
        }
    }

    fn read_number(&mut self) -> Token {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.advance();
        }
        if self.peek() == Some(b'.') && self.peek2().is_some_and(|c| c.is_ascii_digit()) {
            self.advance(); // consume .
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.advance();
            }
            // Check for exponent
            if self.peek().is_some_and(|c| c == b'e' || c == b'E') {
                self.advance();
                if self.peek().is_some_and(|c| c == b'+' || c == b'-') {
                    self.advance();
                }
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.advance();
                }
            }
            let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
            Token::Float(s.parse().unwrap_or(0.0))
        } else {
            let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
            Token::Int(s.parse().unwrap_or(0))
        }
    }

    fn read_ident_or_keyword(&mut self) -> Token {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'\'')
        {
            self.advance();
        }
        let word = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
        match word {
            "if" => Token::If,
            "then" => Token::Then,
            "else" => Token::Else,
            "let" => Token::Let,
            "in" => Token::In,
            "rec" => Token::Rec,
            "with" => Token::With,
            "assert" => Token::Assert,
            "inherit" => Token::Inherit,
            "or" => Token::OrKw,
            "true" => Token::Ident("true".to_string()),
            "false" => Token::Ident("false".to_string()),
            "null" => Token::Ident("null".to_string()),
            _ => Token::Ident(word.to_string()),
        }
    }

    fn read_path_or_search(&mut self) -> Result<Token, LexError> {
        if self.peek() == Some(b'<') {
            let start = self.pos;
            self.advance(); // <
            while self.peek().is_some_and(|c| c != b'>') {
                if self.peek().is_none() {
                    return Err(LexError::UnterminatedPath { pos: start });
                }
                self.advance();
            }
            self.advance(); // >
            let path = std::str::from_utf8(&self.input[start + 1..self.pos - 1]).unwrap();
            Ok(Token::SearchPath(path.to_string()))
        } else {
            let start = self.pos;
            while self
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || b"/_.-+".contains(&c))
            {
                self.advance();
            }
            let path = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
            Ok(Token::Path(path.to_string()))
        }
    }

    /// Tokenize the next token.
    pub fn next_token(&mut self) -> Result<Token, LexError> {
        self.skip_whitespace_and_comments();

        let Some(ch) = self.peek() else {
            return Ok(Token::Eof);
        };

        match ch {
            // Strings
            b'"' => self.read_string(),
            b'\'' if self.peek2() == Some(b'\'') => self.read_indented_string(),

            // Numbers
            b'0'..=b'9' => Ok(self.read_number()),

            // Identifiers / keywords / booleans
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = self.pos;
                let tok = self.read_ident_or_keyword();
                // Check if this is actually a path (contains /)
                if self.peek() == Some(b'/') {
                    // Reset and read as path
                    self.pos = start;
                    self.read_path_or_search()
                } else {
                    Ok(tok)
                }
            }

            // Search path
            b'<' if self.peek2().is_some_and(|c| c.is_ascii_alphanumeric()) => {
                self.read_path_or_search()
            }

            // Path starting with . or /
            b'.' if self.peek2() == Some(b'/') => self.read_path_or_search(),
            b'/' if self.peek2().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'.') => {
                self.read_path_or_search()
            }
            b'~' if self.peek2() == Some(b'/') => self.read_path_or_search(),

            // Operators and punctuation (two-char first)
            b'=' if self.peek2() == Some(b'=') => { self.pos += 2; Ok(Token::Eq) }
            b'!' if self.peek2() == Some(b'=') => { self.pos += 2; Ok(Token::Neq) }
            b'<' if self.peek2() == Some(b'=') => { self.pos += 2; Ok(Token::Le) }
            b'>' if self.peek2() == Some(b'=') => { self.pos += 2; Ok(Token::Ge) }
            b'&' if self.peek2() == Some(b'&') => { self.pos += 2; Ok(Token::And) }
            b'|' if self.peek2() == Some(b'|') => { self.pos += 2; Ok(Token::Or) }
            b'-' if self.peek2() == Some(b'>') => { self.pos += 2; Ok(Token::Impl) }
            b'/' if self.peek2() == Some(b'/') => { self.pos += 2; Ok(Token::Update) }
            b'+' if self.peek2() == Some(b'+') => { self.pos += 2; Ok(Token::Concat) }
            b'$' if self.peek2() == Some(b'{') => { self.pos += 2; Ok(Token::DollarBrace) }
            b'.' if self.peek2() == Some(b'.') => {
                if self.input.get(self.pos + 2) == Some(&b'.') {
                    self.pos += 3;
                    Ok(Token::Ellipsis)
                } else {
                    self.pos += 1;
                    Ok(Token::Dot)
                }
            }

            // Single-char operators
            b'+' => { self.advance(); Ok(Token::Plus) }
            b'-' => { self.advance(); Ok(Token::Minus) }
            b'*' => { self.advance(); Ok(Token::Star) }
            b'/' => { self.advance(); Ok(Token::Slash) }
            b'<' => { self.advance(); Ok(Token::Lt) }
            b'>' => { self.advance(); Ok(Token::Gt) }
            b'!' => { self.advance(); Ok(Token::Not) }
            b'?' => { self.advance(); Ok(Token::Question) }
            b'=' => { self.advance(); Ok(Token::Assign) }
            b'@' => { self.advance(); Ok(Token::At) }
            b'.' => { self.advance(); Ok(Token::Dot) }
            b',' => { self.advance(); Ok(Token::Comma) }
            b':' => { self.advance(); Ok(Token::Colon) }
            b';' => { self.advance(); Ok(Token::Semi) }

            // Delimiters
            b'(' => { self.advance(); Ok(Token::LParen) }
            b')' => { self.advance(); Ok(Token::RParen) }
            b'{' => { self.advance(); Ok(Token::LBrace) }
            b'}' => { self.advance(); Ok(Token::RBrace) }
            b'[' => { self.advance(); Ok(Token::LBracket) }
            b']' => { self.advance(); Ok(Token::RBracket) }

            _ => Err(LexError::UnexpectedChar {
                ch: ch as char,
                pos: self.pos,
            }),
        }
    }

    /// Tokenize the entire input.
    pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
        let mut lexer = Lexer::new(input);
        let mut tokens = Vec::new();
        loop {
            let tok = lexer.next_token()?;
            if tok == Token::Eof {
                tokens.push(tok);
                break;
            }
            tokens.push(tok);
        }
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(input: &str) -> Vec<Token> {
        Lexer::tokenize(input).unwrap()
    }

    #[test]
    fn integers() {
        assert_eq!(lex("42"), vec![Token::Int(42), Token::Eof]);
        assert_eq!(lex("0"), vec![Token::Int(0), Token::Eof]);
    }

    #[test]
    fn floats() {
        assert_eq!(lex("3.14"), vec![Token::Float(3.14), Token::Eof]);
        assert_eq!(lex("1.0"), vec![Token::Float(1.0), Token::Eof]);
    }

    #[test]
    fn strings() {
        assert_eq!(lex(r#""hello""#), vec![Token::Str("hello".to_string()), Token::Eof]);
        assert_eq!(lex(r#""hello\nworld""#), vec![Token::Str("hello\nworld".to_string()), Token::Eof]);
        assert_eq!(lex(r#""a\"b""#), vec![Token::Str("a\"b".to_string()), Token::Eof]);
    }

    #[test]
    fn keywords() {
        assert_eq!(lex("if then else"), vec![Token::If, Token::Then, Token::Else, Token::Eof]);
        assert_eq!(lex("let in"), vec![Token::Let, Token::In, Token::Eof]);
        assert_eq!(lex("with rec"), vec![Token::With, Token::Rec, Token::Eof]);
    }

    #[test]
    fn identifiers() {
        assert_eq!(lex("foo"), vec![Token::Ident("foo".to_string()), Token::Eof]);
        assert_eq!(lex("foo_bar"), vec![Token::Ident("foo_bar".to_string()), Token::Eof]);
        assert_eq!(lex("foo-bar"), vec![Token::Ident("foo-bar".to_string()), Token::Eof]);
    }

    #[test]
    fn operators() {
        assert_eq!(lex("+ - * == !="), vec![
            Token::Plus, Token::Minus, Token::Star, Token::Eq, Token::Neq, Token::Eof
        ]);
        assert_eq!(lex("&& || -> // ++"), vec![
            Token::And, Token::Or, Token::Impl, Token::Update, Token::Concat, Token::Eof
        ]);
    }

    #[test]
    fn delimiters() {
        assert_eq!(lex("( ) { } [ ]"), vec![
            Token::LParen, Token::RParen, Token::LBrace, Token::RBrace,
            Token::LBracket, Token::RBracket, Token::Eof
        ]);
    }

    #[test]
    fn punctuation() {
        assert_eq!(lex(". , : ; = @"), vec![
            Token::Dot, Token::Comma, Token::Colon, Token::Semi, Token::Assign, Token::At, Token::Eof
        ]);
    }

    #[test]
    fn comments() {
        assert_eq!(lex("42 # comment"), vec![Token::Int(42), Token::Eof]);
        assert_eq!(lex("/* block */ 42"), vec![Token::Int(42), Token::Eof]);
        assert_eq!(lex("/* nested /* comment */ */ 42"), vec![Token::Int(42), Token::Eof]);
    }

    #[test]
    fn search_path() {
        assert_eq!(lex("<nixpkgs>"), vec![Token::SearchPath("nixpkgs".to_string()), Token::Eof]);
    }

    #[test]
    fn ellipsis() {
        assert_eq!(lex("..."), vec![Token::Ellipsis, Token::Eof]);
    }

    #[test]
    fn let_expression() {
        let tokens = lex("let x = 1; in x");
        assert_eq!(tokens, vec![
            Token::Let,
            Token::Ident("x".to_string()),
            Token::Assign,
            Token::Int(1),
            Token::Semi,
            Token::In,
            Token::Ident("x".to_string()),
            Token::Eof,
        ]);
    }

    #[test]
    fn function_def() {
        let tokens = lex("x: x + 1");
        assert_eq!(tokens, vec![
            Token::Ident("x".to_string()),
            Token::Colon,
            Token::Ident("x".to_string()),
            Token::Plus,
            Token::Int(1),
            Token::Eof,
        ]);
    }

    #[test]
    fn attrset() {
        let tokens = lex("{ a = 1; b = 2; }");
        assert_eq!(tokens, vec![
            Token::LBrace,
            Token::Ident("a".to_string()), Token::Assign, Token::Int(1), Token::Semi,
            Token::Ident("b".to_string()), Token::Assign, Token::Int(2), Token::Semi,
            Token::RBrace,
            Token::Eof,
        ]);
    }

    #[test]
    fn comparison_operators() {
        assert_eq!(lex("< > <= >="), vec![
            Token::Lt, Token::Gt, Token::Le, Token::Ge, Token::Eof
        ]);
    }

    #[test]
    fn boolean_literals() {
        assert_eq!(lex("true false null"), vec![
            Token::Ident("true".to_string()),
            Token::Ident("false".to_string()),
            Token::Ident("null".to_string()),
            Token::Eof,
        ]);
    }

    // ── New tests ────────────────────────────────────────

    #[test]
    fn negative_number_is_minus_plus_int() {
        // Nix lexer does not produce negative int tokens; negation is
        // a unary operator at the parser level.
        assert_eq!(lex("-42"), vec![Token::Minus, Token::Int(42), Token::Eof]);
    }

    #[test]
    fn very_long_identifier() {
        let long = "a".repeat(500);
        let tokens = lex(&long);
        assert_eq!(tokens, vec![Token::Ident(long), Token::Eof]);
    }

    #[test]
    fn nested_block_comments() {
        // Already partially covered, but exercise deeper nesting
        assert_eq!(
            lex("/* outer /* inner /* deep */ inner */ outer */ 7"),
            vec![Token::Int(7), Token::Eof],
        );
    }

    #[test]
    fn unicode_in_strings() {
        assert_eq!(
            lex(r#""hello ☺ world""#),
            vec![Token::Str("hello ☺ world".to_string()), Token::Eof],
        );
    }

    #[test]
    fn tab_and_newline_in_whitespace() {
        assert_eq!(lex("\t\n\r 42"), vec![Token::Int(42), Token::Eof]);
    }

    #[test]
    fn empty_input() {
        assert_eq!(lex(""), vec![Token::Eof]);
    }

    #[test]
    fn dollar_brace_token() {
        assert_eq!(
            lex("${"),
            vec![Token::DollarBrace, Token::Eof],
        );
    }

    #[test]
    fn whitespace_only() {
        assert_eq!(lex("   \t  \n  "), vec![Token::Eof]);
    }

    #[test]
    fn comment_only() {
        assert_eq!(lex("# just a comment"), vec![Token::Eof]);
    }

    #[test]
    fn float_with_exponent() {
        let tokens = lex("1.5e10");
        assert_eq!(tokens.len(), 2);
        if let Token::Float(f) = tokens[0] {
            assert!((f - 1.5e10).abs() < 1.0);
        } else {
            panic!("expected float");
        }
    }
}
