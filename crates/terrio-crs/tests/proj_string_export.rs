//! `SpatialRef::to_proj4` against the PROJ strings PROJ 9.8.1 writes (pyproj 3.8.0's
//! `CRS.to_proj4()`, `proj_as_proj_string` with `PJ_PROJ_4`), frozen in
//! `data/proj_string_export.tsv` by `scripts/gen_proj_string_export.py`: every 20th EPSG and ESRI
//! code, the codes behind each rule of the writer, custom PROJ strings and WKT (bound CRSs among
//! them), compound references (`EPSG:4326+5773`) and every 20th IAU_2015 CRS. The strings must
//! be PROJ's, character for character.
//!
//! Known differences, left out of the data: an ESRI WKT of Miller's projection, which PROJ reads
//! on the authalic sphere (`+proj=mill +R_A`) and terrio does not; a WKT's grid shift (GDAL's
//! `PROJ4_GRIDS` in a datum, a `BOUNDCRS` with a grid), which terrio's WKT reader does not read,
//! so its string has no `+nadgrids`; and the order of the parameters of a custom PROJ string,
//! which PROJ rewrites in its own order, adding the defaults, and terrio keeps.

use std::collections::HashMap;
use terrio_crs::{CrsQuery, SpatialRef, Transformer, list_crs};

struct Row {
    kind: String,
    input: String,
    proj: Option<String>,
}

fn rows() -> Vec<Row> {
    parse(include_str!("data/proj_string_export.tsv"))
}

/// The rows of the data's format.
fn parse(text: &str) -> Vec<Row> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let mut f = l.splitn(3, '\t');
            let (kind, input, proj) = (f.next().unwrap(), f.next().unwrap(), f.next().unwrap());
            Row { kind: kind.into(), input: input.into(), proj: (proj != "NONE").then(|| proj.into()) }
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

#[test]
fn proj_strings_match_proj() {
    let rows = rows();
    let mut read = 0;
    let mut failures = Vec::new();
    for row in &rows {
        let Some(s) = crs(row) else { continue };
        read += 1;
        let ours = s.to_proj4();
        if ours != row.proj {
            failures.push(format!("{} {}:\n  terrio {ours:?}\n  PROJ   {:?}", row.kind, row.input, row.proj));
        }
    }
    assert!(failures.is_empty(), "{} of {read} differ from PROJ:\n{}", failures.len(), failures.join("\n"));
    // The codes terrio does not read are methods it does not compute without the wbprojection
    // fallback (Aitoff, the other Eckerts...) and ESRI codes it does not have.
    assert!(read * 100 >= rows.len() * 90, "terrio reads only {read} of {} rows", rows.len());
}

#[test]
#[ignore = "needs every code's PROJ string: scripts/gen_proj_string_export.py --all PATH, then TERRIO_PROJ_STRINGS_ALL=PATH"]
fn every_code_matches_proj() {
    // The whole EPSG, ESRI and IAU_2015 tables, of which the frozen data keeps every 20th code.
    let path = std::env::var("TERRIO_PROJ_STRINGS_ALL").expect("TERRIO_PROJ_STRINGS_ALL");
    let rows = parse(&std::fs::read_to_string(&path).unwrap());
    let (mut read, mut failures) = (0, Vec::new());
    for row in &rows {
        let Some(s) = crs(row) else { continue };
        read += 1;
        let ours = s.to_proj4();
        if ours != row.proj {
            failures.push(format!("{} {}:\n  terrio {ours:?}\n  PROJ   {:?}", row.kind, row.input, row.proj));
        }
    }
    println!("{read} of {} codes read, {} differ from PROJ", rows.len(), failures.len());
    assert!(failures.is_empty(), "{} of {read} differ from PROJ:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn rule_codes_are_all_read() {
    // Each rule of the writer is checked on a code terrio reads.
    for code in [3857, 2056, 4267, 2065, 2046, 8044, 4801, 4901, 7405, 5703, 5754, 2985] {
        assert!(SpatialRef::from_epsg(code).is_ok(), "EPSG:{code}");
    }
    for code in [102100, 54030, 53049] {
        assert!(SpatialRef::from_esri(code).is_ok(), "ESRI:{code}");
    }
}

#[test]
fn engineering_crs_and_proj_refusals_have_no_proj_string() {
    // Polar Stereographic (variant C) has no PROJ method; an engineering CRS no PROJ string.
    assert_eq!(SpatialRef::from_epsg(2985).unwrap().to_proj4(), None);
    let local = SpatialRef::from_wkt(r#"LOCAL_CS["Engineering",LOCAL_DATUM["site",0],UNIT["metre",1],AXIS["X",EAST],AXIS["Y",NORTH]]"#);
    assert_eq!(local.to_proj4(), None);
}

#[test]
fn proj_strings_read_back_as_the_same_conversion() {
    // Read back, a registry CRS's PROJ string converts as the CRS does. Like PROJ's, it has no
    // datum shift, so the transformation between the two is PROJ's ballpark, which keeps the
    // coordinates; checked at the centre of the CRS's area of use.
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let areas: HashMap<String, [f64; 4]> = list_crs(&CrsQuery { include_deprecated: true, ..Default::default() })
        .into_iter()
        .filter_map(|e| Some((e.authority_code(), e.bounds?)))
        .collect();
    let mut checked = 0;
    for row in rows().iter().filter(|r| r.kind == "EPSG" || r.kind == "ESRI") {
        let (Some(s), Some(proj)) = (crs(row), row.proj.as_ref()) else { continue };
        if proj.starts_with("+vunits") || proj.starts_with("+vto_meter") || proj.contains("+proj=geocent") {
            continue;
        }
        // A PROJ string has no angular unit, so a geographic CRS in other units than degrees
        // (EPSG:4807 in grads) reads back in degrees, in PROJ too.
        if s.is_geographic() && !s.to_wkt().unwrap_or_default().contains(r#"UNIT["degree""#) {
            continue;
        }
        // Nor has it the axis order of CRSs whose axes point south or west, but for the methods
        // that define it (EPSG:8044, Cassini-Soldner, reads back east and north).
        let wkt = s.to_wkt().unwrap_or_default();
        if (wkt.contains(",SOUTH]") || wkt.contains(",WEST]")) && !proj.contains("+axis=") {
            continue;
        }
        let Ok(back) = SpatialRef::from_proj4(proj) else { panic!("{} {}: {proj} does not read back", row.kind, row.input) };
        // An area across the antimeridian is left out: its centre is not between west and east.
        let Some([w, south, e, n]) = areas.get(&format!("{}:{}", row.kind, row.input)).copied().filter(|[w, _, e, _]| w < e) else {
            continue;
        };
        let Ok(to) = Transformer::new(&wgs84, &s) else { continue };
        let Ok(p) = to.transform((w + e) / 2.0, (south + n) / 2.0) else { continue };
        let q = Transformer::new(&s, &back).unwrap().transform(p.0, p.1).unwrap();
        let tolerance = if s.is_geographic() { 1e-9 } else { 1e-3 };
        assert!((q.0 - p.0).abs() <= tolerance && (q.1 - p.1).abs() <= tolerance, "{} {}: {p:?} reads back as {q:?}", row.kind, row.input);
        checked += 1;
    }
    assert!(checked > 300, "only {checked} CRSs checked");
}

#[test]
fn shape_parameters_without_a_change_the_named_ellipsoid_as_proj() {
    // `+f`, `+rf` and `+b` without `+a` change the shape of `+ellps` or `+datum`'s ellipsoid, as
    // PROJ's `pj_ell_set` does: what reading back ESRI:102035's PROJ string needs. PROJ 9.8.1's
    // (pyproj 3.8.0) `Transformer.from_crs(crs.geodetic_crs, crs, always_xy=True)` values.
    for (definition, (lon, lat), expected) in [
        ("+proj=ortho +f=0 +lat_0=90 +lon_0=0 +x_0=0 +y_0=0 +datum=WGS84 +units=m", (0.0, 45.0), (0.0, -4510023.924036823)),
        ("+proj=merc +ellps=GRS80 +rf=300 +units=m", (10.0, 50.0), (1113194.9079327357, 6413714.784992442)),
        ("+proj=merc +ellps=GRS80 +b=6356000 +units=m", (10.0, 50.0), (1113194.9079327357, 6412372.842122187)),
    ] {
        let crs = SpatialRef::from_proj4(definition).unwrap();
        // The geographic CRS on the same ellipsoid: the definition's, as a longitude/latitude.
        let shape: Vec<&str> = definition
            .split_whitespace()
            .filter(|t| ["+f=", "+rf=", "+b=", "+ellps=", "+datum="].iter().any(|k| t.starts_with(k)))
            .collect();
        let geographic = SpatialRef::from_proj4(&format!("+proj=longlat {}", shape.join(" "))).unwrap();
        let (x, y) = Transformer::new(&geographic, &crs).unwrap().transform(lon, lat).unwrap();
        assert!((x - expected.0).abs() < 1e-6 && (y - expected.1).abs() < 1e-6, "{definition}: ({x}, {y}), PROJ {expected:?}");
    }
}

#[test]
fn ellipsoids_proj_refuses_are_refused() {
    // PROJ refuses these (`Invalid value for an argument`): a semi-minor axis above the
    // semi-major one, an eccentricity of 1 or more. Read, they would give NaN coordinates.
    for definition in ["+proj=longlat +ellps=GRS80 +b=6400000", "+proj=longlat +ellps=GRS80 +es=1.2", "+proj=longlat +a=6378137 +b=6400000"]
    {
        assert!(SpatialRef::from_proj4(definition).is_err(), "{definition}");
    }
}

#[test]
fn a_unit_given_as_a_fraction_is_written_by_its_name() {
    // terrio reads `+to_meter=1/1000` (PROJ refuses it): its string names the unit it reads.
    let crs = SpatialRef::from_proj4("+proj=merc +lon_0=0 +k=1 +x_0=0 +y_0=0 +ellps=WGS84 +to_meter=1/1000").unwrap();
    assert_eq!(crs.to_proj4().as_deref(), Some("+proj=merc +lon_0=0 +k=1 +x_0=0 +y_0=0 +ellps=WGS84 +units=mm +no_defs +type=crs"));
}

#[test]
fn a_shift_proj_refuses_is_not_written() {
    // PROJ refuses `+towgs84` with other than 3 or 7 values, and terrio reads it as no shift: the
    // string has none.
    let crs = SpatialRef::from_proj4("+proj=longlat +ellps=GRS80 +towgs84=1,2,3,4,5,6").unwrap();
    assert_eq!(crs.to_proj4().as_deref(), Some("+proj=longlat +ellps=GRS80 +no_defs +type=crs"));
}

#[test]
fn airy_and_copenhagen_are_read_as_proj_defines_them() {
    // PROJ 9.8.1's `airy` is a = 6377563.396, rf = 299.3249646 (b = 6356256.909237285), and its
    // Copenhagen meridian 12°34'40.35"E = 12.577875°: pyproj 3.8.0's
    // `Transformer.from_crs(..., always_xy=True)` values.
    let airy = SpatialRef::from_proj4("+proj=longlat +ellps=airy").unwrap();
    let grid = SpatialRef::from_proj4("+proj=tmerc +lat_0=49 +lon_0=-2 +k=0.9996012717 +x_0=400000 +y_0=-100000 +ellps=airy").unwrap();
    let (x, y) = Transformer::new(&airy, &grid).unwrap().transform(-1.0, 60.0).unwrap();
    assert!((x - 455770.8204770909).abs() < 1e-6 && (y - 1124268.6754824587).abs() < 1e-6, "({x}, {y})");
    let intl = SpatialRef::from_proj4("+proj=longlat +ellps=intl").unwrap();
    let copenhagen = SpatialRef::from_proj4("+proj=longlat +ellps=intl +pm=copenhagen").unwrap();
    let (lon, lat) = Transformer::new(&intl, &copenhagen).unwrap().transform(13.0, 55.0).unwrap();
    assert!((lon - 0.42212500000000014).abs() < 1e-12 && (lat - 55.0).abs() < 1e-12, "({lon}, {lat})");
}
