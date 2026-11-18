//! ESRI's vertical CRS (`VERTCS`) is read as a vertical CRS, as PROJ 9.9.0 reads it
//! (opengeos/terrio#1012, item 59): a code it declares is kept when its unit and direction are the
//! code's, its axis points down by `PARAMETER["Direction",-1]`, and it is the vertical CRS of a
//! `COMPD_CS` and of ESRI's compound CRS, a `GEOGCS` or `PROJCS` followed by a `VERTCS` (ESRI's
//! WKT of EPSG:9707). A `VERT_CS` after a `GEOGCS`, or a `VERTCS` after a WKT 2 root, is ignored,
//! as by PROJ.
//!
//! `data/vertical_esri.json` (`scripts/gen_vertical_esri.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`, no
//! grids installed) holds the type, code and axes PROJ reads, and for a compound CRS the point PROJ
//! places from EPSG:9707 (`always_xy`). A compound CRS has no code in terrio unless it declares
//! one, where PROJ identifies EPSG:9707 by its parts (`SpatialRef::from_wkt`). Tolerance: 1 mm
//! (1e-8° for a geographic CRS). A vertical CRS without a code is identified by its name, datum,
//! unit and direction, as by PROJ, for every EPSG vertical CRS ESRI writes.

use serde_json::Value;
use terrio_crs::{AxisOrder, SpatialRef, Transformer};

#[test]
fn an_esri_vertcs_is_a_vertical_crs() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/vertical_esri.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 14);
    let point: Vec<f64> = data["point"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    let from = SpatialRef::from_epsg(9707).unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        let axes: Vec<&str> = case["axes"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        match case["type"].as_str().unwrap() {
            "Vertical CRS" => {
                assert_eq!(crs.epsg.map(u64::from), case["epsg"].as_u64(), "{name}");
                assert_eq!(crs.axis_order(), AxisOrder::Other, "{name}");
                assert!(!crs.has_height(), "{name}");
            }
            "Compound CRS" => {
                assert_eq!(crs.epsg, None, "{name}");
                assert!(crs.has_height(), "{name}");
                assert_eq!(crs.axis_order(), AxisOrder::EastNorth, "{name}: {axes:?}");
                let p = case["proj"].as_array().unwrap();
                let (x, y, z) = Transformer::new(&from, &crs).and_then(|t| t.transform_3d(point[0], point[1], point[2])).unwrap();
                let (px, py, pz) = (p[0].as_f64().unwrap(), p[1].as_f64().unwrap(), p[2].as_f64().unwrap());
                let tolerance = if crs.is_geographic() { 1e-8 } else { 1e-3 };
                assert!(
                    (x - px).abs() < tolerance && (y - py).abs() < tolerance && (z - pz).abs() < 1e-3,
                    "{name}: ({x}, {y}, {z}), PROJ ({px}, {py}, {pz})"
                );
            }
            _ => {
                assert_eq!(crs.epsg.map(u64::from), case["epsg"].as_u64(), "{name}");
                assert!(!crs.has_height(), "{name}");
                assert_eq!(axes, ["north", "east"], "{name}");
                assert_eq!(crs.axis_order(), AxisOrder::NorthEast, "{name}");
            }
        }
    }
    // The compound CRS's parts are known, its vertical CRS declared or identified (but on its
    // axis down, which EGM96 height is not).
    for (name, parts) in [
        ("GEOGCS and VERTCS declaring EPSG:5773", Some((4326, 5773))),
        ("ESRI's WKT of EPSG:9707", Some((4326, 5773))),
        ("GEOGCS and VERTCS, Direction -1", None),
    ] {
        let case = cases.iter().find(|c| c["name"] == name).unwrap();
        assert_eq!(SpatialRef::from_wkt(case["wkt"].as_str().unwrap()).compound_parts(), parts, "{name}");
    }
}

/// A vertical CRS without a code is the EPSG CRS PROJ identifies by its name, datum, unit and
/// direction: every EPSG vertical CRS ESRI writes, in ESRI's WKT and in GDAL's WKT 1 without its
/// `AUTHORITY` (`identified` in `data/vertical_esri.json`).
#[test]
fn a_vertical_crs_is_identified_by_its_name_as_by_proj() {
    let data: Value = serde_json::from_str(include_str!("data/vertical_esri.json")).unwrap();
    let cases = data["identified"].as_array().unwrap();
    assert_eq!(cases.len(), 308);
    let mut differ = Vec::new();
    for case in cases {
        for kind in ["esri", "gdal"] {
            let got = SpatialRef::from_wkt(case[kind].as_str().unwrap()).epsg.map(u64::from);
            let want = case[format!("{kind}_epsg")].as_u64();
            if got != want {
                differ.push(format!("EPSG:{} {kind}: {got:?}, PROJ {want:?}", case["code"]));
            }
        }
    }
    assert!(differ.is_empty(), "{}", differ.join("\n"));
}

/// A name identifies a vertical CRS only on its datum, unit and direction, as for PROJ 9.9.0
/// (`CRS.from_wkt(wkt).to_epsg()`, pyproj 3.8.0): its name in any case, ESRI's or EPSG's, but not
/// on another datum, in feet, or down.
#[test]
fn a_name_identifies_a_vertical_crs_only_on_its_datum_unit_and_direction() {
    let esri =
        r#"VERTCS["EGM96_height",VDATUM["EGM96_Geoid"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#;
    let gdal = r#"VERT_CS["EGM96 height",VERT_DATUM["EGM96 geoid",2005],UNIT["metre",1],AXIS["Gravity-related height",UP]]"#;
    let navd_ft = r#"VERTCS["NAVD88_height_(ftUS)",VDATUM["North_American_Vertical_Datum_1988"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#;
    for (wkt, proj) in [
        (esri.to_string(), Some(5773)),
        (esri.replace("EGM96_height", "egm96_height"), Some(5773)),
        (esri.replace("EGM96_height", "EGM96 height"), Some(5773)),
        (esri.replace(r#"VDATUM["EGM96_Geoid"]"#, r#"VDATUM["Other_Datum"]"#), None),
        (esri.replace("EGM96_height", "Foo"), None),
        (esri.replace(r#""Direction",1.0"#, r#""Direction",-1.0"#), None),
        (esri.replace(r#"UNIT["Meter",1.0]"#, r#"UNIT["Foot_US",0.3048006096012192]"#), None),
        (navd_ft.to_string(), None),
        (gdal.to_string(), Some(5773)),
        (gdal.replace("EGM96 height", "egm96 HEIGHT"), Some(5773)),
        (gdal.replace("EGM96 height", "EGM96_height"), Some(5773)),
        (gdal.replace(r#"VERT_DATUM["EGM96 geoid""#, r#"VERT_DATUM["Other""#), None),
    ] {
        assert_eq!(SpatialRef::from_wkt(&wkt).epsg, proj, "{wkt}");
    }
}

/// ESRI's compound CRS is named after its parts, and written as a `COMPD_CS`.
#[test]
fn an_esri_compound_crs_is_a_compd_cs() {
    let g = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    let v = r#"VERTCS["EGM96_height",VDATUM["EGM96_Geoid"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#;
    let crs = SpatialRef::from_wkt(&format!("{g},{v}"));
    assert_eq!(crs.name().as_deref(), Some("GCS_WGS_1984 + EGM96_height"));
    let written = crs.to_wkt().unwrap();
    assert!(written.starts_with(r#"COMPD_CS["GCS_WGS_1984 + EGM96_height",GEOGCS["GCS_WGS_1984""#), "{written}");
    assert!(written.contains(r#"VERTCS["EGM96_height""#), "{written}");
}

/// A `SpatialRef` built from a file's text without `from_wkt` (as `terrio` builds a raster's or a
/// layer's) reads ESRI's compound CRS as compound too: `identify_epsg` and its definition.
#[test]
fn an_esri_compound_crs_is_compound_whatever_builds_it() {
    let g = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    let v = r#"VERTCS["EGM96_height",VDATUM["EGM96_Geoid"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#;
    let wkt = format!("{g},{v}");
    assert_eq!(terrio_crs::identify_epsg(&wkt), None);
    let raw = SpatialRef { wkt: Some(wkt.clone()), ..Default::default() };
    assert!(raw.has_height());
    assert_eq!(raw.compound_parts(), Some((4326, 5773)));
    // Too long once written as a `COMPD_CS` (its names twice): read by its first node, as before.
    let long = format!("{},{v}", g.replace("GCS_WGS_1984", &"x".repeat(60_000)));
    assert!(long.len() < 100_000);
    assert!(!SpatialRef::from_wkt(&long).has_height());
}

/// A compound CRS whose vertical CRS has geoid grids of its own (GDAL's `PROJ4_GRIDS`, in its
/// vertical datum) is not EPSG:9707 by its parts, though named EGM96 height: PROJ 9.9.0 reads it as
/// a bound vertical CRS of no code. Its parts are unknown, it is not the same CRS as EPSG:9707, and
/// its PROJJSON is refused rather than written as EGM96 height without its grids.
#[test]
fn own_geoid_grids_keep_a_compound_crs_from_its_parts_codes() {
    let wgs84 = SpatialRef::from_epsg(4326).unwrap().to_wkt().unwrap();
    let vertical = r#"VERT_CS["EGM96 height",VERT_DATUM["EGM96 geoid",2005,EXTENSION["PROJ4_GRIDS","custom.gtx"]],UNIT["metre",1],AXIS["Gravity-related height",UP]]"#;
    let crs = SpatialRef::from_wkt(&format!(r#"COMPD_CS["x",{wgs84},{vertical}]"#));
    assert_eq!(crs.compound_parts(), None);
    assert!(!crs.same_as(&SpatialRef::from_epsg(9707).unwrap()));
    assert!(crs.to_projjson().is_err());
    assert!(crs.to_proj4().is_some_and(|p| p.contains("+geoidgrids=custom.gtx")), "{:?}", crs.to_proj4());
    // Without the grids, it is EGM96 height's.
    let plain =
        SpatialRef::from_wkt(&format!(r#"COMPD_CS["x",{wgs84},{}]"#, vertical.replace(r#",EXTENSION["PROJ4_GRIDS","custom.gtx"]"#, "")));
    assert_eq!(plain.compound_parts(), Some((4326, 5773)));
}

/// Where GDAL's `PROJ4_GRIDS` extension of a vertical CRS counts, against PROJ 9.9.0 (pyproj
/// 3.8.0, `PROJ_NETWORK=OFF`): in its vertical datum (where GDAL writes it), it makes a bound CRS
/// of no code even with a declared `AUTHORITY`, and wins over one at its root; at its root, PROJ
/// ignores it, and so does terrio beside a declared code (without one, terrio reads it as before,
/// where PROJ identifies EGM96 height). PROJ's `to_epsg()` of the vertical CRS, and `to_proj4()`
/// of `COMPD_CS["x",GEOGCS["WGS 84",...,AUTHORITY["EPSG","4326"]],<vertical>]`.
#[test]
fn geoid_grids_by_their_place() {
    const WGS84: &str = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4326"]]"#;
    let wkt = |datum: &str, root: &str, declared: bool| {
        let grids = |g: &str| if g.is_empty() { String::new() } else { format!(r#",EXTENSION["PROJ4_GRIDS","{g}"]"#) };
        let id = if declared { r#",AUTHORITY["EPSG","5773"]"# } else { "" };
        format!(
            r#"VERT_CS["EGM96 height",VERT_DATUM["EGM96 geoid",2005{}],UNIT["metre",1]{},AXIS["Gravity-related height",UP]{id}]"#,
            grids(datum),
            grids(root)
        )
    };
    let with_grids = |g: &str| format!("+proj=longlat +datum=WGS84 +geoidgrids={g} +geoid_crs=WGS84 +vunits=m +no_defs +type=crs");
    let plain = "+proj=longlat +datum=WGS84 +vunits=m +no_defs +type=crs".to_string();
    // (datum grids, root grids, declared, code, compound PROJ string)
    let cases = [
        ("custom.gtx", "", false, None, with_grids("custom.gtx")),
        ("custom.gtx", "", true, None, with_grids("custom.gtx")),
        ("dat.gtx", "root.gtx", false, None, with_grids("dat.gtx")),
        ("", "custom.gtx", true, Some(5773), plain.clone()),
        // PROJ: 5773 and no grids.
        ("", "custom.gtx", false, None, with_grids("custom.gtx")),
        ("", "", false, Some(5773), plain),
    ];
    for (datum, root, declared, code, proj4) in cases {
        let v = wkt(datum, root, declared);
        assert_eq!(SpatialRef::from_wkt(&v).epsg, code, "{v}");
        let compound = SpatialRef::from_wkt(&format!(r#"COMPD_CS["x",{WGS84},{v}]"#));
        assert_eq!(compound.to_proj4(), Some(proj4), "{v}");
        assert_eq!(compound.compound_parts(), code.map(|c| (4326, c)), "{v}");
    }
}
