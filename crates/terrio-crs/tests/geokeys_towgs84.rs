// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! A CRS with its own datum shift (a WKT's `TOWGS84`) is written to GeoKeys with it, not as the
//! code it is identified as alone, whose own transformations place it elsewhere
//! (opengeos/terrio#1012, item 62).

use terrio_crs::{SpatialRef, Transformer};

/// ESRI's British_National_Grid with a datum shift of its own, which has no `AUTHORITY`.
const ESRI_BNG: &str = r#"PROJCS["British_National_Grid",GEOGCS["GCS_OSGB_1936",DATUM["D_OSGB_1936",SPHEROID["Airy_1830",6377563.396,299.3249646],TOWGS84[375,-111,431,0,0,0,0]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",400000.0],PARAMETER["False_Northing",-100000.0],PARAMETER["Central_Meridian",-2.0],PARAMETER["Scale_Factor",0.9996012717],PARAMETER["Latitude_Of_Origin",49.0],UNIT["Meter",1.0]]"#;

/// PROJ 9.9.0's ESRI WKT 1 of WGS 84 / UTM zone 33N (EPSG:32633).
const UTM_33N: &str = r#"PROJCS["WGS_1984_UTM_Zone_33N",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",15.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;

/// The WGS 84 longitude and latitude of (400000, 300000) in a CRS.
fn wgs84(crs: &SpatialRef) -> (f64, f64) {
    Transformer::new(crs, &SpatialRef::from_epsg(4326).unwrap()).unwrap().transform(400000.0, 300000.0).unwrap()
}

/// The GeogTOWGS84GeoKey (2062) values of written GeoKeys.
fn towgs84(keys: &terrio_crs::geokeys::GeoKeyValues) -> Vec<f64> {
    keys.doubles.iter().filter(|(id, _)| *id == 2062).map(|(_, x)| *x).collect()
}

/// GDAL 3.10.3 (`gdal_create -a_srs` of the `.prj`) writes ESRI's BNG with `TOWGS84` as a
/// user-defined projected CRS (ProjectedCSTypeGeoKey 32767) with GeogTOWGS84GeoKey 375, -111, 431,
/// not as EPSG:27700. Read back, the keys place (400000, 300000) where the WKT does, and where PROJ
/// 9.9.0 places the WKT: (-2.0014439944404914, 52.59776558325506) (`Transformer.from_crs(
/// CRS.from_wkt(wkt), "EPSG:4326", always_xy=True)`), not where EPSG:27700 does
/// (-2.0014331155108924, 52.59779243083379), 3.1 m away.
#[test]
fn an_identified_code_with_its_own_shift_is_written_with_the_shift() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let crs = SpatialRef::from_wkt(ESRI_BNG);
    // Identified as GDAL identifies it (`gdalsrsinfo -e`).
    assert_eq!(crs.epsg, Some(27700));
    let keys = crs.to_geokeys().unwrap();
    assert!(keys.shorts.contains(&(3072, 32767)) && !keys.shorts.contains(&(3072, 27700)), "{:?}", keys.shorts);
    assert_eq!(&towgs84(&keys)[..3], [375.0, -111.0, 431.0]);
    let tags = keys.encode().unwrap();
    let back = terrio_crs::geokeys::read(&tags.directory, &tags.doubles, tags.ascii.as_bytes()).crs.unwrap();
    for (x, y) in [wgs84(&crs), wgs84(&back)] {
        assert!((x - -2.0014439944404914).abs() < 1e-9 && (y - 52.59776558325506).abs() < 1e-9, "({x}, {y})");
    }
    let (x, y) = wgs84(&SpatialRef::from_epsg(27700).unwrap());
    assert!((x - -2.0014331155108924).abs() < 1e-9 && (y - 52.59779243083379).abs() < 1e-9, "({x}, {y})");
}

/// A WKT that declares its code and has a `TOWGS84` (GDAL's WKT 1 of EPSG:27700 or 4277 with one
/// added) is written by GDAL 3.10.3 with its code and GeogTOWGS84GeoKey: ProjectedCSTypeGeoKey
/// 27700, or GeographicTypeGeoKey 4277, and 375, -111, 431. A registered CRS without a shift of
/// its own has no GeogTOWGS84GeoKey, the deprecated EPSG:3785 included, whose registry WKT holds a
/// `TOWGS84` its PROJ string has not.
#[test]
fn a_declared_code_with_its_own_shift_is_written_with_both() {
    for (code, key) in [(27700, 3072), (4277, 2048)] {
        let wkt = terrio_crs::epsg_wkt(code).unwrap().replacen(
            r#"AUTHORITY["EPSG","7001"]]"#,
            r#"AUTHORITY["EPSG","7001"]],TOWGS84[375,-111,431,0,0,0,0]"#,
            1,
        );
        assert!(wkt.contains("TOWGS84"), "{wkt}");
        let keys = SpatialRef::from_wkt(&wkt).to_geokeys().unwrap();
        assert!(keys.shorts.contains(&(key, code as u16)), "{code}: {:?}", keys.shorts);
        assert_eq!(&towgs84(&keys)[..3], [375.0, -111.0, 431.0], "{code}");
    }
    // A zero shift on WGS 84 is none: GDAL 3.10.3 writes ESRI's WGS_1984_UTM_Zone_33N with
    // `TOWGS84[0,0,0,0,0,0,0]` as ProjectedCSTypeGeoKey 32633.
    let utm = UTM_33N.replacen("298.257223563]", "298.257223563],TOWGS84[0,0,0,0,0,0,0]", 1);
    assert!(utm.contains("TOWGS84"), "{utm}");
    let keys = SpatialRef::from_wkt(&utm).to_geokeys().unwrap();
    assert!(keys.shorts.contains(&(3072, 32633)) && towgs84(&keys).is_empty(), "{:?} {:?}", keys.shorts, keys.doubles);
    for code in [27700, 4277, 3785] {
        let keys = SpatialRef::from_epsg(code).unwrap().to_geokeys().unwrap();
        assert!(towgs84(&keys).is_empty(), "{code}: {:?}", keys.doubles);
    }
}

/// A WKT 2 `BOUNDCRS` to WGS 84 whose transformation `+towgs84` only approximates (a
/// Molodensky-Badekas evaluation point, a full rotation matrix) is written as its WKT in an ESRI
/// PE citation, which keeps the exact transformation, not as GeogTOWGS84GeoKey nor as a code; one
/// it represents exactly (Molodensky-Badekas without an evaluation point) with GeogTOWGS84GeoKey
/// (cases of `wkt2_bound.json`).
#[test]
fn a_bound_crs_beyond_towgs84_is_written_as_its_wkt() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("data/wkt2_bound.json")).unwrap();
    let list = cases.as_array().or_else(|| cases["cases"].as_array()).unwrap();
    let named = |name: &str| list.iter().find(|c| c["name"] == name).unwrap()["wkt"].as_str().unwrap().to_string();
    for name in [
        "method EPSG:9636 about an evaluation point, to WGS 84",
        "method EPSG:1063 about an evaluation point, to WGS 84",
        "method by name 'Coordinate Frame rotation full matrix (geog2D) (3D)', 7 parameters, to WGS 84",
    ] {
        let keys = SpatialRef::from_wkt(&named(name)).to_geokeys().unwrap();
        assert!(towgs84(&keys).is_empty(), "{name}: {:?}", keys.doubles);
        assert!(keys.ascii.iter().any(|(id, t)| *id == 3073 && t.starts_with("ESRI PE String = BOUNDCRS")), "{name}: {:?}", keys.ascii);
    }
    let keys = SpatialRef::from_wkt(&named("method by name 'Molodensky-Badekas (CF geog2D domain)', 7 parameters, to WGS 84"))
        .to_geokeys()
        .unwrap();
    assert_eq!(towgs84(&keys).len(), 7, "{:?}", keys.doubles);
}

/// PDAL's test_utm16.las (PDAL 2.10.2 test data) states NAD83 / UTM zone 16N (EPSG:26916) with
/// GeogTOWGS84GeoKey 0, 0, 0, which terrio reads with its shift. PROJ 9.7.1 reads that WKT as a
/// bound CRS over NAD83 and takes it into NAD83 (EPSG:4269) by the inverse projection alone
/// (`Transformer.from_crs(CRS.from_wkt(wkt), "EPSG:4269", always_xy=True)` is "Inverse of UTM
/// zone 16N + axis order change (2D)"), placing (809327.68, 4324249.48) at
/// (-83.42759780586651, 39.01259906906862). Through WGS 84 instead, with the Ohio HPGN grid
/// (us_noaa_ohhpgn.tif) available, NAD83 to WGS 84 moved it 0.2 m.
#[test]
fn a_code_with_its_own_shift_goes_into_its_own_datum_without_it() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let directory: [u16; 36] = [
        1, 1, 0, 8, 1024, 0, 1, 1, 1025, 0, 1, 1, 1026, 34737, 21, 0, 2049, 34737, 6, 21, 2054, 0, 1, 9102, 2062, 34736, 3, 0, 3072, 0, 1,
        26916, 3076, 0, 1, 9001,
    ];
    let crs = SpatialRef::from_geokeys(&directory, &[0.0, 0.0, 0.0], b"NAD83 / UTM zone 16N|NAD83|\0").unwrap();
    assert!(crs.wkt.as_deref().is_some_and(|w| w.contains("TOWGS84[0,0,0]")), "{:?}", crs.wkt);
    let (x, y) = (809_327.68, 4_324_249.48);
    let nad83 = Transformer::new(&crs, &SpatialRef::from_epsg(4269).unwrap()).unwrap();
    assert!(nad83.operation_at(x, y, 0.0).unwrap().is_none(), "no datum transformation into NAD83");
    let (lon, lat) = nad83.transform(x, y).unwrap();
    assert!((lon + 83.42759780586651).abs() < 1e-11 && (lat - 39.01259906906862).abs() < 1e-11, "({lon}, {lat})");
    // Into WGS 84, the shift is still the bound's.
    let op = Transformer::new(&crs, &SpatialRef::from_epsg(4326).unwrap()).unwrap().operation_at(x, y, 0.0).unwrap().unwrap();
    assert_eq!(op.name, "Transformation to WGS 84");
}
