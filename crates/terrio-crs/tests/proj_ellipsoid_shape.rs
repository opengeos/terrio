//! A PROJ string's ellipsoid is read as PROJ 9.8.1 reads it, with `+type=crs`: `+R` first, then
//! `+datum`'s ellipsoid or else `+ellps`', with `+a` for its size, and the first of `+rf`, `+f`,
//! `+es`, `+e` and `+b` for its shape (PROJ drops the others). A named ellipsoid or datum keeps
//! its shape under `+f`, `+es` and `+e`. An ellipsoid PROJ defines by its semi-minor axis
//! (`clrk66`, `sphere`) keeps that axis under `+a`, `+rf`, `+f`, `+es` and `+e`; a datum's is
//! defined by its inverse flattening (NAD27's Clarke 1866 too), which `+a` keeps. `+f=0` makes
//! orthographic spherical. A spherification flag PROJ's CRS parser does not take as part of the
//! method (all but `+R_A` for `laea` and `cea`) keeps the string as PROJ reads
//! it in a pipeline: the shape parameter then changes the named ellipsoid's shape, `+a` keeps
//! its eccentricity, and the first flag of `+R_A`, `+R_V`, `+R_a`, `+R_g`, `+R_h`, `+R_lat_a`,
//! `+R_lat_g` and `+R_C` makes a sphere of it. A shape without a size is refused.
//!
//! `data/proj_ellipsoid_shape.json` (`scripts/gen_proj_ellipsoid_shape.py`, pyproj 3.8.0,
//! no grids, `PROJ_NETWORK=OFF`) holds, for each string, PROJ's coordinates of a point
//! (`Transformer.from_crs("EPSG:4326", CRS(s + " +type=crs"), always_xy=True)`, or from a
//! geographic CRS to EPSG:4326) and the axes of the CRS's ellipsoid (`CRS.ellipsoid`), or null
//! where PROJ refuses the string. Tolerance: 1 mm.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

/// Where terrio places the case's point, or why it refuses the string.
fn place(s: &str, point: (f64, f64)) -> Result<(SpatialRef, Vec<f64>), String> {
    let crs = SpatialRef::parse(s).map_err(|e| e.to_string())?;
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let got = if crs.is_geographic() {
        let (x, y) = Transformer::new(&crs, &wgs84).and_then(|t| t.transform(point.0, point.1)).map_err(|e| e.to_string())?;
        vec![x, y]
    } else if crs.is_geocentric() {
        let (x, y, z) = Transformer::new(&wgs84, &crs).and_then(|t| t.transform_3d(point.0, point.1, 0.0)).map_err(|e| e.to_string())?;
        vec![x, y, z]
    } else {
        let (x, y) = Transformer::new(&wgs84, &crs).and_then(|t| t.transform(point.0, point.1)).map_err(|e| e.to_string())?;
        vec![x, y]
    };
    Ok((crs, got))
}

#[test]
fn ellipsoid_parameters_are_read_as_proj_reads_them() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/proj_ellipsoid_shape.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 481);
    let mut bad = vec![];
    for case in cases {
        let s = case["proj_string"].as_str().unwrap();
        let point = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        let got = place(s, point);
        let Some(proj) = case["proj"].as_array() else {
            if let Ok((_, got)) = got {
                bad.push(format!("{s}: PROJ refuses it, terrio {got:?}"));
            }
            continue;
        };
        let proj = proj.iter().map(|v| v.as_f64().unwrap()).collect::<Vec<_>>();
        let (crs, got) = match got {
            Ok(g) => g,
            Err(e) => {
                bad.push(format!("{s}: PROJ {proj:?}, terrio refuses it: {e}"));
                continue;
            }
        };
        let off = if crs.is_geographic() {
            ((got[0] - proj[0]) * proj[1].to_radians().cos()).hypot(got[1] - proj[1]) * 111_195.0
        } else {
            got.iter().zip(&proj).map(|(g, p)| (g - p) * (g - p)).sum::<f64>().sqrt()
        };
        if off >= 1e-3 {
            bad.push(format!("{s}: PROJ {proj:?}, terrio {got:?}, {off:.3} m"));
        }
        // The ellipsoid the CRS reports, before a spherification flag makes a sphere of it.
        let Some(params) = crs.projection_params() else {
            bad.push(format!("{s}: no projection parameters"));
            continue;
        };
        let (a, b) = (case["ellipsoid"][0].as_f64().unwrap(), case["ellipsoid"][1].as_f64().unwrap());
        if (params.a - a).abs() >= 1e-3 || (params.b - b).abs() >= 1e-3 {
            bad.push(format!("{s}: PROJ's ellipsoid {a}, {b}, terrio's {}, {}", params.a, params.b));
        }
    }
    assert!(bad.is_empty(), "{} differences from PROJ in {} cases:\n{}", bad.len(), cases.len(), bad.join("\n"));
}

/// An unchanged datum's ellipsoid is its named ellipsoid, bit for bit, so that WKT 1 names it:
/// PROJ 9.8.1 writes `SPHEROID["Clarke 1866",6378206.4,294.978698213898,AUTHORITY["EPSG","7008"]]`
/// for `+proj=longlat +datum=NAD27 +towgs84=-8,160,176`.
#[test]
fn an_unchanged_datum_keeps_its_named_ellipsoid() {
    let params = |s: &str| SpatialRef::parse(s).unwrap().projection_params().unwrap();
    for (datum, ellps) in [("NAD27", "clrk66"), ("ire65", "mod_airy"), ("WGS84", "WGS84"), ("OSGB36", "airy")] {
        let e = params(&format!("+proj=tmerc +ellps={ellps}"));
        // Also under `+a` of its own semi-major axis, which PROJ 9.8.1 reports as the named
        // ellipsoid (Clarke 1866, b = 6356583.8).
        for s in [
            format!("+proj=tmerc +datum={datum}"),
            format!("+proj=tmerc +datum={datum} +a={}", e.a),
            format!("+proj=tmerc +ellps={ellps} +a={}", e.a),
        ] {
            let d = params(&s);
            assert_eq!((d.a.to_bits(), d.b.to_bits()), (e.a.to_bits(), e.b.to_bits()), "{s}");
        }
    }
    let wkt = SpatialRef::parse("+proj=longlat +datum=NAD27 +a=6378206.4").unwrap().to_wkt().unwrap();
    assert!(wkt.contains(r#"SPHEROID["Clarke 1866",6378206.4,294.9786982139"#), "{wkt}");
    let wkt = SpatialRef::parse("+proj=longlat +datum=NAD27 +towgs84=-8,160,176").unwrap().to_wkt().unwrap();
    // Its name and code; terrio computes the inverse flattening from the eccentricity
    // (294.978698213901), PROJ from the axes.
    assert!(wkt.contains(r#"SPHEROID["Clarke 1866",6378206.4,294.9786982139"#) && wkt.contains(r#"AUTHORITY["EPSG","7008"]]"#), "{wkt}");
    // Under `+a`, PROJ keeps the datum's inverse flattening (b = 6378303.518 for 6400 km).
    let reshaped = params("+proj=tmerc +datum=NAD27 +a=6400000");
    assert!((reshaped.b - 6_378_303.518_054_856).abs() < 1e-6, "{}", reshaped.b);
}

/// WKT writes the CRS's ellipsoid, not the sphere a spherification flag or spherical
/// orthographic computes on, as PROJ 9.8.1's `CRS(s + " +type=crs").to_wkt("WKT1_GDAL")` writes
/// it (pyproj 3.8.0); the PROJ4 extension keeps the flag. The inverse flattening of Clarke 1866,
/// which terrio computes from its eccentricity, is compared to 10 decimals.
#[test]
fn wkt_writes_the_crs_ellipsoid_under_a_spherification_flag() {
    let wgs84 = r#"SPHEROID["WGS 84",6378137,298.257223563"#;
    for (s, spheroid) in [
        // `+R_A` as the method's spherical variant.
        ("+proj=laea +lat_0=45 +lon_0=-100 +ellps=WGS84 +R_A", wgs84),
        ("+proj=cea +ellps=WGS84 +R_A", wgs84),
        ("+proj=mill +ellps=WGS84 +R_A", wgs84),
        ("+proj=laea +ellps=WGS84 +R_a", wgs84),
        ("+proj=laea +ellps=clrk66 +R_A", r#"SPHEROID["Clarke 1866",6378206.4,294.9786982139"#),
        // Flags PROJ keeps in its extension.
        ("+proj=tmerc +ellps=WGS84 +R_A", wgs84),
        ("+proj=tmerc +ellps=WGS84 +R_V", wgs84),
        ("+proj=tmerc +ellps=WGS84 +R_C", wgs84),
        ("+proj=ortho +f=0 +datum=WGS84", wgs84),
    ] {
        let wkt = SpatialRef::parse(s).unwrap().to_wkt().unwrap();
        assert!(wkt.contains(spheroid), "{s}: {wkt}");
    }
}

/// A WKT's `PROJ4` extension is computed on as PROJ 9.8.1 computes on it, as a pipeline step:
/// `+ellps=WGS84 +f=0` (or an unreadable `+f`, read as 0) is a sphere, `+ellps=clrk66
/// +a=6400000` keeps Clarke 1866's eccentricity, no ellipsoid is GRS 1980, `+R_a` is not `+R_A`
/// (as it is to PROJ's CRS parser for `laea`), and `+datum=` is `pj_datum_set`'s shift onto the
/// figure, under `+R_C` too. Where the step refuses the ellipsoid (`+rf=abc`
/// or `+rf=0`, `+es=1`, a shape without a size), PROJ creates the CRS but refuses the
/// transformation, and terrio refuses it. `data/proj_ellipsoid_shape.json`'s `extensions`
/// (`Transformer.from_crs("EPSG:4326", CRS(wkt), always_xy=True)`). Tolerance: 1 mm.
#[test]
fn a_wkt_proj4_extension_is_computed_on_as_a_pipeline_step() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/proj_ellipsoid_shape.json")).unwrap();
    let cases = data["extensions"].as_array().unwrap();
    assert_eq!(cases.len(), 25);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let mut bad = vec![];
    for case in cases {
        let wkt = case["wkt"].as_str().unwrap();
        let got = Transformer::new(&wgs84, &SpatialRef::from_wkt(wkt)).and_then(|t| t.transform(-80.0, 30.0));
        match (case["proj"].as_array(), got) {
            (None, Ok(got)) => bad.push(format!("{wkt}: PROJ refuses it, terrio {got:?}")),
            (Some(proj), Ok(got)) => {
                let off = (got.0 - proj[0].as_f64().unwrap()).hypot(got.1 - proj[1].as_f64().unwrap());
                if off >= 1e-3 {
                    bad.push(format!("{wkt}: PROJ {proj:?}, terrio {got:?}, {off:.3} m"));
                }
            }
            (Some(proj), Err(e)) => bad.push(format!("{wkt}: PROJ {proj:?}, terrio refuses it: {e}")),
            (None, Err(_)) => {}
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The WKT terrio writes for a PROJ string reads back as the string, as PROJ 9.8.1 reads the
/// string: where WKT 1 cannot describe the CRS, its `PROJ4` extension, which PROJ and terrio
/// compute on as a pipeline step, gives the CRS's figure and datum rather than the string's own
/// ellipsoid parameters (which the step reads otherwise). `data/proj_ellipsoid_shape.json`'s
/// `written` holds such strings, the extension terrio writes, and PROJ's coordinates of that WKT
/// (`Transformer.from_crs("EPSG:4326", CRS(wkt), always_xy=True)`, the same as of the string).
/// Every string of `cases` is read back from its WKT where PROJ places it. Tolerance: 1 mm.
#[test]
fn a_written_wkt_reads_back_as_its_proj_string() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/proj_ellipsoid_shape.json")).unwrap();
    let written = data["written"].as_array().unwrap();
    assert_eq!(written.len(), 9);
    for case in written {
        let s = case["proj_string"].as_str().unwrap();
        let wkt = SpatialRef::parse(s).unwrap().to_wkt().unwrap();
        let extension = format!("EXTENSION[\"PROJ4\",\"{}\"]", case["extension"].as_str().unwrap());
        assert!(wkt.contains(&extension), "{s}: {wkt}");
    }
    let mut bad = vec![];
    for case in data["cases"].as_array().unwrap() {
        let s = case["proj_string"].as_str().unwrap();
        let Some(proj) = case["proj"].as_array() else { continue };
        let proj = proj.iter().map(|v| v.as_f64().unwrap()).collect::<Vec<_>>();
        let Some(wkt) = SpatialRef::parse(s).ok().and_then(|c| c.to_wkt()) else { continue };
        let point = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        let back = SpatialRef::from_wkt(&wkt);
        let wgs84 = SpatialRef::from_epsg(4326).unwrap();
        let got = if back.is_geographic() {
            Transformer::new(&back, &wgs84).and_then(|t| t.transform(point.0, point.1)).map(|p| vec![p.0, p.1])
        } else if back.is_geocentric() {
            Transformer::new(&wgs84, &back).and_then(|t| t.transform_3d(point.0, point.1, 0.0)).map(|p| vec![p.0, p.1, p.2])
        } else {
            Transformer::new(&wgs84, &back).and_then(|t| t.transform(point.0, point.1)).map(|p| vec![p.0, p.1])
        };
        let Ok(got) = got else {
            bad.push(format!("{s}: its WKT is refused: {wkt}"));
            continue;
        };
        let off = if back.is_geographic() {
            ((got[0] - proj[0]) * proj[1].to_radians().cos()).hypot(got[1] - proj[1]) * 111_195.0
        } else {
            got.iter().zip(&proj).map(|(g, p)| (g - p) * (g - p)).sum::<f64>().sqrt()
        };
        if off >= 1e-3 {
            bad.push(format!("{s}: PROJ {proj:?}, read back {got:?}, {off:.3} m: {wkt}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// A WKT's `PROJ4` extension is kept through the WKT (not as the CRS's PROJ string), and the
/// writers that describe a PROJ string by PROJ's CRS rules (PROJJSON, GeoKeys) refuse one the
/// pipeline step computes on otherwise: `+ellps=WGS84 +f=0` is a sphere there. One that places
/// points alike both ways is described as before, as PROJ 9.8.1 writes PROJJSON for all of them
/// (losing the sphere of the first).
#[test]
fn writers_describe_an_extension_only_as_it_is_computed_on() {
    let wkt = |ext: &str| {
        format!(
            "PROJCS[\"custom\",GEOGCS[\"custom\",DATUM[\"custom\",SPHEROID[\"WGS 84\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],PROJECTION[\"Transverse_Mercator\"],PARAMETER[\"latitude_of_origin\",0],PARAMETER[\"central_meridian\",-75],PARAMETER[\"scale_factor\",1],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0],UNIT[\"metre\",1],EXTENSION[\"PROJ4\",\"{ext}\"]]"
        )
    };
    for (ext, alike) in [
        ("+proj=tmerc +lon_0=-75 +ellps=WGS84 +f=0", false),
        ("+proj=tmerc +lon_0=-75 +ellps=clrk66 +a=6400000", false),
        ("+proj=tmerc +lon_0=-75 +datum=WGS84 +a=6400000", false),
        ("+proj=tmerc +lon_0=-75 +ellps=WGS84", true),
        // NAD83, which the step takes as WGS 84 (a null shift on GRS 1980), and GRS 1980, the
        // step's ellipsoid without one, which places points as the CRS rules' WGS 84 does.
        ("+proj=tmerc +lon_0=-75 +datum=NAD83 +units=m +no_defs", true),
        ("+proj=tmerc +lon_0=-75 +units=m +no_defs", true),
    ] {
        let crs = SpatialRef::from_wkt(&wkt(ext));
        assert_eq!(crs.proj4, None, "{ext}");
        assert_eq!(crs.to_projjson().is_ok(), alike, "{ext}: {:?}", crs.to_projjson());
        assert_eq!(terrio_crs::geokeys::user_defined_keys(&crs, "custom").is_some(), alike, "{ext}");
    }
}
