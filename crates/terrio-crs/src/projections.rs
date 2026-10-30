// SPDX-FileCopyrightText: PROJ contributors
// SPDX-FileCopyrightText: Copyright (c) 1995, Gerald Evenden
// SPDX-FileCopyrightText: Copyright (c) 2003   Gerald I. Evenden
// SPDX-FileCopyrightText: Changes to handle +over are: Copyright 2011-2014 Morelli Informatik
// SPDX-FileCopyrightText: Copyright (c) 2003, 2006   Gerald I. Evenden
// SPDX-FileCopyrightText: Copyright (c) 2001, Thomas Flemming, tf@ttqv.com
// SPDX-FileCopyrightText: Copyright (c) 2004   Gerald I. Evenden
// SPDX-FileCopyrightText: Copyright (c) 2012   Martin Raspaud
// SPDX-FileCopyrightText: Copyright (c) 2011, 2012  Martin Lambers <marlam@marlam.de>
// SPDX-FileCopyrightText: Copyright (c) 2017-2019 Heidelberg University
// SPDX-FileCopyrightText: Copyright (c) 2018, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2025 Javier Jimenez Shaw
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   PROJ 9.8.1:
//     src/projections/tmerc.cpp
//     src/projections/lcc.cpp
//     src/projections/merc.cpp
//     src/projections/aea.cpp
//     src/projections/eqdc.cpp
//     src/projections/laea.cpp
//     src/projections/stere.cpp
//     src/projections/sterea.cpp
//     src/gauss.cpp
//     src/projections/somerc.cpp
//     src/projections/cass.cpp
//     src/projections/poly.cpp
//     src/projections/eqc.cpp
//     src/projections/gn_sinu.cpp
//     src/projections/bonne.cpp
//     src/projections/mill.cpp
//     src/projections/vandg.cpp
//     src/projections/gnom.cpp
//     src/projections/cea.cpp
//     src/projections/eqearth.cpp
//     src/projections/omerc.cpp
//     src/projections/krovak.cpp
//     src/projections/labrd.cpp
//     src/projections/nzmg.cpp
//     src/zpoly1.cpp
//     src/projections/col_urban.cpp
//     src/projections/aeqd.cpp
//     src/projections/ortho.cpp
//     src/projections/geos.cpp
//     src/projections/robin.cpp
//     src/projections/healpix.cpp
//     src/projections/igh.cpp
//     src/projections/qsc.cpp
//     src/projections/isea.cpp
//     src/projections/aitoff.cpp
//     src/projections/wink1.cpp
//     src/projections/wink2.cpp
//     src/projections/gall.cpp
//     src/projections/gstmerc.cpp
//     src/projections/fouc_s.cpp
//     src/projections/putp4p.cpp
//     src/projections/sconics.cpp
//     src/generic_inverse.cpp
//     src/latitudes.cpp
//     src/ell_set.cpp
//     src/aasincos.cpp
//   PROJ 5fa31c29f0a806568ed7bc76c1a557604db4a804:
//     src/projections/moll.cpp

//! The map projections EPSG CRSs use, ported from PROJ (<https://proj.org>, MIT licence) so that
//! terrio's results match GDAL's. PROJ's copyright and permission notice is in `NOTICE`.
//!
//! As in PROJ, a projection works on an ellipsoid with a semi-major axis of 1: the forward
//! projection takes the longitude relative to the central meridian and the latitude in radians,
//! and the caller scales by the semi-major axis and adds the false easting and northing.

// The constants are PROJ's, digit for digit.
#![allow(clippy::excessive_precision)]

use super::math::*;
use super::params::Params;
use crate::{Error, Result};
use std::f64::consts::PI;

mod perspective;

/// A map projection's forward and inverse formulas.
pub(crate) trait Proj: Send + Sync + std::fmt::Debug {
    /// Projects a longitude (relative to the central meridian) and latitude in radians.
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)>;
    /// Unprojects normalized coordinates into a longitude (relative to the central meridian) and
    /// a latitude in radians.
    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)>;
}

/// What a projection's setup reads and may change: the ellipsoid, origin, scale and false
/// easting and northing.
pub(crate) struct Setup<'a> {
    pub p: &'a Params,
    pub ell: Ell,
    pub phi0: f64,
    pub lam0: f64,
    pub k0: f64,
    pub x0: f64,
    pub y0: f64,
}

fn err(msg: &str) -> Error {
    Error::Crs(msg.to_string())
}

/// An integer parameter as PROJ's pj_param reads it: digits only (else an invalid value), then
/// atoi, which reads a C long (saturating) and casts it to int; 0 when absent or empty.
fn proj_int(p: &Params, method: &str, key: &str) -> Result<i32> {
    let v = p.str(key).unwrap_or("");
    if !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err(&format!("{method}: invalid {key}, it should be an integer")));
    }
    Ok(if v.is_empty() { 0 } else { v.parse::<i64>().unwrap_or(i64::MAX) as i32 })
}

/// Builds the projection `name`. `Ok(None)` when terrio does not implement it.
pub(crate) fn build(name: &str, s: &mut Setup<'_>) -> Result<Option<Box<dyn Proj>>> {
    Ok(Some(match name {
        "nsper" | "tpers" => Box::new(perspective::Perspective::new(s, name == "tpers")?),
        "tmerc" | "etmerc" => Box::new(Tmerc::new(s.ell, s.k0, s.phi0)),
        "utm" => {
            let zone = s.p.f64("zone").ok_or_else(|| err("utm needs +zone"))?;
            if !(1.0..=60.0).contains(&zone) {
                return Err(err("invalid UTM zone"));
            }
            s.lam0 = ((zone.trunc() - 1.0) * 6.0 - 180.0 + 3.0).to_radians();
            s.k0 = 0.9996;
            s.phi0 = 0.0;
            s.x0 = 500000.0;
            s.y0 = if s.p.has("south") { 10000000.0 } else { 0.0 };
            Box::new(Tmerc::new(s.ell, s.k0, 0.0))
        }
        "lcc" => Box::new(Lcc::new(s)?),
        "merc" => Box::new(Merc::new(s)?),
        // Web Mercator: the spherical formulas on the semi-major axis, with geodetic latitudes
        // and a scale of 1 whatever `k_0`, as PROJ's `webmerc`.
        "webmerc" => {
            s.k0 = 1.0;
            Box::new(Merc { e: 0.0, k0: 1.0 })
        }
        "aea" => Box::new(Aea::new(s)?),
        "eqdc" => Box::new(Eqdc::new(s)?),
        "laea" => Box::new(Laea::new(s)),
        "stere" => Box::new(Stere::new(s)),
        "ups" => {
            s.phi0 = if s.p.has("south") { -HALFPI } else { HALFPI };
            s.k0 = 0.994;
            s.x0 = 2000000.0;
            s.y0 = 2000000.0;
            s.lam0 = 0.0;
            Box::new(Stere::new(s))
        }
        "sterea" => Box::new(Sterea::new(s)?),
        "somerc" => Box::new(Somerc::new(s)),
        "cass" => Box::new(Cass::new(s)),
        "poly" => Box::new(Poly::new(s)),
        "eqc" => Box::new(Eqc::new(s)?),
        "sinu" => Box::new(Sinu::new(s)),
        "bonne" => Box::new(Bonne::new(s)?),
        "mill" => Box::new(Mill::new(s)),
        "vandg" => Box::new(Vandg::new(s)),
        "gnom" => Box::new(Gnom::new(s)),
        "cea" => Box::new(Cea::new(s)?),
        "eqearth" => Box::new(EqEarth::new(s)),
        "omerc" => Box::new(Omerc::new(s)?),
        "krovak" => Box::new(Krovak::new(s, false)?),
        "mod_krovak" => Box::new(Krovak::new(s, true)?),
        "labrd" => Box::new(Labrd::new(s)),
        "nzmg" => Box::new(Nzmg::new(s)),
        "col_urban" => Box::new(ColUrban::new(s)),
        "aeqd" => Box::new(Aeqd::new(s)),
        "ortho" => Box::new(Ortho::new(s)),
        "geos" => Box::new(Geos::new(s)?),
        "robin" => Box::new(Robin),
        "moll" | "wag4" | "wag5" => Box::new(Moll::new(name)),
        "healpix" => Box::new(Healpix::new(s, false)?),
        "rhealpix" => Box::new(Healpix::new(s, true)?),
        "igh" => Box::new(Igh::new()?),
        "qsc" => Box::new(Qsc::new(s)),
        "isea" => Box::new(Isea::new(s)?),
        "wintri" => Box::new(Wintri::new(s)?),
        "wink1" => Box::new(Wink1 { cosphi1: s.p.angle("lat_ts").unwrap_or(0.0).cos() }),
        "wink2" => Box::new(Wink2 { cosphi1: s.p.angle("lat_1").unwrap_or(0.0).cos() }),
        "gall" => Box::new(Gall),
        "gstmerc" => Box::new(Gstmerc::new(s)),
        "fouc_s" => Box::new(FoucS::new(s)?),
        "mbtfps" => Box::new(Mbtfps::new()),
        "putp4p" => Box::new(Putp4p { cx: 0.874038744, cy: 3.883251825 }),
        "weren" => Box::new(Putp4p { cx: 1.0, cy: 4.442882938 }),
        "murd2" => Box::new(Sconic::new(s, SconicKind::Murd2)?),
        "tissot" => Box::new(Sconic::new(s, SconicKind::Tissot)?),
        _ => return Ok(None),
    }))
}

// ---------------------------------------------------------------------------------------------
// Transverse Mercator: the "exact" Poder/Engsager algorithm PROJ uses by default.

const ETMERC_ORDER: usize = 6;

/// Clenshaw summation of a real series of sin(2kB) (PROJ's `gatg`).
fn gatg(p: &[f64; ETMERC_ORDER], b: f64) -> f64 {
    let cos_2b = (2.0 * b).cos();
    let two_cos_2b = 2.0 * cos_2b;
    let (mut h, mut h1, mut h2) = (0.0, p[ETMERC_ORDER - 1], 0.0);
    for i in (0..ETMERC_ORDER - 1).rev() {
        h = -h2 + two_cos_2b * h1 + p[i];
        h2 = h1;
        h1 = h;
    }
    b + h * (2.0 * b).sin()
}

/// Clenshaw summation of a complex series (PROJ's `clenS`); returns the real and imaginary parts.
fn clen_s(a: &[f64; ETMERC_ORDER], arg_r: f64, arg_i: f64) -> (f64, f64) {
    let (sin_r, cos_r) = arg_r.sin_cos();
    let (sinh_i, cosh_i) = (arg_i.sinh(), arg_i.cosh());
    let r = 2.0 * cos_r * cosh_i;
    let i = -2.0 * sin_r * sinh_i;
    let (mut hr, mut hr1, mut hi, mut hi1) = (a[ETMERC_ORDER - 1], 0.0, 0.0, 0.0);
    for k in (0..ETMERC_ORDER - 1).rev() {
        let (hr2, hi2) = (hr1, hi1);
        hr1 = hr;
        hi1 = hi;
        hr = -hr2 + r * hr1 - i * hi1 + a[k];
        hi = -hi2 + i * hr1 + r * hi1;
    }
    let r = sin_r * cosh_i;
    let i = cos_r * sinh_i;
    (r * hr - i * hi, r * hi + i * hr)
}

#[derive(Debug)]
struct Tmerc {
    qn: f64,
    zb: f64,
    cgb: [f64; ETMERC_ORDER],
    cbg: [f64; ETMERC_ORDER],
    utg: [f64; ETMERC_ORDER],
    gtu: [f64; ETMERC_ORDER],
}

impl Tmerc {
    fn new(ell: Ell, k0: f64, phi0: f64) -> Self {
        let f = ell.es / (1.0 + (1.0 - ell.es).sqrt());
        let n = f / (2.0 - f);
        let mut np = n;
        let mut cgb = [0.0; 6];
        let mut cbg = [0.0; 6];
        let mut utg = [0.0; 6];
        let mut gtu = [0.0; 6];
        cgb[0] = n * (2.0 + n * (-2.0 / 3.0 + n * (-2.0 + n * (116.0 / 45.0 + n * (26.0 / 45.0 + n * (-2854.0 / 675.0))))));
        cbg[0] = n * (-2.0 + n * (2.0 / 3.0 + n * (4.0 / 3.0 + n * (-82.0 / 45.0 + n * (32.0 / 45.0 + n * (4642.0 / 4725.0))))));
        np *= n;
        cgb[1] = np * (7.0 / 3.0 + n * (-8.0 / 5.0 + n * (-227.0 / 45.0 + n * (2704.0 / 315.0 + n * (2323.0 / 945.0)))));
        cbg[1] = np * (5.0 / 3.0 + n * (-16.0 / 15.0 + n * (-13.0 / 9.0 + n * (904.0 / 315.0 + n * (-1522.0 / 945.0)))));
        np *= n;
        cgb[2] = np * (56.0 / 15.0 + n * (-136.0 / 35.0 + n * (-1262.0 / 105.0 + n * (73814.0 / 2835.0))));
        cbg[2] = np * (-26.0 / 15.0 + n * (34.0 / 21.0 + n * (8.0 / 5.0 + n * (-12686.0 / 2835.0))));
        np *= n;
        cgb[3] = np * (4279.0 / 630.0 + n * (-332.0 / 35.0 + n * (-399572.0 / 14175.0)));
        cbg[3] = np * (1237.0 / 630.0 + n * (-12.0 / 5.0 + n * (-24832.0 / 14175.0)));
        np *= n;
        cgb[4] = np * (4174.0 / 315.0 + n * (-144838.0 / 6237.0));
        cbg[4] = np * (-734.0 / 315.0 + n * (109598.0 / 31185.0));
        np *= n;
        cgb[5] = np * (601676.0 / 22275.0);
        cbg[5] = np * (444337.0 / 155925.0);

        np = n * n;
        let qn = k0 / (1.0 + n) * (1.0 + np * (1.0 / 4.0 + np * (1.0 / 64.0 + np / 256.0)));
        utg[0] = n * (-0.5 + n * (2.0 / 3.0 + n * (-37.0 / 96.0 + n * (1.0 / 360.0 + n * (81.0 / 512.0 + n * (-96199.0 / 604800.0))))));
        gtu[0] = n * (0.5 + n * (-2.0 / 3.0 + n * (5.0 / 16.0 + n * (41.0 / 180.0 + n * (-127.0 / 288.0 + n * (7891.0 / 37800.0))))));
        utg[1] = np * (-1.0 / 48.0 + n * (-1.0 / 15.0 + n * (437.0 / 1440.0 + n * (-46.0 / 105.0 + n * (1118711.0 / 3870720.0)))));
        gtu[1] = np * (13.0 / 48.0 + n * (-3.0 / 5.0 + n * (557.0 / 1440.0 + n * (281.0 / 630.0 + n * (-1983433.0 / 1935360.0)))));
        np *= n;
        utg[2] = np * (-17.0 / 480.0 + n * (37.0 / 840.0 + n * (209.0 / 4480.0 + n * (-5569.0 / 90720.0))));
        gtu[2] = np * (61.0 / 240.0 + n * (-103.0 / 140.0 + n * (15061.0 / 26880.0 + n * (167603.0 / 181440.0))));
        np *= n;
        utg[3] = np * (-4397.0 / 161280.0 + n * (11.0 / 504.0 + n * (830251.0 / 7257600.0)));
        gtu[3] = np * (49561.0 / 161280.0 + n * (-179.0 / 168.0 + n * (6601661.0 / 7257600.0)));
        np *= n;
        utg[4] = np * (-4583.0 / 161280.0 + n * (108847.0 / 3991680.0));
        gtu[4] = np * (34729.0 / 80640.0 + n * (-3418889.0 / 1995840.0));
        np *= n;
        utg[5] = np * (-20648693.0 / 638668800.0);
        gtu[5] = np * (212378941.0 / 319334400.0);

        let z = gatg(&cbg, phi0);
        let zb = -qn * (z + clen_s(&gtu, 2.0 * z, 0.0).0);
        Self { qn, zb, cgb, cbg, utg, gtu }
    }
}

impl Proj for Tmerc {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let cn = gatg(&self.cbg, phi);
        let (sin_cn, cos_cn) = cn.sin_cos();
        let (sin_ce, cos_ce) = lam.sin_cos();
        let cn = sin_cn.atan2(cos_ce * cos_cn);
        let ce = (sin_ce * cos_cn).atan2(sin_cn.hypot(cos_cn * cos_ce));
        let ce = ce.tan().asinh();
        let (dcn, dce) = clen_s(&self.gtu, 2.0 * cn, 2.0 * ce);
        let (cn, ce) = (cn + dcn, ce + dce);
        if ce.abs() > 2.623395162778 {
            return None;
        }
        Some((self.qn * ce, self.qn * cn + self.zb))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let cn = (y - self.zb) / self.qn;
        let ce = x / self.qn;
        if ce.abs() > 2.623395162778 {
            return None;
        }
        let (dcn, dce) = clen_s(&self.utg, 2.0 * cn, 2.0 * ce);
        let (cn, ce) = (cn + dcn, ce + dce);
        let ce = ce.sinh().atan();
        let (sin_cn, cos_cn) = cn.sin_cos();
        let (sin_ce, cos_ce) = ce.sin_cos();
        let lam = sin_ce.atan2(cos_ce * cos_cn);
        let cn = (sin_cn * cos_ce).atan2(sin_ce.hypot(cos_ce * cos_cn));
        Some((lam, gatg(&self.cgb, cn)))
    }
}

// ---------------------------------------------------------------------------------------------
// Lambert Conformal Conic.

#[derive(Debug)]
struct Lcc {
    e: f64,
    n: f64,
    c: f64,
    rho0: f64,
    k0: f64,
    ellips: bool,
}

impl Lcc {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let phi1 = s.p.angle("lat_1").unwrap_or(0.0);
        // Without `lat_2`: one standard parallel with `lat_0` (Lambert Conic Conformal (1SP), or
        // its variant B), else a second parallel on the equator, as PROJ reads such a CRS (2SP,
        // the parameters left out at 0). PROJ's `lcc` operation would put both on `lat_1`.
        let phi2 = match s.p.angle("lat_2") {
            Some(v) => v,
            None if s.p.angle("lat_0").is_some() => phi1,
            None => 0.0,
        };
        if (phi1 + phi2).abs() < 1e-10 {
            return Err(err("lcc: lat_1 = -lat_2"));
        }
        let e = s.ell.e;
        let es = s.ell.es;
        let mut sinphi = phi1.sin();
        let mut n = sinphi;
        let cosphi = phi1.cos();
        let secant = (phi1 - phi2).abs() >= 1e-10;
        let (c, rho0);
        let phi0 = s.phi0;
        if es != 0.0 {
            let m1 = msfn(sinphi, cosphi, es);
            let ml1 = tsfn(phi1, sinphi, e);
            if secant {
                sinphi = phi2.sin();
                n = (m1 / msfn(sinphi, phi2.cos(), es)).ln();
                n /= (ml1 / tsfn(phi2, sinphi, e)).ln();
            }
            c = m1 * ml1.powf(-n) / n;
            rho0 = if (phi0.abs() - HALFPI).abs() < 1e-10 { 0.0 } else { c * tsfn(phi0, phi0.sin(), e).powf(n) };
        } else {
            if secant {
                n = (cosphi / phi2.cos()).ln() / ((FORTPI + 0.5 * phi2).tan() / (FORTPI + 0.5 * phi1).tan()).ln();
            }
            c = cosphi * (FORTPI + 0.5 * phi1).tan().powf(n) / n;
            rho0 = if (phi0.abs() - HALFPI).abs() < 1e-10 { 0.0 } else { c * (FORTPI + 0.5 * phi0).tan().powf(-n) };
        }
        Ok(Self { e, n, c, rho0, k0: s.k0, ellips: es != 0.0 })
    }
}

impl Proj for Lcc {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let rho = if (phi.abs() - HALFPI).abs() < 1e-10 {
            if phi * self.n <= 0.0 {
                return None;
            }
            0.0
        } else if self.ellips {
            self.c * tsfn(phi, phi.sin(), self.e).powf(self.n)
        } else {
            self.c * (FORTPI + 0.5 * phi).tan().powf(-self.n)
        };
        let lam = lam * self.n;
        Some((self.k0 * (rho * lam.sin()), self.k0 * (self.rho0 - rho * lam.cos())))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (mut x, mut y) = (x / self.k0, y / self.k0);
        y = self.rho0 - y;
        let mut rho = x.hypot(y);
        if rho == 0.0 {
            return Some((0.0, if self.n > 0.0 { HALFPI } else { -HALFPI }));
        }
        if self.n < 0.0 {
            rho = -rho;
            x = -x;
            y = -y;
        }
        let phi = if self.ellips {
            phi2((rho / self.c).powf(1.0 / self.n), self.e)
        } else {
            2.0 * (self.c / rho).powf(1.0 / self.n).atan() - HALFPI
        };
        Some((x.atan2(y) / self.n, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Mercator.

#[derive(Debug)]
struct Merc {
    e: f64,
    k0: f64,
}

impl Merc {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        if let Some(ts) = s.p.angle("lat_ts") {
            let phits = ts.abs();
            if phits >= HALFPI {
                return Err(err("merc: lat_ts must be less than 90°"));
            }
            s.k0 = if s.ell.es != 0.0 { msfn(phits.sin(), phits.cos(), s.ell.es) } else { phits.cos() };
        }
        Ok(Self { e: s.ell.e, k0: s.k0 })
    }
}

impl Proj for Merc {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        if (phi.abs() - HALFPI).abs() <= 1e-10 {
            return None;
        }
        let y = if self.e == 0.0 { phi.tan().asinh() } else { phi.tan().asinh() - self.e * (self.e * phi.sin()).atanh() };
        Some((self.k0 * lam, self.k0 * y))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let phi = if self.e == 0.0 { (y / self.k0).sinh().atan() } else { sinhpsi2tanphi((y / self.k0).sinh(), self.e).atan() };
        Some((x / self.k0, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Albers Equal Area.

#[derive(Debug)]
struct Aea {
    e: f64,
    one_es: f64,
    n: f64,
    n2: f64,
    c: f64,
    dd: f64,
    rho0: f64,
    ec: f64,
    ellips: bool,
}

impl Aea {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let phi1 = s.p.angle("lat_1").unwrap_or(0.0);
        let phi2 = s.p.angle("lat_2").unwrap_or(0.0);
        if (phi1 + phi2).abs() < 1e-10 {
            return Err(err("aea: lat_1 = -lat_2"));
        }
        let (e, es, one_es) = (s.ell.e, s.ell.es, s.ell.one_es);
        let mut sinphi = phi1.sin();
        let mut n = sinphi;
        let mut cosphi = phi1.cos();
        let secant = (phi1 - phi2).abs() >= 1e-10;
        let ellips = es > 0.0;
        let (c, dd, rho0, mut ec, n2);
        ec = 0.0;
        if ellips {
            let m1 = msfn(sinphi, cosphi, es);
            let ml1 = qsfn(sinphi, e, one_es);
            if secant {
                sinphi = phi2.sin();
                cosphi = phi2.cos();
                let m2 = msfn(sinphi, cosphi, es);
                let ml2 = qsfn(sinphi, e, one_es);
                if ml2 == ml1 {
                    return Err(err("aea: invalid standard parallels"));
                }
                n = (m1 * m1 - m2 * m2) / (ml2 - ml1);
            }
            ec = 1.0 - 0.5 * one_es * ((1.0 - e) / (1.0 + e)).ln() / e;
            c = m1 * m1 + n * ml1;
            dd = 1.0 / n;
            rho0 = dd * (c - n * qsfn(s.phi0.sin(), e, one_es)).sqrt();
            n2 = n + n;
        } else {
            if secant {
                n = 0.5 * (n + phi2.sin());
            }
            n2 = n + n;
            c = cosphi * cosphi + n2 * sinphi;
            dd = 1.0 / n;
            rho0 = dd * (c - n2 * s.phi0.sin()).sqrt();
        }
        Ok(Self { e, one_es, n, n2, c, dd, rho0, ec, ellips })
    }

    fn phi1(qs: f64, e: f64, one_es: f64) -> Option<f64> {
        let mut phi = (0.5 * qs).asin();
        if e < 1e-7 {
            return Some(phi);
        }
        for _ in 0..15 {
            let (sinpi, cospi) = phi.sin_cos();
            let con = e * sinpi;
            let com = 1.0 - con * con;
            let dphi = 0.5 * com * com / cospi * (qs / one_es - sinpi / com + 0.5 / e * ((1.0 - con) / (1.0 + con)).ln());
            phi += dphi;
            if dphi.abs() <= 1e-10 {
                return Some(phi);
            }
        }
        None
    }
}

impl Proj for Aea {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let rho = self.c - if self.ellips { self.n * qsfn(phi.sin(), self.e, self.one_es) } else { self.n2 * phi.sin() };
        if rho < 0.0 {
            return None;
        }
        let rho = self.dd * rho.sqrt();
        let lam = lam * self.n;
        Some((rho * lam.sin(), self.rho0 - rho * lam.cos()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (mut x, mut y) = (x, self.rho0 - y);
        let mut rho = x.hypot(y);
        if rho == 0.0 {
            return Some((0.0, if self.n > 0.0 { HALFPI } else { -HALFPI }));
        }
        if self.n < 0.0 {
            rho = -rho;
            x = -x;
            y = -y;
        }
        let mut phi = rho / self.dd;
        if self.ellips {
            phi = (self.c - phi * phi) / self.n;
            if (self.ec - phi.abs()).abs() > 1e-7 {
                phi = Self::phi1(phi, self.e, self.one_es)?;
            } else {
                phi = if phi < 0.0 { -HALFPI } else { HALFPI };
            }
        } else {
            phi = (self.c - phi * phi) / self.n2;
            phi = if phi.abs() <= 1.0 {
                phi.asin()
            } else if phi < 0.0 {
                -HALFPI
            } else {
                HALFPI
            };
        }
        Some((x.atan2(y) / self.n, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Equidistant Conic.

#[derive(Debug)]
struct Eqdc {
    en: Mlfn,
    n: f64,
    c: f64,
    rho0: f64,
    ellips: bool,
}

impl Eqdc {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let phi1 = s.p.angle("lat_1").unwrap_or(0.0);
        let phi2 = s.p.angle("lat_2").unwrap_or(0.0);
        if (phi1 + phi2).abs() < 1e-10 {
            return Err(err("eqdc: lat_1 = -lat_2"));
        }
        let es = s.ell.es;
        let en = Mlfn::new(es);
        let (sinphi, cosphi) = phi1.sin_cos();
        let secant = (phi1 - phi2).abs() >= 1e-10;
        let ellips = es > 0.0;
        let mut n = sinphi;
        let (c, rho0);
        if ellips {
            let m1 = msfn(sinphi, cosphi, es);
            let ml1 = en.mlfn(phi1, sinphi, cosphi);
            if secant {
                let (sinphi2, cosphi2) = phi2.sin_cos();
                n = (m1 - msfn(sinphi2, cosphi2, es)) / (en.mlfn(phi2, sinphi2, cosphi2) - ml1);
                if n == 0.0 {
                    return Err(err("eqdc: invalid standard parallels"));
                }
            }
            c = ml1 + m1 / n;
            rho0 = c - en.mlfn(s.phi0, s.phi0.sin(), s.phi0.cos());
        } else {
            if secant {
                n = (cosphi - phi2.cos()) / (phi2 - phi1);
            }
            if n == 0.0 {
                return Err(err("eqdc: invalid standard parallels"));
            }
            c = phi1 + cosphi / n;
            rho0 = c - s.phi0;
        }
        Ok(Self { en, n, c, rho0, ellips })
    }
}

impl Proj for Eqdc {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let rho = self.c - if self.ellips { self.en.mlfn(phi, phi.sin(), phi.cos()) } else { phi };
        let lam = lam * self.n;
        Some((rho * lam.sin(), self.rho0 - rho * lam.cos()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (mut x, mut y) = (x, self.rho0 - y);
        let mut rho = x.hypot(y);
        if rho == 0.0 {
            return Some((0.0, if self.n > 0.0 { HALFPI } else { -HALFPI }));
        }
        if self.n < 0.0 {
            rho = -rho;
            x = -x;
            y = -y;
        }
        let phi = self.c - rho;
        let phi = if self.ellips { self.en.inv(phi) } else { phi };
        Some((x.atan2(y) / self.n, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Lambert Azimuthal Equal Area.

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    NPole,
    SPole,
    Equit,
    Obliq,
}

fn azimuthal_mode(phi0: f64) -> Mode {
    let t = phi0.abs();
    if (t - HALFPI).abs() < 1e-10 {
        if phi0 < 0.0 { Mode::SPole } else { Mode::NPole }
    } else if t < 1e-10 {
        Mode::Equit
    } else {
        Mode::Obliq
    }
}

#[derive(Debug)]
struct Laea {
    mode: Mode,
    e: f64,
    es: f64,
    one_es: f64,
    phi0: f64,
    qp: f64,
    rq: f64,
    dd: f64,
    xmf: f64,
    ymf: f64,
    sinb1: f64,
    cosb1: f64,
    apa: [f64; 6],
}

impl Laea {
    fn new(s: &mut Setup<'_>) -> Self {
        let mode = azimuthal_mode(s.phi0);
        let (e, es, one_es) = (s.ell.e, s.ell.es, s.ell.one_es);
        let mut l = Self {
            mode,
            e,
            es,
            one_es,
            phi0: s.phi0,
            qp: 0.0,
            rq: 0.0,
            dd: 0.0,
            xmf: 0.0,
            ymf: 0.0,
            sinb1: 0.0,
            cosb1: 0.0,
            apa: [0.0; 6],
        };
        if es != 0.0 {
            l.qp = qsfn(1.0, e, one_es);
            l.apa = authset(es);
            match mode {
                Mode::NPole | Mode::SPole => l.dd = 1.0,
                Mode::Equit => {
                    l.rq = (0.5 * l.qp).sqrt();
                    l.dd = 1.0 / l.rq;
                    l.xmf = 1.0;
                    l.ymf = 0.5 * l.qp;
                }
                Mode::Obliq => {
                    l.rq = (0.5 * l.qp).sqrt();
                    let sinphi = s.phi0.sin();
                    l.sinb1 = qsfn(sinphi, e, one_es) / l.qp;
                    l.cosb1 = (1.0 - l.sinb1 * l.sinb1).sqrt();
                    l.dd = s.phi0.cos() / ((1.0 - es * sinphi * sinphi).sqrt() * l.rq * l.cosb1);
                    l.xmf = l.rq;
                    l.ymf = l.rq / l.dd;
                    l.xmf *= l.dd;
                }
            }
        } else if mode == Mode::Obliq {
            l.sinb1 = s.phi0.sin();
            l.cosb1 = s.phi0.cos();
        }
        l
    }
}

impl Proj for Laea {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (sinlam, coslam) = lam.sin_cos();
        if self.es == 0.0 {
            let (sinphi, cosphi) = phi.sin_cos();
            return match self.mode {
                Mode::Equit | Mode::Obliq => {
                    let y = if self.mode == Mode::Equit {
                        1.0 + cosphi * coslam
                    } else {
                        1.0 + self.sinb1 * sinphi + self.cosb1 * cosphi * coslam
                    };
                    if y <= 1e-10 {
                        return None;
                    }
                    let y = (2.0 / y).sqrt();
                    let x = y * cosphi * sinlam;
                    let y = y * if self.mode == Mode::Equit { sinphi } else { self.cosb1 * sinphi - self.sinb1 * cosphi * coslam };
                    Some((x, y))
                }
                Mode::NPole | Mode::SPole => {
                    let coslam = if self.mode == Mode::NPole { -coslam } else { coslam };
                    if (phi + self.phi0).abs() < 1e-10 {
                        return None;
                    }
                    let y = FORTPI - phi * 0.5;
                    let y = 2.0 * if self.mode == Mode::SPole { y.cos() } else { y.sin() };
                    Some((y * sinlam, y * coslam))
                }
            };
        }
        let sinphi = phi.sin();
        let mut q = qsfn(sinphi, self.e, self.one_es);
        let (mut sinb, mut cosb) = (0.0, 0.0);
        if matches!(self.mode, Mode::Obliq | Mode::Equit) {
            sinb = q / self.qp;
            let cosb2 = 1.0 - sinb * sinb;
            cosb = if cosb2 > 0.0 { cosb2.sqrt() } else { 0.0 };
        }
        let b = match self.mode {
            Mode::Obliq => 1.0 + self.sinb1 * sinb + self.cosb1 * cosb * coslam,
            Mode::Equit => 1.0 + cosb * coslam,
            Mode::NPole => {
                q = self.qp - q;
                HALFPI + phi
            }
            Mode::SPole => {
                q += self.qp;
                phi - HALFPI
            }
        };
        if b.abs() < 1e-10 {
            return None;
        }
        match self.mode {
            Mode::Obliq => {
                let b = (2.0 / b).sqrt();
                let y = self.ymf * b * (self.cosb1 * sinb - self.sinb1 * cosb * coslam);
                Some((self.xmf * b * cosb * sinlam, y))
            }
            Mode::Equit => {
                let b = (2.0 / (1.0 + cosb * coslam)).sqrt();
                Some((self.xmf * b * cosb * sinlam, b * sinb * self.ymf))
            }
            Mode::NPole | Mode::SPole => {
                if q >= 1e-15 {
                    let b = q.sqrt();
                    Some((b * sinlam, coslam * if self.mode == Mode::SPole { b } else { -b }))
                } else {
                    Some((0.0, 0.0))
                }
            }
        }
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        if self.es == 0.0 {
            let (mut x, mut y) = (x, y);
            let rh = x.hypot(y);
            let mut phi = rh * 0.5;
            if phi > 1.0 {
                return None;
            }
            phi = 2.0 * phi.asin();
            let (sinz, cosz) = phi.sin_cos();
            match self.mode {
                Mode::Equit => {
                    phi = if rh.abs() <= 1e-10 { 0.0 } else { (y * sinz / rh).asin() };
                    x *= sinz;
                    y = cosz * rh;
                }
                Mode::Obliq => {
                    phi = if rh.abs() <= 1e-10 { self.phi0 } else { (cosz * self.sinb1 + y * sinz * self.cosb1 / rh).asin() };
                    x *= sinz * self.cosb1;
                    y = (cosz - phi.sin() * self.sinb1) * rh;
                }
                Mode::NPole => {
                    y = -y;
                    phi = HALFPI - phi;
                }
                Mode::SPole => phi -= HALFPI,
            }
            let lam = if y == 0.0 && matches!(self.mode, Mode::Equit | Mode::Obliq) { 0.0 } else { x.atan2(y) };
            return Some((lam, phi));
        }
        let (mut x, mut y) = (x, y);
        let ab;
        match self.mode {
            Mode::Equit | Mode::Obliq => {
                x /= self.dd;
                y *= self.dd;
                let rho = x.hypot(y);
                if rho < 1e-10 {
                    return Some((0.0, self.phi0));
                }
                let sce = 2.0 * (0.5 * rho / self.rq).asin();
                let (sce, cce) = sce.sin_cos();
                x *= sce;
                if self.mode == Mode::Obliq {
                    ab = cce * self.sinb1 + y * sce * self.cosb1 / rho;
                    y = rho * self.cosb1 * cce - y * self.sinb1 * sce;
                } else {
                    ab = y * sce / rho;
                    y = rho * cce;
                }
            }
            Mode::NPole | Mode::SPole => {
                if self.mode == Mode::NPole {
                    y = -y;
                }
                let q = x * x + y * y;
                if q == 0.0 {
                    return Some((0.0, self.phi0));
                }
                let a = 1.0 - q / self.qp;
                ab = if self.mode == Mode::SPole { -a } else { a };
            }
        }
        Some((x.atan2(y), authlat_inverse(aasin(ab), &self.apa, self.e, self.one_es, self.qp)))
    }
}

// ---------------------------------------------------------------------------------------------
// Stereographic (polar and oblique, PROJ's `stere`).

#[derive(Debug)]
struct Stere {
    mode: Mode,
    e: f64,
    phi0: f64,
    akm1: f64,
    sin_x1: f64,
    cos_x1: f64,
}

fn ssfn(phit: f64, sinphi: f64, e: f64) -> f64 {
    let sinphi = sinphi * e;
    (0.5 * (HALFPI + phit)).tan() * ((1.0 - sinphi) / (1.0 + sinphi)).powf(0.5 * e)
}

impl Stere {
    fn new(s: &mut Setup<'_>) -> Self {
        // At a pole, `lat_ts` gives the hemisphere, as PROJ reads such a CRS (Polar Stereographic
        // (variant B), whose standard parallel's sign is the pole's); a bare `stere` operation
        // would take `lat_0`'s.
        if let Some(ts) = s.p.angle("lat_ts")
            && (s.phi0.abs() - HALFPI).abs() < 1e-10
        {
            s.phi0 = if ts < 0.0 { -HALFPI } else { HALFPI };
        }
        let phits = s.p.angle("lat_ts").unwrap_or(HALFPI).abs();
        let mode = azimuthal_mode(s.phi0);
        let (e, es, k0, phi0) = (s.ell.e, s.ell.es, s.k0, s.phi0);
        let akm1;
        let (mut sin_x1, mut cos_x1) = (0.0, 0.0);
        if es != 0.0 {
            match mode {
                Mode::NPole | Mode::SPole => {
                    if (phits - HALFPI).abs() < 1e-10 {
                        akm1 = 2.0 * k0 / ((1.0 + e).powf(1.0 + e) * (1.0 - e).powf(1.0 - e)).sqrt();
                    } else {
                        let t = phits.sin();
                        let te = t * e;
                        akm1 = phits.cos() / tsfn(phits, t, e) / (1.0 - te * te).sqrt();
                    }
                }
                Mode::Equit | Mode::Obliq => {
                    let (t, cosphi0) = phi0.sin_cos();
                    // The conformal latitude x of the origin, from T = tan(45° + x/2) without
                    // `atan`: near a pole, `2 atan(T) - 90°` left cos x with a relative error of
                    // about 1e-6 (metres at 4,000 km from an origin 1e-8° off the pole).
                    let tan_half = if phi0 < 0.0 { cosphi0 / (1.0 - t) } else { (1.0 + t) / cosphi0 };
                    let big_t = tan_half * ((1.0 - t * e) / (1.0 + t * e)).powf(0.5 * e);
                    let t = t * e;
                    akm1 = 2.0 * k0 * cosphi0 / (1.0 - t * t).sqrt();
                    sin_x1 = (big_t * big_t - 1.0) / (big_t * big_t + 1.0);
                    cos_x1 = 2.0 * big_t / (big_t * big_t + 1.0);
                }
            }
        } else {
            match mode {
                Mode::Obliq => {
                    sin_x1 = phi0.sin();
                    cos_x1 = phi0.cos();
                    akm1 = 2.0 * k0;
                }
                Mode::Equit => akm1 = 2.0 * k0,
                Mode::NPole | Mode::SPole => {
                    akm1 = if (phits - HALFPI).abs() >= 1e-10 { phits.cos() / (FORTPI - 0.5 * phits).tan() } else { 2.0 * k0 };
                }
            }
        }
        Self { mode, e, phi0, akm1, sin_x1, cos_x1 }
    }
}

impl Proj for Stere {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (sinlam, mut coslam) = lam.sin_cos();
        let mut sinphi = phi.sin();
        if self.e == 0.0 {
            let cosphi = phi.cos();
            return match self.mode {
                Mode::Equit | Mode::Obliq => {
                    let y = if self.mode == Mode::Equit {
                        1.0 + cosphi * coslam
                    } else {
                        1.0 + self.sin_x1 * sinphi + self.cos_x1 * cosphi * coslam
                    };
                    if y <= 1e-10 {
                        return None;
                    }
                    let a = self.akm1 / y;
                    let x = a * cosphi * sinlam;
                    let y = a * if self.mode == Mode::Equit { sinphi } else { self.cos_x1 * sinphi - self.sin_x1 * cosphi * coslam };
                    Some((x, y))
                }
                Mode::NPole | Mode::SPole => {
                    let mut phi = phi;
                    if self.mode == Mode::NPole {
                        coslam = -coslam;
                        phi = -phi;
                    }
                    if (phi - HALFPI).abs() < 1e-8 {
                        return None;
                    }
                    let y = self.akm1 * (FORTPI + 0.5 * phi).tan();
                    Some((sinlam * y, y * coslam))
                }
            };
        }
        let (mut sin_x, mut cos_x) = (0.0, 0.0);
        if matches!(self.mode, Mode::Obliq | Mode::Equit) {
            let x = 2.0 * ssfn(phi, sinphi, self.e).atan() - HALFPI;
            sin_x = x.sin();
            cos_x = x.cos();
        }
        let (x, y) = match self.mode {
            Mode::Obliq => {
                let denom = self.cos_x1 * (1.0 + self.sin_x1 * sin_x + self.cos_x1 * cos_x * coslam);
                if denom == 0.0 {
                    return None;
                }
                let a = self.akm1 / denom;
                (a * cos_x, a * (self.cos_x1 * sin_x - self.sin_x1 * cos_x * coslam))
            }
            Mode::Equit => {
                let denom = 1.0 + cos_x * coslam;
                if denom == 0.0 {
                    return None;
                }
                let a = self.akm1 / denom;
                (a * cos_x, a * sin_x)
            }
            Mode::NPole | Mode::SPole => {
                let mut phi = phi;
                if self.mode == Mode::SPole {
                    phi = -phi;
                    coslam = -coslam;
                    sinphi = -sinphi;
                }
                if (phi - HALFPI).abs() < 1e-15 {
                    return None;
                }
                let x = self.akm1 * tsfn(phi, sinphi, self.e);
                (x, -x * coslam)
            }
        };
        Some((x * sinlam, y))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (mut x, mut y) = (x, y);
        let rho = x.hypot(y);
        if self.e == 0.0 {
            let c = 2.0 * (rho / self.akm1).atan();
            let (sinc, cosc) = c.sin_cos();
            let mut lam = 0.0;
            let phi;
            match self.mode {
                Mode::Equit => {
                    phi = if rho.abs() <= 1e-10 { 0.0 } else { (y * sinc / rho).asin() };
                    if cosc != 0.0 || x != 0.0 {
                        lam = (x * sinc).atan2(cosc * rho);
                    }
                }
                Mode::Obliq => {
                    phi = if rho.abs() <= 1e-10 { self.phi0 } else { (cosc * self.sin_x1 + y * sinc * self.cos_x1 / rho).asin() };
                    let c = cosc - self.sin_x1 * phi.sin();
                    if c != 0.0 || x != 0.0 {
                        lam = (x * sinc * self.cos_x1).atan2(c * rho);
                    }
                }
                Mode::NPole | Mode::SPole => {
                    if self.mode == Mode::NPole {
                        y = -y;
                    }
                    phi = if rho.abs() <= 1e-10 { self.phi0 } else { (if self.mode == Mode::SPole { -cosc } else { cosc }).asin() };
                    lam = if x == 0.0 && y == 0.0 { 0.0 } else { x.atan2(y) };
                }
            }
            return Some((lam, phi));
        }
        let (tp, mut phi_l, halfpi, halfe);
        match self.mode {
            Mode::Obliq | Mode::Equit => {
                let t = 2.0 * (rho * self.cos_x1).atan2(self.akm1);
                let (sinphi, cosphi) = t.sin_cos();
                phi_l = if rho == 0.0 {
                    (cosphi * self.sin_x1).asin()
                } else {
                    (cosphi * self.sin_x1 + (y * sinphi * self.cos_x1 / rho)).asin()
                };
                tp = (0.5 * (HALFPI + phi_l)).tan();
                x *= sinphi;
                y = rho * self.cos_x1 * cosphi - y * self.sin_x1 * sinphi;
                halfpi = HALFPI;
                halfe = 0.5 * self.e;
            }
            Mode::NPole | Mode::SPole => {
                if self.mode == Mode::NPole {
                    y = -y;
                }
                tp = -rho / self.akm1;
                phi_l = HALFPI - 2.0 * tp.atan();
                halfpi = -HALFPI;
                halfe = -0.5 * self.e;
            }
        }
        for _ in 0..8 {
            let sinphi = self.e * phi_l.sin();
            let mut phi = 2.0 * (tp * ((1.0 + sinphi) / (1.0 - sinphi)).powf(halfe)).atan() - halfpi;
            if (phi_l - phi).abs() < 1e-10 {
                if self.mode == Mode::SPole {
                    phi = -phi;
                }
                let lam = if x == 0.0 && y == 0.0 { 0.0 } else { x.atan2(y) };
                return Some((lam, phi));
            }
            phi_l = phi;
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Oblique Stereographic (double stereographic, EPSG 9809, PROJ's `sterea`).

#[derive(Debug)]
struct Gauss {
    c: f64,
    k: f64,
    e: f64,
    ratexp: f64,
}

fn srat(esinp: f64, ratexp: f64) -> f64 {
    ((1.0 - esinp) / (1.0 + esinp)).powf(ratexp)
}

impl Gauss {
    /// Returns the conformal sphere, the conformal latitude of the origin and the sphere's radius.
    fn new(e: f64, phi0: f64) -> Result<(Self, f64, f64)> {
        let es = e * e;
        let sphi = phi0.sin();
        let cphi = phi0.cos();
        let cphi2 = cphi * cphi;
        let rc = (1.0 - es).sqrt() / (1.0 - es * sphi * sphi);
        let c = (1.0 + es * cphi2 * cphi2 / (1.0 - es)).sqrt();
        if c == 0.0 {
            return Err(err("gauss: invalid ellipsoid"));
        }
        // The conformal latitude chi (sin chi = sin phi0 / c), and tan(45° + x/2) of it and of phi0,
        // from their distances to the pole, 1 - |sin|: near a pole `asin` and `tan` round chi to
        // the pole while phi0 is not, which made `k` wrong by orders of magnitude (from about
        // 1e-8° off a pole).
        let to_pole = cphi2 / (1.0 + sphi.abs());
        let d = (es * cphi2 * cphi2 / ((1.0 - es) * (c + 1.0)) + to_pole) / c;
        let cos_chi = (d * (2.0 - d)).sqrt();
        let chi = ((1.0 - d).copysign(sphi)).atan2(cos_chi);
        // tan(45° + x/2) = (1 + |sin x|) / cos x, inverted for x < 0.
        let tan_half = |to_pole: f64, cos: f64| if phi0 < 0.0 { cos / (2.0 - to_pole) } else { (2.0 - to_pole) / cos };
        let ratexp = 0.5 * c * e;
        let srat_val = srat(e * sphi, ratexp);
        let k = tan_half(d, cos_chi) / (tan_half(to_pole, cphi).powf(c) * srat_val);
        Ok((Self { c, k, e, ratexp }, chi, rc))
    }

    fn fwd(&self, lam: f64, phi: f64) -> (f64, f64) {
        let phi = 2.0 * (self.k * (0.5 * phi + FORTPI).tan().powf(self.c) * srat(self.e * phi.sin(), self.ratexp)).atan() - HALFPI;
        (self.c * lam, phi)
    }

    fn inv(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let num = ((0.5 * phi + FORTPI).tan() / self.k).powf(1.0 / self.c);
        let lam = lam / self.c;
        let mut sphi = phi;
        for _ in 0..20 {
            let p = 2.0 * (num * srat(self.e * sphi.sin(), -0.5 * self.e)).atan() - HALFPI;
            if (p - sphi).abs() < 1e-14 {
                return Some((lam, p));
            }
            sphi = p;
        }
        None
    }
}

#[derive(Debug)]
struct Sterea {
    gauss: Gauss,
    phic0: f64,
    sinc0: f64,
    cosc0: f64,
    r2: f64,
    k0: f64,
}

impl Sterea {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let (gauss, phic0, r) = Gauss::new(s.ell.e, s.phi0)?;
        Ok(Self { gauss, phic0, sinc0: phic0.sin(), cosc0: phic0.cos(), r2: 2.0 * r, k0: s.k0 })
    }
}

impl Proj for Sterea {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (lam, phi) = self.gauss.fwd(lam, phi);
        let (sinc, cosc) = phi.sin_cos();
        let cosl = lam.cos();
        let denom = 1.0 + self.sinc0 * sinc + self.cosc0 * cosc * cosl;
        if denom == 0.0 {
            return None;
        }
        let k = self.k0 * self.r2 / denom;
        Some((k * cosc * lam.sin(), k * (self.cosc0 * sinc - self.sinc0 * cosc * cosl)))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (x, y) = (x / self.k0, y / self.k0);
        let rho = x.hypot(y);
        let (lam, phi) = if rho != 0.0 {
            let c = 2.0 * rho.atan2(self.r2);
            let (sinc, cosc) = c.sin_cos();
            let phi = (cosc * self.sinc0 + y * sinc * self.cosc0 / rho).asin();
            let lam = (x * sinc).atan2(rho * self.cosc0 * cosc - y * self.sinc0 * sinc);
            (lam, phi)
        } else {
            (0.0, self.phic0)
        };
        self.gauss.inv(lam, phi)
    }
}

// ---------------------------------------------------------------------------------------------
// Swiss Oblique Mercator.

#[derive(Debug)]
struct Somerc {
    e: f64,
    rone_es: f64,
    hlf_e: f64,
    c: f64,
    k: f64,
    kr: f64,
    sinp0: f64,
    cosp0: f64,
}

impl Somerc {
    fn new(s: &mut Setup<'_>) -> Self {
        let (e, es, one_es) = (s.ell.e, s.ell.es, s.ell.one_es);
        let rone_es = 1.0 / one_es;
        let hlf_e = 0.5 * e;
        let mut cp = s.phi0.cos();
        cp *= cp;
        let c = (1.0 + es * cp * cp * rone_es).sqrt();
        let sp = s.phi0.sin();
        let sinp0 = sp / c;
        let phip0 = aasin(sinp0);
        let cosp0 = phip0.cos();
        let sp = sp * e;
        let k = (FORTPI + 0.5 * phip0).tan().ln() - c * ((FORTPI + 0.5 * s.phi0).tan().ln() - hlf_e * ((1.0 + sp) / (1.0 - sp)).ln());
        let kr = s.k0 * one_es.sqrt() / (1.0 - sp * sp);
        Self { e, rone_es, hlf_e, c, k, kr, sinp0, cosp0 }
    }
}

impl Proj for Somerc {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let sp = self.e * phi.sin();
        let phip =
            2.0 * (self.c * ((FORTPI + 0.5 * phi).tan().ln() - self.hlf_e * ((1.0 + sp) / (1.0 - sp)).ln()) + self.k).exp().atan() - HALFPI;
        let lamp = self.c * lam;
        let cp = phip.cos();
        let phipp = aasin(self.cosp0 * phip.sin() - self.sinp0 * cp * lamp.cos());
        let lampp = aasin(cp * lamp.sin() / phipp.cos());
        Some((self.kr * lampp, self.kr * (FORTPI + 0.5 * phipp).tan().ln()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let phipp = 2.0 * ((y / self.kr).exp().atan() - FORTPI);
        let lampp = x / self.kr;
        let cp = phipp.cos();
        let mut phip = aasin(self.cosp0 * phipp.sin() + self.sinp0 * cp * lampp.cos());
        let lamp = aasin(cp * lampp.sin() / phip.cos());
        let con = (self.k - (FORTPI + 0.5 * phip).tan().ln()) / self.c;
        for _ in 0..6 {
            let esp = self.e * phip.sin();
            let delp = (con + (FORTPI + 0.5 * phip).tan().ln() - self.hlf_e * ((1.0 + esp) / (1.0 - esp)).ln())
                * (1.0 - esp * esp)
                * phip.cos()
                * self.rone_es;
            phip -= delp;
            if delp.abs() < 1e-10 {
                return Some((lamp / self.c, phip));
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Cassini-Soldner.

#[derive(Debug)]
struct Cass {
    es: f64,
    en: Mlfn,
    m0: f64,
    phi0: f64,
    hyperbolic: bool,
}

impl Cass {
    fn new(s: &mut Setup<'_>) -> Self {
        let en = Mlfn::new(s.ell.es);
        let m0 = en.mlfn(s.phi0, s.phi0.sin(), s.phi0.cos());
        Self { es: s.ell.es, en, m0, phi0: s.phi0, hyperbolic: s.p.has("hyperbolic") }
    }
}

const C1: f64 = 0.16666666666666666666;
const C2: f64 = 0.00833333333333333333;
const C3: f64 = 0.04166666666666666666;
const C4: f64 = 0.33333333333333333333;
const C5: f64 = 0.06666666666666666666;

impl Proj for Cass {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        if self.es == 0.0 {
            let x = (phi.cos() * lam.sin()).asin();
            let y = phi.tan().atan2(lam.cos()) - self.phi0;
            return Some((x, y));
        }
        let (sinphi, cosphi) = phi.sin_cos();
        let mut y = self.en.mlfn(phi, sinphi, cosphi);
        let n = 1.0 / (1.0 - self.es * sinphi * sinphi).sqrt();
        let tn = phi.tan();
        let t = tn * tn;
        let a1 = lam * cosphi;
        let c = self.es * cosphi * cosphi / (1.0 - self.es);
        let a2 = a1 * a1;
        // EPSG guidance note 7-2 (and PROJ since 9.2): X = ν(A − T A³/6 − (8 − T + 8C) T A⁵/120).
        let x = n * a1 * (1.0 - a2 * t * (C1 + (8.0 - t + 8.0 * c) * a2 * C2));
        y -= self.m0 - n * tn * a2 * (0.5 + (5.0 - t + 6.0 * c) * a2 * C3);
        if self.hyperbolic {
            let rho = (1.0 - self.es) * n * n * n;
            y -= y * y * y / (6.0 * rho * n);
        }
        Some((x, y))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        if self.es == 0.0 {
            let dd = y + self.phi0;
            let phi = (dd.sin() * x.cos()).asin();
            let lam = x.tan().atan2(dd.cos());
            return Some((lam, phi));
        }
        let (y0, mut y) = (y, y);
        if self.hyperbolic {
            let ph1 = self.en.inv(self.m0 + y);
            let sn = ph1.sin();
            let n = 1.0 / (1.0 - self.es * sn * sn).sqrt();
            let rho = (1.0 - self.es) * n * n * n;
            y += y * y * y / (6.0 * rho * n);
        }
        let ph1 = self.en.inv(self.m0 + y);
        let tn = ph1.tan();
        let t = tn * tn;
        let n = ph1.sin();
        let mut r = 1.0 / (1.0 - self.es * n * n);
        let n = r.sqrt();
        r *= (1.0 - self.es) * n;
        let dd = x / n;
        let d2 = dd * dd;
        let phi = ph1 - (n * tn / r) * d2 * (0.5 - (1.0 + 3.0 * t) * d2 * C3);
        let lam = dd * (1.0 + t * d2 * (-C4 + (1.0 + 3.0 * t) * d2 * C5)) / ph1.cos();
        // The series is a first guess, refined on the forward projection as PROJ does: it drifts
        // by centimetres away from the central meridian.
        generic_inverse_2d(|lam, phi| self.fwd(lam, phi), x, y0, lam, phi, 1e-12)
    }
}

// ---------------------------------------------------------------------------------------------
// American Polyconic.

#[derive(Debug)]
struct Poly {
    es: f64,
    one_es: f64,
    en: Mlfn,
    ml0: f64,
    phi0: f64,
}

impl Poly {
    fn new(s: &mut Setup<'_>) -> Self {
        let en = Mlfn::new(s.ell.es);
        let ml0 = if s.ell.es != 0.0 { en.mlfn(s.phi0, s.phi0.sin(), s.phi0.cos()) } else { -s.phi0 };
        Self { es: s.ell.es, one_es: s.ell.one_es, en, ml0, phi0: s.phi0 }
    }
}

impl Proj for Poly {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        if self.es == 0.0 {
            if phi.abs() <= 1e-10 {
                return Some((lam, self.ml0));
            }
            let cot = 1.0 / phi.tan();
            let e = lam * phi.sin();
            return Some((e.sin() * cot, phi - self.phi0 + cot * (1.0 - e.cos())));
        }
        if phi.abs() <= 1e-10 {
            return Some((lam, -self.ml0));
        }
        let (sp, cp) = phi.sin_cos();
        let ms = if cp.abs() > 1e-10 { msfn(sp, cp, self.es) / sp } else { 0.0 };
        let lam = lam * sp;
        Some((ms * lam.sin(), (self.en.mlfn(phi, sp, cp) - self.ml0) + ms * (1.0 - lam.cos())))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        if self.es == 0.0 {
            let y = self.phi0 + y;
            if y.abs() <= 1e-10 {
                return Some((x, 0.0));
            }
            let mut phi = y;
            let b = x * x + y * y;
            for i in 0..10 {
                let tp = phi.tan();
                let dphi = (y * (phi * tp + 1.0) - phi - 0.5 * (phi * phi + b) * tp) / ((phi - y) / tp - 1.0);
                phi -= dphi;
                if dphi.abs() <= 1e-12 {
                    break;
                }
                if i == 9 {
                    return None;
                }
            }
            return Some(((x * phi.tan()).asin() / phi.sin(), phi));
        }
        let y = y + self.ml0;
        if y.abs() <= 1e-10 {
            return Some((x, 0.0));
        }
        let r = y * y + x * x;
        let mut phi = y;
        let mut converged = false;
        for _ in 0..20 {
            let (sp, cp) = phi.sin_cos();
            let s2ph = sp * cp;
            if cp.abs() < 1e-12 {
                return None;
            }
            let mut mlp = (1.0 - self.es * sp * sp).sqrt();
            let c = sp * mlp / cp;
            let ml = self.en.mlfn(phi, sp, cp);
            let mlb = ml * ml + r;
            mlp = self.one_es / (mlp * mlp * mlp);
            let dphi = (ml + ml + c * mlb - 2.0 * y * (c * ml + 1.0))
                / (self.es * s2ph * (mlb - 2.0 * y * ml) / c + 2.0 * (y - ml) * (c * mlp - 1.0 / s2ph) - mlp - mlp);
            phi += dphi;
            if dphi.abs() <= 1e-12 {
                converged = true;
                break;
            }
        }
        if !converged {
            return None;
        }
        let c = phi.sin();
        let lam = (x * phi.tan() * (1.0 - self.es * c * c).sqrt()).asin() / phi.sin();
        Some((lam, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Equidistant Cylindrical: EPSG's ellipsoidal method 1028 on an ellipsoid (as PROJ 9.8 and later),
// the spherical method 1029 on a sphere.

#[derive(Debug)]
struct Eqc {
    rc: f64,
    phi0: f64,
    /// The meridian distance of an ellipsoid, and its value at the origin latitude.
    en: Option<(Mlfn, f64)>,
}

impl Eqc {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let phi1 = s.p.angle("lat_ts").unwrap_or(0.0);
        let (sin1, cos1) = phi1.sin_cos();
        if cos1 <= 0.0 {
            return Err(err("eqc: |lat_ts| must be less than 90°"));
        }
        if s.ell.es == 0.0 {
            return Ok(Self { rc: cos1, phi0: s.phi0, en: None });
        }
        // ν(φ1) cos φ1: the radius of the standard parallel.
        let rc = cos1 / (1.0 - s.ell.es * sin1 * sin1).sqrt();
        let en = Mlfn::new(s.ell.es);
        let m0 = en.mlfn(s.phi0, s.phi0.sin(), s.phi0.cos());
        Ok(Self { rc, phi0: s.phi0, en: Some((en, m0)) })
    }
}

impl Proj for Eqc {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        match &self.en {
            Some((en, m0)) => Some((self.rc * lam, en.mlfn(phi, phi.sin(), phi.cos()) - m0)),
            None => Some((self.rc * lam, phi - self.phi0)),
        }
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        match &self.en {
            Some((en, m0)) => Some((x / self.rc, en.inv(y + m0))),
            None => Some((x / self.rc, y + self.phi0)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Sinusoidal (Sanson-Flamsteed), with PROJ's ellipsoidal form on an ellipsoid.

#[derive(Debug)]
struct Sinu {
    es: f64,
    en: Mlfn,
}

impl Sinu {
    fn new(s: &mut Setup<'_>) -> Self {
        Self { es: s.ell.es, en: Mlfn::new(s.ell.es) }
    }
}

impl Proj for Sinu {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        if self.es == 0.0 {
            return Some((lam * phi.cos(), phi));
        }
        let (s, c) = phi.sin_cos();
        Some((lam * c / (1.0 - self.es * s * s).sqrt(), self.en.mlfn(phi, s, c)))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let phi = if self.es == 0.0 { y } else { self.en.inv(y) };
        // Past a pole, no point; at one, any longitude.
        let t = phi.abs();
        if t < HALFPI {
            let s = phi.sin();
            Some((x * (1.0 - self.es * s * s).sqrt() / phi.cos(), phi))
        } else if t - 1e-10 < HALFPI {
            Some((0.0, phi))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Bonne.

#[derive(Debug)]
struct Bonne {
    es: f64,
    phi1: f64,
    /// cot φ1 on a sphere, ν(φ1) cot φ1 on an ellipsoid: the radius of the standard parallel.
    am1: f64,
    /// The meridian distance of an ellipsoid, and its value at φ1.
    en: Option<(Mlfn, f64)>,
}

impl Bonne {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let phi1 = s.p.angle("lat_1").unwrap_or(0.0);
        if phi1.abs() < 1e-10 {
            return Err(err("bonne: lat_1 must not be 0"));
        }
        let es = s.ell.es;
        if es == 0.0 {
            let am1 = if phi1.abs() + 1e-10 >= HALFPI { 0.0 } else { 1.0 / phi1.tan() };
            return Ok(Self { es, phi1, am1, en: None });
        }
        let en = Mlfn::new(es);
        let (sin1, cos1) = phi1.sin_cos();
        let m1 = en.mlfn(phi1, sin1, cos1);
        let am1 = cos1 / ((1.0 - es * sin1 * sin1).sqrt() * sin1);
        Ok(Self { es, phi1, am1, en: Some((en, m1)) })
    }
}

impl Proj for Bonne {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (sinphi, cosphi) = phi.sin_cos();
        let (rh, e) = match &self.en {
            Some((en, m1)) => {
                let rh = self.am1 + m1 - en.mlfn(phi, sinphi, cosphi);
                (rh, cosphi * lam / (rh * (1.0 - self.es * sinphi * sinphi).sqrt()))
            }
            None => {
                let rh = self.am1 + self.phi1 - phi;
                (rh, lam * cosphi / rh)
            }
        };
        if rh.abs() <= 1e-10 {
            return Some((0.0, 0.0));
        }
        Some((rh * e.sin(), self.am1 - rh * e.cos()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let y = self.am1 - y;
        let rh = x.hypot(y).copysign(self.phi1);
        let phi = match &self.en {
            Some((en, m1)) => en.inv(self.am1 + m1 - rh),
            None => self.am1 + self.phi1 - rh,
        };
        let t = phi.abs();
        if t > HALFPI + 1e-10 {
            return None;
        }
        if HALFPI - t <= 1e-10 {
            return Some((0.0, phi));
        }
        let sinphi = phi.sin();
        let lm = rh * (1.0 - self.es * sinphi * sinphi).sqrt() / phi.cos();
        let lam = if self.phi1 > 0.0 { lm * x.atan2(y) } else { lm * (-x).atan2(-y) };
        Some((lam, phi))
    }
}

/// The radius, for a semi-major axis of 1, of the sphere PROJ puts its spherical-only Miller and
/// Van der Grinten projections on: on an ellipsoid, the sphere of the same area (`+R_A`, which
/// PROJ adds to their conversions), with PROJ's series in e².
fn authalic_radius(ell: Ell) -> f64 {
    let es = ell.es;
    1.0 - es * (1.0 / 6.0 + es * (17.0 / 360.0 + es * 67.0 / 3024.0))
}

// ---------------------------------------------------------------------------------------------
// Miller Cylindrical (spherical).

#[derive(Debug)]
struct Mill {
    r: f64,
}

impl Mill {
    fn new(s: &mut Setup<'_>) -> Self {
        Self { r: authalic_radius(s.ell) }
    }
}

impl Proj for Mill {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        Some((self.r * lam, self.r * (FORTPI + 0.4 * phi).tan().ln() * 1.25))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        Some((x / self.r, 2.5 * ((0.8 * y / self.r).exp().atan() - FORTPI)))
    }
}

// ---------------------------------------------------------------------------------------------
// Van der Grinten (I) (spherical), after Snyder (1987), p. 241, as PROJ.

#[derive(Debug)]
struct Vandg {
    r: f64,
}

impl Vandg {
    fn new(s: &mut Setup<'_>) -> Self {
        Self { r: authalic_radius(s.ell) }
    }
}

const VANDG_TOL: f64 = 1e-10;
const THIRD: f64 = 0.33333333333333333333;
const C2_27: f64 = 0.07407407407407407407;
const PI4_3: f64 = 4.18879020478639098458;
const PISQ: f64 = 9.86960440108935861869;
const TPISQ: f64 = 19.73920880217871723738;
const HPISQ: f64 = 4.93480220054467930934;

impl Proj for Vandg {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        use std::f64::consts::PI;
        let p2 = (phi / HALFPI).abs();
        if p2 - VANDG_TOL > 1.0 {
            return None;
        }
        let p2 = p2.min(1.0);
        let (x, y) = if phi.abs() <= VANDG_TOL {
            (lam, 0.0)
        } else if lam.abs() <= VANDG_TOL || (p2 - 1.0).abs() < VANDG_TOL {
            (0.0, (PI * (0.5 * p2.asin()).tan()).copysign(phi))
        } else {
            let al = 0.5 * (PI / lam - lam / PI).abs();
            let al2 = al * al;
            let g = (1.0 - p2 * p2).sqrt();
            let g = g / (p2 + g - 1.0);
            let g2 = g * g;
            let p = g * (2.0 / p2 - 1.0);
            let p2 = p * p;
            let x = g - p2;
            let g = p2 + al2;
            let x = (PI * (al * x + (al2 * x * x - g * (g2 - p2)).sqrt()).abs() / g).copysign(lam);
            let t = (x / PI).abs();
            let t = 1.0 - t * (t + 2.0 * al);
            if t < -VANDG_TOL {
                return None;
            }
            (x, if t < 0.0 { 0.0 } else { t.sqrt() * PI.copysign(phi) })
        };
        Some((self.r * x, self.r * y))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        use std::f64::consts::PI;
        let (x, y) = (x / self.r, y / self.r);
        let x2 = x * x;
        let ay = y.abs();
        if ay < VANDG_TOL {
            let t = x2 * x2 + TPISQ * (x2 + HPISQ);
            let lam = if x.abs() <= VANDG_TOL { 0.0 } else { 0.5 * (x2 - PISQ + t.sqrt()) / x };
            return Some((lam, 0.0));
        }
        let y2 = y * y;
        let r = x2 + y2;
        let r2 = r * r;
        let c1 = -PI * ay * (r + PISQ);
        let c3 = r2 + 2.0 * PI * (ay * r + PI * (y2 + PI * (ay + HALFPI)));
        let c2 = (c1 + PISQ * (r - 3.0 * y2)) / c3;
        let c0 = PI * ay;
        let al = c1 / c3 - THIRD * c2 * c2;
        let m = 2.0 * (-THIRD * al).sqrt();
        let d = C2_27 * c2 * c2 * c2 + (c0 * c0 - THIRD * c2 * c1) / c3;
        let al_m = al * m;
        if al_m.abs() < 1e-16 {
            return None;
        }
        let d = 3.0 * d / al_m;
        let t = d.abs();
        if t - VANDG_TOL > 1.0 {
            return None;
        }
        let mut d = if t > 1.0 { if d > 0.0 { 0.0 } else { PI } } else { d.acos() };
        if r > PISQ {
            d = 2.0 * PI - d;
        }
        let phi = (PI * (m * (d * THIRD + PI4_3).cos() - THIRD * c2)).copysign(y);
        let t = r2 + TPISQ * (x2 - y2 + HPISQ);
        let lam = if x.abs() <= VANDG_TOL { 0.0 } else { 0.5 * (r - PISQ + if t <= 0.0 { 0.0 } else { t.sqrt() }) / x };
        Some((lam, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Gnomonic. On an ellipsoid, Karney's ellipsoidal gnomonic ("Algorithms for geodesics", 2013,
// section 8), as PROJ: ρ = m12 / M12 along the azimuth of the geodesic from the centre.

#[derive(Debug)]
struct Gnom {
    mode: Mode,
    phi0: f64,
    sinph0: f64,
    cosph0: f64,
    /// The geodesics of an ellipsoid with a semi-major axis of 1.
    geod: Option<geographiclib_rs::Geodesic>,
}

impl Gnom {
    fn new(s: &mut Setup<'_>) -> Self {
        let geod = (s.ell.es != 0.0).then(|| geographiclib_rs::Geodesic::new(1.0, 1.0 - s.ell.one_es.sqrt()));
        let (sinph0, cosph0) = s.phi0.sin_cos();
        Self { mode: azimuthal_mode(s.phi0), phi0: s.phi0, sinph0, cosph0, geod }
    }
}

impl Proj for Gnom {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        use geographiclib_rs::InverseGeodesic;
        if let Some(g) = &self.geod {
            let (_, azi1, _, m12, mm12, _, _): (f64, f64, f64, f64, f64, f64, f64) =
                g.inverse(self.phi0.to_degrees(), 0.0, phi.to_degrees(), lam.to_degrees());
            // Beyond the horizon (M12 <= 0, or NaN).
            if mm12.is_nan() || mm12 <= 0.0 {
                return None;
            }
            let rho = m12 / mm12;
            let (sina, cosa) = azi1.to_radians().sin_cos();
            return Some((rho * sina, rho * cosa));
        }
        let (sinphi, cosphi) = phi.sin_cos();
        let coslam = lam.cos();
        let d = match self.mode {
            Mode::Equit => cosphi * coslam,
            Mode::Obliq => self.sinph0 * sinphi + self.cosph0 * cosphi * coslam,
            Mode::SPole => -sinphi,
            Mode::NPole => sinphi,
        };
        if d <= 1e-10 {
            return None;
        }
        let k = 1.0 / d;
        let y = match self.mode {
            Mode::Equit => k * sinphi,
            Mode::Obliq => k * (self.cosph0 * sinphi - self.sinph0 * cosphi * coslam),
            Mode::NPole => -k * cosphi * coslam,
            Mode::SPole => k * cosphi * coslam,
        };
        Some((k * cosphi * lam.sin(), y))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        use geographiclib_rs::DirectGeodesic;
        if let Some(g) = &self.geod {
            // Newton's method for the distance s along the azimuth, with dρ/ds = 1 / M12², or
            // beyond ρ = 1 on 1/ρ, with d(1/ρ)/ds = -1 / m12².
            let eps = 0.01 * f64::EPSILON.sqrt();
            let azi0 = x.atan2(y).to_degrees();
            let lat0 = self.phi0.to_degrees();
            let mut rho = x.hypot(y);
            let mut s = rho.atan();
            let little = rho <= 1.0;
            if !little {
                rho = 1.0 / rho;
            }
            // As PROJ: the point at the last distance computed, once a step is small enough (or
            // NaN, whose result is then refused), even when that is at the last iteration.
            let mut converged = false;
            let mut point = (f64::NAN, f64::NAN);
            for _ in 0..10 {
                let (lat, lon, _, m12, mm12, _): (f64, f64, f64, f64, f64, f64) = g.direct(lat0, 0.0, azi0, s);
                point = (lon.to_radians(), lat.to_radians());
                if converged {
                    break;
                }
                let ds = if little { (m12 - rho * mm12) * mm12 } else { (rho * m12 - mm12) * m12 };
                s -= ds;
                if ds.is_nan() || ds.abs() < eps {
                    converged = true;
                }
            }
            return (converged && point.0.is_finite() && point.1.is_finite()).then_some(point);
        }
        let rh = x.hypot(y);
        if rh <= 1e-10 {
            return Some((0.0, self.phi0));
        }
        let z = rh.atan();
        let (sinz, cosz) = z.sin_cos();
        let (phi, x, y) = match self.mode {
            Mode::Obliq => {
                let phi = aasin(cosz * self.sinph0 + y * sinz * self.cosph0 / rh);
                (phi, x * sinz * self.cosph0, (cosz - self.sinph0 * phi.sin()) * rh)
            }
            Mode::Equit => (aasin(y * sinz / rh), x * sinz, cosz * rh),
            Mode::SPole => (z - HALFPI, x, y),
            Mode::NPole => (HALFPI - z, x, -y),
        };
        Some((x.atan2(y), phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Cylindrical Equal Area.

#[derive(Debug)]
struct Cea {
    k0: f64,
    e: f64,
    one_es: f64,
    qp: f64,
    apa: [f64; 6],
}

impl Cea {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let mut t = 0.0;
        if let Some(ts) = s.p.angle("lat_ts") {
            t = ts;
            s.k0 = t.cos();
            if s.k0 < 0.0 {
                return Err(err("cea: invalid lat_ts"));
            }
        }
        let mut c = Self { k0: s.k0, e: s.ell.e, one_es: s.ell.one_es, qp: 0.0, apa: [0.0; 6] };
        if s.ell.es != 0.0 {
            let t = t.sin();
            c.k0 /= (1.0 - s.ell.es * t * t).sqrt();
            c.apa = authset(s.ell.es);
            c.qp = qsfn(1.0, c.e, c.one_es);
        }
        Ok(c)
    }
}

impl Proj for Cea {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        if self.e == 0.0 {
            return Some((self.k0 * lam, phi.sin() / self.k0));
        }
        Some((self.k0 * lam, 0.5 * qsfn(phi.sin(), self.e, self.one_es) / self.k0))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        if self.e == 0.0 {
            let y = y * self.k0;
            let t = y.abs();
            if t - 1e-10 > 1.0 {
                return None;
            }
            let phi = if t >= 1.0 { if y < 0.0 { -HALFPI } else { HALFPI } } else { y.asin() };
            return Some((x / self.k0, phi));
        }
        Some((x / self.k0, authlat_inverse(aasin(2.0 * y * self.k0 / self.qp), &self.apa, self.e, self.one_es, self.qp)))
    }
}

// ---------------------------------------------------------------------------------------------
// Equal Earth.

#[derive(Debug)]
struct EqEarth {
    es: f64,
    e: f64,
    qp: f64,
    rqda: f64,
    apa: [f64; 6],
}

const EE_A1: f64 = 1.340264;
const EE_A2: f64 = -0.081106;
const EE_A3: f64 = 0.000893;
const EE_A4: f64 = 0.003796;

impl EqEarth {
    fn new(s: &mut Setup<'_>) -> Self {
        let mut q = Self { es: s.ell.es, e: s.ell.e, qp: 0.0, rqda: 1.0, apa: [0.0; 6] };
        if q.es != 0.0 {
            q.apa = authset(q.es);
            q.qp = qsfn(1.0, q.e, 1.0 - q.es);
            q.rqda = (0.5 * q.qp).sqrt();
        }
        q
    }
}

impl Proj for EqEarth {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let m = 3f64.sqrt() / 2.0;
        let mut sbeta = phi.sin();
        if self.es != 0.0 {
            sbeta = (qsfn(sbeta, self.e, 1.0 - self.es) / self.qp).clamp(-1.0, 1.0);
        }
        let psi = (m * sbeta).asin();
        let psi2 = psi * psi;
        let psi6 = psi2 * psi2 * psi2;
        let x = lam * psi.cos() / (m * (EE_A1 + 3.0 * EE_A2 * psi2 + psi6 * (7.0 * EE_A3 + 9.0 * EE_A4 * psi2)));
        let y = psi * (EE_A1 + EE_A2 * psi2 + psi6 * (EE_A3 + EE_A4 * psi2));
        Some((x * self.rqda, y * self.rqda))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let m = 3f64.sqrt() / 2.0;
        let (x, y) = (x / self.rqda, y / self.rqda);
        let mut psi = y;
        let mut ok = false;
        for _ in 0..12 {
            let psi2 = psi * psi;
            let psi6 = psi2 * psi2 * psi2;
            let fy = psi * (EE_A1 + EE_A2 * psi2 + psi6 * (EE_A3 + EE_A4 * psi2)) - y;
            let fpsi = EE_A1 + 3.0 * EE_A2 * psi2 + psi6 * (7.0 * EE_A3 + 9.0 * EE_A4 * psi2);
            let dlat = fy / fpsi;
            psi -= dlat;
            if dlat.abs() < 1e-12 {
                ok = true;
                break;
            }
        }
        if !ok {
            return None;
        }
        let psi2 = psi * psi;
        let psi6 = psi2 * psi2 * psi2;
        let lam = m * x * (EE_A1 + 3.0 * EE_A2 * psi2 + psi6 * (7.0 * EE_A3 + 9.0 * EE_A4 * psi2)) / psi.cos();
        let beta = aasin(psi.sin() / m);
        let phi = if self.es != 0.0 { authlat_inverse(beta, &self.apa, self.e, 1.0 - self.es, self.qp) } else { beta };
        Some((lam, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Hotine Oblique Mercator (variants A and B).

#[derive(Debug)]
struct Omerc {
    e: f64,
    a: f64,
    b: f64,
    e_: f64,
    arb: f64,
    bra: f64,
    rb: f64,
    singam: f64,
    cosgam: f64,
    sinrot: f64,
    cosrot: f64,
    v_pole_n: f64,
    v_pole_s: f64,
    u_0: f64,
    no_rot: bool,
}

impl Omerc {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let p = s.p;
        let no_rot = p.has("no_rot");
        let alpha = p.angle("alpha");
        let gamma = p.angle("gamma");
        let lamc = p.angle("lonc").unwrap_or(0.0);
        let no_off = p.has("no_uoff") || p.has("no_off");
        let (e, es, one_es) = (s.ell.e, s.ell.es, s.ell.one_es);
        let phi0 = s.phi0;
        if alpha.is_none() && gamma.is_none() {
            return Err(err("omerc: two-point form is not supported"));
        }
        let com = one_es.sqrt();
        let (b, a, d, mut f, e_);
        if phi0.abs() > 1e-10 {
            let (sinph0, cosph0) = phi0.sin_cos();
            let con = 1.0 - es * sinph0 * sinph0;
            let bb = cosph0 * cosph0;
            b = (1.0 + es * bb * bb / one_es).sqrt();
            a = b * s.k0 * com / con;
            d = b * com / (cosph0 * con.sqrt());
            f = d * d - 1.0;
            if f <= 0.0 {
                f = 0.0;
            } else {
                f = f.sqrt();
                if phi0 < 0.0 {
                    f = -f;
                }
            }
            f += d;
            e_ = f * tsfn(phi0, sinph0, e).powf(b);
        } else {
            b = 1.0 / com;
            a = s.k0;
            d = 1.0;
            f = 1.0;
            e_ = 1.0;
        }
        let (gamma0, alpha_c, gamma) = match (alpha, gamma) {
            (Some(alpha_c), g) => {
                let gamma0 = aasin(alpha_c.sin() / d);
                (gamma0, alpha_c, g.unwrap_or(alpha_c))
            }
            (None, Some(g)) => (g, aasin(d * g.sin()), g),
            (None, None) => unreachable!(),
        };
        s.lam0 = lamc - aasin(0.5 * (f - 1.0 / f) * gamma0.tan()) / b;
        let singam = gamma0.sin();
        let cosgam = gamma0.cos();
        let sinrot = gamma.sin();
        let cosrot = gamma.cos();
        let rb = 1.0 / b;
        let arb = a * rb;
        let bra = 1.0 / arb;
        let u_0 = if no_off {
            0.0
        } else {
            let u = (arb * ((d * d - 1.0).sqrt() / alpha_c.cos()).atan()).abs();
            if phi0 < 0.0 { -u } else { u }
        };
        let f2 = 0.5 * gamma0;
        let v_pole_n = arb * (FORTPI - f2).tan().ln();
        let v_pole_s = arb * (FORTPI + f2).tan().ln();
        Ok(Self { e, a, b, e_, arb, bra, rb, singam, cosgam, sinrot, cosrot, v_pole_n, v_pole_s, u_0, no_rot })
    }
}

impl Proj for Omerc {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (u, v);
        if (phi.abs() - HALFPI).abs() > 1e-10 {
            let w = self.e_ / tsfn(phi, phi.sin(), self.e).powf(self.b);
            let temp = 1.0 / w;
            let s = 0.5 * (w - temp);
            let t = 0.5 * (w + temp);
            let vv = (self.b * lam).sin();
            let uu = (s * self.singam - vv * self.cosgam) / t;
            if (uu.abs() - 1.0).abs() < 1e-10 {
                return None;
            }
            v = 0.5 * self.arb * ((1.0 - uu) / (1.0 + uu)).ln();
            let temp = (self.b * lam).cos();
            u = if temp.abs() < 1e-7 { self.a * lam } else { self.arb * (s * self.cosgam + vv * self.singam).atan2(temp) };
        } else {
            v = if phi > 0.0 { self.v_pole_n } else { self.v_pole_s };
            u = self.arb * phi;
        }
        if self.no_rot {
            Some((u, v))
        } else {
            let u = u - self.u_0;
            Some((v * self.cosrot + u * self.sinrot, u * self.cosrot - v * self.sinrot))
        }
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (u, v) = if self.no_rot { (x, y) } else { (y * self.cosrot + x * self.sinrot + self.u_0, x * self.cosrot - y * self.sinrot) };
        let qp = (-self.bra * v).exp();
        if qp == 0.0 {
            return None;
        }
        let sp = 0.5 * (qp - 1.0 / qp);
        let tp = 0.5 * (qp + 1.0 / qp);
        let vp = (self.bra * u).sin();
        let up = (vp * self.cosgam + sp * self.singam) / tp;
        if (up.abs() - 1.0).abs() < 1e-10 {
            return Some((0.0, if up < 0.0 { -HALFPI } else { HALFPI }));
        }
        let phi = self.e_ / ((1.0 + up) / (1.0 - up)).sqrt();
        let phi = phi2(phi.powf(1.0 / self.b), self.e);
        let lam = -self.rb * (sp * self.cosgam - vp * self.singam).atan2((self.bra * u).cos());
        Some((lam, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Krovak, and Krovak Modified (EPSG 1042).

#[derive(Debug)]
struct Krovak {
    e: f64,
    alpha: f64,
    k: f64,
    n: f64,
    rho0: f64,
    ad: f64,
    czech: f64,
    modified: bool,
    /// Bessel's semi-major axis over the CRS's, which normalized coordinates are in.
    scale: f64,
}

const KROVAK_S0: f64 = 1.37008346281555;
const KROVAK_UQ: f64 = 1.04216856380474;
/// Bessel 1841, which PROJ's Krovak uses whatever the CRS's ellipsoid.
const KROVAK_A: f64 = 6377397.155;
const KROVAK_ES: f64 = 0.006674372230614;

impl Krovak {
    fn new(s: &mut Setup<'_>, modified: bool) -> Result<Self> {
        let p = s.p;
        if p.angle("lat_0").is_none() {
            s.phi0 = 0.863937979737193;
        }
        if p.angle("lon_0").is_none() {
            s.lam0 = 0.7417649320975901 - 0.308341501185665;
        }
        if p.f64("k").is_none() && p.f64("k_0").is_none() {
            s.k0 = 0.9999;
        }
        let czech = if p.has("czech") { 1.0 } else { -1.0 };
        // As PROJ, on Bessel's ellipsoid whatever the CRS's (which only its datum uses).
        let (e, es, phi0) = (KROVAK_ES.sqrt(), KROVAK_ES, s.phi0);
        let alpha = (1.0 + (es * phi0.cos().powi(4)) / (1.0 - es)).sqrt();
        let u0 = (phi0.sin() / alpha).asin();
        let g = ((1.0 + e * phi0.sin()) / (1.0 - e * phi0.sin())).powf(alpha * e / 2.0);
        let t = (phi0 / 2.0 + FORTPI).tan();
        if t == 0.0 {
            return Err(err("krovak: invalid lat_0"));
        }
        let k = (u0 / 2.0 + FORTPI).tan() / t.powf(alpha) * g;
        let n0 = (1.0 - es).sqrt() / (1.0 - es * phi0.sin().powi(2));
        let n = KROVAK_S0.sin();
        let rho0 = s.k0 * n0 / KROVAK_S0.tan();
        let ad = HALFPI - KROVAK_UQ;
        // PROJ applies the false origin to the southing and westing, before their sign change.
        s.x0 *= czech;
        s.y0 *= czech;
        Ok(Self { e, alpha, k, n, rho0, ad, czech, modified, scale: KROVAK_A / s.ell.a })
    }

    /// The correction of Krovak Modified, in normalized units, from the normalized unmodified
    /// southing and westing (without false origin).
    fn correction(&self, xr: f64, yr: f64) -> (f64, f64) {
        const X0: f64 = 1089000.0;
        const Y0: f64 = 654000.0;
        const C1: f64 = 2.946529277e-02;
        const C2: f64 = 2.515965696e-02;
        const C3: f64 = 1.193845912e-07;
        const C4: f64 = -4.668270147e-07;
        const C5: f64 = 9.233980362e-12;
        const C6: f64 = 1.523735715e-12;
        const C7: f64 = 1.696780024e-18;
        const C8: f64 = 4.408314235e-18;
        const C9: f64 = -8.331083518e-24;
        const C10: f64 = -3.689471323e-24;
        let xr = xr * KROVAK_A - X0;
        let yr = yr * KROVAK_A - Y0;
        let xr2 = xr * xr;
        let yr2 = yr * yr;
        let xr2_yr2 = xr2 - yr2;
        let quart = xr2 * xr2 + yr2 * yr2 - 6.0 * xr2 * yr2;
        let dx = C1 + C3 * xr - C4 * yr - 2.0 * C6 * xr * yr + C5 * xr2_yr2 + C7 * xr * (xr2 - 3.0 * yr2) - C8 * yr * (3.0 * xr2 - yr2)
            + 4.0 * C9 * xr * yr * xr2_yr2
            + C10 * quart;
        let dy = C2 + C3 * yr + C4 * xr + 2.0 * C5 * xr * yr + C6 * xr2_yr2 + C8 * xr * (xr2 - 3.0 * yr2) + C7 * yr * (3.0 * xr2 - yr2)
            - 4.0 * C10 * xr * yr * xr2_yr2
            + C9 * quart;
        (dx / KROVAK_A, dy / KROVAK_A)
    }
}

impl Proj for Krovak {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let gfi = ((1.0 + self.e * phi.sin()) / (1.0 - self.e * phi.sin())).powf(self.alpha * self.e / 2.0);
        let u = 2.0 * ((self.k * (phi / 2.0 + FORTPI).tan().powf(self.alpha) / gfi).atan() - FORTPI);
        let deltav = -lam * self.alpha;
        let s = (self.ad.cos() * u.sin() + self.ad.sin() * u.cos() * deltav.cos()).asin();
        let cos_s = s.cos();
        if cos_s < 1e-12 {
            return Some((0.0, 0.0));
        }
        let d = (u.cos() * deltav.sin() / cos_s).asin();
        let eps = self.n * d;
        let rho = self.rho0 * (KROVAK_S0 / 2.0 + FORTPI).tan().powf(self.n) / (s / 2.0 + FORTPI).tan().powf(self.n);
        // Southing and westing.
        let mut xs = rho * eps.cos();
        let mut yw = rho * eps.sin();
        if self.modified {
            let (dx, dy) = self.correction(xs, yw);
            xs -= dx;
            yw -= dy;
        }
        Some((yw * self.czech * self.scale, xs * self.czech * self.scale))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let mut yw = x * self.czech / self.scale;
        let mut xs = y * self.czech / self.scale;
        if self.modified {
            // The correction is small and smooth: a fixed-point iteration converges at once.
            let (x0, y0) = (xs, yw);
            for _ in 0..5 {
                let (dx, dy) = self.correction(xs, yw);
                xs = x0 + dx;
                yw = y0 + dy;
            }
        }
        let rho = xs.hypot(yw);
        let eps = yw.atan2(xs);
        let d = eps / KROVAK_S0.sin();
        let s = if rho == 0.0 {
            HALFPI
        } else {
            2.0 * (((self.rho0 / rho).powf(1.0 / self.n) * (KROVAK_S0 / 2.0 + FORTPI).tan()).atan() - FORTPI)
        };
        let u = (self.ad.cos() * s.sin() - self.ad.sin() * s.cos() * d.cos()).asin();
        let deltav = (s.cos() * d.sin() / u.cos()).asin();
        let lam = -deltav / self.alpha;
        let mut phi = u;
        for _ in 0..15 {
            let fi1 = 2.0
                * ((self.k.powf(-1.0 / self.alpha)
                    * (u / 2.0 + FORTPI).tan().powf(1.0 / self.alpha)
                    * ((1.0 + self.e * phi.sin()) / (1.0 - self.e * phi.sin())).powf(self.e / 2.0))
                .atan()
                    - FORTPI);
            if (fi1 - phi).abs() < 1e-15 {
                return Some((lam, fi1));
            }
            phi = fi1;
        }
        Some((lam, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Laborde (Madagascar).

#[derive(Debug)]
struct Labrd {
    e: f64,
    one_es: f64,
    k0: f64,
    phi0: f64,
    krg: f64,
    p0s: f64,
    a: f64,
    c: f64,
    ca: f64,
    cb: f64,
    cc: f64,
    cd: f64,
}

impl Labrd {
    fn new(s: &mut Setup<'_>) -> Self {
        let az = s.p.angle("azi").unwrap_or(0.0);
        let (e, es, one_es, phi0, k0) = (s.ell.e, s.ell.es, s.ell.one_es, s.phi0, s.k0);
        let sinp = phi0.sin();
        let t = 1.0 - es * sinp * sinp;
        let n = 1.0 / t.sqrt();
        let r = one_es * n / t;
        let krg = k0 * (n * r).sqrt();
        let p0s = ((r / n).sqrt() * phi0.tan()).atan();
        let a = sinp / p0s.sin();
        let t = e * sinp;
        let c = 0.5 * e * a * ((1.0 + t) / (1.0 - t)).ln() + (-a * (FORTPI + 0.5 * phi0).tan().ln()) + (FORTPI + 0.5 * p0s).tan().ln();
        let t = az + az;
        let mut cb = 1.0 / (12.0 * krg * krg);
        let ca = (1.0 - t.cos()) * cb;
        cb *= t.sin();
        let cc = 3.0 * (ca * ca - cb * cb);
        let cd = 6.0 * ca * cb;
        Self { e, one_es, k0, phi0, krg, p0s, a, c, ca, cb, cc, cd }
    }
}

impl Proj for Labrd {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let v1 = self.a * (FORTPI + 0.5 * phi).tan().ln();
        let t = self.e * phi.sin();
        let v2 = 0.5 * self.e * self.a * ((1.0 + t) / (1.0 - t)).ln();
        let ps = 2.0 * ((v1 - v2 + self.c).exp().atan() - FORTPI);
        let i1 = ps - self.p0s;
        let cosps = ps.cos();
        let cosps2 = cosps * cosps;
        let sinps = ps.sin();
        let sinps2 = sinps * sinps;
        let i4 = self.a * cosps;
        let i2 = 0.5 * self.a * i4 * sinps;
        let i3 = i2 * self.a * self.a * (5.0 * cosps2 - sinps2) / 12.0;
        let mut i6 = i4 * self.a * self.a;
        let i5 = i6 * (cosps2 - sinps2) / 6.0;
        i6 *= self.a * self.a * (5.0 * cosps2 * cosps2 + sinps2 * (sinps2 - 18.0 * cosps2)) / 120.0;
        let t = lam * lam;
        let mut x = self.krg * lam * (i4 + t * (i5 + t * i6));
        let mut y = self.krg * (i1 + t * (i2 + t * i3));
        let x2 = x * x;
        let y2 = y * y;
        let v1 = 3.0 * x * y2 - x * x2;
        let v2 = y * y2 - 3.0 * x2 * y;
        x += self.ca * v1 + self.cb * v2;
        y += self.ca * v2 - self.cb * v1;
        Some((x, y))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (mut x, mut y) = (x, y);
        let x2 = x * x;
        let y2 = y * y;
        let v1 = 3.0 * x * y2 - x * x2;
        let v2 = y * y2 - 3.0 * x2 * y;
        let v3 = x * (5.0 * y2 * y2 + x2 * (-10.0 * y2 + x2));
        let v4 = y * (5.0 * x2 * x2 + y2 * (-10.0 * x2 + y2));
        x += -self.ca * v1 - self.cb * v2 + self.cc * v3 + self.cd * v4;
        y += self.cb * v1 - self.ca * v2 - self.cd * v3 + self.cc * v4;
        let ps = self.p0s + y / self.krg;
        let mut pe = ps + self.phi0 - self.p0s;
        for _ in 0..20 {
            let v1 = self.a * (FORTPI + 0.5 * pe).tan().ln();
            let tpe = self.e * pe.sin();
            let v2 = 0.5 * self.e * self.a * ((1.0 + tpe) / (1.0 - tpe)).ln();
            let t = ps - 2.0 * ((v1 - v2 + self.c).exp().atan() - FORTPI);
            pe += t;
            if t.abs() < 1e-10 {
                break;
            }
        }
        let t = self.e * pe.sin();
        let t = 1.0 - t * t;
        let re = self.one_es / (t * t.sqrt());
        let t = ps.tan();
        let t2 = t * t;
        let s = self.krg * self.krg;
        let mut d = re * self.k0 * self.krg;
        let i7 = t / (2.0 * d);
        let i8 = t * (5.0 + 3.0 * t2) / (24.0 * d * s);
        d = ps.cos() * self.krg * self.a;
        let i9 = 1.0 / d;
        d *= s;
        let i10 = (1.0 + 2.0 * t2) / (6.0 * d);
        let i11 = (5.0 + t2 * (28.0 + 24.0 * t2)) / (120.0 * d * s);
        let x2 = x * x;
        Some((x * (i9 + x2 * (-i10 + x2 * i11)), pe + x2 * (-i7 + i8 * x2)))
    }
}

// ---------------------------------------------------------------------------------------------
// New Zealand Map Grid.

#[derive(Debug)]
struct Nzmg {
    phi0: f64,
}

const NZMG_BF: [(f64, f64); 6] = [
    (0.7557853228, 0.0),
    (0.249204646, 0.003371507),
    (-0.001541739, 0.041058560),
    (-0.10162907, 0.01727609),
    (-0.26623489, -0.36249218),
    (-0.6870983, -1.1651967),
];
const NZMG_TPHI: [f64; 9] = [1.5627014243, 0.5185406398, -0.03333098, -0.1052906, -0.0368594, 0.007317, 0.01220, 0.00394, -0.0013];
const NZMG_TPSI: [f64; 10] =
    [0.6399175073, -0.1358797613, 0.063294409, -0.02526853, 0.0117879, -0.0055161, 0.0026906, -0.001333, 0.00067, -0.00034];
const SEC5_TO_RAD: f64 = 0.4848136811095359935899141023;
const RAD_TO_SEC5: f64 = 2.062648062470963551564733573;

/// z · (C₀ + z·(C₁ + … + z·Cₙ)) over complex numbers (PROJ's `pj_zpoly1`).
fn zpoly1(z: (f64, f64), c: &[(f64, f64)]) -> (f64, f64) {
    let n = c.len() - 1;
    let mut a = c[n];
    for k in (0..n).rev() {
        let t = a.0;
        a.0 = c[k].0 + z.0 * t - z.1 * a.1;
        a.1 = c[k].1 + z.0 * a.1 + z.1 * t;
    }
    let t = a.0;
    (z.0 * t - z.1 * a.1, z.0 * a.1 + z.1 * t)
}

/// [`zpoly1`] with its derivative (PROJ's `pj_zpolyd1`).
fn zpolyd1(z: (f64, f64), c: &[(f64, f64)]) -> ((f64, f64), (f64, f64)) {
    let n = c.len() - 1;
    let mut a = c[n];
    let mut b = a;
    let mut first = true;
    for k in (0..n).rev() {
        if first {
            first = false;
        } else {
            let t = b.0;
            b.0 = a.0 + z.0 * t - z.1 * b.1;
            b.1 = a.1 + z.0 * b.1 + z.1 * t;
        }
        let t = a.0;
        a.0 = c[k].0 + z.0 * t - z.1 * a.1;
        a.1 = c[k].1 + z.0 * a.1 + z.1 * t;
    }
    let t = b.0;
    b.0 = a.0 + z.0 * t - z.1 * b.1;
    b.1 = a.1 + z.0 * b.1 + z.1 * t;
    let t = a.0;
    a.0 = z.0 * t - z.1 * a.1;
    a.1 = z.0 * a.1 + z.1 * t;
    (a, b)
}

impl Nzmg {
    fn new(s: &mut Setup<'_>) -> Self {
        s.ell = Ell::from_a_es(6378388.0, s.ell.es);
        s.lam0 = 173f64.to_radians();
        s.phi0 = (-41f64).to_radians();
        s.x0 = 2510000.0;
        s.y0 = 6023150.0;
        Self { phi0: s.phi0 }
    }
}

impl Proj for Nzmg {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let phi = (phi - self.phi0) * RAD_TO_SEC5;
        let mut r = NZMG_TPSI[9];
        for k in (0..9).rev() {
            r = NZMG_TPSI[k] + phi * r;
        }
        r *= phi;
        let p = zpoly1((r, lam), &NZMG_BF);
        Some((p.1, p.0))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let mut p = (y, x);
        let mut ok = false;
        for _ in 0..20 {
            let (mut f, fp) = zpolyd1(p, &NZMG_BF);
            f.0 -= y;
            f.1 -= x;
            let den = fp.0 * fp.0 + fp.1 * fp.1;
            let dr = -(f.0 * fp.0 + f.1 * fp.1) / den;
            let di = -(f.1 * fp.0 - f.0 * fp.1) / den;
            p.0 += dr;
            p.1 += di;
            if dr.abs() + di.abs() <= 1e-10 {
                ok = true;
                break;
            }
        }
        if !ok {
            return None;
        }
        let mut phi = NZMG_TPHI[8];
        for k in (0..8).rev() {
            phi = NZMG_TPHI[k] + p.0 * phi;
        }
        Some((p.1, self.phi0 + p.0 * phi * SEC5_TO_RAD))
    }
}

// ---------------------------------------------------------------------------------------------
// Colombia Urban (EPSG 1052).

#[derive(Debug)]
struct ColUrban {
    es: f64,
    phi0: f64,
    rho0: f64,
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    h0: f64,
}

impl ColUrban {
    fn new(s: &mut Setup<'_>) -> Self {
        let h0 = s.p.f64("h_0").unwrap_or(0.0) / s.ell.a;
        let es = s.ell.es;
        let sinphi0 = s.phi0.sin();
        let nu0 = 1.0 / (1.0 - es * sinphi0 * sinphi0).sqrt();
        let a = 1.0 + h0 / nu0;
        let rho0 = (1.0 - es) / (1.0 - es * sinphi0 * sinphi0).powf(1.5);
        let b = s.phi0.tan() / (2.0 * rho0 * nu0);
        let c = 1.0 + h0;
        let d = rho0 * (1.0 + h0 / (1.0 - es));
        Self { es, phi0: s.phi0, rho0, a, b, c, d, h0 }
    }
}

impl Proj for ColUrban {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (sinphi, cosphi) = phi.sin_cos();
        let nu = 1.0 / (1.0 - self.es * sinphi * sinphi).sqrt();
        let lam_nu_cosphi = lam * nu * cosphi;
        let x = self.a * lam_nu_cosphi;
        let sinphi_m = (0.5 * (phi + self.phi0)).sin();
        let rho_m = (1.0 - self.es) / (1.0 - self.es * sinphi_m * sinphi_m).powf(1.5);
        let g = 1.0 + self.h0 / rho_m;
        let y = g * self.rho0 * ((phi - self.phi0) + self.b * lam_nu_cosphi * lam_nu_cosphi);
        Some((x, y))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let phi = self.phi0 + y / self.d - self.b * (x / self.c) * (x / self.c);
        let sinphi = phi.sin();
        let nu = 1.0 / (1.0 - self.es * sinphi * sinphi).sqrt();
        Some((x / (self.c * nu * phi.cos()), phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Azimuthal Equidistant (with the Guam variant).

#[derive(Debug)]
struct Aeqd {
    mode: Mode,
    ell: Ell,
    phi0: f64,
    en: Mlfn,
    mp: f64,
    m1: f64,
    guam: bool,
}

impl Aeqd {
    fn new(s: &mut Setup<'_>) -> Self {
        let mode = azimuthal_mode(s.phi0);
        let en = Mlfn::new(s.ell.es);
        let mp = match mode {
            Mode::NPole => en.mlfn(HALFPI, 1.0, 0.0),
            Mode::SPole => en.mlfn(-HALFPI, -1.0, 0.0),
            _ => 0.0,
        };
        let m1 = en.mlfn(s.phi0, s.phi0.sin(), s.phi0.cos());
        Self { mode, ell: s.ell, phi0: s.phi0, en, mp, m1, guam: s.p.has("guam") }
    }
}

impl Proj for Aeqd {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let es = self.ell.es;
        if self.guam {
            let (sinphi, cosphi) = phi.sin_cos();
            let t = 1.0 / (1.0 - es * sinphi * sinphi).sqrt();
            let x = lam * cosphi * t;
            let y = self.en.mlfn(phi, sinphi, cosphi) - self.m1 + 0.5 * lam * lam * cosphi * sinphi * t;
            return Some((x, y));
        }
        match self.mode {
            Mode::NPole | Mode::SPole => {
                let (sinphi, cosphi) = phi.sin_cos();
                let coslam = if self.mode == Mode::NPole { -lam.cos() } else { lam.cos() };
                let rho = (self.mp - self.en.mlfn(phi, sinphi, cosphi)).abs();
                Some((rho * lam.sin(), rho * coslam))
            }
            _ => {
                if lam.abs() < 1e-10 && (phi - self.phi0).abs() < 1e-10 {
                    return Some((0.0, 0.0));
                }
                let (s12, azi1) = geodesic::inverse(self.ell.es, self.phi0, 0.0, phi, lam)?;
                Some((s12 * azi1.sin(), s12 * azi1.cos()))
            }
        }
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        if self.guam {
            let x2 = 0.5 * x * x;
            let mut phi = self.phi0;
            let mut t = 1.0;
            for _ in 0..3 {
                t = self.ell.e * phi.sin();
                t = (1.0 - t * t).sqrt();
                phi = self.en.inv(self.m1 + y - x2 * phi.tan() * t);
            }
            return Some((x * t / phi.cos(), phi));
        }
        let rho = x.hypot(y);
        if rho < 1e-10 {
            return Some((0.0, self.phi0));
        }
        match self.mode {
            Mode::NPole | Mode::SPole => {
                let phi = self.en.inv(if self.mode == Mode::NPole { self.mp - rho } else { self.mp + rho });
                let lam = x.atan2(if self.mode == Mode::NPole { -y } else { y });
                Some((lam, phi))
            }
            _ => {
                let azi1 = x.atan2(y);
                geodesic::direct(self.ell.es, self.phi0, azi1, rho)
            }
        }
    }
}

/// Geodesics on an ellipsoid of semi-major axis 1, by Vincenty's formulas (which agree with
/// Karney's, which PROJ uses, to well below a millimetre away from antipodes).
mod geodesic {
    /// The distance and forward azimuth from (0, φ1) to (λ2, φ2).
    pub fn inverse(es: f64, phi1: f64, lam1: f64, phi2: f64, lam2: f64) -> Option<(f64, f64)> {
        let f = 1.0 - (1.0 - es).sqrt();
        let b = 1.0 - f;
        let l = lam2 - lam1;
        let u1 = ((1.0 - f) * phi1.tan()).atan();
        let u2 = ((1.0 - f) * phi2.tan()).atan();
        let (su1, cu1) = u1.sin_cos();
        let (su2, cu2) = u2.sin_cos();
        let mut lambda = l;
        for _ in 0..200 {
            let (sl, cl) = lambda.sin_cos();
            let sin_sigma = ((cu2 * sl).powi(2) + (cu1 * su2 - su1 * cu2 * cl).powi(2)).sqrt();
            if sin_sigma == 0.0 {
                return Some((0.0, 0.0));
            }
            let cos_sigma = su1 * su2 + cu1 * cu2 * cl;
            let sigma = sin_sigma.atan2(cos_sigma);
            let sin_alpha = cu1 * cu2 * sl / sin_sigma;
            let cos2_alpha = 1.0 - sin_alpha * sin_alpha;
            let cos2sm = if cos2_alpha != 0.0 { cos_sigma - 2.0 * su1 * su2 / cos2_alpha } else { 0.0 };
            let c = f / 16.0 * cos2_alpha * (4.0 + f * (4.0 - 3.0 * cos2_alpha));
            let prev = lambda;
            lambda = l + (1.0 - c) * f * sin_alpha * (sigma + c * sin_sigma * (cos2sm + c * cos_sigma * (-1.0 + 2.0 * cos2sm * cos2sm)));
            if (lambda - prev).abs() < 1e-14 {
                let u2sq = cos2_alpha * (1.0 - b * b) / (b * b);
                let a_ = 1.0 + u2sq / 16384.0 * (4096.0 + u2sq * (-768.0 + u2sq * (320.0 - 175.0 * u2sq)));
                let b_ = u2sq / 1024.0 * (256.0 + u2sq * (-128.0 + u2sq * (74.0 - 47.0 * u2sq)));
                let ds = b_
                    * sin_sigma
                    * (cos2sm
                        + b_ / 4.0
                            * (cos_sigma * (-1.0 + 2.0 * cos2sm * cos2sm)
                                - b_ / 6.0 * cos2sm * (-3.0 + 4.0 * sin_sigma * sin_sigma) * (-3.0 + 4.0 * cos2sm * cos2sm)));
                let s = b * a_ * (sigma - ds);
                let (sl, cl) = lambda.sin_cos();
                let azi = (cu2 * sl).atan2(cu1 * su2 - su1 * cu2 * cl);
                return Some((s, azi));
            }
        }
        None
    }

    /// The point at distance `s` along azimuth `azi1` from (0, φ1): (λ, φ).
    pub fn direct(es: f64, phi1: f64, azi1: f64, s: f64) -> Option<(f64, f64)> {
        let f = 1.0 - (1.0 - es).sqrt();
        let b = 1.0 - f;
        let (sa1, ca1) = azi1.sin_cos();
        let tu1 = (1.0 - f) * phi1.tan();
        let cu1 = 1.0 / (1.0 + tu1 * tu1).sqrt();
        let su1 = tu1 * cu1;
        let sigma1 = tu1.atan2(ca1);
        let sin_alpha = cu1 * sa1;
        let cos2_alpha = 1.0 - sin_alpha * sin_alpha;
        let u2sq = cos2_alpha * (1.0 - b * b) / (b * b);
        let a_ = 1.0 + u2sq / 16384.0 * (4096.0 + u2sq * (-768.0 + u2sq * (320.0 - 175.0 * u2sq)));
        let b_ = u2sq / 1024.0 * (256.0 + u2sq * (-128.0 + u2sq * (74.0 - 47.0 * u2sq)));
        let mut sigma = s / (b * a_);
        let (mut cos2sm, mut ss, mut cs);
        let mut it = 0;
        loop {
            cos2sm = (2.0 * sigma1 + sigma).cos();
            ss = sigma.sin();
            cs = sigma.cos();
            let ds = b_
                * ss
                * (cos2sm
                    + b_ / 4.0
                        * (cs * (-1.0 + 2.0 * cos2sm * cos2sm)
                            - b_ / 6.0 * cos2sm * (-3.0 + 4.0 * ss * ss) * (-3.0 + 4.0 * cos2sm * cos2sm)));
            let prev = sigma;
            sigma = s / (b * a_) + ds;
            it += 1;
            if (sigma - prev).abs() < 1e-14 || it > 200 {
                break;
            }
        }
        let (ss, cs) = sigma.sin_cos();
        let cos2sm = (2.0 * sigma1 + sigma).cos();
        let tmp = su1 * ss - cu1 * cs * ca1;
        let phi2 = (su1 * cs + cu1 * ss * ca1).atan2((1.0 - f) * (sin_alpha * sin_alpha + tmp * tmp).sqrt());
        let lambda = (ss * sa1).atan2(cu1 * cs - su1 * ss * ca1);
        let c = f / 16.0 * cos2_alpha * (4.0 + f * (4.0 - 3.0 * cos2_alpha));
        let l = lambda - (1.0 - c) * f * sin_alpha * (sigma + c * ss * (cos2sm + c * cs * (-1.0 + 2.0 * cos2sm * cos2sm)));
        Some((l, phi2))
    }
}

// ---------------------------------------------------------------------------------------------
// Orthographic, with the local orthographic variant (EPSG 1130: azimuth and scale).

#[derive(Debug)]
struct Ortho {
    ell: Ell,
    sinph0: f64,
    cosph0: f64,
    nu0: f64,
    cos_alpha: f64,
    sin_alpha: f64,
    k0: f64,
}

impl Ortho {
    fn new(s: &mut Setup<'_>) -> Self {
        let (sinph0, cosph0) = s.phi0.sin_cos();
        let es = s.ell.es;
        let nu0 = 1.0 / (1.0 - es * sinph0 * sinph0).sqrt();
        let alpha = s.p.angle("alpha").unwrap_or(0.0);
        let k0 = s.k0;
        Self { ell: s.ell, sinph0, cosph0, nu0, cos_alpha: alpha.cos(), sin_alpha: alpha.sin(), k0 }
    }
}

impl Proj for Ortho {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let es = self.ell.es;
        let (sinphi, cosphi) = phi.sin_cos();
        let coslam = lam.cos();
        // Visible hemisphere check, as PROJ does.
        if self.sinph0 * sinphi + self.cosph0 * cosphi * coslam < -1e-10 {
            return None;
        }
        let nu = 1.0 / (1.0 - es * sinphi * sinphi).sqrt();
        let x = nu * cosphi * lam.sin();
        let y = nu * (sinphi * self.cosph0 - cosphi * self.sinph0 * coslam) + es * (self.nu0 * self.sinph0 - nu * sinphi) * self.cosph0;
        // Local orthographic: rotate by the azimuth and scale.
        let xr = x * self.cos_alpha - y * self.sin_alpha;
        let yr = x * self.sin_alpha + y * self.cos_alpha;
        Some((self.k0 * xr, self.k0 * yr))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        // Undo the local scale/rotation. In geocentric coordinates on the unit ellipsoid,
        // x is the east component and y (less the origin shift) the north component.
        // Intersect the line perpendicular to that plane with X² + Y² + Z²/(1-es) = 1.
        // The nearer root has its ellipsoid normal facing the observer. Unlike an unbounded
        // Newton iteration, this also works at visible poles and rejects points past the limb.
        let (x, y) = (x / self.k0, y / self.k0);
        let east = x * self.cos_alpha + y * self.sin_alpha;
        let north = -x * self.sin_alpha + y * self.cos_alpha - self.ell.es * self.nu0 * self.sinph0 * self.cosph0;
        let r = self.ell.one_es;
        let a = self.cosph0 * self.cosph0 + self.sinph0 * self.sinph0 / r;
        let b = self.sinph0 * self.cosph0 * north * (1.0 / r - 1.0);
        let c = east * east + north * north * (self.sinph0 * self.sinph0 + self.cosph0 * self.cosph0 / r) - 1.0;
        let disc = b * b - a * c;
        if !disc.is_finite() || disc < -1e-11 {
            return None;
        }
        let toward = (-b + disc.max(0.0).sqrt()) / a;
        let gx = self.cosph0 * toward - self.sinph0 * north;
        let gz = self.sinph0 * toward + self.cosph0 * north;
        Some((east.atan2(gx), gz.atan2(r * gx.hypot(east))))
    }
}

// ---------------------------------------------------------------------------------------------
// Geostationary satellite view: the scan angles of a satellite at height `h` above the equator,
// times `h`. `+sweep=x` (GOES) sweeps the x angle across the y one, `+sweep=y` (Meteosat,
// Himawari, the default) the other way round.

#[derive(Debug)]
struct Geos {
    /// The satellite height above the ellipsoid, over the semi-major axis.
    h: f64,
    /// `a² / b²`.
    rp: f64,
    /// The semi-minor axis over the semi-major axis.
    b: f64,
    es: f64,
    sweep_x: bool,
}

impl Geos {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let h = s.p.f64("h").ok_or_else(|| err("geos needs +h"))?;
        if !h.is_finite() || h <= 0.0 || h / s.ell.a > 1e100 {
            return Err(err("invalid geos height +h"));
        }
        let sweep_x = match s.p.str("sweep") {
            None | Some("y") => false,
            Some("x") => true,
            Some(_) => return Err(err("geos +sweep must be x or y")),
        };
        let es = s.ell.es;
        Ok(Self { h: h / s.ell.a, rp: 1.0 / s.ell.one_es, b: s.ell.one_es.sqrt(), es, sweep_x })
    }
}

impl Proj for Geos {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        // The point on the ellipsoid, from its geocentric latitude.
        let phic = (phi.tan() / self.rp).atan();
        let (sinc, cosc) = phic.sin_cos();
        let r = self.b / (1.0 - self.es * cosc * cosc).sqrt();
        let (vx, vy, vz) = (r * cosc * lam.cos(), r * cosc * lam.sin(), r * sinc);
        let d = 1.0 + self.h - vx;
        // Hidden from the satellite.
        if d * vx - vy * vy - self.rp * vz * vz < 0.0 {
            return None;
        }
        let (x, y) = if self.sweep_x { ((vy / vz.hypot(d)).atan(), (vz / d).atan()) } else { ((vy / d).atan(), (vz / vy.hypot(d)).atan()) };
        Some((self.h * x, self.h * y))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        // The ray from the satellite along the scan angles, met with the ellipsoid.
        let (tx, ty) = (x / self.h, y / self.h);
        let (ux, uy, uz) = if self.sweep_x { (-ty.cos(), tx.tan(), ty.sin()) } else { (-tx.cos(), tx.sin(), ty.tan()) };
        let sx = 1.0 + self.h;
        let qa = ux * ux + uy * uy + self.rp * uz * uz;
        let qb = 2.0 * sx * ux;
        let qc = sx * sx - 1.0;
        let disc = qb * qb - 4.0 * qa * qc;
        if disc < 0.0 {
            return None;
        }
        // The nearer root, in the form that keeps its precision near the limb.
        let t = 2.0 * qc / (-qb + disc.sqrt());
        // Scan angles past a right angle point the ray away from the Earth.
        if t.is_nan() || t <= 0.0 {
            return None;
        }
        let (px, py, pz) = (sx + t * ux, t * uy, t * uz);
        Some((py.atan2(px), (self.rp * pz / px.hypot(py)).atan()))
    }
}

// ---------------------------------------------------------------------------------------------
// The world projections below are spherical in PROJ, which computes them on a sphere of the
// semi-major axis whatever the ellipsoid (it sets `es` to 0), except the Gauss-Schreiber
// transverse Mercator.

/// PROJ's `aasin`: `asin`, refusing arguments past ±1 by more than rounding.
fn aasin_checked(v: f64) -> Option<f64> {
    if v.abs() > 1.0 + 1e-14 { None } else { Some(aasin(v)) }
}

/// PROJ's `pj_generic_inverse_2d`: Newton's method on the forward projection, from `(lam, phi)`,
/// until the forward projection is within `tol` of `(x, y)`.
fn generic_inverse_2d(fwd: impl Fn(f64, f64) -> Option<(f64, f64)>, x: f64, y: f64, lam: f64, phi: f64, tol: f64) -> Option<(f64, f64)> {
    use std::f64::consts::PI;
    let (mut lam, mut phi) = (lam, phi);
    let (mut d_lam_x, mut d_lam_y, mut d_phi_x, mut d_phi_y) = (0.0, 0.0, 0.0, 0.0);
    for i in 0..15 {
        let (ax, ay) = fwd(lam, phi)?;
        let (dx, dy) = (ax - x, ay - y);
        if dx.abs() < tol && dy.abs() < tol {
            return Some((lam, phi));
        }
        // The Jacobian, by finite differences, only while far from the result.
        if i == 0 || dx.abs() > 1e-6 || dy.abs() > 1e-6 {
            let dl = if lam > 0.0 { -1e-6 } else { 1e-6 };
            let (x2, y2) = fwd(lam + dl, phi)?;
            let (x_lam, y_lam) = ((x2 - ax) / dl, (y2 - ay) / dl);
            let dp = if phi > 0.0 { -1e-6 } else { 1e-6 };
            let (x2, y2) = fwd(lam, phi + dp)?;
            let (x_phi, y_phi) = ((x2 - ax) / dp, (y2 - ay) / dp);
            let det = x_lam * y_phi - x_phi * y_lam;
            if det != 0.0 {
                d_lam_x = y_phi / det;
                d_lam_y = -x_phi / det;
                d_phi_x = -y_lam / det;
                d_phi_y = x_lam / det;
            }
        }
        // Corrections limited against overshoots from a bad first guess.
        lam = (lam - (dx * d_lam_x + dy * d_lam_y).clamp(-0.3, 0.3)).clamp(-PI, PI);
        phi = (phi - (dx * d_phi_x + dy * d_phi_y).clamp(-0.3, 0.3)).clamp(-HALFPI, HALFPI);
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Robinson: a cubic in the latitude per 5° band, with PROJ's single-precision coefficients
// (their constant terms are Robinson's table rounded to `f32`).

#[derive(Debug)]
struct Robin;

/// `X(φ)` per 5° band: `c0 + c1 z + c2 z² + c3 z³`, z the offset into the band in degrees.
const ROBIN_X: [[f32; 4]; 19] = [
    [1.0, 2.2199e-17, -7.15515e-05, 3.1103e-06],
    [0.9986, -0.000482243, -2.4897e-05, -1.3309e-06],
    [0.9954, -0.00083103, -4.48605e-05, -9.86701e-07],
    [0.99, -0.00135364, -5.9661e-05, 3.6777e-06],
    [0.9822, -0.00167442, -4.49547e-06, -5.72411e-06],
    [0.973, -0.00214868, -9.03571e-05, 1.8736e-08],
    [0.96, -0.00305085, -9.00761e-05, 1.64917e-06],
    [0.9427, -0.00382792, -6.53386e-05, -2.6154e-06],
    [0.9216, -0.00467746, -0.00010457, 4.81243e-06],
    [0.8962, -0.00536223, -3.23831e-05, -5.43432e-06],
    [0.8679, -0.00609363, -0.000113898, 3.32484e-06],
    [0.835, -0.00698325, -6.40253e-05, 9.34959e-07],
    [0.7986, -0.00755338, -5.00009e-05, 9.35324e-07],
    [0.7597, -0.00798324, -3.5971e-05, -2.27626e-06],
    [0.7186, -0.00851367, -7.01149e-05, -8.6303e-06],
    [0.6732, -0.00986209, -0.000199569, 1.91974e-05],
    [0.6213, -0.010418, 8.83923e-05, 6.24051e-06],
    [0.5722, -0.00906601, 0.000182, 6.24051e-06],
    [0.5322, -0.00677797, 0.000275608, 6.24051e-06],
];

/// `Y(φ)` per 5° band, as `ROBIN_X`.
const ROBIN_Y: [[f32; 4]; 19] = [
    [-5.20417e-18, 0.0124, 1.21431e-18, -8.45284e-11],
    [0.062, 0.0124, -1.26793e-09, 4.22642e-10],
    [0.124, 0.0124, 5.07171e-09, -1.60604e-09],
    [0.186, 0.0123999, -1.90189e-08, 6.00152e-09],
    [0.248, 0.0124002, 7.10039e-08, -2.24e-08],
    [0.31, 0.0123992, -2.64997e-07, 8.35986e-08],
    [0.372, 0.0124029, 9.88983e-07, -3.11994e-07],
    [0.434, 0.0123893, -3.69093e-06, -4.35621e-07],
    [0.4958, 0.0123198, -1.02252e-05, -3.45523e-07],
    [0.5571, 0.0121916, -1.54081e-05, -5.82288e-07],
    [0.6176, 0.0119938, -2.41424e-05, -5.25327e-07],
    [0.6769, 0.011713, -3.20223e-05, -5.16405e-07],
    [0.7346, 0.0113541, -3.97684e-05, -6.09052e-07],
    [0.7903, 0.0109107, -4.89042e-05, -1.04739e-06],
    [0.8435, 0.0103431, -6.4615e-05, -1.40374e-09],
    [0.8936, 0.00969686, -6.4636e-05, -8.547e-06],
    [0.9394, 0.00840947, -0.000192841, -4.2106e-06],
    [0.9761, 0.00616527, -0.000256, -4.2106e-06],
    [1.0, 0.00328947, -0.000319159, -4.2106e-06],
];

const ROBIN_FXC: f64 = 0.8487;
const ROBIN_FYC: f64 = 1.3523;
/// Radians to 5° bands, and back.
const ROBIN_C1: f64 = 11.45915590261646417544;
const ROBIN_RC1: f64 = 0.08726646259971647884;
const ROBIN_NODES: usize = 18;

/// A band's cubic at `z` (PROJ's `V`).
fn robin_v(c: &[f32; 4], z: f64) -> f64 {
    c[0] as f64 + z * (c[1] as f64 + z * (c[2] as f64 + z * c[3] as f64))
}

/// Its derivative (PROJ's `DV`).
fn robin_dv(c: &[f32; 4], z: f64) -> f64 {
    c[1] as f64 + 2.0 * z * c[2] as f64 + z * z * 3.0 * c[3] as f64
}

impl Proj for Robin {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        if phi.is_nan() {
            return None;
        }
        let dphi = phi.abs();
        let i = ((dphi * ROBIN_C1 + 1e-15).floor() as usize).min(ROBIN_NODES);
        let z = (dphi - ROBIN_RC1 * i as f64).to_degrees();
        let x = robin_v(&ROBIN_X[i], z) * ROBIN_FXC * lam;
        let y = robin_v(&ROBIN_Y[i], z) * ROBIN_FYC;
        Some((x, if phi < 0.0 { -y } else { y }))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let lam = x / ROBIN_FXC;
        let phi = (y / ROBIN_FYC).abs();
        if phi.is_nan() {
            return None;
        }
        if phi >= 1.0 {
            // Past the pole by more than rounding: outside the projection.
            if phi > 1.000001 {
                return None;
            }
            return Some((lam / ROBIN_X[ROBIN_NODES][0] as f64, if y < 0.0 { -HALFPI } else { HALFPI }));
        }
        // The band holding the northing, then Newton's method on its cubic from a linear guess.
        let mut i = (phi * ROBIN_NODES as f64).floor() as usize;
        loop {
            if ROBIN_Y[i][0] as f64 > phi {
                i -= 1;
            } else if ROBIN_Y[i + 1][0] as f64 <= phi {
                i += 1;
            } else {
                break;
            }
        }
        let c = &ROBIN_Y[i];
        let mut t = 5.0 * (phi - c[0] as f64) / (ROBIN_Y[i + 1][0] as f64 - c[0] as f64);
        let mut converged = false;
        for _ in 0..100 {
            let t1 = (robin_v(c, t) - phi) / robin_dv(c, t);
            t -= t1;
            if t1.abs() < 1e-10 {
                converged = true;
                break;
            }
        }
        if !converged {
            return None;
        }
        let phi = (5.0 * i as f64 + t).to_radians();
        let lam = lam / robin_v(&ROBIN_X[i], t);
        // Past the antimeridian, by more than the rounding of terrio's normalization (PROJ's
        // differs by an ulp).
        if lam.abs() > std::f64::consts::PI + 1e-12 {
            return None;
        }
        Some((lam, if y < 0.0 { -phi } else { phi }))
    }
}

// ---------------------------------------------------------------------------------------------
// Mollweide and Wagner IV/V, ported from PROJ's moll.cpp. Keep PROJ's iteration limit and
// step tolerance, including its finite approximation to the Mollweide pole.

#[derive(Debug)]
struct Moll {
    cx: f64,
    cy: f64,
    cp: f64,
}

impl Moll {
    fn new(name: &str) -> Self {
        if name == "wag5" {
            return Self { cx: 0.90977, cy: 1.65014, cp: 3.00896 };
        }
        let p = if name == "wag4" { PI / 3.0 } else { HALFPI };
        let sp = p.sin();
        let cp = 2.0 * p + (2.0 * p).sin();
        let r = (2.0 * PI * sp / cp).sqrt();
        Self { cx: 2.0 * r / PI, cy: r / sp, cp }
    }
}

impl Proj for Moll {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        if !lam.is_finite() || !phi.is_finite() {
            return None;
        }
        let k = self.cp * phi.sin();
        let mut theta = phi;
        let mut converged = false;
        for _ in 0..30 {
            let step = (theta + theta.sin() - k) / (1.0 + theta.cos());
            theta -= step;
            if step.abs() < 1e-7 {
                converged = true;
                break;
            }
        }
        theta = if converged { theta * 0.5 } else { HALFPI.copysign(theta) };
        Some((self.cx * lam * theta.cos(), self.cy * theta.sin()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let theta = aasin_checked(y / self.cy)?;
        let lam = x / (self.cx * theta.cos());
        // PROJ excludes the antimeridian itself from this inverse's domain.
        if !lam.is_finite() || lam.abs() >= PI {
            return None;
        }
        let twice = 2.0 * theta;
        Some((lam, aasin_checked((twice + twice.sin()) / self.cp)?))
    }
}

// ---------------------------------------------------------------------------------------------
// HEALPix and rHEALPix, ported from PROJ's healpix.cpp (Raichev and Speth): the equal-area HEALPix
// projection, cylindrical equal-area between the latitudes +-asin(2/3) and in four interrupted
// polar caps beyond them, and rHEALPix, which combines each pole's caps into one square. On an
// ellipsoid, as in PROJ, the latitude is the authalic one and the sphere has the authalic radius.

/// PROJ's slack on the edges of the image.
const HEALPIX_EPS: f64 = 1e-15;

/// The rotations PROJ indexes by -3 to 3 (counterclockwise by that many right angles).
fn healpix_rotation(index: i32) -> [[f64; 2]; 2] {
    match index.rem_euclid(4) {
        1 => [[0.0, -1.0], [1.0, 0.0]],
        2 => [[-1.0, 0.0], [0.0, -1.0]],
        3 => [[0.0, 1.0], [-1.0, 0.0]],
        _ => [[1.0, 0.0], [0.0, 1.0]],
    }
}

/// PROJ's `sign`: 1, -1 or 0.
fn healpix_sign(v: f64) -> f64 {
    if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// PROJ's `rotate`.
fn healpix_rotate((x, y): (f64, f64), angle: f64) -> (f64, f64) {
    (x * angle.cos() - y * angle.sin(), y * angle.cos() + x * angle.sin())
}

/// PROJ's `pnpoly`: whether the point is inside the polygon (or one of its vertices). As in
/// PROJ, the edge from the last vertex back to the first is not counted.
fn healpix_pnpoly(vert: &[(f64, f64)], x: f64, y: f64) -> bool {
    if vert.iter().any(|&(vx, vy)| x == vx && y == vy) {
        return true;
    }
    let mut counter = 0;
    let mut p1 = vert[0];
    for &p2 in &vert[1..] {
        if y > p1.1.min(p2.1) && y <= p1.1.max(p2.1) && x <= p1.0.max(p2.0) && p1.1 != p2.1 {
            let xinters = (y - p1.1) * (p2.0 - p1.0) / (p2.1 - p1.1) + p1.0;
            if p1.0 == p2.0 || x <= xinters {
                counter += 1;
            }
        }
        p1 = p2;
    }
    counter % 2 == 1
}

/// PROJ's `healpix_sphere`: HEALPix on the unit sphere.
fn healpix_sphere(lam: f64, phi: f64) -> (f64, f64) {
    let phi0 = (2.0f64 / 3.0).asin();
    if phi.abs() <= phi0 {
        (lam, 3.0 * PI / 8.0 * phi.sin())
    } else {
        let sigma = (3.0 * (1.0 - phi.sin().abs())).sqrt();
        let cn = (2.0 * lam / PI + 2.0).floor().min(3.0);
        let lamc = -3.0 * FORTPI + HALFPI * cn;
        (lamc + (lam - lamc) * sigma, healpix_sign(phi) * FORTPI * (2.0 - sigma))
    }
}

/// PROJ's inverse of `healpix_sphere`.
fn healpix_sphere_inverse(x: f64, y: f64) -> (f64, f64) {
    if y.abs() <= FORTPI {
        (x, (8.0 * y / (3.0 * PI)).asin())
    } else if y.abs() < HALFPI {
        let cn = (2.0 * x / PI + 2.0).floor().min(3.0);
        let xc = -3.0 * FORTPI + HALFPI * cn;
        let tau = 2.0 - 4.0 * y.abs() / PI;
        (xc + (x - xc) / tau, healpix_sign(y) * (1.0 - tau * tau / 3.0).asin())
    } else {
        (-PI, healpix_sign(y) * HALFPI)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum HealpixRegion {
    North,
    South,
    Equatorial,
}

/// The authalic latitude as PROJ 9.8.1 computes it on an ellipsoid.
#[derive(Debug)]
struct HealpixAuthalic {
    /// Geodetic to authalic (`C[xi,phi]`), used when |n| < 0.01.
    forward: Option<[f64; 6]>,
    /// Authalic to geodetic (`C[phi,xi]`).
    apa: [f64; 6],
    e: f64,
    one_es: f64,
    qp: f64,
}

impl HealpixAuthalic {
    /// PROJ's `pj_authalic_lat`.
    fn forward(&self, phi: f64) -> f64 {
        match &self.forward {
            Some(c) => authlat(phi, c),
            None => (qsfn(phi.sin(), self.e, self.one_es) / self.qp).clamp(-1.0, 1.0).asin(),
        }
    }

    /// PROJ's `pj_authalic_lat_inverse`.
    fn inverse(&self, beta: f64) -> f64 {
        authlat_inverse(beta, &self.apa, self.e, self.one_es, self.qp)
    }
}

#[derive(Debug)]
struct Healpix {
    /// rHEALPix's north and south polar square positions (0 to 3); `None` for HEALPix.
    squares: Option<(i32, i32)>,
    /// HEALPix's rotation of the image (`+rot_xy`), in radians.
    rot_xy: f64,
    /// `None` on a sphere.
    authalic: Option<HealpixAuthalic>,
    /// The authalic radius on the unit semi-major axis (1 on a sphere).
    scale: f64,
}

impl Healpix {
    fn new(s: &Setup<'_>, rhealpix: bool) -> Result<Self> {
        let squares = if rhealpix {
            let int = |key: &str| proj_int(s.p, "rhealpix", key);
            let (north, south) = (int("north_square")?, int("south_square")?);
            if !(0..=3).contains(&north) {
                return Err(err("rhealpix: invalid north_square, it should be in [0,3]"));
            }
            if !(0..=3).contains(&south) {
                return Err(err("rhealpix: invalid south_square, it should be in [0,3]"));
            }
            Some((north, south))
        } else {
            None
        };
        let rot_xy = if rhealpix { 0.0 } else { s.p.angle("rot_xy").unwrap_or(0.0) };
        let (authalic, scale) = if s.ell.es != 0.0 {
            let (e, one_es) = (s.ell.e, s.ell.one_es);
            let b = one_es.sqrt();
            let n = (1.0 - b) / (1.0 + b);
            let qp = qsfn(1.0, e, one_es);
            let forward = (n.abs() < 0.01).then(|| authset_forward(s.ell.es));
            (Some(HealpixAuthalic { forward, apa: authset(s.ell.es), e, one_es, qp }), (0.5 * qp).sqrt())
        } else {
            (None, 1.0)
        };
        Ok(Self { squares, rot_xy, authalic, scale })
    }

    /// PROJ's `in_image`.
    fn in_image(&self, x: f64, y: f64) -> bool {
        let e = HEALPIX_EPS;
        match self.squares {
            None => {
                let verts = [
                    (-PI - e, FORTPI),
                    (-3.0 * FORTPI, HALFPI + e),
                    (-HALFPI, FORTPI + e),
                    (-FORTPI, HALFPI + e),
                    (0.0, FORTPI + e),
                    (FORTPI, HALFPI + e),
                    (HALFPI, FORTPI + e),
                    (3.0 * FORTPI, HALFPI + e),
                    (PI + e, FORTPI),
                    (PI + e, -FORTPI),
                    (3.0 * FORTPI, -HALFPI - e),
                    (HALFPI, -FORTPI - e),
                    (FORTPI, -HALFPI - e),
                    (0.0, -FORTPI - e),
                    (-FORTPI, -HALFPI - e),
                    (-HALFPI, -FORTPI - e),
                    (-3.0 * FORTPI, -HALFPI - e),
                    (-PI - e, -FORTPI),
                    (-PI - e, FORTPI),
                ];
                healpix_pnpoly(&verts, x, y)
            }
            Some((north, south)) => {
                let (north, south) = (north as f64, south as f64);
                let verts = [
                    (-PI - e, FORTPI + e),
                    (-PI + north * HALFPI - e, FORTPI + e),
                    (-PI + north * HALFPI - e, 3.0 * FORTPI + e),
                    (-PI + (north + 1.0) * HALFPI + e, 3.0 * FORTPI + e),
                    (-PI + (north + 1.0) * HALFPI + e, FORTPI + e),
                    (PI + e, FORTPI + e),
                    (PI + e, -FORTPI - e),
                    (-PI + (south + 1.0) * HALFPI + e, -FORTPI - e),
                    (-PI + (south + 1.0) * HALFPI + e, -3.0 * FORTPI - e),
                    (-PI + south * HALFPI - e, -3.0 * FORTPI - e),
                    (-PI + south * HALFPI - e, -FORTPI - e),
                    (-PI - e, -FORTPI - e),
                ];
                healpix_pnpoly(&verts, x, y)
            }
        }
    }

    /// PROJ's `get_cap`: the polar cap's number, its tip and the region of a point of the HEALPix
    /// image (forward), or of the rHEALPix image (inverse).
    fn cap(x: f64, y: f64, north: i32, south: i32, inverse: bool) -> (i32, f64, f64, HealpixRegion) {
        let e = HEALPIX_EPS;
        if !inverse {
            let (region, c) = if y > FORTPI {
                (HealpixRegion::North, HALFPI)
            } else if y < -FORTPI {
                (HealpixRegion::South, -HALFPI)
            } else {
                return (0, x, y, HealpixRegion::Equatorial);
            };
            let (cn, cx) = if x < -HALFPI {
                (0, -3.0 * FORTPI)
            } else if x < 0.0 {
                (1, -FORTPI)
            } else if x < HALFPI {
                (2, FORTPI)
            } else {
                (3, 3.0 * FORTPI)
            };
            (cn, cx, c, region)
        } else if y > FORTPI {
            let cx = -3.0 * FORTPI + north as f64 * HALFPI;
            let x = x - north as f64 * HALFPI;
            // The HEALPix cap that the point moves to when the polar square is taken apart.
            let cn = if y >= -x - FORTPI - e && y < x + 5.0 * FORTPI - e {
                (north + 1) % 4
            } else if y > -x - FORTPI + e && y >= x + 5.0 * FORTPI - e {
                (north + 2) % 4
            } else if y <= -x - FORTPI + e && y > x + 5.0 * FORTPI + e {
                (north + 3) % 4
            } else {
                north
            };
            (cn, cx, HALFPI, HealpixRegion::North)
        } else if y < -FORTPI {
            let cx = -3.0 * FORTPI + south as f64 * HALFPI;
            let x = x - south as f64 * HALFPI;
            let cn = if y <= x + FORTPI + e && y > -x - 5.0 * FORTPI + e {
                (south + 1) % 4
            } else if y < x + FORTPI - e && y <= -x - 5.0 * FORTPI + e {
                (south + 2) % 4
            } else if y >= x + FORTPI - e && y < -x - 5.0 * FORTPI - e {
                (south + 3) % 4
            } else {
                south
            };
            (cn, cx, -HALFPI, HealpixRegion::South)
        } else {
            (0, x, y, HealpixRegion::Equatorial)
        }
    }

    /// PROJ's `combine_caps`: HEALPix to rHEALPix (forward) or back (inverse).
    fn combine_caps(x: f64, y: f64, north: i32, south: i32, inverse: bool) -> (f64, f64) {
        let (cn, cx, cy, region) = Self::cap(x, y, north, south, inverse);
        if region == HealpixRegion::Equatorial {
            return (cx, cy);
        }
        let pole = if region == HealpixRegion::North { north } else { south };
        // Forward, rotate about the cap's tip and move it to the polar square; inverse, undo it.
        let turn = if (region == HealpixRegion::North) != inverse { cn - pole } else { -(cn - pole) };
        let r = healpix_rotation(turn);
        let (vx, vy) = (x - cx, y - cy);
        let (dx, dy) = (r[0][0] * vx + r[0][1] * vy, r[1][0] * vx + r[1][1] * vy);
        let ax = -3.0 * FORTPI + (if inverse { cn } else { pole }) as f64 * HALFPI;
        let ay = if region == HealpixRegion::North { HALFPI } else { -HALFPI };
        (dx + ax, dy + ay)
    }
}

impl Proj for Healpix {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let phi = self.authalic.as_ref().map_or(phi, |a| a.forward(phi));
        let (x, y) = healpix_sphere(lam, phi);
        let (x, y) = match self.squares {
            None => healpix_rotate((x, y), -self.rot_xy),
            Some((north, south)) => Self::combine_caps(x, y, north, south, false),
        };
        Some((x * self.scale, y * self.scale))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (x, y) = (x / self.scale, y / self.scale);
        let (x, y) = match self.squares {
            None => healpix_rotate((x, y), self.rot_xy),
            Some(_) => (x, y),
        };
        if !self.in_image(x, y) {
            return None;
        }
        let (x, y) = match self.squares {
            None => (x, y),
            Some((north, south)) => Self::combine_caps(x, y, north, south, true),
        };
        let (lam, phi) = healpix_sphere_inverse(x, y);
        Some((lam, self.authalic.as_ref().map_or(phi, |a| a.inverse(phi))))
    }
}

// ---------------------------------------------------------------------------------------------
// Interrupted Goode Homolosine, ported from PROJ's igh.cpp: twelve zones, each a spherical
// sinusoidal (between the equator and 40 degrees 44' 11.8") or Mollweide (poleward of it) projection
// with its own central meridian, the Mollweide zones shifted to meet the sinusoidal ones.

/// The latitude where the sinusoidal zones meet the Mollweide ones.
const IGH_PHI_BOUNDARY: f64 = (40.0 + 44.0 / 60.0 + 11.8 / 3600.0) * PI / 180.0;

/// PROJ's slack on the zones' edges.
const IGH_EPSLN: f64 = 1e-10;

#[derive(Debug)]
struct Igh {
    moll: Moll,
    /// The offset of the northern Mollweide zones (the southern ones are shifted by -dy0).
    dy0: f64,
}

impl Igh {
    fn new() -> Result<Self> {
        let moll = Moll::new("moll");
        let (_, y1) = moll.fwd(0.0, IGH_PHI_BOUNDARY).ok_or_else(|| err("igh: Mollweide of the boundary"))?;
        Ok(Self { moll, dy0: IGH_PHI_BOUNDARY - y1 })
    }

    /// The zone (1 to 12) of a longitude and latitude in radians (forward), or of an easting and
    /// northing on the unit sphere, with the zones' offsets (inverse), as in PROJ. The same edges
    /// serve both: each zone's easting spans its longitudes (its false easting is its central
    /// meridian, and the zones' extents do not overlap), and the Mollweide zones' offsets put the
    /// sinusoidal/Mollweide boundary at the northing IGH_PHI_BOUNDARY.
    fn zone(x: f64, y: f64) -> usize {
        if y >= IGH_PHI_BOUNDARY {
            if x <= -40f64.to_radians() { 1 } else { 2 }
        } else if y >= 0.0 {
            if x <= -40f64.to_radians() { 3 } else { 4 }
        } else {
            let band = if y >= -IGH_PHI_BOUNDARY { 5 } else { 9 };
            band + if x <= -100f64.to_radians() {
                0
            } else if x <= -20f64.to_radians() {
                1
            } else if x <= 80f64.to_radians() {
                2
            } else {
                3
            }
        }
    }

    /// A zone's central meridian (also its false easting) and false northing.
    fn origin(&self, z: usize) -> (f64, f64) {
        let lam0: f64 = match z {
            1 | 3 => -100.0,
            2 | 4 => 30.0,
            5 | 9 => -160.0,
            6 | 10 => -60.0,
            7 | 11 => 20.0,
            _ => 140.0,
        };
        let y0 = match z {
            1 | 2 => self.dy0,
            9..=12 => -self.dy0,
            _ => 0.0,
        };
        (lam0.to_radians(), y0)
    }

    fn mollweide(z: usize) -> bool {
        !(3..=8).contains(&z)
    }
}

impl Proj for Igh {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let z = Self::zone(lam, phi);
        let (lam0, y0) = self.origin(z);
        let lam = lam - lam0;
        let (x, y) = if Self::mollweide(z) { self.moll.fwd(lam, phi)? } else { (lam * phi.cos(), phi) };
        Some((x + lam0, y + y0))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        // PROJ's bounds. The southern bound is -y90 + EPSLN, where the northern is y90 + EPSLN, as
        // in PROJ's igh.cpp: it refuses the south pole's own northing, -y90.
        let y90 = self.dy0 + 2f64.sqrt(); // sqrt(2) is the Mollweide pole's northing.
        if y > y90 + IGH_EPSLN || y < -y90 + IGH_EPSLN {
            return None;
        }
        let z = Self::zone(x, y);
        let (lam0, y0) = self.origin(z);
        let (x, y) = (x - lam0, y - y0);
        let (lam, phi) = if Self::mollweide(z) { self.moll.inv(x, y)? } else { (x / y.cos(), y) };
        let lam = lam + lam0;
        // The point must be in the zone's own longitudes (zones 1 and 2 also hold the lobes'
        // polar caps that PROJ allows).
        let within = |lo: f64, hi: f64| lam >= lo.to_radians() - IGH_EPSLN && lam <= hi.to_radians() + IGH_EPSLN;
        let above = |lat: f64| phi >= lat.to_radians() - IGH_EPSLN && phi <= 90f64.to_radians() + IGH_EPSLN;
        let ok = match z {
            1 => within(-180.0, -40.0) || (within(-40.0, -10.0) && above(60.0)),
            2 => within(-40.0, 180.0) || (within(-180.0, -160.0) && above(50.0)) || (within(-50.0, -40.0) && above(60.0)),
            3 | 5 | 9 => within(-180.0, if z == 3 { -40.0 } else { -100.0 }),
            4 => within(-40.0, 180.0),
            6 | 10 => within(-100.0, -20.0),
            7 | 11 => within(-20.0, 80.0),
            _ => within(80.0, 180.0),
        };
        ok.then_some((lam, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Quadrilateralized spherical cube, as PROJ's qsc.cpp: one of the six cube faces, chosen by the
// projection centre, with the ellipsoid shifted to the sphere by geocentric latitude.

#[derive(Debug, Clone, Copy, PartialEq)]
enum QscFace {
    Front,
    Right,
    Back,
    Left,
    Top,
    Bottom,
}

#[derive(Debug)]
struct Qsc {
    face: QscFace,
    /// The semi-minor axis on the unit semi-major axis; 1 on a sphere.
    b: f64,
    es: f64,
}

/// The four areas of a cube face, counterclockwise from the area of definition.
#[derive(Debug, Clone, Copy, PartialEq)]
enum QscArea {
    A0,
    A1,
    A2,
    A3,
}

/// The theta angle and area of a point on an equatorial face (PROJ's qsc_fwd_equat_face_theta).
fn qsc_equat_face_theta(colat: f64, y: f64, x: f64) -> (f64, QscArea) {
    if colat < 1e-10 {
        return (0.0, QscArea::A0);
    }
    let theta = y.atan2(x);
    if theta.abs() <= FORTPI {
        (theta, QscArea::A0)
    } else if theta > FORTPI && theta <= HALFPI + FORTPI {
        (theta - HALFPI, QscArea::A1)
    } else if theta > HALFPI + FORTPI || theta <= -(HALFPI + FORTPI) {
        (if theta >= 0.0 { theta - PI } else { theta + PI }, QscArea::A2)
    } else {
        (theta + HALFPI, QscArea::A3)
    }
}

/// PROJ's qsc_shift_longitude_origin.
fn qsc_shift_longitude(longitude: f64, offset: f64) -> f64 {
    let slon = longitude + offset;
    if slon < -PI {
        slon + 2.0 * PI
    } else if slon > PI {
        slon - 2.0 * PI
    } else {
        slon
    }
}

impl Qsc {
    fn new(s: &Setup<'_>) -> Self {
        let face = if s.phi0 >= HALFPI - FORTPI / 2.0 {
            QscFace::Top
        } else if s.phi0 <= -(HALFPI - FORTPI / 2.0) {
            QscFace::Bottom
        } else if s.lam0.abs() <= FORTPI {
            QscFace::Front
        } else if s.lam0.abs() <= HALFPI + FORTPI {
            if s.lam0 > 0.0 { QscFace::Right } else { QscFace::Left }
        } else {
            QscFace::Back
        };
        Self { face, b: (1.0 - s.ell.es).sqrt(), es: s.ell.es }
    }
}

impl Proj for Qsc {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        // The geodetic latitude as a geocentric latitude: the shift from the ellipsoid to the
        // sphere. b is also 1 - f on the unit semi-major axis.
        let lat = if self.es != 0.0 { (self.b * self.b * phi.tan()).atan() } else { phi };
        let mut longitude = lam;
        // colat is PROJ's phi: the angle from the face centre.
        let (theta, colat, area);
        match self.face {
            QscFace::Top => {
                colat = HALFPI - lat;
                (theta, area) = if (FORTPI..=HALFPI + FORTPI).contains(&longitude) {
                    (longitude - HALFPI, QscArea::A0)
                } else if longitude > HALFPI + FORTPI || longitude <= -(HALFPI + FORTPI) {
                    (if longitude > 0.0 { longitude - PI } else { longitude + PI }, QscArea::A1)
                } else if longitude > -(HALFPI + FORTPI) && longitude <= -FORTPI {
                    (longitude + HALFPI, QscArea::A2)
                } else {
                    (longitude, QscArea::A3)
                };
            }
            QscFace::Bottom => {
                colat = HALFPI + lat;
                (theta, area) = if (FORTPI..=HALFPI + FORTPI).contains(&longitude) {
                    (-longitude + HALFPI, QscArea::A0)
                } else if (-FORTPI..FORTPI).contains(&longitude) {
                    (-longitude, QscArea::A1)
                } else if (-(HALFPI + FORTPI)..-FORTPI).contains(&longitude) {
                    (-longitude - HALFPI, QscArea::A2)
                } else {
                    (if longitude > 0.0 { -longitude + PI } else { -longitude - PI }, QscArea::A3)
                };
            }
            face => {
                longitude = match face {
                    QscFace::Right => qsc_shift_longitude(longitude, HALFPI),
                    QscFace::Back => qsc_shift_longitude(longitude, PI),
                    QscFace::Left => qsc_shift_longitude(longitude, -HALFPI),
                    _ => longitude,
                };
                let (sinlat, coslat) = lat.sin_cos();
                let (sinlon, coslon) = longitude.sin_cos();
                let (q, r, s) = (coslat * coslon, coslat * sinlon, sinlat);
                let (p, y, x) = match face {
                    QscFace::Front => (q.acos(), s, r),
                    QscFace::Right => (r.acos(), s, -q),
                    QscFace::Back => ((-q).acos(), s, -r),
                    _ => ((-r).acos(), s, q),
                };
                colat = p;
                (theta, area) = qsc_equat_face_theta(colat, y, x);
            }
        }
        // mu and nu for the area of definition: Eq. (3-21) of O'Neill and Laubscher (1976), with
        // its typos corrected against Eq. (3-14), and t = tan(nu) from Eq. (3-38).
        let mut mu = ((12.0 / PI) * (theta + (theta.sin() * FORTPI.cos()).acos() - HALFPI)).atan();
        let t = ((1.0 - colat.cos()) / (mu.cos() * mu.cos()) / (1.0 - (1.0 / theta.cos()).atan().cos())).sqrt();
        mu += match area {
            QscArea::A1 => HALFPI,
            QscArea::A2 => PI,
            QscArea::A3 => PI + HALFPI,
            _ => 0.0,
        };
        Some((t * mu.cos(), t * mu.sin()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let nu = x.hypot(y).atan();
        let mut mu = y.atan2(x);
        let area = if x >= 0.0 && x >= y.abs() {
            QscArea::A0
        } else if y >= 0.0 && y >= x.abs() {
            mu -= HALFPI;
            QscArea::A1
        } else if x < 0.0 && -x >= y.abs() {
            mu = if mu < 0.0 { mu + PI } else { mu - PI };
            QscArea::A2
        } else {
            mu += HALFPI;
            QscArea::A3
        };
        // phi and theta for the area of definition.
        let t = (PI / 12.0) * mu.tan();
        let theta = (t.sin() / (t.cos() - 1.0 / 2f64.sqrt())).atan();
        let cosmu = mu.cos();
        let tannu = nu.tan();
        let cosphi = (1.0 - cosmu * cosmu * tannu * tannu * (1.0 - (1.0 / theta.cos()).atan().cos())).clamp(-1.0, 1.0);
        let (lam, mut lat) = match self.face {
            QscFace::Top => {
                let lam = match area {
                    QscArea::A0 => theta + HALFPI,
                    QscArea::A1 => {
                        if theta < 0.0 {
                            theta + PI
                        } else {
                            theta - PI
                        }
                    }
                    QscArea::A2 => theta - HALFPI,
                    _ => theta,
                };
                (lam, HALFPI - cosphi.acos())
            }
            QscFace::Bottom => {
                let lam = match area {
                    QscArea::A0 => -theta + HALFPI,
                    QscArea::A1 => -theta,
                    QscArea::A2 => -theta - HALFPI,
                    _ => {
                        if theta < 0.0 {
                            -theta - PI
                        } else {
                            -theta + PI
                        }
                    }
                };
                (lam, cosphi.acos() - HALFPI)
            }
            face => {
                // Through cartesian coordinates on the unit sphere.
                let mut q = cosphi;
                let mut u = q * q;
                let mut s = if u >= 1.0 { 0.0 } else { (1.0 - u).sqrt() * theta.sin() };
                u += s * s;
                let mut r = if u >= 1.0 { 0.0 } else { (1.0 - u).sqrt() };
                // Into the area, then into the cube face.
                match area {
                    QscArea::A1 => (r, s) = (-s, r),
                    QscArea::A2 => (r, s) = (-r, -s),
                    QscArea::A3 => (r, s) = (s, -r),
                    _ => {}
                }
                match face {
                    QscFace::Right => (q, r) = (-r, q),
                    QscFace::Back => (q, r) = (-q, -r),
                    QscFace::Left => (q, r) = (r, -q),
                    _ => {}
                }
                let lam = r.atan2(q);
                let lam = match face {
                    QscFace::Right => qsc_shift_longitude(lam, -HALFPI),
                    QscFace::Back => qsc_shift_longitude(lam, -PI),
                    QscFace::Left => qsc_shift_longitude(lam, HALFPI),
                    _ => lam,
                };
                (lam, (-s).acos() - HALFPI)
            }
        };
        // The shift from the sphere back to the ellipsoid.
        if self.es != 0.0 {
            let tanphi = lat.tan();
            let xa = self.b / (tanphi * tanphi + self.b * self.b).sqrt();
            let phi = ((1.0 - xa * xa).sqrt() / (self.b * xa)).atan();
            lat = if lat < 0.0 { -phi } else { phi };
        }
        Some((lam, lat))
    }
}

// ---------------------------------------------------------------------------------------------
// Icosahedral Snyder Equal Area, as PROJ's isea.cpp (Nathan Wagner's forward; the inverse by
// Jerome Jacovella-St-Louis after Franz-Benjamin Mocnik's geogrid): the sphere on the 20 faces of
// an icosahedron, unfolded in the plane. As in PROJ, the forward takes the geodetic latitude on
// the unit sphere, while the inverse uses the authalic radius on an ellipsoid; the inverse is
// defined only for PROJ's default grid (aperture 3, resolution 4, no azimuth) in its two
// orientations.

const ISEA_E_RAD: f64 = 0.91843818701052843323;
const ISEA_F_RAD: f64 = 0.18871053078356206978;
/// g: the spherical distance from a face centre to its vertices.
const ISEA_SDC2VOS: f64 = 0.6523581397843681859886783;
/// tan(g).
const ISEA_TANG: f64 = 0.76393202250021030358019673567;
const ISEA_TAN30: f64 = 0.57735026918962576450914878;
const ISEA_COT_THETA: f64 = 1.0 / ISEA_TAN30;
/// cos and sin of G (36 degrees), and cos(g).
const ISEA_COS_G: f64 = 0.80901699437494742410229341718281905886;
const ISEA_SIN_G: f64 = 0.587785252292473129168705954639072768597652;
const ISEA_COS_SDC2VOS: f64 = 0.7946544722917661229596057297879189448539;
const ISEA_SIN_G_COS_SDC2VOS: f64 = ISEA_SIN_G * ISEA_COS_SDC2VOS;
const ISEA_SQRT3: f64 = 1.73205080756887729352744634150587236694280525381038;
const ISEA_TABLE_G: f64 = ISEA_TANG * (ISEA_SQRT3 / 2.0);
const ISEA_TABLE_H: f64 = 0.25 * ISEA_TANG;
/// R' / R.
const ISEA_RPRIME_OVER_R: f64 = 0.9103832815095032;
const ISEA_STD_LAT: f64 = 1.01722196792335072101;
const ISEA_STD_LONG: f64 = 0.19634954084936207740;
const ISEA_DEG120: f64 = 2.09439510239319549229;
const ISEA_D2R: f64 = PI / 180.0;

/// The centres of the icosahedron's faces (latitude, longitude).
const ISEA_FACES: [(f64, f64); 20] = [
    (ISEA_E_RAD, ISEA_D2R * -144.0),
    (ISEA_E_RAD, ISEA_D2R * -72.0),
    (ISEA_E_RAD, ISEA_D2R * 0.0),
    (ISEA_E_RAD, ISEA_D2R * 72.0),
    (ISEA_E_RAD, ISEA_D2R * 144.0),
    (ISEA_F_RAD, ISEA_D2R * -144.0),
    (ISEA_F_RAD, ISEA_D2R * -72.0),
    (ISEA_F_RAD, ISEA_D2R * 0.0),
    (ISEA_F_RAD, ISEA_D2R * 72.0),
    (ISEA_F_RAD, ISEA_D2R * 144.0),
    (-ISEA_F_RAD, ISEA_D2R * -108.0),
    (-ISEA_F_RAD, ISEA_D2R * -36.0),
    (-ISEA_F_RAD, ISEA_D2R * 36.0),
    (-ISEA_F_RAD, ISEA_D2R * 108.0),
    (-ISEA_F_RAD, ISEA_D2R * 180.0),
    (-ISEA_E_RAD, ISEA_D2R * -108.0),
    (-ISEA_E_RAD, ISEA_D2R * -36.0),
    (-ISEA_E_RAD, ISEA_D2R * 36.0),
    (-ISEA_E_RAD, ISEA_D2R * 108.0),
    (-ISEA_E_RAD, ISEA_D2R * 180.0),
];

/// PROJ's safeArcSin and safeArcCos.
fn isea_asin(t: f64) -> f64 {
    const E: f64 = 1e-15;
    if t.abs() < E {
        0.0
    } else if (t - 1.0).abs() < E {
        HALFPI
    } else if (t + 1.0).abs() < E {
        -HALFPI
    } else {
        t.asin()
    }
}

fn isea_acos(t: f64) -> f64 {
    const E: f64 = 1e-15;
    if t.abs() < E {
        HALFPI
    } else if (t + 1.0).abs() < E {
        PI
    } else if (t - 1.0).abs() < E {
        0.0
    } else {
        t.acos()
    }
}

/// A longitude brought into [-pi, pi] as PROJ's isea does, strictly (`adjlon` keeps values within
/// pi + 1e-12 as they are).
fn isea_normalize_lon(lon: f64) -> f64 {
    let mut lon = lon % (2.0 * PI);
    while lon > PI {
        lon -= 2.0 * PI;
    }
    while lon < -PI {
        lon += 2.0 * PI;
    }
    lon
}

/// The planar frame of the inverse, for an icosahedron of radius R on the ellipsoid's authalic
/// sphere (PROJ's pj_isea_data::initialize).
#[derive(Debug)]
struct IseaPlane {
    /// The orientation reverted after the inverse: (latitude, longitude), and its latitude's
    /// cosine and sine.
    orientation: (f64, f64),
    cos_lat: f64,
    sin_lat: f64,
    r2: f64,
    rprime_2x: f64,
    rprime_tang: f64,
    rprime2_tan2g: f64,
    center_to_base: f64,
    tri_width: f64,
    y_offsets: [f64; 4],
    xo: f64,
    yo: f64,
    sx: f64,
    sy: f64,
}

#[derive(Debug)]
struct Isea {
    o_lat: f64,
    o_lon: f64,
    o_az: f64,
    face_sin_cos: [(f64, f64); 20],
    /// The semi-major axis in metres, by which the inverse scales its input.
    a: f64,
    plane: Option<IseaPlane>,
}

impl Isea {
    fn new(s: &Setup<'_>) -> Result<Self> {
        let (mut o_lat, mut o_lon) = (ISEA_STD_LAT, ISEA_STD_LONG);
        // A flag without a value is invalid, as in PROJ.
        let option = |key: &str| s.p.has(key).then(|| s.p.str(key).unwrap_or(""));
        match option("orient") {
            None | Some("isea") => {}
            Some("pole") => (o_lat, o_lon) = (HALFPI, 0.0),
            Some(_) => return Err(err("isea: invalid orient, only isea or pole are supported")),
        }
        // PROJ refuses `+azi=30x` (and reads a bare `+azi` as 0).
        if option("azi").is_some_and(|v| !v.is_empty()) && s.p.angle("azi").is_none() {
            return Err(err("isea: invalid azi, it should be an angle"));
        }
        let o_az = s.p.angle("azi").unwrap_or(0.0);
        // As in PROJ, `lon_0` both moves the central meridian and rotates the icosahedron.
        if let Some(lon) = s.p.angle("lon_0") {
            o_lon = lon;
        }
        if let Some(lat) = s.p.angle("lat_0") {
            o_lat = lat;
        }
        match option("mode") {
            None | Some("plane") => {}
            // Grid addresses, not coordinates.
            Some("di" | "dd" | "hex") => return Err(err("isea: only mode=plane is supported")),
            Some(_) => return Err(err("isea: invalid mode, only plane, di, dd or hex are supported")),
        }
        let resolution = if s.p.has("resolution") { proj_int(s.p, "isea", "resolution")? } else { 4 };
        let aperture = if s.p.has("aperture") { proj_int(s.p, "isea", "aperture")? } else { 3 };
        // PROJ compares the orientation exactly: a tolerance would define an inverse PROJ refuses.
        let orientation = if o_az != 0.0 || aperture != 3 || resolution != 4 {
            None
        } else if o_lat == ISEA_STD_LAT && o_lon == ISEA_STD_LONG {
            // The poles on edge midpoints, the equator symmetric.
            Some(((ISEA_E_RAD + ISEA_F_RAD) / 2.0, ISEA_D2R * -11.25))
        } else if o_lat == HALFPI && o_lon == 0.0 {
            Some((0.0, 0.0))
        } else {
            None
        };
        let a = s.ell.a;
        let plane = orientation.map(|orientation| {
            let (r2, rprime) = if s.ell.e > 0.0 {
                let (a2, b) = (a * a, a * s.ell.one_es.sqrt());
                let c2 = b * b;
                let e = s.ell.e;
                let surface = PI * (2.0 * a2 + c2 / e * ((1.0 + e) / (1.0 - e)).ln());
                let r2 = surface / (4.0 * PI);
                (r2, ISEA_RPRIME_OVER_R * r2.sqrt())
            } else {
                (a * a, ISEA_RPRIME_OVER_R * a)
            };
            let rprime_tang = rprime * ISEA_TANG;
            let center_to_base = rprime_tang / 2.0;
            let tri_width = rprime_tang * ISEA_SQRT3;
            IseaPlane {
                orientation,
                cos_lat: orientation.0.cos(),
                sin_lat: orientation.0.sin(),
                r2,
                rprime_2x: 2.0 * rprime,
                rprime_tang,
                rprime2_tan2g: rprime_tang * rprime_tang,
                center_to_base,
                tri_width,
                y_offsets: [-2.0 * center_to_base, -4.0 * center_to_base, -5.0 * center_to_base, -7.0 * center_to_base],
                xo: 2.5 * tri_width,
                yo: -1.5 * center_to_base,
                sx: 1.0 / tri_width,
                sy: 1.0 / (3.0 * center_to_base),
            }
        });
        Ok(Self { o_lat, o_lon, o_az, face_sin_cos: ISEA_FACES.map(|(lat, _)| (lat.sin(), lat.cos())), a, plane })
    }

    /// Snyder's forward on the face that holds the point (PROJ's isea_snyder_forward).
    fn snyder_forward(&self, lat: f64, lon: f64) -> Option<(usize, f64, f64)> {
        let (sin_lat, cos_lat) = lat.sin_cos();
        for (i, &(_, center_lon)) in ISEA_FACES.iter().enumerate() {
            let (sin_c, cos_c) = self.face_sin_cos[i];
            let dlon = lon - center_lon;
            let cos_lat_cos_lon = cos_lat * dlon.cos();
            let z = isea_acos(sin_c * sin_lat + cos_c * cos_lat_cos_lon);
            if z > ISEA_SDC2VOS + 0.000005 {
                continue;
            }
            let mut az = (cos_lat * dlon.sin()).atan2(cos_c * sin_lat - sin_c * cos_lat_cos_lon);
            az -= if (5..=9).contains(&i) || i == 15 || i == 16 {
                PI
            } else if i >= 17 {
                -PI
            } else {
                0.0
            };
            if az < 0.0 {
                az += 2.0 * PI;
            }
            let mut multiples = 0.0;
            while az < 0.0 {
                az += ISEA_DEG120;
                multiples -= 1.0;
            }
            while az > ISEA_DEG120 + f64::EPSILON {
                az -= ISEA_DEG120;
                multiples += 1.0;
            }
            let (sin_az, cos_az) = az.sin_cos();
            let q = ISEA_TANG.atan2(cos_az + sin_az * ISEA_COT_THETA);
            if z > q + 0.000005 {
                continue;
            }
            let h = (sin_az * ISEA_SIN_G_COS_SDC2VOS - cos_az * ISEA_COS_G).acos();
            let ag = az + ISEA_D2R * 36.0 + h - PI;
            let mut azprime = (2.0 * ag).atan2(ISEA_RPRIME_OVER_R * ISEA_RPRIME_OVER_R * ISEA_TANG * ISEA_TANG - 2.0 * ag * ISEA_COT_THETA);
            let dprime = ISEA_RPRIME_OVER_R * ISEA_TANG / (azprime.cos() + azprime.sin() * ISEA_COT_THETA);
            let f = dprime / (2.0 * ISEA_RPRIME_OVER_R * (q / 2.0).sin());
            let rho = 2.0 * ISEA_RPRIME_OVER_R * f * (z / 2.0).sin();
            azprime += ISEA_DEG120 * multiples;
            return Some((i, rho * azprime.sin(), rho * azprime.cos()));
        }
        // PROJ exits here: no face holds the point.
        None
    }

    /// The point in the frame of the icosahedron (PROJ's isea_ctran with Snyder's ctran).
    fn rotate(&self, lat: f64, lon: f64) -> (f64, f64) {
        let (alpha, beta) = (self.o_lat, self.o_lon + PI);
        let dlambda = lon - beta;
        let (sin_p, cos_p) = lat.sin_cos();
        let (sin_a, cos_a) = alpha.sin_cos();
        let (sin_dl, cos_dl) = dlambda.sin_cos();
        let sin_phip = sin_a * sin_p - cos_a * cos_p * cos_dl;
        let lp_b = (cos_p * sin_dl).atan2(sin_a * cos_p * cos_dl + cos_a * sin_p);
        let lambdap = isea_normalize_lon(lp_b + beta);
        let lon = isea_normalize_lon(lambdap - (-self.o_az + self.o_lon));
        (isea_asin(sin_phip), lon)
    }

    /// Unprojects a point of the plane, in metres from the frame's origin (PROJ's
    /// ISEAPlanarProjection::cartesianToGeo).
    fn cartesian_to_geo(&self, g: &IseaPlane, x: f64, y: f64) -> Option<(f64, f64)> {
        const EPSILON: f64 = 1e-11;
        let (sr, cr) = (-(ISEA_SQRT3 / 2.0), 0.5);
        let mut px = x;
        if px < 0.0 || (px < g.tri_width / 2.0 && y < 0.0 && y * cr < px * sr) {
            px += 5.0 * g.tri_width;
        }
        let yp = -(px * sr + y * cr);
        let mut x = (px * cr - y * sr + yp * (1.0 / ISEA_SQRT3)) * g.sx;
        let mut y_ = yp * g.sy;
        if x < 0.0 || (y_ > x && x < 5.0 - EPSILON) {
            x += EPSILON;
        } else if x > 5.0 || (y_ < x && x > EPSILON) {
            x -= EPSILON;
        }
        if y_ < 0.0 || (x > y_ && y_ < 6.0 - EPSILON) {
            y_ += EPSILON;
        } else if y_ > 6.0 || (x < y_ && y_ > EPSILON) {
            y_ -= EPSILON;
        }
        if !((0.0..=5.0).contains(&x) && (0.0..=6.0).contains(&y_)) {
            return None;
        }
        let ix = (x as i32).clamp(0, 4);
        let iy = (y_ as i32).clamp(0, 5);
        if iy != ix && iy != ix + 1 {
            return None;
        }
        let top = x - ix as f64 > y_ - iy as f64;
        let rhombus = ix + iy;
        // The upper row of rhombi holds faces 0 to 4 and 5 to 9, the lower 10 to 14 and 15 to 19.
        let column = (rhombus / 2) as usize;
        let face = match (rhombus % 2 == 0, top) {
            (true, true) => column,
            (true, false) => column + 5,
            (false, true) => column + 10,
            (false, false) => column + 15,
        };
        let (fy, fx) = (face / 5, face % 5);
        let rx = px - (2 * fx + fy / 2 + 1) as f64 * g.tri_width / 2.0;
        let ry = y - (g.y_offsets[fy] + 3.0 * g.center_to_base);
        let (lat, mut lon) = self.icosahedron_to_sphere(g, face, rx, ry)?;
        if lon < -PI - EPSILON {
            lon += 2.0 * PI;
        } else if lon > PI + EPSILON {
            lon -= 2.0 * PI;
        }
        Some((lat, lon))
    }

    /// PROJ's ISEAPlanarProjection::icosahedronToSphere.
    fn icosahedron_to_sphere(&self, g: &IseaPlane, face: usize, x: f64, y: f64) -> Option<(f64, f64)> {
        const PRECISION: f64 = ISEA_D2R * 1e-11;
        const AZ_MAX: f64 = ISEA_D2R * 120.0;
        const WEST_VERTEX_LON: f64 = ISEA_D2R * -144.0;
        let mut az = x.atan2(y);
        let rho = (x * x + y * y).sqrt();
        let mut az_adjustment = if face <= 4 || (10..=14).contains(&face) { 0.0 } else { ISEA_D2R * 180.0 };
        az += az_adjustment;
        while az < 0.0 {
            az_adjustment += AZ_MAX;
            az += AZ_MAX;
        }
        while az > AZ_MAX {
            az_adjustment -= AZ_MAX;
            az -= AZ_MAX;
        }
        let (sin_az, cos_az) = az.sin_cos();
        let cot_az = cos_az / sin_az;
        let area = g.rprime2_tan2g / (2.0 * (cot_az + ISEA_COT_THETA));
        let target = area / g.r2 - WEST_VERTEX_LON;
        let mut delta = 10.0 * PRECISION;
        let mut az_earth = az;
        // Newton's method, as in PROJ; the cap only guards against a loop that does not end.
        let mut iterations = 0;
        while delta.abs() > PRECISION {
            iterations += 1;
            if iterations > 100 {
                return None;
            }
            let (sin_e, cos_e) = az_earth.sin_cos();
            let h = (sin_e * ISEA_SIN_G_COS_SDC2VOS - cos_e * ISEA_COS_G).acos();
            let f = target - h - az_earth;
            let f2 = (cos_e * ISEA_SIN_G_COS_SDC2VOS + sin_e * ISEA_COS_G) / h.sin() - 1.0;
            delta = -f / f2;
            az_earth += delta;
        }
        let (sin_e, cos_e) = az_earth.sin_cos();
        let q = ISEA_TANG.atan2(cos_e + sin_e * ISEA_COT_THETA);
        let d = g.rprime_tang / (cos_az + sin_az * ISEA_COT_THETA);
        let f = d / (g.rprime_2x * (q / 2.0).sin());
        let z = 2.0 * (rho / (g.rprime_2x * f)).asin();
        az_earth -= az_adjustment;
        let (sin_lat0, cos_lat0) = self.face_sin_cos[face];
        let (sin_z, cos_z) = z.sin_cos();
        let cos_lat0_sin_z = cos_lat0 * sin_z;
        let lat = isea_asin(sin_lat0 * cos_z + cos_lat0_sin_z * az_earth.cos());
        let lon = ISEA_FACES[face].1 + (az_earth.sin() * cos_lat0_sin_z).atan2(cos_z - sin_lat0 * lat.sin());
        // PROJ's revertOrientation.
        let precision_per_definition = ISEA_D2R * 1e-5;
        let lon =
            if lat < ISEA_D2R * -90.0 + precision_per_definition || lat > ISEA_D2R * 90.0 - precision_per_definition { 0.0 } else { lon };
        let (o_lat, o_lon) = g.orientation;
        if o_lat != 0.0 || o_lon != 0.0 {
            let (sin_lat, cos_lat) = lat.sin_cos();
            let (sin_lon, cos_lon) = lon.sin_cos();
            let cos_lon_cos_lat = cos_lon * cos_lat;
            Some((
                (sin_lat * g.cos_lat - cos_lon_cos_lat * g.sin_lat).asin(),
                (sin_lon * cos_lat).atan2(cos_lon_cos_lat * g.cos_lat + sin_lat * g.sin_lat) - o_lon,
            ))
        } else {
            Some((lat, lon))
        }
    }
}

impl Proj for Isea {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (lat, lon) = self.rotate(phi, lam);
        let (tri, mut x, mut y) = self.snyder_forward(lat, lon)?;
        // Faces 5 to 9 and 15 to 19 point down: turn them half a turn, then move the face to its
        // place in the plane (PROJ's isea_tri_plane).
        if (tri / 5) % 2 == 1 {
            x = -x;
            y = -y;
        }
        let mut cx = ISEA_TABLE_G * ((tri % 5) as f64 - 2.0) * 2.0;
        if tri > 9 {
            cx += ISEA_TABLE_G;
        }
        let cy = match tri / 5 {
            0 => 5.0 * ISEA_TABLE_H,
            1 => ISEA_TABLE_H,
            2 => -ISEA_TABLE_H,
            _ => -5.0 * ISEA_TABLE_H,
        };
        Some((x + cx * ISEA_RPRIME_OVER_R, y + cy * ISEA_RPRIME_OVER_R))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let g = self.plane.as_ref()?;
        // The inverse's origin differs from the forward's.
        let (lat, lon) = self.cartesian_to_geo(g, x * self.a + g.xo, y * self.a + g.yo)?;
        Some((lon, lat))
    }
}

// ---------------------------------------------------------------------------------------------
// Winkel Tripel: the mean of Aitoff and the equirectangular projection on `lat_1`. PROJ's
// operator defaults `lat_1` to acos(2/π), but its CRS model (and so GDAL and pyproj) to 0, which
// terrio follows.

#[derive(Debug)]
struct Wintri {
    cosphi1: f64,
}

impl Wintri {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let cosphi1 = s.p.angle("lat_1").unwrap_or(0.0).cos();
        if cosphi1 == 0.0 {
            return Err(err("wintri: |lat_1| should be < 90°"));
        }
        Ok(Self { cosphi1 })
    }
}

impl Proj for Wintri {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (sin_half_lam, cos_half_lam) = (0.5 * lam).sin_cos();
        let (sin_phi, cos_phi) = phi.sin_cos();
        // Aitoff, with atan2 for its precision near the origin: alpha / sin(alpha) is `a`.
        let sin_alpha = (cos_phi * sin_half_lam).hypot(sin_phi);
        let a = if sin_alpha == 0.0 { 1.0 } else { sin_alpha.atan2(cos_phi * cos_half_lam) / sin_alpha };
        let x = 2.0 * cos_phi * sin_half_lam * a;
        let y = sin_phi * a;
        Some(((x + lam * self.cosphi1) * 0.5, (y + phi) * 0.5))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        // Newton's method on the Jacobian (Bildirici and Ipbuker, 2002), in rounds until the
        // solution projects back onto the point, as PROJ does.
        const MAXITER: u32 = 10;
        const MAXROUND: u32 = 20;
        const EPSILON: f64 = 1e-12;
        if x.abs() < EPSILON && y.abs() < EPSILON {
            return Some((0.0, 0.0));
        }
        let (mut lam, mut phi) = (x, y);
        let mut round = 0;
        loop {
            let mut iter = 0;
            loop {
                let (sl, cl) = (lam * 0.5).sin_cos();
                let (sp, cp) = phi.sin_cos();
                let d = cp * cl;
                let c = 1.0 - d * d;
                let denom = c.powf(1.5);
                if denom == 0.0 {
                    return None;
                }
                let d = d.acos() / denom;
                let f1 = 0.5 * (2.0 * d * c * cp * sl + lam * self.cosphi1) - x;
                let f2 = 0.5 * (d * c * sp + phi) - y;
                let f1p = 0.5 * (2.0 * (sl * cl * sp * cp / c - d * sp * sl));
                let f1l = 0.5 * (cp * cp * sl * sl / c + d * cp * cl * sp * sp + self.cosphi1);
                let f2p = 0.5 * (sp * sp * cl / c + d * sl * sl * cp + 1.0);
                let f2l = 0.5 * (0.5 * (sp * cp * sl / c - d * sp * cp * cp * sl * cl));
                let det = f1p * f2l - f2p * f1l;
                let dl = ((f2 * f1p - f1 * f2p) / det) % std::f64::consts::PI;
                let dp = (f1 * f2l - f2 * f1l) / det;
                phi -= dp;
                lam -= dl;
                let again = (dp.abs() > EPSILON || dl.abs() > EPSILON) && iter < MAXITER;
                iter += 1;
                if !again {
                    break;
                }
            }
            // Aitoff's symmetrical solution past a pole, folded back.
            if phi > HALFPI {
                phi -= 2.0 * (phi - HALFPI);
            }
            if phi < -HALFPI {
                phi -= 2.0 * (phi + HALFPI);
            }
            let (fx, fy) = self.fwd(lam, phi)?;
            let again = ((x - fx).abs() > EPSILON || (y - fy).abs() > EPSILON) && round < MAXROUND;
            round += 1;
            if !again {
                break;
            }
        }
        Some((lam, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Winkel I: the mean of the sinusoidal and the equirectangular projection on `lat_ts`.

#[derive(Debug)]
struct Wink1 {
    cosphi1: f64,
}

impl Proj for Wink1 {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        Some((0.5 * lam * (self.cosphi1 + phi.cos()), phi))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        Some((2.0 * x / (self.cosphi1 + y.cos()), y))
    }
}

// ---------------------------------------------------------------------------------------------
// Winkel II: the mean of Mollweide-like parallels and the equirectangular projection on `lat_1`.

#[derive(Debug)]
struct Wink2 {
    cosphi1: f64,
}

impl Proj for Wink2 {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        use std::f64::consts::{FRAC_2_PI, PI};
        let y = phi * FRAC_2_PI;
        let k = PI * phi.sin();
        let mut phi = phi * 1.8;
        let mut converged = false;
        for _ in 0..10 {
            let v = (phi + phi.sin() - k) / (1.0 + phi.cos());
            phi -= v;
            if v.abs() < 1e-7 {
                converged = true;
                break;
            }
        }
        let phi = if converged {
            phi * 0.5
        } else if phi < 0.0 {
            -HALFPI
        } else {
            HALFPI
        };
        Some((0.5 * lam * (phi.cos() + self.cosphi1), FORTPI * (phi.sin() + y)))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        // PROJ stops within 1e-10 of the radius, which leaves up to 1e-8° (0.6 mm); terrio goes on
        // to 1e-12, from the same start.
        generic_inverse_2d(|lam, phi| self.fwd(lam, phi), x, y, x, y, 1e-12)
    }
}

// ---------------------------------------------------------------------------------------------
// Gall stereographic.

#[derive(Debug)]
struct Gall;

const GALL_YF: f64 = 1.70710678118654752440;
const GALL_XF: f64 = std::f64::consts::FRAC_1_SQRT_2;
const GALL_RYF: f64 = 0.58578643762690495119;
const GALL_RXF: f64 = std::f64::consts::SQRT_2;

impl Proj for Gall {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        Some((GALL_XF * lam, GALL_YF * (0.5 * phi).tan()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        Some((GALL_RXF * x, 2.0 * (y * GALL_RYF).atan()))
    }
}

// ---------------------------------------------------------------------------------------------
// Gauss-Schreiber transverse Mercator (Gauss-Laborde Réunion): a spherical transverse Mercator of
// the ellipsoid's conformal sphere. Ellipsoidal, unlike the others here.

#[derive(Debug)]
struct Gstmerc {
    e: f64,
    c: f64,
    n1: f64,
    n2: f64,
    ys: f64,
}

impl Gstmerc {
    fn new(s: &mut Setup<'_>) -> Self {
        let (es, e, phi0) = (s.ell.es, s.ell.e, s.phi0);
        let sinphi0 = phi0.sin();
        let n1 = (1.0 + es * phi0.cos().powi(4) / (1.0 - es)).sqrt();
        let phic = (sinphi0 / n1).asin();
        let c = tsfn(-phic, -sinphi0 / n1, 0.0).ln() - n1 * tsfn(-phi0, -sinphi0, e).ln();
        // PROJ's n2 carries the semi-major axis, which terrio applies afterwards.
        let n2 = s.k0 * (1.0 - es).sqrt() / (1.0 - es * sinphi0 * sinphi0);
        Self { e, c, n1, n2, ys: -n2 * phic }
    }
}

impl Proj for Gstmerc {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let l = self.n1 * lam;
        let ls = self.c + self.n1 * tsfn(-phi, -phi.sin(), self.e).ln();
        let sin_ls1 = l.sin() / ls.cosh();
        let ls1 = tsfn(-sin_ls1.asin(), -sin_ls1, 0.0).ln();
        Some((self.n2 * ls1, self.ys + self.n2 * (ls.sinh() / l.cos()).atan()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (u, v) = (x / self.n2, (y - self.ys) / self.n2);
        let l = (u.sinh() / v.cos()).atan();
        let sin_c = v.sin() / u.cosh();
        let lc = tsfn(-sin_c.asin(), -sin_c, 0.0).ln();
        Some((l / self.n1, -phi2(((lc - self.c) / self.n1).exp(), self.e)))
    }
}

// ---------------------------------------------------------------------------------------------
// Foucaut sinusoidal, from the sinusoidal (`n` = 0, the default) to the equirectangular
// projection (`n` = 1).

#[derive(Debug)]
struct FoucS {
    n: f64,
    n1: f64,
}

impl FoucS {
    fn new(s: &mut Setup<'_>) -> Result<Self> {
        let n = s.p.f64("n").unwrap_or(0.0);
        if !(0.0..=1.0).contains(&n) {
            return Err(err("fouc_s: n should be in [0, 1]"));
        }
        Ok(Self { n, n1: 1.0 - n })
    }
}

impl Proj for FoucS {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let t = phi.cos();
        Some((lam * t / (self.n + self.n1 * t), self.n * phi + self.n1 * phi.sin()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let phi = if self.n != 0.0 {
            let mut phi = y;
            let mut converged = false;
            for _ in 0..10 {
                let v = (self.n * phi + self.n1 * phi.sin() - y) / (self.n + self.n1 * phi.cos());
                phi -= v;
                if v.abs() < 1e-7 {
                    converged = true;
                    break;
                }
            }
            if converged {
                phi
            } else if y < 0.0 {
                -HALFPI
            } else {
                HALFPI
            }
        } else {
            aasin_checked(y)?
        };
        let v = phi.cos();
        Some((x * (self.n + self.n1 * v) / v, phi))
    }
}

// ---------------------------------------------------------------------------------------------
// McBryde-Thomas flat-polar sinusoidal: PROJ's general sinusoidal series with m = 1/2 and
// n = 1 + π/4 (not `mbt_fps`, a different projection).

#[derive(Debug)]
struct Mbtfps {
    cx: f64,
    cy: f64,
}

const MBTFPS_M: f64 = 0.5;
const MBTFPS_N: f64 = 1.785398163397448309615660845;

impl Mbtfps {
    fn new() -> Self {
        let cy = ((MBTFPS_M + 1.0) / MBTFPS_N).sqrt();
        Self { cx: cy / (MBTFPS_M + 1.0), cy }
    }
}

impl Proj for Mbtfps {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let k = MBTFPS_N * phi.sin();
        let mut phi = phi;
        let mut converged = false;
        for _ in 0..8 {
            let v = (MBTFPS_M * phi + phi.sin() - k) / (MBTFPS_M + phi.cos());
            phi -= v;
            if v.abs() < 1e-7 {
                converged = true;
                break;
            }
        }
        if !converged {
            return None;
        }
        Some((self.cx * lam * (MBTFPS_M + phi.cos()), self.cy * phi))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let y = y / self.cy;
        let phi = aasin_checked((MBTFPS_M * y + y.sin()) / MBTFPS_N)?;
        Some((x / (self.cx * (MBTFPS_M + y.cos())), phi))
    }
}

// ---------------------------------------------------------------------------------------------
// Putniņš P4′ and Werenskiold I (its scaled form), with PROJ's constants as written: they are
// truncated, and the exact ones differ from PROJ's results by millimetres to a centimetre.

#[derive(Debug)]
struct Putp4p {
    cx: f64,
    cy: f64,
}

impl Proj for Putp4p {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let phi = aasin_checked(0.883883476 * phi.sin())?;
        let x = self.cx * lam * phi.cos();
        let phi = phi * 0.333333333333333;
        Some((x / phi.cos(), self.cy * phi.sin()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let phi = aasin_checked(y / self.cy)?;
        let lam = x * phi.cos() / self.cx;
        let phi = phi * 3.0;
        Some((lam / phi.cos(), aasin_checked(1.13137085 * phi.sin())?))
    }
}

// ---------------------------------------------------------------------------------------------
// Two of PROJ's simple conics (`sconics.cpp`) on `lat_1` and `lat_2`, ported literally: Murdoch
// II and Tissot (PROJ's Tissot is not the textbook form).

#[derive(Debug, Clone, Copy, PartialEq)]
enum SconicKind {
    Murd2,
    Tissot,
}

#[derive(Debug)]
struct Sconic {
    kind: SconicKind,
    n: f64,
    rho_c: f64,
    rho_0: f64,
    sig: f64,
}

impl Sconic {
    fn new(s: &mut Setup<'_>, kind: SconicKind) -> Result<Self> {
        let p1 = s.p.angle("lat_1").ok_or_else(|| err("lat_1 should be specified"))?;
        let p2 = s.p.angle("lat_2").ok_or_else(|| err("lat_2 should be specified"))?;
        let del = 0.5 * (p2 - p1);
        let sig = 0.5 * (p2 + p1);
        if del.abs() < 1e-10 || sig.abs() < 1e-10 {
            return Err(err("|lat_1 - lat_2| and |lat_1 + lat_2| should be > 0"));
        }
        let (n, rho_c, rho_0) = match kind {
            SconicKind::Tissot => {
                let n = sig.sin();
                let cs = del.cos();
                let rho_c = n / cs + cs / n;
                (n, rho_c, ((rho_c - 2.0 * s.phi0.sin()) / n).sqrt())
            }
            SconicKind::Murd2 => {
                let cs = del.cos().sqrt();
                let rho_c = cs / sig.tan();
                (sig.sin() * cs, rho_c, rho_c + (sig - s.phi0).tan())
            }
        };
        Ok(Self { kind, n, rho_c, rho_0, sig })
    }
}

impl Proj for Sconic {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let rho = match self.kind {
            SconicKind::Murd2 => self.rho_c + (self.sig - phi).tan(),
            SconicKind::Tissot => self.rho_c - phi,
        };
        let lam = lam * self.n;
        Some((rho * lam.sin(), self.rho_0 - rho * lam.cos()))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (mut x, mut y) = (x, self.rho_0 - y);
        let mut rho = x.hypot(y);
        if self.n < 0.0 {
            rho = -rho;
            x = -x;
            y = -y;
        }
        let lam = x.atan2(y) / self.n;
        let phi = match self.kind {
            SconicKind::Murd2 => self.sig - (rho - self.rho_c).atan(),
            SconicKind::Tissot => self.rho_c - rho,
        };
        Some((lam, phi))
    }
}

#[cfg(test)]
mod benchmark_regressions {
    use super::*;

    #[test]
    fn gauss_scale_is_finite_at_exact_poles() {
        for e in [0.0, Ell::wgs84().e, 0.5] {
            for phi0 in [-HALFPI, HALFPI] {
                let (gauss, chi, radius) = Gauss::new(e, phi0).unwrap();
                // At either pole c = 1 and the tangent ratio is 1. This is
                // the analytic limit, including the former south-pole branch.
                let expected = 1.0 / srat(e * phi0.sin(), 0.5 * e);
                assert!(gauss.k.is_finite() && (gauss.k - expected).abs() < 1e-14);
                assert!((chi - phi0).abs() < 1e-15 && radius.is_finite());
            }
        }
    }

    #[test]
    fn mollweide_and_wagner_refuse_non_finite_inputs() {
        for name in ["moll", "wag4", "wag5"] {
            let projection = Moll::new(name);
            for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert!(projection.fwd(0.0, value).is_none(), "{name}: latitude {value}");
                assert!(projection.fwd(value, 0.0).is_none(), "{name}: longitude {value}");
                assert!(projection.inv(value, 0.0).is_none(), "{name}: x {value}");
                assert!(projection.inv(0.0, value).is_none(), "{name}: y {value}");
            }
        }
    }
}
