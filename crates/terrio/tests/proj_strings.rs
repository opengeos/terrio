//! Every PROJ projection, as a PROJ string with typical parameters on a sphere and on WGS 84, and
//! as pyproj's WKT1_GDAL and WKT2_2019 of the same CRS, against PROJ (#134). The reference points
//! are pyproj's (PROJ 9.8.1), from `scripts/gen_proj_string_points.py`
//! (`tests/data/proj_strings.tsv`), transformed from the geographic CRS on the same figure (in
//! the same encoding), so that no datum transformation is involved: forward within 1 mm, and the
//! inverse of PROJ's projected points within 1e-8°. terrio must either match PROJ or refuse the
//! CRS: a projection it does not implement raises, rather than giving other coordinates.
//!
//! The WKT terrio itself writes for each CRS, read back by terrio, is checked too; its known
//! differences belong to #124.

use std::collections::BTreeMap;
use std::path::Path;

use terrio::crs::{SpatialRef, Transformer};

/// Projections terrio does not implement: their PROJ strings (and WKT) are refused.
const UNSUPPORTED: &[&str] = &[
    "airy",
    "august",
    "bacon",
    "bertin1953",
    "bipc",
    "boggs",
    "cc",
    "comill",
    "denoy",
    "gins8",
    "gn_sinu",
    "goode",
    "igh",
    "imw_p",
    "larr",
    "lask",
    "lcca",
    "mbt_fps",
    "mil_os",
    "nicol",
    "nsper",
    "ortel",
    "rouss",
    "tcc",
    "tpers",
    "urm5",
    "urmfps",
    "vandg2",
    "vandg3",
    "vandg4",
    "wag7",
];

/// PROJ strings terrio transforms differently from PROJ, on both figures: `(projection, why)`.
const PROJ_DIFFERS: &[(&str, &str)] = &[(
    "krovak",
    "off Bessel's ellipsoid, PROJ's inverse is not the inverse of its forward: the forward scales by Bessel's semi-major \
     axis and the inverse by the CRS's (the forward agrees)",
)];

/// pyproj's WKT that terrio reads as another CRS than PROJ does, or refuses, on both figures:
/// `(encoding, projection, why)`.
const WKT_DIFFERS: &[(&str, &str, &str)] = &[];

/// terrio's own WKT of a PROJ string that terrio reads back as another CRS, on both figures:
/// `(projection, why)`. Listed ones that now read back are reported, not failed, as the WKT
/// terrio writes is fixed separately (#124).
const OWN_WKT_DIFFERS: &[(&str, &str)] = &[];

fn proj_differs(name: &str) -> bool {
    PROJ_DIFFERS.iter().any(|&(n, _)| n == name)
}

/// A CRS of the reference file in its encodings, and PROJ's points.
#[derive(Default)]
struct Case {
    name: String,
    earth: String,
    /// Each encoding (`proj`, `wkt1`, `wkt2`) and its geographic CRS's (`wkt1_geog`, …); absent
    /// when PROJ rejects the PROJ string, or cannot write the WKT.
    encodings: BTreeMap<String, String>,
    rejected: bool,
    /// Longitude, latitude, PROJ's x and y (infinite where PROJ refuses the point) and PROJ's
    /// inverse of them (NaN without an inverse).
    points: Vec<[f64; 6]>,
}

fn cases() -> Vec<Case> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/proj_strings.tsv");
    let text = std::fs::read_to_string(path).unwrap();
    let mut cases: Vec<Case> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# case ") {
            let (name, earth) = rest.split_once(' ').unwrap();
            cases.push(Case { name: name.into(), earth: earth.into(), ..Case::default() });
            continue;
        }
        let Some(case) = cases.last_mut() else { continue };
        if line == "# rejected" {
            case.rejected = true;
        } else if let Some(rest) = line.strip_prefix("# ") {
            if let Some((key, value)) = rest.split_once(' ').filter(|(_, v)| *v != "-") {
                case.encodings.insert(key.into(), value.into());
            }
        } else if !line.is_empty() {
            let v: Vec<f64> = line.split('\t').map(|c| c.parse().unwrap()).collect();
            let inv = if v.len() == 6 { [v[4], v[5]] } else { [f64::NAN, f64::NAN] };
            case.points.push([v[0], v[1], v[2], v[3], inv[0], inv[1]]);
        }
    }
    cases
}

/// How terrio does on a case's points.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// Every point within the tolerances.
    Matches,
    /// terrio refuses the CRS (or the transformation).
    Refused(String),
    /// terrio transforms, but not as PROJ.
    Differs(String),
}

/// The difference between two longitudes in degrees, across the antimeridian.
fn dlon(a: f64, b: f64) -> f64 {
    ((a - b + 180.0).rem_euclid(360.0) - 180.0).abs()
}

/// Checks a CRS and its geographic CRS, as terrio reads them, against PROJ's points.
fn outcome(crs: terrio::Result<SpatialRef>, geog: terrio::Result<SpatialRef>, points: &[[f64; 6]]) -> Outcome {
    let tr = |a: &terrio::Result<SpatialRef>, b: &terrio::Result<SpatialRef>| match (a, b) {
        (Ok(a), Ok(b)) => Transformer::new(a, b),
        (Err(e), _) | (_, Err(e)) => Err(terrio::Error::Crs(e.to_string())),
    };
    let (fwd, inv) = match (tr(&geog, &crs), tr(&crs, &geog)) {
        (Ok(f), Ok(i)) => (f, i),
        (Err(e), _) | (_, Err(e)) => return Outcome::Refused(e.to_string()),
    };
    let mut worst = (0.0_f64, String::new());
    for &[lon, lat, x, y, ilon, ilat] in points {
        let got = fwd.transform(lon, lat).ok().filter(|p| p.0.is_finite() && p.1.is_finite());
        if !x.is_finite() {
            if let Some(p) = got {
                return Outcome::Differs(format!("forward of ({lon}, {lat}), which PROJ refuses, gives {p:?}"));
            }
            continue;
        }
        let Some((tx, ty)) = got else { return Outcome::Differs(format!("refuses ({lon}, {lat}), which PROJ projects")) };
        let d = (tx - x).hypot(ty - y);
        if d > 1e-3_f64.max(1e-12 * x.hypot(y)) && d > worst.0 {
            worst = (d, format!("forward of ({lon}, {lat}) {d:.3e} m from PROJ"));
        }
        if ilon.is_nan() {
            continue;
        }
        match inv.transform(x, y) {
            Ok((tlon, tlat)) => {
                let d = if ilat.abs() > 89.999 { 0.0 } else { dlon(tlon, ilon) }.max((tlat - ilat).abs());
                if (d.is_nan() || d > 1e-8) && worst.1.is_empty() {
                    worst = (0.0, format!("inverse of ({x}, {y}) {d:.3e}° from PROJ"));
                }
            }
            Err(e) => return Outcome::Differs(format!("refuses the inverse of ({x}, {y}): {e}")),
        }
    }
    if worst.1.is_empty() { Outcome::Matches } else { Outcome::Differs(worst.1) }
}

/// The PROJ string of a case, and its geographic CRS, as terrio reads them.
fn from_proj(c: &Case) -> (terrio::Result<SpatialRef>, terrio::Result<SpatialRef>) {
    let earth = if c.earth == "sphere" { "+R=6371000" } else { "+ellps=WGS84" };
    (SpatialRef::from_proj4(&c.encodings["proj"]), SpatialRef::from_proj4(&format!("+proj=longlat {earth} +no_defs")))
}

/// A case's WKT in an encoding, and its geographic CRS's, as terrio reads them.
fn from_wkt(c: &Case, key: &str) -> Option<(terrio::Result<SpatialRef>, terrio::Result<SpatialRef>)> {
    let wkt = c.encodings.get(key)?;
    let geog = c.encodings.get(&format!("{key}_geog"))?;
    Some((SpatialRef::parse(wkt), SpatialRef::parse(geog)))
}

#[test]
fn proj_strings_match_proj_or_are_refused() {
    let mut problems = Vec::new();
    for c in cases().iter().filter(|c| !c.rejected) {
        let (crs, geog) = from_proj(c);
        let unsupported = UNSUPPORTED.contains(&c.name.as_str());
        let differs = proj_differs(&c.name);
        let label = format!("{} ({})", c.encodings["proj"], c.earth);
        match outcome(crs, geog, &c.points) {
            Outcome::Matches if unsupported || differs => {
                problems.push(format!("{label}: matches PROJ, but is listed as unsupported or differing"))
            }
            Outcome::Matches => {}
            Outcome::Refused(e) if !unsupported => problems.push(format!("{label}: refused ({e}), but not listed as unsupported")),
            Outcome::Refused(_) => {}
            Outcome::Differs(d) if !differs => problems.push(format!("{label}: {d}")),
            Outcome::Differs(_) => {}
        }
    }
    assert!(problems.is_empty(), "{} PROJ strings:\n{}", problems.len(), problems.join("\n"));
}

#[test]
fn unsupported_projections_are_refused_in_every_encoding() {
    for c in cases().iter().filter(|c| UNSUPPORTED.contains(&c.name.as_str())) {
        for key in ["wkt1", "wkt2"] {
            if let Some((crs, geog)) = from_wkt(c, key) {
                let o = outcome(crs, geog, &c.points);
                assert!(matches!(o, Outcome::Refused(_)), "{} ({}) as {key}: not refused: {o:?}", c.name, c.earth);
            }
        }
    }
}

#[test]
fn pyproj_wkt_matches_proj() {
    let mut problems = Vec::new();
    for c in cases().iter().filter(|c| !c.rejected && !UNSUPPORTED.contains(&c.name.as_str())) {
        for key in ["wkt1", "wkt2"] {
            let Some((crs, geog)) = from_wkt(c, key) else { continue };
            let listed = WKT_DIFFERS.iter().any(|&(k, n, _)| k == key && n == c.name);
            match outcome(crs, geog, &c.points) {
                Outcome::Matches if listed => problems.push(format!("{} ({}) as {key}: matches PROJ, but is listed", c.name, c.earth)),
                Outcome::Matches => {}
                // Differs as the PROJ string does: listed there.
                Outcome::Differs(_) if proj_differs(&c.name) => {}
                o if !listed => problems.push(format!("{} ({}) as {key}: {o:?}", c.name, c.earth)),
                _ => {}
            }
        }
    }
    assert!(problems.is_empty(), "{} WKT:\n{}", problems.len(), problems.join("\n"));
}

#[test]
fn own_wkt_reads_back_as_the_same_crs() {
    let mut problems = Vec::new();
    let mut fixed = Vec::new();
    for c in cases().iter().filter(|c| !c.rejected && !UNSUPPORTED.contains(&c.name.as_str())) {
        if proj_differs(&c.name) {
            continue;
        }
        let (crs, geog) = from_proj(c);
        let (Ok(crs), Ok(geog)) = (crs, geog) else { continue };
        let back = |s: &SpatialRef| s.to_wkt().ok_or_else(|| terrio::Error::Crs("no WKT".into())).and_then(|w| SpatialRef::parse(&w));
        let listed = OWN_WKT_DIFFERS.iter().any(|&(n, _)| n == c.name);
        match outcome(back(&crs), back(&geog), &c.points) {
            Outcome::Matches if listed => fixed.push(format!("{} ({})", c.name, c.earth)),
            Outcome::Matches => {}
            o if !listed => problems.push(format!("{} ({}): {o:?}", c.name, c.earth)),
            _ => {}
        }
    }
    if !fixed.is_empty() {
        eprintln!("terrio's WKT now reads back as the same CRS for {}: remove them from OWN_WKT_DIFFERS", fixed.join(", "));
    }
    assert!(problems.is_empty(), "{} CRSs:\n{}", problems.len(), problems.join("\n"));
}

#[test]
fn proj_methods_keep_the_crs_figure_and_units() {
    // A PROJ method's parameters are its projection's: one naming the figure, datum or units, or
    // one given twice, is refused rather than overriding the CRS's.
    let hammer = cases().into_iter().find(|c| c.name == "hammer" && c.earth == "wgs84").unwrap();
    let (wkt, geog) = (&hammer.encodings["wkt2"], &hammer.encodings["wkt2_geog"]);
    let usable = |w: &str| {
        let (crs, geog) = (SpatialRef::parse(w), SpatialRef::parse(geog));
        matches!(outcome(crs, geog, &hammer.points), Outcome::Matches | Outcome::Differs(_))
    };
    assert!(usable(wkt));
    for extra in [r#"PARAMETER["towgs84",100]"#, r#"PARAMETER["R",6371000]"#, r#"PARAMETER["lon_0",10],PARAMETER["lon_0",20]"#] {
        let bad = wkt.replace(r#"METHOD["PROJ hammer"]"#, &format!(r#"METHOD["PROJ hammer"],{extra}"#));
        assert_ne!(&bad, wkt);
        assert!(!usable(&bad), "{extra} accepted");
    }
}
