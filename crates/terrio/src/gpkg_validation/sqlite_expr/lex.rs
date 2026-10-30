use super::*;

#[derive(Debug, Clone)]
pub(super) enum Token {
    Name(String, u8),
    Literal(Value, bool),
    Symbol(String),
}
impl Token {
    pub fn is(&self, text: &str) -> bool {
        match self {
            Self::Name(name, 0) | Self::Symbol(name) => name.eq_ignore_ascii_case(text),
            _ => false,
        }
    }
}
pub(super) struct Spanned {
    pub tokens: Vec<Token>,
    pub spans: Vec<(usize, usize)>,
}
pub(super) fn tokens(sql: &str) -> Result<Vec<Token>> {
    spanned(sql).map(|result| result.tokens)
}
pub(super) fn spanned(sql: &str) -> Result<Spanned> {
    let mut spans = Vec::new();
    let bytes = sql.as_bytes();
    let mut at = 0;
    let mut out = Vec::new();
    while at < bytes.len() {
        let byte = bytes[at];
        if byte.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if bytes[at..].starts_with(b"--") {
            at += bytes[at..].iter().position(|b| *b == b'\n').unwrap_or(bytes.len() - at);
            continue;
        }
        if bytes[at..].starts_with(b"/*") {
            at += 2 + bytes[at + 2..].windows(2).position(|p| p == b"*/").ok_or_else(|| bad("unterminated comment"))? + 2;
            continue;
        }
        let token_start = at;
        let blob = matches!(byte, b'x' | b'X') && bytes.get(at + 1) == Some(&b'\'');
        if blob || matches!(byte, b'\'' | b'"' | b'`' | b'[') {
            if blob {
                at += 1;
            }
            let open = bytes[at];
            let close = if open == b'[' { b']' } else { open };
            at += 1;
            let mut text = Vec::new();
            loop {
                let b = *bytes.get(at).ok_or_else(|| bad("unterminated quote"))?;
                at += 1;
                if b == close {
                    if close != b']' && bytes.get(at) == Some(&close) {
                        text.push(close);
                        at += 1;
                    } else {
                        break;
                    }
                } else {
                    text.push(b);
                }
            }
            let text = String::from_utf8(text).map_err(|_| bad("invalid UTF-8 SQL"))?;
            if blob {
                if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(bad("invalid blob literal"));
                }
                out.push(Token::Literal(
                    Value::Blob((0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()),
                    false,
                ));
            } else if open == b'\'' {
                out.push(Token::Literal(Value::Text(text), false));
            } else {
                out.push(Token::Name(text, if open == b'"' { 2 } else { 1 }));
            }
            spans.push((token_start, at));
            continue;
        }
        if byte.is_ascii_digit() || byte == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit) {
            let start = at;
            if bytes[at..].starts_with(b"0x") || bytes[at..].starts_with(b"0X") {
                at += 2;
                let first = at;
                while bytes.get(at).is_some_and(|b| b.is_ascii_hexdigit() || *b == b'_') {
                    at += 1;
                }
                let text = sql[first..at].replace('_', "");
                for (i, byte) in bytes[first..at].iter().enumerate() {
                    if *byte == b'_'
                        && (i == 0
                            || first + i + 1 == at
                            || !bytes[first + i - 1].is_ascii_hexdigit()
                            || !bytes[first + i + 1].is_ascii_hexdigit())
                    {
                        return Err(bad("invalid hexadecimal separator"));
                    }
                }
                out.push(Token::Literal(
                    Value::Int(u64::from_str_radix(&text, 16).map_err(|_| bad("invalid hexadecimal integer"))? as i64),
                    false,
                ));
                spans.push((token_start, at));
                continue;
            }
            while bytes.get(at).is_some_and(|b| b.is_ascii_digit() || matches!(b, b'.' | b'_')) {
                at += 1;
            }
            if matches!(bytes.get(at), Some(b'e' | b'E')) {
                at += 1;
                if matches!(bytes.get(at), Some(b'+' | b'-')) {
                    at += 1;
                }
                while bytes.get(at).is_some_and(|b| b.is_ascii_digit() || *b == b'_') {
                    at += 1;
                }
            }
            let raw = &sql[start..at];
            for (i, b) in raw.bytes().enumerate() {
                if b == b'_'
                    && (i == 0 || i + 1 == raw.len() || !raw.as_bytes()[i - 1].is_ascii_digit() || !raw.as_bytes()[i + 1].is_ascii_digit())
                {
                    return Err(bad("invalid numeric separator"));
                }
            }
            let text = raw.replace('_', "");
            let integer = !text.contains(['.', 'e', 'E']);
            let value = text
                .parse::<i64>()
                .map(Value::Int)
                .or_else(|_| text.parse::<f64>().map(Value::Real))
                .map_err(|_| bad("invalid numeric literal"))?;
            out.push(Token::Literal(value, integer && text == "9223372036854775808"));
            spans.push((token_start, at));
            continue;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 128 {
            let start = at;
            at += 1;
            while bytes.get(at).is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$') || *b >= 128) {
                at += 1;
            }
            out.push(Token::Name(sql[start..at].into(), 0));
            spans.push((token_start, at));
            continue;
        }
        let mut matched = false;
        for symbol in ["->>", "||", "<<", ">>", "<=", ">=", "==", "!=", "<>", "->"] {
            if bytes[at..].starts_with(symbol.as_bytes()) {
                out.push(Token::Symbol(symbol.into()));
                at += symbol.len();
                matched = true;
                break;
            }
        }
        if matched {
            spans.push((token_start, at));
            continue;
        }
        if b"()+-*/%~&|=<>.,;".contains(&byte) {
            out.push(Token::Symbol((byte as char).to_string()));
            at += 1;
        } else {
            return Err(bad("unsupported SQL token"));
        }
        spans.push((token_start, at));
    }
    Ok(Spanned { tokens: out, spans })
}
