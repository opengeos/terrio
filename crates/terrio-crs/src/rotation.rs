//! Rotated-pole grids: PROJ's `+proj=ob_tran +o_proj=longlat` with `+o_lat_p`, `+o_lon_p` and
//! `+lon_0`, the form CF's `rotated_latitude_longitude` and GRIB's rotated grids take.
//!
//! As in PROJ, the rotation applies to the longitude and latitude as they are, with no datum or
//! geodetic-to-spherical conversion: on a sphere and on WGS 84, a grid's coordinates are the same.

use super::math::*;
use super::params::Params;
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

/// The `ob_tran` parameters terrio applies: `o_proj` and the new pole. The others (`o_lon_c`,
/// `o_lat_c`, `o_alpha`, `o_lon_1`...) define the pole in other ways.
const KNOWN: [&str; 3] = ["o_proj", "o_lat_p", "o_lon_p"];

impl Rotation {
    /// The rotation of an `ob_tran` PROJ string. Fails on the forms terrio does not apply: an
    /// `o_proj` other than `longlat`, or a pole not given by `o_lat_p` and `o_lon_p`.
    pub fn from_params(p: &Params) -> Result<Self> {
        let o_proj = p.str("o_proj").ok_or_else(|| err("+proj=ob_tran without +o_proj"))?;
        if !matches!(o_proj, "longlat" | "latlong" | "lonlat" | "latlon") {
            return Err(err(&format!("+proj=ob_tran with +o_proj={o_proj} (only +o_proj=longlat is)")));
        }
        if let Some(other) = p.keys().find(|k| k.starts_with("o_") && !KNOWN.contains(k)) {
            return Err(err(&format!("+proj=ob_tran with +{other} (only +o_lat_p and +o_lon_p are)")));
        }
        let phip = p.angle("o_lat_p").ok_or_else(|| err("+proj=ob_tran without +o_lat_p"))?;
        let (sphip, cphip) = phip.sin_cos();
        Ok(Self { sphip, cphip, lamp: p.angle("o_lon_p").unwrap_or(0.0) })
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
        for s in ["+proj=ob_tran +o_proj=merc +o_lat_p=40", "+proj=ob_tran +o_proj=longlat +o_lon_c=10 +o_lat_c=40 +o_alpha=5"] {
            assert!(Rotation::from_params(&Params::parse(s)).is_err(), "{s}");
        }
    }
}
