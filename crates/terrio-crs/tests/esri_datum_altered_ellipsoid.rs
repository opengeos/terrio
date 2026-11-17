//! An Esri datum name on an altered ellipsoid keeps the datum's transformation, applied on the
//! file's ellipsoid, as PROJ 9.9.0 applies it (#1012 item 51).

use terrio_crs::{SpatialRef, TransformOptions, Transformer};

/// A WKT, a point in it and PROJ's WGS 84 longitude and latitude for it.
type Case = (&'static str, (f64, f64), (f64, f64));

/// Esri's Monte_Mario_Italy_1 with its International 1924 axis changed from 6378388 to 6378300.
/// PROJ 9.9.0 (pyproj 3.8.0, network off, no grids) finds `D_Monte_Mario` by its name (EPSG:4265)
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

/// Esri GEOGCSs of other datums (PROJ 9.9.0's `WKT1_ESRI` of the EPSG CRS) with their axis 88 m
/// shorter, from the centre of the CRS's area to WGS 84, as PROJ 9.9.0 transforms them (network off,
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
/// on an altered ellipsoid is not found by its name: PROJ 9.9.0 (pyproj 3.8.0, network off, no
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
/// best operation needs OSTN15), as PROJ 9.9.0 refuses it ("No operation matching criteria
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

/// A `BOUNDCRS` whose target names a datum PROJ finds by its name (`European_Datum_1950`) with
/// its own `TOWGS84`, under a source on another ellipsoid (Bessel), does not give the source the
/// target's datum transformations: PROJ 9.9.0 (pyproj 3.8.0, network off, no grids) cannot go on
/// through such a target and places (10, 50) by "Ballpark geographic offset from Local to WGS 84",
/// unchanged. Reported by RustGIS.
#[test]
fn a_bound_crs_target_named_as_a_datum_is_not_its_source_datum() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wkt = r#"BOUNDCRS[SOURCECRS[GEOGCRS["Local",DATUM["Local datum",ELLIPSOID["Bessel 1841",6377397.155,299.1528128,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["lat",north],AXIS["lon",east],ANGLEUNIT["degree",0.0174532925199433]]],TARGETCRS[GEOGCS["ED50",DATUM["European_Datum_1950",SPHEROID["International 1924",6378388,297],TOWGS84[-80,-90,-110,0,0,0,0]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4230"]]],ABRIDGEDTRANSFORMATION["t",METHOD["Position Vector transformation (geog2D domain)",ID["EPSG",9606]],PARAMETER["X-axis translation",598.1,ID["EPSG",8605]],PARAMETER["Y-axis translation",73.7,ID["EPSG",8606]],PARAMETER["Z-axis translation",418.2,ID["EPSG",8607]],PARAMETER["X-axis rotation",0.202,ID["EPSG",8608]],PARAMETER["Y-axis rotation",0.045,ID["EPSG",8609]],PARAMETER["Z-axis rotation",-2.455,ID["EPSG",8610]],PARAMETER["Scale difference",6.7,SCALEUNIT["parts per million",1E-06],ID["EPSG",8611]]]]"#;
    let t = Transformer::new(&SpatialRef::from_wkt(wkt), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let (lon, lat) = t.transform(10.0, 50.0).unwrap();
    assert!((lon - 10.0).abs() < 1e-9 && (lat - 50.0).abs() < 1e-9, "({lon}, {lat})");
}

/// The same with a target named as a datum on the source's own ellipsoid (`D_Monte_Mario` on
/// Bessel, with its own `TOWGS84`): PROJ 9.9.0 places (12, 42) by a ballpark, unchanged, not by
/// Monte Mario's transformation on Bessel (about 230 m away).
#[test]
fn a_bound_crs_target_on_the_source_ellipsoid_is_not_its_source_datum() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wkt = r#"BOUNDCRS[SOURCECRS[GEOGCRS["Local",DATUM["Local datum",ELLIPSOID["Bessel 1841",6377397.155,299.1528128,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["lat",north],AXIS["lon",east],ANGLEUNIT["degree",0.0174532925199433]]],TARGETCRS[GEOGCS["MM",DATUM["D_Monte_Mario",SPHEROID["Bessel 1841",6377397.155,299.1528128],TOWGS84[-80,-90,-110,0,0,0,0]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]],ABRIDGEDTRANSFORMATION["t",METHOD["Position Vector transformation (geog2D domain)",ID["EPSG",9606]],PARAMETER["X-axis translation",598.1,ID["EPSG",8605]],PARAMETER["Y-axis translation",73.7,ID["EPSG",8606]],PARAMETER["Z-axis translation",418.2,ID["EPSG",8607]],PARAMETER["X-axis rotation",0.202,ID["EPSG",8608]],PARAMETER["Y-axis rotation",0.045,ID["EPSG",8609]],PARAMETER["Z-axis rotation",-2.455,ID["EPSG",8610]],PARAMETER["Scale difference",6.7,SCALEUNIT["parts per million",1E-06],ID["EPSG",8611]]]]"#;
    let t = Transformer::new(&SpatialRef::from_wkt(wkt), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let (lon, lat) = t.transform(12.0, 42.0).unwrap();
    assert!((lon - 12.0).abs() < 1e-9 && (lat - 42.0).abs() < 1e-9, "({lon}, {lat})");
}

/// Esri's Mercator_Auxiliary_Sphere, which projects on a sphere, on `D_Monte_Mario` with an
/// altered International 1924 axis (6378390): the datum's transformation on the geographic CRS's
/// ellipsoid, not the projection's sphere. PROJ 9.9.0 places WGS 84 (12, 42) at
/// (1335909.7424683373, 5161087.942280648). Within 1 mm.
#[test]
fn an_auxiliary_sphere_on_an_altered_datum_keeps_its_transformation() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wkt = r#"PROJCS["x",GEOGCS["GCS_Monte_Mario",DATUM["D_Monte_Mario",SPHEROID["International_1924",6378390.0,297.0]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Mercator_Auxiliary_Sphere"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",0.0],PARAMETER["Standard_Parallel_1",0.0],PARAMETER["Auxiliary_Sphere_Type",0.0],UNIT["Meter",1.0]]"#;
    let t = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &SpatialRef::from_wkt(wkt)).unwrap();
    let (x, y) = t.transform(12.0, 42.0).unwrap();
    assert!((x - 1_335_909.742_468_337_3).abs() < 1e-3 && (y - 5_161_087.942_280_648).abs() < 1e-3, "({x}, {y})");
}

/// A datum whose transformation to WGS 84 is a null one (TWD97, RDN2008, IGM95, MAGNA-SIRGAS),
/// named by an Esri GEOGCS on another ellipsoid: PROJ 9.9.0 applies "<datum> to WGS 84 (1)"
/// geocentrically, from the file's ellipsoid to WGS 84's, which moves latitudes (70 m for TWD97
/// on International 1924). On the datum's own ellipsoid the point is kept. `Transformer.from_crs(
/// CRS(wkt), "EPSG:4326", always_xy=True)`, pyproj 3.8.0, network off. Reported by RustGIS.
#[test]
fn a_null_transformation_on_another_ellipsoid_is_applied_geocentrically() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for (datum, spheroid, (x, y), (lon, lat), name) in [
        ("D_TWD_1997", "6378388.0,297.0", (121.5, 25.0), (121.5, 24.999_369_532_669_89), "TWD97 to WGS 84 (1)"),
        (
            "D_Rete_Dinamica_Nazionale_2008",
            "6378000.0,298.257223563",
            (12.5, 41.9),
            (12.5, 41.900_004_110_644_24),
            "ETRS89-ITA [RDN2008] to WGS 84 (1)",
        ), // EPSG v13.102's name (PROJ 9.9.0)
        ("D_IGM_1995", "6378000.0,298.257223563", (12.5, 41.9), (12.5, 41.900_004_110_644_24), "ETRS89-ITA [IGM95] to WGS 84 (1)"), // EPSG v13.102's name (PROJ 9.9.0)
        ("D_MAGNA", "6378388.0,297.0", (-74.1, 4.6), (-74.1, 4.599_868_339_200_481), "MAGNA-SIRGAS to WGS 84 (1)"),
        ("D_TWD_1997", "6378137.0,298.257222101", (121.5, 25.0), (121.5, 25.0), "TWD97 to WGS 84 (1)"),
    ] {
        let wkt = format!(
            r#"GEOGCS["GCS_X",DATUM["{datum}",SPHEROID["S",{spheroid}]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#
        );
        let t = Transformer::new(&SpatialRef::from_wkt(&wkt), &wgs84).unwrap();
        let (a, b) = t.transform(x, y).unwrap();
        assert!((a - lon).abs() < 1e-9 && (b - lat).abs() < 1e-9, "{datum} on {spheroid}: ({a}, {b}), PROJ ({lon}, {lat})");
        let op = t.operation_at(x, y, 0.0).unwrap().unwrap();
        assert_eq!((op.name.as_str(), op.ballpark), (name, false), "{datum} on {spheroid}");
        // Between 2D CRSs, a height is kept, as PROJ keeps it.
        assert_eq!(t.transform_3d(x, y, 100.0).unwrap().2, 100.0, "{datum} on {spheroid}");
    }
}

/// Where that stops, on GRS 1980 with its axis 88 m shorter: a concatenation of null
/// transformations between geographic CRSs (ETRF89 through ETRS89) is applied geocentrically too.
/// One registered between geocentric CRSs (NAD83(CSRS)v6 through NAD83(2011)) keeps the point, as
/// PROJ 9.9.0 puts a ballpark before it. A datum name PROJ finds wins over the datum's ID:
/// `Greenland_1996` with GR96's ID (EPSG:1421) is the former datum EPSG:6747, which no current CRS
/// has, so the point is kept by a ballpark; `D_GR96`, and an unknown name with that ID, are GR96
/// and its null transformation. Values as above. PROJ reports
/// the NAD83(CSRS)v6 case as a ballpark (its first step), terrio by the registered operation
/// alone: not compared (opengeos/terrio#1012 item 56).
#[test]
fn null_transformations_on_another_ellipsoid_stop_where_proj_stops() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let geogcs = |name: &str, datum: &str| {
        format!(
            r#"GEOGCS["{name}",DATUM["{datum}",SPHEROID["GRS_1980",6378049.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#
        )
    };
    let with_id = |datum: &str| {
        format!(
            r#"GEOGCS["X",DATUM["{datum}",SPHEROID["S",6378049.0,298.257222101],AUTHORITY["EPSG","1421"]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#
        )
    };
    for (wkt, (x, y), (lon, lat), ballpark) in [
        (geogcs("GCS_ETRF_1989", "D_ETRF_1989"), (10.955, 58.995), (10.955, 58.995_002_339_885_04), Some(false)),
        (geogcs("NAD83(CSRS)v6", "North_American_Datum_of_1983_(CSRS)_version_6"), (-90.87, 62.335), (-90.87, 62.335), None),
        (geogcs("GCS_Greenland_1996", "D_Greenland_1996"), (-33.505, 71.7), (-33.505, 71.7), Some(true)),
        (with_id("Greenland_1996"), (-33.505, 71.7), (-33.505, 71.7), Some(true)),
        (geogcs("GCS_GR96", "D_GR96"), (-33.505, 71.7), (-33.505, 71.700_001_578_126_53), Some(false)),
        (with_id("Foo"), (-33.505, 71.7), (-33.505, 71.700_001_578_126_53), Some(false)),
    ] {
        let t = Transformer::new(&SpatialRef::from_wkt(&wkt), &wgs84).unwrap();
        let (a, b) = t.transform(x, y).unwrap();
        assert!((a - lon).abs() < 1e-9 && (b - lat).abs() < 1e-9, "{wkt}: ({a}, {b}), PROJ ({lon}, {lat})");
        if let Some(ballpark) = ballpark {
            assert_eq!(t.operation_at(x, y, 0.0).unwrap().unwrap().ballpark, ballpark, "{wkt}");
        }
    }
}
