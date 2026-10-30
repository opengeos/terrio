// SPDX-FileCopyrightText: PROJ contributors
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   PROJ 9.8.1:
//     src/projections/ob_tran.cpp

//! PROJ's general oblique transformation, `+proj=ob_tran`: rotated-pole grids
//! (`+o_proj=longlat`, the form CF's `rotated_latitude_longitude` and GRIB's rotated grids take)
//! and other projections on a rotated sphere or ellipsoid.
//!
//! As in PROJ, the rotation applies to the longitude and latitude as they are, with no datum or
//! geodetic-to-spherical conversion: on a sphere and on WGS 84, a grid's coordinates are the same.

use super::math::*;
use super::params::Params;
use super::projections::Proj;
use crate::{Error, Result};

/// The rotation of a rotated-pole grid. Longitudes are relative to `+lon_0`, which the caller
/// applies, as for a projection's central meridian.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rotation {
    /// The sine and cosine of the grid's pole latitude (`+o_lat_p`).
    sphip: f64,
    cphip: f64,
    /// The grid longitude of the geographic pole (`+o_lon_p`), in radians.
    lamp: f64,
}

/// The `ob_tran` parameters terrio applies: `o_proj` and the new pole, given as a pole
/// (`o_lat_p`, `o_lon_p`), as a centre and azimuth (`o_lon_c`, `o_lat_c`, `o_alpha`) or as two
/// points on the new equator (`o_lon_1`, `o_lat_1`, `o_lon_2`, `o_lat_2`).
const KNOWN: [&str; 10] = ["o_proj", "o_lat_p", "o_lon_p", "o_alpha", "o_lon_c", "o_lat_c", "o_lon_1", "o_lat_1", "o_lon_2", "o_lat_2"];

/// PROJ's tolerance on the pole latitude (a transverse rotation below it) and on the other forms'
/// latitudes.
const TOL: f64 = 1e-10;

impl Rotation {
    /// The rotation of an `ob_tran` PROJ string, as PROJ's `ob_tran` setup computes it.
    pub fn from_params(p: &Params) -> Result<Self> {
        p.str("o_proj").ok_or_else(|| err("+proj=ob_tran without +o_proj"))?;
        if let Some(other) = p.keys().find(|k| k.starts_with("o_") && !KNOWN.contains(k)) {
            return Err(err(&format!("+proj=ob_tran with +{other}")));
        }
        // An absent angle is 0; one that does not read as an angle is refused (PROJ reads it as 0).
        let angle = |k: &str| match p.angle(k) {
            Some(v) => Ok(v),
            None if !p.has(k) => Ok(0.0),
            None => Err(invalid(&format!("+{k} is not an angle"))),
        };
        let (lamp, phip) = if p.has("o_alpha") {
            let (lamc, phic, alpha) = (angle("o_lon_c")?, angle("o_lat_c")?, angle("o_alpha")?);
            if (phic.abs() - HALFPI).abs() <= TOL {
                return Err(invalid("|o_lat_c| should be < 90 degrees"));
            }
            (lamc + (-alpha.cos()).atan2(-alpha.sin() * phic.sin()), aasin(phic.cos() * alpha.sin()))
        } else if p.has("o_lat_p") {
            (angle("o_lon_p")?, angle("o_lat_p")?)
        } else if !["o_lon_1", "o_lat_1", "o_lon_2", "o_lat_2"].iter().any(|k| p.has(k)) {
            return Err(invalid("no pole: give +o_lat_p, +o_lon_c, +o_lat_c and +o_alpha, or +o_lon_1, +o_lat_1, +o_lon_2 and +o_lat_2"));
        } else {
            let (lam1, phi1, lam2, phi2) = (angle("o_lon_1")?, angle("o_lat_1")?, angle("o_lon_2")?, angle("o_lat_2")?);
            if phi1.abs() > HALFPI - TOL || phi2.abs() > HALFPI - TOL {
                return Err(invalid("|o_lat_1| and |o_lat_2| should be < 90 degrees"));
            }
            if (phi1 - phi2).abs() < TOL {
                return Err(invalid("o_lat_1 should be different from o_lat_2"));
            }
            if phi1.abs() < TOL {
                return Err(invalid("o_lat_1 should be different from zero"));
            }
            let lamp = (phi1.cos() * phi2.sin() * lam1.cos() - phi1.sin() * phi2.cos() * lam2.cos())
                .atan2(phi1.sin() * phi2.cos() * lam2.sin() - phi1.cos() * phi2.sin() * lam1.sin());
            (lamp, (-(lamp - lam1).cos() / phi1.tan()).atan())
        };
        if !(phip.is_finite() && lamp.is_finite()) {
            return Err(invalid("the pole must be finite"));
        }
        // PROJ's transverse case, its own formulas, which are the oblique ones at a pole latitude
        // of 0.
        let (sphip, cphip) = if phip.abs() > TOL { phip.sin_cos() } else { (0.0, 1.0) };
        Ok(Self { sphip, cphip, lamp })
    }

    /// The grid's pole latitude (`+o_lat_p`) and the grid longitude of the geographic pole
    /// (`+o_lon_p`), in radians.
    pub fn pole(&self) -> (f64, f64) {
        (self.sphip.atan2(self.cphip), self.lamp)
    }

    /// The grid coordinates (radians) of a longitude, relative to `+lon_0`, and a latitude
    /// (PROJ's `o_forward`: Snyder's formulas 5-7 and 5-8b).
    pub fn fwd(&self, lam: f64, phi: f64) -> (f64, f64) {
        let (sinphi, cosphi) = phi.sin_cos();
        let (sinlam, coslam) = lam.sin_cos();
        let lam = adjlon((cosphi * sinlam).atan2(self.sphip * cosphi * coslam + self.cphip * sinphi) + self.lamp);
        (lam, aasin(self.sphip * sinphi - self.cphip * cosphi * coslam))
    }

    /// The longitude, relative to `+lon_0`, and latitude (radians) of grid coordinates (PROJ's
    /// `o_inverse`).
    pub fn inv(&self, lam: f64, phi: f64) -> (f64, f64) {
        let (sinphi, cosphi) = phi.sin_cos();
        let (sinlam, coslam) = (lam - self.lamp).sin_cos();
        let phi = aasin(self.sphip * sinphi + self.cphip * cosphi * coslam);
        ((cosphi * sinlam).atan2(self.sphip * cosphi * coslam - self.cphip * sinphi), phi)
    }
}

fn err(msg: &str) -> Error {
    Error::Crs(format!("unsupported projection: {msg}"))
}

fn invalid(msg: &str) -> Error {
    Error::Crs(format!("invalid +proj=ob_tran: {msg}"))
}

/// PROJ's `ob_tran` with a projection other than `longlat` (`+o_proj`): the rotation, then that
/// projection's own formulas, on the CRS's figure. The projection's own central meridian and
/// false easting and northing do not apply, as in PROJ.
#[derive(Debug)]
pub(crate) struct ObTran {
    pub rot: Rotation,
    pub link: Box<dyn Proj>,
}

impl Proj for ObTran {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (lam, phi) = self.rot.fwd(lam, phi);
        self.link.fwd(lam, phi)
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (lam, phi) = self.link.inv(x, y)?;
        Some(self.rot.inv(lam, phi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_undoes_forward() {
        let r = Rotation::from_params(&Params::parse("+proj=ob_tran +o_proj=longlat +o_lat_p=39.25 +o_lon_p=10")).unwrap();
        for (lam, phi) in [(0.3, 0.7), (-2.5, -1.2), (3.0, 0.0)] {
            let (x, y) = r.fwd(lam, phi);
            let (l, p) = r.inv(x, y);
            assert!((adjlon(l - lam)).abs() < 1e-12 && (p - phi).abs() < 1e-12);
        }
    }

    #[test]
    fn refuses_other_forms() {
        for s in [
            "+proj=ob_tran +o_lat_p=40",
            "+proj=ob_tran +o_proj=longlat +o_lat_p=40 +o_foo=1",
            "+proj=ob_tran +o_proj=longlat +o_lon_1=10",
            "+proj=ob_tran +o_proj=moll +o_lat_p=nan",
            "+proj=ob_tran +o_proj=moll +o_lat_p=45 +o_lon_p=inf",
        ] {
            assert!(Rotation::from_params(&Params::parse(s)).is_err(), "{s}");
        }
    }
}
