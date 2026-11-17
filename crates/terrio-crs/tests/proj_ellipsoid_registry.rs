//! The PROJ strings of the EPSG and ESRI registries keep their ellipsoids under PROJ 9.9.0's
//! reading of ellipsoid parameters (`tests/proj_ellipsoid_shape.rs`). For every registry CRS with
//! a PROJ string, the semi-major and semi-minor axes terrio reads from that string (as PROJ
//! writes it, `to_proj4`) are those of the CRS's own WKT `SPHEROID`/`ELLIPSOID` within 1e-6 m (the
//! rounding of its inverse flattening to 15 digits), but for the CRSs of [`KEPT_AXES`], whose
//! axes are those terrio read before, bit for bit.

use terrio_crs::{CrsQuery, SpatialRef, list_crs};

/// The CRSs whose PROJ string has another ellipsoid than their WKT's, beyond the rounding of
/// the WKT's inverse flattening, with the axes terrio read from the string before, bit for bit.
const KEPT_AXES: &[(&str, f64, f64)] = &[
    // In links, as its WKT gives the axes.
    ("EPSG:3139", 6378306.3696, 6356571.996),
    // Web Mercator, on its sphere.
    ("EPSG:3785", 6378137.0, 6378137.0),
    ("EPSG:3857", 6378137.0, 6378137.0),
    ("EPSG:6871", 6378137.0, 6378137.0),
    ("EPSG:900913", 6378137.0, 6378137.0),
    ("ESRI:102100", 6378137.0, 6378137.0),
    // PROJ's `bess_nam`, whose semi-major axis is rounded from EPSG's Bessel Namibia.
    ("EPSG:4006", 6377483.865, 6356165.382966326),
    ("EPSG:4293", 6377483.865, 6356165.382966326),
    ("EPSG:29333", 6377483.865, 6356165.382966326),
    ("EPSG:29371", 6377483.865, 6356165.382966326),
    ("EPSG:29373", 6377483.865, 6356165.382966326),
    ("EPSG:29375", 6377483.865, 6356165.382966326),
    ("EPSG:29377", 6377483.865, 6356165.382966326),
    ("EPSG:29379", 6377483.865, 6356165.382966326),
    ("EPSG:29381", 6377483.865, 6356165.382966326),
    ("EPSG:29383", 6377483.865, 6356165.382966326),
    ("EPSG:29385", 6377483.865, 6356165.382966326),
    ("ESRI:103989", 6377483.865, 6356165.382966326),
    // PROJ's `walbeck` and `clrk66` by their semi-minor axes, where ESRI's WKT rounds the
    // inverse flattening.
    ("ESRI:37007", 6376896.0, 6355834.8467),
    ("ESRI:102096", 6378206.4, 6356583.8),
    ("ESRI:102237", 6378206.4, 6356583.8),
    ("ESRI:102238", 6378206.4, 6356583.8),
    ("ESRI:102239", 6378206.4, 6356583.8),
];

/// The semi-major axis and inverse flattening of a WKT's first ellipsoid node.
fn spheroid(wkt: &str) -> Option<(f64, f64)> {
    let start = ["SPHEROID[", "ELLIPSOID["].iter().find_map(|k| wkt.find(k).map(|i| i + k.len()))?;
    let mut args = wkt[start..].split(',').skip(1).map(|v| v.trim_end_matches(']').trim().parse::<f64>());
    Some((args.next()?.ok()?, args.next()?.ok()?))
}

#[test]
fn registry_proj_strings_keep_their_ellipsoids() {
    let mut bad = vec![];
    let mut checked = 0;
    for entry in list_crs(&CrsQuery { authorities: vec!["EPSG".into(), "ESRI".into()], include_deprecated: true, ..Default::default() }) {
        let code = entry.authority_code();
        let Ok(crs) = SpatialRef::parse(&code) else { continue };
        let (Some(proj), Some(wkt)) = (crs.to_proj4(), crs.to_wkt()) else { continue };
        let Some(read) = SpatialRef::from_proj4(&proj).ok().and_then(|s| s.projection_params()) else { continue };
        // Bit for bit those read before, else within the rounding of the WKT's inverse flattening.
        let (expected, tolerance) = match KEPT_AXES.iter().find(|o| o.0 == code) {
            Some(&(_, a, b)) => ((a, b), 0.0),
            None => match spheroid(&wkt) {
                Some((a, rf)) => ((a, if rf == 0.0 { a } else { a - a / rf }), 1e-6),
                None => continue,
            },
        };
        checked += 1;
        let (da, db) = ((read.a - expected.0).abs(), (read.b - expected.1).abs());
        if !da.is_finite() || !db.is_finite() || da > tolerance || db > tolerance {
            bad.push(format!("(\"{code}\", {:?}, {:?}), // {proj}: expected {expected:?}", read.a, read.b));
        }
    }
    assert!(checked > 8000, "only {checked} registry CRSs checked");
    assert!(bad.is_empty(), "{} of {checked}:\n{}", bad.len(), bad.join("\n"));
}
