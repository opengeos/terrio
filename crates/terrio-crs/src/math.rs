//! Ellipsoids, units and the helper functions map projections share, after PROJ's
//! (<https://proj.org>, MIT licence) definitions and algorithms; PROJ's notice is in `NOTICE`.

// The constants are PROJ's, digit for digit.
#![allow(clippy::excessive_precision)]

use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

/// An ellipsoid normalized to a semi-major axis of 1, as projections use it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Ell {
    /// The semi-major axis in metres.
    pub a: f64,
    /// The eccentricity squared.
    pub es: f64,
    /// The eccentricity.
    pub e: f64,
    /// `1 - es`.
    pub one_es: f64,
}

impl Ell {
    /// An ellipsoid from its semi-major axis and inverse flattening (0 for a sphere).
    pub fn from_a_rf(a: f64, rf: f64) -> Self {
        let f = if rf == 0.0 { 0.0 } else { 1.0 / rf };
        Self::from_a_es(a, 2.0 * f - f * f)
    }

    /// An ellipsoid from its semi-major and semi-minor axes.
    pub fn from_a_b(a: f64, b: f64) -> Self {
        Self::from_a_es(a, 1.0 - (b * b) / (a * a))
    }

    /// An ellipsoid from its semi-major axis and eccentricity squared.
    pub fn from_a_es(a: f64, es: f64) -> Self {
        let es = if es.abs() < 1e-15 { 0.0 } else { es };
        Self { a, es, e: es.sqrt(), one_es: 1.0 - es }
    }

    /// WGS 84.
    pub fn wgs84() -> Self {
        Self::from_a_rf(6378137.0, 298.257223563)
    }

    /// One of PROJ's named ellipsoids (`+ellps=`).
    pub fn named(name: &str) -> Option<Self> {
        // (name, a, rf, or b when negative).
        const ELLIPSOIDS: &[(&str, f64, f64)] = &[
            ("MERIT", 6378137.0, 298.257),
            ("SGS85", 6378136.0, 298.257),
            ("GRS80", 6378137.0, 298.257222101),
            ("IAU76", 6378140.0, 298.257),
            ("airy", 6377563.396, -6356256.910),
            ("APL4.9", 6378137.0, 298.25),
            ("NWL9D", 6378145.0, 298.25),
            ("mod_airy", 6377340.189, -6356034.446),
            ("andrae", 6377104.43, 300.0),
            ("danish", 6377019.2563, 300.0),
            ("aust_SA", 6378160.0, 298.25),
            ("GRS67", 6378160.0, 298.2471674270),
            ("GSK2011", 6378136.5, 298.2564151),
            ("bessel", 6377397.155, 299.1528128),
            ("bess_nam", 6377483.865, 299.1528128),
            ("clrk66", 6378206.4, -6356583.8),
            ("clrk80", 6378249.145, 293.4663),
            ("clrk80ign", 6378249.2, 293.4660212936269),
            ("CPM", 6375738.7, 334.29),
            ("delmbr", 6376428.0, 311.5),
            ("engelis", 6378136.05, 298.2566),
            ("evrst30", 6377276.345, 300.8017),
            ("evrst48", 6377304.063, 300.8017),
            ("evrst56", 6377301.243, 300.8017),
            ("evrst69", 6377295.664, 300.8017),
            ("evrstSS", 6377298.556, 300.8017),
            ("fschr60", 6378166.0, 298.3),
            ("fschr60m", 6378155.0, 298.3),
            ("fschr68", 6378150.0, 298.3),
            ("helmert", 6378200.0, 298.3),
            ("hough", 6378270.0, 297.0),
            ("intl", 6378388.0, 297.0),
            ("krass", 6378245.0, 298.3),
            ("kaula", 6378163.0, 298.24),
            ("lerch", 6378139.0, 298.257),
            ("mprts", 6397300.0, 191.0),
            ("new_intl", 6378157.5, -6356772.2),
            ("plessis", 6376523.0, -6355863.0),
            ("PZ90", 6378136.0, 298.25784),
            ("SEasia", 6378155.0, -6356773.3205),
            ("walbeck", 6376896.0, -6355834.8467),
            ("WGS60", 6378165.0, 298.3),
            ("WGS66", 6378145.0, 298.25),
            ("WGS72", 6378135.0, 298.26),
            ("WGS84", 6378137.0, 298.257223563),
            ("sphere", 6370997.0, 0.0),
        ];
        let &(_, a, v) = ELLIPSOIDS.iter().find(|e| e.0 == name)?;
        Some(if v < 0.0 { Self::from_a_b(a, -v) } else { Self::from_a_rf(a, v) })
    }
}

/// The size in metres of one of PROJ's named linear units (`+units=`).
pub(crate) fn unit_to_meter(name: &str) -> Option<f64> {
    Some(match name {
        "m" => 1.0,
        "km" => 1000.0,
        "dm" => 0.1,
        "cm" => 0.01,
        "mm" => 0.001,
        "kmi" => 1852.0,
        "in" => 0.0254,
        "ft" => 0.3048,
        "yd" => 0.9144,
        "mi" => 1609.344,
        "fath" => 1.8288,
        "ch" => 20.1168,
        "link" => 0.201168,
        "us-in" => 1.0 / 39.37,
        "us-ft" => 1200.0 / 3937.0,
        "us-yd" => 3600.0 / 3937.0,
        "us-ch" => 79200.0 / 3937.0,
        "us-mi" => 6336000.0 / 3937.0,
        "ind-yd" => 0.91439523,
        "ind-ft" => 0.30479841,
        "ind-ch" => 20.11669506,
        _ => return None,
    })
}

/// The longitude of one of PROJ's named prime meridians (`+pm=`), in degrees.
pub(crate) fn prime_meridian(name: &str) -> Option<f64> {
    Some(match name {
        "greenwich" => 0.0,
        "lisbon" => -9.131906111111,
        "paris" => 2.337229166667,
        "bogota" => -74.080916666667,
        "madrid" => -3.687938888889,
        "rome" => 12.452333333333,
        "bern" => 7.439583333333,
        "jakarta" => 106.807719444444,
        "ferro" => -17.666666666667,
        "brussels" => 4.367975,
        "stockholm" => 18.058277777778,
        "athens" => 23.7163375,
        "oslo" => 10.722916666667,
        "copenhagen" => 12.57788,
        _ => return None,
    })
}

/// Wraps a longitude in radians into [-π, π].
pub(crate) fn adjlon(lon: f64) -> f64 {
    if lon.abs() <= PI + 1e-12 {
        return lon;
    }
    let l = (lon + PI).rem_euclid(2.0 * PI) - PI;
    if l == -PI && lon > 0.0 { PI } else { l }
}

/// `asin` clamped to [-1, 1].
pub(crate) fn aasin(v: f64) -> f64 {
    v.clamp(-1.0, 1.0).asin()
}

/// PROJ's `pj_tsfn`: the conformal latitude function t.
pub(crate) fn tsfn(phi: f64, sinphi: f64, e: f64) -> f64 {
    let c = e * sinphi;
    (0.5 * (FRAC_PI_2 - phi)).tan() / ((1.0 - c) / (1.0 + c)).powf(0.5 * e)
}

/// PROJ's `pj_msfn`: cos φ / sqrt(1 - e² sin² φ).
pub(crate) fn msfn(sinphi: f64, cosphi: f64, es: f64) -> f64 {
    cosphi / (1.0 - es * sinphi * sinphi).sqrt()
}

/// PROJ 9.9's `pj_authalic_lat_q`, using atanh to avoid cancellation in the log ratio.
pub(crate) fn qsfn(sinphi: f64, e: f64, one_es: f64) -> f64 {
    if e >= 1e-7 {
        let con = e * sinphi;
        let div1 = 1.0 - con * con;
        if div1 == 0.0 {
            return f64::INFINITY;
        }
        one_es * (sinphi / div1 + con.atanh() / e)
    } else {
        sinphi + sinphi
    }
}

/// tan φ from sinh ψ (ψ the isometric latitude), by Newton's method (PROJ's
/// `pj_sinhpsi2tanphi`, after Karney).
pub(crate) fn sinhpsi2tanphi(taup: f64, e: f64) -> f64 {
    let rooteps = f64::EPSILON.sqrt();
    let tol = rooteps / 10.0;
    let tmax = 2.0 / rooteps;
    let e2m = 1.0 - e * e;
    let stol = tol * taup.abs().max(1.0);
    let mut tau = if taup.abs() > 70.0 { taup * (e * e.atanh()).exp() } else { taup / e2m };
    if tau.abs() >= tmax {
        return tau;
    }
    for _ in 0..5 {
        let tau1 = (1.0 + tau * tau).sqrt();
        let sig = (e * (e * tau / tau1).atanh()).sinh();
        let taupa = (1.0 + sig * sig).sqrt() * tau - sig * tau1;
        let dtau = (taup - taupa) * (1.0 + e2m * tau * tau) / (e2m * tau1 * (1.0 + taupa * taupa).sqrt());
        tau += dtau;
        if dtau.abs() < stol {
            break;
        }
    }
    tau
}

/// PROJ's `pj_phi2`: the latitude from the conformal function t.
pub(crate) fn phi2(ts: f64, e: f64) -> f64 {
    sinhpsi2tanphi((1.0 / ts - ts) / 2.0, e).atan()
}

/// PROJ 9.8.1's meridian distance and inverse, to order six in third flattening.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mlfn {
    radius: f64,
    forward: [f64; 6],
    inverse: [f64; 6],
}

impl Mlfn {
    pub fn new(es: f64) -> Self {
        // C[mu,phi] and C[phi,mu], even powers only within each row.
        const FORWARD: [f64; 12] = [
            -3.0 / 2.0,
            9.0 / 16.0,
            -3.0 / 32.0,
            15.0 / 16.0,
            -15.0 / 32.0,
            135.0 / 2048.0,
            -35.0 / 48.0,
            105.0 / 256.0,
            315.0 / 512.0,
            -189.0 / 512.0,
            -693.0 / 1280.0,
            1001.0 / 2048.0,
        ];
        const INVERSE: [f64; 12] = [
            3.0 / 2.0,
            -27.0 / 32.0,
            269.0 / 512.0,
            21.0 / 16.0,
            -55.0 / 32.0,
            6759.0 / 4096.0,
            151.0 / 96.0,
            -417.0 / 128.0,
            1097.0 / 512.0,
            -15543.0 / 2560.0,
            8011.0 / 2560.0,
            293393.0 / 61440.0,
        ];
        let b = (1.0 - es).sqrt();
        let n = (1.0 - b) / (1.0 + b);
        let n2 = n * n;
        let coefficients = |table: &[f64; 12]| {
            let mut result = [0.0; 6];
            let (mut offset, mut power) = (0, n);
            for (i, value) in result.iter_mut().enumerate() {
                let width = (5 - i) / 2 + 1;
                *value = power * table[offset..offset + width].iter().rev().fold(0.0, |y, c| y * n2 + c);
                offset += width;
                power *= n;
            }
            result
        };
        let radius = (((1.0 / 256.0 * n2 + 1.0 / 64.0) * n2 + 1.0 / 4.0) * n2 + 1.0) / (1.0 + n);
        Self { radius, forward: coefficients(&FORWARD), inverse: coefficients(&INVERSE) }
    }

    pub fn mlfn(&self, phi: f64, sphi: f64, cphi: f64) -> f64 {
        self.radius * (phi + auxlat_sum(sphi, cphi, &self.forward))
    }

    pub fn inv(&self, arg: f64) -> f64 {
        authlat(arg / self.radius, &self.inverse)
    }
}

/// Coefficients converting authalic to geodetic latitudes, as PROJ since 9.7: the Fourier series
/// in the authalic latitude whose coefficients are Taylor series in the third flattening n, to
/// order 6 (Karney, "On auxiliary latitudes", 2024; PROJ's `pj_auxlat_coeffs`, `C[phi,xi]`).
pub(crate) fn authset(es: f64) -> [f64; 6] {
    const C: [f64; 21] = [
        4.0 / 3.0,
        4.0 / 45.0,
        -16.0 / 35.0,
        -2582.0 / 14175.0,
        60136.0 / 467775.0,
        28112932.0 / 212837625.0,
        46.0 / 45.0,
        152.0 / 945.0,
        -11966.0 / 14175.0,
        -21016.0 / 51975.0,
        251310128.0 / 638512875.0,
        3044.0 / 2835.0,
        3802.0 / 14175.0,
        -94388.0 / 66825.0,
        -8797648.0 / 10945935.0,
        6059.0 / 4725.0,
        41072.0 / 93555.0,
        -1472637812.0 / 638512875.0,
        768272.0 / 467775.0,
        455935736.0 / 638512875.0,
        4210684958.0 / 1915538625.0,
    ];
    let b = (1.0 - es).sqrt();
    let n = (1.0 - b) / (1.0 + b);
    let mut apa = [0.0; 6];
    let (mut o, mut d) = (0, n);
    for (l, a) in apa.iter_mut().enumerate() {
        // A polynomial in n of degree 5 - l, by Horner's method.
        let p = &C[o..o + 6 - l];
        *a = d * p.iter().rev().fold(0.0, |y, c| y * n + c);
        o += 6 - l;
        d *= n;
    }
    apa
}

/// The geodetic latitude of an authalic latitude: `beta + Σ apa[k] sin((2k + 2) beta)`, by
/// Clenshaw summation (PROJ's `pj_authalic_lat_inverse`, for ellipsoids of flattening up to
/// about 1/50).
pub(crate) fn authlat(beta: f64, apa: &[f64; 6]) -> f64 {
    let (s, c) = beta.sin_cos();
    beta + auxlat_sum(s, c, apa)
}

fn auxlat_sum(s: f64, c: f64, apa: &[f64; 6]) -> f64 {
    let x = 2.0 * (c - s) * (c + s);
    let (mut u0, mut u1) = (0.0, 0.0);
    for a in apa.iter().rev() {
        let t = x * u0 - u1 + a;
        u1 = u0;
        u0 = t;
    }
    2.0 * s * c * u0
}

/// PROJ 9.8.1's inverse authalic latitude: refine the Fourier estimate when
/// |n| >= 0.01, where the series alone is insufficient for planetary ellipsoids.
pub(crate) fn authlat_inverse(beta: f64, apa: &[f64; 6], e: f64, one_es: f64, qp: f64) -> f64 {
    let mut phi = authlat(beta, apa);
    let b = one_es.sqrt();
    let n = (1.0 - b) / (1.0 + b);
    if n.abs() < 0.01 {
        return phi;
    }
    let es = 1.0 - one_es;
    let q_div_one_es = beta.sin() * qp / one_es;
    for _ in 0..10 {
        let (sinphi, cosphi) = phi.sin_cos();
        let t = 1.0 - es * (sinphi * sinphi);
        let delta = t * t / (2.0 * cosphi) * (q_div_one_es - sinphi / t - (e * sinphi).atanh() / e);
        if !delta.is_finite() || delta.abs() < 1e-15 {
            break;
        }
        phi += delta;
    }
    phi
}

/// π/4, for the formulas below.
pub(crate) const FORTPI: f64 = FRAC_PI_4;
pub(crate) const HALFPI: f64 = FRAC_PI_2;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authalic_inverse_keeps_its_estimate_when_refinement_is_nonfinite() {
        let one_es = 0.25;
        let e = (1.0_f64 - one_es).sqrt();
        let apa = authset(e * e);
        let beta = 0.5;
        let estimate = authlat(beta, &apa);
        assert_eq!(authlat_inverse(beta, &apa, e, one_es, f64::INFINITY), estimate);
    }

    #[test]
    fn authalic_q_retains_precision_near_the_equator() {
        let ellipsoid = Ell::named("GRS80").unwrap();
        for sine in [1e-12, -1e-12] {
            let expected = 2.0 * ellipsoid.one_es * sine;
            assert!((qsfn(sine, ellipsoid.e, ellipsoid.one_es) - expected).abs() < 1e-27);
        }
    }
}
