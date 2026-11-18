//! A WKT whose root `AUTHORITY` or `ID` is not the CRS its nodes describe is read by its nodes, as
//! PROJ 9.9.0 reads it: a geographic code on a projected CRS, a code that does not exist, a
//! compound code's units and axes on a 2D CRS in degrees, a geographic code on a geocentric CRS, a
//! code of another datum on the same ellipsoid. The declared code is kept only when the content is
//! that CRS; an unknown method stays unusable rather than read as the code. An Esri CRS's name
//! identifies its code only on its datum (ESRI's `D_` datum names included).
//!
//! `data/wkt_root_code.json` (`scripts/gen_wkt_root_code.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`, no
//! grids installed) holds PROJ's point in each CRS from WGS 84 (`always_xy`), `null` where PROJ
//! refuses, the code PROJ identifies, the code a case declares that is not its CRS, and
//! `"terrio": "refused"` for a case terrio refuses where PROJ reads it. Tolerance: 1 mm (1e-8° for
//! a geographic CRS).

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

fn cases() -> Vec<Value> {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/wkt_root_code.json")).unwrap();
    data["cases"].as_array().unwrap().clone()
}

/// The CRS of a case, and the point PROJ places from WGS 84 in it, by terrio.
fn read(case: &Value) -> (SpatialRef, terrio_crs::Result<(f64, f64)>) {
    let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
    let (lon, lat) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
    let got = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).and_then(|t| t.transform(lon, lat));
    (crs, got)
}

#[test]
fn a_wkt_is_read_by_its_nodes_not_its_root_code() {
    let cases = cases();
    assert_eq!(cases.len(), 35);
    for case in cases.iter().filter(|c| c["terrio"].is_null()) {
        let name = case["name"].as_str().unwrap();
        let (crs, got) = read(case);
        match case["proj"].as_array() {
            Some(p) => {
                let (x, y) = got.unwrap_or_else(|e| panic!("{name}: {e}"));
                let tolerance = if crs.is_geographic() { 1e-8 } else { 1e-3 };
                let (px, py) = (p[0].as_f64().unwrap(), p[1].as_f64().unwrap());
                assert!((x - px).abs() < tolerance && (y - py).abs() < tolerance, "{name}: ({x}, {y}), PROJ ({px}, {py})");
            }
            None => {
                assert!(got.is_err(), "{name}: {got:?}");
                assert_eq!(crs.to_proj4(), None, "{name}");
            }
        }
        // The code PROJ identifies, none for an Esri name on another datum, and never a declared
        // code that is not the CRS, nor in the PROJJSON written.
        // Where PROJ identifies none, the code terrio identifies by the content (`terrio_epsg`).
        let want = case["terrio_epsg"].as_u64().or(case["epsg"].as_u64());
        assert_eq!(crs.epsg.map(u64::from), want, "{name}");
        if let Some(code) = case["esri"].as_u64() {
            assert_eq!(crs.esri, Some(code as u32), "{name}");
        }
        if name.contains("renamed") {
            assert_eq!((crs.epsg, crs.esri), (None, None), "{name}");
        }
        if let Some(declared) = case["declared"].as_u64() {
            assert_ne!(crs.epsg, Some(declared as u32), "{name}");
            // The root's `id` (a bound CRS's source's), not an inner CRS's.
            if let Ok(json) = crs.to_projjson() {
                let v: Value = serde_json::from_str(&json).unwrap();
                let root = if v["type"] == "BoundCRS" { &v["source_crs"] } else { &v };
                assert_ne!(root["id"]["code"].as_u64(), Some(declared), "{name}: {json}");
            }
        }
    }
}

/// A method terrio's WKT 2 reader does not know (Belgian Lambert 72, opengeos/terrio#1012 item
/// 43) under a code that is not its CRS is refused, not read as that code; PROJ reads its content.
#[test]
fn an_unread_method_under_another_code_is_refused() {
    let cases = cases();
    let refused: Vec<&Value> = cases.iter().filter(|c| c["terrio"] == "refused").collect();
    assert_eq!(refused.len(), 1);
    for case in refused {
        let name = case["name"].as_str().unwrap();
        let (crs, got) = read(case);
        assert!(got.is_err(), "{name}: {got:?}");
        assert_eq!(crs.to_proj4(), None, "{name}");
        assert_ne!(crs.epsg, case["declared"].as_u64().map(|c| c as u32), "{name}");
    }
}

/// A compound CRS whose vertical CRS declares a code of another unit or direction (EGM96 height,
/// EPSG:5773, in US survey feet or as depths) is not written as that code: PROJJSON refuses it
/// rather than give its heights in metres up.
#[test]
fn a_vertical_crs_of_another_unit_is_not_written_as_its_code() {
    let cases = cases();
    for name in ["WGS 84 + EGM96 height in US survey feet declaring EPSG:9707", "WGS 84 + EGM96 depth declaring EPSG:9707"] {
        let case = cases.iter().find(|c| c["name"] == name).unwrap();
        let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        assert!(crs.to_projjson().is_err(), "{name}: {:?}", crs.to_projjson());
    }
    let own = cases.iter().find(|c| c["name"] == "WGS 84 + EGM96 height with its own code").unwrap();
    let json = SpatialRef::from_wkt(own["wkt"].as_str().unwrap()).to_projjson().unwrap();
    assert!(json.contains("\"code\":5773"), "{json}");
}

/// A compound CRS whose heights are in another unit than its vertical code's has no compound
/// parts to write as GeoKeys (EPSG:7405's ODN height in metres), as it has no compound code.
#[test]
fn a_vertical_crs_of_another_unit_has_no_compound_parts() {
    let cases = cases();
    let case = cases.iter().find(|c| c["name"] == "WGS 84 + EGM96 height in US survey feet declaring EPSG:9707").unwrap();
    assert_eq!(SpatialRef::from_wkt(case["wkt"].as_str().unwrap()).compound_parts(), None);
    let own = cases.iter().find(|c| c["name"] == "WGS 84 + EGM96 height with its own code").unwrap();
    assert_eq!(SpatialRef::from_wkt(own["wkt"].as_str().unwrap()).compound_parts(), Some((4326, 5773)));
}

/// A vertical CRS declaring a horizontal code is not that CRS: no code, and no PROJ string of
/// WGS 84 (PROJ 9.9.0 identifies no code either).
#[test]
fn a_vertical_crs_declaring_a_horizontal_code_is_not_it() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wkt = r#"VERT_CS["ODN height",VERT_DATUM["Ordnance Datum Newlyn",2005],UNIT["metre",1],AXIS["Gravity-related height",UP],AUTHORITY["EPSG","4326"]]"#;
    let crs = SpatialRef::from_wkt(wkt);
    assert_eq!(crs.epsg, None);
    assert_eq!(crs.to_proj4(), None);
}

/// The registry's own WKT of a CRS on the authalic sphere carries its PROJ string as an extension
/// (EPSG:3973 to 3975, and 9311 on NAD27): read back, it is its code, placed as the code is,
/// and its PROJ string is PROJ 9.9.0's (`projinfo -o PROJ`), with its datum.
#[test]
fn a_registry_wkt_with_its_proj_string_is_its_code() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for (code, proj4) in [
        (9311, "+proj=laea +R_A +lat_0=45 +lon_0=-100 +x_0=0 +y_0=0 +datum=NAD27 +units=m +no_defs +type=crs"),
        (3973, "+proj=laea +R_A +lat_0=90 +lon_0=0 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs +type=crs"),
        (3974, "+proj=laea +R_A +lat_0=-90 +lon_0=0 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs +type=crs"),
        (3975, "+proj=cea +R_A +lat_ts=30 +lon_0=0 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs +type=crs"),
    ] {
        let crs = SpatialRef::from_epsg(code).unwrap();
        assert_eq!(crs.to_proj4().as_deref(), Some(proj4), "EPSG:{code}");
        let want = Transformer::new(&wgs84, &crs).unwrap().transform(-100.0, 40.0).unwrap();
        for (form, w) in [("WKT", crs.to_wkt()), ("strict WKT 1", crs.to_wkt1_strict())] {
            let read = SpatialRef::from_wkt(&w.unwrap());
            assert_eq!(read.epsg, Some(code), "EPSG:{code}, {form}");
            assert_eq!(read.to_proj4().as_deref(), Some(proj4), "EPSG:{code}, {form}");
            let got = Transformer::new(&wgs84, &read).unwrap().transform(-100.0, 40.0).unwrap();
            assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "EPSG:{code}, {form}: {got:?}, {want:?}");
        }
    }
}

/// Into and from a vertical CRS without a transformation (its code dropped, or none), heights
/// are kept, as PROJ 9.9.0's ballpark vertical transformation keeps them, in the vertical CRS's
/// unit and direction; not moved by the horizontal datum shift (about 50 m for OSGB36). EPSG:7405
/// (British National Grid + ODN height) without codes, in US survey feet and as depths, from
/// EPSG:4979 at (-2°, 54°, 100 m); PROJ: 100, 328.0833 and -100, and 100 back.
#[test]
fn heights_into_a_vertical_crs_without_a_transformation_are_kept() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wgs84 = SpatialRef::from_epsg(4979).unwrap();
    let own = SpatialRef::from_epsg(7405).unwrap().to_wkt().unwrap();
    let swap = |w: &str, from: &str, to: &str| {
        assert!(w.contains(from), "{from}");
        w.replace(from, to)
    };
    let no_code = swap(&swap(&own, r#",AUTHORITY["EPSG","5701"]"#, ""), r#",AUTHORITY["EPSG","7405"]]"#, "]");
    let metre_up = r#"UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Gravity-related height",UP]"#;
    let feet =
        swap(&own, metre_up, r#"UNIT["US survey foot",0.304800609601219,AUTHORITY["EPSG","9003"]],AXIS["Gravity-related height",UP]"#);
    let depth = swap(&own, r#"AXIS["Gravity-related height",UP]"#, r#"AXIS["Gravity-related depth",DOWN]"#);
    for (name, wkt, height) in
        [("without codes", no_code, 100.0), ("in US survey feet", feet, 328.083_333_333_333_4), ("as depths", depth, -100.0)]
    {
        let crs = SpatialRef::from_wkt(&wkt);
        let (x, y, z) = Transformer::new(&wgs84, &crs).unwrap().transform_3d(-2.0, 54.0, 100.0).unwrap();
        assert!((z - height).abs() < 1e-6, "{name}: {z}");
        let (_, _, back) = Transformer::new(&crs, &wgs84).unwrap().transform_3d(x, y, z).unwrap();
        assert!((back - 100.0).abs() < 1e-6, "{name}: {back}");
    }
}

/// An unknown method declaring an ESRI code is not that CRS: no code, and refused, as PROJ 9.9.0
/// refuses it ("Input is not a transformation"); not placed as Mollweide (ESRI:54009).
#[test]
fn an_unknown_method_declaring_an_esri_code_is_refused() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let bng = SpatialRef::from_epsg(27700).unwrap().to_wkt().unwrap();
    let root = bng.rfind(",AUTHORITY[").unwrap();
    assert!(bng.contains(r#"PROJECTION["Transverse_Mercator"]"#));
    let wkt = format!("{},AUTHORITY[\"ESRI\",\"54009\"]]", bng[..root].replace("Transverse_Mercator", "Unheard_Of"));
    let crs = SpatialRef::from_wkt(&wkt);
    assert_eq!((crs.epsg, crs.esri), (None, None));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    assert!(Transformer::new(&wgs84, &crs).and_then(|t| t.transform(-2.0, 54.0)).is_err());
}

/// A method terrio cannot read keeps a declared ESRI code only when it is the ESRI CRS's own:
/// not one whose name merely begins with it, nor another EPSG method (Belgian Lambert, EPSG 9803,
/// declaring ESRI:54009, Mollweide on WGS 84).
#[test]
fn another_unread_method_declaring_an_esri_code_is_refused() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let own = SpatialRef::from_esri(54009).unwrap().to_wkt().unwrap();
    assert!(own.contains(r#"PROJECTION["Mollweide"]"#), "{own}");
    assert_eq!(SpatialRef::from_wkt(&own).esri, Some(54009));
    for method in [r#"PROJECTION["Mollweide_X"]"#, r#"PROJECTION["Lambert_Conformal_Conic_2SP_Belgium",AUTHORITY["EPSG","9803"]]"#] {
        let crs = SpatialRef::from_wkt(&own.replace(r#"PROJECTION["Mollweide"]"#, method));
        assert_eq!((crs.epsg, crs.esri), (None, None), "{method}");
    }
}

/// An ESRI CRS's WKT on a local datum, keeping its root ESRI code, is not that CRS: no ESRI code,
/// and no NAD83 transformations (ESRI:102003, USA Contiguous Albers, on NAD83).
#[test]
fn an_esri_code_on_a_local_datum_is_not_kept() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let own = SpatialRef::from_esri(102003).unwrap().to_wkt().unwrap();
    // Without its root code, then declaring ESRI:102003 at the root.
    let body = match own.rfind(",AUTHORITY[") {
        Some(i) if own[i..].matches(']').count() == 2 => own[..i].to_string(),
        _ => own.trim_end().strip_suffix(']').unwrap().to_string(),
    };
    let declare = |w: &str| format!("{w},AUTHORITY[\"ESRI\",\"102003\"]]");
    assert_eq!(SpatialRef::from_wkt(&declare(&body)).esri, Some(102003));
    for from in [r#"DATUM["North_American_Datum_1983""#, r#",AUTHORITY["EPSG","6269"]"#, r#",AUTHORITY["EPSG","4269"]"#] {
        assert!(body.contains(from), "{from}");
    }
    let local = body
        .replace(r#"DATUM["North_American_Datum_1983""#, r#"DATUM["My_Local""#)
        .replace(r#",AUTHORITY["EPSG","6269"]"#, "")
        .replace(r#",AUTHORITY["EPSG","4269"]"#, "");
    let crs = SpatialRef::from_wkt(&declare(&local));
    assert_eq!((crs.epsg, crs.esri), (None, None));
}

/// Heights of a vertical CRS whose geoid model's grid is missing are kept, though the horizontal
/// shift is three-dimensional (PROJ 9.9.0, no grids, network off: EPSG:7405, OSGB36 / British
/// National Grid + ODN height, to EPSG:4979 keeps 100 m at (530000, 180000)).
#[test]
fn heights_without_their_geoid_grid_are_kept() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let (src, dst) = (SpatialRef::from_epsg(7405).unwrap(), SpatialRef::from_epsg(4979).unwrap());
    let (_, _, h) = Transformer::new(&src, &dst).unwrap().transform_3d(530_000.0, 180_000.0, 100.0).unwrap();
    assert!((h - 100.0).abs() < 1e-6, "{h}");
}

/// A geographic CRS declaring its code on another prime meridian keeps the code's datum
/// transformations, after the meridian's shift (PROJ 9.9.0, no grids, network off: NTF (Paris)
/// on a meridian `Another` at 2.33722917 grads goes through "NTF to WGS 84 (1)", to
/// (1.4462680293, 46.7349985004), not a ballpark 56 m off).
#[test]
fn a_geographic_crs_on_another_meridian_keeps_its_datum() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wkt = r#"PROJCS["NTF (Paris) / Lambert zone II",GEOGCS["NTF (Paris)",DATUM["Nouvelle_Triangulation_Francaise_Paris",SPHEROID["Clarke 1880 (IGN)",6378249.2,293.466021293627,AUTHORITY["EPSG","7011"]],AUTHORITY["EPSG","6807"]],PRIMEM["Another",2.33722917,AUTHORITY["EPSG","8903"]],UNIT["grad",0.0157079632679489,AUTHORITY["EPSG","9105"]],AUTHORITY["EPSG","4807"]],PROJECTION["Lambert_Conformal_Conic_1SP"],PARAMETER["latitude_of_origin",52],PARAMETER["central_meridian",0],PARAMETER["scale_factor",0.99987742],PARAMETER["false_easting",600000],PARAMETER["false_northing",2200000],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH]]"#;
    let crs = SpatialRef::from_wkt(wkt);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let (lon, lat) = Transformer::new(&crs, &wgs84).unwrap().transform(549_826.63, 2_192_990.71).unwrap();
    assert!((lon - 1.446_268_029_3).abs() < 1e-8 && (lat - 46.734_998_500_4).abs() < 1e-8, "{lon} {lat}");
}

/// A geographic CRS written `geogcs` (which the WKT reader accepts) is compared with its declared
/// code as one written `GEOGCS`: a local datum declaring OSGB 1936 (EPSG:4277) does not get
/// OSGB36's transformations either way.
#[test]
fn a_lower_case_geogcs_is_compared_with_its_code() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let upper = r#"PROJCS["local",GEOGCS["OSGB 1936",DATUM["My_Local",SPHEROID["Airy 1830",6377563.396,299.3249646]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4277"]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",49],PARAMETER["central_meridian",-2],PARAMETER["scale_factor",0.9996012717],PARAMETER["false_easting",400000],PARAMETER["false_northing",-100000],UNIT["metre",1]]"#;
    let lower = upper.replace("GEOGCS[", "geogcs[");
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let at = |w: &str| Transformer::new(&wgs84, &SpatialRef::from_wkt(w)).unwrap().transform(-2.0, 54.0).unwrap();
    let bng = Transformer::new(&wgs84, &SpatialRef::from_epsg(27700).unwrap()).unwrap().transform(-2.0, 54.0).unwrap();
    let (u, l) = (at(upper), at(&lower));
    assert!((u.0 - l.0).abs() < 1e-6 && (u.1 - l.1).abs() < 1e-6, "{u:?} {l:?}");
    assert!((u.0 - bng.0).hypot(u.1 - bng.1) > 50.0, "{u:?} {bng:?}");
}

/// An unreadable spelling of the ESRI CRS's own method keeps its code only on the code's
/// ellipsoid (ESRI:102035's orthographic, written `Orthographic_Spherical`, on a 6400 km sphere is
/// not ESRI:102035, whose definition terrio would build).
#[test]
fn an_unread_esri_method_on_another_ellipsoid_is_refused() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let own = SpatialRef::from_esri(102035).unwrap().to_wkt().unwrap();
    assert!(own.contains(r#"PROJECTION["Orthographic"]"#), "{own}");
    let spelled = own.replace(r#"PROJECTION["Orthographic"]"#, r#"PROJECTION["Orthographic_Spherical"]"#);
    assert_eq!(SpatialRef::from_wkt(&spelled).esri, Some(102035), "{spelled}");
    let a = terrio_crs::semi_major_axis(&spelled).unwrap();
    let other = spelled.replacen(&format!("{a}"), "6400000", 1);
    assert_ne!(other, spelled);
    assert_eq!(SpatialRef::from_wkt(&other).esri, None, "{other}");
}
