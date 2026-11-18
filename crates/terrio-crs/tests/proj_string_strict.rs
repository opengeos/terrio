//! PROJ strings read as PROJ 9.9.0 reads them (opengeos/terrio#1012 items 36 and 37): angles as
//! its `dmstor` reads them, and names it does not know refused rather than read as a default.
//! References from pyproj 3.8.0 (PROJ 9.9.0, network off): `Transformer.from_crs("EPSG:4326",
//! CRS(s), always_xy=True).transform(10, 45, errcheck=True)`, or the CRS refused. Tolerance 1 mm.

use terrio_crs::{SpatialRef, Transformer};

fn place(proj: &str) -> Option<(f64, f64)> {
    let crs = SpatialRef::from_proj4(proj).ok()?;
    Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).ok()?.transform(10.0, 45.0).ok()
}

fn assert_places(proj: &str, expected: Option<(f64, f64)>) {
    match (place(proj), expected) {
        (Some((x, y)), Some((px, py))) => assert!((x - px).abs() < 1e-3 && (y - py).abs() < 1e-3, "{proj}: ({x}, {y}), PROJ ({px}, {py})"),
        (got, expected) => assert_eq!(got.is_some(), expected.is_some(), "{proj}: {got:?}, PROJ {expected:?}"),
    }
}

/// Angles: `D` and `°` for degrees, minutes and seconds with or without their units, an exponent,
/// radians with a hemisphere letter, and the letter's sign over a leading one; trailing text that
/// is not part of the angle refuses the CRS, for `+lat_0` as for `+lat_1`.
#[test]
fn angles_are_read_as_proj_reads_them() {
    let north = Some((78_846.842, 500_382.189));
    let south = Some((78_846.842, 9_470_479.692));
    let cases: [(&str, Option<(f64, f64)>); 27] = [
        ("40.5", north),
        ("40d30", north),
        ("40D30", north),
        ("40d30'N", north),
        ("40D30'N", north),
        ("40\u{b0}30'", north),
        ("40\u{b0}30'N", north),
        ("40d30'0\"N", north),
        ("0.7068583470577035r", north),
        ("0.7068583470577035R", north),
        ("0.7068583470577035rN", north),
        ("0.7068583470577035rS", south),
        ("0.7068583470577035rW", south),
        ("40.5S", south),
        ("-40.5S", south),
        ("+40.5", north),
        ("40.5d", north),
        ("40d70", Some((78_846.842, 426_348.396))),
        ("40d30'70", Some((78_846.842, 498_222.992))),
        ("1e1", Some((78_846.842, 3_879_576.107))),
        ("40d30'15.5\"S", Some((78_846.842, 9_470_957.799))),
        ("9d-30", None),
        ("40d30x", None),
        ("40.5NS", None),
        ("40.5.2", None),
        ("nan", None),
        ("inf", None),
    ];
    for (angle, expected) in cases {
        assert_places(&format!("+proj=tmerc +lat_0={angle} +lon_0=9 +ellps=GRS80"), expected);
    }
    // A strict parameter, as PROJ reads it the same way.
    assert_places("+proj=lcc +lat_1=40D30'N +lat_2=60 +lat_0=30 +lon_0=9 +ellps=GRS80", Some((78_060.521, 1_685_663.98)));
    assert_places("+proj=lcc +lat_1=9d-30 +lat_2=60 +lat_0=30 +lon_0=9 +ellps=GRS80", None);
}

/// Names PROJ does not know, as written: an ellipsoid, a datum, horizontal or vertical units, and
/// a vertical unit size that is not a positive number. Each refuses the CRS; the names PROJ knows
/// place the point.
#[test]
fn names_proj_does_not_know_are_refused() {
    let tmerc = "+proj=tmerc +lat_0=0 +lon_0=9 +k=0.9996 +x_0=500000";
    let utm = Some((578_815.303, 4_983_436.768));
    for (rest, expected) in [
        ("+ellps=GRS80", utm),
        ("+ellps=foo", None),
        ("+datum=WGS84", utm),
        ("+datum=foo", None),
        ("+ellps=GRS80 +datum=foo", None),
        ("+ellps=GRS80 +units=m", utm),
        ("+ellps=GRS80 +units=ft", Some((1_899_000.338, 16_349_858.163))),
        ("+ellps=GRS80 +units=us-ft", Some((1_898_996.54, 16_349_825.464))),
        ("+ellps=GRS80 +units=feet", None),
        ("+ellps=GRS80 +units=meter", None),
        ("+ellps=GRS80 +units=metre", None),
        ("+ellps=GRS80 +units=M", None),
        ("+ellps=GRS80 +units=foo +to_meter=2", None),
        ("+ellps=GRS80 +vunits=ft", utm),
        ("+ellps=GRS80 +vunits=foo", None),
        ("+ellps=GRS80 +vto_meter=0.3048", utm),
        ("+ellps=GRS80 +vto_meter=abc", None),
        ("+ellps=GRS80 +vto_meter=-1", None),
        // A fraction is a pipeline step's (a WKT's `PROJ4` extension), not a CRS's.
        ("+ellps=GRS80 +vto_meter=1200/3937", None),
    ] {
        assert_places(&format!("{tmerc} {rest}"), expected);
    }
    // A false easting or northing that is not a finite number.
    for value in ["nan", "inf"] {
        assert_places(&format!("+proj=tmerc +lat_0=0 +lon_0=9 +x_0={value} +ellps=GRS80"), None);
        assert_places(&format!("+proj=tmerc +lat_0=0 +lon_0=9 +y_0={value} +ellps=GRS80"), None);
    }
    for proj in [
        "+proj=longlat +ellps=foo",
        "+proj=longlat +ellps=wgs84",
        "+proj=longlat +datum=foo +ellps=WGS84",
        "+proj=longlat +ellps=intl +units=foo",
        "+proj=geocent +ellps=foo",
    ] {
        assert!(SpatialRef::from_proj4(proj).is_err(), "{proj}");
    }
}

/// A WKT's `PROJ4` extension is read as PROJ 9.9.0 reads it, as a pipeline step: values it cannot
/// read are not refused (`+lat_0=abc` is 0, `+x_0=abc` 0), and an angle is read by its longest
/// readable start (`9d-30` is 9°, `40.5NS` 40.5°). Names PROJ does not know are refused, as by PROJ.
/// PROJ's points from WGS 84 (10, 45).
#[test]
fn a_wkt_proj4_extension_is_read_as_a_pipeline_step() {
    let wkt = |ext: &str| {
        format!(
            r#"PROJCS["x",GEOGCS["GRS 1980",DATUM["unknown",SPHEROID["GRS80",6378137,298.257222101]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",9],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4","{ext}"]]"#
        )
    };
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for (ext, (px, py)) in [
        ("+proj=tmerc +lat_0=abc +lon_0=9 +ellps=GRS80", (78_846.841_654_020_22, 4_985_430.940_605_843)),
        ("+proj=tmerc +lat_0=9d-30 +lon_0=9 +ellps=GRS80", (78_846.841_654_020_22, 3_990_180.663_242_629)),
        ("+proj=tmerc +lat_0=0 +lon_0=40.5NS +ellps=GRS80", (-2_399_800.421_768_027_4, 5_459_095.433_999_421)),
        ("+proj=tmerc +lat_0=0 +lon_0=9 +x_0=abc +ellps=GRS80", (78_846.841_654_020_22, 4_985_430.940_605_843)),
        // A second point ends the number (40.5); a fraction is a vertical unit's size.
        ("+proj=tmerc +lat_0=40.5.2 +lon_0=9 +ellps=GRS80", (78_846.841_654_020_22, 500_382.189)),
        ("+proj=tmerc +lat_0=0 +lon_0=9 +ellps=GRS80 +vto_meter=1200/3937", (78_846.841_654_020_22, 4_985_430.940_605_843)),
        // An unreadable number ends the angle, which keeps what was read before it.
        ("+proj=tmerc +lat_0=40d. +lon_0=9 +ellps=GRS80", (78_846.841_654_020_22, 555_901.910_369_254_4)),
        ("+proj=tmerc +lat_0=40d30'. +lon_0=9 +ellps=GRS80", (78_846.841_654_020_22, 500_382.189_403_225)),
        ("+proj=tmerc +lat_0=. +lon_0=9 +ellps=GRS80", (78_846.841_654_020_22, 4_985_430.940_605_843)),
    ] {
        let (x, y) = Transformer::new(&wgs84, &SpatialRef::from_wkt(&wkt(ext))).unwrap().transform(10.0, 45.0).unwrap();
        assert!((x - px).abs() < 1e-3 && (y - py).abs() < 1e-3, "{ext}: ({x}, {y}), PROJ ({px}, {py})");
    }
    // Names PROJ does not know are refused there too, as is a vertical unit size that is not
    // positive.
    for ext in [
        "+proj=tmerc +lat_0=0 +lon_0=9 +ellps=foo",
        "+proj=tmerc +lat_0=0 +lon_0=9 +ellps=GRS80 +datum=foo",
        "+proj=tmerc +lat_0=0 +lon_0=9 +ellps=GRS80 +units=feet",
        "+proj=tmerc +lat_0=0 +lon_0=9 +ellps=GRS80 +vunits=foo",
        "+proj=tmerc +lat_0=0 +lon_0=9 +ellps=GRS80 +vto_meter=-1",
    ] {
        let placed = Transformer::new(&wgs84, &SpatialRef::from_wkt(&wkt(ext))).and_then(|t| t.transform(10.0, 45.0));
        assert!(placed.is_err(), "{ext}: {placed:?}");
    }
}

/// `+R` is read instead of `+ellps`, so an ellipsoid name PROJ does not know beside it is not
/// refused; beside `+a`, which does not replace it, it is.
#[test]
fn an_unknown_ellipsoid_is_ignored_beside_a_sphere_radius_only() {
    assert_places("+proj=tmerc +lon_0=9 +R=6378137 +ellps=foo", Some((78_714.767, 5_009_862.833)));
    assert_places("+proj=merc +R=6378137 +ellps=wgs84", Some((1_113_194.908, 5_621_521.486)));
    assert_places("+proj=tmerc +lon_0=9 +a=6378137 +ellps=foo", None);
}

/// An angle that overflows (`1e400`) is not an angle, as PROJ 9.9.0's `dmstor` gives `HUGE_VAL`
/// ("Invalid value for lat_0"). A `PROJ4` extension's `+vto_meter` fraction is read beside
/// `+geoidgrids` too, and a negative one refused there, as PROJ's pipeline does (an optional grid,
/// `@egm96_15.gtx`, so that the string reads without it): WGS 84 (10, 45) is
/// (78846.84165402022, 4985430.940605843).
#[test]
fn an_overflowing_angle_is_refused_and_a_fraction_reads_beside_a_geoid() {
    assert_places("+proj=tmerc +lat_0=1e400 +lon_0=9 +ellps=GRS80", None);
    // Empty values, refused where PROJ maps the parameter, ignored where it does not.
    assert_places("+proj=tmerc +lat_0= +lon_0=9 +ellps=GRS80", None);
    assert_places("+proj=lcc +lat_1= +lat_2=60 +lat_0=30 +lon_0=9 +ellps=GRS80", None);
    assert_places("+proj=tmerc +lat_0=0 +lon_0=9 +x_0= +ellps=GRS80", None);
    assert_places("+proj=robin +lat_0= +ellps=GRS80", Some((846_701.547, 4_805_073.647)));
    let wkt = |v: &str| {
        format!(
            r#"PROJCS["x",GEOGCS["GRS 1980",DATUM["unknown",SPHEROID["GRS80",6378137,298.257222101]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",9],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4","+proj=tmerc +lat_0=0 +lon_0=9 +ellps=GRS80 +geoidgrids=@egm96_15.gtx +vto_meter={v}"]]"#
        )
    };
    let place = |v: &str| {
        Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &SpatialRef::from_wkt(&wkt(v))).and_then(|t| t.transform(10.0, 45.0))
    };
    // An overflowing angle in an extension is refused there too, as PROJ's pipeline refuses it.
    let overflow = wkt("1").replace("+lat_0=0 ", "+lat_0=1e400 ");
    // Malformed angles too (units out of order, radians after degrees), PROJ's `HUGE_VAL`.
    for angle in ["40d30d", "40d30'0.5r"] {
        let malformed = wkt("1").replace("+lat_0=0 ", &format!("+lat_0={angle} "));
        let t = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &SpatialRef::from_wkt(&malformed));
        assert!(t.and_then(|t| t.transform(10.0, 45.0)).is_err(), "{angle}");
    }
    assert!(
        Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &SpatialRef::from_wkt(&overflow))
            .and_then(|t| t.transform(10.0, 45.0))
            .is_err()
    );
    let (x, y) = place("1200/3937").unwrap();
    assert!((x - 78_846.841_654_020_22).abs() < 1e-3 && (y - 4_985_430.940_605_843).abs() < 1e-3, "({x}, {y})");
    assert!(place("-1").is_err());
}

/// `+R_lat_a` and `+R_lat_g` are read as their longest readable start, as PROJ 9.9.0's `ell_set`
/// reads them, in a PROJ string too: `40.5NS` is 40.5°, `40d.` 40° and `abc` none (0°), while
/// `40d30d`, an overflow, a latitude past 90° and an empty value are refused. PROJ places WGS 84
/// (10, 45) as given.
#[test]
fn a_sphere_radius_latitude_is_read_as_its_readable_start() {
    for (key, cases) in [
        (
            "R_lat_a",
            [
                ("40.5NS", Some((1_112_606.200, 5_618_548.571))),
                ("40d.", Some((1_112_541.980, 5_618_224.264))),
                ("9d-30", Some((1_109_650.318, 5_603_621.666))),
                ("abc", Some((1_109_468.833, 5_602_705.186))),
            ],
        ),
        (
            "R_lat_g",
            [
                ("40d.", Some((1_112_539.813, 5_618_213.323))),
                ("9d-30", Some((1_109_644.362, 5_603_591.587))),
                ("abc", Some((1_109_462.576, 5_602_673.589))),
                (".", Some((1_109_462.576, 5_602_673.589))),
            ],
        ),
    ] {
        for (value, expected) in cases {
            assert_places(&format!("+proj=merc +ellps=GRS80 +{key}={value}"), expected);
        }
        for value in ["40d30d", "1e400", "95", ""] {
            assert_places(&format!("+proj=merc +ellps=GRS80 +{key}={value}"), None);
        }
    }
}
