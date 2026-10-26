//! Spherical near-sided and tilted perspective, ported from PROJ 9.8.1
//! src/projections/nsper.cpp. See NOTICE for the PROJ license and attribution.

use super::{Proj, Setup, err};
use crate::Result;
use crate::math::HALFPI;

const EPS: f64 = 1e-10;

#[derive(Debug)]
enum Aspect {
    North,
    South,
    Equatorial,
    Oblique,
}

#[derive(Debug)]
pub(super) struct Perspective {
    aspect: Aspect,
    phi0: f64,
    sin0: f64,
    cos0: f64,
    pn1: f64,
    p: f64,
    rp: f64,
    pfact: f64,
    h: f64,
    tilt: Option<(f64, f64, f64, f64)>,
}

impl Perspective {
    pub(super) fn new(s: &Setup<'_>, tilted: bool) -> Result<Self> {
        let height = s.p.f64("h").ok_or_else(|| err("perspective needs a positive +h"))?;
        let pn1 = height / s.ell.a;
        if !pn1.is_finite() || pn1 <= 0.0 || pn1 > 1e10 {
            return Err(err("invalid perspective height"));
        }
        if ["lat_0", "lon_0"].iter().any(|key| s.p.has(key) && s.p.angle(key).is_none()) || !s.lam0.is_finite() {
            return Err(err("invalid perspective origin"));
        }
        if !s.phi0.is_finite() || s.phi0.abs() > HALFPI {
            return Err(err("invalid perspective latitude of origin"));
        }
        let tilt = if tilted {
            let angle = |key| {
                let value = if s.p.has(key) { s.p.angle(key) } else { Some(0.0) };
                value.filter(|v| v.is_finite()).ok_or_else(|| err("invalid perspective tilt or azimuth"))
            };
            let (sw, cw) = angle("tilt")?.sin_cos();
            let (sg, cg) = angle("azi")?.sin_cos();
            Some((sw, cw, sg, cg))
        } else {
            None
        };
        let aspect = if (s.phi0.abs() - HALFPI).abs() < EPS {
            if s.phi0 < 0.0 { Aspect::South } else { Aspect::North }
        } else if s.phi0.abs() < EPS {
            Aspect::Equatorial
        } else {
            Aspect::Oblique
        };
        let p = 1.0 + pn1;
        let (sin0, cos0) = s.phi0.sin_cos();
        Ok(Self { aspect, phi0: s.phi0, sin0, cos0, pn1, p, rp: 1.0 / p, pfact: (p + 1.0) * (1.0 / pn1), h: 1.0 / pn1, tilt })
    }
}

impl Proj for Perspective {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (sp, cp) = phi.sin_cos();
        let cl = lam.cos();
        let z = match self.aspect {
            Aspect::Oblique => self.sin0 * sp + self.cos0 * cp * cl,
            Aspect::Equatorial => cp * cl,
            Aspect::North => sp,
            Aspect::South => -sp,
        };
        if z < self.rp {
            return None;
        }
        let k = self.pn1 / (self.p - z);
        let mut x = k * cp * lam.sin();
        let mut y = k * match self.aspect {
            Aspect::Oblique => self.cos0 * sp - self.sin0 * cp * cl,
            Aspect::Equatorial => sp,
            Aspect::North => -cp * cl,
            Aspect::South => cp * cl,
        };
        if let Some((sw, cw, sg, cg)) = self.tilt {
            let yt = y * cg + x * sg;
            let ba = 1.0 / (yt * sw * self.h + cw);
            x = (x * cg - y * sg) * cw * ba;
            y = yt * ba;
        }
        (x.is_finite() && y.is_finite()).then_some((x, y))
    }

    fn inv(&self, mut x: f64, mut y: f64) -> Option<(f64, f64)> {
        if let Some((sw, cw, sg, cg)) = self.tilt {
            let yt = 1.0 / (self.pn1 - y * sw);
            let bm = self.pn1 * x * yt;
            let bq = self.pn1 * y * cw * yt;
            x = bm * cg + bq * sg;
            y = bq * cg - bm * sg;
        }
        let rh = x.hypot(y);
        if rh.abs() <= EPS {
            return Some((0.0, self.phi0));
        }
        let disc = 1.0 - rh * rh * self.pfact;
        if disc < 0.0 {
            return None;
        }
        let sz = (self.p - disc.sqrt()) / (self.pn1 / rh + rh / self.pn1);
        let cz = (1.0 - sz * sz).sqrt();
        let phi = match self.aspect {
            Aspect::Oblique => {
                let phi = (cz * self.sin0 + y * sz * self.cos0 / rh).asin();
                y = (cz - self.sin0 * phi.sin()) * rh;
                x *= sz * self.cos0;
                phi
            }
            Aspect::Equatorial => {
                let phi = (y * sz / rh).asin();
                y = cz * rh;
                x *= sz;
                phi
            }
            Aspect::North => {
                y = -y;
                cz.asin()
            }
            Aspect::South => -cz.asin(),
        };
        let lam = x.atan2(y);
        (lam.is_finite() && phi.is_finite()).then_some((lam, phi))
    }
}
