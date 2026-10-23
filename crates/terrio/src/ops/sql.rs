//! A subset of OGR SQL: attribute filter expressions (`-where`) and simple `SELECT` statements.
//!
//! Expressions support comparisons (`=`, `!=`, `<>`, `<`, `<=`, `>`, `>=`), `AND`, `OR`, `NOT`,
//! parentheses, `IN (...)`, `LIKE` and `ILIKE` with `%` and `_` (and `ESCAPE`), `IS [NOT] NULL`,
//! `BETWEEN`, arithmetic (`+ - * / %`), string concatenation (`||`), numeric and string literals.
//! Field names are matched case-insensitively and may be double-quoted; the special field `FID`
//! is the feature id. As in OGR SQL, comparisons with nulls are false, `LIKE` is case-sensitive
//! and `ILIKE` is not.
//!
//! ```
//! use terrio::ops::sql::Expression;
//! let e = Expression::parse("pop_est > 1e8 AND continent IN ('Asia', 'Africa')").unwrap();
//! assert_eq!(e.fields(), vec!["pop_est", "continent"]);
//! ```

use crate::{Error, Result};
use std::cmp::Ordering;
use wbvector::{Feature, FieldValue, Layer, Schema};

/// Runs a read-only `SELECT` using terrio's OGR SQL subset, preserving source geometry, CRS
/// and feature IDs. Supports field selection/aliases, WHERE, one ORDER BY field, and LIMIT.
/// FROM must name an input layer (case-insensitively). Joins, aggregates, spatial functions and
/// computed SELECT columns are unsupported. Output field names must be unique.
///
/// ```no_run
/// let ds = terrio::open_vector("countries.gpkg", &Default::default())?;
/// let result = terrio::ops::sql::query(&ds.layers,
///     "SELECT ADMIN, POP_EST AS population FROM countries WHERE POP_EST > 1e7 ORDER BY POP_EST DESC LIMIT 10")?;
/// terrio::write_vector(&[result], "largest.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn query(layers: &[Layer], statement: &str) -> Result<Layer> {
    let select = Select::parse(statement)?;
    if !layers.iter().any(|l| l.name.eq_ignore_ascii_case(&select.from)) {
        return Err(Error::InvalidArgument(format!("SQL: no layer named '{}'", select.from)));
    }
    let (mut layer, limit) = super::vector_filter::run_select(layers, &select)?;
    if let Some(n) = limit {
        layer.features.truncate(n);
    }
    Ok(layer)
}

/// A parsed expression, not yet bound to a layer's fields.
#[derive(Debug, Clone, PartialEq)]
pub struct Expression(Expr);

/// An expression bound to a layer's schema, ready to evaluate on its features.
#[derive(Debug, Clone)]
pub struct BoundExpression(Expr);

/// A parsed `SELECT` statement: `SELECT fields FROM layer [WHERE ...] [ORDER BY field [ASC|DESC]]
/// [LIMIT n]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    /// The selected columns: `(field, alias)`. Empty means all fields (`*`).
    pub columns: Vec<(String, Option<String>)>,
    /// The layer to read.
    pub from: String,
    /// The `WHERE` clause.
    pub filter: Option<Expression>,
    /// `ORDER BY field`, with `true` for descending order.
    pub order_by: Option<(String, bool)>,
    /// `LIMIT n`.
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    /// A field by name (unbound).
    Field(String),
    /// A field by index in the schema (bound).
    Index(usize),
    /// The feature id.
    Fid,
    Literal(Value),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        escape: Option<char>,
        negated: bool,
        insensitive: bool,
    },
    In {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BinOp {
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
}

/// A value during evaluation.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
}

impl Value {
    fn from_field(v: &FieldValue) -> Value {
        match v {
            FieldValue::Integer(i) => Value::Int(*i),
            FieldValue::Float(f) => Value::Float(*f),
            FieldValue::Boolean(b) => Value::Int(*b as i64),
            FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => Value::Text(s.clone()),
            FieldValue::Blob(_) | FieldValue::Null => Value::Null,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Text(s) => s.trim().parse().ok(),
            Value::Null => None,
        }
    }

    fn to_text(&self) -> String {
        match self {
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Text(s) => s.clone(),
            Value::Null => String::new(),
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Text(s) => !s.is_empty(),
            Value::Null => false,
        }
    }
}

/// The result of evaluating a predicate, following OGR SQL: a truth value plus a null flag.
#[derive(Debug, Clone, Copy)]
struct Truth {
    value: bool,
    null: bool,
}

const NULL_TRUTH: Truth = Truth { value: false, null: true };

impl Truth {
    fn of(value: bool) -> Self {
        Truth { value, null: false }
    }
}

impl Expression {
    /// Parses an expression.
    pub fn parse(text: &str) -> Result<Self> {
        let tokens = tokenize(text)?;
        let mut p = Parser { tokens, pos: 0 };
        let e = p.expr()?;
        if p.pos < p.tokens.len() {
            return Err(syntax(format!("unexpected '{}'", p.tokens[p.pos])));
        }
        Ok(Expression(e))
    }

    /// The field names the expression refers to, in order of appearance (without `FID`).
    pub fn fields(&self) -> Vec<&str> {
        let mut out = Vec::new();
        collect_fields(&self.0, &mut out);
        out
    }

    /// Binds field names to a layer's schema, matching them case-insensitively. Fails when a field
    /// does not exist.
    pub fn bind(&self, schema: &Schema) -> Result<BoundExpression> {
        Ok(BoundExpression(bind(&self.0, schema)?))
    }
}

impl BoundExpression {
    /// Whether a feature satisfies the expression.
    pub fn matches(&self, feature: &Feature) -> bool {
        truth(&self.0, feature).value
    }
}

impl Select {
    /// Parses a `SELECT` statement.
    pub fn parse(text: &str) -> Result<Self> {
        let tokens = tokenize(text)?;
        let mut p = Parser { tokens, pos: 0 };
        p.expect_keyword("SELECT")?;
        let mut columns = Vec::new();
        if p.eat(&Token::Op("*")) {
        } else {
            loop {
                let name = p.identifier()?;
                let alias = if p.eat_keyword("AS") { Some(p.identifier()?) } else { None };
                columns.push((name, alias));
                if !p.eat(&Token::Comma) {
                    break;
                }
            }
        }
        p.expect_keyword("FROM")?;
        let from = p.identifier()?;
        let filter = if p.eat_keyword("WHERE") { Some(Expression(p.expr()?)) } else { None };
        let mut order_by = None;
        if p.eat_keyword("ORDER") {
            p.expect_keyword("BY")?;
            let field = p.identifier()?;
            let desc = if p.eat_keyword("DESC") {
                true
            } else {
                p.eat_keyword("ASC");
                false
            };
            order_by = Some((field, desc));
        }
        let mut limit = None;
        if p.eat_keyword("LIMIT") {
            match p.next() {
                Some(Token::Int(n)) if n >= 0 => limit = Some(n as usize),
                _ => return Err(syntax("LIMIT needs a non-negative integer".into())),
            }
        }
        if p.pos < p.tokens.len() {
            return Err(syntax(format!("unexpected '{}'", p.tokens[p.pos])));
        }
        Ok(Select { columns, from, filter, order_by, limit })
    }
}

fn syntax(msg: String) -> Error {
    Error::InvalidArgument(format!("SQL: {msg}"))
}

fn collect_fields<'a>(e: &'a Expr, out: &mut Vec<&'a str>) {
    match e {
        Expr::Field(name) => {
            if !out.contains(&name.as_str()) {
                out.push(name);
            }
        }
        Expr::Index(_) | Expr::Fid | Expr::Literal(_) => {}
        Expr::Not(a) | Expr::Neg(a) | Expr::IsNull { expr: a, .. } => collect_fields(a, out),
        Expr::Binary(_, a, b) | Expr::Like { expr: a, pattern: b, .. } => {
            collect_fields(a, out);
            collect_fields(b, out);
        }
        Expr::In { expr, list, .. } => {
            collect_fields(expr, out);
            list.iter().for_each(|x| collect_fields(x, out));
        }
        Expr::Between { expr, low, high, .. } => {
            collect_fields(expr, out);
            collect_fields(low, out);
            collect_fields(high, out);
        }
    }
}

/// The index of a field, matched case-insensitively.
pub(crate) fn field_index(schema: &Schema, name: &str) -> Option<usize> {
    schema.field_index(name).or_else(|| schema.fields().iter().position(|f| f.name.eq_ignore_ascii_case(name)))
}

fn bind(e: &Expr, schema: &Schema) -> Result<Expr> {
    let b = |x: &Expr| bind(x, schema).map(Box::new);
    Ok(match e {
        Expr::Field(name) => match field_index(schema, name) {
            Some(i) => Expr::Index(i),
            None if name.eq_ignore_ascii_case("FID") => Expr::Fid,
            None => return Err(Error::InvalidArgument(format!("SQL: no field named '{name}'"))),
        },
        Expr::Index(_) | Expr::Fid | Expr::Literal(_) => e.clone(),
        Expr::Not(a) => Expr::Not(b(a)?),
        Expr::Neg(a) => Expr::Neg(b(a)?),
        Expr::Binary(op, x, y) => Expr::Binary(*op, b(x)?, b(y)?),
        Expr::Like { expr, pattern, escape, negated, insensitive } => {
            Expr::Like { expr: b(expr)?, pattern: b(pattern)?, escape: *escape, negated: *negated, insensitive: *insensitive }
        }
        Expr::In { expr, list, negated } => {
            Expr::In { expr: b(expr)?, list: list.iter().map(|x| bind(x, schema)).collect::<Result<_>>()?, negated: *negated }
        }
        Expr::Between { expr, low, high, negated } => Expr::Between { expr: b(expr)?, low: b(low)?, high: b(high)?, negated: *negated },
        Expr::IsNull { expr, negated } => Expr::IsNull { expr: b(expr)?, negated: *negated },
    })
}

fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Text(_), _) | (_, Value::Text(_)) => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x.partial_cmp(&y),
            _ => Some(a.to_text().cmp(&b.to_text())),
        },
        _ => a.as_f64()?.partial_cmp(&b.as_f64()?),
    }
}

/// Evaluates a predicate the way OGR SQL does.
fn truth(e: &Expr, f: &Feature) -> Truth {
    match e {
        Expr::Binary(BinOp::And, a, b) => {
            let (x, y) = (truth(a, f), truth(b, f));
            Truth { value: x.value && y.value, null: x.null && y.null }
        }
        Expr::Binary(BinOp::Or, a, b) => {
            let (x, y) = (truth(a, f), truth(b, f));
            Truth { value: x.value || y.value, null: x.null || y.null }
        }
        Expr::Not(a) => {
            let x = truth(a, f);
            Truth { value: !x.value && !x.null, null: x.null }
        }
        Expr::Binary(op @ (BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge), a, b) => {
            let Some(ord) = compare(&eval(a, f), &eval(b, f)) else { return NULL_TRUTH };
            Truth::of(match op {
                BinOp::Eq => ord == Ordering::Equal,
                BinOp::Ne => ord != Ordering::Equal,
                BinOp::Lt => ord == Ordering::Less,
                BinOp::Le => ord != Ordering::Greater,
                BinOp::Gt => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            })
        }
        Expr::Like { expr, pattern, escape, negated, insensitive } => {
            let (v, p) = (eval(expr, f), eval(pattern, f));
            if v == Value::Null || p == Value::Null {
                return NULL_TRUTH;
            }
            let (v, p) = if *insensitive { (v.to_text().to_lowercase(), p.to_text().to_lowercase()) } else { (v.to_text(), p.to_text()) };
            Truth::of(like(&v, &p, *escape) != *negated)
        }
        Expr::In { expr, list, negated } => {
            let v = eval(expr, f);
            if v == Value::Null {
                return NULL_TRUTH;
            }
            let found = list.iter().any(|x| compare(&v, &eval(x, f)) == Some(Ordering::Equal));
            Truth::of(found != *negated)
        }
        Expr::Between { expr, low, high, negated } => {
            let v = eval(expr, f);
            match (compare(&v, &eval(low, f)), compare(&v, &eval(high, f))) {
                (Some(lo), Some(hi)) => Truth::of((lo != Ordering::Less && hi != Ordering::Greater) != *negated),
                _ => NULL_TRUTH,
            }
        }
        Expr::IsNull { expr, negated } => Truth::of((eval(expr, f) == Value::Null) != *negated),
        other => match eval(other, f) {
            Value::Null => NULL_TRUTH,
            v => Truth::of(v.truthy()),
        },
    }
}

fn eval(e: &Expr, f: &Feature) -> Value {
    match e {
        Expr::Literal(v) => v.clone(),
        Expr::Index(i) => f.attributes.get(*i).map(Value::from_field).unwrap_or(Value::Null),
        Expr::Fid => Value::Int(f.fid as i64),
        Expr::Field(_) => Value::Null,
        Expr::Neg(a) => match eval(a, f) {
            Value::Int(i) => Value::Int(-i),
            v => v.as_f64().map(|x| Value::Float(-x)).unwrap_or(Value::Null),
        },
        Expr::Binary(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod), a, b) => arith(*op, eval(a, f), eval(b, f)),
        Expr::Binary(BinOp::Concat, a, b) => match (eval(a, f), eval(b, f)) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (x, y) => Value::Text(format!("{}{}", x.to_text(), y.to_text())),
        },
        predicate => {
            let t = truth(predicate, f);
            if t.null { Value::Null } else { Value::Int(t.value as i64) }
        }
    }
}

fn arith(op: BinOp, a: Value, b: Value) -> Value {
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        let (x, y) = (*x, *y);
        return match op {
            BinOp::Add => Value::Int(x.wrapping_add(y)),
            BinOp::Sub => Value::Int(x.wrapping_sub(y)),
            BinOp::Mul => Value::Int(x.wrapping_mul(y)),
            BinOp::Div if y != 0 => Value::Int(x / y),
            BinOp::Mod if y != 0 => Value::Int(x % y),
            _ => Value::Null,
        };
    }
    let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) else { return Value::Null };
    match op {
        BinOp::Add => Value::Float(x + y),
        BinOp::Sub => Value::Float(x - y),
        BinOp::Mul => Value::Float(x * y),
        BinOp::Div if y != 0.0 => Value::Float(x / y),
        BinOp::Mod if y != 0.0 => Value::Float(x % y),
        _ => Value::Null,
    }
}

/// SQL `LIKE` matching: `%` matches any run of characters, `_` any one character.
fn like(text: &str, pattern: &str, escape: Option<char>) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum P {
        Any,
        One,
        Char(char),
    }
    let mut pat = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            if let Some(n) = chars.next() {
                pat.push(P::Char(n));
            }
        } else {
            pat.push(match c {
                '%' => P::Any,
                '_' => P::One,
                c => P::Char(c),
            });
        }
    }
    let text: Vec<char> = text.chars().collect();
    // Iterative wildcard matching with backtracking to the last `%`.
    let (mut t, mut p) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pat.len() && (pat[p] == P::One || pat[p] == P::Char(text[t])) {
            t += 1;
            p += 1;
        } else if p < pat.len() && pat[p] == P::Any {
            star = Some((p, t));
            p += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|x| *x == P::Any)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Quoted(String),
    Str(String),
    Int(i64),
    Float(f64),
    Op(&'static str),
    LParen,
    RParen,
    Comma,
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Ident(s) => write!(f, "{s}"),
            Token::Quoted(s) => write!(f, "\"{s}\""),
            Token::Str(s) => write!(f, "'{s}'"),
            Token::Int(i) => write!(f, "{i}"),
            Token::Float(x) => write!(f, "{x}"),
            Token::Op(o) => write!(f, "{o}"),
            Token::LParen => write!(f, "("),
            Token::RParen => write!(f, ")"),
            Token::Comma => write!(f, ","),
        }
    }
}

fn tokenize(text: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => out.push(Token::LParen),
            ')' => out.push(Token::RParen),
            ',' => out.push(Token::Comma),
            '\'' | '"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(syntax("unterminated quote".into())),
                        Some(&q) if q == c => {
                            if chars.get(i + 1) == Some(&c) {
                                s.push(c);
                                i += 2;
                            } else {
                                break;
                            }
                        }
                        Some(&ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                out.push(if c == '\'' { Token::Str(s) } else { Token::Quoted(s) });
            }
            c if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit())) => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                    let mut j = i + 1;
                    if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
                        j += 1;
                    }
                    if j < chars.len() && chars[j].is_ascii_digit() {
                        i = j;
                        while i < chars.len() && chars[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                let s: String = chars[start..i].iter().collect();
                out.push(match s.parse::<i64>() {
                    Ok(n) => Token::Int(n),
                    Err(_) => Token::Float(s.parse().map_err(|_| syntax(format!("bad number '{s}'")))?),
                });
                continue;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                    i += 1;
                }
                out.push(Token::Ident(chars[start..i].iter().collect()));
                continue;
            }
            _ => {
                let two: String = chars[i..chars.len().min(i + 2)].iter().collect();
                let op = ["<=", ">=", "<>", "!=", "==", "||"].into_iter().find(|o| *o == two);
                if let Some(op) = op {
                    out.push(Token::Op(if op == "==" { "=" } else { op }));
                    i += 2;
                    continue;
                }
                let op = ["=", "<", ">", "+", "-", "*", "/", "%"].into_iter().find(|o| o.starts_with(c));
                match op {
                    Some(op) => out.push(Token::Op(op)),
                    None => return Err(syntax(format!("unexpected character '{c}'"))),
                }
            }
        }
        i += 1;
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

const RESERVED: &[&str] =
    &["AND", "OR", "NOT", "IN", "LIKE", "ILIKE", "IS", "NULL", "BETWEEN", "ESCAPE", "FROM", "WHERE", "ORDER", "BY", "LIMIT", "AS"];

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn eat(&mut self, t: &Token) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn is_keyword(&self, offset: usize, kw: &str) -> bool {
        matches!(self.tokens.get(self.pos + offset), Some(Token::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_keyword(&mut self, kw: &str) -> bool {
        if self.is_keyword(0, kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        if self.eat_keyword(kw) { Ok(()) } else { Err(syntax(format!("expected {kw}"))) }
    }

    fn identifier(&mut self) -> Result<String> {
        match self.next() {
            Some(Token::Quoted(s)) => Ok(s),
            Some(Token::Ident(s)) if !RESERVED.iter().any(|k| s.eq_ignore_ascii_case(k)) => Ok(s),
            Some(t) => Err(syntax(format!("expected a name, found '{t}'"))),
            None => Err(syntax("expected a name".into())),
        }
    }

    fn expr(&mut self) -> Result<Expr> {
        let mut left = self.and()?;
        while self.eat_keyword("OR") {
            left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut left = self.not()?;
        while self.eat_keyword("AND") {
            left = Expr::Binary(BinOp::And, Box::new(left), Box::new(self.not()?));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.eat_keyword("NOT") { Ok(Expr::Not(Box::new(self.not()?))) } else { self.comparison() }
    }

    fn comparison(&mut self) -> Result<Expr> {
        let left = self.additive()?;
        let op = match self.peek() {
            Some(Token::Op("=")) => Some(BinOp::Eq),
            Some(Token::Op("!=" | "<>")) => Some(BinOp::Ne),
            Some(Token::Op("<")) => Some(BinOp::Lt),
            Some(Token::Op("<=")) => Some(BinOp::Le),
            Some(Token::Op(">")) => Some(BinOp::Gt),
            Some(Token::Op(">=")) => Some(BinOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.pos += 1;
            return Ok(Expr::Binary(op, Box::new(left), Box::new(self.additive()?)));
        }
        if self.eat_keyword("IS") {
            let negated = self.eat_keyword("NOT");
            self.expect_keyword("NULL")?;
            return Ok(Expr::IsNull { expr: Box::new(left), negated });
        }
        let negated = self.is_keyword(0, "NOT") && ["IN", "LIKE", "ILIKE", "BETWEEN"].iter().any(|k| self.is_keyword(1, k));
        if negated {
            self.pos += 1;
        }
        if self.eat_keyword("IN") {
            if !self.eat(&Token::LParen) {
                return Err(syntax("expected '(' after IN".into()));
            }
            let mut list = vec![self.additive()?];
            while self.eat(&Token::Comma) {
                list.push(self.additive()?);
            }
            if !self.eat(&Token::RParen) {
                return Err(syntax("expected ')' to close IN".into()));
            }
            return Ok(Expr::In { expr: Box::new(left), list, negated });
        }
        let insensitive = self.is_keyword(0, "ILIKE");
        if self.eat_keyword("LIKE") || self.eat_keyword("ILIKE") {
            let pattern = self.additive()?;
            let escape = if self.eat_keyword("ESCAPE") {
                match self.next() {
                    Some(Token::Str(s)) if s.chars().count() == 1 => s.chars().next(),
                    _ => return Err(syntax("ESCAPE needs a one-character string".into())),
                }
            } else {
                None
            };
            return Ok(Expr::Like { expr: Box::new(left), pattern: Box::new(pattern), escape, negated, insensitive });
        }
        if self.eat_keyword("BETWEEN") {
            let low = self.additive()?;
            self.expect_keyword("AND")?;
            let high = self.additive()?;
            return Ok(Expr::Between { expr: Box::new(left), low: Box::new(low), high: Box::new(high), negated });
        }
        if negated {
            return Err(syntax("expected IN, LIKE or BETWEEN after NOT".into()));
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op("+")) => BinOp::Add,
                Some(Token::Op("-")) => BinOp::Sub,
                Some(Token::Op("||")) => BinOp::Concat,
                _ => return Ok(left),
            };
            self.pos += 1;
            left = Expr::Binary(op, Box::new(left), Box::new(self.multiplicative()?));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op("*")) => BinOp::Mul,
                Some(Token::Op("/")) => BinOp::Div,
                Some(Token::Op("%")) => BinOp::Mod,
                _ => return Ok(left),
            };
            self.pos += 1;
            left = Expr::Binary(op, Box::new(left), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat(&Token::Op("-")) {
            return Ok(match self.unary()? {
                Expr::Literal(Value::Int(i)) => Expr::Literal(Value::Int(-i)),
                Expr::Literal(Value::Float(x)) => Expr::Literal(Value::Float(-x)),
                e => Expr::Neg(Box::new(e)),
            });
        }
        if self.eat(&Token::Op("+")) {
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.next() {
            Some(Token::Int(i)) => Ok(Expr::Literal(Value::Int(i))),
            Some(Token::Float(x)) => Ok(Expr::Literal(Value::Float(x))),
            Some(Token::Str(s)) => Ok(Expr::Literal(Value::Text(s))),
            Some(Token::Quoted(s)) => Ok(Expr::Field(s)),
            Some(Token::LParen) => {
                let e = self.expr()?;
                if !self.eat(&Token::RParen) {
                    return Err(syntax("expected ')'".into()));
                }
                Ok(e)
            }
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("NULL") => Ok(Expr::Literal(Value::Null)),
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("TRUE") => Ok(Expr::Literal(Value::Int(1))),
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("FALSE") => Ok(Expr::Literal(Value::Int(0))),
            Some(Token::Ident(s)) if !RESERVED.iter().any(|k| s.eq_ignore_ascii_case(k)) => Ok(Expr::Field(s)),
            Some(t) => Err(syntax(format!("unexpected '{t}'"))),
            None => Err(syntax("unexpected end of expression".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{FieldDef, FieldType};

    fn schema() -> Schema {
        let mut s = Schema::new();
        s.add_field(FieldDef::new("name", FieldType::Text));
        s.add_field(FieldDef::new("pop", FieldType::Integer));
        s.add_field(FieldDef::new("area", FieldType::Float));
        s
    }

    fn feature(name: Option<&str>, pop: Option<i64>, area: f64) -> Feature {
        let mut f = Feature::new();
        f.attributes = vec![
            name.map(|n| FieldValue::Text(n.into())).unwrap_or(FieldValue::Null),
            pop.map(FieldValue::Integer).unwrap_or(FieldValue::Null),
            FieldValue::Float(area),
        ];
        f
    }

    fn check(expr: &str, f: &Feature) -> bool {
        Expression::parse(expr).unwrap().bind(&schema()).unwrap().matches(f)
    }

    #[test]
    fn comparisons_and_logic() {
        let f = feature(Some("France"), Some(67_000_000), 551_695.5);
        assert!(check("pop > 1e6", &f));
        assert!(check("POP >= 67000000 AND name = 'France'", &f));
        assert!(!check("name = 'france'", &f));
        assert!(check("name <> 'Spain' or pop < 0", &f));
        assert!(check("NOT (pop < 10)", &f));
        assert!(check("area BETWEEN 500000 AND 600000", &f));
        assert!(check("area NOT BETWEEN 0 AND 1", &f));
        assert!(check("pop / 1000000 = 67", &f));
        assert!(check("-pop < 0 and \"Name\" || 'x' = 'Francex'", &f));
        assert!(check("fid = 0", &f));
    }

    #[test]
    fn in_like_null() {
        let f = feature(Some("Côte d'Ivoire"), None, 1.0);
        assert!(check("name IN ('Ghana', 'Côte d''Ivoire')", &f));
        assert!(check("name NOT IN ('Ghana')", &f));
        assert!(check("name LIKE 'C%Ivoire'", &f));
        assert!(!check("name LIKE 'c%IVOIRE'", &f));
        assert!(check("name LIKE '_ôte%'", &f));
        assert!(!check("name NOT LIKE '%d''I%'", &f));
        assert!(check("name ILIKE 'CÔTE%'", &f));
        assert!(check("pop IS NULL", &f));
        assert!(!check("pop IS NOT NULL", &f));
        // Comparisons with null are false, and so are their negations.
        assert!(!check("pop = 1", &f));
        assert!(!check("NOT (pop = 1)", &f));
        assert!(check("pop = 1 OR area = 1", &f));
        assert!(check("'50%' LIKE '50!%' ESCAPE '!'", &f));
    }

    #[test]
    fn errors() {
        assert!(Expression::parse("pop >").is_err());
        assert!(Expression::parse("name = 'x").is_err());
        assert!(Expression::parse("(pop > 1").is_err());
        assert!(Expression::parse("pop > 1 pop").is_err());
        assert!(Expression::parse("nosuch = 1").unwrap().bind(&schema()).is_err());
    }

    #[test]
    fn select() {
        let s = Select::parse("SELECT name, pop AS population FROM countries WHERE pop > 10 ORDER BY pop DESC LIMIT 5").unwrap();
        assert_eq!(s.columns, vec![("name".into(), None), ("pop".into(), Some("population".into()))]);
        assert_eq!(s.from, "countries");
        assert_eq!(s.order_by, Some(("pop".into(), true)));
        assert_eq!(s.limit, Some(5));
        assert_eq!(s.filter.unwrap().fields(), vec!["pop"]);
        let s = Select::parse("select * from \"my layer\"").unwrap();
        assert!(s.columns.is_empty());
        assert_eq!(s.from, "my layer");
    }
}
