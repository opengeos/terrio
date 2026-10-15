//! Checks terrio's coordinate transformations against GDAL's reference points from
//! `scripts/gen_epsg_points.py`, shared by the tests without and with datum shift grids.

use std::fmt::Write as _;
use terrio::crs::{SpatialRef, Transformer};

/// Errors of one kind of check, in metres (`None` when terrio could not transform).
#[derive(Default)]
pub struct Tally {
    pub errors: Vec<Option<f64>>,
}

impl Tally {
    pub fn count(&self, max: f64) -> usize {
        self.errors.iter().filter(|e| e.is_some_and(|e| e <= max)).count()
    }

    fn row(&self, name: &str) -> String {
        let n = self.errors.len();
        let failed = self.errors.iter().filter(|e| e.is_none()).count();
        let over = n - failed - self.count(10.0);
        format!("| {name} | {n} | {} | {} | {} | {over} | {failed} |", self.count(0.01), self.count(1.0), self.count(10.0))
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

/// The forward, inverse and CRS-to-CRS tallies of a reference file, `None` when it is missing.
/// Writes the error of every point to the file `TERRIO_EPSG_REPORT` names, when set.
pub fn check(name: &str) -> Option<(Tally, Tally, Tally)> {
    let dir = std::env::var_os("TERRIO_TEST_DATA")?;
    let path = std::path::Path::new(&dir).join(name);
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("{} is missing: run scripts/gen_epsg_points.py", path.display());
        return None;
    };
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let crs = |code: &str| code.parse().ok().and_then(|c| SpatialRef::from_epsg(c).ok());
    let num = |s: &str| s.parse::<f64>().unwrap_or(f64::NAN);
    let (mut fwd, mut inv, mut cross) = (Tally::default(), Tally::default(), Tally::default());
    let mut report = String::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f[..] {
            ["F", code, lon, lat, x, y, kind, unit] => {
                let e = crs(code)
                    .and_then(|c| Transformer::new(&wgs84, &c).ok())
                    .and_then(|t| t.transform(num(lon), num(lat)).ok())
                    .map(|p| distance(kind, num(unit), p, (num(x), num(y))))
                    .filter(|e| e.is_finite());
                let _ = writeln!(report, "F\t{code}\t{}", e.map_or("fail".into(), |e| format!("{e:.4}")));
                fwd.errors.push(e);
            }
            ["I", code, x, y, lon, lat] => {
                let e = crs(code)
                    .and_then(|c| Transformer::new(&c, &wgs84).ok())
                    .and_then(|t| t.transform(num(x), num(y)).ok())
                    .map(|p| distance("g", 1.0, p, (num(lon), num(lat))))
                    .filter(|e| e.is_finite());
                let _ = writeln!(report, "I\t{code}\t{}", e.map_or("fail".into(), |e| format!("{e:.4}")));
                inv.errors.push(e);
            }
            ["X", src, dst, x, y, x2, y2, kind, unit] => {
                let e = crs(src)
                    .zip(crs(dst))
                    .and_then(|(s, d)| Transformer::new(&s, &d).ok())
                    .and_then(|t| t.transform(num(x), num(y)).ok())
                    .map(|p| distance(kind, num(unit), p, (num(x2), num(y2))))
                    .filter(|e| e.is_finite());
                let _ = writeln!(report, "X\t{src}\t{dst}\t{}", e.map_or("fail".into(), |e| format!("{e:.4}")));
                cross.errors.push(e);
            }
            _ => {}
        }
    }
    if let Some(p) = std::env::var_os("TERRIO_EPSG_REPORT") {
        std::fs::write(p, &report).unwrap();
    }
    println!("| check | points | ≤ 1 cm | ≤ 1 m | ≤ 10 m | > 10 m | failed |");
    println!("|---|---|---|---|---|---|---|");
    println!("{}", fwd.row("forward (WGS 84 to CRS)"));
    println!("{}", inv.row("inverse (CRS to WGS 84)"));
    println!("{}", cross.row("CRS to CRS"));
    Some((fwd, inv, cross))
}
