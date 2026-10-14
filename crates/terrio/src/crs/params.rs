//! The `+key=value` parameters of a PROJ string.

/// The parameters of a PROJ string, in order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Params {
    items: Vec<(String, Option<String>)>,
}

impl Params {
    /// Parses `+proj=utm +zone=33 +south` (the `+` signs are optional).
    pub fn parse(s: &str) -> Self {
        let items = s
            .split_whitespace()
            .map(|t| t.trim_start_matches('+'))
            .filter(|t| !t.is_empty())
            .map(|t| match t.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (t.to_string(), None),
            })
            .collect();
        Self { items }
    }

    /// Whether the parameter is present.
    pub fn has(&self, key: &str) -> bool {
        self.items.iter().any(|(k, _)| k == key)
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
        assert!(p.has("south"));
        assert_eq!(p.f64("zone"), Some(33.0));
        assert_eq!(p.list("towgs84"), Some(vec![1.0, 2.0, 3.0]));
        assert!((p.angle("lat_0").unwrap().to_degrees() - 40.5).abs() < 1e-12);
        assert_eq!(parse_dms("-74.5"), Some(-74.5));
        assert_eq!(parse_dms("74d30W"), Some(-74.5));
    }
}
