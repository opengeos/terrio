//! A PROJ string's ellipsoid is read as PROJ 9.9.0 reads it, with `+type=crs`: `+R` first, then
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
    assert_eq!(cases.len(), 484);
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
        if !off.is_finite() || off >= 1e-3 {
            bad.push(format!("{s}: PROJ {proj:?}, terrio {got:?}, {off:.3} m"));
        }
        // The ellipsoid the CRS reports, before a spherification flag makes a sphere of it.
        let Some(params) = crs.projection_params() else {
            bad.push(format!("{s}: no projection parameters"));
            continue;
        };
        let (a, b) = (case["ellipsoid"][0].as_f64().unwrap(), case["ellipsoid"][1].as_f64().unwrap());
        let (da, db) = ((params.a - a).abs(), (params.b - b).abs());
        if !da.is_finite() || !db.is_finite() || da >= 1e-3 || db >= 1e-3 {
            bad.push(format!("{s}: PROJ's ellipsoid {a}, {b}, terrio's {}, {}", params.a, params.b));
        }
    }
    assert!(bad.is_empty(), "{} differences from PROJ in {} cases:\n{}", bad.len(), cases.len(), bad.join("\n"));
}

/// An unchanged datum's ellipsoid is its named ellipsoid, bit for bit, so that WKT 1 names it:
/// PROJ 9.9.0 writes `SPHEROID["Clarke 1866",6378206.4,294.978698213898,AUTHORITY["EPSG","7008"]]`
/// for `+proj=longlat +datum=NAD27 +towgs84=-8,160,176`.
#[test]
fn an_unchanged_datum_keeps_its_named_ellipsoid() {
    let params = |s: &str| SpatialRef::parse(s).unwrap().projection_params().unwrap();
    for (datum, ellps) in [("NAD27", "clrk66"), ("ire65", "mod_airy"), ("WGS84", "WGS84"), ("OSGB36", "airy")] {
        let e = params(&format!("+proj=tmerc +ellps={ellps}"));
        // Also under `+a` of its own semi-major axis, which PROJ 9.9.0 reports as the named
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
/// orthographic computes on, as PROJ 9.9.0's `CRS(s + " +type=crs").to_wkt("WKT1_GDAL")` writes
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

/// A WKT's `PROJ4` extension is computed on as PROJ 9.9.0 computes on it, as a pipeline step:
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
    assert_eq!(cases.len(), 27);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let mut bad = vec![];
    for case in cases {
        let wkt = case["wkt"].as_str().unwrap();
        let got = Transformer::new(&wgs84, &SpatialRef::from_wkt(wkt)).and_then(|t| t.transform(-80.0, 30.0));
        match (case["proj"].as_array(), got) {
            (None, Ok(got)) => bad.push(format!("{wkt}: PROJ refuses it, terrio {got:?}")),
            (Some(proj), Ok(got)) => {
                let off = (got.0 - proj[0].as_f64().unwrap()).hypot(got.1 - proj[1].as_f64().unwrap());
                if !off.is_finite() || off >= 1e-3 {
                    bad.push(format!("{wkt}: PROJ {proj:?}, terrio {got:?}, {off:.3} m"));
                }
            }
            (Some(proj), Err(e)) => bad.push(format!("{wkt}: PROJ {proj:?}, terrio refuses it: {e}")),
            (None, Err(_)) => {}
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The WKT terrio writes for a PROJ string reads back as the string, as PROJ 9.9.0 reads the
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
        if !off.is_finite() || off >= 1e-3 {
            bad.push(format!("{s}: PROJ {proj:?}, read back {got:?}, {off:.3} m: {wkt}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// A WKT's `PROJ4` extension is kept through the WKT (not as the CRS's PROJ string), and the
/// writers that describe a PROJ string by PROJ's CRS rules (PROJJSON, GeoKeys) refuse one the
/// pipeline step computes on otherwise: `+ellps=WGS84 +f=0` is a sphere there, and a datum
/// shifted otherwise (by the step's grids, or as placed without grids). One that places points alike
/// both ways is described as before, as PROJ 9.9.0 writes PROJJSON for all of them
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
        // GRS 1980, the step's ellipsoid without one, which places points as the CRS rules'
        // WGS 84 does.
        ("+proj=tmerc +lon_0=-75 +units=m +no_defs", true),
        // NAD83, which the step takes as WGS 84 (a null shift on GRS 1980), as NAD83's
        // transformations to WGS 84 place points without grids (PROJ's default here), whatever is
        // installed (a GDAL `custom_proj4` WKT).
        ("+proj=tmerc +lon_0=-75 +datum=NAD83 +units=m +no_defs", true),
        // A datum the CRS rules shift by its EPSG transformations without grids (OSGB36's
        // Helmert ones by area) and the step by `pj_datum_set`'s, or by NAD27's grids: refused,
        // whether or not grids are installed and wherever the origin is.
        ("+proj=tmerc +lat_0=49 +lon_0=-2 +k=0.9996012717 +x_0=400000 +y_0=-100000 +datum=OSGB36 +units=m +no_defs", false),
        ("+proj=tmerc +lon_0=-75 +datum=NAD27 +units=m +no_defs", false),
        // Compared in metres, not in its units (here of 10⁹ m).
        ("+proj=tmerc +lat_0=49 +lon_0=-2 +datum=OSGB36 +to_meter=1000000000 +no_defs", false),
    ] {
        let crs = SpatialRef::from_wkt(&wkt(ext));
        assert_eq!(crs.proj4, None, "{ext}");
        terrio_crs::take_warnings();
        assert_eq!(crs.to_projjson().is_ok(), alike, "{ext}: {:?}", crs.to_projjson());
        assert_eq!(terrio_crs::geokeys::user_defined_keys(&crs, "custom").is_some(), alike, "{ext}");
        // The check's own diagnostics (a missing grid, a ballpark) are not the caller's.
        assert_eq!(terrio_crs::take_warnings(), Vec::<String>::new(), "{ext}");
    }
    // A null grid on WGS 84, no shift either way, is no grid shift to refuse in PROJJSON.
    assert!(SpatialRef::from_wkt(&wkt("+proj=tmerc +lon_0=-75 +datum=WGS84 +nadgrids=@null +units=m +no_defs")).to_projjson().is_ok());
}

/// PROJJSON is refused where it would describe another figure than the CRS computes on: a WKT
/// whose `PROJ4` extension is on another ellipsoid than its `SPHEROID` (PROJ 9.9.0 writes the
/// WKT's, WGS 84 here, where its coordinates are on the 6400 km sphere), and spherical
/// orthographic (`+f=0`), which PROJ writes as its own "Orthographic (Spherical)" method.
#[test]
fn projjson_is_refused_on_another_figure() {
    let wkt = "PROJCS[\"custom\",GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],PROJECTION[\"custom_proj4\"],UNIT[\"metre\",1],EXTENSION[\"PROJ4\",\"+proj=tmerc +lon_0=-75 +R=6400000\"]]";
    assert!(SpatialRef::from_wkt(wkt).to_projjson().is_err());
    assert!(SpatialRef::parse("+proj=ortho +lat_0=40 +lon_0=-90 +ellps=WGS84 +f=0").unwrap().to_projjson().is_err());
    assert!(SpatialRef::parse("+proj=ortho +lat_0=40 +lon_0=-90 +ellps=WGS84").unwrap().to_projjson().is_ok());
}

/// `+nadgrids=@null` is no shift whatever `+towgs84` says, and keeps a `+datum=` as it is, as
/// PROJ 9.9.0 reads it: `+datum=OSGB36 +nadgrids=@null` into EPSG:4277 and into WGS 84 is the
/// identity (PROJ's bound CRS over OSGB36 with the null grid), likewise NAD27's into EPSG:4267.
/// PROJJSON writes no shift either (PROJ writes the null grid), and reads back so.
#[test]
fn a_null_grid_is_no_shift_and_keeps_the_datum() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let at = |src: &SpatialRef, dst: &str, p: (f64, f64)| {
        Transformer::new(src, &SpatialRef::parse(dst).unwrap()).unwrap().transform(p.0, p.1).unwrap()
    };
    for (s, code, p) in [
        ("+proj=longlat +datum=OSGB36 +nadgrids=@null", "EPSG:4277", (-2.0, 52.0)),
        ("+proj=longlat +datum=NAD27 +nadgrids=@null", "EPSG:4267", (-100.0, 40.0)),
    ] {
        let crs = SpatialRef::parse(s).unwrap();
        for dst in [code, "EPSG:4326"] {
            let q = at(&crs, dst, p);
            assert!((q.0 - p.0).abs() < 1e-9 && (q.1 - p.1).abs() < 1e-9, "{s} into {dst}: {q:?}");
        }
    }
    // Into another datum, through WGS 84 by the null shift, not by the datum's own
    // transformations: (-2, 52) in ETRS89 (PROJ: `unknown to WGS84 + Inverse of ETRS89 to WGS 84
    // (1)`), where plain OSGB36 is about 100 m off; NAD27's into NAD83 likewise.
    for (s, dst, p) in [
        ("+proj=longlat +datum=OSGB36 +nadgrids=@null", "EPSG:4258", (-2.0, 52.0)),
        ("+proj=longlat +datum=NAD27 +nadgrids=@null", "EPSG:4269", (-100.0, 40.0)),
    ] {
        let q = at(&SpatialRef::parse(s).unwrap(), dst, p);
        assert!((q.0 - p.0).abs() < 1e-9 && (q.1 - p.1).abs() < 1e-9, "{s} into {dst}: {q:?}");
    }
    // Its best operation into WGS 84 is the null shift, not OSGB36's grid (OSTN15), which PROJ
    // never uses for it: no missing grid refused under ONLY_BEST.
    let options = terrio_crs::TransformOptions { only_best: true, ..Default::default() };
    let osgb = SpatialRef::parse("+proj=longlat +datum=OSGB36 +nadgrids=@null").unwrap();
    let t = Transformer::with_options(&osgb, &SpatialRef::from_epsg(4326).unwrap(), &options).unwrap();
    let q = t.transform(-2.0, 52.0).unwrap();
    assert!((q.0 + 2.0).abs() < 1e-9 && (q.1 - 52.0).abs() < 1e-9, "{q:?}");
    let used = t.operation_at(-2.0, 52.0, 0.0).unwrap();
    assert!(used.as_ref().is_none_or(|op| !op.name.contains("OSGB36 to WGS 84")), "{used:?}");
    // A WKT with that extension, with or without OSGB36's code, is not EPSG:4277, whose
    // transformations it does not take: (-2, 52) in WGS 84, as PROJ places it.
    let wkt = r#"GEOGCS["OSGB 1936",DATUM["OSGB_1936",SPHEROID["Airy 1830",6377563.396,299.3249646,AUTHORITY["EPSG","7001"]],AUTHORITY["EPSG","6277"]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],EXTENSION["PROJ4","+proj=longlat +datum=OSGB36 +nadgrids=@null"],AUTHORITY["EPSG","4277"]]"#;
    for w in [wkt.to_string(), wkt.replace(r#",AUTHORITY["EPSG","4277"]]"#, "]")] {
        let crs = SpatialRef::from_wkt(&w);
        assert_eq!(crs.epsg, None, "{w}");
        let q = at(&crs, "EPSG:4326", (-2.0, 52.0));
        assert!((q.0 + 2.0).abs() < 1e-9 && (q.1 - 52.0).abs() < 1e-9, "{w}: {q:?}");
    }
    // But one on WGS 84, geographic or projected, is identified as its code, as PROJ 9.9.0
    // identifies them (`CRS.to_epsg()`): the code places points alike. Not one on NAD83, which
    // PROJ identifies as EPSG:4269 by its nodes but places by the extension: (-157.8, 21.3)
    // stays there in WGS 84, where EPSG:4269 moves it by 0.5 m.
    let wgs84 = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],EXTENSION["PROJ4","+proj=longlat +datum=WGS84 +nadgrids=@null"]]"#;
    let utm = r#"PROJCS["WGS 84 / UTM zone 18N",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",-75],PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4","+proj=utm +zone=18 +datum=WGS84 +nadgrids=@null +units=m +no_defs"]]"#;
    let nad83 = r#"GEOGCS["NAD83",DATUM["North_American_Datum_1983",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6269"]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],EXTENSION["PROJ4","+proj=longlat +datum=NAD83 +nadgrids=@null"]]"#;
    for (w, code) in [(wgs84, Some(4326)), (utm, Some(32618)), (nad83, None)] {
        assert_eq!(SpatialRef::from_wkt(w).epsg, code, "{w}");
    }
    let q = at(&SpatialRef::from_wkt(nad83), "EPSG:4326", (-157.8, 21.3));
    assert!((q.0 + 157.8).abs() < 1e-9 && (q.1 - 21.3).abs() < 1e-9, "{q:?}");
    let shifted = SpatialRef::parse("+proj=longlat +ellps=WGS84 +nadgrids=@null +towgs84=1000,0,0").unwrap();
    let json = shifted.to_projjson().unwrap();
    assert!(!json.contains("Geocentric translations"), "{json}");
    let back = SpatialRef::from_projjson(&json).unwrap();
    for crs in [&shifted, &back] {
        let q = at(crs, "EPSG:4326", (10.0, 45.0));
        assert!((q.0 - 10.0).abs() < 1e-9 && (q.1 - 45.0).abs() < 1e-9, "{q:?}");
    }
}

/// GeoServer's Web Mercator (EPSG:900913, a sphere in its extension beside a WGS 84
/// `SPHEROID`) is written as Popular Visualisation Pseudo Mercator on WGS 84, as PROJ 9.9.0
/// writes it; and a WKT with an extension reports its own `SPHEROID`, as PROJ does, whatever
/// the extension computes on.
#[test]
fn an_extension_wkt_reports_and_writes_its_spheroid() {
    let google = r#"PROJCS["Google Maps Global Mercator",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563,AUTHORITY["EPSG","7030"]],AUTHORITY["EPSG","6326"]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Mercator_1SP"],PARAMETER["central_meridian",0],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4","+proj=merc +a=6378137 +b=6378137 +lat_ts=0.0 +lon_0=0.0 +x_0=0.0 +y_0=0 +k=1.0 +units=m +nadgrids=@null +wktext +no_defs"],AUTHORITY["EPSG","900913"]]"#;
    let json = SpatialRef::from_wkt(google).to_projjson().unwrap();
    assert!(json.contains("Popular Visualisation Pseudo Mercator"), "{json}");
    let with = |ext: &str| {
        let w = google.replace(
            "+proj=merc +a=6378137 +b=6378137 +lat_ts=0.0 +lon_0=0.0 +x_0=0.0 +y_0=0 +k=1.0 +units=m +nadgrids=@null +wktext +no_defs",
            ext,
        );
        SpatialRef::from_wkt(&w.replace(r#",AUTHORITY["EPSG","900913"]]"#, "]"))
    };
    // The same sphere given otherwise is EPSG:900913, identified as it converts, and written so.
    for ext in ["+proj=merc +R=6378137 +units=m", "+proj=merc +ellps=WGS84 +f=0 +units=m"] {
        let crs = with(ext);
        assert_eq!(crs.epsg, Some(900913), "{ext}");
        assert!(crs.to_projjson().unwrap().contains("Popular Visualisation Pseudo Mercator"), "{ext}");
    }
    // Not another sphere's Mercator, nor another standard parallel, which would be written as
    // Mercator on the WKT's WGS 84 (33 km off in northing at 50°N), as PROJ writes them.
    for ext in ["+proj=merc +R=6378137 +x_0=1000 +units=m", "+proj=merc +a=6378137 +b=6378137 +lat_ts=10d +units=m"] {
        assert!(with(ext).to_projjson().is_err(), "{ext}");
    }
    let wkt = "PROJCS[\"custom\",GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],PROJECTION[\"custom_proj4\"],UNIT[\"metre\",1],EXTENSION[\"PROJ4\",\"+proj=tmerc +lon_0=-75 +ellps=WGS84 +f=0\"]]";
    let params = SpatialRef::from_wkt(wkt).projection_params().unwrap();
    assert!((params.a - 6_378_137.0).abs() < 1e-6 && (params.b - 6_356_752.314_245_179).abs() < 1e-6, "{params:?}");
}

/// PROJJSON of a datum given no shift by `+nadgrids=@null`: refused where the datum's own
/// transformations shift points (OSGB36), as PROJ writes a bound CRS with the null grid, which
/// terrio's reader does not take, and the datum alone would be read back with them; written on
/// WGS 84, whose own are null, and read back with no shift. GeoKeys do not describe either.
#[test]
fn projjson_of_a_null_grid_beside_a_datum_round_trips_or_is_refused() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let osgb = SpatialRef::parse("+proj=longlat +datum=OSGB36 +nadgrids=@null").unwrap();
    assert!(osgb.to_projjson().is_err());
    assert!(terrio_crs::geokeys::user_defined_keys(&osgb, "custom").is_none());
    let wgs84 = SpatialRef::parse("+proj=longlat +datum=WGS84 +nadgrids=@null").unwrap();
    let back = SpatialRef::from_projjson(&wgs84.to_projjson().unwrap()).unwrap();
    let q = Transformer::new(&back, &SpatialRef::from_epsg(4326).unwrap()).unwrap().transform(-2.0, 52.0).unwrap();
    assert!((q.0 + 2.0).abs() < 1e-9 && (q.1 - 52.0).abs() < 1e-9, "{q:?}");
}

/// `+R` wins over `+a`: Mercator on a 6400 km sphere is not Web Mercator, and beside a WGS 84
/// `SPHEROID` its PROJJSON is refused, as PROJ 9.9.0 writes it as Mercator (variant A) on the
/// sphere of `+R` for the PROJ string and on WGS 84 for the WKT.
#[test]
fn a_mercator_sphere_of_another_radius_is_not_web_mercator() {
    let wkt = r#"PROJCS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Mercator_1SP"],PARAMETER["central_meridian",0],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4","+proj=merc +R=6400000 +a=6378137 +units=m"]]"#;
    assert!(SpatialRef::from_wkt(wkt).to_projjson().is_err());
    let json = SpatialRef::parse("+proj=merc +R=6400000 +a=6378137").unwrap().to_projjson().unwrap();
    assert!(!json.contains("Pseudo Mercator"), "{json}");
}

/// The projection engine's CRS of a WKT whose `PROJ4` extension terrio refuses (`+rf=0`) is
/// that of its nodes, as before extensions were read as pipeline steps: not a CRS terrio cannot
/// transform.
#[cfg(feature = "wbprojection-fallback")]
#[test]
fn a_refused_extension_is_not_handed_to_the_projection_engine() {
    let wkt = r#"PROJCS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",-75],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4","+proj=tmerc +lon_0=10 +ellps=WGS84 +rf=0"]]"#;
    let crs = SpatialRef::from_wkt(wkt).to_wbp().unwrap();
    assert!((crs.projection.params().lon0 + 75.0).abs() < 1e-9, "{}", crs.projection.params().lon0);
}

/// A WKT whose `PROJ4` extension the pipeline step reads on another figure than the projection
/// engine does (`+ellps=WGS84 +f=0` in a Transverse Mercator: a sphere to the step, WGS 84 to
/// the engine) has no projection-engine CRS, rather than one on another ellipsoid; the same on
/// its own figure has.
#[cfg(feature = "wbprojection-fallback")]
#[test]
fn an_extension_the_projection_engine_reads_on_another_figure_is_refused() {
    let wkt = |ext: &str| {
        format!(
            r#"PROJCS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",10],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4","{ext}"]]"#
        )
    };
    assert!(SpatialRef::from_wkt(&wkt("+proj=tmerc +lon_0=10 +x_0=1000 +ellps=WGS84 +f=0 +units=m")).to_wbp().is_err());
    let crs = SpatialRef::from_wkt(&wkt("+proj=tmerc +lon_0=10 +x_0=1000 +ellps=WGS84 +units=m")).to_wbp().unwrap();
    assert!((crs.projection.params().ellipsoid.a - 6378137.0).abs() < 1e-6);
}

/// The same holds for a WKT on Polar Stereographic (variant C) with a `PROJ4` extension, which
/// goes to the projection engine by its own path: refused on another figure, given on its own.
#[cfg(feature = "wbprojection-fallback")]
#[test]
fn a_variant_c_extension_the_projection_engine_reads_on_another_figure_is_refused() {
    let wkt = |ext: &str| {
        format!(
            r#"PROJCS["unknown",GEOGCS["unknown",DATUM["unknown",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Polar_Stereographic_(variant_C)"],PARAMETER["Latitude of standard parallel",-67],PARAMETER["Longitude of origin",140],PARAMETER["Easting at false origin",300000],PARAMETER["Northing at false origin",200000],UNIT["metre",1],EXTENSION["PROJ4","{ext}"]]"#
        )
    };
    let refused = wkt("+proj=stere +lat_0=-90 +lat_ts=-67 +lon_0=140 +x_0=300000 +y_0=200000 +ellps=WGS84 +f=0 +units=m");
    assert!(SpatialRef::from_wkt(&refused).to_wbp().is_err());
    let given = wkt("+proj=stere +lat_0=-90 +lat_ts=-67 +lon_0=140 +x_0=300000 +y_0=200000 +ellps=WGS84 +units=m");
    let crs = SpatialRef::from_wkt(&given).to_wbp().unwrap();
    assert!((crs.projection.params().ellipsoid.a - 6378137.0).abs() < 1e-6);
}
