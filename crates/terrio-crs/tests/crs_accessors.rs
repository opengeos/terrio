//! `SpatialRef::axis_order`, `body`, `is_earth` and `projection_params` against PROJ 9.8.1
//! (pyproj 3.8.0, network off), frozen in `data/crs_accessors.tsv` by
//! `scripts/gen_crs_accessors.py`: every 20th EPSG and ESRI code, the codes behind each rule, custom
//! PROJ strings and WKT, compound references, every 20th IAU_2015 CRS and a CRS of every IAU_2015
//! body. PROJ's values are:
//!
//! - the directions of the first two axes (`CRS.axis_info`), which must be the same;
//! - the name of the ellipsoid's body (`proj_get_celestial_body_name`), which must be the same;
//! - the parameters of the PROJ string PROJ writes for the CRS (`CRS.to_proj4()`), with PROJ's
//!   defaults, and the prime meridian and the ellipsoid PROJ reports. The method must be the same,
//!   angles within 1e-9° (the prime meridian within 1e-8°: PROJ reports Paris as EPSG's 2.5969213
//!   grads, 2.33722917°, where the PROJ string's `paris` is 2°20'14.025", 3e-9° east), lengths
//!   within 1 µm and scale factors and units within 1e-12.
//!
//! Known differences, left out of the comparison:
//! - with the wbprojection fallback, the axis order of an EPSG code terrio reads only through it
//!   is unknown (wbprojection's WKT has no axes; PROJ's are west then north for EPSG:3053 and 16
//!   others);
//! - terrio gives the parameters of the PROJ string it reads a CRS by where PROJ writes none
//!   (Polar Stereographic variant C, planetocentric IAU_2015 CRSs on an ellipsoid, geocentric CRSs
//!   in kilometres, axes a PROJ string can't hold).

use terrio_crs::{AxisOrder, CrsQuery, SpatialRef, list_crs};

struct Row {
    kind: String,
    input: String,
    axis: String,
    body: String,
    params: Option<Vec<String>>,
}

fn parse(text: &str) -> Vec<Row> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            Row {
                kind: f[0].into(),
                input: f[1].into(),
                axis: f[2].into(),
                body: f[3].into(),
                params: (f[4] != "NONE").then(|| f[4..].iter().map(|v| v.to_string()).collect()),
            }
        })
        .collect()
}

/// The CRS of a row, `None` when terrio cannot read it (a method it does not compute, a code it
/// does not know).
fn crs(row: &Row) -> Option<SpatialRef> {
    match row.kind.as_str() {
        "EPSG" => SpatialRef::from_epsg(row.input.parse().unwrap()).ok(),
        "ESRI" => SpatialRef::from_esri(row.input.parse().unwrap()).ok(),
        "proj" => SpatialRef::from_proj4(&row.input).ok(),
        "wkt" => Some(SpatialRef::from_wkt(&row.input)),
        "ref" => SpatialRef::parse_literal(&row.input).ok(),
        k => panic!("unknown kind {k}"),
    }
}

/// How terrio's values differ from a row's, if they do.
fn differences(row: &Row, s: &SpatialRef) -> Vec<String> {
    let mut out = Vec::new();
    let axis = match s.axis_order() {
        AxisOrder::EastNorth => "EN",
        AxisOrder::NorthEast => "NE",
        AxisOrder::Other => "OTHER",
        _ => "UNKNOWN",
    };
    // An EPSG code read only through the wbprojection fallback has no axes terrio knows.
    let fallback_only = axis == "UNKNOWN" && row.kind == "EPSG" && terrio_crs::epsg_wkt(row.input.parse().unwrap()).is_none();
    if axis != row.axis && !fallback_only {
        out.push(format!("axis order {axis}, PROJ {}", row.axis));
    }
    // A CRS without an ellipsoid (vertical) has no body in PROJ.
    if row.body != "?" {
        let body = s.body().unwrap_or("-");
        if body != row.body || s.is_earth() != (row.body == "Earth") {
            out.push(format!("body {body} (Earth: {}), PROJ {}", s.is_earth(), row.body));
        }
    }
    let Some(expected) = &row.params else { return out };
    let Some(p) = s.projection_params() else {
        out.push("no parameters".into());
        return out;
    };
    if p.method != expected[0] {
        out.push(format!("method {}, PROJ {}", p.method, expected[0]));
    }
    let ours = [p.lon_0, p.lat_0, p.k_0, p.x_0, p.y_0, p.prime_meridian, p.a, p.b, p.to_meter];
    let names = ["lon_0", "lat_0", "k_0", "x_0", "y_0", "prime meridian", "a", "b", "metres per unit"];
    let tolerances = [1e-9, 1e-9, 1e-12, 1e-6, 1e-6, 1e-8, 1e-6, 1e-6, 1e-12];
    for (((ours, theirs), name), tolerance) in ours.iter().zip(&expected[1..]).zip(names).zip(tolerances) {
        let theirs: f64 = theirs.parse().unwrap();
        if !within(*ours, theirs, tolerance) {
            out.push(format!("{name} {ours}, PROJ {theirs}"));
        }
    }
    out
}

/// Whether a value is within a tolerance of PROJ's: never for NaN.
fn within(ours: f64, theirs: f64, tolerance: f64) -> bool {
    (ours - theirs).abs() <= tolerance
}

fn compare(rows: &[Row]) -> (usize, Vec<String>) {
    let (mut read, mut failures) = (0, Vec::new());
    for row in rows {
        let Some(s) = crs(row) else { continue };
        read += 1;
        let d = differences(row, &s);
        if !d.is_empty() {
            failures.push(format!("{} {}: {}", row.kind, row.input, d.join("; ")));
        }
    }
    (read, failures)
}

#[test]
fn accessors_match_proj() {
    let rows = parse(include_str!("data/crs_accessors.tsv"));
    let (read, failures) = compare(&rows);
    assert!(failures.is_empty(), "{} of {read} differ from PROJ:\n{}", failures.len(), failures.join("\n"));
    // The codes terrio does not read are methods it does not compute without the wbprojection
    // fallback (Aitoff, the other Eckerts...) and ESRI codes it does not have.
    assert!(read * 100 >= rows.len() * 90, "terrio reads only {read} of {} rows", rows.len());
}

#[test]
#[ignore = "needs every code's values: scripts/gen_crs_accessors.py --all PATH, then TERRIO_CRS_ACCESSORS_ALL=PATH"]
fn every_code_matches_proj() {
    // The whole EPSG, ESRI and IAU_2015 tables, of which the frozen data keeps every 20th code.
    let path = std::env::var("TERRIO_CRS_ACCESSORS_ALL").expect("TERRIO_CRS_ACCESSORS_ALL");
    let rows = parse(&std::fs::read_to_string(&path).unwrap());
    let (read, failures) = compare(&rows);
    assert!(failures.is_empty(), "{} of {read} differ from PROJ:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn a_utm_zone_s_central_meridian_is_a_whole_degree() {
    for zone in 1..=60 {
        let p = SpatialRef::from_proj4(&format!("+proj=utm +zone={zone} +datum=WGS84")).unwrap().projection_params().unwrap();
        assert_eq!(p.lon_0, f64::from(zone) * 6.0 - 183.0, "zone {zone}");
    }
}

#[test]
fn a_vertical_crs_without_a_code_has_one_other_axis() {
    // PROJ 9.8.1 reads both with a single `up` axis.
    for wkt in [
        r#"VERT_CS["h",VERT_DATUM["d",2005],UNIT["metre",1],AXIS["Up",UP]]"#,
        r#"VERTCRS["h",VDATUM["d"],CS[vertical,1],AXIS["gravity-related height (H)",up,LENGTHUNIT["metre",1]]]"#,
    ] {
        assert_eq!(SpatialRef::from_wkt(wkt).axis_order(), AxisOrder::Other, "{wkt}");
    }
}

#[test]
fn a_wkt_s_own_axes_are_kept_where_proj_keeps_them() {
    // PROJ 9.8.1: Polar Stereographic stated east then north (or north then east) keeps them;
    // compass directions other than north and east are other axes; a geographic CRS without axes
    // of a code PROJ doesn't have is east then north.
    let geog = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]"#;
    let polar = |axes: &str| {
        format!(
            r#"PROJCS["p",{geog}],PROJECTION["Polar_Stereographic"],PARAMETER["latitude_of_origin",70],PARAMETER["central_meridian",-45],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1]{axes}]"#
        )
    };
    let cases = [
        (polar(""), AxisOrder::Other),
        (polar(r#",AXIS["Easting",EAST],AXIS["Northing",NORTH]"#), AxisOrder::EastNorth),
        (polar(r#",AXIS["Northing",NORTH],AXIS["Easting",EAST]"#), AxisOrder::NorthEast),
        (polar(r#",AXIS["Easting",SOUTH],AXIS["Northing",SOUTH]"#), AxisOrder::Other),
        (
            r#"PROJCRS["x",BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]]],CONVERSION["c",METHOD["Transverse Mercator"],PARAMETER["Longitude of natural origin",9,ANGLEUNIT["degree",0.0174532925199433]]],CS[Cartesian,2],AXIS["a",northEast,LENGTHUNIT["metre",1]],AXIS["b",northWest,LENGTHUNIT["metre",1]]]"#.to_string(),
            AxisOrder::Other,
        ),
        (format!(r#"{geog},AUTHORITY["EPSG","99999"]]"#), AxisOrder::EastNorth),
    ];
    for (wkt, expected) in cases {
        assert_eq!(SpatialRef::from_wkt(&wkt).axis_order(), expected, "{wkt}");
    }
}

#[test]
#[cfg(feature = "wbprojection-fallback")]
fn an_epsg_code_read_only_through_wbprojection_has_unknown_axes() {
    // PROJ 9.8.1 gives EPSG:3053 (Lambert Conic Conformal, West Orientated) west then north axes,
    // which wbprojection's WKT leaves out; its parameters are still read.
    let s = SpatialRef::from_epsg(3053).unwrap();
    assert_eq!(s.axis_order(), AxisOrder::Unknown);
    assert!(s.is_earth());
}

#[test]
fn nan_is_never_within_a_tolerance() {
    assert!(within(1.0, 1.0 + 1e-10, 1e-9));
    assert!(!within(f64::NAN, 1.0, 1e-9));
    assert!(!within(1.0, f64::NAN, 1e-9));
}

#[test]
fn bound_compound_and_angular_crss_read_as_proj_reads_them() {
    // PROJ 9.8.1 (pyproj 3.8.0): a BOUNDCRS has its source CRS's axes; a COMPD_CS's geographic
    // part with EPSG:4326's authority and no axes is north then east; a compound CRS of Web
    // Mercator is on WGS 84; `+units=km` doesn't make longitudes and latitudes linear.
    let bound = format!(
        r#"BOUNDCRS[SOURCECRS[{}],TARGETCRS[{}],ABRIDGEDTRANSFORMATION["t",METHOD["Geocentric translations (geog2D domain)",ID["EPSG",9603]],PARAMETER["X-axis translation",1,ID["EPSG",8605]],PARAMETER["Y-axis translation",2,ID["EPSG",8606]],PARAMETER["Z-axis translation",3,ID["EPSG",8607]]]]"#,
        WGS84_WKT2, WGS84_WKT2
    );
    assert_eq!(SpatialRef::from_wkt(&bound).axis_order(), AxisOrder::NorthEast, "{bound}");
    let compound = r#"COMPD_CS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4326"]],VERT_CS["EGM96 height",VERT_DATUM["EGM96 geoid",2005],UNIT["metre",1],AXIS["Up",UP]]]"#;
    assert_eq!(SpatialRef::from_wkt(compound).axis_order(), AxisOrder::NorthEast);
    let web = SpatialRef::compound(3857, 5773).unwrap().projection_params().unwrap();
    assert_eq!((web.a, web.b), (6378137.0, 6356752.314245179));
    let km = SpatialRef::from_proj4("+proj=longlat +datum=WGS84 +units=km").unwrap().projection_params().unwrap();
    assert_eq!(km.to_meter, 1.0);
}

/// PROJ 9.8.1's WKT 2 of EPSG:4326 (pyproj 3.8.0 `CRS("EPSG:4326").to_wkt("WKT2_2019")`).
const WGS84_WKT2: &str = r#"GEOGCRS["WGS 84",ENSEMBLE["World Geodetic System 1984 ensemble",MEMBER["World Geodetic System 1984 (Transit)"],MEMBER["World Geodetic System 1984 (G730)"],MEMBER["World Geodetic System 1984 (G873)"],MEMBER["World Geodetic System 1984 (G1150)"],MEMBER["World Geodetic System 1984 (G1674)"],MEMBER["World Geodetic System 1984 (G1762)"],MEMBER["World Geodetic System 1984 (G2139)"],MEMBER["World Geodetic System 1984 (G2296)"],ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]],ENSEMBLEACCURACY[2.0]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["geodetic latitude (Lat)",north,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["geodetic longitude (Lon)",east,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]],USAGE[SCOPE["Horizontal component of 3D system."],AREA["World."],BBOX[-90,-180,90,180]],ID["EPSG",4326]]"#;

#[test]
fn a_crs_terrio_cannot_read_has_no_accessor_values() {
    let s = SpatialRef { wkt: Some("not a CRS".into()), ..Default::default() };
    assert_eq!(s.axis_order(), AxisOrder::Unknown);
    assert_eq!(s.projection_params(), None);
    assert_eq!(s.body(), None);
    assert!(!s.is_earth());
}

#[test]
fn every_iau_body_has_proj_s_name() {
    // The first CRS of each body, which the data has, names it; here every listed body is read.
    let entries = list_crs(&CrsQuery { authorities: vec!["IAU_2015".into()], include_deprecated: true, ..Default::default() });
    let mut bodies = std::collections::BTreeSet::new();
    for entry in &entries {
        let s = SpatialRef::parse_literal(&format!("IAU_2015:{}", entry.code)).unwrap();
        bodies.insert(s.body().unwrap_or_else(|| panic!("IAU_2015:{} has no body", entry.code)));
    }
    assert_eq!(bodies.len(), 97, "{bodies:?}");
}
