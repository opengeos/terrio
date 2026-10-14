//! The expression language of [`calc`](super::calc): a parser for the subset of Python syntax
//! `gdal_calc.py` expressions use.
//!
//! Operators, by increasing precedence: `or`; `and`; `not`; comparisons (`<`, `<=`, `>`, `>=`,
//! `==`, `!=`, chainable); `|`; `^`; `&`; `+`, `-`; `*`, `/`, `//`, `%`; unary `+`, `-`, `~`;
//! `**` (right-associative). Operands are numbers, names (inputs and constants), calls of
//! functions (`where(c, a, b)`, `sqrt(x)`... also written `numpy.sqrt` or `np.sqrt`) and the
//! `.astype(type)` method.

use crate::{Error, Result};

/// A parsed expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// An integer literal.
    Int(i128),
    /// A floating-point literal.
    Float(f64),
    /// `True` or `False`.
    Bool(bool),
    /// A name: an input or a constant.
    Name(String),
    /// A unary operator.
    Unary(UnaryOp, Box<Expr>),
    /// A binary operator.
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// A chain of comparisons `a < b <= c`.
    Compare(Box<Expr>, Vec<(CmpOp, Expr)>),
    /// `a and b and ...`.
    And(Vec<Expr>),
    /// `a or b or ...`.
    Or(Vec<Expr>),
    /// `not a`.
    Not(Box<Expr>),
    /// A function call, with positional and keyword arguments.
    Call(String, Vec<Expr>, Vec<(String, Expr)>),
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-x`.
    Neg,
    /// `+x`.
    Pos,
    /// `~x`.
    Invert,
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `//`.
    FloorDiv,
    /// `%`.
    Mod,
    /// `**`.
    Pow,
    /// `&`.
    BitAnd,
    /// `|`.
    BitOr,
    /// `^`.
    BitXor,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `==`.
    Eq,
    /// `!=`.
    Ne,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Int(i128),
    Float(f64),
    Ident(String),
    Op(&'static str),
    LParen,
    RParen,
    Comma,
    Dot,
    End,
}

fn tokenize(s: &str) -> Result<Vec<Tok>> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    let err = |msg: String| Error::InvalidArgument(format!("invalid expression '{s}': {msg}"));
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()) {
            let start = i;
            let mut float = false;
            if c == '0' && i + 1 < b.len() && matches!(b[i + 1], b'x' | b'X' | b'o' | b'O' | b'b' | b'B') {
                let radix = match b[i + 1] {
                    b'x' | b'X' => 16,
                    b'o' | b'O' => 8,
                    _ => 2,
                };
                i += 2;
                let ds = i;
                while i < b.len() && (b[i] as char).is_ascii_alphanumeric() || i < b.len() && b[i] == b'_' {
                    i += 1;
                }
                let digits: String = s[ds..i].chars().filter(|c| *c != '_').collect();
                let v = i128::from_str_radix(&digits, radix).map_err(|_| err(format!("bad number '{}'", &s[start..i])))?;
                out.push(Tok::Int(v));
                continue;
            }
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                i += 1;
            }
            if i < b.len() && b[i] == b'.' {
                float = true;
                i += 1;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                let mut k = i + 1;
                if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
                    k += 1;
                }
                if k < b.len() && b[k].is_ascii_digit() {
                    float = true;
                    i = k;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text: String = s[start..i].chars().filter(|c| *c != '_').collect();
            if float {
                out.push(Tok::Float(text.parse().map_err(|_| err(format!("bad number '{text}'")))?));
            } else {
                match text.parse::<i128>() {
                    Ok(v) => out.push(Tok::Int(v)),
                    Err(_) => out.push(Tok::Float(text.parse().map_err(|_| err(format!("bad number '{text}'")))?)),
                }
            }
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && ((b[i] as char).is_alphanumeric() || b[i] == b'_' || b[i] >= 0x80) {
                i += 1;
            }
            out.push(Tok::Ident(s[start..i].to_string()));
            continue;
        }
        let rest = &s[i..];
        let op = ["**", "//", "<=", ">=", "==", "!=", "+", "-", "*", "/", "%", "<", ">", "&", "|", "^", "~", "="]
            .into_iter()
            .find(|o| rest.starts_with(o));
        match (c, op) {
            ('(', _) => out.push(Tok::LParen),
            (')', _) => out.push(Tok::RParen),
            (',', _) => out.push(Tok::Comma),
            ('.', _) => out.push(Tok::Dot),
            (_, Some(o)) => {
                out.push(Tok::Op(o));
                i += o.len();
                continue;
            }
            _ => return Err(err(format!("unexpected character '{c}'"))),
        }
        i += 1;
    }
    out.push(Tok::End);
    Ok(out)
}

/// Positional and keyword arguments of a call.
type Args = (Vec<Expr>, Vec<(String, Expr)>);

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    src: &'a str,
}

impl Parser<'_> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn err(&self, msg: impl std::fmt::Display) -> Error {
        Error::InvalidArgument(format!("invalid expression '{}': {msg}", self.src))
    }

    fn is_keyword(&self, k: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s == k)
    }

    fn expect(&mut self, t: Tok, what: &str) -> Result<()> {
        if *self.peek() == t {
            self.next();
            Ok(())
        } else {
            Err(self.err(format!("expected {what}")))
        }
    }

    fn or(&mut self) -> Result<Expr> {
        let first = self.and()?;
        if !self.is_keyword("or") {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.is_keyword("or") {
            self.next();
            items.push(self.and()?);
        }
        Ok(Expr::Or(items))
    }

    fn and(&mut self) -> Result<Expr> {
        let first = self.not()?;
        if !self.is_keyword("and") {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.is_keyword("and") {
            self.next();
            items.push(self.not()?);
        }
        Ok(Expr::And(items))
    }

    fn not(&mut self) -> Result<Expr> {
        if self.is_keyword("not") {
            self.next();
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr> {
        let first = self.bit_or()?;
        let mut rest = Vec::new();
        loop {
            let op = match self.peek() {
                Tok::Op("<") => CmpOp::Lt,
                Tok::Op("<=") => CmpOp::Le,
                Tok::Op(">") => CmpOp::Gt,
                Tok::Op(">=") => CmpOp::Ge,
                Tok::Op("==") => CmpOp::Eq,
                Tok::Op("!=") => CmpOp::Ne,
                _ => break,
            };
            self.next();
            rest.push((op, self.bit_or()?));
        }
        Ok(if rest.is_empty() { first } else { Expr::Compare(Box::new(first), rest) })
    }

    fn binary_level(&mut self, ops: &[(&'static str, BinOp)], next: fn(&mut Self) -> Result<Expr>) -> Result<Expr> {
        let mut left = next(self)?;
        while let Some(op) = ops.iter().find(|(s, _)| *self.peek() == Tok::Op(s)).map(|(_, o)| *o) {
            self.next();
            let right = next(self)?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn bit_or(&mut self) -> Result<Expr> {
        self.binary_level(&[("|", BinOp::BitOr)], Self::bit_xor)
    }

    fn bit_xor(&mut self) -> Result<Expr> {
        self.binary_level(&[("^", BinOp::BitXor)], Self::bit_and)
    }

    fn bit_and(&mut self) -> Result<Expr> {
        self.binary_level(&[("&", BinOp::BitAnd)], Self::additive)
    }

    fn additive(&mut self) -> Result<Expr> {
        self.binary_level(&[("+", BinOp::Add), ("-", BinOp::Sub)], Self::term)
    }

    fn term(&mut self) -> Result<Expr> {
        self.binary_level(&[("*", BinOp::Mul), ("/", BinOp::Div), ("//", BinOp::FloorDiv), ("%", BinOp::Mod)], Self::unary)
    }

    fn unary(&mut self) -> Result<Expr> {
        let op = match self.peek() {
            Tok::Op("-") => UnaryOp::Neg,
            Tok::Op("+") => UnaryOp::Pos,
            Tok::Op("~") => UnaryOp::Invert,
            _ => return self.power(),
        };
        self.next();
        Ok(Expr::Unary(op, Box::new(self.unary()?)))
    }

    fn power(&mut self) -> Result<Expr> {
        let base = self.postfix()?;
        if *self.peek() == Tok::Op("**") {
            self.next();
            let exp = self.unary()?;
            return Ok(Expr::Binary(BinOp::Pow, Box::new(base), Box::new(exp)));
        }
        Ok(base)
    }

    fn args(&mut self) -> Result<Args> {
        let mut args = Vec::new();
        let mut kwargs = Vec::new();
        if *self.peek() == Tok::RParen {
            self.next();
            return Ok((args, kwargs));
        }
        loop {
            if let (Tok::Ident(name), Some(Tok::Op("="))) = (self.peek().clone(), self.toks.get(self.pos + 1)) {
                self.next();
                self.next();
                kwargs.push((name, self.or()?));
            } else {
                if !kwargs.is_empty() {
                    return Err(self.err("positional argument after keyword argument"));
                }
                args.push(self.or()?);
            }
            match self.next() {
                Tok::Comma if *self.peek() == Tok::RParen => {
                    self.next();
                    break;
                }
                Tok::Comma => {}
                Tok::RParen => break,
                _ => return Err(self.err("expected ',' or ')'")),
            }
        }
        Ok((args, kwargs))
    }

    fn postfix(&mut self) -> Result<Expr> {
        let mut e = self.atom()?;
        while *self.peek() == Tok::Dot {
            self.next();
            let Tok::Ident(method) = self.next() else { return Err(self.err("expected a method name after '.'")) };
            self.expect(Tok::LParen, "'(' after a method name")?;
            let (mut args, kwargs) = self.args()?;
            args.insert(0, e);
            e = Expr::Call(method, args, kwargs);
        }
        Ok(e)
    }

    fn atom(&mut self) -> Result<Expr> {
        match self.next() {
            Tok::Int(v) => Ok(Expr::Int(v)),
            Tok::Float(v) => Ok(Expr::Float(v)),
            Tok::LParen => {
                let e = self.or()?;
                self.expect(Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::Ident(name) => {
                // Module qualifiers: `numpy.where`, `np.sqrt`.
                let mut name = name;
                while matches!(name.as_str(), "numpy" | "np" | "math") && *self.peek() == Tok::Dot {
                    self.next();
                    let Tok::Ident(n) = self.next() else { return Err(self.err("expected a name after '.'")) };
                    name = n;
                }
                match name.as_str() {
                    "True" => return Ok(Expr::Bool(true)),
                    "False" => return Ok(Expr::Bool(false)),
                    "and" | "or" | "not" | "if" | "else" | "lambda" | "import" => {
                        return Err(self.err(format!("unexpected keyword '{name}'")));
                    }
                    _ => {}
                }
                if *self.peek() == Tok::LParen {
                    self.next();
                    let (args, kwargs) = self.args()?;
                    return Ok(Expr::Call(name, args, kwargs));
                }
                Ok(Expr::Name(name))
            }
            Tok::End => Err(self.err("unexpected end")),
            t => Err(self.err(format!("unexpected {t:?}"))),
        }
    }
}

impl Expr {
    /// Parses an expression.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim().trim_matches('"');
        let mut p = Parser { toks: tokenize(s)?, pos: 0, src: s };
        let e = p.or()?;
        if *p.peek() != Tok::End {
            return Err(p.err(format!("unexpected {:?}", p.peek())));
        }
        Ok(e)
    }

    /// The names the expression refers to (inputs or constants), in order of appearance.
    pub fn names(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_names(&mut out);
        out
    }

    fn collect_names(&self, out: &mut Vec<String>) {
        match self {
            Expr::Name(n) => {
                if !out.contains(n) {
                    out.push(n.clone());
                }
            }
            Expr::Unary(_, e) | Expr::Not(e) => e.collect_names(out),
            Expr::Binary(_, a, b) => {
                a.collect_names(out);
                b.collect_names(out);
            }
            Expr::Compare(a, rest) => {
                a.collect_names(out);
                rest.iter().for_each(|(_, e)| e.collect_names(out));
            }
            Expr::And(v) | Expr::Or(v) => v.iter().for_each(|e| e.collect_names(out)),
            Expr::Call(_, args, kwargs) => {
                args.iter().for_each(|e| e.collect_names(out));
                kwargs.iter().for_each(|(_, e)| e.collect_names(out));
            }
            Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary(op, Box::new(l), Box::new(r))
    }

    fn n(s: &str) -> Expr {
        Expr::Name(s.into())
    }

    #[test]
    fn precedence() {
        assert_eq!(Expr::parse("A + B * 2").unwrap(), b(BinOp::Add, n("A"), b(BinOp::Mul, n("B"), Expr::Int(2))));
        assert_eq!(Expr::parse("-2**2").unwrap(), Expr::Unary(UnaryOp::Neg, Box::new(b(BinOp::Pow, Expr::Int(2), Expr::Int(2)))));
        assert_eq!(Expr::parse("2**-1").unwrap(), b(BinOp::Pow, Expr::Int(2), Expr::Unary(UnaryOp::Neg, Box::new(Expr::Int(1)))));
        assert_eq!(Expr::parse("2**3**2").unwrap(), b(BinOp::Pow, Expr::Int(2), b(BinOp::Pow, Expr::Int(3), Expr::Int(2))));
        assert_eq!(
            Expr::parse("A & B > 1").unwrap(),
            Expr::Compare(Box::new(b(BinOp::BitAnd, n("A"), n("B"))), vec![(CmpOp::Gt, Expr::Int(1))])
        );
        assert!(matches!(Expr::parse("not A > 1 and B").unwrap(), Expr::And(_)));
        assert_eq!(Expr::parse("1e3").unwrap(), Expr::Float(1000.0));
        assert_eq!(Expr::parse(".5").unwrap(), Expr::Float(0.5));
    }

    #[test]
    fn calls_and_methods() {
        assert_eq!(
            Expr::parse("numpy.where(A > 0, A, 0)").unwrap(),
            Expr::Call(
                "where".into(),
                vec![Expr::Compare(Box::new(n("A")), vec![(CmpOp::Gt, Expr::Int(0))]), n("A"), Expr::Int(0)],
                vec![]
            )
        );
        assert_eq!(Expr::parse("A.astype(np.float32)").unwrap(), Expr::Call("astype".into(), vec![n("A"), n("float32")], vec![]));
        assert_eq!(
            Expr::parse("round(A, decimals=2)").unwrap(),
            Expr::Call("round".into(), vec![n("A")], vec![("decimals".into(), Expr::Int(2))])
        );
        assert_eq!(Expr::parse("(B-A)/(B+A)").unwrap().names(), vec!["B".to_string(), "A".to_string()]);
        assert!(Expr::parse("A +").is_err());
        assert!(Expr::parse("A $ B").is_err());
        assert!(Expr::parse("f(A,").is_err());
    }
}
