//! `SpatialRef::to_wkt1_strict` against the WKT 1 PROJ 9.8.1 writes (pyproj 3.8.0's
//! `CRS.to_wkt("WKT1_GDAL", output_axis_rule=True)`, `proj_as_wkt` with `WKT1_GDAL` and the
//! AXIS nodes GDAL writes), frozen in `data/wkt1_strict_export.tsv` by
//! `scripts/gen_wkt1_strict_export.py`: every 20th EPSG and ESRI code, every registry code PROJ
//! writes no WKT 1 for, the codes behind each rule, custom PROJ strings and WKT (WKT 2 among
//! them), compound references and every 20th IAU_2015 CRS.
//!
//! A registry CRS's WKT 1 must be PROJ's, character for character, and `None` where PROJ has
//! none. Any other CRS's (written by terrio from its PROJ string, or stored as given) must be
//! `None` where PROJ has none or needs its PROJ4 extension, else WKT 1 without the extension
//! that terrio reads back as the same CRS. Web Mercator keeps GDAL's extension under its code.
//!
//! Known differences, listed in [`STRICTER`] and [`LAXER`]: PROJ describes a projected CRS of
//! north-east axes, a cylindrical equal-area projection by its scale and a WKT 2 compound CRS
//! without a code in WKT 1, terrio does not; PROJ keeps UPS in its extension, terrio writes it as
//! Polar Stereographic. And a planetocentric IAU_2015 CRS on an ellipsoid, which PROJ writes as a
//! `GEOGCS` its readers take for planetographic latitudes, has none, as a WKT 2 with a west axis
//! (planetographic IAU_2015 CRSs). The projected CRSs of Halley and Eros are not read back, as
//! PROJ's WKT 1 of them reads back as another CRS too. Left out of the data: EPSG
//! 2985 and 2986 (Polar Stereographic variant C), whose table WKT is GDAL's, without the AXIS
//! nodes PROJ writes.

use std::collections::HashSet;
use std::sync::OnceLock;
use terrio_crs::{CrsQuery, SpatialRef, Transformer, list_crs};

struct Row {
    kind: String,
    input: String,
    wkt: Option<String>,
}

fn rows() -> Vec<Row> {
    parse(include_str!("data/wkt1_strict_export.tsv"))
}

/// The rows of the data's format.
fn parse(text: &str) -> Vec<Row> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let mut f = l.splitn(3, '\t');
            let (kind, input, wkt) = (f.next().unwrap(), f.next().unwrap(), f.next().unwrap());
            Row { kind: kind.into(), input: input.into(), wkt: (wkt != "NONE").then(|| wkt.into()) }
        })
        .collect()
}

/// The codes of terrio's own EPSG and ESRI tables.
fn table() -> &'static HashSet<String> {
    static TABLE: OnceLock<HashSet<String>> = OnceLock::new();
    TABLE.get_or_init(|| {
        list_crs(&CrsQuery { include_deprecated: true, ..Default::default() }).into_iter().map(|e| e.authority_code()).collect()
    })
}

/// The CRS of a row, `None` when terrio cannot read it (a method it does not compute, a code it
/// does not know). A code outside terrio's tables, which the wbprojection fallback reads (when
/// another crate of the workspace enables it), counts as one it does not know: its strict WKT 1
/// is `None`, the fallback's WKT not being the registry's.
fn crs(row: &Row) -> Option<SpatialRef> {
    if (row.kind == "EPSG" || row.kind == "ESRI") && !table().contains(&format!("{}:{}", row.kind, row.input)) {
        return None;
    }
    match row.kind.as_str() {
        "EPSG" => SpatialRef::from_epsg(row.input.parse().unwrap()).ok(),
        "ESRI" => SpatialRef::from_esri(row.input.parse().unwrap()).ok(),
        "proj" => SpatialRef::from_proj4(&row.input).ok(),
        "wkt" => Some(SpatialRef::from_wkt(&row.input)),
        "ref" => SpatialRef::parse_literal(&row.input).ok(),
        k => panic!("unknown kind {k}"),
    }
}

/// Definitions PROJ writes in WKT 1 and terrio does not (it needs the extension for them).
const STRICTER: &[&str] = &[
    // Axis orders other than east-north.
    "+proj=longlat +datum=WGS84 +axis=neu",
    "+proj=utm +zone=32 +datum=WGS84 +axis=neu",
    "+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=0 +y_0=0 +ellps=bessel +axis=nwu +units=m",
    "+proj=tmerc +lon_0=9 +k=0.9996 +x_0=500000 +ellps=GRS80 +axis=neu +units=m",
    // A cylindrical equal-area projection by its scale.
    "+proj=cea +k_0=0.9 +ellps=WGS84 +units=m",
    // A named datum changed by a prime meridian or a shift: PROJ writes its ellipsoid, terrio's
    // reading keeps the datum.
    "+proj=longlat +datum=WGS84 +pm=paris",
    "+proj=longlat +ellps=intl +datum=WGS84 +pm=paris",
    "+proj=longlat +datum=carthage +towgs84=-263,6,431",
    "+proj=longlat +datum=WGS84 +towgs84=1,2,3",
    "+proj=longlat +datum=WGS84 +ellps=WGS84 +towgs84=0,0,0",
    // A flattening changing a named ellipsoid, which PROJ's WKT drops and its projection keeps.
    "+proj=ortho +f=0 ",
    "+proj=ortho +f=0.003 ",
    "+proj=longlat +ellps=GRS80 +es=0.006",
    "+proj=longlat +a=6378137 +b=6356752.314245179 +es=0.006",
    // A word without a `+`, which PROJ reads as a parameter (and ignores when it knows none).
    "+proj=longlat +ellps=GRS80 foo",
    // Planetocentric latitudes on a sphere, the same as geodetic ones.
    "+proj=longlat +geoc +R=6371000",
    // A WKT 2 compound CRS without a code.
    "COMPOUNDCRS[\"custom\"",
];

/// Definitions PROJ keeps in its extension and terrio writes in WKT 1.
const LAXER: &[&str] = &["+proj=ups +ellps=WGS84 +units=m"];

/// The EPSG code at the root of a WKT 1.
fn root_code(wkt: &str) -> Option<u32> {
    let tail = &wkt[wkt.rfind(",AUTHORITY[\"EPSG\",\"")?..];
    // The root's own: the last node, closed by the root's bracket.
    if tail.matches(']').count() != 2 {
        return None;
    }
    let code = tail.trim_start_matches(",AUTHORITY[\"EPSG\",\"").split('"').next()?;
    code.parse().ok()
}

/// The EPSG code of a WKT 1's (first) datum: the `AUTHORITY` that closes its `DATUM` node.
fn datum_code(wkt: &str) -> Option<u32> {
    let start = wkt.find("DATUM[\"")?;
    let mut depth = 0;
    let len = wkt[start..].char_indices().find_map(|(i, c)| {
        depth += match c {
            '[' => 1,
            ']' => -1,
            _ => 0,
        };
        (c == ']' && depth == 0).then_some(i + 1)
    })?;
    root_code(&wkt[start..start + len])
}

/// The EPSG datum terrio identifies a CRS's datum as: its geographic CRS's.
fn identified_datum(s: &SpatialRef) -> Option<u32> {
    let geog = SpatialRef::from_epsg(s.geographic_code()?).ok()?.to_wkt()?;
    datum_code(&geog)
}

/// The directions of a WKT 1's AXIS nodes, in order.
fn axis_directions(wkt: &str) -> Vec<&str> {
    wkt.split(",AXIS[").skip(1).filter_map(|a| a.split(']').next()?.rsplit(',').next()).collect()
}

/// Whether terrio's WKT 1 lost the EPSG code PROJ's has, though it has the code's axes.
fn keeps_code(wkt: &str, code: u32) -> bool {
    let registry = SpatialRef::from_epsg(code).ok().and_then(|s| s.to_wkt1_strict());
    root_code(wkt) != Some(code) && registry.is_some_and(|r| axis_directions(&r) == axis_directions(wkt))
}

/// Whether WKT 1 has an extension (`PROJ4`, or a datum's `PROJ4_GRIDS`) that no EPSG or ESRI
/// code of its root identifies.
fn needs_extension(wkt: &str) -> bool {
    let coded = wkt.rfind(",AUTHORITY[").is_some_and(|i| {
        let tail = &wkt[i..];
        tail.matches(']').count() == 2 && (tail.starts_with(",AUTHORITY[\"EPSG\"") || tail.starts_with(",AUTHORITY[\"ESRI\""))
    });
    wkt.contains("EXTENSION[") && !coded
}

/// Whether terrio reads a WKT 1 back as the CRS: the same CRS, or one whose coordinates are the
/// CRS's (without a code, `same_as` compares WKT, and an IAU_2015 CRS's WKT 1 has no IAU code).
/// Checked on two points near the origin, within 1e-9° or a millionth of their distance from
/// it, which also catches another axis order, direction or unit.
fn reads_back(wkt: &str, s: &SpatialRef) -> bool {
    let back = SpatialRef::from_wkt(wkt);
    if back.same_as(s) {
        return true;
    }
    // A geocentric CRS by its PROJ string (its X, Y and Z do not fit two points).
    if s.to_proj4().is_some_and(|p| p.contains("+proj=geocent")) {
        return back.to_proj4() == s.to_proj4();
    }
    let Ok(t) = Transformer::new(s, &back) else { return false };
    // Points off the origin, at a thousandth of the body's radius: a polar CRS's origin is the
    // pole, where longitudes are ill-conditioned.
    let r = s.to_wkt().as_deref().and_then(terrio_crs::semi_major_axis).unwrap_or(6_378_137.0) * 1e-3;
    let (points, tolerance) =
        if s.is_geographic() { ([(10.0, 20.0), (-30.0, -40.0)], 1e-9) } else { ([(r, -2.0 * r), (-3.0 * r, 4.0 * r)], 1e-6 * r.max(1.0)) };
    points.iter().all(|&(x, y)| t.transform(x, y).is_ok_and(|(u, v)| (u - x).abs() <= tolerance && (v - y).abs() <= tolerance))
}

/// Whether a CRS has an axis pointing west, as WKT 2 gives planetographic IAU_2015 CRSs: terrio
/// writes WKT 2 without a code in WKT 1 through its PROJ string, which has no such axis.
fn west_axis(s: &SpatialRef) -> bool {
    s.wkt.as_deref().is_some_and(|w| w.to_ascii_lowercase().contains(",west,"))
}

/// The projected CRSs of Halley and Eros (IAU_2015:1000036xx, 2000433xx), flattened by a half
/// and more, whose WKT 1 PROJ also reads, without its IAU code, as another CRS (by 0.1% to 6%):
/// the reading back is not checked.
fn read_otherwise_by_proj(row: &Row) -> bool {
    ["IAU_2015:1000036", "IAU_2015:2000433"].iter().any(|p| row.input.starts_with(p))
}

/// Whether a CRS is planetocentric on an ellipsoid (`+geoc` without `+R`).
fn planetocentric_on_ellipsoid(s: &SpatialRef) -> bool {
    s.proj4.as_deref().is_some_and(|p| p.split_whitespace().any(|t| t == "+geoc") && !p.contains("+R="))
}

/// The differences from PROJ of a row's WKT 1, described.
fn check(row: &Row, s: &SpatialRef) -> Option<String> {
    let ours = s.to_wkt1_strict();
    if row.kind == "EPSG" || row.kind == "ESRI" {
        return (ours != row.wkt).then(|| format!("\n  terrio {ours:?}\n  PROJ   {:?}", row.wkt));
    }
    let listed = |list: &[&str]| list.iter().any(|l| row.input.starts_with(l));
    let expected = match &row.wkt {
        _ if listed(STRICTER) || planetocentric_on_ellipsoid(s) || west_axis(s) => false,
        _ if listed(LAXER) => true,
        Some(w) => !needs_extension(w),
        None => false,
    };
    match (&ours, expected) {
        (None, false) => None,
        // On a datum terrio identifies (OSGB36): its WKT 1 keeps the datum's code (its identity and
        // transformations). Not for a WKT 1 given, returned as it is (an ESRI `.prj`'s
        // `D_WGS_1984`), nor for an explicit shift (`+datum=WGS84 +towgs84=...`), whose
        // `TOWGS84` gives the transformation.
        (Some(w), true)
            if !["GEOGCS[", "PROJCS[", "GEOCCS[", "COMPD_CS["].iter().any(|k| row.input.starts_with(k))
                && !w.contains("TOWGS84[")
                && identified_datum(s).is_some_and(|d| datum_code(w) != Some(d)) =>
        {
            Some(format!("\n  terrio lost the datum: {w}\n  PROJ   {:?}", row.wkt))
        }
        // PROJ's WKT 1 under a root EPSG code: terrio's too, unless its axes are not the code's.
        (Some(w), true) if row.wkt.as_deref().and_then(root_code).is_some_and(|c| keeps_code(w, c)) => {
            Some(format!("\n  terrio lost the code: {w}\n  PROJ   {:?}", row.wkt))
        }
        (Some(w), true) if needs_extension(w) => Some(format!("\n  terrio needs its extension: {w}")),
        (Some(w), true) if !read_otherwise_by_proj(row) && !reads_back(w, s) => Some(format!("\n  terrio reads back as another CRS: {w}")),
        (Some(_), true) => None,
        _ => Some(format!("\n  terrio {ours:?}\n  PROJ   {:?}", row.wkt)),
    }
}

#[test]
fn strict_wkt1_matches_proj() {
    let rows = rows();
    let mut read = 0;
    let mut failures = Vec::new();
    for row in &rows {
        let Some(s) = crs(row) else { continue };
        read += 1;
        if let Some(f) = check(row, &s) {
            failures.push(format!("{} {}:{f}", row.kind, row.input));
        }
    }
    assert!(failures.is_empty(), "{} of {read} differ from PROJ:\n{}", failures.len(), failures.join("\n"));
    // The codes terrio does not read are methods it does not compute without the wbprojection
    // fallback (Eckert, Aitoff...) and ESRI codes it does not have.
    assert!(read * 100 >= rows.len() * 90, "terrio reads only {read} of {} rows", rows.len());
}

#[test]
#[ignore = "needs every code's WKT 1: scripts/gen_wkt1_strict_export.py --all PATH, then TERRIO_WKT1_ALL=PATH"]
fn every_code_matches_proj() {
    // The whole EPSG, ESRI and IAU_2015 tables, of which the frozen data keeps every 20th code.
    let path = std::env::var("TERRIO_WKT1_ALL").expect("TERRIO_WKT1_ALL");
    let rows = parse(&std::fs::read_to_string(&path).unwrap());
    let (mut read, mut failures) = (0, Vec::new());
    for row in &rows {
        let Some(s) = crs(row) else { continue };
        read += 1;
        if let Some(f) = check(row, &s) {
            failures.push(format!("{} {}:{f}", row.kind, row.input));
        }
    }
    println!("{read} of {} codes read, {} differ from PROJ", rows.len(), failures.len());
    assert!(failures.is_empty(), "{} of {read} differ from PROJ:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn rule_codes_are_all_read() {
    // Each rule is checked on a code terrio reads.
    for code in [3857, 4979, 4978, 8857, 2046, 5514, 7405, 9707] {
        assert!(SpatialRef::from_epsg(code).is_ok(), "EPSG:{code}");
    }
    for code in [102100, 54030, 53035] {
        assert!(SpatialRef::from_esri(code).is_ok(), "ESRI:{code}");
    }
}

#[test]
fn codes_outside_the_tables_have_none() {
    // An EPSG code outside terrio's tables (EPSG:3053) is not read in the default build; with the
    // wbprojection fallback (another crate of the workspace enables it), it is read through
    // wbprojection's WKT, which is not the registry's.
    assert!(!table().contains("EPSG:3053"));
    assert!(SpatialRef::from_epsg(3053).ok().is_none_or(|crs| crs.to_wkt1_strict().is_none()));
}

#[test]
fn a_wkt2_of_other_axes_than_its_code_is_not_the_codes_wkt1() {
    // A WKT 2 of CRS84, identified as EPSG:4326, is written with east-north axes and without
    // EPSG:4326's code (PROJ writes it under OGC's).
    let crs84 = r#"GEOGCRS["WGS 84 (CRS84)",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]],CS[ellipsoidal,2],AXIS["longitude",east],AXIS["latitude",north],ANGLEUNIT["degree",0.0174532925199433]]"#;
    let w = SpatialRef::from_wkt(crs84).to_wkt1_strict().unwrap();
    assert!(w.contains(",AXIS[\"Longitude\",EAST],AXIS[\"Latitude\",NORTH]]") && !w.contains("\"4326\""), "{w}");
}

#[test]
fn a_polar_wkt2_under_its_code_is_the_codes_wkt1() {
    // Polar CRSs' WKT 2 axes run along meridians (south/south, north/north, ...); identified,
    // they give the code's WKT 1, character for character.
    for code in [5041, 3031, 6931] {
        let id = format!(",ID[\"EPSG\",{code}]]");
        let row = rows().into_iter().find(|r| r.kind == "wkt" && r.input.starts_with("PROJCRS[") && r.input.ends_with(&id));
        let row = row.unwrap_or_else(|| panic!("no WKT 2 row of EPSG:{code}"));
        let registry = SpatialRef::from_epsg(code).unwrap().to_wkt1_strict();
        assert!(registry.is_some(), "EPSG:{code}");
        assert_eq!(SpatialRef::from_wkt(&row.input).to_wkt1_strict(), registry, "EPSG:{code}");
    }
}

#[test]
fn the_default_wkt_keeps_the_extension() {
    // `to_wkt` is unchanged: the extension where WKT 1 needs it.
    let grid = SpatialRef::from_proj4("+proj=longlat +ellps=clrk66 +nadgrids=conus +no_defs").unwrap();
    assert!(grid.to_wkt().is_some_and(|w| w.contains("EXTENSION[\"PROJ4\"")));
    assert_eq!(grid.to_wkt1_strict(), None);
    let web = SpatialRef::from_epsg(3857).unwrap();
    assert_eq!(web.to_wkt1_strict(), web.to_wkt());
}

#[test]
fn a_stored_wkt1_needing_its_extension_has_none() {
    // A WKT 1 given with a PROJ4 extension and no code of its own: `None`; under its own code
    // (GDAL's Web Mercator), as it is.
    let custom = SpatialRef::from_proj4("+proj=longlat +ellps=clrk66 +nadgrids=conus +no_defs").unwrap().to_wkt().unwrap();
    assert!(custom.contains("EXTENSION[\"PROJ4\""));
    assert_eq!(SpatialRef::from_wkt(&custom).to_wkt1_strict(), None);
    let web = SpatialRef::from_epsg(3857).unwrap().to_wkt().unwrap();
    assert_eq!(SpatialRef::from_wkt(&web).to_wkt1_strict().as_deref(), Some(web.as_str()));
}

#[test]
fn to_wkt_describes_without_the_extension() {
    // What `to_wkt` now writes without the extension, as PROJ 9.8.1's WKT1_GDAL does: a
    // geocentric CRS, `+R_C` on a sphere, an ellipsoid of `+a` and `+es` or `+e`.
    for (proj, node) in [
        ("+proj=geocent +ellps=GRS80 +units=m", "GEOCCS[\"unknown\",DATUM[\"Unknown based on GRS 1980 ellipsoid\""),
        ("+proj=merc +R_C +lon_0=0 +x_0=0 +y_0=0 +R=3396190 +units=m", "SPHEROID[\"unknown\",3396190,0]"),
        ("+proj=longlat +a=6378000 +es=0.006", "SPHEROID[\"unknown\",6378000,332.832581074854]"),
        ("+proj=longlat +a=6378137 +e=0.08", "SPHEROID[\"unknown\",6378137,311.999197429715]"),
    ] {
        let w = SpatialRef::from_proj4(proj).unwrap().to_wkt().unwrap();
        assert!(w.contains(node) && !w.contains("EXTENSION"), "{proj}: {w}");
    }
    // Read back, each converts as the PROJ string (without its extension, from the nodes alone).
    for proj in [
        "+proj=merc +R_C +lon_0=0 +x_0=0 +y_0=0 +R=3396190 +units=m",
        "+proj=longlat +a=6378000 +es=0.006",
        "+proj=longlat +a=6378137 +e=0.08",
    ] {
        let crs = SpatialRef::from_proj4(proj).unwrap();
        let back = SpatialRef::from_wkt(&crs.to_wkt().unwrap());
        let t = Transformer::new(&crs, &back).unwrap();
        let p = if crs.is_geographic() { (10.0, 20.0) } else { (100_000.0, -200_000.0) };
        let q = t.transform(p.0, p.1).unwrap();
        assert!((q.0 - p.0).abs() < 1e-6 && (q.1 - p.1).abs() < 1e-6, "{proj}: {q:?}");
    }
    let geocentric = SpatialRef::from_proj4("+proj=geocent +ellps=GRS80 +units=m").unwrap();
    let back = SpatialRef::from_wkt(&geocentric.to_wkt().unwrap());
    assert_eq!(back.to_proj4(), geocentric.to_proj4());
    // Beside `+b`, whose order of precedence differs from PROJ's, `+es` keeps the extension.
    let w = SpatialRef::from_proj4("+proj=longlat +a=6378137 +b=6356752.314245179 +es=0.006").unwrap().to_wkt().unwrap();
    assert!(w.contains("EXTENSION[\"PROJ4\""), "{w}");
}

#[test]
fn a_prime_meridian_in_degrees_minutes_and_seconds_is_read_and_written() {
    // `+pm=2d20'14.025"E` is Paris, as PROJ 9.8.1 reads it: pyproj 3.8.0's
    // `Transformer.from_crs("EPSG:4326", crs, always_xy=True)` gives 7.662770833333334 at 10°E.
    let crs = SpatialRef::from_proj4("+proj=longlat +ellps=GRS80 +pm=2d20'14.025\"E").unwrap();
    let (lon, lat) = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).unwrap().transform(10.0, 45.0).unwrap();
    assert!((lon - 7.662770833333334).abs() < 1e-9 && (lat - 45.0).abs() < 1e-9, "({lon}, {lat})");
    let w = crs.to_wkt1_strict().unwrap();
    assert!(w.contains("PRIMEM[\"unknown\",2.33722916666667]"), "{w}");
}

#[test]
fn defining_inverse_flattenings_are_written() {
    // PROJ writes a named ellipsoid's defining inverse flattening (298.257223563 for WGS 84),
    // and the WKT 1 of a datum shift on it describes the CRS without the extension.
    let w = SpatialRef::from_proj4("+proj=utm +zone=32 +ellps=WGS84 +towgs84=0,0,0").unwrap().to_wkt().unwrap();
    assert!(w.contains("SPHEROID[\"WGS 84\",6378137,298.257223563,") && !w.contains("EXTENSION"), "{w}");
    let w = SpatialRef::from_proj4("+proj=longlat +a=6378137 +rf=298.257223563 +towgs84=0,0,0").unwrap().to_wkt().unwrap();
    assert!(w.contains("SPHEROID[\"unknown\",6378137,298.257223563]") && !w.contains("EXTENSION"), "{w}");
}
