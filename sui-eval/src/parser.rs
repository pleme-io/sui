//! Nix parser — transforms token stream into AST.
//!
//! Operator precedence (low to high):
//! 1. ->  (right)
//! 2. ||  (left)
//! 3. &&  (left)
//! 4. == != (none)
//! 5. < > <= >= (none)
//! 6. //  (right)
//! 7. !   (prefix)
//! 8. + - (left)
//! 9. * / (left)
//! 10. ++ (right)
//! 11. ?  (postfix)
//! 12. -  (unary prefix)
//! 13. function application (left)

use crate::ast::*;
use crate::lexer::{LexError, Lexer, Token};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("lex error: {0}")]
    Lex(#[from] LexError),
    #[error("unexpected token: {0:?}")]
    Unexpected(Token),
    #[error("expected {expected}, got {got:?}")]
    Expected { expected: String, got: Token },
    #[error("unexpected end of input")]
    UnexpectedEof,
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(input: &str) -> Result<Self, ParseError> {
        let tokens = Lexer::tokenize(input)?;
        Ok(Self { tokens, pos: 0 })
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<Token, ParseError> {
        let tok = self.advance();
        if std::mem::discriminant(&tok) == std::mem::discriminant(expected) {
            Ok(tok)
        } else {
            Err(ParseError::Expected {
                expected: format!("{expected:?}"),
                got: tok,
            })
        }
    }

    fn expect_semi(&mut self) -> Result<(), ParseError> {
        self.expect(&Token::Semi)?;
        Ok(())
    }

    /// Parse a complete expression.
    pub fn parse(input: &str) -> Result<Expr, ParseError> {
        let mut parser = Parser::new(input)?;
        let expr = parser.parse_expr()?;
        Ok(expr)
    }

    // ── Expression parsing (precedence climbing) ──────────

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        // Check for lambda, let, if, with, assert first
        match self.peek() {
            Token::Let => return self.parse_let(),
            Token::If => return self.parse_if(),
            Token::With => return self.parse_with(),
            Token::Assert => return self.parse_assert(),
            Token::LBrace => {
                // Could be lambda `{ formals }: body` or attrset `{ bindings }`
                // Peek ahead to distinguish
                if self.is_lambda_start() {
                    return self.parse_lambda();
                }
            }
            Token::Ident(_) => {
                // Could be `ident: body` (lambda) or `ident @ { ... }: body`
                if self.is_simple_lambda() {
                    return self.parse_lambda();
                }
            }
            _ => {}
        }
        self.parse_impl()
    }

    fn is_lambda_start(&self) -> bool {
        // { ... }: — scan forward for } : pattern
        let mut depth = 0;
        let mut i = self.pos;
        while i < self.tokens.len() {
            match &self.tokens[i] {
                Token::LBrace => depth += 1,
                Token::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1 < self.tokens.len()
                            && (self.tokens[i + 1] == Token::Colon
                                || self.tokens[i + 1] == Token::At);
                    }
                }
                Token::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn is_simple_lambda(&self) -> bool {
        // ident : or ident @
        matches!(
            (self.tokens.get(self.pos), self.tokens.get(self.pos + 1)),
            (Some(Token::Ident(_)), Some(Token::Colon))
                | (Some(Token::Ident(_)), Some(Token::At))
        )
    }

    // Precedence level 1: -> (right associative)
    fn parse_impl(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_or()?;
        if *self.peek() == Token::Impl {
            self.advance();
            let rhs = self.parse_impl()?; // right associative
            Ok(Expr::BinOp(BinOp::Impl, Box::new(lhs), Box::new(rhs)))
        } else {
            Ok(lhs)
        }
    }

    // Precedence level 2: || (left)
    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        while *self.peek() == Token::Or {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::BinOp(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    // Precedence level 3: && (left)
    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_comparison()?;
        while *self.peek() == Token::And {
            self.advance();
            let rhs = self.parse_comparison()?;
            lhs = Expr::BinOp(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    // Precedence level 4-5: == != < > <= >= (non-associative)
    fn parse_comparison(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_update()?;
        let op = match self.peek() {
            Token::Eq => BinOp::Eq,
            Token::Neq => BinOp::Neq,
            Token::Lt => BinOp::Lt,
            Token::Le => BinOp::Le,
            Token::Gt => BinOp::Gt,
            Token::Ge => BinOp::Ge,
            _ => return Ok(lhs),
        };
        self.advance();
        let rhs = self.parse_update()?;
        Ok(Expr::BinOp(op, Box::new(lhs), Box::new(rhs)))
    }

    // Precedence level 6: // (right)
    fn parse_update(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_not()?;
        if *self.peek() == Token::Update {
            self.advance();
            let rhs = self.parse_update()?; // right associative
            Ok(Expr::BinOp(BinOp::Update, Box::new(lhs), Box::new(rhs)))
        } else {
            Ok(lhs)
        }
    }

    // Precedence level 7: ! (prefix)
    fn parse_not(&mut self) -> Result<Expr, ParseError> {
        if *self.peek() == Token::Not {
            self.advance();
            let expr = self.parse_not()?;
            Ok(Expr::UnaryOp(UnaryOp::Not, Box::new(expr)))
        } else {
            self.parse_add()
        }
    }

    // Precedence level 8: + - (left)
    fn parse_add(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_mul()?;
            lhs = Expr::BinOp(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    // Precedence level 9: * / (left)
    fn parse_mul(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_concat()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_concat()?;
            lhs = Expr::BinOp(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    // Precedence level 10: ++ (right)
    fn parse_concat(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_unary_minus()?;
        if *self.peek() == Token::Concat {
            self.advance();
            let rhs = self.parse_concat()?; // right associative
            Ok(Expr::BinOp(BinOp::Concat, Box::new(lhs), Box::new(rhs)))
        } else {
            Ok(lhs)
        }
    }

    // Precedence level 12: - (unary prefix)
    fn parse_unary_minus(&mut self) -> Result<Expr, ParseError> {
        if *self.peek() == Token::Minus {
            self.advance();
            let expr = self.parse_application()?;
            Ok(Expr::UnaryOp(UnaryOp::Neg, Box::new(expr)))
        } else {
            self.parse_application()
        }
    }

    // Precedence level 13: function application (left)
    fn parse_application(&mut self) -> Result<Expr, ParseError> {
        let mut func = self.parse_select()?;
        while self.is_atom_start() {
            let arg = self.parse_select()?;
            func = Expr::Apply(Box::new(func), Box::new(arg));
        }
        Ok(func)
    }

    fn is_atom_start(&self) -> bool {
        matches!(
            self.peek(),
            Token::Int(_)
                | Token::Float(_)
                | Token::Str(_)
                | Token::IndStr(_)
                | Token::Path(_)
                | Token::SearchPath(_)
                | Token::Ident(_)
                | Token::LParen
                | Token::LBrace
                | Token::LBracket
                | Token::Rec
        )
    }

    // Select: expr.attr or expr.attr or default, and expr ? attr
    fn parse_select(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_atom()?;
        loop {
            if *self.peek() == Token::Dot {
                self.advance();
                let attr = self.parse_attr_path_segment()?;
                if *self.peek() == Token::OrKw {
                    self.advance();
                    let default = self.parse_select()?;
                    expr = Expr::Select(Box::new(expr), vec![attr], Some(Box::new(default)));
                } else {
                    expr = Expr::Select(Box::new(expr), vec![attr], None);
                }
            } else if *self.peek() == Token::Question {
                self.advance();
                let attr = self.parse_attr_path_segment()?;
                expr = Expr::HasAttr(Box::new(expr), vec![attr]);
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_attr_path_segment(&mut self) -> Result<AttrName, ParseError> {
        match self.advance() {
            Token::Ident(name) => Ok(AttrName::Static(name)),
            Token::Str(s) => Ok(AttrName::Static(s)),
            Token::DollarBrace => {
                let expr = self.parse_expr()?;
                self.expect(&Token::RBrace)?;
                Ok(AttrName::Dynamic(expr))
            }
            tok => {
                // `or` can be used as an attribute name
                if tok == Token::OrKw {
                    return Ok(AttrName::Static("or".to_string()));
                }
                Err(ParseError::Expected {
                    expected: "attribute name".to_string(),
                    got: tok,
                })
            }
        }
    }

    // Atoms
    fn parse_atom(&mut self) -> Result<Expr, ParseError> {
        match self.peek().clone() {
            Token::Int(n) => { let n = n; self.advance(); Ok(Expr::Int(n)) }
            Token::Float(f) => { let f = f; self.advance(); Ok(Expr::Float(f)) }
            Token::Str(ref s) => { let s = s.clone(); self.advance(); Ok(Expr::Str(s)) }
            Token::IndStr(ref s) => { let s = s.clone(); self.advance(); Ok(Expr::Str(s)) }
            Token::Path(ref p) => { let p = p.clone(); self.advance(); Ok(Expr::Path(p)) }
            Token::SearchPath(ref p) => { let p = p.clone(); self.advance(); Ok(Expr::SearchPath(p)) }
            Token::Ident(ref name) => {
                match name.as_str() {
                    "true" => { self.advance(); Ok(Expr::Bool(true)) }
                    "false" => { self.advance(); Ok(Expr::Bool(false)) }
                    "null" => { self.advance(); Ok(Expr::Null) }
                    _ => { let n = name.clone(); self.advance(); Ok(Expr::Var(n)) }
                }
            }
            Token::LParen => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            Token::LBracket => self.parse_list(),
            Token::Rec => self.parse_rec_attrset(),
            Token::LBrace => self.parse_attrset(),
            ref tok => Err(ParseError::Unexpected(tok.clone())),
        }
    }

    fn parse_list(&mut self) -> Result<Expr, ParseError> {
        self.advance(); // [
        let mut items = Vec::new();
        while *self.peek() != Token::RBracket {
            items.push(self.parse_select()?);
        }
        self.advance(); // ]
        Ok(Expr::List(items))
    }

    fn parse_rec_attrset(&mut self) -> Result<Expr, ParseError> {
        self.advance(); // rec
        self.expect(&Token::LBrace)?;
        let bindings = self.parse_bindings()?;
        self.expect(&Token::RBrace)?;
        Ok(Expr::AttrSet(AttrSet {
            recursive: true,
            bindings,
        }))
    }

    fn parse_attrset(&mut self) -> Result<Expr, ParseError> {
        self.advance(); // {
        let bindings = self.parse_bindings()?;
        self.expect(&Token::RBrace)?;
        Ok(Expr::AttrSet(AttrSet {
            recursive: false,
            bindings,
        }))
    }

    fn parse_bindings(&mut self) -> Result<Vec<Binding>, ParseError> {
        let mut bindings = Vec::new();
        loop {
            match self.peek() {
                Token::RBrace | Token::Eof | Token::In => break,
                Token::Inherit => {
                    self.advance();
                    if *self.peek() == Token::LParen {
                        self.advance();
                        let from = self.parse_expr()?;
                        self.expect(&Token::RParen)?;
                        let mut names = Vec::new();
                        while let Token::Ident(ref n) = *self.peek() {
                            names.push(n.clone());
                            self.advance();
                        }
                        self.expect_semi()?;
                        bindings.push(Binding::Inherit(Some(from), names));
                    } else {
                        let mut names = Vec::new();
                        while let Token::Ident(ref n) = *self.peek() {
                            names.push(n.clone());
                            self.advance();
                        }
                        self.expect_semi()?;
                        bindings.push(Binding::Inherit(None, names));
                    }
                }
                _ => {
                    // attr path = expr ;
                    let mut path = Vec::new();
                    path.push(self.parse_attr_path_segment()?);
                    while *self.peek() == Token::Dot {
                        self.advance();
                        path.push(self.parse_attr_path_segment()?);
                    }
                    self.expect(&Token::Assign)?;
                    let value = self.parse_expr()?;
                    self.expect_semi()?;
                    bindings.push(Binding::AttrPath(path, value));
                }
            }
        }
        Ok(bindings)
    }

    // ── Control flow ──────────────────────────────────────

    fn parse_let(&mut self) -> Result<Expr, ParseError> {
        self.advance(); // let
        let bindings = self.parse_bindings()?;
        self.expect(&Token::In)?;
        let body = self.parse_expr()?;
        Ok(Expr::Let(bindings, Box::new(body)))
    }

    fn parse_if(&mut self) -> Result<Expr, ParseError> {
        self.advance(); // if
        let cond = self.parse_expr()?;
        self.expect(&Token::Then)?;
        let then_expr = self.parse_expr()?;
        self.expect(&Token::Else)?;
        let else_expr = self.parse_expr()?;
        Ok(Expr::If(Box::new(cond), Box::new(then_expr), Box::new(else_expr)))
    }

    fn parse_with(&mut self) -> Result<Expr, ParseError> {
        self.advance(); // with
        let env = self.parse_expr()?;
        self.expect(&Token::Semi)?;
        let body = self.parse_expr()?;
        Ok(Expr::With(Box::new(env), Box::new(body)))
    }

    fn parse_assert(&mut self) -> Result<Expr, ParseError> {
        self.advance(); // assert
        let cond = self.parse_expr()?;
        self.expect(&Token::Semi)?;
        let body = self.parse_expr()?;
        Ok(Expr::Assert(Box::new(cond), Box::new(body)))
    }

    // ── Lambda ────────────────────────────────────────────

    fn parse_lambda(&mut self) -> Result<Expr, ParseError> {
        let pattern = self.parse_pattern()?;
        self.expect(&Token::Colon)?;
        let body = self.parse_expr()?;
        Ok(Expr::Lambda(pattern, Box::new(body)))
    }

    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        match self.peek() {
            Token::Ident(_) => {
                if self.tokens.get(self.pos + 1) == Some(&Token::At) {
                    // name @ { formals }
                    let Token::Ident(name) = self.advance() else { unreachable!() };
                    self.advance(); // @
                    let (formals, ellipsis) = self.parse_formals()?;
                    Ok(Pattern::Formals { formals, ellipsis, name: Some(name) })
                } else {
                    let Token::Ident(name) = self.advance() else { unreachable!() };
                    Ok(Pattern::Ident(name))
                }
            }
            Token::LBrace => {
                let (formals, ellipsis) = self.parse_formals()?;
                let name = if *self.peek() == Token::At {
                    self.advance();
                    let Token::Ident(n) = self.advance() else {
                        return Err(ParseError::Expected {
                            expected: "identifier after @".to_string(),
                            got: self.tokens[self.pos - 1].clone(),
                        });
                    };
                    Some(n)
                } else {
                    None
                };
                Ok(Pattern::Formals { formals, ellipsis, name })
            }
            tok => Err(ParseError::Expected {
                expected: "function pattern".to_string(),
                got: tok.clone(),
            }),
        }
    }

    fn parse_formals(&mut self) -> Result<(Vec<Formal>, bool), ParseError> {
        self.expect(&Token::LBrace)?;
        let mut formals = Vec::new();
        let mut ellipsis = false;

        if *self.peek() == Token::RBrace {
            self.advance();
            return Ok((formals, false));
        }

        if *self.peek() == Token::Ellipsis {
            self.advance();
            ellipsis = true;
            self.expect(&Token::RBrace)?;
            return Ok((formals, ellipsis));
        }

        loop {
            if *self.peek() == Token::Ellipsis {
                self.advance();
                ellipsis = true;
                break;
            }

            let Token::Ident(name) = self.advance() else {
                return Err(ParseError::Expected {
                    expected: "formal parameter name".to_string(),
                    got: self.tokens[self.pos - 1].clone(),
                });
            };

            let default = if *self.peek() == Token::Question {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };

            formals.push(Formal { name, default });

            if *self.peek() == Token::Comma {
                self.advance();
            } else {
                break;
            }
        }

        self.expect(&Token::RBrace)?;
        Ok((formals, ellipsis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Expr {
        Parser::parse(input).unwrap()
    }

    #[test]
    fn parse_int() {
        assert_eq!(parse("42"), Expr::Int(42));
    }

    #[test]
    fn parse_string() {
        assert_eq!(parse(r#""hello""#), Expr::Str("hello".to_string()));
    }

    #[test]
    fn parse_bool() {
        assert_eq!(parse("true"), Expr::Bool(true));
        assert_eq!(parse("false"), Expr::Bool(false));
    }

    #[test]
    fn parse_null() {
        assert_eq!(parse("null"), Expr::Null);
    }

    #[test]
    fn parse_arithmetic() {
        assert_eq!(
            parse("1 + 2"),
            Expr::BinOp(BinOp::Add, Box::new(Expr::Int(1)), Box::new(Expr::Int(2)))
        );
    }

    #[test]
    fn parse_precedence() {
        // 1 + 2 * 3 = 1 + (2 * 3)
        let expr = parse("1 + 2 * 3");
        assert_eq!(
            expr,
            Expr::BinOp(
                BinOp::Add,
                Box::new(Expr::Int(1)),
                Box::new(Expr::BinOp(BinOp::Mul, Box::new(Expr::Int(2)), Box::new(Expr::Int(3)))),
            )
        );
    }

    #[test]
    fn parse_let() {
        let expr = parse("let x = 1; in x");
        assert!(matches!(expr, Expr::Let(_, _)));
    }

    #[test]
    fn parse_if() {
        let expr = parse("if true then 1 else 2");
        assert!(matches!(expr, Expr::If(_, _, _)));
    }

    #[test]
    fn parse_lambda() {
        let expr = parse("x: x + 1");
        assert!(matches!(expr, Expr::Lambda(Pattern::Ident(_), _)));
    }

    #[test]
    fn parse_application() {
        let expr = parse("f 42");
        assert!(matches!(expr, Expr::Apply(_, _)));
    }

    #[test]
    fn parse_list() {
        let expr = parse("[1 2 3]");
        if let Expr::List(items) = expr {
            assert_eq!(items.len(), 3);
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn parse_attrset() {
        let expr = parse("{ a = 1; b = 2; }");
        if let Expr::AttrSet(set) = expr {
            assert!(!set.recursive);
            assert_eq!(set.bindings.len(), 2);
        } else {
            panic!("expected attrset");
        }
    }

    #[test]
    fn parse_rec_attrset() {
        let expr = parse("rec { a = 1; b = a; }");
        if let Expr::AttrSet(set) = expr {
            assert!(set.recursive);
        } else {
            panic!("expected rec attrset");
        }
    }

    #[test]
    fn parse_with() {
        let expr = parse("with pkgs; hello");
        assert!(matches!(expr, Expr::With(_, _)));
    }

    #[test]
    fn parse_select() {
        let expr = parse("a.b");
        assert!(matches!(expr, Expr::Select(_, _, None)));
    }

    #[test]
    fn parse_select_or() {
        let expr = parse("a.b or 42");
        assert!(matches!(expr, Expr::Select(_, _, Some(_))));
    }

    #[test]
    fn parse_has_attr() {
        let expr = parse("a ? b");
        assert!(matches!(expr, Expr::HasAttr(_, _)));
    }

    #[test]
    fn parse_comparison() {
        let expr = parse("1 == 2");
        assert!(matches!(expr, Expr::BinOp(BinOp::Eq, _, _)));
    }

    #[test]
    fn parse_formals() {
        let expr = parse("{ a, b ? 1 }: a + b");
        if let Expr::Lambda(Pattern::Formals { formals, ellipsis, name }, _) = expr {
            assert_eq!(formals.len(), 2);
            assert!(!ellipsis);
            assert!(name.is_none());
            assert!(formals[1].default.is_some());
        } else {
            panic!("expected lambda with formals");
        }
    }

    #[test]
    fn parse_formals_ellipsis() {
        let expr = parse("{ a, ... }: a");
        if let Expr::Lambda(Pattern::Formals { ellipsis, .. }, _) = expr {
            assert!(ellipsis);
        } else {
            panic!("expected lambda with ellipsis");
        }
    }

    #[test]
    fn parse_named_formals() {
        let expr = parse("args @ { a }: a");
        if let Expr::Lambda(Pattern::Formals { name, .. }, _) = expr {
            assert_eq!(name, Some("args".to_string()));
        } else {
            panic!("expected named formals");
        }
    }

    #[test]
    fn parse_nested_let() {
        let expr = parse("let a = 1; b = let c = 2; in c; in a + b");
        assert!(matches!(expr, Expr::Let(_, _)));
    }

    #[test]
    fn parse_negation() {
        let expr = parse("-42");
        assert!(matches!(expr, Expr::UnaryOp(UnaryOp::Neg, _)));
    }

    #[test]
    fn parse_not() {
        let expr = parse("!true");
        assert!(matches!(expr, Expr::UnaryOp(UnaryOp::Not, _)));
    }

    #[test]
    fn parse_inherit() {
        let expr = parse("{ inherit a b; c = 3; }");
        if let Expr::AttrSet(set) = expr {
            assert_eq!(set.bindings.len(), 2);
            assert!(matches!(&set.bindings[0], Binding::Inherit(None, names) if names.len() == 2));
        } else {
            panic!("expected attrset with inherit");
        }
    }

    #[test]
    fn parse_inherit_from() {
        let expr = parse("{ inherit (x) a b; }");
        if let Expr::AttrSet(set) = expr {
            assert!(matches!(&set.bindings[0], Binding::Inherit(Some(_), names) if names.len() == 2));
        } else {
            panic!("expected attrset with inherit from");
        }
    }

    // ── New tests ────────────────────────────────────────

    #[test]
    fn parse_error_on_unexpected_token() {
        let result = Parser::parse(")");
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_on_bare_operator() {
        let result = Parser::parse("+");
        assert!(result.is_err());
    }

    #[test]
    fn parse_deeply_nested_let_3_levels() {
        let expr = parse("let a = let b = let c = 1; in c; in b; in a");
        // Outermost is Let
        assert!(matches!(expr, Expr::Let(_, _)));
        if let Expr::Let(bindings, body) = expr {
            // body is Var("a")
            assert_eq!(*body, Expr::Var("a".to_string()));
            // first binding value is another Let
            if let Binding::AttrPath(_, inner) = &bindings[0] {
                assert!(matches!(inner, Expr::Let(_, _)));
            } else {
                panic!("expected attr path binding");
            }
        }
    }

    #[test]
    fn parse_multiple_function_application() {
        // f x y z should be ((f x) y) z — left-associative
        let expr = parse("f x y z");
        // Outermost is Apply(Apply(Apply(f,x),y),z)
        if let Expr::Apply(inner, z) = &expr {
            assert_eq!(**z, Expr::Var("z".to_string()));
            if let Expr::Apply(inner2, y) = inner.as_ref() {
                assert_eq!(**y, Expr::Var("y".to_string()));
                if let Expr::Apply(f, x) = inner2.as_ref() {
                    assert_eq!(**f, Expr::Var("f".to_string()));
                    assert_eq!(**x, Expr::Var("x".to_string()));
                } else {
                    panic!("expected Apply(f,x)");
                }
            } else {
                panic!("expected Apply(Apply(f,x),y)");
            }
        } else {
            panic!("expected Apply");
        }
    }

    #[test]
    fn parse_nested_attribute_access() {
        // a.b.c should produce chained selects
        let expr = parse("a.b.c");
        // Outer: Select(Select(Var(a), [b], None), [c], None)
        if let Expr::Select(inner, path_c, None) = &expr {
            assert_eq!(path_c, &vec![AttrName::Static("c".to_string())]);
            if let Expr::Select(base, path_b, None) = inner.as_ref() {
                assert_eq!(path_b, &vec![AttrName::Static("b".to_string())]);
                assert_eq!(**base, Expr::Var("a".to_string()));
            } else {
                panic!("expected inner select");
            }
        } else {
            panic!("expected outer select");
        }
    }

    #[test]
    fn parse_complex_let_with_lambda() {
        let expr = parse("let f = x: x + 1; in f 5");
        assert!(matches!(expr, Expr::Let(_, _)));
    }

    #[test]
    fn parse_empty_list() {
        let expr = parse("[]");
        if let Expr::List(items) = expr {
            assert!(items.is_empty());
        } else {
            panic!("expected empty list");
        }
    }

    #[test]
    fn parse_empty_attrset() {
        let expr = parse("{}");
        if let Expr::AttrSet(set) = expr {
            assert!(set.bindings.is_empty());
            assert!(!set.recursive);
        } else {
            panic!("expected empty attrset");
        }
    }

    #[test]
    fn parse_with_lambda_body() {
        // with expr; lambda
        let expr = parse("with { x = 1; }; y: y");
        assert!(matches!(expr, Expr::With(_, _)));
    }

    #[test]
    fn parse_logical_combination() {
        let expr = parse("a && b || c");
        // || is lower precedence than &&
        assert!(matches!(expr, Expr::BinOp(BinOp::Or, _, _)));
    }
}
