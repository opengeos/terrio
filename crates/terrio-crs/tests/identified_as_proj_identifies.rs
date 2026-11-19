// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! A WKT is identified, and a declared code kept, as PROJ 9.9.0 identifies it (pyproj 3.8.0,
//! `PROJ_NETWORK=OFF`; `to_epsg(min_confidence=70)` and `list_authority()`): by one comparison
//! of its ellipsoid as PROJ's (`Ellipsoid::_isEquivalentTo`, 1e-10), and a compound CRS by its
//! parts (#1058 step 4, opengeos/terrio#1012 items 19, 47 and 48).

use terrio_crs::{SpatialRef, Transformer, identify_epsg};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

/// EPSG:6986 (IGD05/12, geographic 3D, on GRS 1980) in PROJ's WKT 2 without its code: GRS 1980 is
/// not WGS 84, so it is not EPSG:7138 (IGD05/12 on WGS 84), which PROJ matches at 25% only. PROJ
/// identifies no code and keeps the point from WGS 84, (34.34, 31.49), as terrio now does (it was
/// placed 0.3 m off, through EPSG:7138's transformations; #1012 item 19).
#[test]
fn grs_1980_is_not_wgs_84() {
    offline();
    let wkt = r#"GEOGCRS["IGD05/12",DATUM["Israeli Geodetic Datum 2005(2012)",ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,3],AXIS["geodetic latitude (Lat)",north,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["geodetic longitude (Lon)",east,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]],AXIS["ellipsoidal height (h)",up,ORDER[3],LENGTHUNIT["metre",1]],USAGE[SCOPE["Geodesy."],AREA["Israel - onshore and offshore."],BBOX[29.45,32.99,33.53,35.69]],REMARK["Replaces IGD05 (CRS code 6979)."]]"#;
    let crs = SpatialRef::from_wkt(wkt);
    assert_ne!(crs.epsg, Some(7138), "{crs:?}");
    let (x, y) = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).unwrap().transform(34.34, 31.49).unwrap();
    assert!((x - 34.34).abs() < 1e-8 && (y - 31.49).abs() < 1e-8, "({x}, {y})");
    // ESRI's WKT of EPSG:4640 (RRAF 1991, deprecated, on WGS 84) is not EPSG:4558 (on GRS 1980):
    // PROJ identifies none.
    let rraf = r#"GEOGCS["GCS_RRAF_1991",DATUM["D_RRAF_1991",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    assert_eq!(identify_epsg(rraf), None);
}

/// A declared ESRI code is kept only on the code's ellipsoid, all of it: ESRI:54009 (Mollweide)
/// declared on an ellipsoid of WGS 84's semi-major axis but an inverse flattening of 290 is not
/// ESRI:54009 (PROJ: 25%; 100% on WGS 84 itself). ESRI's WKT of ESRI:54003 (Miller, whose
/// definition is on WGS 84's authalic sphere) is ESRI:54003, as PROJ and GDAL 3.13.3 identify it
/// (100%).
#[test]
fn an_esri_code_is_its_whole_ellipsoids() {
    let mollweide = r#"PROJCS["World_Mollweide",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Mollweide"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",0.0],UNIT["Meter",1.0],AUTHORITY["ESRI","54009"]]"#;
    assert_eq!(SpatialRef::from_wkt(mollweide).esri, Some(54009));
    assert_eq!(SpatialRef::from_wkt(&mollweide.replace("298.257223563", "290.0")).esri, None);
    let miller = r#"PROJCS["World_Miller_Cylindrical",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Miller_Cylindrical"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",0.0],UNIT["Meter",1.0]]"#;
    assert_eq!(SpatialRef::from_wkt(miller).esri, Some(54003));
}

/// EPSG:7405's GDAL WKT 1 without its vertical and root `AUTHORITY` is EPSG:7405 by its parts
/// (OSGB36 / British National Grid and ODN height), as PROJ 9.9.0 identifies it (#1012 item 47),
/// and placed as the code. (PROJ places the code itself, from EPSG:4979 at (-2°, 54°, 100 m), at
/// (400097.945, 455991.260) without OSGM15's grid: its ballpark ranked first, which terrio does not
/// follow, for the code as for its WKT.)
#[test]
fn a_compound_crs_is_its_parts_code() {
    offline();
    let wkt = r#"COMPD_CS["OSGB36 / British National Grid + ODN height",PROJCS["OSGB36 / British National Grid",GEOGCS["OSGB36",DATUM["Ordnance_Survey_of_Great_Britain_1936",SPHEROID["Airy 1830",6377563.396,299.3249646,AUTHORITY["EPSG","7001"]],AUTHORITY["EPSG","6277"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4277"]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",49],PARAMETER["central_meridian",-2],PARAMETER["scale_factor",0.9996012717],PARAMETER["false_easting",400000],PARAMETER["false_northing",-100000],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],AUTHORITY["EPSG","27700"]],VERT_CS["ODN height",VERT_DATUM["Ordnance Datum Newlyn",2005,AUTHORITY["EPSG","5101"]],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Gravity-related height",UP]]]"#;
    assert_eq!(identify_epsg(wkt), Some(7405));
    let crs = SpatialRef::from_wkt(wkt);
    assert_eq!(crs.epsg, Some(7405));
    let from = SpatialRef::from_epsg(4979).unwrap();
    let place = |c: &SpatialRef| Transformer::new(&from, c).unwrap().transform_3d(-2.0, 54.0, 100.0).unwrap();
    assert_eq!(place(&crs), place(&SpatialRef::from_epsg(7405).unwrap()));
    // In feet, its heights are not EPSG:7405's.
    let feet = wkt.replace(
        r#"VERT_DATUM["Ordnance Datum Newlyn",2005,AUTHORITY["EPSG","5101"]],UNIT["metre",1,AUTHORITY["EPSG","9001"]]"#,
        r#"VERT_DATUM["Ordnance Datum Newlyn",2005,AUTHORITY["EPSG","5101"]],UNIT["foot",0.3048]"#,
    );
    assert_ne!(feet, wkt);
    assert_eq!(identify_epsg(&feet), None);
}

/// The PROJJSON of a compound CRS and of a vertical CRS is read back, as PROJ writes it: EPSG:7405
/// and EPSG:5701 (ODN height) are themselves again, by their parts and by name, as PROJ 9.9.0
/// identifies them (#1012 item 48).
#[test]
fn compound_and_vertical_projjson_read_back() {
    for code in [7405, 5701, 9707, 5498] {
        let json = SpatialRef::from_epsg(code).unwrap().to_projjson().unwrap();
        let back = SpatialRef::from_projjson(&json).unwrap_or_else(|e| panic!("EPSG:{code}: {e}\n{json}"));
        assert_eq!(back.epsg, Some(code), "{json}");
    }
    // `components` belongs to a compound CRS only; a bound CRS's source is horizontal.
    let geographic = SpatialRef::from_epsg(4326).unwrap().to_projjson().unwrap();
    assert!(SpatialRef::from_projjson(&geographic.replacen('{', r#"{"components":[1,2],"#, 1)).is_err());
    let compound = SpatialRef::from_epsg(7405).unwrap().to_projjson().unwrap();
    let bound = format!(
        r#"{{"type":"BoundCRS","source_crs":{compound},"target_crs":{geographic},"transformation":{{"name":"t","method":{{"name":"Geocentric translations (geog2D domain)","id":{{"authority":"EPSG","code":9603}}}},"parameters":[]}}}}"#
    );
    let error = SpatialRef::from_projjson(&bound).unwrap_err().to_string();
    assert!(error.contains("bound source must be a geographic or projected CRS"), "{error}");
    // A vertical CRS needs one axis, up or down, in a length unit.
    let json = SpatialRef::from_epsg(5701).unwrap().to_projjson().unwrap();
    assert!(SpatialRef::from_projjson(&json.replace("\"up\"", "\"north\"")).is_err());
    assert!(SpatialRef::from_projjson(&json.replace(r#""subtype":"vertical""#, r#""subtype":"Cartesian""#)).is_err());
}

/// Of a horizontal and a vertical CRS with a current and a deprecated compound CRS, the current
/// one, as PROJ 9.9.0 leaves deprecated CRSs out: NTF (Paris) / Lambert zone II and NGF-IGN69
/// height are EPSG:7411 (PROJ: 70%), not the deprecated 7412, under another name.
#[test]
fn a_compound_crs_is_its_parts_current_code() {
    let h = SpatialRef::from_epsg(27572).unwrap().to_wkt().unwrap();
    let v = SpatialRef::from_epsg(5719).unwrap().to_wkt().unwrap();
    assert_eq!(identify_epsg(&format!(r#"COMPD_CS["Lambert II and heights",{h},{v}]"#)), Some(7411));
    // Truncated or cut in a character, a compound CRS's WKT is no CRS, not a panic.
    for wkt in ["COMPD_CS[", "COMPD_CS(", "COMPD_CS[é"] {
        assert_eq!(identify_epsg(wkt), None);
        assert_eq!(SpatialRef::from_wkt(wkt).epsg, None);
    }
}
