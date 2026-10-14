//! Well-known text geometries.

use std::fmt::Write as _;
use wbvector::{Coord, Geometry, Ring};

/// Formats a geometry as WKT the way GDAL does: `POINT (1 2)`, `POLYGON Z ((...))`, with closed
/// rings.
pub fn format(g: &Geometry) -> String {
    let mut s = String::new();
    let z = crate::vector::geometry_has_z(g);
    write_geometry(&mut s, g, z);
    s
}

fn write_geometry(s: &mut String, g: &Geometry, z: bool) {
    let name = match g {
        Geometry::Point(_) => "POINT",
        Geometry::LineString(_) => "LINESTRING",
        Geometry::Polygon { .. } => "POLYGON",
        Geometry::MultiPoint(_) => "MULTIPOINT",
        Geometry::MultiLineString(_) => "MULTILINESTRING",
        Geometry::MultiPolygon(_) => "MULTIPOLYGON",
        Geometry::GeometryCollection(_) => "GEOMETRYCOLLECTION",
    };
    s.push_str(name);
    if z && !matches!(g, Geometry::GeometryCollection(_)) {
        s.push_str(" Z");
    }
    s.push(' ');
    match g {
        Geometry::Point(c) => write_coords(s, std::slice::from_ref(c), false, z),
        Geometry::LineString(cs) => write_coords(s, cs, false, z),
        Geometry::MultiPoint(cs) => {
            s.push('(');
            for (i, c) in cs.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                write_coords(s, std::slice::from_ref(c), false, z);
            }
            s.push(')');
        }
        Geometry::MultiLineString(ls) => {
            s.push('(');
            for (i, l) in ls.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                write_coords(s, l, false, z);
            }
            s.push(')');
        }
        Geometry::Polygon { exterior, interiors } => write_polygon(s, exterior, interiors, z),
        Geometry::MultiPolygon(ps) => {
            s.push('(');
            for (i, (e, h)) in ps.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                write_polygon(s, e, h, z);
            }
            s.push(')');
        }
        Geometry::GeometryCollection(gs) => {
            s.push('(');
            for (i, g) in gs.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                write_geometry(s, g, z);
            }
            s.push(')');
        }
    }
}

fn write_polygon(s: &mut String, exterior: &Ring, interiors: &[Ring], z: bool) {
    s.push('(');
    for (i, r) in std::iter::once(exterior).chain(interiors).enumerate() {
        if i > 0 {
            s.push(',');
        }
        write_coords(s, &r.0, true, z);
    }
    s.push(')');
}

fn write_coords(s: &mut String, cs: &[Coord], close: bool, z: bool) {
    s.push('(');
    let first = cs.first().filter(|f| close && cs.len() > 1 && cs.last().map(|l| (l.x, l.y)) != Some((f.x, f.y)));
    for (i, c) in cs.iter().chain(first).enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{} {}", c.x, c.y);
        if z {
            let _ = write!(s, " {}", c.z.unwrap_or(0.0));
        }
    }
    s.push(')');
}

/// Parses a WKT geometry such as `POINT (1 2)` or `MULTIPOLYGON Z (((...)))`, with an optional
/// EWKT `SRID=4326;` prefix. Returns `None` for empty geometries and text that is not WKT.
pub fn parse(text: &str) -> Option<Geometry> {
    let text = text.trim();
    let text = match text.get(..5) {
        Some(p) if p.eq_ignore_ascii_case("SRID=") => text.split_once(';')?.1,
        _ => text,
    };
    let mut p = Parser { s: text.as_bytes(), i: 0 };
    let g = p.geometry()?;
    p.skip_ws();
    (p.i == p.s.len()).then_some(g).flatten()
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

#[derive(Clone, Copy)]
struct Dims {
    z: bool,
    m: bool,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn word(&mut self) -> String {
        self.skip_ws();
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_alphabetic() {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).to_ascii_uppercase()
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.s.get(self.i).copied()
    }

    /// Parses a geometry. The outer `Option` fails on bad syntax; the inner is `None` for EMPTY.
    fn geometry(&mut self) -> Option<Option<Geometry>> {
        let kind = self.word();
        let mut dims = Dims { z: false, m: false };
        let mut tag = kind;
        if !is_kind(&tag) {
            // Joined forms such as POINTZ.
            for suffix in ["ZM", "Z", "M"] {
                if let Some(base) = tag.strip_suffix(suffix)
                    && is_kind(base)
                {
                    dims = Dims { z: suffix.contains('Z'), m: suffix.contains('M') };
                    tag = base.to_string();
                    break;
                }
            }
        }
        if !is_kind(&tag) {
            return None;
        }
        let save = self.i;
        match self.word().as_str() {
            "ZM" => dims = Dims { z: true, m: true },
            "Z" => dims = Dims { z: true, m: false },
            "M" => dims = Dims { z: false, m: true },
            "EMPTY" => return Some(None),
            "" => self.i = save,
            _ => return None,
        }
        let save = self.i;
        if self.word() == "EMPTY" {
            return Some(None);
        }
        self.i = save;
        let g = match tag.as_str() {
            "POINT" => Geometry::Point(self.coords(dims)?.into_iter().next()?),
            "LINESTRING" => Geometry::LineString(self.coords(dims)?),
            "POLYGON" => {
                let (exterior, interiors) = self.polygon(dims)?;
                Geometry::Polygon { exterior, interiors }
            }
            "MULTIPOINT" => {
                // Both MULTIPOINT (1 2, 3 4) and MULTIPOINT ((1 2), (3 4)).
                if !self.eat(b'(') {
                    return None;
                }
                let mut pts = Vec::new();
                loop {
                    if self.peek() == Some(b'(') {
                        pts.extend(self.coords(dims)?);
                    } else {
                        pts.push(self.coord(dims)?);
                    }
                    if !self.eat(b',') {
                        break;
                    }
                }
                if !self.eat(b')') {
                    return None;
                }
                Geometry::MultiPoint(pts)
            }
            "MULTILINESTRING" => Geometry::MultiLineString(self.list(|p| p.coords(dims))?),
            "MULTIPOLYGON" => Geometry::MultiPolygon(self.list(|p| p.polygon(dims))?),
            "GEOMETRYCOLLECTION" => Geometry::GeometryCollection(self.list(|p| p.geometry())?.into_iter().flatten().collect()),
            _ => return None,
        };
        Some(Some(g))
    }

    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        if !self.eat(b'(') {
            return None;
        }
        let mut out = Vec::new();
        loop {
            out.push(item(self)?);
            if !self.eat(b',') {
                break;
            }
        }
        self.eat(b')').then_some(out)
    }

    fn polygon(&mut self, dims: Dims) -> Option<(Ring, Vec<Ring>)> {
        let mut rings = self.list(|p| p.coords(dims))?.into_iter().map(|mut cs| {
            if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
                cs.pop();
            }
            Ring::new(cs)
        });
        let exterior = rings.next()?;
        Some((exterior, rings.collect()))
    }

    fn coords(&mut self, dims: Dims) -> Option<Vec<Coord>> {
        self.list(|p| p.coord(dims))
    }

    fn number(&mut self) -> Option<f64> {
        self.skip_ws();
        let start = self.i;
        while self.i < self.s.len() && matches!(self.s[self.i], b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E') {
            self.i += 1;
        }
        let t = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        if t.is_empty() {
            let w = self.word();
            return (w == "NAN").then_some(f64::NAN);
        }
        t.parse().ok()
    }

    fn coord(&mut self, dims: Dims) -> Option<Coord> {
        let x = self.number()?;
        let y = self.number()?;
        let mut vals = Vec::new();
        while matches!(self.peek(), Some(b'0'..=b'9' | b'.' | b'-' | b'+' | b'N' | b'n')) {
            vals.push(self.number()?);
        }
        let (z, m) = match (dims.z, dims.m, vals.as_slice()) {
            (_, _, [z, m, ..]) => (Some(*z), Some(*m)),
            (false, true, [m]) => (None, Some(*m)),
            (_, _, [z]) => (Some(*z), None),
            _ => (None, None),
        };
        Some(Coord { x, y, z, m })
    }
}

fn is_kind(s: &str) -> bool {
    matches!(s, "POINT" | "LINESTRING" | "POLYGON" | "MULTIPOINT" | "MULTILINESTRING" | "MULTIPOLYGON" | "GEOMETRYCOLLECTION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wkt() {
        assert_eq!(parse("POINT (1 2)"), Some(Geometry::point(1.0, 2.0)));
        assert_eq!(parse("point z (1 2 3)"), Some(Geometry::point_z(1.0, 2.0, 3.0)));
        assert_eq!(parse("POINTZ (1 2 3)"), Some(Geometry::point_z(1.0, 2.0, 3.0)));
        assert_eq!(parse("SRID=4326;POINT(1 2)"), Some(Geometry::point(1.0, 2.0)));
        assert_eq!(parse("MULTIPOINT ((1 2), (3 4))"), parse("MULTIPOINT (1 2, 3 4)"));
        let Some(Geometry::Polygon { exterior, interiors }) = parse("POLYGON ((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 1))") else {
            panic!("not a polygon")
        };
        assert_eq!((exterior.len(), interiors.len(), interiors[0].len()), (4, 1, 3));
        let Some(Geometry::MultiPolygon(ps)) = parse("MULTIPOLYGON (((0 0, 1 0, 1 1, 0 0)), ((5 5, 6 5, 6 6, 5 5)))") else {
            panic!("not a multipolygon")
        };
        assert_eq!(ps.len(), 2);
        let Some(Geometry::GeometryCollection(gs)) = parse("GEOMETRYCOLLECTION (POINT (1 2), LINESTRING (0 0, 1 1))") else {
            panic!("not a collection")
        };
        assert_eq!(gs.len(), 2);
        assert_eq!(parse("POINT EMPTY"), None);
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("POINT (1 2) junk"), None);
        // What is written parses back.
        for wkt in [
            "MULTILINESTRING ((0 0,1 1),(2 2,3 3))",
            "POLYGON Z ((0 0 1,4 0 1,4 4 1,0 0 1))",
            "MULTIPOINT ((1 2),(3 4))",
            "GEOMETRYCOLLECTION (POINT (1 2),LINESTRING (0 0,1 1))",
        ] {
            let g = parse(wkt).unwrap();
            assert_eq!(format(&g), wkt);
        }
    }
}
