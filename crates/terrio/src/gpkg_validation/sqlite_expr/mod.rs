//! Read-only SQLite scalar expressions for integrity checks and view projections.
//! SQLite affinity, collation and three-valued logic are intentionally separate
//! from Terrio's OGR SQL dialect. Unknown functions/operators fail explicitly.
mod compound;
mod functions;
mod joins;
mod lex;
mod order;
mod parse;
mod rows;
mod select;
pub(super) use select::View;
#[cfg(all(test, not(target_family = "wasm")))]
mod tests;
use super::*;
pub(super) use rows::RowEvaluator;
use std::cmp::Ordering;

fn bad(message: &str) -> Error {
    Error::InvalidData(format!("SQLite expression: {message}"))
}
#[derive(Debug, Clone, PartialEq)]
enum Expr {
    MinMagnitude,
    Literal(Value),
    Column(Vec<String>, u8),
    Unary(String, Box<Expr>),
    Binary(String, Box<Expr>, Box<Expr>),
    Collate(Box<Expr>, String),
    Cast(Box<Expr>, String),
    Using(Box<Expr>, Box<Expr>, String),
    Call(String, Vec<Expr>),
    Between(Box<Expr>, Box<Expr>, Box<Expr>, bool),
    In(Box<Expr>, Vec<Expr>, bool),
    Pattern(Box<Expr>, Box<Expr>, Option<Box<Expr>>, bool, bool),
    Case(Option<Box<Expr>>, Vec<(Expr, Expr)>, Box<Expr>),
}
impl Expr {
    fn children(&self) -> Vec<&Expr> {
        match self {
            Self::MinMagnitude | Self::Literal(_) | Self::Column(..) => vec![],
            Self::Unary(_, a) | Self::Collate(a, _) | Self::Cast(a, _) => vec![a],
            Self::Binary(_, a, b) | Self::Using(a, b, _) => vec![a, b],
            Self::Call(_, args) => args.iter().collect(),
            Self::Between(a, b, c, _) => vec![a, b, c],
            Self::In(a, list, _) => std::iter::once(a.as_ref()).chain(list).collect(),
            Self::Pattern(a, b, escape, ..) => {
                let mut out = vec![a.as_ref(), b.as_ref()];
                if let Some(e) = escape {
                    out.push(e);
                }
                out
            }
            Self::Case(base, arms, otherwise) => base
                .iter()
                .map(Box::as_ref)
                .chain(arms.iter().flat_map(|(a, b)| [a, b]))
                .chain(std::iter::once(otherwise.as_ref()))
                .collect(),
        }
    }
    fn children_mut(&mut self) -> Vec<&mut Expr> {
        match self {
            Self::MinMagnitude | Self::Literal(_) | Self::Column(..) => vec![],
            Self::Unary(_, a) | Self::Collate(a, _) | Self::Cast(a, _) => vec![a],
            Self::Binary(_, a, b) | Self::Using(a, b, _) => vec![a, b],
            Self::Call(_, args) => args.iter_mut().collect(),
            Self::Between(a, b, c, _) => vec![a, b, c],
            Self::In(a, args, _) => std::iter::once(a.as_mut()).chain(args.iter_mut()).collect(),
            Self::Pattern(a, b, escape, ..) => {
                let mut out = vec![a.as_mut(), b.as_mut()];
                if let Some(e) = escape {
                    out.push(e);
                }
                out
            }
            Self::Case(base, arms, otherwise) => base
                .iter_mut()
                .map(Box::as_mut)
                .chain(arms.iter_mut().flat_map(|(a, b)| [a, b]))
                .chain(std::iter::once(otherwise.as_mut()))
                .collect(),
        }
    }
    fn explicit_collation(&self) -> Option<&str> {
        if let Self::Collate(_, collation) = self {
            return Some(collation);
        }
        self.children().into_iter().find_map(Self::explicit_collation)
    }
}
#[derive(Debug, Clone)]
pub(super) struct Expression(Expr);
pub(super) struct Context<'a> {
    pub columns: &'a [schema::Column],
    pub row: &'a [Value],
    pub rowid: Option<i64>,
    pub table: Option<&'a str>,
    pub encoding: u32,
    pub strict: bool,
    /// ICU-style Unicode case handling, as in the GDAL reference build.
    /// False selects stock SQLite's ASCII case handling.
    pub unicode_case: bool,
    /// True when the reader has already applied physical REAL read conversion.
    pub logical: bool,
}
#[derive(Clone, Copy, PartialEq)]
enum Affinity {
    Numeric,
    Real,
    Text,
    Blob,
}
fn affinity(kind: &str) -> Affinity {
    if kind.contains("INT") {
        Affinity::Numeric
    } else if ["CHAR", "CLOB", "TEXT"].iter().any(|s| kind.contains(s)) {
        Affinity::Text
    } else if kind.is_empty() || kind.contains("BLOB") {
        Affinity::Blob
    } else if ["REAL", "FLOA", "DOUB"].iter().any(|s| kind.contains(s)) {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}
pub(super) fn has_real_affinity(kind: &str) -> bool {
    affinity(kind) == Affinity::Real
}
#[derive(Clone)]
struct Operand {
    value: Value,
    affinity: Option<Affinity>,
    collation: Option<String>,
}
impl Operand {
    fn scalar(value: Value) -> Self {
        Self { value, affinity: None, collation: None }
    }
}
fn real(value: f64) -> Value {
    if value.is_nan() { Value::Null } else { Value::Real(value) }
}
fn text(value: &Value) -> Result<String> {
    Ok(match value {
        Value::Null => String::new(),
        Value::Int(value) => value.to_string(),
        Value::Real(value) => integrity::real_text(*value),
        Value::Text(value) => value.clone(),
        Value::Blob(bytes) => {
            String::from_utf8(bytes.clone()).map_err(|_| Error::Unsupported("non-UTF8 SQLite blob-to-text conversion".into()))?
        }
    })
}
fn apply(value: Value, affinity: Affinity) -> Result<Value> {
    Ok(match (affinity, value) {
        (Affinity::Numeric | Affinity::Real, Value::Text(s)) => integrity::numeric(&s).unwrap_or(Value::Text(s)),
        (Affinity::Text, v @ Value::Int(_)) | (Affinity::Text, v @ Value::Real(_)) => Value::Text(text(&v)?),
        (_, value) => value,
    })
}
/// Apply a declared column's affinity to a generated value, then SQLite's REAL
/// read conversion. INTEGER affinity never truncates a fractional value.
pub(super) fn column_value(value: Value, column: &schema::Column) -> Result<Value> {
    let kind = affinity(&column.data_type);
    let value = apply(value, kind)?;
    Ok(
        if kind == Affinity::Real
            && let Value::Int(value) = value
        {
            Value::Real(value as f64)
        } else {
            value
        },
    )
}
fn arithmetic(value: &Value) -> Value {
    match value {
        Value::Null => Value::Null,
        Value::Int(_) | Value::Real(_) => value.clone(),
        Value::Text(s) => numeric_prefix(s),
        Value::Blob(bytes) => numeric_prefix(&String::from_utf8_lossy(bytes)),
    }
}
fn numeric_prefix(text: &str) -> Value {
    let s = text.split('\0').next().unwrap_or_default().trim_start_matches(|c: char| matches!(c, '\t'..='\r' | ' '));
    let b = s.as_bytes();
    let mut at = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let mut digits = 0;
    while b.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
        digits += 1;
    }
    if b.get(at) == Some(&b'.') {
        at += 1;
        while b.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return Value::Int(0);
    }
    if matches!(b.get(at), Some(b'e' | b'E')) {
        let before = at;
        at += 1;
        if matches!(b.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let start = at;
        while b.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if start == at {
            at = before;
        }
    }
    let s = &s[..at];
    let value = integrity::numeric(s).unwrap_or(Value::Int(0));
    if s.contains(['.', 'e', 'E'])
        && let Value::Int(value) = value
    {
        Value::Real(value as f64)
    } else {
        value
    }
}
pub(super) fn truth(value: &Value) -> Option<bool> {
    match arithmetic(value) {
        Value::Null => None,
        Value::Int(i) => Some(i != 0),
        Value::Real(r) => Some(r != 0.0),
        _ => unreachable!(),
    }
}
fn boolean(value: Option<bool>) -> Value {
    value.map_or(Value::Null, |v| Value::Int(i64::from(v)))
}
fn integer(value: &Value) -> i64 {
    match arithmetic(value) {
        Value::Int(i) => i,
        Value::Real(r) => r as i64,
        _ => 0,
    }
}
fn float(value: &Value) -> f64 {
    match arithmetic(value) {
        Value::Int(i) => i as f64,
        Value::Real(r) => r,
        _ => 0.0,
    }
}
fn compare(mut a: Operand, mut b: Operand, encoding: u32, explicit: Option<&str>) -> Result<Option<Ordering>> {
    let numeric = |a| matches!(a, Some(Affinity::Numeric | Affinity::Real));
    if numeric(a.affinity) && !numeric(b.affinity) {
        b.value = apply(b.value, Affinity::Numeric)?;
    } else if numeric(b.affinity) && !numeric(a.affinity) {
        a.value = apply(a.value, Affinity::Numeric)?;
    } else if a.affinity == Some(Affinity::Text) && matches!(b.affinity, None | Some(Affinity::Blob)) {
        b.value = apply(b.value, Affinity::Text)?;
    } else if b.affinity == Some(Affinity::Text) && matches!(a.affinity, None | Some(Affinity::Blob)) {
        a.value = apply(a.value, Affinity::Text)?;
    }
    if let (Value::Text(a_text), Value::Text(b_text)) = (&a.value, &b.value) {
        let collation = explicit.or(a.collation.as_deref()).or(b.collation.as_deref()).unwrap_or("BINARY");
        let key = |s: &str| -> (Vec<u8>, usize) {
            match collation {
                "NOCASE" => (s.split('\0').next().unwrap_or_default().as_bytes().to_ascii_lowercase(), s.len()),
                "RTRIM" => (s.trim_end_matches(' ').as_bytes().to_vec(), 0),
                _ if matches!(encoding, 2 | 3) => {
                    (s.encode_utf16().flat_map(|u| if encoding == 2 { u.to_le_bytes() } else { u.to_be_bytes() }).collect(), 0)
                }
                _ => (s.as_bytes().to_vec(), 0),
            }
        };
        return Ok(Some(key(a_text).cmp(&key(b_text))));
    }
    Ok(schema_extension::compare(&a.value, &b.value))
}
fn math(op: &str, a: Value, b: Value) -> Value {
    let (a, b) = (arithmetic(&a), arithmetic(&b));
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    if matches!(op, "&" | "|" | "<<" | ">>" | "%") {
        let (x, y) = (integer(&a), integer(&b));
        let result = match op {
            "&" => x & y,
            "|" => x | y,
            "%" => {
                if y == 0 {
                    return Value::Null;
                }
                if y == -1 { 0 } else { x % y }
            }
            _ => {
                let left = (op == "<<") ^ (y < 0);
                let shift = y.unsigned_abs();
                if shift >= 64 {
                    if left || x >= 0 { 0 } else { -1 }
                } else if left {
                    (x as u64).wrapping_shl(shift as u32) as i64
                } else {
                    x >> shift
                }
            }
        };
        return if op == "%" && (matches!(a, Value::Real(_)) || matches!(b, Value::Real(_))) {
            Value::Real(result as f64)
        } else {
            Value::Int(result)
        };
    }
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        let result = match op {
            "+" => x.checked_add(*y),
            "-" => x.checked_sub(*y),
            "*" => x.checked_mul(*y),
            "/" => {
                if *y == 0 {
                    return Value::Null;
                }
                x.checked_div(*y)
            }
            _ => None,
        };
        if let Some(value) = result {
            return Value::Int(value);
        }
    }
    let (x, y) = (float(&a), float(&b));
    real(match op {
        "+" => x + y,
        "-" => x - y,
        "*" => x * y,
        "/" => {
            if y == 0.0 {
                return Value::Null;
            }
            x / y
        }
        _ => unreachable!(),
    })
}
impl Expression {
    /// Index key collation comes only from the outer COLLATE or a bare column.
    /// SQLite does not propagate operand collations through index expressions.
    pub(super) fn index_column(&self, columns: &[schema::Column]) -> (Option<usize>, String) {
        fn column(expr: &Expr, columns: &[schema::Column]) -> Option<usize> {
            match expr {
                Expr::Collate(inner, _) => column(inner, columns),
                Expr::Column(parts, _) => columns.iter().position(|c| c.name.eq_ignore_ascii_case(parts.last().unwrap())),
                _ => None,
            }
        }
        let index = column(&self.0, columns);
        let collation = match &self.0 {
            Expr::Collate(_, name) => name.clone(),
            Expr::Column(..) => index.map_or_else(|| "BINARY".into(), |i| columns[i].collation.clone()),
            _ => "BINARY".into(),
        };
        (index, collation)
    }

    pub(super) fn index_term(sql: &str) -> Result<(Self, bool)> {
        parse::Parser::index_term(sql).map(|(expr, descending)| (Self(expr), descending))
    }
    pub(super) fn parse(sql: &str) -> Result<Self> {
        parse::Parser::parse(sql).map(Self)
    }
    pub(super) fn evaluate(&self, ctx: &Context<'_>) -> Result<Value> {
        Ok(evaluate(&self.0, ctx)?.value)
    }
    pub(super) fn check(&self, ctx: &Context<'_>) -> Result<bool> {
        Ok(truth(&self.evaluate(ctx)?) != Some(false))
    }
}
fn evaluate(expr: &Expr, ctx: &Context<'_>) -> Result<Operand> {
    let eval = |e: &Expr| evaluate(e, ctx);
    let scalar = Operand::scalar;
    let value = match expr {
        Expr::MinMagnitude => scalar(Value::Real(9_223_372_036_854_775_808.0)),
        Expr::Literal(value) => scalar(value.clone()),
        Expr::Column(parts, quoted) => {
            let name = parts.last().unwrap();
            if parts.len() > 1 && !ctx.table.is_some_and(|t| t.eq_ignore_ascii_case(&parts[parts.len() - 2])) {
                return Err(bad("unknown table qualifier"));
            }
            if let Some(index) = ctx.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)) {
                let column = &ctx.columns[index];
                let mut value = ctx.row.get(index).cloned().unwrap_or(Value::Null);
                if !ctx.logical
                    && affinity(&column.data_type) == Affinity::Real
                    && let Value::Int(i) = value
                {
                    value = Value::Real(i as f64);
                }
                Operand {
                    value,
                    affinity: Some(if ctx.strict && column.data_type == "ANY" { Affinity::Blob } else { affinity(&column.data_type) }),
                    collation: Some(column.collation.clone()),
                }
            } else if *quoted == 0 && parts.len() == 1 && name.eq_ignore_ascii_case("true") {
                scalar(Value::Int(1))
            } else if *quoted == 0 && parts.len() == 1 && name.eq_ignore_ascii_case("false") {
                scalar(Value::Int(0))
            } else if ["rowid", "_rowid_", "oid"].iter().any(|s| name.eq_ignore_ascii_case(s))
                && let Some(id) = ctx.rowid
            {
                scalar(Value::Int(id))
            } else if *quoted == 2 && parts.len() == 1 {
                scalar(Value::Text(name.clone()))
            } else {
                return Err(bad(&format!("no column named {name}")));
            }
        }
        Expr::Using(left, right, _) => {
            let mut source = eval(left)?;
            if matches!(source.value, Value::Null) {
                source.value = eval(right)?.value;
            }
            source
        }
        Expr::Collate(e, _) => eval(e)?,
        Expr::Cast(e, kind) => {
            let source = eval(e)?;
            let value = functions::cast(&source.value, kind, ctx.encoding)?;
            Operand { value, affinity: Some(affinity(kind)), collation: source.collation }
        }
        Expr::Unary(op, e) => {
            let mut value = eval(e)?;
            if op == "+" {
                value.affinity = None;
                value
            } else {
                scalar(match op.as_str() {
                    "NOT" => boolean(truth(&value.value).map(|v| !v)),
                    "~" => {
                        if matches!(value.value, Value::Null) {
                            Value::Null
                        } else {
                            Value::Int(!integer(&value.value))
                        }
                    }
                    _ => match arithmetic(&value.value) {
                        Value::Int(i) => i.checked_neg().map_or_else(|| real(-(i as f64)), Value::Int),
                        Value::Real(r) => real(-r),
                        _ => Value::Null,
                    },
                })
            }
        }
        Expr::Binary(op, a, b) => {
            let left = eval(a)?;
            if op == "AND" && truth(&left.value) == Some(false) {
                return Ok(scalar(Value::Int(0)));
            }
            if op == "OR" && truth(&left.value) == Some(true) {
                return Ok(scalar(Value::Int(1)));
            }
            let right = eval(b)?;
            let explicit = a.explicit_collation().or_else(|| b.explicit_collation());
            scalar(match op.as_str() {
                "AND" | "OR" => {
                    let (a, b) = (truth(&left.value), truth(&right.value));
                    boolean(if op == "AND" {
                        if a == Some(false) || b == Some(false) { Some(false) } else { a.zip(b).map(|(a, b)| a && b) }
                    } else if a == Some(true) || b == Some(true) {
                        Some(true)
                    } else {
                        a.zip(b).map(|(a, b)| a || b)
                    })
                }
                "||" => {
                    if matches!(left.value, Value::Null) || matches!(right.value, Value::Null) {
                        Value::Null
                    } else {
                        Value::Text(format!("{}{}", text(&left.value)?, text(&right.value)?))
                    }
                }
                "=" | "==" | "<>" | "!=" | "<" | ">" | "<=" | ">=" | "IS" | "IS NOT" => {
                    let is = matches!(op.as_str(), "IS" | "IS NOT");
                    let special_bool = if is
                        && let Expr::Column(parts, 0) = b.as_ref()
                        && parts.len() == 1
                        && !ctx.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&parts[0]))
                    {
                        if parts[0].eq_ignore_ascii_case("true") {
                            Some(true)
                        } else if parts[0].eq_ignore_ascii_case("false") {
                            Some(false)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(expected) = special_bool {
                        boolean(Some((truth(&left.value) == Some(expected)) ^ (op == "IS NOT")))
                    } else if is && (matches!(left.value, Value::Null) || matches!(right.value, Value::Null)) {
                        boolean(Some((matches!(left.value, Value::Null) && matches!(right.value, Value::Null)) ^ (op == "IS NOT")))
                    } else {
                        boolean(compare(left, right, ctx.encoding, explicit)?.map(|order| match op.as_str() {
                            "=" | "==" | "IS" => order == Ordering::Equal,
                            "<>" | "!=" | "IS NOT" => order != Ordering::Equal,
                            "<" => order == Ordering::Less,
                            ">" => order == Ordering::Greater,
                            "<=" => order != Ordering::Greater,
                            _ => order != Ordering::Less,
                        }))
                    }
                }
                _ => math(op, left.value, right.value),
            })
        }
        Expr::Between(a, low, high, not) => {
            let value = eval(a)?;
            let low_value = eval(low)?;
            let high_value = eval(high)?;
            let ge = compare(value.clone(), low_value, ctx.encoding, a.explicit_collation().or_else(|| low.explicit_collation()))?
                .map(|o| o != Ordering::Less);
            let le = compare(value, high_value, ctx.encoding, a.explicit_collation().or_else(|| high.explicit_collation()))?
                .map(|o| o != Ordering::Greater);
            let result = if ge == Some(false) || le == Some(false) { Some(false) } else { ge.zip(le).map(|(a, b)| a && b) };
            scalar(boolean(result.map(|v| v ^ *not)))
        }
        Expr::In(a, list, not) => {
            let left = eval(a)?;
            let mut result = Some(false);
            for item in list {
                let mut right = eval(item)?;
                right.affinity = None;
                match compare(left.clone(), right, ctx.encoding, a.explicit_collation().or_else(|| item.explicit_collation()))? {
                    Some(Ordering::Equal) => {
                        result = Some(true);
                        break;
                    }
                    None => result = None,
                    _ => {}
                }
            }
            scalar(boolean(result.map(|v| v ^ *not)))
        }
        Expr::Pattern(a, b, escape, glob, not) => {
            let (a, b) = (eval(a)?.value, eval(b)?.value);
            if matches!(a, Value::Null) || matches!(b, Value::Null) {
                scalar(Value::Null)
            } else {
                let escape = escape.as_ref().map(|e| eval(e).map(|v| v.value)).transpose()?;
                if matches!(escape, Some(Value::Null)) {
                    scalar(Value::Null)
                } else {
                    scalar(Value::Int(i64::from(
                        functions::pattern(
                            &text(&a)?,
                            &text(&b)?,
                            escape.as_ref().map(text).transpose()?.as_deref(),
                            *glob,
                            ctx.unicode_case,
                        )? ^ *not,
                    )))
                }
            }
        }
        Expr::Case(base, arms, otherwise) => {
            let base = base.as_ref().map(|e| eval(e)).transpose()?;
            let mut chosen = None;
            for (condition, value) in arms {
                let condition_value = eval(condition)?;
                let matches = if let Some(base) = &base {
                    compare(base.clone(), condition_value, ctx.encoding, expr.explicit_collation())? == Some(Ordering::Equal)
                } else {
                    truth(&condition_value.value) == Some(true)
                };
                if matches {
                    chosen = Some(value);
                    break;
                }
            }
            scalar(eval(chosen.unwrap_or(otherwise))?.value)
        }
        Expr::Call(name, args) => scalar(functions::call(name, args, ctx)?),
    };
    Ok(if let Some(collation) = expr.explicit_collation() { Operand { collation: Some(collation.into()), ..value } } else { value })
}
