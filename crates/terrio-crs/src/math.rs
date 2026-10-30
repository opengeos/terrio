// SPDX-FileCopyrightText: PROJ contributors
// SPDX-FileCopyrightText: Copyright (c) 2000, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2025 Javier Jimenez Shaw
// SPDX-FileCopyrightText: Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND SunPro
//
// Derived from (translated to Rust and modified):
//   PROJ 9.8.1:
//     src/ellps.cpp
//     src/units.cpp
//     src/datums.cpp
//     src/adjlon.cpp
//     src/aasincos.cpp
//     src/tsfn.cpp
//     src/msfn.cpp
//     src/phi2.cpp
//     src/mlfn.cpp
//     src/latitudes.cpp
//   PROJ 9.9.0:
//     src/latitudes.cpp
//   GNU C Library 2.39 (fdlibm code, under Sun Microsystems' notice):
//     sysdeps/ieee754/dbl-64/s_asinh.c
//   fdlibm 5.3:
//     e_atanh.c

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

// `asinh` is translated from the GNU C Library's `sysdeps/ieee754/dbl-64/s_asinh.c` (glibc 2.39),
// fdlibm code that carries this notice:
//
// ====================================================
// Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
//
// Developed at SunPro, a Sun Microsystems, Inc. business.
// Permission to use, copy, modify, and distribute this
// software is freely granted, provided that this notice
// is preserved.
// ====================================================

/// `asin` clamped to [-1, 1].
/// `asinh` as glibc 2.39 computes it (`sysdeps/ieee754/dbl-64/s_asinh.c`), which PROJ calls:
/// Rust's `f64::asinh` has its own formula, which differs in the last bit for about a tenth of
/// the inputs. Built on `ln`, `ln_1p` and `sqrt`, which are the C library's on linux-gnu; elsewhere
/// PROJ uses another C library, and `f64::asinh` stands in.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn asinh(x: f64) -> f64 {
    // glibc's `ln2` (6.93147180559945286227e-01) is this double.
    use std::f64::consts::LN_2 as LN2;
    let ix = ((x.to_bits() >> 32) as u32) & 0x7fff_ffff;
    // |x| < 2^-28: asinh(x) is x.
    if ix < 0x3e30_0000 {
        return x;
    }
    let w = if ix > 0x41b0_0000 {
        // |x| > 2^28, or infinite or NaN.
        if ix >= 0x7ff0_0000 {
            return x + x;
        }
        x.abs().ln() + LN2
    } else {
        let xa = x.abs();
        if ix > 0x4000_0000 {
            // 2 < |x| <= 2^28.
            (2.0 * xa + 1.0 / ((xa * xa + 1.0).sqrt() + xa)).ln()
        } else {
            let t = xa * xa;
            (xa + t / (1.0 + (1.0 + t).sqrt())).ln_1p()
        }
    };
    w.copysign(x)
}

/// `asinh`: the C library's where its formula is not known.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn asinh(x: f64) -> f64 {
    x.asinh()
}

// `atanh` is translated from fdlibm 5.3's `e_atanh.c` (https://www.netlib.org/fdlibm/), which
// carries this notice:
//
// ====================================================
// Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
//
// Developed at SunSoft, a Sun Microsystems, Inc. business.
// Permission to use, copy, modify, and distribute this
// software is freely granted, provided that this notice
// is preserved.
// ====================================================

/// `atanh` as fdlibm computes it (`e_atanh.c`), which gives glibc 2.39's results, which PROJ
/// calls; built on `ln_1p`, the C library's on linux-gnu (see [`asinh`]).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
// `(x - x) / (x - x)` is fdlibm's NaN for |x| > 1, computed at run time as fdlibm computes it.
#[allow(clippy::eq_op)]
pub(crate) fn atanh(x: f64) -> f64 {
    let hx = (x.to_bits() >> 32) as u32;
    let lx = x.to_bits() as u32;
    let ix = hx & 0x7fff_ffff;
    // |x| > 1, or NaN.
    if (ix | ((lx | lx.wrapping_neg()) >> 31)) > 0x3ff0_0000 {
        return (x - x) / (x - x);
    }
    // |x| = 1.
    if ix == 0x3ff0_0000 {
        return x / 0.0;
    }
    // |x| < 2^-28: atanh(x) is x.
    if ix < 0x3e30_0000 {
        return x;
    }
    let xa = x.abs();
    let t = if ix < 0x3fe0_0000 {
        // |x| < 0.5.
        let t = xa + xa;
        0.5 * (t + t * xa / (1.0 - xa)).ln_1p()
    } else {
        0.5 * ((xa + xa) / (1.0 - xa)).ln_1p()
    };
    if hx >> 31 == 0 { t } else { -t }
}

/// `atanh`: the C library's where its formula is not known.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn atanh(x: f64) -> f64 {
    x.atanh()
}

pub(crate) fn aasin(v: f64) -> f64 {
    v.clamp(-1.0, 1.0).asin()
}

/// PROJ's `pj_tsfn`: the conformal latitude function t = exp(-ψ), ψ the isometric latitude, in
/// PROJ 9's form (`src/tsfn.cpp`, 9.7.1): `exp(e atanh(e sin φ))` times `cos φ / (1 + sin φ)`
/// where sin φ > 0, else `(1 - sin φ) / cos φ`, with the C library's `atanh` (see [`atanh`]).
/// PROJ's earlier `tan((π/2 - φ) / 2) / ((1 - e sin φ) / (1 + e sin φ))^(e/2)` is the same
/// function but not the same bits: through a Lambert conformal conic's constants it moved
/// latitudes by a few units in the last place from GDAL's.
pub(crate) fn tsfn(phi: f64, sinphi: f64, e: f64) -> f64 {
    let cosphi = phi.cos();
    (e * atanh(e * sinphi)).exp() * if sinphi > 0.0 { cosphi / (1.0 + sinphi) } else { (1.0 - sinphi) / cosphi }
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
    auxlat_coeffs(es, &C_PHI_XI)
}

/// Coefficients converting geodetic to authalic latitudes, as PROJ since 9.7 (`C[xi,phi]`): the
/// authalic latitude of a geodetic latitude is [`authlat`] with them, for |n| < 0.01.
pub(crate) fn authset_forward(es: f64) -> [f64; 6] {
    auxlat_coeffs(es, &C_XI_PHI)
}

const C_PHI_XI: [f64; 21] = [
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

const C_XI_PHI: [f64; 21] = [
    -4.0 / 3.0,
    -4.0 / 45.0,
    88.0 / 315.0,
    538.0 / 4725.0,
    20824.0 / 467775.0,
    -44732.0 / 2837835.0,
    34.0 / 45.0,
    8.0 / 105.0,
    -2482.0 / 14175.0,
    -37192.0 / 467775.0,
    -12467764.0 / 212837625.0,
    -1532.0 / 2835.0,
    -898.0 / 14175.0,
    54968.0 / 467775.0,
    100320856.0 / 1915538625.0,
    6007.0 / 14175.0,
    24496.0 / 467775.0,
    -5884124.0 / 70945875.0,
    -23356.0 / 66825.0,
    -839792.0 / 19348875.0,
    570284222.0 / 1915538625.0,
];

/// The six coefficients of a series in the third flattening n from PROJ's table of 21.
fn auxlat_coeffs(es: f64, c: &[f64; 21]) -> [f64; 6] {
    let b = (1.0 - es).sqrt();
    let n = (1.0 - b) / (1.0 + b);
    let mut apa = [0.0; 6];
    let (mut o, mut d) = (0, n);
    for (l, a) in apa.iter_mut().enumerate() {
        // A polynomial in n of degree 5 - l, by Horner's method.
        let p = &c[o..o + 6 - l];
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

/// `asinh` and `atanh` bit for bit glibc's (2.39): digests of their results over more than a
/// million inputs each, edge cases included, computed with Python's `math` module (which calls
/// glibc) over the inputs `inputs()` below generates, in the same order:
///
/// ```text
/// asinh: n=1450179 digest=0xa5618f3884af810c
/// atanh: n=1075082 digest=0xf9ea50ee57c7ddb2
/// ```
#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
mod glibc_tests {
    use super::{asinh, atanh};

    /// The SplitMix64 generator.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    /// The next value toward +infinity (C's `nextafter(x, INFINITY)`).
    fn up(x: f64) -> f64 {
        if x.is_nan() || x == f64::INFINITY {
            x
        } else if x == 0.0 {
            f64::from_bits(1)
        } else if x > 0.0 {
            f64::from_bits(x.to_bits() + 1)
        } else {
            f64::from_bits(x.to_bits() - 1)
        }
    }

    /// The inputs, in the order the reference digests were computed over.
    fn inputs() -> Vec<f64> {
        let two = |k: i64| f64::from_bits(((1023 + k) as u64) << 52);
        let mut v = Vec::new();
        let edges = [0.0, 5e-324, 2.2250738585072014e-308, two(-28), 0.5, 1.0, 2.0, two(28), 1e-300, 1e300, f64::MAX, f64::INFINITY];
        for e in edges {
            for x in [e, up(e), -up(-e)] {
                v.push(x);
                v.push(-x);
            }
        }
        v.push(f64::NAN);
        for k in 1..54 {
            v.push(1.0 - two(-k));
            v.push(-(1.0 - two(-k)));
        }
        let mut r = SplitMix64(0x5EED);
        for _ in 0..300_000 {
            v.push(f64::from_bits(r.next()));
        }
        for _ in 0..300_000 {
            v.push(((r.next() >> 11) as f64 * two(-53)) * 2.0 - 1.0);
        }
        for _ in 0..250_000 {
            let u = r.next();
            let exp = 1023 - 40 + (u >> 52) % 81;
            v.push(f64::from_bits(((u >> 63) << 63) | (exp << 52) | (u & ((1 << 52) - 1))));
        }
        for _ in 0..400_000 {
            let u = r.next();
            let x = (u >> 11) as f64 * two(-53);
            v.push(if u & 1 == 1 { -x } else { x });
        }
        for _ in 0..200_000 {
            // 3.14, not pi: latitudes just inside the poles, as the reference generator made them
            // (the digests are of exactly these values).
            #[allow(clippy::approx_constant)]
            let phi = ((r.next() >> 11) as f64 * two(-53) - 0.5) * 3.14;
            v.push(phi.tan());
        }
        v
    }

    #[test]
    fn asinh_and_atanh_are_glibc_s() {
        let fold = |h: u64, x: f64| (h ^ x.to_bits()).wrapping_mul(0x100_0000_01B3);
        let (mut h_asinh, mut h_atanh, mut n_asinh, mut n_atanh) = (0xCBF2_9CE4_8422_2325, 0xCBF2_9CE4_8422_2325, 0, 0);
        for x in inputs() {
            h_asinh = fold(h_asinh, asinh(x));
            n_asinh += 1;
            if x.is_nan() || x.abs() < 1.0 {
                h_atanh = fold(h_atanh, atanh(x));
                n_atanh += 1;
            }
        }
        assert_eq!((n_asinh, n_atanh), (1_450_179, 1_075_082));
        assert_eq!(h_asinh, 0xa561_8f38_84af_810c, "asinh differs from glibc's");
        assert_eq!(h_atanh, 0xf9ea_50ee_57c7_ddb2, "atanh differs from glibc's");
    }
}

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

    #[test]
    // Away from the poles, where asin(q / qp) loses precision.
    fn forward_authalic_series_is_the_closed_form_on_wgs84() {
        let ellipsoid = Ell::named("WGS84").unwrap();
        let apa = authset_forward(ellipsoid.es);
        let qp = qsfn(1.0, ellipsoid.e, ellipsoid.one_es);
        for degrees in [-80.0_f64, -60.0, -10.0, 1.0, 45.0, 75.0] {
            let phi = degrees.to_radians();
            let closed = (qsfn(phi.sin(), ellipsoid.e, ellipsoid.one_es) / qp).asin();
            assert!((authlat(phi, &apa) - closed).abs() < 1e-14, "{degrees}");
        }
    }
}
