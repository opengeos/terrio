//! An Esri datum name on an altered ellipsoid keeps the datum's transformation, applied on the
//! file's ellipsoid, as PROJ 9.8.1 applies it (#1012 item 51).

use terrio_crs::{SpatialRef, TransformOptions, Transformer};

/// A WKT, a point in it and PROJ's WGS 84 longitude and latitude for it.
type Case = (&'static str, (f64, f64), (f64, f64));

/// Esri's Monte_Mario_Italy_1 with its International 1924 axis changed from 6378388 to 6378300.
/// PROJ 9.8.1 (pyproj 3.8.0, network off, no grids) finds `D_Monte_Mario` by its name (EPSG:4265)
/// and applies "Monte Mario to WGS 84 (4)" on the file's ellipsoid: `Transformer.from_crs(
/// CRS(wkt), "EPSG:4326", always_xy=True)`. Within 1e-9°.
#[test]
fn an_esri_datum_on_an_altered_ellipsoid_keeps_its_transformation() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wkt = r#"PROJCS["Monte_Mario_Italy_1",GEOGCS["GCS_Monte_Mario",DATUM["D_Monte_Mario",SPHEROID["International_1924",6378300.0,297.0]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",1500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",9.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;
    let crs = SpatialRef::from_wkt(wkt);
    let t = Transformer::new(&crs, &SpatialRef::from_epsg(4326).unwrap()).unwrap();
    for ((x, y), (lon, lat)) in [
        ((1_514_000.0, 5_034_000.0), (9.178_713_232_542, 45.459_834_112_213)),
        ((1_514_881.490_594_152, 5_034_556.812_441_661), (9.190_004_725_089_445, 45.464_827_873_511_766)),
        ((1_500_000.0, 4_500_000.0), (8.999_644_403_324, 40.651_346_988_646)),
    ] {
        let (a, b) = t.transform(x, y).unwrap();
        assert!((a - lon).abs() < 1e-9 && (b - lat).abs() < 1e-9, "({x}, {y}): ({a}, {b}), PROJ ({lon}, {lat})");
    }
}

/// Esri GEOGCSs of other datums (PROJ 9.8.1's `WKT1_ESRI` of the EPSG CRS) with their axis 88 m
/// shorter, from the centre of the CRS's area to WGS 84, as PROJ 9.8.1 transforms them (network off,
/// no grids): each with its datum's transformation on the file's ellipsoid. Within 1e-9°.
#[test]
fn esri_datums_on_altered_ellipsoids_keep_their_transformations() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let cases: [Case; 3] = [
        // OSGB 1936 (EPSG:4277)
        (
            r#"GEOGCS["GCS_OSGB_1936",DATUM["D_OSGB_1936",SPHEROID["Airy_1830",6377475.396,299.3249646]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#,
            (-3.5, 55.379999999999995),
            (-3.501360763173231, 55.38000863995997),
        ),
        // ED50 (EPSG:4230)
        (
            r#"GEOGCS["GCS_European_1950",DATUM["D_European_1950",SPHEROID["International_1924",6378300.0,297.0]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#,
            (16.255, 55.22),
            (16.253934538597814, 55.219432142594655),
        ),
        // Tokyo (EPSG:4301)
        (
            r#"GEOGCS["GCS_Tokyo",DATUM["D_Tokyo",SPHEROID["Bessel_1841",6377309.155,299.1528128]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#,
            (138.44, 32.955),
            (138.4369785972462, 32.95850950985113),
        ),
    ];
    for (wkt, (x, y), (lon, lat)) in cases {
        let t = Transformer::new(&SpatialRef::from_wkt(wkt), &wgs84).unwrap();
        let (a, b) = t.transform(x, y).unwrap();
        assert!((a - lon).abs() < 1e-9 && (b - lat).abs() < 1e-9, "{wkt}: ({a}, {b}), PROJ ({lon}, {lat})");
    }
}

/// A datum named with spaces (its official name, or an EPSG alias, as PROJ's own WKT writes them)
/// on an altered ellipsoid is not found by its name: PROJ 9.8.1 (pyproj 3.8.0, network off, no
/// grids) places it by a ballpark, the point unchanged to WGS 84. With underscores (`Monte_Mario`,
/// `OSGB_1936`) or a `D_` prefix, it is found, and the point moves.
#[test]
fn a_datum_on_an_altered_ellipsoid_is_found_by_its_name_only_as_proj_finds_it() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let moved = |datum: &str, spheroid: &str, (lon, lat): (f64, f64)| {
        let wkt = format!(r#"GEOGCS["x",DATUM["{datum}",SPHEROID[{spheroid}]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#);
        let (x, y) = Transformer::new(&SpatialRef::from_wkt(&wkt), &wgs84).unwrap().transform(lon, lat).unwrap();
        (x - lon).abs() + (y - lat).abs() > 1e-7
    };
    let mario = (r#""International 1924",6378300,297"#, (12.5, 41.9));
    let osgb = (r#""Airy 1830",6377475.396,299.3249646"#, (-2.0, 53.0));
    for (datum, (spheroid, at), proj_moves) in [
        ("D_Monte_Mario", mario, true),
        ("Monte_Mario", mario, true),
        ("Monte Mario", mario, false),
        ("OSGB_1936", osgb, true),
        ("D_OSGB36", osgb, true),
        ("D_Monte Mario", mario, true),
        ("OSGB36", osgb, false),
        ("Ordnance Survey of Great Britain 1936", osgb, false),
        ("European_1950", (r#""International 1924",6378300,297"#, (10.0, 50.0)), false),
    ] {
        assert_eq!(moved(datum, spheroid, at), proj_moves, "{datum}");
    }
}

/// A datum found by its name keeps its operations whose grids are missing for `ONLY_BEST`: Esri
/// OSGB 1936 on an Airy ellipsoid 88 m shorter, with no grids, is refused under `only_best` (its
/// best operation needs OSTN15), as PROJ 9.8.1 refuses it ("No operation matching criteria
/// found"); without it, PROJ's Helmert gives (-2.0014488760169202, 53.000310567922426).
#[test]
fn a_datum_found_by_its_name_keeps_its_missing_grids_for_only_best() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wkt = r#"GEOGCS["GCS_OSGB_1936",DATUM["D_OSGB_1936",SPHEROID["Airy_1830",6377475.396,299.3249646]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    let (crs, wgs84) = (SpatialRef::from_wkt(wkt), SpatialRef::from_epsg(4326).unwrap());
    let (lon, lat) = Transformer::new(&crs, &wgs84).unwrap().transform(-2.0, 53.0).unwrap();
    assert!((lon - -2.001_448_876_016_920_2).abs() < 1e-9 && (lat - 53.000_310_567_922_426).abs() < 1e-9, "({lon}, {lat})");
    let strict = Transformer::with_options(&crs, &wgs84, &TransformOptions { only_best: true, ..Default::default() });
    assert!(strict.and_then(|t| t.transform(-2.0, 53.0)).is_err());
}
