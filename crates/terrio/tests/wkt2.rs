//! WKT 2 projected CRSs (#123): pyproj's WKT 2 (2019 and 2015) of PROJ strings and of EPSG CRSs
//! without their root `ID` give the coordinates of the CRS they describe, as WKT 1 does: within
//! 1 mm of PROJ, of terrio's PROJ string or EPSG code, and of terrio's WKT 1.

mod wkt2_cases {
    include!("wkt2/cases.rs");
}

use terrio::crs::{SpatialRef, Transformer};
use wkt2_cases::CASES;

/// CRSs whose PROJ string or EPSG code terrio itself transforms differently from PROJ, whatever
/// the WKT: Cassini-Soldner (0.4 m), Robinson and sinusoidal (through wbprojection), and NAD83
/// to WGS 84 for EPSG:2263. Their WKT 2 is still compared with terrio's PROJ string or code.
const PROJ_DIFFERS: [&str; 4] = ["cass", "robin", "sinu", "epsg_2263"];

/// The coordinates of WGS 84 points in a CRS.
fn forward(crs: &str, points: &[(f64, f64)]) -> terrio::Result<Vec<(f64, f64)>> {
    let t = Transformer::new(&SpatialRef::parse("EPSG:4326")?, &SpatialRef::parse(crs)?)?;
    points.iter().map(|&(lon, lat)| t.transform(lon, lat)).collect()
}

fn max_distance(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    a.iter().zip(b).map(|(p, q)| (p.0 - q.0).hypot(p.1 - q.1)).fold(0.0, f64::max)
}

#[test]
fn wkt2_projected_crss_agree_with_proj_and_wkt1() {
    let mut failures = Vec::new();
    for c in CASES {
        // A US survey foot CRS: 1 mm is 0.00328 ft.
        let tol = 1e-3 / if c.reference.contains("us-ft") || c.name == "epsg_2263" { 0.3048006096012192 } else { 1.0 };
        let reference = forward(c.reference, c.points).unwrap_or_else(|e| panic!("{}: {e}", c.name));
        if let Some(proj) = c.proj.filter(|_| !PROJ_DIFFERS.contains(&c.name)) {
            let d = max_distance(&reference, proj);
            if d >= tol {
                failures.push(format!("{}: terrio's reference CRS is {d} from PROJ", c.name));
            }
        }
        let mut versions = vec![("WKT2_2019", c.wkt2_2019), ("WKT2_2015", c.wkt2_2015)];
        if let Some(w1) = c.wkt1 {
            versions.push(("WKT1", w1));
        }
        for (version, wkt) in versions {
            match forward(wkt, c.points) {
                Ok(got) => {
                    let d = max_distance(&got, &reference);
                    if d >= tol {
                        failures.push(format!("{} {version}: {d} from the reference", c.name));
                    }
                }
                Err(e) => failures.push(format!("{} {version}: {e}", c.name)),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A method or a parameter terrio does not know is refused, not replaced by a default.
#[test]
fn unknown_wkt2_methods_and_parameters_raise() {
    let tmerc = CASES.iter().find(|c| c.name == "tmerc").unwrap().wkt2_2019;
    let method = tmerc.replace(r#"METHOD["Transverse Mercator",ID["EPSG",9807]]"#, r#"METHOD["Unknown method"]"#);
    assert!(SpatialRef::parse(&method).and_then(|s| Transformer::new(&SpatialRef::parse("EPSG:4326")?, &s)).is_err());
    let parameter = tmerc
        .replace(r#"PARAMETER["Latitude of natural origin",10,"#, r#"PARAMETER["Latitude of something",10,"#)
        .replace(r#"ID["EPSG",8801]"#, r#"ID["EPSG",1]"#);
    assert!(SpatialRef::parse(&parameter).and_then(|s| Transformer::new(&SpatialRef::parse("EPSG:4326")?, &s)).is_err());
    // Nodes that do not change coordinates are ignored.
    let remark = tmerc.replacen(r#"CS[Cartesian,2]"#, r#"REMARK["a remark"],CS[Cartesian,2]"#, 1);
    let points = [(-103.0, 10.0)];
    assert!(max_distance(&forward(&remark, &points).unwrap(), &forward(tmerc, &points).unwrap()) < 1e-9);
}

/// A `BOUNDCRS` whose transformation is not to WGS 84, or is a grid's, keeps its source CRS.
#[test]
fn boundcrs_without_a_helmert_to_wgs84_keeps_its_source() {
    let tmerc = CASES.iter().find(|c| c.name == "tmerc").unwrap().wkt2_2019;
    let bound = |target: &str, transformation: &str| {
        format!("BOUNDCRS[SOURCECRS[{tmerc}],TARGETCRS[{target}],ABRIDGEDTRANSFORMATION[\"t\",{transformation}]]")
    };
    let wgs84 = r#"GEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]],CS[ellipsoidal,2],AXIS["latitude",north],AXIS["longitude",east],ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",4326]]"#;
    let nad83 = r#"GEOGCRS["NAD83",DATUM["North American Datum 1983",ELLIPSOID["GRS 1980",6378137,298.257222101]],CS[ellipsoidal,2],AXIS["latitude",north],AXIS["longitude",east],ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",4269]]"#;
    let points = [(-103.0, 10.0), (-100.0, 12.0)];
    let plain = forward(tmerc, &points).unwrap();
    for wkt in [
        bound(wgs84, r#"METHOD["NTv2",ID["EPSG",9615]],PARAMETERFILE["Latitude and longitude difference file","grid.gsb"]"#),
        bound(
            nad83,
            r#"METHOD["Geocentric translations (geog2D domain)",ID["EPSG",9603]],PARAMETER["X-axis translation",100],PARAMETER["Y-axis translation",0],PARAMETER["Z-axis translation",0]"#,
        ),
    ] {
        assert!(max_distance(&forward(&wkt, &points).unwrap(), &plain) < 1e-6, "{wkt}");
    }
}
