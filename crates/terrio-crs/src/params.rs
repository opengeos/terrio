// Written to match the behavior of:
//   PROJ 9.8.1:
//     src/param.cpp / src/dmstor.cpp
// No code is translated from these sources.

//! The `+key=value` parameters of a PROJ string.

/// The parameters of a PROJ string, in order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Params {
    items: Vec<(String, Option<String>)>,
    unterminated_quote: bool,
}

impl Params {
    /// Parses `+proj=utm +zone=33 +south` (the `+` signs are optional). As in PROJ, a value may
    /// be in double quotes (`+title="UTM 32"`), with `""` for a quote inside.
    pub fn parse(s: &str) -> Self {
        let (tokens, unterminated_quote) = tokens(s);
        let items = tokens
            .into_iter()
            .filter_map(|t| {
                let t = t.trim_start_matches('+');
                (!t.is_empty()).then(|| match t.split_once('=') {
                    Some((k, v)) => (k.to_string(), Some(v.to_string())),
                    None => (t.to_string(), None),
                })
            })
            .collect();
        Self { items, unterminated_quote }
    }

    /// Refuses a quoted value that would swallow the rest of a definition.
    pub fn validate_quoting(&self) -> crate::Result<()> {
        if self.unterminated_quote {
            return Err(crate::Error::Crs("unterminated quoted PROJ value".into()));
        }
        Ok(())
    }

    /// Whether the parameter is present.
    pub fn has(&self, key: &str) -> bool {
        self.items.iter().any(|(k, _)| k == key)
    }

    /// The parameters' names, in order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|(k, _)| k.as_str())
    }

    /// The parameter's value.
    pub fn str(&self, key: &str) -> Option<&str> {
        self.items.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.as_deref())
    }

    /// The parameter's numeric value.
    pub fn f64(&self, key: &str) -> Option<f64> {
        self.str(key).and_then(|v| v.trim().parse().ok())
    }

    /// The parameter as an angle, in radians: decimal degrees or PROJ's DMS (`40d30'15"N`).
    pub fn angle(&self, key: &str) -> Option<f64> {
        self.str(key).and_then(parse_dms).map(f64::to_radians)
    }

    /// The parameter as a comma-separated list of numbers.
    pub fn list(&self, key: &str) -> Option<Vec<f64>> {
        self.str(key).map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
    }
}

/// The whitespace-separated tokens of a PROJ string, without the double quotes around values.
fn tokens(s: &str) -> (Vec<String>, bool) {
    let (mut out, mut token, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                token.push('"');
                chars.next();
            }
            // A quote opens a value right after `=`, and closes it; elsewhere it is a character,
            // such as DMS seconds (`52d30'10"N`).
            '"' if quoted || token.ends_with('=') => quoted = !quoted,
            c if c.is_whitespace() && !quoted => out.extend((!token.is_empty()).then(|| std::mem::take(&mut token))),
            c => token.push(c),
        }
    }
    out.extend((!token.is_empty()).then_some(token));
    (out, quoted)
}

/// Parses decimal degrees or PROJ's DMS notation into degrees.
fn parse_dms(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Ok(v) = s.parse() {
        return Some(v);
    }
    let (body, sign) = match s.chars().last()? {
        'N' | 'n' | 'E' | 'e' => (&s[..s.len() - 1], 1.0),
        'S' | 's' | 'W' | 'w' => (&s[..s.len() - 1], -1.0),
        _ => (s, 1.0),
    };
    let mut deg = 0.0;
    let mut rest = body;
    for (sep, div) in [('d', 1.0), ('\'', 60.0), ('"', 3600.0)] {
        if let Some((v, r)) = rest.split_once(sep) {
            deg += v.trim().parse::<f64>().ok()?.abs() / div;
            rest = r;
        }
    }
    if !rest.trim().is_empty() {
        let div = if body.contains('\'') {
            3600.0
        } else if body.contains('d') {
            60.0
        } else {
            1.0
        };
        deg += rest.trim().parse::<f64>().ok()? / div;
    }
    let negative = body.trim_start().starts_with('-');
    Some(sign * if negative { -deg } else { deg })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tokens_and_angles() {
        let p = Params::parse("+proj=utm +zone=33 +south +towgs84=1,2,3 +lat_0=40d30'N");
        p.validate_quoting().unwrap();
        assert!(p.has("south"));
        assert_eq!(p.f64("zone"), Some(33.0));
        assert_eq!(p.list("towgs84"), Some(vec![1.0, 2.0, 3.0]));
        assert!((p.angle("lat_0").unwrap().to_degrees() - 40.5).abs() < 1e-12);
        assert_eq!(parse_dms("-74.5"), Some(-74.5));
        assert_eq!(parse_dms("74d30W"), Some(-74.5));
    }

    #[test]
    fn dms_seconds_are_not_quotes() {
        let p = Params::parse(r#"+proj=tmerc +lat_0=52d30'10"N +lon_0=5d10'20"E +title="a b" +k=0.9996"#);
        assert!((p.angle("lat_0").unwrap().to_degrees() - (52.0 + 30.0 / 60.0 + 10.0 / 3600.0)).abs() < 1e-12);
        assert!((p.angle("lon_0").unwrap().to_degrees() - (5.0 + 10.0 / 60.0 + 20.0 / 3600.0)).abs() < 1e-12);
        p.validate_quoting().unwrap();
        assert_eq!((p.str("title"), p.f64("k")), (Some("a b"), Some(0.9996)));
    }

    #[test]
    fn values_may_be_quoted() {
        let p = Params::parse(r#"+proj="aea" +title="UTM ""zone"" 32"  +towgs84="1,2,3" +no_defs"#);
        p.validate_quoting().unwrap();
        assert_eq!(p.str("proj"), Some("aea"));
        assert_eq!(p.str("title"), Some(r#"UTM "zone" 32"#));
        assert_eq!(p.list("towgs84"), Some(vec![1.0, 2.0, 3.0]));
        assert!(p.has("no_defs"));
    }
}
