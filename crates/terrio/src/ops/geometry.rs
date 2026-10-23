//! Planar centroids, multipart splitting and vertex extraction. Coordinates stay in the input
//! CRS; reproject geographic data first when a centroid in linear units is needed.

use crate::{Error, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Result};
use wbvector::{Coord, Ring};

/// One planar, two-dimensional centroid per input feature. Polygon centroids are weighted by
/// area (subtracting holes), line centroids by length, and point centroids equally. Collections
/// use their highest non-degenerate dimension. Empty and null geometries produce null geometry;
/// attributes, feature IDs and CRS are preserved. Non-finite XY coordinates are rejected.
///
/// ```no_run
/// let ds = terrio::open_vector("countries.gpkg", &Default::default())?;
/// let points = terrio::ops::geometry::centroids(&ds.layers[0])?;
/// terrio::write_vector(&[points], "centroids.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn centroids(layer: &Layer) -> Result<Layer> {
    let mut out = header(layer, Some(GeometryType::Point));
    for f in &layer.features {
        let geometry = match &f.geometry {
            Some(g) => {
                let coords = g.all_coords();
                if coords.iter().any(|c| !c.x.is_finite() || !c.y.is_finite()) {
                    return Err(Error::InvalidArgument(format!("feature {} has non-finite XY coordinates", f.fid)));
                }
                coords.first().and_then(|origin| {
                    let mut a = Centroid { origin: (origin.x, origin.y), ..Default::default() };
                    a.geometry(g);
                    a.point().map(Geometry::Point)
                })
            }
            None => None,
        };
        out.features.push(Feature { fid: f.fid, geometry, attributes: f.attributes.clone() });
    }
    Ok(out)
}

/// Recursively splits multipart geometries and collections, keeping each part's attributes,
/// Z/M coordinates and CRS. Null geometries and empty single geometries are retained; empty
/// multipart geometries produce no features. Output feature IDs are sequential, starting at 1.
///
/// ```no_run
/// let ds = terrio::open_vector("countries.gpkg", &Default::default())?;
/// let parts = terrio::ops::geometry::singleparts(&ds.layers[0]);
/// terrio::write_vector(&[parts], "parts.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn singleparts(layer: &Layer) -> Layer {
    use GeometryType as T;
    let ty = layer.geom_type.and_then(|t| match t {
        T::MultiPoint => Some(T::Point),
        T::MultiLineString => Some(T::LineString),
        T::MultiPolygon => Some(T::Polygon),
        T::GeometryCollection => None,
        t => Some(t),
    });
    let mut out = header(layer, ty);
    for f in &layer.features {
        let parts = f.geometry.as_ref().map_or_else(|| vec![None], |g| super::vector_filter::explode(g).into_iter().map(Some).collect());
        for geometry in parts {
            out.features.push(Feature { fid: out.features.len() as u64 + 1, geometry, attributes: f.attributes.clone() });
        }
    }
    out
}

/// Extracts every vertex, including each ring's repeated closing vertex, into a point
/// layer. Z/M and source attributes are preserved. Added fields are `source_fid` (text, to retain
/// all u64 IDs), `part_index`, `ring_index` (null outside polygons) and `vertex_index`, all indices
/// starting at 0. Parts are numbered in recursive traversal order; polygon exteriors are ring 0.
/// Field name collisions are resolved with `_2`, `_3`, etc. Empty/null geometries yield no points.
///
/// ```no_run
/// let ds = terrio::open_vector("roads.gpkg", &Default::default())?;
/// let nodes = terrio::ops::geometry::vertices(&ds.layers[0]);
/// terrio::write_vector(&[nodes], "nodes.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn vertices(layer: &Layer) -> Layer {
    let mut out = header(layer, Some(GeometryType::Point));
    for (name, ty) in [
        ("source_fid", FieldType::Text),
        ("part_index", FieldType::Integer),
        ("ring_index", FieldType::Integer),
        ("vertex_index", FieldType::Integer),
    ] {
        let name = unique_field(&out, name);
        out.schema.add_field(FieldDef::new(name, ty));
    }
    for f in &layer.features {
        let Some(g) = &f.geometry else { continue };
        for (part, g) in super::vector_filter::explode(g).iter().enumerate() {
            let mut emit = |coords: &[Coord], ring: Option<usize>| {
                // Drivers may omit a ring's closing coordinate in the data model. Include it
                // consistently, whether or not the source format stored it explicitly.
                let close = ring.is_some() && coords.first().zip(coords.last()).is_some_and(|(a, b)| a.x != b.x || a.y != b.y);
                for (vertex, c) in coords.iter().chain(coords.first().filter(|_| close)).enumerate() {
                    let mut attributes = f.attributes.clone();
                    attributes.resize(layer.schema.len(), FieldValue::Null);
                    attributes.extend([
                        FieldValue::Text(f.fid.to_string()),
                        FieldValue::Integer(part as i64),
                        ring.map_or(FieldValue::Null, |i| FieldValue::Integer(i as i64)),
                        FieldValue::Integer(vertex as i64),
                    ]);
                    out.features.push(Feature {
                        fid: out.features.len() as u64 + 1,
                        geometry: Some(Geometry::Point(c.clone())),
                        attributes,
                    });
                }
            };
            match g {
                Geometry::Point(c) => emit(std::slice::from_ref(c), None),
                Geometry::LineString(cs) => emit(cs, None),
                Geometry::Polygon { exterior, interiors } => {
                    for (ring, r) in std::iter::once(exterior).chain(interiors).enumerate() {
                        emit(&r.0, Some(ring));
                    }
                }
                _ => unreachable!("explode returns simple geometries"),
            }
        }
    }
    out
}

pub(crate) fn header(layer: &Layer, ty: Option<GeometryType>) -> Layer {
    let mut out = Layer::new(layer.name.clone());
    out.crs = layer.crs.clone();
    out.schema = layer.schema.clone();
    out.geom_type = ty;
    out
}

pub(crate) fn unique_field(layer: &Layer, name: &str) -> String {
    let mut candidate = name.to_string();
    let mut suffix = 2;
    while layer.schema.fields().iter().any(|f| f.name.eq_ignore_ascii_case(&candidate)) {
        candidate = format!("{name}_{suffix}");
        suffix += 1;
    }
    candidate
}

#[derive(Default)]
struct Centroid {
    origin: (f64, f64),
    // Weighted sums in each dimension. Subtracting an origin avoids loss of precision for
    // small polygons with large projected coordinates.
    sums: [(f64, f64, f64); 3],
}

impl Centroid {
    fn add(&mut self, dim: usize, x: f64, y: f64, weight: f64) {
        let a = &mut self.sums[dim];
        a.0 += x * weight;
        a.1 += y * weight;
        a.2 += weight;
    }

    fn line(&mut self, cs: &[Coord], close: bool) {
        for c in cs {
            self.add(0, c.x - self.origin.0, c.y - self.origin.1, 1.0);
        }
        for (a, b) in cs.iter().zip(cs.iter().skip(1).chain(cs.first().filter(|_| close))) {
            let (ax, ay, bx, by) = (a.x - self.origin.0, a.y - self.origin.1, b.x - self.origin.0, b.y - self.origin.1);
            self.add(1, (ax + bx) / 2.0, (ay + by) / 2.0, (bx - ax).hypot(by - ay));
        }
    }

    fn ring(&mut self, r: &Ring, sign: f64) {
        self.line(&r.0, true);
        let (mut area, mut x, mut y) = (0.0, 0.0, 0.0);
        for (a, b) in r.0.iter().zip(r.0.iter().skip(1).chain(r.0.first())) {
            let (ax, ay, bx, by) = (a.x - self.origin.0, a.y - self.origin.1, b.x - self.origin.0, b.y - self.origin.1);
            let cross = ax * by - bx * ay;
            area += cross;
            x += (ax + bx) * cross;
            y += (ay + by) * cross;
        }
        if area != 0.0 {
            self.add(2, x / (3.0 * area), y / (3.0 * area), sign * area.abs());
        }
    }

    fn geometry(&mut self, g: &Geometry) {
        match g {
            Geometry::Point(c) => self.add(0, c.x - self.origin.0, c.y - self.origin.1, 1.0),
            Geometry::MultiPoint(cs) => cs.iter().for_each(|c| self.add(0, c.x - self.origin.0, c.y - self.origin.1, 1.0)),
            Geometry::LineString(cs) => self.line(cs, false),
            Geometry::MultiLineString(ls) => ls.iter().for_each(|cs| self.line(cs, false)),
            Geometry::Polygon { exterior, interiors } => {
                self.ring(exterior, 1.0);
                interiors.iter().for_each(|r| self.ring(r, -1.0));
            }
            Geometry::MultiPolygon(ps) => {
                for (e, hs) in ps {
                    self.ring(e, 1.0);
                    hs.iter().for_each(|r| self.ring(r, -1.0));
                }
            }
            Geometry::GeometryCollection(gs) => gs.iter().for_each(|g| self.geometry(g)),
        }
    }

    fn point(&self) -> Option<Coord> {
        self.sums.iter().rev().find(|s| s.2 > 0.0).map(|&(x, y, w)| Coord::xy(self.origin.0 + x / w, self.origin.1 + y / w))
    }
}
