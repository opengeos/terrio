// Written to match the behavior of:
//   PDAL 2.10.2:
//     filters/private/expr/ (expression language)
// No code is translated from these sources.

//! PDAL's expression language, for `filters.expression` and `filters.assign`'s `value`:
//! conditions such as `Classification == 2 && Z > 100` and assignments such as
//! `Classification = 2 WHERE HeightAboveGround < 0.5`.
//!
//! Values are doubles: numbers, dimensions (named as PDAL or terrio names them, in any case),
//! `+ - * /` (dividing by zero gives NaN), unary `-`, parentheses and PDAL's functions (`floor`,
//! `ceil`, `round`, `abs`, `fabs`, `sqrt`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `sinh`,
//! `cosh`, `tanh`, `asinh`, `acosh`, `log`, `log2`, `log10`, `exp`, `exp2`, and `nan()`,
//! `lowest()`, `highest()`). Conditions compare values (`== != < <= > >=`), test them (`isnan`,
//! `ismax`, `ismin`) and combine with `&&`, `||` and `!`.

use crate::pointcloud::{Dimension, PointCloud};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(&'static str),
    LParen,
    RParen,
}

fn lex(s: &str) -> Result<Vec<Tok>> {
    let err = |m: String| Error::InvalidArgument(format!("invalid expression '{s}': {m}"));
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            // As C++'s `>>` reads a double: the longest prefix that is a number.
            let mut end = i;
            let mut best = None;
            while end < b.len() && (b[end].is_ascii_alphanumeric() || matches!(b[end], b'.' | b'+' | b'-')) {
                end += 1;
                if let Ok(v) = s[i..end].parse::<f64>()
                    && !s[i..end].ends_with(['+', '-'])
                {
                    best = Some((v, end));
                }
            }
            let (v, end) = best.ok_or_else(|| err(format!("invalid number at {i}")))?;
            out.push(Tok::Num(v));
            i = end;
            continue;
        }
        if c.is_ascii_alphabetic() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Ident(s[start..i].to_string()));
            continue;
        }
        let next = b.get(i + 1).copied().map(char::from);
        let (tok, len) = match (c, next) {
            ('&', Some('&')) => (Tok::Op("&&"), 2),
            ('|', Some('|')) => (Tok::Op("||"), 2),
            ('!', Some('=')) => (Tok::Op("!="), 2),
            ('!', _) => (Tok::Op("!"), 1),
            ('=', Some('=')) => (Tok::Op("=="), 2),
            ('=', _) => (Tok::Op("="), 1),
            ('<', Some('=')) => (Tok::Op("<="), 2),
            ('<', _) => (Tok::Op("<"), 1),
            ('>', Some('=')) => (Tok::Op(">="), 2),
            ('>', _) => (Tok::Op(">"), 1),
            ('-', Some('-')) => return Err(err("found disallowed consecutive dashes: '--'".into())),
            ('-', _) => (Tok::Op("-"), 1),
            ('+', _) => (Tok::Op("+"), 1),
            ('*', _) => (Tok::Op("*"), 1),
            ('/', _) => (Tok::Op("/"), 1),
            ('(', _) => (Tok::LParen, 1),
            (')', _) => (Tok::RParen, 1),
            _ => return Err(err(format!("unexpected '{c}'"))),
        };
        out.push(tok);
        i += len;
    }
    Ok(out)
}

/// A node of an expression.
#[derive(Debug, Clone)]
enum Node {
    Num(f64),
    /// A dimension, by its index in the expression's names.
    Var(usize),
    Neg(Box<Node>),
    Math(char, Box<Node>, Box<Node>),
    Func(fn(f64) -> f64, Box<Node>),
    Test(fn(f64) -> bool, Box<Node>),
    Compare(&'static str, Box<Node>, Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
    Bool(bool),
}

impl Node {
    fn is_bool(&self) -> bool {
        matches!(self, Node::Test(..) | Node::Compare(..) | Node::And(..) | Node::Or(..) | Node::Not(_) | Node::Bool(_))
    }

    fn value(&self, v: &dyn Fn(usize) -> f64) -> f64 {
        match self {
            Node::Num(x) => *x,
            Node::Var(k) => v(*k),
            Node::Neg(a) => -a.value(v),
            Node::Math(op, a, b) => {
                let (l, r) = (a.value(v), b.value(v));
                match op {
                    '+' => l + r,
                    '-' => l - r,
                    '*' => l * r,
                    _ if r == 0.0 => f64::NAN,
                    _ => l / r,
                }
            }
            Node::Func(f, a) => f(a.value(v)),
            _ => f64::NAN,
        }
    }

    fn truth(&self, v: &dyn Fn(usize) -> f64) -> bool {
        match self {
            Node::Bool(b) => *b,
            Node::Test(f, a) => f(a.value(v)),
            Node::Compare(op, a, b) => compare(op, a.value(v), b.value(v)),
            Node::And(a, b) => a.truth(v) && b.truth(v),
            Node::Or(a, b) => a.truth(v) || b.truth(v),
            Node::Not(a) => !a.truth(v),
            _ => false,
        }
    }
}

fn compare(op: &str, l: f64, r: f64) -> bool {
    match op {
        "==" => l == r,
        "!=" => l != r,
        "<" => l < r,
        "<=" => l <= r,
        ">" => l > r,
        _ => l >= r,
    }
}

/// A function of a value.
type Func = fn(f64) -> f64;
/// A test of a value.
type Test = fn(f64) -> bool;

const FUNCS: &[(&str, Func)] = &[
    ("floor", f64::floor),
    ("ceil", f64::ceil),
    ("round", f64::round),
    ("abs", f64::abs),
    ("fabs", f64::abs),
    ("sqrt", f64::sqrt),
    ("sin", f64::sin),
    ("cos", f64::cos),
    ("tan", f64::tan),
    ("asin", f64::asin),
    ("acos", f64::acos),
    ("atan", f64::atan),
    ("sinh", f64::sinh),
    ("cosh", f64::cosh),
    ("tanh", f64::tanh),
    ("asinh", f64::asinh),
    ("acosh", f64::acosh),
    ("log", f64::ln),
    ("log2", f64::log2),
    ("log10", f64::log10),
    ("exp", f64::exp),
    ("exp2", f64::exp2),
];

fn is_nan(x: f64) -> bool {
    x.is_nan()
}
fn is_max(x: f64) -> bool {
    x == f64::MAX
}
fn is_min(x: f64) -> bool {
    x == f64::MIN
}

const TESTS: &[(&str, Test)] = &[("isnan", is_nan), ("ismax", is_max), ("ismin", is_min)];

struct Parser<'a> {
    text: &'a str,
    toks: Vec<Tok>,
    pos: usize,
    names: Vec<String>,
}

impl Parser<'_> {
    fn err(&self, m: &str) -> Error {
        Error::InvalidArgument(format!("invalid expression '{}': {m}", self.text))
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Op(o)) if *o == op) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn at_where(&self) -> bool {
        matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("WHERE"))
    }

    fn or(&mut self) -> Result<Node> {
        let mut left = self.and()?;
        while self.eat_op("||") {
            let right = self.and()?;
            if !left.is_bool() || !right.is_bool() {
                return Err(self.err("can't apply '||' to numeric expression"));
            }
            left = Node::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Node> {
        let mut left = self.not()?;
        while self.eat_op("&&") {
            let right = self.not()?;
            if !left.is_bool() || !right.is_bool() {
                return Err(self.err("can't apply '&&' to numeric expression"));
            }
            left = Node::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Node> {
        if self.eat_op("!") {
            let sub = self.not()?;
            if !sub.is_bool() {
                return Err(self.err("can't apply '!' to numeric value"));
            }
            return Ok(Node::Not(Box::new(sub)));
        }
        self.compare()
    }

    fn compare(&mut self) -> Result<Node> {
        let mut left = self.add()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Op(o)) if matches!(*o, "==" | "!=" | "<" | "<=" | ">" | ">=") => *o,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.add()?;
            if left.is_bool() || right.is_bool() {
                return Err(self.err(&format!("can't apply '{op}' to logical expression")));
            }
            left = match (&left, &right) {
                (Node::Num(a), Node::Num(b)) => Node::Bool(compare(op, *a, *b)),
                _ => Node::Compare(op, Box::new(left), Box::new(right)),
            };
        }
    }

    fn add(&mut self) -> Result<Node> {
        let mut left = self.mul()?;
        loop {
            let op = if self.eat_op("+") {
                '+'
            } else if self.eat_op("-") {
                '-'
            } else {
                return Ok(left);
            };
            let right = self.mul()?;
            left = self.math(op, left, right)?;
        }
    }

    fn mul(&mut self) -> Result<Node> {
        let mut left = self.unary()?;
        loop {
            let op = if self.eat_op("*") {
                '*'
            } else if self.eat_op("/") {
                '/'
            } else {
                return Ok(left);
            };
            let right = self.unary()?;
            left = self.math(op, left, right)?;
        }
    }

    fn math(&self, op: char, left: Node, right: Node) -> Result<Node> {
        if left.is_bool() || right.is_bool() {
            return Err(self.err(&format!("can't apply '{op}' to logical expression")));
        }
        Ok(match (&left, &right) {
            (Node::Num(_), Node::Num(0.0)) if op == '/' => return Err(self.err("divide by 0")),
            (Node::Num(_), Node::Num(_)) => Node::Num(Node::Math(op, Box::new(left), Box::new(right)).value(&|_| 0.0)),
            _ => Node::Math(op, Box::new(left), Box::new(right)),
        })
    }

    fn unary(&mut self) -> Result<Node> {
        if self.eat_op("-") {
            let sub = self.primary()?;
            if sub.is_bool() {
                return Err(self.err("can't negate a logical expression"));
            }
            return Ok(match sub {
                Node::Num(x) => Node::Num(-x),
                s => Node::Neg(Box::new(s)),
            });
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Node> {
        match self.peek().cloned() {
            Some(Tok::Num(x)) => {
                self.pos += 1;
                Ok(Node::Num(x))
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                let e = self.or()?;
                if self.peek() != Some(&Tok::RParen) {
                    return Err(self.err("expected ')'"));
                }
                self.pos += 1;
                Ok(e)
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                if self.peek() != Some(&Tok::LParen) {
                    let n = crate::pointcloud::names::from_any(&name);
                    let k = match self.names.iter().position(|m| *m == n) {
                        Some(k) => k,
                        None => {
                            self.names.push(n);
                            self.names.len() - 1
                        }
                    };
                    return Ok(Node::Var(k));
                }
                self.pos += 1;
                let constant = match name.as_str() {
                    "nan" => Some(f64::NAN),
                    "lowest" => Some(f64::MIN),
                    "highest" => Some(f64::MAX),
                    _ => None,
                };
                if let Some(c) = constant {
                    if self.peek() != Some(&Tok::RParen) {
                        return Err(self.err(&format!("expecting ')' to close function invocation of '{name}'")));
                    }
                    self.pos += 1;
                    return Ok(Node::Num(c));
                }
                let func = FUNCS.iter().find(|(n, _)| *n == name).map(|f| f.1);
                let test = TESTS.iter().find(|(n, _)| *n == name).map(|f| f.1);
                if func.is_none() && test.is_none() {
                    return Err(self.err(&format!("invalid function name '{name}'")));
                }
                let arg = self.add()?;
                if arg.is_bool() {
                    return Err(self.err(&format!("'{name}' takes a value")));
                }
                if self.peek() != Some(&Tok::RParen) {
                    return Err(self.err(&format!("expecting ')' following '{name}' argument")));
                }
                self.pos += 1;
                Ok(match (func, test) {
                    (Some(f), _) => Node::Func(f, Box::new(arg)),
                    (_, Some(t)) => Node::Test(t, Box::new(arg)),
                    _ => unreachable!("checked above"),
                })
            }
            Some(t) => Err(self.err(&format!("unexpected {t:?}"))),
            None => Err(self.err("unexpected end")),
        }
    }
}

/// A column an expression reads.
enum Column<'c> {
    Dim(&'c Dimension),
    /// A dimension's values times a factor: PDAL's `ScanAngleRank` (degrees) of LAS 1.4 points,
    /// whose scan angles are stored in 0.006° units.
    Scaled(&'c Dimension, f64),
}

impl Column<'_> {
    fn get(&self, i: usize) -> f64 {
        match self {
            Column::Dim(d) => d.get(i),
            // PDAL stores the product in a float.
            Column::Scaled(d, f) => (d.get(i) * f) as f32 as f64,
        }
    }
}

/// The columns of the dimensions an expression names.
fn columns<'c>(cloud: &'c PointCloud, names: &[String], text: &str) -> Result<Vec<Column<'c>>> {
    names
        .iter()
        .map(|n| {
            if let Some(d) = cloud.dim(n).or_else(|| cloud.dim(&crate::pointcloud::names::from_pdal(n))) {
                return Ok(Column::Dim(d));
            }
            if n == "scan_angle_rank"
                && let Some(d) = cloud.dim("scan_angle")
            {
                return Ok(Column::Scaled(d, 0.006));
            }
            Err(Error::InvalidArgument(format!("unknown dimension '{}' in expression '{text}'", crate::pointcloud::names::to_pdal(n))))
        })
        .collect()
}

/// A condition (PDAL's conditional expression): true or false for each point.
#[derive(Debug, Clone)]
pub struct Condition {
    text: String,
    node: Node,
    names: Vec<String>,
}

impl Condition {
    /// Parses a condition, such as `Classification == 2 && Z > 100`.
    pub fn parse(text: &str) -> Result<Self> {
        let mut p = Parser { text, toks: lex(text)?, pos: 0, names: Vec::new() };
        let node = p.or()?;
        if p.pos != p.toks.len() {
            return Err(p.err(&format!("unexpected {:?}", p.toks[p.pos])));
        }
        Self::checked(text, node, p.names)
    }

    fn checked(text: &str, node: Node, names: Vec<String>) -> Result<Self> {
        let err = |m: &str| Error::InvalidArgument(format!("invalid expression '{text}': {m}"));
        match node {
            Node::Bool(b) => Err(err(if b { "expression is always true" } else { "expression is always false" })),
            n if !n.is_bool() => Err(err("expression evaluates to a value, not a boolean")),
            node => Ok(Self { text: text.to_string(), node, names }),
        }
    }

    /// Whether each point passes.
    pub fn mask(&self, cloud: &PointCloud) -> Result<Vec<bool>> {
        let cols = columns(cloud, &self.names, &self.text)?;
        Ok((0..cloud.len()).map(|i| self.node.truth(&|k| cols[k].get(i))).collect())
    }
}

/// An assignment (PDAL's `filters.assign` `value`): `Dimension = value [WHERE condition]`.
#[derive(Debug, Clone)]
pub struct Assignment {
    text: String,
    /// The dimension assigned, as terrio names it.
    pub target: String,
    /// The dimension assigned, as the expression names it.
    target_name: String,
    value: Node,
    condition: Option<Node>,
    names: Vec<String>,
}

impl Assignment {
    /// Parses an assignment, such as `Classification = 2 WHERE HeightAboveGround < 0.5`.
    pub fn parse(text: &str) -> Result<Self> {
        let mut p = Parser { text, toks: lex(text)?, pos: 0, names: Vec::new() };
        let target_name = match p.peek().cloned() {
            Some(Tok::Ident(n)) => n,
            _ => return Err(p.err("expected dimension name for assignment")),
        };
        let target = crate::pointcloud::names::from_any(&target_name);
        p.pos += 1;
        if !p.eat_op("=") {
            return Err(p.err("expected '=' after dimension name in assignment"));
        }
        // The value ends at the end or at WHERE.
        let end =
            (p.pos..p.toks.len()).find(|&i| matches!(&p.toks[i], Tok::Ident(w) if w.eq_ignore_ascii_case("WHERE"))).unwrap_or(p.toks.len());
        let rest = p.toks.split_off(end);
        let value = p.add()?;
        if p.pos != p.toks.len() {
            return Err(p.err(&format!("invalid token {:?} following valid math expression", p.toks[p.pos])));
        }
        if value.is_bool() {
            return Err(p.err("the value assigned is a logical expression"));
        }
        let condition = if rest.is_empty() {
            None
        } else {
            p.toks.extend(rest);
            debug_assert!(p.at_where());
            p.pos += 1;
            let c = p.or()?;
            if p.pos != p.toks.len() {
                return Err(p.err(&format!("unexpected {:?}", p.toks[p.pos])));
            }
            if !c.is_bool() {
                return Err(p.err("the condition evaluates to a value, not a boolean"));
            }
            Some(c)
        };
        Ok(Self { text: text.to_string(), target, target_name, value, condition, names: p.names })
    }

    /// The name of the dimension a cloud without the target gets: PDAL's name in terrio's
    /// convention (snake case).
    pub fn new_dimension_name(&self) -> String {
        crate::pointcloud::names::from_pdal(&self.target_name)
    }

    /// The values assigned to each point: `None` where the condition fails. The dimensions the
    /// expression names must exist (the target need not, unless named in the expression).
    pub fn values(&self, cloud: &PointCloud) -> Result<Vec<Option<f64>>> {
        let cols = columns(cloud, &self.names, &self.text)?;
        Ok((0..cloud.len())
            .map(|i| {
                let v = |k: usize| cols[k].get(i);
                match &self.condition {
                    Some(c) if !c.truth(&v) => None,
                    _ => Some(self.value.value(&v)),
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pdal_expressions() {
        assert!(Condition::parse("Classification == 2 && Z > 10").is_ok());
        assert!(Condition::parse("!(Classification == 7) || isnan(Z)").is_ok());
        assert!(Condition::parse("Z + 1").is_err());
        assert!(Condition::parse("1 < 2").is_err());
        assert!(Condition::parse("Z --1 > 0").is_err());
        let a = Assignment::parse("Classification = 2 WHERE HeightAboveGround < 0.5 && ReturnNumber == 1").unwrap();
        assert_eq!(a.target, "classification");
        assert_eq!(a.names, vec!["height_above_ground", "return_number"]);
        let a = Assignment::parse("Z = Z * 0.3048 - 1e2").unwrap();
        assert!(a.condition.is_none());
        assert_eq!(a.value.value(&|_| 10.0), 10.0 * 0.3048 - 100.0);
    }
}
