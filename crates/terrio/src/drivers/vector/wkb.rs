//! Well-known binary geometries, decoded straight from the bytes.

use wbvector::{Coord, Geometry, Ring};

/// Decodes WKB as `Geometry::from_wkb` does (the same geometries, and `None` where it fails),
/// reading from the slice directly and allocating coordinate vectors at their final size.
pub(crate) fn from_wkb(b: &[u8]) -> Option<Geometry> {
    Wkb { b, pos: 0 }.geometry()
}

/// A WKB reader.
struct Wkb<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Wkb<'_> {
    fn bytes<const N: usize>(&mut self) -> Option<[u8; N]> {
        let v = self.b.get(self.pos..self.pos + N)?.try_into().ok()?;
        self.pos += N;
        Some(v)
    }

    fn u32(&mut self, le: bool) -> Option<u32> {
        let b = self.bytes::<4>()?;
        Some(if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn f64(&mut self, le: bool) -> Option<f64> {
        let b = self.bytes::<8>()?;
        Some(if le { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) })
    }

    fn coord(&mut self, le: bool, z: bool, m: bool) -> Option<Coord> {
        let x = self.f64(le)?;
        let y = self.f64(le)?;
        let z = if z { Some(self.f64(le)?) } else { None };
        let m = if m { Some(self.f64(le)?) } else { None };
        Some(Coord { x, y, z, m })
    }

    fn coords(&mut self, le: bool, z: bool, m: bool) -> Option<Vec<Coord>> {
        let n = self.u32(le)? as u64;
        let size = 16 + 8 * (z as u64 + m as u64);
        // Fewer bytes than the coordinates need: reading them would fail.
        if n.checked_mul(size)? > (self.b.len() - self.pos) as u64 {
            return None;
        }
        let mut cs = Vec::with_capacity(n as usize);
        for _ in 0..n {
            cs.push(self.coord(le, z, m)?);
        }
        Some(cs)
    }

    fn ring(&mut self, le: bool, z: bool, m: bool) -> Option<Ring> {
        let mut cs = self.coords(le, z, m)?;
        // The closing point is dropped.
        if cs.len() > 1 && cs.first() == cs.last() {
            cs.pop();
        }
        Some(Ring::new(cs))
    }

    /// The members of a collection, each a WKB geometry, mapped by `f`.
    fn members<T>(&mut self, le: bool, f: impl Fn(Geometry) -> Option<T>) -> Option<Vec<T>> {
        let n = self.u32(le)? as usize;
        // Each member takes at least 5 bytes.
        let mut out = Vec::with_capacity(n.min((self.b.len() - self.pos) / 5));
        for _ in 0..n {
            out.push(f(self.geometry()?)?);
        }
        Some(out)
    }

    fn geometry(&mut self) -> Option<Geometry> {
        let le = self.bytes::<1>()?[0] == 1;
        let raw = self.u32(le)?;
        // ISO and EWKB type variants.
        let (base, z, m) = if raw & 0xFFFF_0000 != 0 {
            (raw & 0x0000_FFFF, raw & 0x8000_0000 != 0, raw & 0x4000_0000 != 0)
        } else if raw > 3000 {
            (raw - 3000, true, true)
        } else if raw > 2000 {
            (raw - 2000, false, true)
        } else if raw > 1000 {
            (raw - 1000, true, false)
        } else {
            (raw, false, false)
        };
        Some(match base {
            1 => Geometry::Point(self.coord(le, z, m)?),
            2 => Geometry::LineString(self.coords(le, z, m)?),
            3 => {
                let n = self.u32(le)? as usize;
                if n == 0 {
                    return Some(Geometry::Polygon { exterior: Ring::default(), interiors: vec![] });
                }
                let exterior = self.ring(le, z, m)?;
                let mut interiors = Vec::with_capacity((n - 1).min((self.b.len() - self.pos) / 4));
                for _ in 1..n {
                    interiors.push(self.ring(le, z, m)?);
                }
                Geometry::Polygon { exterior, interiors }
            }
            4 => Geometry::MultiPoint(self.members(le, |g| match g {
                Geometry::Point(c) => Some(c),
                _ => None,
            })?),
            5 => Geometry::MultiLineString(self.members(le, |g| match g {
                Geometry::LineString(cs) => Some(cs),
                _ => None,
            })?),
            6 => Geometry::MultiPolygon(self.members(le, |g| match g {
                Geometry::Polygon { exterior, interiors } => Some((exterior, interiors)),
                _ => None,
            })?),
            7 => Geometry::GeometryCollection(self.members(le, Some)?),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_wkb_as_wbvector() {
        let ring = |o: f64| vec![Coord::xy(o, 0.0), Coord::xy(o + 1.0, 0.0), Coord::xy(o, 1.0), Coord::xy(o, 0.0)];
        let poly = Geometry::polygon(ring(0.0), vec![ring(0.2)]);
        let geoms = vec![
            Geometry::point(1.5, -2.0),
            Geometry::Point(Coord { x: 1.0, y: 2.0, z: Some(3.0), m: Some(4.0) }),
            Geometry::line_string(ring(5.0)),
            poly.clone(),
            Geometry::MultiPolygon(vec![(Ring::new(ring(0.0)), vec![]), (Ring::new(ring(3.0)), vec![Ring::new(ring(3.1))])]),
            Geometry::MultiPoint(vec![Coord::xy(1.0, 1.0), Coord { x: 2.0, y: 2.0, z: Some(1.0), m: None }]),
            Geometry::MultiLineString(vec![ring(1.0), vec![]]),
            Geometry::GeometryCollection(vec![poly, Geometry::point(0.0, 0.0)]),
            Geometry::Polygon { exterior: Ring::default(), interiors: vec![] },
        ];
        for g in &geoms {
            let wkb = g.to_wkb();
            assert_eq!(from_wkb(&wkb), Geometry::from_wkb(&wkb).ok(), "{g:?}");
            // Truncated and corrupted encodings fail (or not) alike.
            for cut in 0..wkb.len() {
                assert_eq!(from_wkb(&wkb[..cut]), Geometry::from_wkb(&wkb[..cut]).ok(), "{g:?} cut at {cut}");
            }
            for at in 0..wkb.len().min(16) {
                let mut bad = wkb.clone();
                bad[at] ^= 0xff;
                assert_eq!(from_wkb(&bad), Geometry::from_wkb(&bad).ok(), "{g:?} corrupted at {at}");
            }
        }
        // Big-endian and EWKB/ISO type codes.
        let mut be = vec![0u8];
        be.extend(1001u32.to_be_bytes());
        for v in [1.0f64, 2.0, 3.0] {
            be.extend(v.to_be_bytes());
        }
        assert_eq!(from_wkb(&be), Geometry::from_wkb(&be).ok());
        let mut ewkb = vec![1u8];
        ewkb.extend((0x8000_0001u32).to_le_bytes());
        for v in [1.0f64, 2.0, 3.0] {
            ewkb.extend(v.to_le_bytes());
        }
        assert_eq!(from_wkb(&ewkb), Geometry::from_wkb(&ewkb).ok());
    }
}
