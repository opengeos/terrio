//! Checks terrio's coordinate transformations against GDAL's reference points from
//! `scripts/gen_epsg_points.py`, shared by the tests without and with datum shift grids.
//!
//! The reference has many points per CRS (50 of each kind by default); they are grouped by CRS
//! (by pair of CRSs from CRS to CRS), and a CRS counts with its worst point.

use std::collections::HashMap;
use std::fmt::Write as _;
use terrio::crs::{SpatialRef, Transformer};

/// The errors of one kind of check, one per CRS (or pair of CRSs): the largest over its points,
/// in metres, `None` when terrio could not transform one of them.
#[derive(Default)]
pub struct Tally {
    pub errors: Vec<Option<f64>>,
    /// The number of points over all CRSs.
    pub points: usize,
}

impl Tally {
    /// The number of CRSs whose every point is within `max` metres.
    pub fn count(&self, max: f64) -> usize {
        self.errors.iter().filter(|e| e.is_some_and(|e| e <= max)).count()
    }

    fn row(&self, name: &str) -> String {
        let n = self.errors.len();
        let failed = self.errors.iter().filter(|e| e.is_none()).count();
        let over = n - failed - self.count(10.0);
        format!(
            "| {name} | {n} | {} | {} | {} | {} | {over} | {failed} |",
            self.points,
            self.count(0.01),
            self.count(1.0),
            self.count(10.0)
        )
    }
}

/// The distance in metres between two points in a CRS's units.
fn distance(kind: &str, unit: f64, (x, y): (f64, f64), (gx, gy): (f64, f64)) -> f64 {
    if kind == "g" {
        let lat = (gy * unit).to_radians();
        let dx = (x - gx) * unit * lat.cos();
        let dy = (y - gy) * unit;
        111_320.0 * dx.hypot(dy)
    } else {
        ((x - gx) * unit).hypot((y - gy) * unit)
    }
}

/// The points of one CRS (or pair) and one kind of check, in the reference file's order.
struct Group<'a> {
    kind: &'a str,
    /// The EPSG code, or the source and target codes from CRS to CRS.
    codes: Vec<&'a str>,
    /// Each point's fields after the codes.
    points: Vec<Vec<&'a str>>,
}

/// The worst point of a group: its error (`None` when terrio could not transform it), its input
/// coordinates, how many points there are and how many terrio could not transform.
struct Worst {
    error: Option<f64>,
    at: (f64, f64),
    n: usize,
    failed: usize,
}

fn worst(g: &Group<'_>, wgs84: &SpatialRef) -> Worst {
    let crs = |code: &str| code.parse().ok().and_then(|c| SpatialRef::from_epsg(c).ok());
    let num = |s: &str| s.parse::<f64>().unwrap_or(f64::NAN);
    // The transformation, made once for the group's points.
    let t = match g.kind {
        "F" => crs(g.codes[0]).and_then(|c| Transformer::new(wgs84, &c).ok()),
        "I" => crs(g.codes[0]).and_then(|c| Transformer::new(&c, wgs84).ok()),
        _ => crs(g.codes[0]).zip(crs(g.codes[1])).and_then(|(s, d)| Transformer::new(&s, &d).ok()),
    };
    let (mut max, mut at, mut failed) = (0.0_f64, (f64::NAN, f64::NAN), 0);
    for p in &g.points {
        // Every kind has the input, then the reference output; F and X then the output's kind and
        // unit, I being geographic in degrees.
        let (input, reference) = ((num(p[0]), num(p[1])), (num(p[2]), num(p[3])));
        let (kind, unit) = if g.kind == "I" { ("g", 1.0) } else { (p[4], num(p[5])) };
        let e = t
            .as_ref()
            .and_then(|t| t.transform(input.0, input.1).ok())
            .map(|q| distance(kind, unit, q, reference))
            .filter(|e| e.is_finite());
        match e {
            None => {
                if failed == 0 {
                    at = input;
                }
                failed += 1;
            }
            Some(e) if failed == 0 && (e > max || at.0.is_nan()) => (max, at) = (e, input),
            Some(_) => {}
        }
    }
    Worst { error: (failed == 0).then_some(max), at, n: g.points.len(), failed }
}

/// The forward, inverse and CRS-to-CRS tallies of a reference file, `None` when it is missing.
/// Writes a row per CRS (and kind of check) to the file `TERRIO_EPSG_REPORT` names, when set:
/// `kind codes… points failed worst x y`, `worst` being the largest error in metres (`fail` when
/// terrio could not transform a point) and `x y` the input of that point (or of the first that
/// failed).
pub fn check(name: &str) -> Option<(Tally, Tally, Tally)> {
    let dir = std::env::var_os("TERRIO_TEST_DATA")?;
    let path = std::path::Path::new(&dir).join(name);
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("{} is missing: run scripts/gen_epsg_points.py", path.display());
        return None;
    };
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let mut groups: Vec<Group<'_>> = Vec::new();
    let mut index: HashMap<(&str, &str, &str), usize> = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let (kind, codes, rest) = match f[..] {
            ["F", code, ..] if f.len() == 8 => ("F", vec![code], &f[2..]),
            ["I", code, ..] if f.len() == 6 => ("I", vec![code], &f[2..]),
            ["X", src, dst, ..] if f.len() == 9 => ("X", vec![src, dst], &f[3..]),
            _ => continue,
        };
        let key = (kind, codes[0], codes.get(1).copied().unwrap_or(""));
        let i = *index.entry(key).or_insert_with(|| {
            groups.push(Group { kind, codes, points: Vec::new() });
            groups.len() - 1
        });
        groups[i].points.push(rest.to_vec());
    }
    let (mut fwd, mut inv, mut cross) = (Tally::default(), Tally::default(), Tally::default());
    let mut report = String::new();
    for g in &groups {
        let w = worst(g, &wgs84);
        let tally = match g.kind {
            "F" => &mut fwd,
            "I" => &mut inv,
            _ => &mut cross,
        };
        tally.errors.push(w.error);
        tally.points += w.n;
        let error = w.error.map_or("fail".into(), |e| format!("{e:.4}"));
        let _ = writeln!(report, "{}\t{}\t{}\t{}\t{error}\t{}\t{}", g.kind, g.codes.join("\t"), w.n, w.failed, w.at.0, w.at.1);
    }
    if let Some(p) = std::env::var_os("TERRIO_EPSG_REPORT") {
        std::fs::write(p, &report).unwrap();
    }
    println!("CRSs whose every point is within a distance of GDAL's:");
    println!("| check | CRSs | points | ≤ 1 cm | ≤ 1 m | ≤ 10 m | > 10 m | failed |");
    println!("|---|---|---|---|---|---|---|---|");
    println!("{}", fwd.row("forward (WGS 84 to CRS)"));
    println!("{}", inv.row("inverse (CRS to WGS 84)"));
    println!("{}", cross.row("CRS to CRS"));
    Some((fwd, inv, cross))
}
