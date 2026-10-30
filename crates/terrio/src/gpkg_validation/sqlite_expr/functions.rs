use super::*;

pub(super) fn known(name: &str) -> Result<()> {
    if matches!(
        name,
        "coalesce"
            | "ifnull"
            | "nullif"
            | "iif"
            | "if"
            | "abs"
            | "typeof"
            | "length"
            | "octet_length"
            | "lower"
            | "upper"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "substr"
            | "substring"
            | "instr"
            | "replace"
            | "hex"
            | "unhex"
            | "unicode"
            | "char"
            | "quote"
            | "min"
            | "max"
            | "sign"
            | "likely"
            | "unlikely"
            | "likelihood"
            | "json_valid"
            | "like"
            | "glob"
    ) {
        Ok(())
    } else {
        Err(Error::Unsupported(format!("SQLite function {name}")))
    }
}
fn arity(name: &str, actual: usize, min: usize, max: usize) -> Result<()> {
    if (min..=max).contains(&actual) { Ok(()) } else { Err(bad(&format!("wrong number of arguments to {name}"))) }
}
pub(super) fn validate(name: &str, args: &[Expr]) -> Result<()> {
    let (min, max) = match name {
        "coalesce" | "iif" | "if" | "min" | "max" => (2, usize::MAX),
        "ifnull" | "nullif" | "instr" | "glob" | "likelihood" => (2, 2),
        "char" => (0, usize::MAX),
        "trim" | "ltrim" | "rtrim" | "unhex" => (1, 2),
        "substr" | "substring" | "like" => (2, 3),
        "replace" => (3, 3),
        _ => (1, 1),
    };
    arity(name, args.len(), min, max)?;
    if name == "likelihood" && !matches!(args[1],Expr::Literal(Value::Real(p)) if (0.0..=1.0).contains(&p)) {
        return Err(bad("likelihood probability must be a floating-point literal from 0 to 1"));
    }
    Ok(())
}
pub(super) fn cast(value: &Value, kind: &str, encoding: u32) -> Result<Value> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    // SQLite interprets a BLOB cast using the database's text encoding.
    // Numeric casts only inspect a prefix, so malformed later code units need
    // not be representable as a Rust string. TEXT casts fail explicitly there.
    let decoded;
    let value = if let Value::Blob(bytes) = value
        && affinity(kind) != Affinity::Blob
    {
        let strict = affinity(kind) == Affinity::Text;
        let string = if matches!(encoding, 2 | 3) {
            let units: Vec<_> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| if encoding == 2 { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
                .collect();
            if strict {
                String::from_utf16(&units).map_err(|_| Error::Unsupported("non-Unicode SQLite UTF16 blob-to-text cast".into()))?
            } else {
                String::from_utf16_lossy(&units)
            }
        } else if strict {
            String::from_utf8(bytes.clone()).map_err(|_| Error::Unsupported("non-UTF8 SQLite blob-to-text cast".into()))?
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        };
        decoded = Value::Text(string);
        &decoded
    } else {
        value
    };
    if kind.contains("INT") {
        let result = match value {
            Value::Text(s) => integer_prefix(s),
            Value::Blob(b) => integer_prefix(&String::from_utf8_lossy(b)),
            _ => integer(value),
        };
        return Ok(Value::Int(result));
    }
    Ok(match affinity(kind) {
        Affinity::Text => Value::Text(text(value)?),
        Affinity::Blob => match value {
            Value::Blob(b) => Value::Blob(b.clone()),
            _ => {
                let text = text(value)?;
                Value::Blob(if matches!(encoding, 2 | 3) {
                    text.encode_utf16().flat_map(|u| if encoding == 2 { u.to_le_bytes() } else { u.to_be_bytes() }).collect()
                } else {
                    text.into_bytes()
                })
            }
        },
        Affinity::Real => real(float(value)),
        Affinity::Numeric => {
            if matches!(value, Value::Text(_) | Value::Blob(_)) {
                let value = arithmetic(value);
                if let Value::Real(r) = value {
                    if (-2_251_799_813_685_248.0..2_251_799_813_685_248.0).contains(&r)
                        && let Some(i) = metadata::integer(&value)
                    {
                        Value::Int(i)
                    } else {
                        value
                    }
                } else {
                    value
                }
            } else {
                value.clone()
            }
        }
    })
}
fn integer_prefix(s: &str) -> i64 {
    let s = s.split('\0').next().unwrap_or_default().trim_start_matches(|c: char| matches!(c, '\t'..='\r' | ' '));
    let b = s.as_bytes();
    let mut at = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let start = at;
    while b.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    if at == start {
        return 0;
    }
    s[..at].parse().unwrap_or_else(|_| if b.first() == Some(&b'-') { i64::MIN } else { i64::MAX })
}
pub(super) fn call(name: &str, args: &[Expr], ctx: &Context<'_>) -> Result<Value> {
    if matches!(name, "coalesce" | "ifnull") {
        arity(name, args.len(), 2, if name == "ifnull" { 2 } else { usize::MAX })?;
        for arg in args {
            let value = evaluate(arg, ctx)?.value;
            if !matches!(value, Value::Null) {
                return Ok(value);
            }
        }
        return Ok(Value::Null);
    }
    if matches!(name, "iif" | "if") {
        arity(name, args.len(), 2, usize::MAX)?;
        for pair in args.as_chunks::<2>().0 {
            if truth(&evaluate(&pair[0], ctx)?.value) == Some(true) {
                return Ok(evaluate(&pair[1], ctx)?.value);
            }
        }
        return if args.len() % 2 == 1 { Ok(evaluate(args.last().unwrap(), ctx)?.value) } else { Ok(Value::Null) };
    }
    let mut operands = args.iter().map(|arg| evaluate(arg, ctx)).collect::<Result<Vec<_>>>()?;
    if name == "nullif" {
        arity(name, args.len(), 2, 2)?;
        for operand in &mut operands {
            operand.affinity = None;
        }
        let explicit = args.iter().find_map(Expr::explicit_collation);
        return Ok(if compare(operands[0].clone(), operands[1].clone(), ctx.encoding, explicit)? == Some(Ordering::Equal) {
            Value::Null
        } else {
            operands.remove(0).value
        });
    }
    if matches!(name, "min" | "max") {
        arity(name, args.len(), 2, usize::MAX)?;
        if operands.iter().any(|v| matches!(v.value, Value::Null)) {
            return Ok(Value::Null);
        }
        for operand in &mut operands {
            operand.affinity = None;
        }
        let collation = operands.iter().find_map(|v| v.collation.as_deref());
        let mut chosen = 0;
        for index in 1..operands.len() {
            let order = compare(operands[index].clone(), operands[chosen].clone(), ctx.encoding, collation)?.unwrap();
            if order == if name == "min" { Ordering::Less } else { Ordering::Greater } || name == "min" && order == Ordering::Equal {
                chosen = index;
            }
        }
        return Ok(operands[chosen].value.clone());
    }
    let values: Vec<_> = operands.into_iter().map(|v| v.value).collect();
    let minmax = match name {
        "char" => (0, usize::MAX),
        "trim" | "ltrim" | "rtrim" | "unhex" => (1, 2),
        "substr" | "substring" | "like" => (2, 3),
        "instr" | "glob" | "likelihood" => (2, 2),
        "replace" => (3, 3),
        _ => (1, 1),
    };
    arity(name, values.len(), minmax.0, minmax.1)?;
    if name == "typeof" {
        return Ok(Value::Text(
            match values[0] {
                Value::Null => "null",
                Value::Int(_) => "integer",
                Value::Real(_) => "real",
                Value::Text(_) => "text",
                Value::Blob(_) => "blob",
            }
            .into(),
        ));
    }
    if name == "quote" {
        return Ok(Value::Text(match &values[0] {
            Value::Null => "NULL".into(),
            Value::Text(s) => format!("'{}'", s.split('\0').next().unwrap_or_default().replace('\'', "''")),
            Value::Blob(b) => format!("X'{}'", b.iter().map(|b| format!("{b:02X}")).collect::<String>()),
            v => text(v)?,
        }));
    }
    if name == "char" {
        let s: String = values.iter().map(|v| u32::try_from(integer(v)).ok().and_then(char::from_u32).unwrap_or('\u{fffd}')).collect();
        return Ok(Value::Text(s));
    }
    if matches!(name, "likely" | "unlikely" | "likelihood") {
        return Ok(values[0].clone());
    }
    if name == "hex" {
        let bytes = match &values[0] {
            Value::Blob(b) => b.clone(),
            v => text(v)?.into_bytes(),
        };
        return Ok(Value::Text(bytes.iter().map(|b| format!("{b:02X}")).collect()));
    }
    if name == "json_valid" {
        if matches!(values[0], Value::Null) {
            return Ok(Value::Null);
        }
        return Ok(Value::Int(i64::from(serde_json::from_str::<serde_json::Value>(&text(&values[0])?).is_ok())));
    }
    if values.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    Ok(match name {
        "abs" => match &values[0] {
            Value::Int(i) => Value::Int(i.checked_abs().ok_or_else(|| bad("integer overflow in abs"))?),
            value => real(float(value).abs()),
        },
        "sign" => {
            let value = match &values[0] {
                Value::Text(s) => integrity::numeric(s),
                Value::Blob(_) => None,
                v => Some(v.clone()),
            };
            value.map_or(Value::Null, |v| {
                Value::Int(if float(&v) > 0.0 {
                    1
                } else if float(&v) < 0.0 {
                    -1
                } else {
                    0
                })
            })
        }
        "length" => Value::Int(match &values[0] {
            Value::Blob(b) => b.len(),
            v => text(v)?.split('\0').next().unwrap_or_default().chars().count(),
        } as i64),
        "octet_length" => Value::Int(match &values[0] {
            Value::Blob(b) => b.len(),
            v => {
                let s = text(v)?;
                if matches!(ctx.encoding, 2 | 3) { s.encode_utf16().count() * 2 } else { s.len() }
            }
        } as i64),
        "lower" => Value::Text(if ctx.unicode_case { text(&values[0])?.to_lowercase() } else { text(&values[0])?.to_ascii_lowercase() }),
        "upper" => Value::Text(if ctx.unicode_case { text(&values[0])?.to_uppercase() } else { text(&values[0])?.to_ascii_uppercase() }),
        "trim" | "ltrim" | "rtrim" => {
            let s = text(&values[0])?;
            let chars = values.get(1).map(text).transpose()?.unwrap_or(" ".into());
            let trim = |c: char| chars.contains(c);
            Value::Text(
                if name == "ltrim" {
                    s.trim_start_matches(trim)
                } else if name == "rtrim" {
                    s.trim_end_matches(trim)
                } else {
                    s.trim_matches(trim)
                }
                .into(),
            )
        }
        "substr" | "substring" => {
            let start = integer(&values[1]);
            let length = values.get(2).map(integer);
            if let Value::Blob(bytes) = &values[0] {
                let (a, b) = slice(bytes.len(), start, length);
                Value::Blob(bytes[a..b].to_vec())
            } else {
                let s = text(&values[0])?;
                let chars: Vec<_> = s.split('\0').next().unwrap_or_default().chars().collect();
                let (a, b) = slice(chars.len(), start, length);
                Value::Text(chars[a..b].iter().collect())
            }
        }
        "instr" => {
            if let (Value::Blob(a), Value::Blob(b)) = (&values[0], &values[1]) {
                Value::Int(if b.is_empty() { 1 } else { a.windows(b.len()).position(|w| w == b).map_or(0, |i| i as i64 + 1) })
            } else {
                let a = text(&values[0])?;
                let b = text(&values[1])?;
                Value::Int(a.find(&b).map_or(0, |i| a[..i].chars().count() as i64 + 1))
            }
        }
        "replace" => {
            let a = text(&values[0])?;
            let b = text(&values[1])?;
            let c = text(&values[2])?;
            Value::Text(if b.is_empty() { a } else { a.replace(&b, &c) })
        }
        "unicode" => text(&values[0])?.chars().next().map_or(Value::Null, |c| Value::Int(c as i64)),
        "unhex" => {
            let s = text(&values[0])?;
            let ignore = values.get(1).map(text).transpose()?.unwrap_or_default();
            let mut bytes = Vec::new();
            let mut digit = None;
            for c in s.chars() {
                if let Some(value) = c.to_digit(16).filter(|_| c.is_ascii()) {
                    if let Some(high) = digit.take() {
                        bytes.push((high * 16 + value) as u8);
                    } else {
                        digit = Some(value);
                    }
                } else if digit.is_some() || !ignore.contains(c) {
                    return Ok(Value::Null);
                }
            }
            if digit.is_some() { Value::Null } else { Value::Blob(bytes) }
        }
        "like" | "glob" => Value::Int(i64::from(pattern(
            &text(&values[1])?,
            &text(&values[0])?,
            values.get(2).map(text).transpose()?.as_deref(),
            name == "glob",
            ctx.unicode_case,
        )?)),
        _ => return Err(Error::Unsupported(format!("SQLite function {name}"))),
    })
}
fn slice(size: usize, start: i64, length: Option<i64>) -> (usize, usize) {
    let size = size as i128;
    let start = start as i128;
    let mut position = if start > 0 {
        start - 1
    } else if start < 0 {
        size + start
    } else {
        0
    };
    if length.is_none() {
        return (position.clamp(0, size) as usize, size as usize);
    }
    let length = length.map(i128::from).unwrap_or(size + 1);
    let (a, b) = if length < 0 {
        (position + length, position)
    } else {
        if start == 0 {
            position = -1;
        }
        (position, position + length)
    };
    (a.clamp(0, size) as usize, b.clamp(0, size).max(a.clamp(0, size)) as usize)
}
#[derive(Clone)]
enum Pattern {
    Star,
    Any,
    Char(char),
    Class(Vec<(char, char)>, bool),
}
// LIKE with ICU uses simple one-code-point case folding, not expansions such
// as ß -> ss. The representative need only preserve the equivalence classes.
fn fold(c: char) -> char {
    if c == '\u{131}' {
        return c;
    }
    let mut upper = c.to_uppercase();
    let first = upper.next().unwrap();
    let upper = if upper.next().is_none() { first } else { c };
    let mut lower = upper.to_lowercase();
    let first = lower.next().unwrap();
    if lower.next().is_none() { first } else { c }
}
pub(super) fn pattern(value: &str, pattern: &str, escape: Option<&str>, glob: bool, unicode: bool) -> Result<bool> {
    if pattern.len() > 50_000 {
        return Err(bad("LIKE/GLOB pattern exceeds SQLite's 50000-byte limit"));
    }
    let escape = escape
        .map(|s| {
            let mut chars = s.chars();
            let ch = chars.next().ok_or_else(|| bad("ESCAPE must be one character"))?;
            if chars.next().is_some() {
                return Err(bad("ESCAPE must be one character"));
            }
            Ok(ch)
        })
        .transpose()?;
    let chars: Vec<_> = pattern.split('\0').next().unwrap_or_default().chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        at += 1;
        if Some(c) == escape && !glob {
            if at == chars.len() {
                return Ok(false);
            }
            tokens.push(Pattern::Char(chars[at]));
            at += 1;
        } else if c == if glob { '*' } else { '%' } {
            tokens.push(Pattern::Star);
        } else if c == if glob { '?' } else { '_' } {
            tokens.push(Pattern::Any);
        } else if glob && c == '[' {
            let negated = chars.get(at) == Some(&'^');
            if negated {
                at += 1;
            }
            let mut ranges = Vec::new();
            if chars.get(at) == Some(&']') {
                ranges.push((']', ']'));
                at += 1;
            }
            while at < chars.len() && chars[at] != ']' {
                let first = chars[at];
                at += 1;
                if chars.get(at) == Some(&'-') && chars.get(at + 1).is_some_and(|c| *c != ']') {
                    ranges.push((first, chars[at + 1]));
                    at += 2;
                } else {
                    ranges.push((first, first));
                }
            }
            if at == chars.len() {
                return Ok(false);
            }
            at += 1;
            tokens.push(Pattern::Class(ranges, negated));
        } else {
            tokens.push(Pattern::Char(c));
        }
    }
    let mut states = vec![false; tokens.len() + 1];
    states[0] = true;
    for i in 0..tokens.len() {
        if matches!(tokens[i], Pattern::Star) && states[i] {
            states[i + 1] = true;
        }
    }
    for ch in value.split('\0').next().unwrap_or_default().chars() {
        let mut next = vec![false; states.len()];
        for (i, token) in tokens.iter().enumerate() {
            if !states[i] {
                continue;
            }
            match token {
                Pattern::Star => next[i] = true,
                Pattern::Any => next[i + 1] = true,
                Pattern::Char(c) => {
                    next[i + 1] = if glob {
                        *c == ch
                    } else if unicode {
                        fold(*c) == fold(ch)
                    } else {
                        c.eq_ignore_ascii_case(&ch)
                    }
                }
                Pattern::Class(ranges, not) => next[i + 1] = ranges.iter().any(|(a, b)| *a <= ch && ch <= *b) ^ *not,
            }
        }
        for i in 0..tokens.len() {
            if matches!(tokens[i], Pattern::Star) && next[i] {
                next[i + 1] = true;
            }
        }
        states = next;
        if !states.iter().any(|state| *state) {
            return Ok(false);
        }
    }
    Ok(*states.last().unwrap())
}
