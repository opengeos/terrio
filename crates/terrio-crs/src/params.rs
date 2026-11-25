// SPDX-FileCopyrightText: PROJ contributors
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   PROJ 9.8.1:
//     src/dmstor.cpp (`dmstor_ctx`, in `dms`)
// Written to match the behavior of:
//   PROJ 9.8.1:
//     src/param.cpp

//! The `+key=value` parameters of a PROJ string.

/// Boolean switches consumed by the CRS reader; numeric parameters keep presence semantics.
const FLAGS: &[&str] = &[
    "south",
    "no_defs",
    "wktext",
    "over",
    "approx",
    "geoc",
    "R_A",
    "R_C",
    "R_V",
    "R_a",
    "R_g",
    "R_h",
    "no_rot",
    "no_uoff",
    "no_off",
    "czech",
    "guam",
    "hyperbolic",
    "exact",
    "transpose",
    "no_z_transform",
    "inv",
    "omit_fwd",
    "omit_inv",
];

fn flag_value(value: Option<&str>) -> Option<bool> {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("true" | "t" | "yes" | "y" | "on" | "1") => Some(true),
        Some("false" | "f" | "no" | "n" | "off" | "0") => Some(false),
        _ => None,
    }
}

/// Parameters every method reads, which PROJ checks only for the methods its CRS parser maps them
/// for (`+lat_0=abc` is refused for `tmerc` and ignored for `robin`): an unreadable value is
/// still read as absent. The scale factor's own check is in `Def::from_params`.
const LENIENT: [&str; 6] = ["lat_0", "lon_0", "x_0", "y_0", "k_0", "k"];

/// The parameters of a PROJ string, in order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Params {
    items: Vec<(String, Option<String>)>,
    unterminated_quote: bool,
    /// The parameters read as a number or an angle whose values are neither, in reading order.
    invalid: std::cell::RefCell<Vec<String>>,
    /// Angles read by their longest readable start, as PROJ reads a pipeline step's (a WKT's
    /// `PROJ4` extension: `9d-30` is 9°), not whole, as its CRS parser reads them.
    angle_prefixes: bool,
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
        Self { items, unterminated_quote, invalid: Default::default(), angle_prefixes: false }
    }

    /// [`Self::parse`] of a WKT's `PROJ4` extension, whose angles PROJ reads as a pipeline step's.
    pub fn parse_extension(s: &str) -> Self {
        Self { angle_prefixes: true, ..Self::parse(s) }
    }

    /// Whether these are a WKT `PROJ4` extension's parameters ([`Self::parse_extension`]), which
    /// PROJ reads as a pipeline step's, without its CRS parser's checks.
    pub fn is_extension(&self) -> bool {
        self.angle_prefixes
    }

    /// Refuses a quoted value that would swallow the rest of a definition.
    pub fn validate_quoting(&self) -> crate::Result<()> {
        if self.unterminated_quote {
            return Err(crate::Error::Crs("unterminated quoted PROJ value".into()));
        }
        Ok(())
    }

    /// Refuses non-boolean values on recognized switches rather than treating them as enabled.
    pub fn validate_flags(&self) -> crate::Result<()> {
        for (key, value) in &self.items {
            if FLAGS.contains(&key.as_str()) && flag_value(value.as_deref()).is_none() {
                return Err(crate::Error::Crs(format!("+{key} needs a boolean flag value")));
            }
        }
        if self.str("init").is_some() && self.keys().any(|key| FLAGS.contains(&key) && !matches!(key, "no_defs" | "wktext")) {
            return Err(crate::Error::Crs(
                "boolean overrides combined with +init are not supported; use an expanded +proj definition".into(),
            ));
        }
        Ok(())
    }

    /// Recognized boolean parameters, in definition order, with their normalized values.
    pub fn boolean_parameters(&self) -> Vec<(String, bool)> {
        let mut seen = std::collections::HashSet::new();
        self.items
            .iter()
            .filter(|(key, _)| FLAGS.contains(&key.as_str()) && seen.insert(key.as_str()))
            .filter_map(|(key, value)| flag_value(value.as_deref()).map(|value| (key.clone(), value)))
            .collect()
    }

    /// Whether a parameter is present, or a recognized boolean switch is enabled.
    /// Call `validate_flags` first: invalid switch values otherwise read as disabled.
    pub fn has(&self, key: &str) -> bool {
        self.items
            .iter()
            .find(|(k, _)| k == key)
            .is_some_and(|(_, value)| !FLAGS.contains(&key) || flag_value(value.as_deref()).unwrap_or(false))
    }

    /// The parameters' names, in order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|(k, _)| k.as_str())
    }

    /// The parameter's value.
    pub fn str(&self, key: &str) -> Option<&str> {
        self.items.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.as_deref())
    }

    /// Reads a parameter's value. A value present but not read (`abc`, or an empty `+key=`) is
    /// remembered for [`Self::invalid`] and read as absent.
    fn read<T>(&self, key: &str, parse: impl FnOnce(&str) -> Option<T>) -> Option<T> {
        let value = self.str(key)?;
        let v = parse(value);
        if v.is_none() && !LENIENT.contains(&key) && !self.invalid.borrow().iter().any(|k| k == key) {
            self.invalid.borrow_mut().push(key.to_string());
        }
        v
    }

    /// The parameters read as a number or an angle without being one, in reading order: PROJ
    /// refuses a CRS with such a value ("invalid value for lat_ts"). Only parameters read count,
    /// as PROJ only checks those its method uses, and not those of [`LENIENT`].
    /// Call after all numeric and angular reads: this is a snapshot of failures recorded so far.
    pub fn invalid(&self) -> Vec<String> {
        self.invalid.borrow().clone()
    }

    /// The parameter's numeric value.
    pub fn f64(&self, key: &str) -> Option<f64> {
        self.read(key, |v| v.trim().parse().ok()).map(|v| self.rounded(key, v))
    }

    /// A conversion parameter as PROJ 9.9.0's CRS parser stores it (`io.cpp`,
    /// `PROJStringParser::Private::buildProjectedCRS`): the nearest integer when within 1e-8 of it,
    /// angles in degrees, false eastings and northings in the CRS's unit (`+lat_0=45.000000001` is
    /// 45, #1012 item 39). Not in a WKT's `PROJ4` extension, which PROJ reads as a pipeline step.
    fn rounded(&self, key: &str, v: f64) -> f64 {
        let near = |x: f64| if (x - x.round()).abs() < 1e-8 { x.round() } else { x };
        if self.angle_prefixes || !v.is_finite() {
            return v;
        }
        match key {
            "lat_0" | "lon_0" | "lat_1" | "lat_2" | "lat_ts" | "lonc" | "alpha" | "gamma" | "k" | "k_0" => near(v),
            "x_0" | "y_0" => {
                let unit = self.str("units").and_then(crate::math::unit_to_meter);
                let m = unit.or_else(|| self.str("to_meter").and_then(|t| t.trim().parse().ok())).filter(|m: &f64| *m > 0.0).unwrap_or(1.0);
                // The value itself when not rounded, not its round trip through the unit.
                let r = near(v / m);
                if r == v / m { v } else { r * m }
            }
            _ => v,
        }
    }

    /// The parameter as an angle, in radians: decimal degrees, PROJ's DMS (`40d30'15"N`), or
    /// radians (`0.5r`).
    pub fn angle(&self, key: &str) -> Option<f64> {
        self.degrees(key).map(f64::to_radians)
    }

    /// The parameter as an angle in degrees, as [`Self::angle`] reads it.
    pub fn degrees(&self, key: &str) -> Option<f64> {
        if self.angle_prefixes { self.read(key, |v| dms(v, false)) } else { self.read(key, parse_dms).map(|v| self.rounded(key, v)) }
    }

    /// The parameter as a comma-separated list of numbers.
    pub fn list(&self, key: &str) -> Option<Vec<f64>> {
        self.read(key, |v| v.split(',').map(|x| x.trim().parse().ok()).collect())
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

/// Parses an angle as PROJ 9.8.1's `dmstor` reads it, into degrees: a sign, then up to three
/// numbers each followed by its unit (`d`, `D` or `°` for degrees, `'` for minutes, `"` for
/// seconds; a number without one is in the unit after the previous one), or a single number in
/// radians (`0.5r`), then an optional hemisphere letter (`N`, `E`, `S`, `W`, either case), which
/// gives the sign over the leading one. The whole value must be read: PROJ refuses `9d-30` or
/// `40.5NS` ("Invalid value"), so they are not angles here either.
pub(crate) fn parse_dms(s: &str) -> Option<f64> {
    dms(s, true)
}

/// [`parse_dms`] of the value's longest readable start, as PROJ reads `+R_lat_a` and `+R_lat_g`
/// even in a PROJ string (`40.5NS` is 40.5°, `abc` none): `INFINITY` where PROJ's `dmstor` gives
/// `HUGE_VAL` (`40d30d`, `1e400`).
pub(crate) fn parse_dms_prefix(s: &str) -> Option<f64> {
    dms(s, false)
}

/// [`parse_dms`], of the whole value (`whole`) or of its longest readable start, as PROJ's
/// `dmstor` reads a pipeline step's angle: `9d-30` is 9°, `40.5NS` 40.5°.
fn dms(s: &str, whole: bool) -> Option<f64> {
    let s = s.trim();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut negative = false;
    if let Some(c @ ('+' | '-')) = chars.first() {
        negative = *c == '-';
        i = 1;
    }
    // A number as `strtod` reads it: digits with one point, then an exponent only when digits
    // follow (`40.5.2` is 40.5 and `.2`).
    let number = |i: &mut usize| -> Option<f64> {
        let start = *i;
        let mut point = false;
        while *i < chars.len() && (chars[*i].is_ascii_digit() || (chars[*i] == '.' && !point)) {
            point |= chars[*i] == '.';
            *i += 1;
        }
        if *i < chars.len() && matches!(chars[*i], 'e' | 'E') {
            let mut j = *i + 1;
            if j < chars.len() && matches!(chars[j], '+' | '-') {
                j += 1;
            }
            if j < chars.len() && chars[j].is_ascii_digit() {
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                *i = j;
            }
        }
        chars[start..*i].iter().collect::<String>().parse().ok()
    };
    // PROJ's `HUGE_VAL` for a malformed angle (`40d30d`): no angle for a CRS's value; for a
    // pipeline step's, an infinite one, which the parameter's own check refuses, as PROJ does.
    let huge_val = (!whole).then_some(f64::INFINITY);
    // In degrees, as PROJ sums them: degrees, minutes and seconds.
    const UNITS: [f64; 3] = [1.0, 1.0 / 60.0, 1.0 / 3600.0];
    let (mut degrees, mut radians) = (0.0, None);
    let mut next = 0;
    while next < 3 && i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
        // An unreadable number (`40d.`) ends the angle without being read: PROJ's `strtod`
        // reads nothing there, so the angle so far stands (a whole value is then refused below).
        let start = i;
        let Some(v) = number(&mut i) else {
            i = start;
            break;
        };
        let unit = match chars.get(i) {
            Some('d' | 'D' | '\u{b0}') => 0,
            Some('\'') => 1,
            Some('"') => 2,
            Some('r' | 'R') => {
                // Radians only as the first number: else PROJ's `HUGE_VAL`, an error (see below).
                if next > 0 {
                    return huge_val;
                }
                radians = Some(v);
                i += 1;
                break;
            }
            // No unit: the one after the previous number's, and the end of the numbers.
            _ => {
                degrees += v * UNITS[next];
                break;
            }
        };
        // Units in order (not minutes after seconds).
        if unit < next {
            return huge_val;
        }
        degrees += v * UNITS[unit];
        i += 1;
        next = unit + 1;
    }
    if i == usize::from(chars.first().is_some_and(|c| matches!(c, '+' | '-'))) {
        return None;
    }
    if let Some(c) = chars.get(i)
        && "NnEeSsWw".contains(*c)
    {
        negative = "SsWw".contains(*c);
        i += 1;
    }
    if whole && i != chars.len() {
        return None;
    }
    let v = radians.map_or(degrees, f64::to_degrees);
    // An overflow (`1e400`) is PROJ's `HUGE_VAL`, an error: no angle for a CRS's value; for a
    // pipeline step's, the overflow itself, which the parameter's own check then refuses
    // (`+lat_0`: "|lat_0| should be <= 90°"), rather than an absent value read as 0.
    if !v.is_finite() && whole {
        return None;
    }
    Some(if negative { -v } else { v })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_hemisphere_letter_gives_the_sign_over_a_minus() {
        // As PROJ 9.8.1 reads them (`+lat_0=-52.5S` is -52.5, `-52.5N` 52.5).
        let near = |s: &str, v: f64| (super::parse_dms(s).unwrap() - v).abs() < 1e-12;
        let seconds = 52.0 + 30.0 / 60.0 + 10.0 / 3600.0;
        for (s, v) in [("-52.5S", -52.5), ("-52.5N", 52.5), ("52.5S", -52.5), ("-52.5", -52.5), ("-5W", -5.0), ("-5E", 5.0)] {
            assert!(near(s, v), "{s}");
        }
        for (s, v) in [("-52d30'10\"S", -seconds), ("-52d30'10\"N", seconds), ("52d30'10\"S", -seconds), ("-52d30'10\"", -seconds)] {
            assert!(near(s, v), "{s}");
        }
    }

    use super::*;

    #[test]
    fn boolean_switches_keep_numeric_parameter_presence() {
        for value in ["False", "false", "F", "f", "NO", "off", "0"] {
            let p = Params::parse(&format!("+south={value} +lat_0=0 +zone=0"));
            p.validate_flags().unwrap();
            assert!(!p.has("south"));
            assert!(p.has("lat_0") && p.has("zone"));
        }
        for value in ["True", "true", "T", "YES", "on", "1"] {
            let p = Params::parse(&format!("+south={value}"));
            p.validate_flags().unwrap();
            assert!(p.has("south"));
        }
        for value in ["", "maybe", "2", "NaN"] {
            assert!(Params::parse(&format!("+south={value}")).validate_flags().is_err());
        }
    }

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
