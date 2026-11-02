//! Planar spatial SQL subset, independently implemented against GDAL 3.13.3 SQLite.
use super::*;
use crate::{Geometry, GeometryType};
use geo::{Area, Centroid, Intersects};

pub(super) fn validate_arity(name: &str, n: usize) -> Result<()> {
    let expected = match name {
        "ST_AREA" | "ST_LENGTH" | "ST_CENTROID" => 1,
        "ST_INTERSECTS" | "ST_BUFFER" | "ST_TRANSFORM" => 2,
        _ => return Err(syntax(format!("unsupported function '{name}'"))),
    };
    if n != expected {
        return Err(syntax(format!("{name} requires {expected} arguments")));
    }
    Ok(())
}

pub(super) fn is_geometry(e: &Expr) -> bool {
    matches!(e, Expr::Geometry | Expr::Transform(..))
        || matches!(e, Expr::Call(n, _) if matches!(n.as_str(), "ST_BUFFER" | "ST_TRANSFORM" | "ST_CENTROID"))
}

pub(super) fn validate(name: &str, args: &[Expr]) -> Result<()> {
    if !is_geometry(&args[0]) && !matches!(args[0], Expr::Literal(Value::Null)) {
        return Err(syntax(format!("{name} requires a geometry argument")));
    }
    if name == "ST_INTERSECTS" && !is_geometry(&args[1]) && !matches!(args[1], Expr::Literal(Value::Null)) {
        return Err(syntax("ST_Intersects requires two geometries".into()));
    }
    Ok(())
}

fn epsg(e: &Expr) -> Option<u32> {
    if let Expr::Literal(Value::Int(n)) = e { u32::try_from(*n).ok().filter(|n| *n > 0) } else { None }
}
pub(super) fn output_epsg(e: &Expr, source: Option<u32>) -> Option<u32> {
    match e {
        Expr::Transform(_, t) => Some(t.target),
        Expr::Call(n, a) if n == "ST_TRANSFORM" => epsg(&a[1]),
        Expr::Call(_, a) => output_epsg(&a[0], source),
        _ => source,
    }
}
pub(super) fn output_type(e: &Expr, source: Option<GeometryType>) -> Option<GeometryType> {
    match e {
        Expr::Call(n, _) if n == "ST_CENTROID" => Some(GeometryType::Point),
        Expr::Call(n, _) if n == "ST_BUFFER" => None,
        Expr::Transform(e, _) => output_type(e, source),
        Expr::Call(_, a) => output_type(&a[0], source),
        _ => source,
    }
}

pub(super) fn evaluate(name: &str, args: &[Expr], f: &Feature) -> Value {
    let Value::Geometry(g) = eval(&args[0], f) else {
        // SpatiaLite predicates use -1 for an unknown geometry.
        return if name == "ST_INTERSECTS" { Value::Int(-1) } else { Value::Null };
    };
    let shape = super::super::topology::to_geo(&g);
    match name {
        "ST_AREA" => Value::Float(shape.unsigned_area()),
        "ST_LENGTH" => Value::Float(length(&g)),
        "ST_CENTROID" => {
            shape.centroid().map(|p| Value::Geometry(Box::new(Geometry::Point(wbvector::Coord::xy(p.x(), p.y()))))).unwrap_or(Value::Null)
        }
        "ST_INTERSECTS" => match eval(&args[1], f) {
            Value::Geometry(b) => Value::Int(shape.intersects(&super::super::topology::to_geo(&b)) as i64),
            _ => Value::Int(-1),
        },
        "ST_BUFFER" => {
            let Some(distance) = eval(&args[1], f).as_f64().filter(|d| d.is_finite()) else { return Value::Null };
            let mut l = Layer::new("geometry");
            l.features.push(Feature { fid: f.fid, geometry: Some(*g), attributes: Vec::new() });
            super::super::topology::buffer(&l, &super::super::topology::BufferOptions { distance, quadrant_segments: 30 })
                .ok()
                .and_then(|mut l| l.features.pop()?.geometry)
                .filter(|g| !g.all_coords().is_empty())
                .map(|g| {
                    let g = match g {
                        Geometry::MultiPolygon(mut ps) if ps.len() == 1 => {
                            let (exterior, interiors) = ps.remove(0);
                            Geometry::Polygon { exterior, interiors }
                        }
                        g => g,
                    };
                    Value::Geometry(Box::new(g))
                })
                .unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

fn length(g: &Geometry) -> f64 {
    let line = |cs: &[wbvector::Coord]| cs.windows(2).map(|p| (p[1].x - p[0].x).hypot(p[1].y - p[0].y)).sum::<f64>();
    match g {
        Geometry::LineString(cs) => line(cs),
        Geometry::MultiLineString(ls) => ls.iter().map(|cs| line(cs)).sum(),
        Geometry::GeometryCollection(gs) => gs.iter().map(length).sum(),
        _ => 0.0,
    }
}

/// A transform is prepared once per expression, shared by every evaluated row.
#[derive(Clone)]
pub(super) struct Transform {
    source: u32,
    target: u32,
    cartesian: bool,
    transformer: std::sync::Arc<crate::crs::Transformer>,
}
impl std::fmt::Debug for Transform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Transform").field(&self.source).field(&self.target).finish()
    }
}
impl PartialEq for Transform {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.target == other.target
    }
}
impl Transform {
    pub(super) fn new(args: &[Expr]) -> Result<Self> {
        let source =
            output_epsg(&args[0], args.get(2).and_then(epsg)).ok_or_else(|| syntax("ST_Transform requires an EPSG source CRS".into()))?;
        let target = epsg(&args[1]).ok_or_else(|| syntax("ST_Transform requires a constant positive EPSG code".into()))?;
        let (src, dst) = (crate::crs::SpatialRef::from_epsg(source)?, crate::crs::SpatialRef::from_epsg(target)?);
        Ok(Self {
            source,
            target,
            cartesian: src.is_geocentric() || dst.is_geocentric(),
            transformer: std::sync::Arc::new(crate::crs::Transformer::new(&src, &dst)?),
        })
    }
    pub(super) fn evaluate(&self, value: Value) -> Value {
        let Value::Geometry(mut g) = value else { return Value::Null };
        let result = crate::vector::map_coords(&mut g, &mut |c| {
            if self.cartesian {
                let (x, y, z) = self.transformer.transform_3d(c.x, c.y, c.z.unwrap_or(0.))?;
                c.x = x;
                c.y = y;
                c.z = Some(z);
            } else {
                (c.x, c.y) = self.transformer.transform(c.x, c.y)?;
            }
            Ok(())
        });
        if result.is_ok() { Value::Geometry(g) } else { Value::Null }
    }
}
