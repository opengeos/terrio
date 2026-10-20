//! GeoTIFFs whose CRS has no EPSG code, written by GDAL 3.12
//! (`scripts/make_user_crs_fixtures.py`): every projection method GDAL writes as GeoKeys, an ESRI
//! PE string, and `.aux.xml` sidecars. Each is read eagerly and lazily, and a point transformed into
//! its CRS is checked against PROJ's coordinates for the CRS GDAL reads.

use std::path::{Path, PathBuf};

use terrio::crs::{SpatialRef, Transformer};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/user_crs")
}

/// A file, GDAL's CRS for it (as a PROJ string), a point (lon, lat) and its coordinates in that CRS.
type Expected = (String, String, (f64, f64), (f64, f64));

/// What GDAL reads, by file.
fn expected() -> Vec<Expected> {
    let text = std::fs::read_to_string(dir().join("expected.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let f = |c: &serde_json::Value, k: &str| c[k].as_f64().unwrap();
    let gdal = |c: &serde_json::Value| c["gdal"].as_str().unwrap().to_string();
    v.as_object().unwrap().iter().map(|(name, c)| (name.clone(), gdal(c), (f(c, "lon"), f(c, "lat")), (f(c, "x"), f(c, "y")))).collect()
}

/// Files whose CRS terrio reads but whose projection it computes differently from PROJ, or (from
/// WKT) reads with other parameters: their CRS read is checked against GDAL's parameters instead.
const PROJECTION_DIFFERS: &[&str] = &[
    // Spherical or approximate formulas in wbprojection.
    "gnom",
    "mill",
    "robin",
    "sinu",
    "vandg",
    // The WKT 2 of the sidecar: its parameters are a WKT 2 reading issue.
    "eqearth_sidecar",
    "geos_esri_pe",
];

/// The parameters of a PROJ string, by name.
fn params(proj: &str) -> std::collections::HashMap<String, String> {
    proj.split_whitespace()
        .filter_map(|t| t.strip_prefix('+'))
        .map(|t| t.split_once('=').map_or((t, ""), |(k, v)| (k, v)))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Whether `ours` has GDAL's projection method and parameters (absent numbers being 0).
fn same_params(ours: &str, gdal: &str) -> bool {
    let (a, b) = (params(ours), params(gdal));
    let num = |m: &std::collections::HashMap<String, String>, k: &str| m.get(k).map_or(Some(0.0), |v| v.parse::<f64>().ok());
    a.get("proj") == b.get("proj")
        && ["lon_0", "lat_0", "lat_1", "lat_2", "lat_ts", "x_0", "y_0", "h"]
            .iter()
            .all(|k| matches!((num(&a, k), num(&b, k)), (Some(x), Some(y)) if (x - y).abs() < 1e-9))
}

#[test]
fn reads_user_defined_crs_as_gdal() {
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let mut failures = Vec::new();
    for (name, gdal, (lon, lat), (x, y)) in expected() {
        let path = dir().join(format!("{name}.tif"));
        let path = path.to_str().unwrap();
        let eager = terrio::open_raster(path).unwrap().crs();
        let lazy = terrio::open_raster_lazy(path).unwrap().crs();
        for (how, crs) in [("eager", &eager), ("lazy", &lazy)] {
            if crs.is_unknown() || crs.epsg == Some(9001) {
                failures.push(format!("{name} ({how}): CRS read as {crs:?}"));
                continue;
            }
            if PROJECTION_DIFFERS.contains(&name.as_str()) {
                if let Some(p) = &crs.proj4
                    && !same_params(p, &gdal)
                {
                    failures.push(format!("{name} ({how}): {p} is not GDAL's {gdal}"));
                }
                continue;
            }
            match Transformer::new(&wgs84, crs).and_then(|t| t.transform(lon, lat)) {
                Ok((tx, ty)) if (tx - x).abs() < 1e-3 && (ty - y).abs() < 1e-3 => {}
                r => failures.push(format!("{name} ({how}): {r:?}, PROJ ({x}, {y}), CRS {crs:?}")),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
