// SPDX-FileCopyrightText: PROJ contributors
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! The projection parameters and world projections terrio reads and writes, once (#1058 step 5).
//! A parameter has its EPSG code and name, the normalized names a WKT reader also takes for it, and
//! the kind of its value. The WKT 2 reader (`wkt2`) and the PROJJSON writers (`projjson_write`,
//! `projjson_gdal`) share them, as PROJ shares its `paramMappings` (`parammappings.cpp`). A world
//! projection has its PROJ name and the names the WKT 1 and WKT 2 readers and the WKT 1 writer
//! take for it.
//!
//! [`conversion_from_proj`] decides a PROJ string's EPSG method and parameter values once, as PROJ
//! decides them; the PROJJSON writer (`projjson_write`) writes it.

use crate::params::Params;

/// The kind of a parameter's value: an angle (in degrees), a length (in metres) or a scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Angle,
    Length,
    Scale,
}

/// A projection parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Param {
    /// Its EPSG code; PROJ's parameters without one have codes down from `u32::MAX`, which match
    /// no `ID`.
    pub code: u32,
    /// Its EPSG name (PROJ's, without an EPSG code).
    pub name: &'static str,
    /// Its name normalized as [`crate::wkt1::norm`] normalizes it (checked by a test).
    pub key: &'static str,
    /// Other normalized names a WKT 2 reader takes for it (GDAL's WKT 1 names, older EPSG names).
    pub aliases: &'static [&'static str],
    pub kind: Kind,
}

impl Param {
    /// Whether a normalized name is the parameter's.
    pub(crate) fn named(&self, key: &str) -> bool {
        self.key == key || self.aliases.contains(&key)
    }
}

const fn param(code: u32, name: &'static str, key: &'static str, aliases: &'static [&'static str], kind: Kind) -> Param {
    Param { code, name, key, aliases, kind }
}

use Kind::{Angle, Length, Scale};

pub(crate) const LAT_NAT: Param = param(8801, "Latitude of natural origin", "latitudeofnaturalorigin", &["latitudeoforigin"], Angle);
pub(crate) const LON_NAT: Param = param(8802, "Longitude of natural origin", "longitudeofnaturalorigin", &["centralmeridian"], Angle);
pub(crate) const K_NAT: Param = param(8805, "Scale factor at natural origin", "scalefactoratnaturalorigin", &[], Scale);
pub(crate) const FE: Param = param(8806, "False easting", "falseeasting", &[], Length);
pub(crate) const FN: Param = param(8807, "False northing", "falsenorthing", &[], Length);
pub(crate) const LAT_CENTRE: Param = param(8811, "Latitude of projection centre", "latitudeofprojectioncentre", &[], Angle);
pub(crate) const LON_CENTRE: Param = param(8812, "Longitude of projection centre", "longitudeofprojectioncentre", &[], Angle);
pub(crate) const AZIMUTH: Param =
    param(8813, "Azimuth at projection centre", "azimuthatprojectioncentre", &["azimuthofinitialline"], Angle);
pub(crate) const GAMMA: Param = param(8814, "Angle from Rectified to Skew Grid", "anglefromrectifiedtoskewgrid", &[], Angle);
pub(crate) const K_CENTRE: Param =
    param(8815, "Scale factor at projection centre", "scalefactoratprojectioncentre", &["scalefactoroninitialline"], Scale);
pub(crate) const FE_CENTRE: Param = param(8816, "Easting at projection centre", "eastingatprojectioncentre", &[], Length);
pub(crate) const FN_CENTRE: Param = param(8817, "Northing at projection centre", "northingatprojectioncentre", &[], Length);
pub(crate) const LAT_PSEUDO_SP: Param = param(8818, "Latitude of pseudo standard parallel", "latitudeofpseudostandardparallel", &[], Angle);
pub(crate) const K_PSEUDO_SP: Param =
    param(8819, "Scale factor on pseudo standard parallel", "scalefactoronpseudostandardparallel", &[], Scale);
pub(crate) const LAT_FALSE: Param = param(8821, "Latitude of false origin", "latitudeoffalseorigin", &[], Angle);
pub(crate) const LON_FALSE: Param = param(8822, "Longitude of false origin", "longitudeoffalseorigin", &[], Angle);
pub(crate) const SP1: Param = param(8823, "Latitude of 1st standard parallel", "latitudeof1ststandardparallel", &[], Angle);
pub(crate) const SP2: Param = param(8824, "Latitude of 2nd standard parallel", "latitudeof2ndstandardparallel", &[], Angle);
pub(crate) const FE_FALSE: Param = param(8826, "Easting at false origin", "eastingatfalseorigin", &[], Length);
pub(crate) const FN_FALSE: Param = param(8827, "Northing at false origin", "northingatfalseorigin", &[], Length);
pub(crate) const LON_INITIAL: Param = param(8830, "Initial longitude", "initiallongitude", &[], Angle);
pub(crate) const ZONE_WIDTH: Param = param(8831, "Zone width", "zonewidth", &[], Angle);
pub(crate) const LAT_TS: Param = param(8832, "Latitude of standard parallel", "latitudeofstandardparallel", &[], Angle);
pub(crate) const LON_ORIGIN: Param = param(8833, "Longitude of origin", "longitudeoforigin", &[], Angle);
pub(crate) const LAT_TOPO: Param = param(8834, "Latitude of topocentric origin", "latitudeoftopocentricorigin", &[], Angle);
pub(crate) const LON_TOPO: Param = param(8835, "Longitude of topocentric origin", "longitudeoftopocentricorigin", &[], Angle);
pub(crate) const HEIGHT_TOPO: Param =
    param(8836, "Ellipsoidal height of topocentric origin", "ellipsoidalheightoftopocentricorigin", &[], Length);
pub(crate) const VIEW_HEIGHT: Param = param(8840, "Viewpoint height", "viewpointheight", &[], Length);
pub(crate) const CO_LAT_CONE: Param = param(1036, "Co-latitude of cone axis", "colatitudeofconeaxis", &[], Angle);
pub(crate) const K_ELLIPSOID: Param = param(1038, "Ellipsoid scaling factor", "ellipsoidscalingfactor", &[], Scale);
pub(crate) const H_PLANE_ORIGIN: Param = param(1039, "Projection plane origin height", "projectionplaneoriginheight", &[], Length);
// Krovak Modified's evaluation point and coefficients, which PROJ's `mod_krovak` fixes. Their
// names are the EPSG registry's: PROJ 9.9.0's `proj.db` leaves them out of its conversions.
pub(crate) const EVAL_1: Param = param(8617, "Ordinate 1 of evaluation point", "ordinate1ofevaluationpoint", &[], Length);
pub(crate) const EVAL_2: Param = param(8618, "Ordinate 2 of evaluation point", "ordinate2ofevaluationpoint", &[], Length);
pub(crate) const KROVAK_C: [Param; 10] = [
    param(1026, "C1", "c1", &[], Scale),
    param(1027, "C2", "c2", &[], Scale),
    param(1028, "C3", "c3", &[], Scale),
    param(1029, "C4", "c4", &[], Scale),
    param(1030, "C5", "c5", &[], Scale),
    param(1031, "C6", "c6", &[], Scale),
    param(1032, "C7", "c7", &[], Scale),
    param(1033, "C8", "c8", &[], Scale),
    param(1034, "C9", "c9", &[], Scale),
    param(1035, "C10", "c10", &[], Scale),
];

// PROJ's parameters without an EPSG code.
pub(crate) const SAT_HEIGHT: Param = param(u32::MAX, "Satellite Height", "satelliteheight", &[], Length);
pub(crate) const LAT_P1: Param = param(u32::MAX - 1, "Latitude of 1st point", "latitudeof1stpoint", &[], Angle);
pub(crate) const LON_P1: Param = param(u32::MAX - 2, "Longitude of 1st point", "longitudeof1stpoint", &[], Angle);
pub(crate) const LAT_P2: Param = param(u32::MAX - 3, "Latitude of 2nd point", "latitudeof2ndpoint", &[], Angle);
pub(crate) const LON_P2: Param = param(u32::MAX - 4, "Longitude of 2nd point", "longitudeof2ndpoint", &[], Angle);
pub(crate) const LAT_TRUE_SCALE: Param = param(u32::MAX - 5, "Latitude of true scale", "latitudeoftruescale", &[], Angle);

/// A world projection of a central meridian: its PROJ name, the normalized method name the WKT 2
/// reader takes for it, and its GDAL WKT 1 method, which the WKT 1 reader and writer take.
#[derive(Clone, Copy, Debug)]
pub(crate) struct World {
    pub proj: &'static str,
    pub wkt2: Option<&'static str>,
    pub wkt1: Option<&'static str>,
}

/// The world projections: those the WKT 2 reader takes, in its order, then Equal Earth.
pub(crate) const WORLD: [World; 28] = [
    World { proj: "moll", wkt2: Some("mollweide"), wkt1: Some("Mollweide") },
    World { proj: "robin", wkt2: Some("robinson"), wkt1: None },
    World { proj: "sinu", wkt2: Some("sinusoidal"), wkt1: None },
    World { proj: "eck1", wkt2: Some("eckerti"), wkt1: Some("Eckert_I") },
    World { proj: "eck2", wkt2: Some("eckertii"), wkt1: Some("Eckert_II") },
    World { proj: "eck3", wkt2: Some("eckertiii"), wkt1: Some("Eckert_III") },
    World { proj: "eck4", wkt2: Some("eckertiv"), wkt1: Some("Eckert_IV") },
    World { proj: "eck5", wkt2: Some("eckertv"), wkt1: Some("Eckert_V") },
    World { proj: "eck6", wkt2: Some("eckertvi"), wkt1: Some("Eckert_VI") },
    World { proj: "mill", wkt2: Some("millercylindrical"), wkt1: None },
    World { proj: "vandg", wkt2: Some("vandergrinten"), wkt1: Some("VanDerGrinten") },
    World { proj: "natearth", wkt2: Some("naturalearth"), wkt1: Some("Natural_Earth") },
    World { proj: "qua_aut", wkt2: Some("quarticauthalic"), wkt1: None },
    // PROJ's uninterrupted goode, which terrio refuses; it is not igh.
    World { proj: "goode", wkt2: Some("goodehomolosine"), wkt1: None },
    World { proj: "igh", wkt2: Some("interruptedgoodehomolosine"), wkt1: Some("Interrupted_Goode_Homolosine") },
    World { proj: "aitoff", wkt2: Some("aitoff"), wkt1: Some("Aitoff") },
    World { proj: "crast", wkt2: Some("crasterparabolic"), wkt1: None },
    World { proj: "natearth2", wkt2: Some("naturalearthii"), wkt1: None },
    World { proj: "patterson", wkt2: Some("patterson"), wkt1: None },
    World { proj: "times", wkt2: Some("times"), wkt1: None },
    World { proj: "mbtfpq", wkt2: Some("flatpolarquartic"), wkt1: None },
    World { proj: "wag1", wkt2: Some("wagneri"), wkt1: None },
    World { proj: "wag2", wkt2: Some("wagnerii"), wkt1: None },
    World { proj: "wag4", wkt2: Some("wagneriv"), wkt1: None },
    World { proj: "wag5", wkt2: Some("wagnerv"), wkt1: None },
    World { proj: "wag6", wkt2: Some("wagnervi"), wkt1: None },
    World { proj: "gall", wkt2: Some("gallstereographic"), wkt1: Some("Gall_Stereographic") },
    World { proj: "eqearth", wkt2: None, wkt1: Some("Equal_Earth") },
];

/// A projected CRS's conversion: its EPSG method and parameter values.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Conversion {
    /// Its name, when it has one (a UTM zone's); else it is unnamed, as PROJ names it `unknown`.
    pub name: Option<String>,
    /// Its EPSG code, when it has one (a UTM zone's).
    pub conversion_code: Option<u32>,
    /// Its method's name, and EPSG code (none for a method PROJ has without one, Miller
    /// Cylindrical).
    pub method: &'static str,
    pub method_code: Option<u32>,
    /// Its parameters' values, in degrees, metres and unity, in the order PROJ writes them.
    pub values: Vec<(Param, f64)>,
    /// The PROJ string's parameters it carries (others must have the value used when they are
    /// absent to be written: `projjson_write::refuse_unwritten`).
    pub written: &'static [&'static str],
}

/// Whether a PROJ string gives a sphere by its own parameters (`+R`, or `+a` and `+b` equal).
pub(crate) fn on_sphere(p: &Params) -> bool {
    !p.has("ellps") && !p.has("datum") && (p.has("R") || (p.has("a") && p.f64("a") == p.f64("b")))
}

/// Whether [`conversion_from_proj`] gives a PROJ string as Popular Visualisation Pseudo Mercator (EPSG
/// method 1024): Mercator on the sphere of WGS 84's semi-major axis (`+a=6378137 +b=6378137`),
/// with a null standard parallel and a scale of 1.
pub(crate) fn pseudo_mercator(p: &Params) -> bool {
    let k0 = p.f64("k_0").or_else(|| p.f64("k")).unwrap_or(1.0);
    // `+R` wins over `+a`: a sphere of another radius is not Web Mercator's.
    p.str("proj") == Some("merc")
        && !p.has("R")
        && on_sphere(p)
        && p.f64("a") == Some(6378137.0)
        && p.angle("lat_ts").map(f64::to_degrees).unwrap_or(0.0) == 0.0
        && k0 == 1.0
}

/// The conversion of a projected CRS's PROJ string, by its EPSG method, as PROJ decides it (except
/// where a branch notes a deliberate difference); or why there is none.
///
/// It decides; it does not validate. The PROJ string must be one terrio reads as a CRS: a value
/// it cannot parse is taken as absent here (`+lat_0=abc` as a latitude of origin of 0°). Nor does
/// `Ok` mean the conversion carries every parameter of the string: a writer refuses the others
/// itself (`projjson_write::refuse_unwritten`, by [`Conversion::written`]).
pub(crate) fn conversion_from_proj(p: &Params) -> Result<Conversion, String> {
    let proj = p.str("proj").unwrap_or("");
    let deg = |k: &str| p.angle(k).map(f64::to_degrees);
    let num = |k: &str| p.f64(k);
    let k0 = num("k_0").or_else(|| num("k")).unwrap_or(1.0);
    let (lat0, lon0) = (deg("lat_0").unwrap_or(0.0), deg("lon_0").unwrap_or(0.0));
    let (x0, y0) = (num("x_0").unwrap_or(0.0), num("y_0").unwrap_or(0.0));
    // On a sphere, the spherical methods, as PROJ 9.9.0 writes them: whenever the CRS's figure is
    // one (`+R`, which overrides `+ellps`, `+ellps=sphere`, `+a` = `+b`).
    let sphere = crate::def::proj_ellipsoid(p, proj, false).map_or_else(|_| on_sphere(p), |e| e.crs.es == 0.0);
    let mut name = None;
    let mut conversion_id = None;
    // The parameters most methods carry.
    const ORIGIN: &[&str] = &["lat_0", "lon_0", "x_0", "y_0"];
    const ORIGIN_K: &[&str] = &["lat_0", "lon_0", "k", "k_0", "x_0", "y_0"];
    const CONIC: &[&str] = &["lat_0", "lon_0", "lat_1", "lat_2", "x_0", "y_0"];
    let (method, code, values, written): (&str, u32, Vec<(Param, f64)>, &[&str]) = match proj {
        "utm" => {
            let zone = num("zone").ok_or_else(|| "a UTM projection without zone".to_string())? as u32;
            if !(1..=60).contains(&zone) {
                return Err(format!("a UTM projection of zone {zone}"));
            }
            let south = p.has("south");
            name = Some(format!("UTM zone {zone}{}", if south { "S" } else { "N" }));
            conversion_id = Some(if south { 16100 } else { 16000 } + zone);
            let lon = f64::from(zone) * 6.0 - 183.0;
            let values =
                vec![(LAT_NAT, 0.0), (LON_NAT, lon), (K_NAT, 0.9996), (FE, 500000.0), (FN, if south { 10_000_000.0 } else { 0.0 })];
            ("Transverse Mercator", 9807, values, &["zone", "south"])
        }
        // South-orientated (EPSG method 9808), whose false easting and northing the readers
        // write negated under `+axis=wsu`. A difference from PROJ 9.8.1, which writes a typed
        // `+axis=wsu +x_0=1000` with a false easting of 1000: that places its origin 2000 m away,
        // where W = FE - x places it at the string's. South then west (`+axis=swu`, a north-east
        // WKT 1) is Transverse Mercator on those axes, as PROJ writes it: PROJ reads method 9808
        // west then south whatever its axes.
        "tmerc" if p.str("axis") == Some("wsu") => {
            let values = vec![(LAT_NAT, lat0), (LON_NAT, lon0), (K_NAT, k0), (FE, 0.0 - x0), (FN, 0.0 - y0)];
            ("Transverse Mercator (South Orientated)", 9808, values, ORIGIN_K)
        }
        // PROJ 9.9.0's `tmerczoned`, whose `+lon_0` is the initial longitude.
        "tmerczoned" => {
            let width = num("width")
                .filter(|w| w.is_finite() && *w > 0.0)
                .ok_or_else(|| "a zoned grid system without a positive width".to_string())?;
            (
                "Transverse Mercator Zoned Grid System",
                9824,
                vec![(LAT_NAT, lat0), (LON_INITIAL, lon0), (ZONE_WIDTH, width), (K_NAT, k0), (FE, x0), (FN, y0)],
                &["lat_0", "lon_0", "width", "k", "k_0", "x_0", "y_0"],
            )
        }
        "tmerc" => ("Transverse Mercator", 9807, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (K_NAT, k0), (FE, x0), (FN, y0)], ORIGIN_K),
        "lcc" => {
            // As PROJ (and terrio) read the CRS: with `lat_0` and no `lat_2`, one standard parallel
            // (1SP, or variant B away from the origin); else two, the second on the equator
            // without `lat_2` (2SP, or 2SP Michigan with a scale).
            let sp1 = deg("lat_1").unwrap_or(0.0);
            let one = !p.has("lat_2") && p.has("lat_0");
            if one && lat0 == sp1 {
                let values = vec![(LAT_NAT, sp1), (LON_NAT, lon0), (K_NAT, k0), (FE, x0), (FN, y0)];
                ("Lambert Conic Conformal (1SP)", 9801, values, &["lat_0", "lat_1", "lon_0", "k", "k_0", "x_0", "y_0"])
            } else if one {
                let values = vec![(LAT_NAT, sp1), (K_NAT, k0), (LAT_FALSE, lat0), (LON_FALSE, lon0), (FE_FALSE, x0), (FN_FALSE, y0)];
                ("Lambert Conic Conformal (1SP variant B)", 1102, values, &["lat_0", "lat_1", "lon_0", "k", "k_0", "x_0", "y_0"])
            } else {
                let mut values = vec![
                    (LAT_FALSE, lat0),
                    (LON_FALSE, lon0),
                    (SP1, sp1),
                    (SP2, deg("lat_2").unwrap_or(0.0)),
                    (FE_FALSE, x0),
                    (FN_FALSE, y0),
                ];
                if k0 == 1.0 {
                    ("Lambert Conic Conformal (2SP)", 9802, values, &["lat_0", "lat_1", "lat_2", "lon_0", "k", "k_0", "x_0", "y_0"])
                } else {
                    values.push((K_ELLIPSOID, k0));
                    (
                        "Lambert Conic Conformal (2SP Michigan)",
                        1051,
                        values,
                        &["lat_0", "lat_1", "lat_2", "lon_0", "k", "k_0", "x_0", "y_0"],
                    )
                }
            }
        }
        "aea" | "eqdc" => {
            let values = vec![
                (LAT_FALSE, lat0),
                (LON_FALSE, lon0),
                (SP1, deg("lat_1").unwrap_or(0.0)),
                (SP2, deg("lat_2").unwrap_or(0.0)),
                (FE_FALSE, x0),
                (FN_FALSE, y0),
            ];
            if proj == "aea" { ("Albers Equal Area", 9822, values, CONIC) } else { ("Equidistant Conic", 1119, values, CONIC) }
        }
        "laea" if sphere => {
            ("Lambert Azimuthal Equal Area (Spherical)", 1027, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN)
        }
        "laea" => ("Lambert Azimuthal Equal Area", 9820, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        // EPSG:3857: WGS 84 coordinates projected on its semi-major axis's sphere.
        "merc" if pseudo_mercator(p) => {
            let values = vec![(LAT_NAT, 0.0), (LON_NAT, lon0), (FE, x0), (FN, y0)];
            ("Popular Visualisation Pseudo Mercator", 1024, values, &["lat_ts", "lon_0", "x_0", "y_0"])
        }
        // With `lat_ts`, 0 too, variant B, as PROJ 9.9.0 writes it; its scale is then the parallel's,
        // and `k` changes nothing (in PROJ's `merc` and terrio's alike).
        "merc" => match deg("lat_ts") {
            Some(ts) => (
                "Mercator (variant B)",
                9805,
                vec![(SP1, ts), (LON_NAT, lon0), (FE, x0), (FN, y0)],
                &["lat_ts", "lon_0", "k", "k_0", "x_0", "y_0"],
            ),
            None => {
                let values = vec![(LAT_NAT, 0.0), (LON_NAT, lon0), (K_NAT, k0), (FE, x0), (FN, y0)];
                ("Mercator (variant A)", 9804, values, &["lat_ts", "lon_0", "k", "k_0", "x_0", "y_0"])
            }
        },
        "stere" if lat0.abs() == 90.0 => match deg("lat_ts").filter(|_| !p.has("k") && !p.has("k_0")) {
            Some(ts) => (
                "Polar Stereographic (variant B)",
                9829,
                vec![(LAT_TS, ts), (LON_ORIGIN, lon0), (FE, x0), (FN, y0)],
                &["lat_0", "lat_ts", "lon_0", "x_0", "y_0"],
            ),
            None => {
                ("Polar Stereographic (variant A)", 9810, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (K_NAT, k0), (FE, x0), (FN, y0)], ORIGIN_K)
            }
        },
        "sterea" => ("Oblique Stereographic", 9809, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (K_NAT, k0), (FE, x0), (FN, y0)], ORIGIN_K),
        "somerc" => {
            let values = vec![
                (LAT_CENTRE, lat0),
                (LON_CENTRE, lon0),
                (AZIMUTH, 90.0),
                (GAMMA, 90.0),
                (K_CENTRE, k0),
                (FE_CENTRE, x0),
                (FN_CENTRE, y0),
            ];
            ("Hotine Oblique Mercator (variant B)", 9815, values, ORIGIN_K)
        }
        "omerc" => {
            let alpha = deg("alpha").ok_or_else(|| "an oblique Mercator without azimuth".to_string())?;
            let gamma = deg("gamma").unwrap_or(alpha);
            // Without `lonc`, the centre is on the prime meridian: PROJ's `omerc` (and terrio's)
            // ignores `lon_0` with an azimuth, and PROJ 9.9.0 writes 0.
            let lonc = deg("lonc").unwrap_or(0.0);
            let written: &[&str] = if p.has("lonc") {
                &["lat_0", "lonc", "alpha", "gamma", "k", "k_0", "x_0", "y_0", "no_uoff", "no_off"]
            } else {
                &["lat_0", "lon_0", "alpha", "gamma", "k", "k_0", "x_0", "y_0", "no_uoff", "no_off"]
            };
            if p.has("no_uoff") || p.has("no_off") {
                let values =
                    vec![(LAT_CENTRE, lat0), (LON_CENTRE, lonc), (AZIMUTH, alpha), (GAMMA, gamma), (K_CENTRE, k0), (FE, x0), (FN, y0)];
                ("Hotine Oblique Mercator (variant A)", 9812, values, written)
            } else {
                let values = vec![
                    (LAT_CENTRE, lat0),
                    (LON_CENTRE, lonc),
                    (AZIMUTH, alpha),
                    (GAMMA, gamma),
                    (K_CENTRE, k0),
                    (FE_CENTRE, x0),
                    (FN_CENTRE, y0),
                ];
                ("Hotine Oblique Mercator (variant B)", 9815, values, written)
            }
        }
        // Krovak Modified shares Krovak's constants; PROJ 9.9.0 writes neither its evaluation point
        // nor its coefficients.
        "krovak" | "mod_krovak" => {
            // As terrio and PROJ compute it: their defaults, and the pseudo standard parallel fixed
            // (PROJ ignores `lat_1` and `lat_ts`). The co-latitude of the cone axis is fixed too:
            // another `alpha` (which PROJ reads) is refused.
            let modified = proj == "mod_krovak";
            let alpha = deg("alpha").unwrap_or(30.288_139_752_777_8);
            if (alpha - 30.288_139_752_777_8).abs() > 1e-6 {
                return Err(format!("a {} projection with +alpha={alpha}", if modified { "Krovak Modified" } else { "Krovak" }));
            }
            let values = vec![
                (LAT_CENTRE, deg("lat_0").unwrap_or(49.5)),
                (LON_ORIGIN, deg("lon_0").unwrap_or(24.0 + 50.0 / 60.0)),
                // Krovak Modified's as given, as PROJ 9.9.0 writes it.
                (CO_LAT_CONE, if modified { alpha } else { 30.288_139_752_777_8 }),
                (LAT_PSEUDO_SP, 78.5),
                (K_PSEUDO_SP, num("k_0").or_else(|| num("k")).unwrap_or(0.9999)),
                (FE, x0),
                (FN, y0),
            ];
            let written: &[&str] = &["lat_0", "lon_0", "alpha", "lat_1", "lat_ts", "k", "k_0", "x_0", "y_0"];
            let (name, code) = match (modified, p.str("axis") == Some("swu")) {
                (false, true) => ("Krovak", 9819),
                (false, false) => ("Krovak (North Orientated)", 1041),
                (true, true) => ("Krovak Modified", 1042),
                (true, false) => ("Krovak Modified (North Orientated)", 1043),
            };
            (name, code, values, written)
        }
        // Its latitude of origin changes nothing (PROJ writes none); a `k_0` without `lat_ts` is
        // not written. On a sphere, the spherical method, as PROJ 9.9.0 writes it.
        "cea" => {
            let values = vec![(SP1, deg("lat_ts").unwrap_or(0.0)), (LON_NAT, lon0), (FE, x0), (FN, y0)];
            let (method, code) =
                if sphere { ("Lambert Cylindrical Equal Area (Spherical)", 9834) } else { ("Lambert Cylindrical Equal Area", 9835) };
            (method, code, values, &["lat_0", "lat_ts", "lon_0", "x_0", "y_0"])
        }
        "eqc" => {
            // Its latitude of origin as PROJ writes it: only when not 0.
            let mut values = vec![(SP1, deg("lat_ts").unwrap_or(0.0)), (LON_NAT, lon0), (FE, x0), (FN, y0)];
            if lat0 != 0.0 {
                values.insert(1, (LAT_NAT, lat0));
            }
            let (method, code) = if sphere { ("Equidistant Cylindrical (Spherical)", 1029) } else { ("Equidistant Cylindrical", 1028) };
            (method, code, values, &["lat_0", "lat_ts", "lon_0", "x_0", "y_0"])
        }
        // A method PROJ has without an EPSG code (0 here).
        "mill" => ("Miller Cylindrical", 0, vec![(LON_NAT, lon0), (FE, x0), (FN, y0)], &["lon_0", "x_0", "y_0"]),
        "cass" if p.has("hyperbolic") => (
            "Hyperbolic Cassini-Soldner",
            9833,
            vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)],
            &["lat_0", "lon_0", "x_0", "y_0", "hyperbolic"],
        ),
        "cass" => ("Cassini-Soldner", 9806, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        "poly" => ("American Polyconic", 9818, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        "nzmg" => ("New Zealand Map Grid", 9811, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        "aeqd" if p.has("guam") => {
            ("Guam Projection", 9831, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], &["lat_0", "lon_0", "x_0", "y_0", "guam"])
        }
        "col_urban" => (
            "Colombia Urban",
            1052,
            vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0), (H_PLANE_ORIGIN, num("h_0").unwrap_or(0.0))],
            &["lat_0", "lon_0", "x_0", "y_0", "h_0"],
        ),
        "aeqd" => ("Azimuthal Equidistant", 1125, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        "ortho" => ("Orthographic", 9840, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        "eqearth" => ("Equal Earth", 1078, vec![(LON_NAT, lon0), (FE, x0), (FN, y0)], &["lon_0", "x_0", "y_0"]),
        "bonne" => {
            // Its parallel is `lat_1`; `lat_0` changes nothing then.
            let values = vec![(LAT_NAT, deg("lat_1").unwrap_or(lat0)), (LON_NAT, lon0), (FE, x0), (FN, y0)];
            ("Bonne", 9827, values, if p.has("lat_1") { &["lat_0", "lat_1", "lon_0", "x_0", "y_0"] } else { ORIGIN })
        }
        other => return Err(format!("the projection +proj={other}")),
    };
    // `str::parse` reads `nan` and `inf`, which JSON cannot carry.
    if let Some((param, _)) = values.iter().find(|(_, v)| !v.is_finite()) {
        return Err(format!("a non-finite {}", param.name.to_lowercase()));
    }
    Ok(Conversion { name, conversion_code: conversion_id, method, method_code: (code != 0).then_some(code), values, written })
}

/// Every parameter of the table.
#[cfg(test)]
const ALL: [Param; 49] = [
    LAT_NAT,
    LON_NAT,
    K_NAT,
    FE,
    FN,
    LAT_CENTRE,
    LON_CENTRE,
    AZIMUTH,
    GAMMA,
    K_CENTRE,
    FE_CENTRE,
    FN_CENTRE,
    LAT_PSEUDO_SP,
    K_PSEUDO_SP,
    LAT_FALSE,
    LON_FALSE,
    SP1,
    SP2,
    FE_FALSE,
    FN_FALSE,
    LON_INITIAL,
    ZONE_WIDTH,
    LAT_TS,
    LON_ORIGIN,
    LAT_TOPO,
    LON_TOPO,
    HEIGHT_TOPO,
    VIEW_HEIGHT,
    CO_LAT_CONE,
    K_ELLIPSOID,
    H_PLANE_ORIGIN,
    EVAL_1,
    EVAL_2,
    KROVAK_C[0],
    KROVAK_C[1],
    KROVAK_C[2],
    KROVAK_C[3],
    KROVAK_C[4],
    KROVAK_C[5],
    KROVAK_C[6],
    KROVAK_C[7],
    KROVAK_C[8],
    KROVAK_C[9],
    SAT_HEIGHT,
    LAT_P1,
    LON_P1,
    LAT_P2,
    LON_P2,
    LAT_TRUE_SCALE,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A PROJ string's method, as PROJ 9.9.0 writes it in PROJJSON (`CRS(s).to_json()`).
    #[test]
    fn a_proj_string_has_the_method_proj_gives_it() {
        for (s, method, code) in [
            ("+proj=utm +zone=33 +south +ellps=WGS84", "Transverse Mercator", 9807),
            ("+proj=tmerc +axis=wsu +lon_0=25 +x_0=1000 +y_0=2000 +ellps=WGS84", "Transverse Mercator (South Orientated)", 9808),
            ("+proj=lcc +lat_0=45 +lat_1=45 +lon_0=3 +ellps=GRS80", "Lambert Conic Conformal (1SP)", 9801),
            ("+proj=lcc +lat_0=40 +lat_1=45 +lon_0=3 +ellps=GRS80", "Lambert Conic Conformal (1SP variant B)", 1102),
            ("+proj=lcc +lat_1=44 +lat_2=49 +lat_0=46.5 +lon_0=3 +ellps=GRS80", "Lambert Conic Conformal (2SP)", 9802),
            ("+proj=merc +lat_ts=30 +ellps=WGS84", "Mercator (variant B)", 9805),
            ("+proj=merc +lat_ts=0 +k=0.99 +ellps=WGS84", "Mercator (variant B)", 9805),
            ("+proj=merc +k=0.99 +ellps=WGS84", "Mercator (variant A)", 9804),
            ("+proj=eqc +lat_ts=30 +R=6371000", "Equidistant Cylindrical (Spherical)", 1029),
            ("+proj=eqc +lat_ts=30 +ellps=WGS84", "Equidistant Cylindrical", 1028),
            ("+proj=cea +lat_ts=30 +a=6371000 +b=6371000", "Lambert Cylindrical Equal Area (Spherical)", 9834),
            ("+proj=cea +lat_ts=30 +ellps=WGS84", "Lambert Cylindrical Equal Area", 9835),
            ("+proj=cea +ellps=sphere", "Lambert Cylindrical Equal Area (Spherical)", 9834),
            ("+proj=eqc +R=6371000 +ellps=WGS84", "Equidistant Cylindrical (Spherical)", 1029),
            ("+proj=laea +ellps=sphere", "Lambert Azimuthal Equal Area (Spherical)", 1027),
            ("+proj=stere +lat_0=90 +lat_ts=70 +ellps=WGS84", "Polar Stereographic (variant B)", 9829),
            ("+proj=omerc +lat_0=4 +lonc=115 +alpha=53 +gamma=53.13 +no_uoff +ellps=GRS80", "Hotine Oblique Mercator (variant A)", 9812),
            ("+proj=krovak +ellps=bessel", "Krovak (North Orientated)", 1041),
        ] {
            let c = conversion_from_proj(&Params::parse(s)).unwrap();
            assert_eq!((c.method, c.method_code), (method, Some(code)), "{s}");
        }
        // Its false easting and northing negated, as terrio reads `+axis=wsu` (W = FE - x).
        let south = conversion_from_proj(&Params::parse("+proj=tmerc +axis=wsu +lon_0=25 +x_0=1000 +y_0=2000 +ellps=WGS84")).unwrap();
        assert!(south.values.contains(&(FE, -1000.0)) && south.values.contains(&(FN, -2000.0)), "{:?}", south.values);
        // Miller Cylindrical, which PROJ writes without an EPSG code.
        let mill = conversion_from_proj(&Params::parse("+proj=mill +lon_0=10 +R=6371000")).unwrap();
        assert_eq!((mill.method, mill.method_code, mill.values), ("Miller Cylindrical", None, vec![(LON_NAT, 10.0), (FE, 0.0), (FN, 0.0)]));
        // An oblique Mercator without `lonc` is centred on the prime meridian, whatever `lon_0`.
        let omerc = conversion_from_proj(&Params::parse("+proj=omerc +lat_0=4 +lon_0=115 +alpha=53 +k=0.99984 +ellps=GRS80")).unwrap();
        assert!(omerc.values.contains(&(LON_CENTRE, 0.0)), "{:?}", omerc.values);
        // Mercator with `lat_ts` 0: variant B, without the `k` it ignores.
        let merc = conversion_from_proj(&Params::parse("+proj=merc +lat_ts=0 +k=0.99 +ellps=WGS84")).unwrap();
        assert_eq!(merc.values, vec![(SP1, 0.0), (LON_NAT, 0.0), (FE, 0.0), (FN, 0.0)]);
        let utm = conversion_from_proj(&Params::parse("+proj=utm +zone=33 +south +ellps=WGS84")).unwrap();
        assert_eq!((utm.name.as_deref(), utm.conversion_code), (Some("UTM zone 33S"), Some(16133)));
        let north = conversion_from_proj(&Params::parse("+proj=utm +zone=33 +ellps=WGS84")).unwrap();
        assert_eq!((north.name.as_deref(), north.conversion_code), (Some("UTM zone 33N"), Some(16033)));
        assert_eq!(conversion_from_proj(&Params::parse("+proj=utm +ellps=WGS84")), Err("a UTM projection without zone".to_string()));
    }

    /// Each world projection's PROJ name, WKT 2 name and WKT 1 method are its own, and its WKT 2
    /// name is normalized as the reader compares it.
    #[test]
    fn the_world_table_is_consistent() {
        for w in WORLD {
            let same = |v: &&World| v.proj == w.proj || (w.wkt2.is_some() && v.wkt2 == w.wkt2) || (w.wkt1.is_some() && v.wkt1 == w.wkt1);
            assert_eq!(WORLD.iter().filter(same).count(), 1, "{w:?}");
            if let Some(k) = w.wkt2 {
                assert_eq!(k, crate::wkt1::norm(k), "{w:?}");
            }
        }
    }

    /// Each parameter's key is its name normalized, and its code and names are its own. (The EPSG
    /// names and codes are those of PROJ 9.9.0's `proj.db`, `conversion_param`.)
    #[test]
    fn the_parameter_table_is_consistent() {
        // Every parameter constant of this file (and of its arrays of them) is in `ALL`, which the
        // checks below go over.
        let constants: usize = include_str!("methods.rs")
            .lines()
            .filter(|l| l.starts_with("pub(crate) const "))
            .map(|l| match l.split_once(": [Param; ") {
                Some((_, n)) => n.split(']').next().and_then(|n| n.parse().ok()).unwrap(),
                None => usize::from(l.contains(": Param =")),
            })
            .sum();
        assert_eq!(constants, ALL.len());
        for p in ALL {
            assert_eq!(p.key, crate::wkt1::norm(p.name), "{p:?}");
            let same = |q: &&Param| q.code == p.code || q.named(p.key) || p.aliases.iter().any(|a| q.named(a));
            assert_eq!(ALL.iter().filter(same).count(), 1, "{p:?}");
        }
    }
}
