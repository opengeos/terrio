//! Datum transformations: the geodetic steps of PROJ pipelines (geocentric conversion, Helmert
//! with or without rates of change, Molodensky-Badekas, geographic offsets and the grid-based
//! steps `hgridshift`, `vgridshift`, `gridshift`, `xyzgridshift` and `deformation`), run as PROJ
//! runs them.

use super::grid::{self, GridSet, GridShiftKinds};
use super::math::Ell;
use super::params::Params;
use std::sync::{Arc, OnceLock};

/// The grids of a step, by their `grids=` list, loaded when first used.
#[derive(Debug)]
pub(crate) struct Grids {
    spec: String,
    sets: OnceLock<Vec<Arc<GridSet>>>,
    kinds: OnceLock<Option<GridShiftKinds>>,
}

impl Clone for Grids {
    fn clone(&self) -> Self {
        Grids::new(&self.spec)
    }
}

impl PartialEq for Grids {
    fn eq(&self, other: &Self) -> bool {
        self.spec == other.spec
    }
}

impl std::fmt::Debug for GridSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GridSet")
    }
}

impl Grids {
    fn new(spec: &str) -> Self {
        Self { spec: spec.to_string(), sets: OnceLock::new(), kinds: OnceLock::new() }
    }

    /// The grids, loaded with `load` (`None` when a required one is missing, which is not
    /// remembered so that a grid installed later is found).
    fn get(&self, load: fn(&str) -> Option<Vec<Arc<GridSet>>>) -> Option<&[Arc<GridSet>]> {
        if let Some(s) = self.sets.get() {
            return Some(s);
        }
        let s = load(&self.spec)?;
        Some(self.sets.get_or_init(|| s))
    }
}

/// The rates of change of a time-dependent Helmert transformation (PROJ's 15-parameter
/// Helmert), with the parameters at the reference epoch.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rates {
    /// Translations (m), rotations (arc-seconds) and scale (ppm) at the epoch.
    t: [f64; 3],
    rot: [f64; 3],
    s: f64,
    /// Their rates per year.
    dt: [f64; 3],
    drot: [f64; 3],
    ds: f64,
    epoch: f64,
    position_vector: bool,
    exact: bool,
}

/// One step of a datum transformation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    /// Geodetic to geocentric coordinates on an ellipsoid (inverse: geocentric to geodetic).
    Cart { ell: Ell, inv: bool },
    /// A Helmert transformation of geocentric coordinates, about a pivot point for
    /// Molodensky-Badekas, and with rates of change for a time-dependent one.
    Helmert { t: [f64; 3], r: [[f64; 3]; 3], scale: f64, pivot: [f64; 3], inv: bool, rates: Option<Box<Rates>> },
    /// Offsets in radians added to the longitude and latitude, and in metres to the height.
    GeogOffset { dlon: f64, dlat: f64, dh: f64, inv: bool },
    /// EPSG's Vertical Offset and Slope (PROJ's `vertoffset`): heights plus an offset varying
    /// linearly with the distance from an origin, along the meridian and the parallel.
    VertOffset { lat0: f64, lon0: f64, dh: f64, slope_lat: f64, slope_lon: f64, rho0: f64, nu0: f64, inv: bool },
    /// Heights to depths (PROJ's `axisswap order=1,2,-3`), its own inverse.
    ZFlip,
    /// Horizontal shift grids (NTv2, GeoTIFF), applied between `t_epoch` and `t_final` when set.
    HGridShift { grids: Grids, inv: bool, epoch: f64, final_epoch: f64 },
    /// Vertical shift grids (geoid models): heights plus `multiplier` times the grid value.
    VGridShift { grids: Grids, multiplier: f64, inv: bool, epoch: f64, final_epoch: f64 },
    /// PROJ's generic `gridshift` (NADCON5 and other grids with their own types).
    GridShift { grids: Grids, interpolation: Option<String>, no_z: bool, inv: bool },
    /// Geocentric translations interpolated in a grid (`xyzgridshift`).
    XyzGridShift { grids: Grids, ell: Ell, input_ref: bool, multiplier: f64, inv: bool },
    /// A velocity grid applied over a time span (`deformation`).
    Deformation { grids: Grids, ell: Ell, dt: Option<f64>, epoch: f64, inv: bool },
}

/// A datum transformation to WGS 84, with the area where it applies and what PROJ ranks it by.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Op {
    /// West, south, east and north in degrees. West may exceed east across the antimeridian.
    pub bounds: [f64; 4],
    /// The accuracy in metres, negative when unknown.
    pub accuracy: f64,
    /// The accuracy PROJ gives it when it concatenates it with other operations (in chains,
    /// and with the projections of projected CRSs): the sum of its steps', which differs for a
    /// concatenation registered with its own.
    pub chain_accuracy: f64,
    /// Likewise its area: the intersection of its steps'.
    pub chain_bounds: [f64; 4],
    /// Whether its area is offshore (PROJ only uses those when no other fits).
    pub offshore: bool,
    /// Whether its area is named "unknown".
    pub unknown_area: bool,
    /// Whether this is PROJ's "ballpark" transformation (no datum shift, as a last resort).
    pub ballpark: bool,
    /// Whether it chains several transformations through intermediate datums.
    pub concatenated: bool,
    /// Whether PROJ's database registers it between geographic 3D CRSs, which PROJ does not
    /// chain with transformations between 2D ones.
    pub between_3d: bool,
    /// Whether PROJ's database registers it between geocentric CRSs, which PROJ chains otherwise
    /// than those between geographic CRSs.
    pub geocentric: bool,
    /// Whether PROJ prefers it among those of equal accuracy (NADCON5 NAD83 to NAD83(HARN),
    /// GDA94 to WGS 84 (1), GDA2020 to WGS 84 (2)).
    pub priority: bool,
    /// Whether the pipeline keeps heights between 2D CRSs (PROJ's `push v_3` and `pop v_3`).
    pub keeps_height: bool,
    pub steps: Vec<Step>,
    /// PROJ's name for it, the names of its steps joined by ` + ` (empty for ballparks), by which
    /// PROJ breaks the last ties when it sorts the chains it makes.
    pub name: String,
}

/// Whether PROJ prefers an operation of this name among those of equal accuracy: one that uses
/// the NADCON5 NAD83 to NAD83(HARN) transformations, GDA94 to WGS 84 (1) or GDA2020 to WGS 84
/// (2) (PROJ's `isPriorityOp`, on the operation's whole name).
pub(crate) fn is_priority(name: &str) -> bool {
    ["NAD83 to NAD83(HARN) (47)", "NAD83 to NAD83(HARN) (48)", "NAD83 to NAD83(HARN) (49)", "NAD83 to NAD83(HARN) (50)"]
        .iter()
        .chain(&["GDA94 to WGS 84 (1)", "GDA2020 to WGS 84 (2)"])
        .any(|p| name.contains(p))
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

/// The rotation matrix of a Helmert transformation (rotations in arc-seconds), as PROJ builds
/// it: approximate unless `exact`, transposed for the position vector convention.
fn rotation(rot: [f64; 3], position_vector: bool, exact: bool) -> [[f64; 3]; 3] {
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
    r
}

/// A Helmert step from its seven parameters (metres, arc-seconds and ppm).
pub(crate) fn helmert(t: [f64; 3], rot: [f64; 3], s: f64, position_vector: bool, exact: bool, pivot: [f64; 3]) -> Step {
    Step::Helmert { t, r: rotation(rot, position_vector, exact), scale: 1.0 + s * 1e-6, pivot, inv: false, rates: None }
}

/// The steps of a `+towgs84=` shift (position vector convention) from an ellipsoid to WGS 84.
pub(crate) fn towgs84_steps(ell: Ell, p: &[f64]) -> Vec<Step> {
    let wgs84 = Ell::wgs84();
    if p.iter().all(|v| *v == 0.0) && ell.a == wgs84.a && ell.es == wgs84.es {
        return Vec::new();
    }

    let g = |i: usize| p.get(i).copied().unwrap_or(0.0);
    vec![
        Step::Cart { ell, inv: false },
        helmert([g(0), g(1), g(2)], [g(3), g(4), g(5)], g(6), true, false, [0.0; 3]),
        Step::Cart { ell: Ell::wgs84(), inv: true },
    ]
}

/// The steps of `+nadgrids=` (horizontal grids to WGS 84).
pub(crate) fn nadgrids_steps(spec: &str) -> Vec<Step> {
    vec![Step::HGridShift { grids: Grids::new(spec), inv: false, epoch: 0.0, final_epoch: 0.0 }]
}

/// A `vgridshift` step: heights plus `multiplier` times the grid value.
pub(crate) fn vgridshift_step(spec: &str, multiplier: f64) -> Step {
    Step::VGridShift { grids: Grids::new(spec), multiplier, inv: false, epoch: 0.0, final_epoch: 0.0 }
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
        let epochs = || (p.f64("t_epoch").unwrap_or(0.0), p.f64("t_final").unwrap_or(0.0));
        let step = match name {
            "cart" => Step::Cart { ell: ellipsoid(&p)?, inv },
            "helmert" | "molobadekas" => {
                if p.has("theta") || p.has("transpose") {
                    return None;
                }
                let g = |k: &str| p.f64(k).unwrap_or(0.0);
                let pv = p.str("convention") != Some("coordinate_frame");
                let pivot = [g("px"), g("py"), g("pz")];
                let (t, rot, sc) = ([g("x"), g("y"), g("z")], [g("rx"), g("ry"), g("rz")], g("s"));
                let mut h = helmert(t, rot, sc, pv, p.has("exact"), pivot);
                let timed = ["dx", "dy", "dz", "drx", "dry", "drz", "ds"].iter().any(|k| g(k) != 0.0);
                if let Step::Helmert { inv: i, rates, .. } = &mut h {
                    *i = inv;
                    if timed {
                        *rates = Some(Box::new(Rates {
                            t,
                            rot,
                            s: sc,
                            dt: [g("dx"), g("dy"), g("dz")],
                            drot: [g("drx"), g("dry"), g("drz")],
                            ds: g("ds"),
                            epoch: g("t_epoch"),
                            position_vector: pv,
                            exact: p.has("exact"),
                        }));
                    }
                }
                h
            }
            "geogoffset" => {
                let g = |k: &str| p.f64(k).unwrap_or(0.0) * SEC_TO_RAD;
                Step::GeogOffset { dlon: g("dlon"), dlat: g("dlat"), dh: p.f64("dh").unwrap_or(0.0), inv }
            }
            "axisswap" if p.str("order").is_some_and(|o| o.replace(' ', "") == "1,2,-3") => Step::ZFlip,
            "vertoffset" => {
                // As PROJ sets it up: slopes in arc-seconds, the radii of curvature at the origin
                // on the step's ellipsoid (GRS80 by default).
                let ell = ellipsoid(&p).unwrap_or_else(|| Ell::from_a_rf(6_378_137.0, 298.257_222_101));
                let lat0 = p.f64("lat_0").unwrap_or(0.0).to_radians();
                let s = lat0.sin();
                let w = 1.0 - ell.es * s * s;
                Step::VertOffset {
                    lat0,
                    lon0: p.f64("lon_0").unwrap_or(0.0).to_radians(),
                    dh: p.f64("dh").unwrap_or(0.0),
                    slope_lat: p.f64("slope_lat").unwrap_or(0.0) * SEC_TO_RAD,
                    slope_lon: p.f64("slope_lon").unwrap_or(0.0) * SEC_TO_RAD,
                    rho0: ell.a * (1.0 - ell.es) / (w * w.sqrt()),
                    nu0: ell.a / w.sqrt(),
                    inv,
                }
            }
            "hgridshift" => {
                let (epoch, final_epoch) = epochs();
                Step::HGridShift { grids: Grids::new(p.str("grids")?), inv, epoch, final_epoch }
            }
            "vgridshift" => {
                let (epoch, final_epoch) = epochs();
                Step::VGridShift {
                    grids: Grids::new(p.str("grids")?),
                    multiplier: p.f64("multiplier").unwrap_or(-1.0),
                    inv,
                    epoch,
                    final_epoch,
                }
            }
            "gridshift" => Step::GridShift {
                grids: Grids::new(p.str("grids")?),
                interpolation: p.str("interpolation").map(str::to_string),
                no_z: p.has("no_z_transform"),
                inv,
            },
            "xyzgridshift" => Step::XyzGridShift {
                grids: Grids::new(p.str("grids")?),
                ell: ellipsoid(&p).unwrap_or_else(Ell::wgs84),
                input_ref: p.str("grid_ref") != Some("output_crs"),
                multiplier: p.f64("multiplier").unwrap_or(1.0),
                inv,
            },
            "deformation" => Step::Deformation {
                grids: Grids::new(p.str("grids")?),
                ell: ellipsoid(&p).unwrap_or_else(Ell::wgs84),
                dt: p.f64("dt"),
                epoch: p.f64("t_epoch").unwrap_or(0.0),
                inv,
            },
            _ => return None,
        };
        out.push(step);
    }
    Some(out)
}

/// Geodetic (radians, metres) to geocentric coordinates.
pub(crate) fn to_cart(ell: &Ell, lon: f64, lat: f64, h: f64) -> [f64; 3] {
    let (sp, cp) = lat.sin_cos();
    let n = ell.a / (1.0 - ell.es * sp * sp).sqrt();
    [(n + h) * cp * lon.cos(), (n + h) * cp * lon.sin(), (n * ell.one_es + h) * sp]
}

/// Geocentric to geodetic coordinates, by Bowring's formula as PROJ's `cart` does.
pub(crate) fn from_cart(ell: &Ell, x: f64, y: f64, z: f64) -> [f64; 3] {
    // PROJ 9.7.0 cart.cpp (Thomas Knudsen / SDFE): normalized Bowring inverse,
    // including its pole clamp inside the ellipsoid and its origin convention.
    let (xn, yn, zn) = (x / ell.a, y / ell.a, z / ell.a);
    let p = xn.hypot(yn);
    let b_a = ell.one_es.sqrt();
    let norm = zn.hypot(p * b_a);
    let (c, s) = if norm == 0.0 { (1.0, 0.0) } else { (p * b_a / norm, zn / norm) };
    let y_phi = zn + ell.es / ell.one_es * b_a * s * s * s;
    let x_phi = p - ell.es * c * c * c;
    let norm_phi = y_phi.hypot(x_phi);
    let (mut cp, mut sp) = if norm_phi == 0.0 { (1.0, 0.0) } else { (x_phi / norm_phi, y_phi / norm_phi) };
    let lat = if x_phi <= 0.0 {
        cp = 0.0;
        sp = if z >= 0.0 { 1.0 } else { -1.0 };
        sp * std::f64::consts::FRAC_PI_2
    } else {
        (y_phi / x_phi).atan()
    };
    let h = if cp < 1e-6 {
        let b2 = b_a * b_a;
        let radius = ell.a * ((cp * cp + b2 * b2 * sp * sp) / (cp * cp + b2 * sp * sp)).sqrt();
        z.abs() - radius
    } else {
        let n = ell.a / (1.0 - ell.es * sp * sp).sqrt();
        ell.a * p / cp - n
    };
    [yn.atan2(xn), lat, h]
}

/// Applies a Helmert transformation (or its inverse, with the transposed rotation).
fn apply_helmert(v: [f64; 3], t: &[f64; 3], r: &[[f64; 3]; 3], scale: f64, pivot: &[f64; 3], inverse: bool) -> [f64; 3] {
    let mut o = [0.0; 3];
    if inverse {
        let d = [v[0] - t[0] - pivot[0], v[1] - t[1] - pivot[1], v[2] - t[2] - pivot[2]];
        for (i, oi) in o.iter_mut().enumerate() {
            *oi = (r[0][i] * d[0] + r[1][i] * d[1] + r[2][i] * d[2]) / scale + pivot[i];
        }
    } else {
        let d = [v[0] - pivot[0], v[1] - pivot[1], v[2] - pivot[2]];
        for (i, oi) in o.iter_mut().enumerate() {
            *oi = scale * (r[i][0] * d[0] + r[i][1] * d[1] + r[i][2] * d[2]) + t[i] + pivot[i];
        }
    }
    o
}

/// Geocentric translations from a grid at a geocentric point (PROJ's `xyzgridshift`).
fn xyz_values(sets: &[Arc<GridSet>], ell: &Ell, v: [f64; 3], multiplier: f64) -> Option<[f64; 3]> {
    let g = from_cart(ell, v[0], v[1], v[2]);
    let d = grid::bilinear3(sets, g[0], g[1], ["x_translation", "y_translation", "z_translation"], "metre")?;
    Some([d[0] * multiplier, d[1] * multiplier, d[2] * multiplier])
}

/// The velocity (m/year) of a deformation grid at a geocentric point, in geocentric axes.
fn deformation_shift(sets: &[Arc<GridSet>], ell: &Ell, v: [f64; 3]) -> Option<[f64; 3]> {
    let g = from_cart(ell, v[0], v[1], v[2]);
    let vel = grid::bilinear3(sets, g[0], g[1], ["east_velocity", "north_velocity", "up_velocity"], "millimetres per year")?;
    let (e, n, u) = (vel[0] / 1000.0, vel[1] / 1000.0, vel[2] / 1000.0);
    let (sp, cp) = g[1].sin_cos();
    let (sl, cl) = g[0].sin_cos();
    Some([-sp * cl * n - sl * e + cp * cl * u, -sp * sl * n + cl * e + cp * sl * u, cp * n + sp * u])
}

/// Whether a time-restricted grid step applies at time `t` (PROJ's `t_epoch`/`t_final`).
fn in_time(epoch: f64, final_epoch: f64, t: Option<f64>) -> bool {
    if final_epoch == 0.0 || epoch == 0.0 {
        return true;
    }
    t.is_some_and(|t| t < epoch) && final_epoch > epoch
}

impl Step {
    /// Applies the step to `v` (longitude and latitude in radians and height, or geocentric
    /// coordinates) at time `t` (decimal year, `None` for PROJ's `HUGE_VAL`). `None` when it
    /// fails (outside its grids, a missing grid or time).
    fn apply(&self, v: [f64; 3], t: Option<f64>, reverse: bool) -> Option<[f64; 3]> {
        match self {
            Step::Cart { ell, inv } => {
                Some(if *inv != reverse { from_cart(ell, v[0], v[1], v[2]) } else { to_cart(ell, v[0], v[1], v[2]) })
            }
            Step::Helmert { t: tr, r, scale, pivot, inv, rates } => {
                let inverse = *inv != reverse;
                match rates {
                    Some(q) if t.is_some_and(|t| t != q.epoch) => {
                        // PROJ's update_parameters: the parameters at the observation epoch.
                        let dt = t.unwrap_or(q.epoch) - q.epoch;
                        let tt = [q.t[0] + q.dt[0] * dt, q.t[1] + q.dt[1] * dt, q.t[2] + q.dt[2] * dt];
                        let rot = [q.rot[0] + q.drot[0] * dt, q.rot[1] + q.drot[1] * dt, q.rot[2] + q.drot[2] * dt];
                        let s = 1.0 + (q.s + q.ds * dt) * 1e-6;
                        let rm = rotation(rot, q.position_vector, q.exact);
                        Some(apply_helmert(v, &tt, &rm, s, pivot, inverse))
                    }
                    _ => Some(apply_helmert(v, tr, r, *scale, pivot, inverse)),
                }
            }
            Step::GeogOffset { dlon, dlat, dh, inv } => {
                let sign = if *inv != reverse { -1.0 } else { 1.0 };
                Some([v[0] + sign * dlon, v[1] + sign * dlat, v[2] + sign * dh])
            }
            Step::ZFlip => Some([v[0], v[1], -v[2]]),
            Step::VertOffset { lat0, lon0, dh, slope_lat, slope_lon, rho0, nu0, inv } => {
                let sign = if *inv != reverse { -1.0 } else { 1.0 };
                let off = dh + slope_lat * rho0 * (v[1] - lat0) + slope_lon * nu0 * (v[0] - lon0) * v[1].cos();
                Some([v[0], v[1], v[2] + sign * off])
            }
            Step::HGridShift { grids, inv, epoch, final_epoch } => {
                if !in_time(*epoch, *final_epoch, t) {
                    return Some(v);
                }
                let sets = grids.get(grid::hgrids)?;
                if sets.is_empty() {
                    return Some(v);
                }
                let (lon, lat) = grid::hgrid_apply(sets, v[0], v[1], *inv != reverse)?;
                Some([lon, lat, v[2]])
            }
            Step::VGridShift { grids, multiplier, inv, epoch, final_epoch } => {
                if !in_time(*epoch, *final_epoch, t) {
                    return Some(v);
                }
                let sets = grids.get(grid::vgrids)?;
                if sets.is_empty() {
                    return Some(v);
                }
                let d = grid::vgrid_value(sets, v[0], v[1], *multiplier)?;
                Some([v[0], v[1], if *inv != reverse { v[2] - d } else { v[2] + d }])
            }
            Step::GridShift { grids, interpolation, no_z, inv } => {
                let sets = grids.get(grid::generic_grids)?;
                let kinds = grids.kinds.get_or_init(|| grid::gridshift_kinds(sets)).as_ref()?;
                grid::gridshift_apply(sets, kinds, v, *inv != reverse, interpolation.as_deref(), *no_z)
            }
            Step::XyzGridShift { grids, ell, input_ref, multiplier, inv } => {
                let sets = grids.get(grid::generic_grids)?;
                let forward = *inv == reverse;
                let factor = if forward { 1.0 } else { -1.0 };
                // The grid is referenced to the input CRS: forward applies it directly, the
                // inverse iterates (and the other way round for the output CRS).
                if forward == *input_ref {
                    let d = xyz_values(sets, ell, v, *multiplier)?;
                    Some([v[0] + factor * d[0], v[1] + factor * d[1], v[2] + factor * d[2]])
                } else {
                    let mut p = v;
                    for _ in 0..10 {
                        let d = xyz_values(sets, ell, p, *multiplier)?;
                        let d = [d[0] * factor, d[1] * factor, d[2] * factor];
                        let err = ((p[0] - v[0]) - d[0]).powi(2) + ((p[1] - v[1]) - d[1]).powi(2) + ((p[2] - v[2]) - d[2]).powi(2);
                        p = [v[0] + d[0], v[1] + d[1], v[2] + d[2]];
                        if err < 1e-10 {
                            break;
                        }
                    }
                    Some(p)
                }
            }
            Step::Deformation { grids, ell, dt, epoch, inv } => {
                let sets = grids.get(grid::generic_grids)?;
                let dt = match dt {
                    Some(d) => *d,
                    None => t? - epoch,
                };
                if *inv == reverse {
                    let s = deformation_shift(sets, ell, v)?;
                    Some([v[0] + dt * s[0], v[1] + dt * s[1], v[2] + dt * s[2]])
                } else {
                    // PROJ's pj_deformation_reverse_shift.
                    let d0 = deformation_shift(sets, ell, v)?;
                    let z0 = d0[2];
                    let mut out = [v[0] - dt * d0[0], v[1] - dt * d0[1], v[2] + dt * d0[2]];
                    for _ in 0..10 {
                        let Some(d) = deformation_shift(sets, ell, out) else { break };
                        let dif = [out[0] + dt * d[0] - v[0], out[1] + dt * d[1] - v[1], out[2] - dt * d[2] - v[2]];
                        out = [out[0] + dif[0], out[1] + dif[1], out[2] + dif[2]];
                        if dif[0].hypot(dif[1]) <= 1e-8 {
                            break;
                        }
                    }
                    out[2] = v[2] - dt * z0;
                    Some(out)
                }
            }
        }
    }

    /// The grids the step needs, with whether each is optional.
    pub(crate) fn grid_names(&self) -> Vec<(String, bool)> {
        match self {
            Step::HGridShift { grids, .. }
            | Step::VGridShift { grids, .. }
            | Step::GridShift { grids, .. }
            | Step::XyzGridShift { grids, .. }
            | Step::Deformation { grids, .. } => grid::grid_names(&grids.spec),
            _ => Vec::new(),
        }
    }
}

impl Op {
    /// A transformation that applies everywhere.
    pub fn world(steps: Vec<Step>) -> Self {
        Self {
            bounds: [-180.0, -90.0, 180.0, 90.0],
            accuracy: -1.0,
            chain_accuracy: -1.0,
            chain_bounds: [-180.0, -90.0, 180.0, 90.0],
            offshore: false,
            unknown_area: false,
            ballpark: false,
            concatenated: false,
            between_3d: false,
            geocentric: false,
            priority: false,
            keeps_height: true,
            steps,
            name: String::new(),
        }
    }

    /// The transformation as PROJ concatenates it with other operations: with the accuracy and
    /// area of its steps.
    pub fn as_concatenated(&self) -> Self {
        let priority = self.priority || is_priority(&self.name);
        Self { accuracy: self.chain_accuracy, bounds: self.chain_bounds, priority, ..self.clone() }
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

    /// The grids the transformation needs, with whether each is optional.
    pub fn grids(&self) -> Vec<(String, bool)> {
        self.steps.iter().flat_map(Step::grid_names).collect()
    }

    /// Whether it needs grids.
    pub fn needs_grids(&self) -> bool {
        self.steps.iter().any(|s| !s.grid_names().is_empty())
    }

    /// Whether every grid it requires can be used (PROJ's "instantiable").
    pub fn available(&self) -> bool {
        self.grids().iter().all(|(name, optional)| *optional || grid::available(name))
    }
}

/// A point being transformed: longitude and latitude in radians, height in metres, and time
/// (decimal year, `None` for none).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Point {
    pub lon: f64,
    pub lat: f64,
    pub h: f64,
    pub t: Option<f64>,
}

/// Runs datum steps on a geodetic point. Heights go through the transformation when `heights`
/// is set (between 3D CRSs, or for pipelines PROJ runs without `push`/`pop`), and are kept
/// otherwise. `reverse` runs the inverse transformation. `None` when a step fails.
pub(crate) fn run(steps: &[Step], p: Point, reverse: bool, heights: bool) -> Option<Point> {
    let mut v = [p.lon, p.lat, p.h];
    if reverse {
        for s in steps.iter().rev() {
            v = s.apply(v, p.t, true)?;
        }
    } else {
        for s in steps {
            v = s.apply(v, p.t, false)?;
        }
    }
    if !v[0].is_finite() || !v[1].is_finite() {
        return None;
    }
    Some(Point { lon: v[0], lat: v[1], h: if heights { v[2] } else { p.h }, t: p.t })
}

/// Runs an op on a point: its heights change unless it keeps them between 2D CRSs.
pub(crate) fn run_op(op: &Op, p: Point, reverse: bool, three_d: bool) -> Option<Point> {
    run(&op.steps, p, reverse, three_d || !op.keeps_height)
}

/// Whether `op` ranks before `best` for a point both hold, as PROJ's
/// `pj_get_suggested_operation` compares them.
fn better(op: &Op, best: &Op) -> bool {
    better_by(op, op.accuracy, best, best.accuracy)
}

/// [`better`] with the accuracies given apart: those of the operations as PROJ chains them with
/// others (a vertical transformation and the horizontal one to its interpolation CRS).
pub(crate) fn better_by(op: &Op, accuracy: f64, best: &Op, best_accuracy: f64) -> bool {
    ((accuracy >= 0.0 && accuracy < best_accuracy)
        || (accuracy == best_accuracy
            && op.pseudo_area() < best.pseudo_area()
            && !(op.unknown_area && !best.unknown_area)
            && !best.priority))
        && !op.offshore
}

/// Picks the transformation for a point as PROJ does: among those whose area holds it (as told
/// by `contains`, given the op and its index), the most accurate, then the one with the smallest
/// area.
pub(crate) fn pick_in<T: AsRef<Op>>(ops: &[T], contains: impl Fn(&Op, usize) -> bool) -> Option<&Op> {
    pick_index(ops, &[], contains).map(|i| ops[i].as_ref())
}

/// The index of the transformation PROJ picks for a point, skipping `excluded`.
pub(crate) fn pick_index<T: AsRef<Op>>(ops: &[T], excluded: &[usize], contains: impl Fn(&Op, usize) -> bool) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, op) in ops.iter().map(AsRef::as_ref).enumerate() {
        if excluded.contains(&i) || !contains(op, i) {
            continue;
        }
        if best.is_none_or(|b| better(op, ops[b].as_ref())) {
            best = Some(i);
        }
    }
    best
}

/// Transforms a point with the operations of a list as PROJ's `proj_trans` does: the best one
/// for the point (see [`pick_index`]), and when it fails (outside its grid), the next best, up
/// to three attempts; then the first operation that needs no grid.
pub(crate) fn transform_with<T: AsRef<Op>>(
    ops: &[T],
    contains: impl Fn(&Op, usize) -> bool,
    apply: impl Fn(usize) -> Option<Point>,
) -> Option<Point> {
    let mut excluded = Vec::new();
    for _ in 0..3 {
        let Some(i) = pick_index(ops, &excluded, &contains) else { break };
        if let Some(p) = apply(i) {
            return Some(p);
        }
        excluded.push(i);
    }
    let first = ops.iter().position(|o| !o.as_ref().needs_grids())?;
    apply(first)
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
        let p = Point { lon: 0.09, lat: 0.91, h: 0.0, t: None };
        let q = run(&steps, p, false, false).unwrap();
        let r = run(&steps, q, true, false).unwrap();
        assert!((r.lon - 0.09).abs() < 1e-10 && (r.lat - 0.91).abs() < 1e-10);
    }
}
