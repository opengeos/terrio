//! Datum transformations: the geodetic steps of PROJ pipelines (geocentric conversion, Helmert,
//! Molodensky-Badekas and geographic offsets), run as PROJ runs them.

use super::math::Ell;
use super::params::Params;

/// One step of a datum transformation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    /// Geodetic to geocentric coordinates on an ellipsoid (inverse: geocentric to geodetic).
    Cart { ell: Ell, inv: bool },
    /// A Helmert transformation of geocentric coordinates, about a pivot point for
    /// Molodensky-Badekas.
    Helmert { t: [f64; 3], r: [[f64; 3]; 3], scale: f64, pivot: [f64; 3], inv: bool },
    /// Offsets in radians added to the longitude and latitude.
    GeogOffset { dlon: f64, dlat: f64, inv: bool },
}

/// A datum transformation to WGS 84, with the area where it applies and what PROJ ranks it by.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Op {
    /// West, south, east and north in degrees. West may exceed east across the antimeridian.
    pub bounds: [f64; 4],
    /// The accuracy in metres, negative when unknown.
    pub accuracy: f64,
    /// Whether its area is offshore (PROJ only uses those when no other fits).
    pub offshore: bool,
    /// Whether its area is named "unknown".
    pub unknown_area: bool,
    /// Whether this is PROJ's "ballpark" transformation (no datum shift, as a last resort).
    pub ballpark: bool,
    /// Whether it chains several transformations through intermediate datums.
    pub concatenated: bool,
    pub steps: Vec<Step>,
}

const SEC_TO_RAD: f64 = std::f64::consts::PI / 180.0 / 3600.0;

/// The ellipsoid of a step's `ellps=`, `a=`/`rf=`/`b=` or `R=` parameters.
pub(crate) fn ellipsoid(p: &Params) -> Option<Ell> {
    if let Some(r) = p.f64("R") {
        return Some(Ell::from_a_rf(r, 0.0));
    }
    if let Some(a) = p.f64("a") {
        if let Some(rf) = p.f64("rf") {
            return Some(Ell::from_a_rf(a, rf));
        }
        if let Some(b) = p.f64("b") {
            return Some(Ell::from_a_b(a, b));
        }
        if let Some(f) = p.f64("f") {
            return Some(Ell::from_a_rf(a, if f == 0.0 { 0.0 } else { 1.0 / f }));
        }
        if let Some(es) = p.f64("es") {
            return Some(Ell::from_a_es(a, es));
        }
        if let Some(e) = p.f64("e") {
            return Some(Ell::from_a_es(a, e * e));
        }
        return p.str("ellps").and_then(Ell::named).map(|e| Ell::from_a_es(a, e.es)).or(Some(Ell::from_a_rf(a, 0.0)));
    }
    p.str("ellps").and_then(Ell::named)
}

/// A Helmert step from its seven parameters (metres, arc-seconds and ppm).
pub(crate) fn helmert(t: [f64; 3], rot: [f64; 3], s: f64, position_vector: bool, exact: bool, pivot: [f64; 3]) -> Step {
    let (f, th, p) = (rot[0] * SEC_TO_RAD, rot[1] * SEC_TO_RAD, rot[2] * SEC_TO_RAD);
    let mut r = if exact {
        let (sf, cf) = f.sin_cos();
        let (st, ct) = th.sin_cos();
        let (sp, cp) = p.sin_cos();
        [
            [ct * cp, cf * sp + sf * st * cp, sf * sp - cf * st * cp],
            [-ct * sp, cf * cp - sf * st * sp, sf * cp + cf * st * sp],
            [st, -sf * ct, cf * ct],
        ]
    } else {
        [[1.0, p, -th], [-p, 1.0, f], [th, -f, 1.0]]
    };
    if position_vector {
        r = [[r[0][0], r[1][0], r[2][0]], [r[0][1], r[1][1], r[2][1]], [r[0][2], r[1][2], r[2][2]]];
    }
    Step::Helmert { t, r, scale: 1.0 + s * 1e-6, pivot, inv: false }
}

/// The steps of a `+towgs84=` shift (position vector convention) from an ellipsoid to WGS 84.
pub(crate) fn towgs84_steps(ell: Ell, p: &[f64]) -> Vec<Step> {
    let g = |i: usize| p.get(i).copied().unwrap_or(0.0);
    vec![
        Step::Cart { ell, inv: false },
        helmert([g(0), g(1), g(2)], [g(3), g(4), g(5)], g(6), true, false, [0.0; 3]),
        Step::Cart { ell: Ell::wgs84(), inv: true },
    ]
}

/// Parses the `;`-separated steps of the EPSG table.
pub(crate) fn parse_steps(s: &str) -> Option<Vec<Step>> {
    let mut out = Vec::new();
    for part in s.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (inv, body) = match part.strip_prefix("inv ") {
            Some(b) => (true, b),
            None => (false, part),
        };
        let (name, rest) = body.split_once(' ').unwrap_or((body, ""));
        let p = Params::parse(rest);
        let step = match name {
            "cart" => Step::Cart { ell: ellipsoid(&p)?, inv },
            "helmert" | "molobadekas" => {
                let g = |k: &str| p.f64(k).unwrap_or(0.0);
                let pv = p.str("convention") != Some("coordinate_frame");
                let pivot = [g("px"), g("py"), g("pz")];
                let mut h = helmert([g("x"), g("y"), g("z")], [g("rx"), g("ry"), g("rz")], g("s"), pv, p.has("exact"), pivot);
                if let Step::Helmert { inv: i, .. } = &mut h {
                    *i = inv;
                }
                h
            }
            "geogoffset" => {
                let g = |k: &str| p.f64(k).unwrap_or(0.0) * SEC_TO_RAD;
                Step::GeogOffset { dlon: g("dlon"), dlat: g("dlat"), inv }
            }
            _ => return None,
        };
        out.push(step);
    }
    Some(out)
}

/// Geodetic (radians, metres) to geocentric coordinates.
fn to_cart(ell: &Ell, lon: f64, lat: f64, h: f64) -> [f64; 3] {
    let (sp, cp) = lat.sin_cos();
    let n = ell.a / (1.0 - ell.es * sp * sp).sqrt();
    [(n + h) * cp * lon.cos(), (n + h) * cp * lon.sin(), (n * ell.one_es + h) * sp]
}

/// Geocentric to geodetic coordinates, by Bowring's formula as PROJ's `cart` does.
fn from_cart(ell: &Ell, x: f64, y: f64, z: f64) -> [f64; 3] {
    let a = ell.a;
    let b = a * ell.one_es.sqrt();
    let p = x.hypot(y);
    let lon = y.atan2(x);
    if ell.es == 0.0 {
        let lat = z.atan2(p);
        return [lon, lat, x.hypot(y).hypot(z) - a];
    }
    let second = ell.es / ell.one_es;
    let theta = (z * a).atan2(p * b);
    let (s, c) = theta.sin_cos();
    let lat = (z + second * b * s * s * s).atan2(p - ell.es * a * c * c * c);
    let (sp, cp) = lat.sin_cos();
    let n = a / (1.0 - ell.es * sp * sp).sqrt();
    let h = if cp.abs() < 1e-6 { z / sp - n * ell.one_es } else { p / cp - n };
    [lon, lat, h]
}

impl Step {
    fn apply(&self, v: [f64; 3], reverse: bool) -> [f64; 3] {
        match self {
            Step::Cart { ell, inv } => {
                if *inv != reverse {
                    from_cart(ell, v[0], v[1], v[2])
                } else {
                    to_cart(ell, v[0], v[1], v[2])
                }
            }
            Step::Helmert { t, r, scale, pivot, inv } => {
                if *inv != reverse {
                    // PROJ inverts with the transpose of the rotation.
                    let d = [v[0] - t[0] - pivot[0], v[1] - t[1] - pivot[1], v[2] - t[2] - pivot[2]];
                    let mut o = [0.0; 3];
                    for (i, oi) in o.iter_mut().enumerate() {
                        *oi = (r[0][i] * d[0] + r[1][i] * d[1] + r[2][i] * d[2]) / scale + pivot[i];
                    }
                    o
                } else {
                    let d = [v[0] - pivot[0], v[1] - pivot[1], v[2] - pivot[2]];
                    let mut o = [0.0; 3];
                    for (i, oi) in o.iter_mut().enumerate() {
                        *oi = scale * (r[i][0] * d[0] + r[i][1] * d[1] + r[i][2] * d[2]) + t[i] + pivot[i];
                    }
                    o
                }
            }
            Step::GeogOffset { dlon, dlat, inv } => {
                let sign = if *inv != reverse { -1.0 } else { 1.0 };
                [v[0] + sign * dlon, v[1] + sign * dlat, v[2]]
            }
        }
    }
}

impl Op {
    /// A transformation that applies everywhere.
    pub fn world(steps: Vec<Step>) -> Self {
        Self {
            bounds: [-180.0, -90.0, 180.0, 90.0],
            accuracy: -1.0,
            offshore: false,
            unknown_area: false,
            ballpark: false,
            concatenated: false,
            steps,
        }
    }

    /// PROJ's measure of the size of the area, for ranking transformations of equal accuracy.
    pub fn pseudo_area(&self) -> f64 {
        let [w, s, mut e, n] = self.bounds;
        if w > e {
            e += 360.0;
        }
        (e - w) * (n.to_radians().sin() - s.to_radians().sin())
    }

    /// Whether the op's area holds a point in degrees.
    pub fn contains(&self, lon: f64, lat: f64) -> bool {
        let [w, s, e, n] = self.bounds;
        if lat < s || lat > n {
            return false;
        }
        if w <= e { lon >= w && lon <= e } else { lon >= w || lon <= e }
    }
}

/// Runs datum steps on a geodetic point (longitude and latitude in radians), keeping its height
/// as PROJ's `push`/`pop` steps do. `reverse` runs the inverse transformation.
pub(crate) fn run(steps: &[Step], lon: f64, lat: f64, reverse: bool) -> (f64, f64) {
    let mut v = [lon, lat, 0.0];
    if reverse {
        for s in steps.iter().rev() {
            v = s.apply(v, true);
        }
    } else {
        for s in steps {
            v = s.apply(v, false);
        }
    }
    (v[0], v[1])
}

/// Picks the transformation for a point as PROJ does: among those whose area holds it (as told
/// by `contains`, given the op and its index), the most accurate, then the one with the smallest
/// area; the first of the list when none does.
pub(crate) fn pick_by<T: AsRef<Op>>(ops: &[T], contains: impl Fn(&Op, usize) -> bool) -> Option<&Op> {
    pick_in(ops, contains).or_else(|| ops.first().map(AsRef::as_ref))
}

/// [`pick_by`] without falling back on the first transformation when none fits.
pub(crate) fn pick_in<T: AsRef<Op>>(ops: &[T], contains: impl Fn(&Op, usize) -> bool) -> Option<&Op> {
    let mut best: Option<&Op> = None;
    for (i, op) in ops.iter().map(AsRef::as_ref).enumerate() {
        if !contains(op, i) {
            continue;
        }
        let better = match best {
            None => true,
            Some(b) => {
                op.accuracy >= 0.0
                    && (op.accuracy < b.accuracy
                        || (op.accuracy == b.accuracy && op.pseudo_area() < b.pseudo_area() && !(op.unknown_area && !b.unknown_area))
                        || (op.accuracy == b.accuracy && !op.unknown_area && b.unknown_area))
                    && !op.offshore
            }
        };
        if better {
            best = Some(op);
        }
    }
    best
}

impl AsRef<Op> for Op {
    fn as_ref(&self) -> &Op {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cart_round_trips() {
        let ell = Ell::named("intl").unwrap();
        let c = to_cart(&ell, 0.3, 0.8, 100.0);
        let g = from_cart(&ell, c[0], c[1], c[2]);
        assert!((g[0] - 0.3).abs() < 1e-14 && (g[1] - 0.8).abs() < 1e-12 && (g[2] - 100.0).abs() < 1e-6, "{g:?}");
    }

    #[test]
    fn helmert_inverts() {
        let steps = parse_steps("cart ellps=bessel;helmert x=565.4171 y=50.3319 z=465.5524 rx=0.398957388243134 ry=-0.343987817378283 rz=1.87740163998045 s=4.0725 convention=coordinate_frame;inv cart ellps=WGS84").unwrap();
        let (lon, lat) = run(&steps, 0.09, 0.91, false);
        let (l2, p2) = run(&steps, lon, lat, true);
        assert!((l2 - 0.09).abs() < 1e-10 && (p2 - 0.91).abs() < 1e-10);
    }
}
