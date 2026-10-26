//! Vector geometry operations, the equivalents of GDAL 3.13's `gdal vector simplify`,
//! `segmentize`, `convex-hull`, `concave-hull`, `swap-xy`, `set-geom-type`, `combine` and `sort`.
//!
//! The per-feature operations are [`GeometryOp`]s: [`apply`] runs one on a layer, and
//! [`crate::ops::vector_filter::VectorTranslateOptions::geometry_op`] runs one on layers read and
//! written as streams (see [`crate::stream`]). [`combine`] and [`sort`] need the whole layer.
//! Attributes, feature IDs and CRS are preserved unless stated otherwise. Distances are in the
//! layer's CRS units (degrees for geographic CRSs).
//!
//! ```no_run
//! use terrio::ops::vector_geom::{self, GeometryOp, SortMethod};
//!
//! let ds = terrio::open_vector("countries.gpkg", &Default::default())?;
//! let simple = vector_geom::apply(&ds.layers[0], &GeometryOp::Simplify { tolerance: 0.01 })?;
//! let sorted = vector_geom::sort(&simple, SortMethod::Hilbert);
//! terrio::write_vector(&[sorted], "simple.gpkg", &Default::default())?;
//! # Ok::<(), terrio::Error>(())
//! ```

use super::hull;
use super::simplify;
use crate::{Error, Feature, FieldValue, Geometry, GeometryType, Layer, Result};
use wbvector::{Coord, Ring};

/// Options of the concave hull ([`GeometryOp::ConcaveHull`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConcaveHullOptions {
    /// The concavity, from 0 (most concave; the polygons themselves for polygonal input) to 1
    /// (the convex hull).
    pub ratio: f64,
    /// Whether hulls may have holes.
    pub allow_holes: bool,
    /// For polygonal input: whether the hull must follow the outer boundaries of the polygons.
    pub tight: bool,
}

impl Default for ConcaveHullOptions {
    fn default() -> Self {
        Self { ratio: 0.5, allow_holes: false, tight: false }
    }
}

/// A geometry type with its dimensions, as `--geometry-type` names it (`MULTIPOLYGON`,
/// `POINTZ`, `LINESTRING M`, `POINT25D`, `GEOMETRY`...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeometryTypeSpec {
    /// The type; `None` for `GEOMETRY` (any type).
    pub base: Option<GeometryType>,
    /// Whether the geometries have Z.
    pub z: bool,
    /// Whether the geometries have M.
    pub m: bool,
}

impl GeometryTypeSpec {
    /// Parses an OGC geometry type name, with an optional `Z`, `M`, `ZM` or `25D` suffix.
    /// Curve and surface types (`CIRCULARSTRING`, `CURVEPOLYGON`, `TIN`...) are not supported:
    /// terrio has no curve geometries.
    pub fn parse(s: &str) -> Result<Self> {
        let up = s.trim().to_ascii_uppercase().replace(' ', "");
        let (base, z, m) = if let Some(b) = up.strip_suffix("ZM") {
            (b, true, true)
        } else if let Some(b) = up.strip_suffix("25D") {
            (b, true, false)
        } else if let Some(b) = up.strip_suffix('Z') {
            (b, true, false)
        } else if let Some(b) = up.strip_suffix('M').filter(|b| !b.is_empty()) {
            (b, false, true)
        } else {
            (up.as_str(), false, false)
        };
        let base = match base {
            "GEOMETRY" | "UNKNOWN" => None,
            "POINT" => Some(GeometryType::Point),
            "LINESTRING" => Some(GeometryType::LineString),
            "POLYGON" => Some(GeometryType::Polygon),
            "MULTIPOINT" => Some(GeometryType::MultiPoint),
            "MULTILINESTRING" => Some(GeometryType::MultiLineString),
            "MULTIPOLYGON" => Some(GeometryType::MultiPolygon),
            "GEOMETRYCOLLECTION" => Some(GeometryType::GeometryCollection),
            "CIRCULARSTRING" | "COMPOUNDCURVE" | "CURVEPOLYGON" | "MULTICURVE" | "MULTISURFACE" | "CURVE" | "SURFACE" | "TRIANGLE"
            | "TIN" | "POLYHEDRALSURFACE" => {
                return Err(Error::Unsupported(format!("geometry type '{s}': terrio has no curve or surface geometries")));
            }
            _ => return Err(Error::InvalidArgument(format!("unknown geometry type '{s}'"))),
        };
        Ok(Self { base, z, m })
    }

    /// The type and dimensions of a geometry.
    pub fn of(g: &Geometry) -> Self {
        Self { base: Some(g.geom_type()), z: any_coord(g, &|c| c.z.is_some()), m: any_coord(g, &|c| c.m.is_some()) }
    }

    fn flat(base: GeometryType) -> Self {
        Self { base: Some(base), z: false, m: false }
    }
}

/// The coordinate dimensions of `--dim`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    /// X and Y.
    Xy,
    /// X, Y and Z.
    Xyz,
    /// X, Y and M.
    Xym,
    /// X, Y, Z and M.
    Xyzm,
}

impl Dimension {
    /// Parses `XY`, `XYZ`, `XYM` or `XYZM` (any case).
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_uppercase().as_str() {
            "XY" => Self::Xy,
            "XYZ" => Self::Xyz,
            "XYM" => Self::Xym,
            "XYZM" => Self::Xyzm,
            _ => return Err(Error::InvalidArgument(format!("unknown dimension '{s}': expected XY, XYZ, XYM or XYZM"))),
        })
    }
}

/// Options of [`GeometryOp::SetGeomType`], as `gdal vector set-geom-type`'s.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SetGeomTypeOptions {
    /// `--geometry-type`: force every geometry (and the layer) to this type. Cannot be combined
    /// with `multi`, `single`, `linear`, `curve` or `dim`.
    pub geometry_type: Option<GeometryTypeSpec>,
    /// `--multi`: single-part types become multi-part ones.
    pub multi: bool,
    /// `--single`: multi-part types become single-part ones.
    pub single: bool,
    /// `--linear`: curve types become linear (terrio's geometries are all linear already).
    pub linear: bool,
    /// `--curve`: not supported (terrio has no curve geometries).
    pub curve: bool,
    /// `--dim`: force the coordinate dimensions (added Z and M values are 0).
    pub dim: Option<Dimension>,
    /// `--layer-only`: change only the layer's declared type.
    pub layer_only: bool,
    /// `--feature-only`: change only the features' geometries.
    pub feature_only: bool,
    /// `--skip`: drop the features whose geometry could not be converted to the target type.
    pub skip: bool,
}

impl SetGeomTypeOptions {
    fn validate(&self) -> Result<()> {
        if self.geometry_type.is_some() && (self.multi || self.single || self.linear || self.curve || self.dim.is_some()) {
            return Err(Error::InvalidArgument("--geometry-type cannot be used with any of --multi/single/linear/curve/dim".into()));
        }
        if self.multi && self.single {
            return Err(Error::InvalidArgument("--multi and --single are mutually exclusive".into()));
        }
        if self.linear && self.curve {
            return Err(Error::InvalidArgument("--linear and --curve are mutually exclusive".into()));
        }
        if self.layer_only && self.feature_only {
            return Err(Error::InvalidArgument("--layer-only and --feature-only are mutually exclusive".into()));
        }
        if self.curve {
            return Err(Error::Unsupported("--curve: terrio has no curve geometries".into()));
        }
        Ok(())
    }

    /// The type a geometry (or layer) of type `t` is converted to (GDAL's `ConvertType`).
    pub fn convert_type(&self, t: GeometryTypeSpec) -> GeometryTypeSpec {
        if let Some(spec) = self.geometry_type {
            return spec;
        }
        let mut out = t;
        if self.multi {
            out.base = out.base.map(promote);
        } else if self.single {
            out.base = out.base.map(single);
        }
        match self.dim {
            Some(Dimension::Xy) => (out.z, out.m) = (false, false),
            Some(Dimension::Xyz) => (out.z, out.m) = (true, false),
            Some(Dimension::Xym) => (out.z, out.m) = (false, true),
            Some(Dimension::Xyzm) => (out.z, out.m) = (true, true),
            None => {}
        }
        out
    }
}

fn promote(t: GeometryType) -> GeometryType {
    match t {
        GeometryType::Point => GeometryType::MultiPoint,
        GeometryType::LineString => GeometryType::MultiLineString,
        GeometryType::Polygon => GeometryType::MultiPolygon,
        t => t,
    }
}

fn single(t: GeometryType) -> GeometryType {
    match t {
        GeometryType::MultiPoint => GeometryType::Point,
        GeometryType::MultiLineString => GeometryType::LineString,
        GeometryType::MultiPolygon => GeometryType::Polygon,
        t => t,
    }
}

/// A per-feature geometry operation.
#[derive(Debug, Clone, PartialEq)]
pub enum GeometryOp {
    /// `gdal vector simplify`: topology-preserving simplification (GEOS's
    /// `TopologyPreservingSimplifier`, see [`crate::ops::simplify`]). Z is kept, M dropped.
    Simplify {
        /// The distance tolerance, in CRS units (non-negative).
        tolerance: f64,
    },
    /// `gdal vector segmentize`: vertices added so that no segment is longer than `max_length`
    /// (OGR's `segmentize`, see [`crate::ops::simplify::segmentize`]).
    Segmentize {
        /// The maximum segment length, in CRS units (positive).
        max_length: f64,
    },
    /// `gdal vector convex-hull`: the convex hull of each geometry (a Polygon, or a LineString or
    /// Point for degenerate input). The layer's type becomes unknown.
    ConvexHull,
    /// `gdal vector concave-hull`: the concave hull of each geometry, of its polygons for
    /// polygonal input. Polygon and MultiPolygon layers keep their type (the hull is forced to
    /// it, as GDAL does); other layers' type becomes unknown.
    ConcaveHull(ConcaveHullOptions),
    /// `gdal vector swap-xy`: X and Y swapped in every coordinate.
    SwapXy,
    /// `gdal vector set-geom-type`: geometry type and dimension conversions.
    SetGeomType(SetGeomTypeOptions),
}

impl GeometryOp {
    /// ogr2ogr option precedence: positive simplify first, then positive segmentize.
    pub fn from_ogr2ogr(simplify: Option<f64>, segmentize: Option<f64>) -> Option<Self> {
        match (simplify, segmentize) {
            (Some(tolerance), _) if tolerance > 0. => Some(Self::Simplify { tolerance }),
            (_, Some(max_length)) if max_length > 0. => Some(Self::Segmentize { max_length }),
            _ => None,
        }
    }

    /// Checks the operation's parameters.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Simplify { tolerance } if !(*tolerance >= 0.0 && tolerance.is_finite()) => {
                Err(Error::InvalidArgument("the simplification tolerance must be a non-negative number".into()))
            }
            Self::Segmentize { max_length } if !(*max_length > 0.0 && max_length.is_finite()) => {
                Err(Error::InvalidArgument("the maximum segment length must be a positive number".into()))
            }
            Self::ConcaveHull(o) if !(0.0..=1.0).contains(&o.ratio) => {
                Err(Error::InvalidArgument("the concave hull ratio must be between 0 and 1".into()))
            }
            Self::SetGeomType(o) => o.validate(),
            _ => Ok(()),
        }
    }

    /// The declared geometry type of a layer of type `t` after the operation.
    pub fn layer_type(&self, t: Option<GeometryType>) -> Option<GeometryType> {
        match self {
            Self::ConvexHull => None,
            Self::ConcaveHull(_) => t.filter(|t| matches!(t, GeometryType::Polygon | GeometryType::MultiPolygon)),
            Self::SetGeomType(o) if !o.feature_only => o.convert_type(GeometryTypeSpec { base: t, z: false, m: false }).base,
            _ => t,
        }
    }

    /// Applies the operation to a geometry, for a layer declared as `layer_type` (the input's).
    /// `None` drops the feature (`set-geom-type --skip`).
    pub fn apply_geometry(&self, g: Geometry, layer_type: Option<GeometryType>) -> Result<Option<Geometry>> {
        Ok(Some(match self {
            Self::Simplify { tolerance } => simplify::simplify_preserve_topology(&g, *tolerance)?,
            Self::Segmentize { max_length } => simplify::segmentize(&g, *max_length)?,
            Self::ConvexHull => hull::convex_hull(&g)?,
            Self::ConcaveHull(o) => {
                let h = match g {
                    Geometry::Polygon { .. } | Geometry::MultiPolygon(_) => {
                        hull::concave_hull_of_polygons(&g, o.ratio, o.tight, o.allow_holes)?
                    }
                    _ => hull::concave_hull(&g, o.ratio, o.allow_holes)?,
                };
                match self.layer_type(layer_type) {
                    Some(t) => force_to(h, GeometryTypeSpec::flat(t)),
                    None => h,
                }
            }
            Self::SwapXy => swap_xy(g),
            Self::SetGeomType(o) => {
                if o.layer_only {
                    return Ok(Some(g));
                }
                let target = o.convert_type(GeometryTypeSpec::of(&g));
                let out = force_to(g, target);
                if o.skip && target.base.is_some() {
                    let got = GeometryTypeSpec::of(&out);
                    let same = if is_empty(&out) { got.base == target.base } else { got == target };
                    if !same {
                        return Ok(None);
                    }
                }
                out
            }
        }))
    }

    /// Applies the operation to a feature of a layer declared as `layer_type`. Null geometries
    /// stay null. Errors name the feature.
    pub fn apply_feature(&self, mut f: Feature, layer_type: Option<GeometryType>) -> Result<Option<Feature>> {
        let Some(g) = f.geometry.take() else { return Ok(Some(f)) };
        match self.apply_geometry(g, layer_type) {
            Ok(Some(g)) => {
                f.geometry = Some(g);
                Ok(Some(f))
            }
            Ok(None) => Ok(None),
            Err(Error::InvalidArgument(m)) => Err(Error::InvalidArgument(format!("feature {}: {m}", f.fid))),
            Err(Error::InvalidData(m)) => Err(Error::InvalidData(format!("feature {}: {m}", f.fid))),
            Err(e) => Err(e),
        }
    }
}

/// Applies a per-feature operation to every feature of a layer.
pub fn apply(layer: &Layer, op: &GeometryOp) -> Result<Layer> {
    op.validate()?;
    let mut out = super::geometry::header(layer, op.layer_type(layer.geom_type));
    out.features.reserve(layer.features.len());
    for f in &layer.features {
        let mut f = f.clone();
        f.attributes.resize(layer.schema.len(), FieldValue::Null);
        if let Some(f) = op.apply_feature(f, layer.geom_type)? {
            out.features.push(f);
        }
    }
    Ok(out)
}

fn any_coord(g: &Geometry, predicate: &impl Fn(&Coord) -> bool) -> bool {
    let ring = |r: &Ring| r.0.iter().any(predicate);
    match g {
        Geometry::Point(c) => predicate(c),
        Geometry::LineString(cs) | Geometry::MultiPoint(cs) => cs.iter().any(predicate),
        Geometry::MultiLineString(ls) => ls.iter().any(|cs| cs.iter().any(predicate)),
        Geometry::Polygon { exterior, interiors } => ring(exterior) || interiors.iter().any(ring),
        Geometry::MultiPolygon(ps) => ps.iter().any(|(e, holes)| ring(e) || holes.iter().any(ring)),
        Geometry::GeometryCollection(gs) => gs.iter().any(|g| any_coord(g, predicate)),
    }
}

fn is_empty(g: &Geometry) -> bool {
    !any_coord(g, &|_| true)
}

fn map_coords(g: Geometry, f: &impl Fn(Coord) -> Coord) -> Geometry {
    let line = |cs: Vec<Coord>| cs.into_iter().map(f).collect::<Vec<_>>();
    let ring = |r: Ring| Ring(line(r.0));
    match g {
        Geometry::Point(c) => Geometry::Point(f(c)),
        Geometry::LineString(cs) => Geometry::LineString(line(cs)),
        Geometry::MultiPoint(cs) => Geometry::MultiPoint(line(cs)),
        Geometry::MultiLineString(ls) => Geometry::MultiLineString(ls.into_iter().map(line).collect()),
        Geometry::Polygon { exterior, interiors } => {
            Geometry::Polygon { exterior: ring(exterior), interiors: interiors.into_iter().map(ring).collect() }
        }
        Geometry::MultiPolygon(ps) => {
            Geometry::MultiPolygon(ps.into_iter().map(|(e, h)| (ring(e), h.into_iter().map(ring).collect())).collect())
        }
        Geometry::GeometryCollection(gs) => Geometry::GeometryCollection(gs.into_iter().map(|g| map_coords(g, f)).collect()),
    }
}

/// Swaps the X and Y of every coordinate (OGR's `swapXY`).
pub fn swap_xy(g: Geometry) -> Geometry {
    map_coords(g, &|c| Coord { x: c.y, y: c.x, ..c })
}

/// Sets the Z and M dimensions of a geometry (OGR's `set3D` and `setMeasured`): added values
/// are 0, removed ones are dropped.
pub fn set_dimensions(g: Geometry, z: bool, m: bool) -> Geometry {
    map_coords(g, &|c| Coord {
        z: if z { Some(c.z.unwrap_or(0.0)) } else { None },
        m: if m { Some(c.m.unwrap_or(0.0)) } else { None },
        ..c
    })
}

fn with_dims(g: Geometry, t: GeometryTypeSpec) -> Geometry {
    set_dimensions(g, t.z, t.m)
}

fn is_collection(t: GeometryType) -> bool {
    matches!(t, GeometryType::MultiPoint | GeometryType::MultiLineString | GeometryType::MultiPolygon | GeometryType::GeometryCollection)
}

/// Whether type `t` is `sup` or one of its subtypes (OGR's `OGR_GT_IsSubClassOf`).
fn is_subclass(t: GeometryType, sup: GeometryType) -> bool {
    t == sup || (sup == GeometryType::GeometryCollection && is_collection(t))
}

/// The parts of a collection, one level down.
fn members(g: Geometry) -> Vec<Geometry> {
    match g {
        Geometry::MultiPoint(cs) => cs.into_iter().map(Geometry::Point).collect(),
        Geometry::MultiLineString(ls) => ls.into_iter().map(Geometry::LineString).collect(),
        Geometry::MultiPolygon(ps) => ps.into_iter().map(|(exterior, interiors)| Geometry::Polygon { exterior, interiors }).collect(),
        Geometry::GeometryCollection(gs) => gs,
        g => vec![g],
    }
}

fn empty_of(t: GeometryType) -> Geometry {
    match t {
        GeometryType::Point | GeometryType::MultiPoint => Geometry::MultiPoint(Vec::new()),
        GeometryType::LineString => Geometry::LineString(Vec::new()),
        GeometryType::Polygon => Geometry::Polygon { exterior: Ring(Vec::new()), interiors: Vec::new() },
        GeometryType::MultiLineString => Geometry::MultiLineString(Vec::new()),
        GeometryType::MultiPolygon => Geometry::MultiPolygon(Vec::new()),
        GeometryType::GeometryCollection => Geometry::GeometryCollection(Vec::new()),
    }
}

/// Converts a geometry to a type, as OGR's `OGRGeometryFactory::forceTo` does for linear
/// geometries (`gdal vector set-geom-type`): conversions that cannot be made leave the type
/// unchanged, though the target's dimensions may still be applied, as OGR does. Empty
/// geometries become empty geometries of the target type (an empty point is an empty
/// multipoint: terrio cannot represent empty points).
pub fn force_to(g: Geometry, target: GeometryTypeSpec) -> Geometry {
    use GeometryType as T;
    // OGR ForceTo leaves generic GEOMETRY targets unchanged, including Z/M suffixes.
    // The explicit --dim option is the operation that changes their coordinates.
    let Some(tt) = target.base else { return g };
    if is_empty(&g) {
        return empty_of(tt);
    }
    let src = GeometryTypeSpec::of(&g);
    let st = src.base.expect("a geometry's type");
    if (target.z || target.m) && !src.z && !src.m {
        return with_dims(force_to(g, GeometryTypeSpec::flat(tt)), target);
    }
    if st == tt {
        return with_dims(g, target);
    }
    if is_collection(st) && tt == T::GeometryCollection {
        return with_dims(Geometry::GeometryCollection(members(g)), target);
    }
    // Promote a single geometry to a collection.
    if !is_collection(st) && is_subclass(promote(st), tt) {
        let multi = match (tt, g) {
            (T::MultiPoint, Geometry::Point(c)) => Geometry::MultiPoint(vec![c]),
            (T::MultiLineString, Geometry::LineString(l)) => Geometry::MultiLineString(vec![l]),
            (T::MultiPolygon, Geometry::Polygon { exterior, interiors }) => Geometry::MultiPolygon(vec![(exterior, interiors)]),
            (_, g) => Geometry::GeometryCollection(vec![g]),
        };
        return with_dims(multi, target);
    }
    let mut g = g;
    if st == T::LineString && tt == T::MultiPolygon {
        let tmp = force_to(g, GeometryTypeSpec::flat(T::Polygon));
        if tmp.geom_type() != T::LineString {
            return force_to(tmp, target);
        }
        g = tmp;
    } else if is_collection(st)
        && let Some(only) = single_member(&g)
    {
        let sub = force_to(only, target);
        if is_subclass(sub.geom_type(), tt) {
            return sub;
        }
    }
    let converted = match tt {
        T::LineString => force_to_line_string(g),
        T::Polygon => force_to_polygon(g),
        T::MultiPolygon => force_to_multi_polygon(g),
        T::MultiLineString => force_to_multi_line_string(g),
        T::MultiPoint => force_to_multi_point(g),
        _ => return g,
    };
    with_dims(converted, target)
}

fn single_member(g: &Geometry) -> Option<Geometry> {
    let count = match g {
        Geometry::MultiPoint(cs) => cs.len(),
        Geometry::MultiLineString(ls) => ls.len(),
        Geometry::MultiPolygon(ps) => ps.len(),
        Geometry::GeometryCollection(gs) => gs.len(),
        _ => 1,
    };
    (count == 1).then(|| members(g.clone()).remove(0))
}

fn closed(r: Ring) -> Vec<Coord> {
    simplify::closed_ring(&r.0)
}

fn same_point(a: &Coord, b: &Coord) -> bool {
    a.x == b.x && a.y == b.y && a.z == b.z
}

/// OGR's `forceToLineString` (joining lines in order only).
fn force_to_line_string(g: Geometry) -> Geometry {
    match g {
        Geometry::LineString(_) => g,
        Geometry::Polygon { exterior, interiors } if interiors.is_empty() => Geometry::LineString(closed(exterior)),
        Geometry::Polygon { .. } => g,
        Geometry::MultiLineString(_) | Geometry::GeometryCollection(_) => {
            let mut parts = members(g);
            if parts.is_empty() {
                return Geometry::LineString(Vec::new());
            }
            let mut i0 = 0;
            while i0 < parts.len() {
                let Geometry::LineString(l0) = &parts[i0] else {
                    i0 += 1;
                    continue;
                };
                if l0.len() < 2 {
                    i0 += 1;
                    continue;
                }
                let (start0, end0) = (l0[0].clone(), l0[l0.len() - 1].clone());
                let mut joined = false;
                for i1 in i0 + 1..parts.len() {
                    let Geometry::LineString(l1) = &parts[i1] else { continue };
                    if l1.len() < 2 {
                        continue;
                    }
                    let (start1, end1) = (&l1[0], &l1[l1.len() - 1]);
                    if same_point(&end0, start1) {
                        let tail: Vec<Coord> = l1[1..].to_vec();
                        parts.remove(i1);
                        if let Geometry::LineString(l0) = &mut parts[i0] {
                            l0.extend(tail);
                        }
                        joined = true;
                        break;
                    }
                    if same_point(end1, &start0) {
                        let tail: Vec<Coord> = match &parts[i0] {
                            Geometry::LineString(l0) => l0[1..].to_vec(),
                            _ => Vec::new(),
                        };
                        if let Geometry::LineString(l1) = &mut parts[i1] {
                            l1.extend(tail);
                        }
                        parts.remove(i0);
                        joined = true;
                        break;
                    }
                }
                if !joined {
                    i0 += 1;
                }
            }
            if parts.len() == 1 { parts.remove(0) } else { Geometry::GeometryCollection(parts) }
        }
        g => g,
    }
}

/// OGR's `forceToPolygon`.
fn force_to_polygon(g: Geometry) -> Geometry {
    match g {
        Geometry::Polygon { .. } => g,
        Geometry::LineString(cs) => {
            let closed = cs.len() >= 3 && cs.first().zip(cs.last()).is_some_and(|(a, b)| a.x == b.x && a.y == b.y);
            if closed { Geometry::Polygon { exterior: simplify::open_ring(cs), interiors: Vec::new() } } else { Geometry::LineString(cs) }
        }
        Geometry::MultiPolygon(_) | Geometry::GeometryCollection(_) => {
            let mut rings = Vec::new();
            for m in members(g) {
                if let Geometry::Polygon { exterior, interiors } = m
                    && !exterior.0.is_empty()
                {
                    rings.push(exterior);
                    rings.extend(interiors);
                }
            }
            let mut rings = rings.into_iter();
            match rings.next() {
                Some(exterior) => Geometry::Polygon { exterior, interiors: rings.collect() },
                None => Geometry::Polygon { exterior: Ring(Vec::new()), interiors: Vec::new() },
            }
        }
        g => g,
    }
}

/// OGR's `forceToMultiPolygon`.
fn force_to_multi_polygon(g: Geometry) -> Geometry {
    match g {
        Geometry::MultiPolygon(_) => g,
        Geometry::Polygon { exterior, interiors } => Geometry::MultiPolygon(vec![(exterior, interiors)]),
        Geometry::GeometryCollection(gs) => {
            if !gs.iter().all(|m| matches!(m, Geometry::Polygon { .. } | Geometry::MultiPolygon(_))) {
                return Geometry::GeometryCollection(gs);
            }
            let mut out = Vec::new();
            for m in gs {
                match m {
                    Geometry::Polygon { exterior, interiors } => out.push((exterior, interiors)),
                    Geometry::MultiPolygon(ps) => out.extend(ps),
                    _ => {}
                }
            }
            Geometry::MultiPolygon(out)
        }
        g => g,
    }
}

/// OGR's `forceToMultiLineString`.
fn force_to_multi_line_string(g: Geometry) -> Geometry {
    match g {
        Geometry::MultiLineString(_) => g,
        Geometry::LineString(l) => Geometry::MultiLineString(vec![l]),
        Geometry::GeometryCollection(gs) => {
            if !gs.iter().all(|m| matches!(m, Geometry::LineString(_))) {
                return Geometry::GeometryCollection(gs);
            }
            Geometry::MultiLineString(
                gs.into_iter()
                    .filter_map(|m| match m {
                        Geometry::LineString(l) => Some(l),
                        _ => None,
                    })
                    .collect(),
            )
        }
        Geometry::Polygon { exterior, interiors } => {
            Geometry::MultiLineString(std::iter::once(exterior).chain(interiors).filter(|r| !r.0.is_empty()).map(closed).collect())
        }
        Geometry::MultiPolygon(ps) => Geometry::MultiLineString(
            ps.into_iter().flat_map(|(e, h)| std::iter::once(e).chain(h)).filter(|r| !r.0.is_empty()).map(closed).collect(),
        ),
        g => g,
    }
}

/// OGR's `forceToMultiPoint`.
fn force_to_multi_point(g: Geometry) -> Geometry {
    match g {
        Geometry::MultiPoint(_) => g,
        Geometry::Point(c) => Geometry::MultiPoint(vec![c]),
        Geometry::GeometryCollection(gs) => {
            if !gs.iter().all(|m| matches!(m, Geometry::Point(_))) {
                return Geometry::GeometryCollection(gs);
            }
            Geometry::MultiPoint(
                gs.into_iter()
                    .filter_map(|m| match m {
                        Geometry::Point(c) => Some(c),
                        _ => None,
                    })
                    .collect(),
            )
        }
        g => g,
    }
}

/// Which non-grouping fields `combine` copies, as `--add-extra-fields` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtraFields {
    /// Only the `group_by` fields (`no`).
    #[default]
    No,
    /// Fields whose values are identical within at least one group; groups whose values differ
    /// get null (`sometimes-identical`).
    SometimesIdentical,
    /// Fields whose values are identical within every group (`always-identical`).
    AlwaysIdentical,
}

impl ExtraFields {
    /// Parses `no`, `sometimes-identical` or `always-identical`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "no" => Self::No,
            "sometimes-identical" => Self::SometimesIdentical,
            "always-identical" => Self::AlwaysIdentical,
            _ => return Err(Error::InvalidArgument(format!("unknown --add-extra-fields value '{s}'"))),
        })
    }
}

/// Options of [`combine`], as `gdal vector combine`'s.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CombineOptions {
    /// Combine the features with the same values of these fields (one output feature per
    /// group). By default, the whole layer becomes one feature.
    pub group_by: Vec<String>,
    /// Keep multi-part inputs as members of a geometry collection, rather than combining their
    /// parts.
    pub keep_nested: bool,
    /// Which other fields to copy.
    pub add_extra_fields: ExtraFields,
}

/// A field value as OGR's `GetFieldAsString` formats it, which `combine` groups by.
fn field_string(v: &FieldValue) -> String {
    match v {
        FieldValue::Integer(i) => i.to_string(),
        FieldValue::Float(f) => format_g15(*f),
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.clone(),
        FieldValue::Boolean(b) => (*b as i32).to_string(),
        FieldValue::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
        FieldValue::Null => String::new(),
    }
}

/// C's `%.15g`.
fn format_g15(v: f64) -> String {
    if !v.is_finite() {
        return if v.is_nan() {
            "nan".into()
        } else if v > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let e = format!("{v:.14e}");
    let (mantissa, exp) = e.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let trim = |s: &str| if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s.to_string() };
    if !(-4..15).contains(&exp) {
        format!("{}e{}{:02}", trim(mantissa), if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        trim(&format!("{v:.*}", (14 - exp).max(0) as usize))
    }
}

/// Combines the geometries of a layer into collections, one feature per group of `group_by`
/// values, as `gdal vector combine` does. Groups are ordered by their values as text (OGR's
/// formatting; nulls as empty text), as GDAL orders them. The output has the `group_by` fields
/// (and the extra fields `add_extra_fields` selects) and a MultiPoint, MultiLineString or
/// MultiPolygon geometry for layers of single-part types, a GeometryCollection otherwise. By
/// default the parts of multi-part inputs are added one by one; with `keep_nested`, multi-part
/// inputs are added whole and the output becomes a GeometryCollection. Null and empty geometries
/// add nothing (a group of them gets an empty collection). When any geometry has Z (or M), every
/// output does, with 0 where missing. Feature IDs are sequential from 1.
pub fn combine(layer: &Layer, options: &CombineOptions) -> Result<Layer> {
    use std::collections::BTreeMap;
    let mut group_idx = Vec::new();
    for name in &options.group_by {
        let i = crate::ops::sql::field_index(&layer.schema, name)
            .ok_or_else(|| Error::InvalidArgument(format!("field '{name}' does not exist in layer '{}'", layer.name)))?;
        if group_idx.contains(&i) {
            return Err(Error::InvalidArgument(format!("field '{name}' is given twice")));
        }
        group_idx.push(i);
    }
    let candidates: Vec<usize> = (0..layer.schema.len()).filter(|i| !group_idx.contains(i)).collect();
    let value = |f: &Feature, i: usize| f.attributes.get(i).cloned().unwrap_or(FieldValue::Null);
    struct Group {
        first: usize,
        parts: Vec<Geometry>,
        identical: Vec<bool>,
        values: Vec<String>,
    }
    let typed = layer.geom_type.filter(|t| !is_collection(*t)).map(promote);
    let mut out_type = typed.unwrap_or(GeometryType::GeometryCollection);
    let mut groups: BTreeMap<Vec<String>, Group> = BTreeMap::new();
    let (mut has_z, mut has_m) = (false, false);
    for (k, f) in layer.features.iter().enumerate() {
        let key: Vec<String> = group_idx.iter().map(|&i| field_string(&value(f, i))).collect();
        let group = groups.entry(key).or_insert_with(|| Group {
            first: k,
            parts: Vec::new(),
            identical: vec![true; candidates.len()],
            values: candidates.iter().map(|&i| field_string(&value(f, i))).collect(),
        });
        for (c, &i) in candidates.iter().enumerate() {
            if group.identical[c] && field_string(&value(f, i)) != group.values[c] {
                group.identical[c] = false;
            }
        }
        let Some(g) = &f.geometry else { continue };
        if is_empty(g) {
            continue;
        }
        let spec = GeometryTypeSpec::of(g);
        has_z |= spec.z;
        has_m |= spec.m;
        let src_collection = is_collection(g.geom_type());
        if options.keep_nested && src_collection {
            out_type = GeometryType::GeometryCollection;
        }
        let parts = if options.keep_nested || !src_collection { vec![g.clone()] } else { members(g.clone()) };
        if out_type != GeometryType::GeometryCollection {
            let want = single(out_type);
            if let Some(bad) = parts.iter().find(|p| p.geom_type() != want) {
                return Err(Error::InvalidArgument(format!(
                    "feature {}: cannot add a {} to a {} (the layer declares {}); combine to a geometry collection by converting the layer first with set-geom-type --geometry-type GEOMETRY",
                    f.fid,
                    bad.geom_type().as_str(),
                    out_type.as_str(),
                    layer.geom_type.map_or("no type", |t| t.as_str()),
                )));
            }
        }
        group.parts.extend(parts);
    }
    // Which extra fields to add (GDAL decides it with SQL over all the groups).
    let extra: Vec<usize> = match options.add_extra_fields {
        ExtraFields::No => Vec::new(),
        ExtraFields::SometimesIdentical => (0..candidates.len()).filter(|&c| groups.values().any(|g| g.identical[c])).collect(),
        ExtraFields::AlwaysIdentical => {
            (0..candidates.len()).filter(|&c| !groups.is_empty() && groups.values().all(|g| g.identical[c])).collect()
        }
    };
    let mut out = Layer::new(layer.name.clone());
    out.crs = layer.crs.clone();
    out.geom_type = Some(out_type);
    for &i in group_idx.iter().chain(extra.iter().map(|&c| &candidates[c])) {
        out.schema.add_field(layer.schema.fields()[i].clone());
    }
    for (n, group) in groups.into_values().enumerate() {
        let first = &layer.features[group.first];
        let mut attributes: Vec<FieldValue> = group_idx.iter().map(|&i| value(first, i)).collect();
        for &c in &extra {
            attributes.push(if group.identical[c] { value(first, candidates[c]) } else { FieldValue::Null });
        }
        let geometry = match out_type {
            GeometryType::GeometryCollection => Geometry::GeometryCollection(group.parts),
            t => force_to_typed(group.parts, t),
        };
        let geometry = if has_z || has_m { set_dimensions(geometry, has_z, has_m) } else { geometry };
        out.features.push(Feature { fid: n as u64 + 1, geometry: Some(geometry), attributes });
    }
    Ok(out)
}

/// The multi-part geometry of type `t` made of `parts` (all of `t`'s part type).
fn force_to_typed(parts: Vec<Geometry>, t: GeometryType) -> Geometry {
    match t {
        GeometryType::MultiPoint => Geometry::MultiPoint(
            parts
                .into_iter()
                .filter_map(|p| match p {
                    Geometry::Point(c) => Some(c),
                    _ => None,
                })
                .collect(),
        ),
        GeometryType::MultiLineString => Geometry::MultiLineString(
            parts
                .into_iter()
                .filter_map(|p| match p {
                    Geometry::LineString(l) => Some(l),
                    _ => None,
                })
                .collect(),
        ),
        GeometryType::MultiPolygon => Geometry::MultiPolygon(
            parts
                .into_iter()
                .filter_map(|p| match p {
                    Geometry::Polygon { exterior, interiors } => Some((exterior, interiors)),
                    _ => None,
                })
                .collect(),
        ),
        _ => Geometry::GeometryCollection(parts),
    }
}

/// How [`sort`] orders features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMethod {
    /// By the Hilbert code of the centre of each geometry's bounding box, on a 65535 × 65535
    /// grid over the layer's extent (GDAL's `GDALHilbertCode`).
    #[default]
    Hilbert,
    /// In the order of the leaves of a sort-tile-recursive tree of the bounding boxes, with nodes
    /// of 10 (GEOS's `TemplateSTRtree`): vertical slices by the boxes' X centres, each sorted by
    /// the Y centres.
    StrTree,
}

impl SortMethod {
    /// Parses `hilbert` or `strtree`.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hilbert" => Ok(Self::Hilbert),
            "strtree" => Ok(Self::StrTree),
            _ => Err(Error::InvalidArgument(format!("unknown sort method '{s}': expected hilbert or strtree"))),
        }
    }
}

/// GDAL's `GDALHilbertCode` of a point on a 16-bit grid (public domain code by
/// rawrunprotected, as GDAL uses it).
fn hilbert(x: u32, y: u32) -> u32 {
    let mut a = x ^ y;
    let mut b = 0xFFFF ^ a;
    let mut c = 0xFFFF ^ (x | y);
    let mut d = x & (y ^ 0xFFFF);
    let mut aa = a | (b >> 1);
    let mut bb = (a >> 1) ^ a;
    let mut cc = ((c >> 1) ^ (b & (d >> 1))) ^ c;
    let mut dd = ((a & (c >> 1)) ^ (d >> 1)) ^ d;
    for shift in [2, 4] {
        (a, b, c, d) = (aa, bb, cc, dd);
        aa = (a & (a >> shift)) ^ (b & (b >> shift));
        bb = (a & (b >> shift)) ^ (b & ((a ^ b) >> shift));
        cc ^= (a & (c >> shift)) ^ (b & (d >> shift));
        dd ^= (b & (c >> shift)) ^ ((a ^ b) & (d >> shift));
    }
    (a, b, c, d) = (aa, bb, cc, dd);
    cc ^= (a & (c >> 8)) ^ (b & (d >> 8));
    dd ^= (b & (c >> 8)) ^ ((a ^ b) & (d >> 8));
    let a = cc ^ (cc >> 1);
    let b = dd ^ (dd >> 1);
    let mut i0 = x ^ y;
    let mut i1 = b | (0xFFFF ^ (i0 | a));
    for (shift, mask) in [(8, 0x00FF00FF), (4, 0x0F0F0F0F), (2, 0x33333333), (1, 0x55555555)] {
        i0 = (i0 | (i0 << shift)) & mask;
        i1 = (i1 | (i1 << shift)) & mask;
    }
    (i1 << 1) | i0
}

/// The Hilbert code of `(x, y)` in the extent `(min_x, min_y, max_x, max_y)`.
fn hilbert_code(extent: (f64, f64, f64, f64), x: f64, y: f64) -> u32 {
    const MAX: f64 = ((1u32 << 16) - 2) as f64;
    let axis = |min: f64, max: f64, value: f64| {
        let width = max - min;
        let fraction = if width == 0.0 {
            0.0
        } else if width.is_finite() {
            (value - min) / width
        } else {
            (value / 2.0 - min / 2.0) / (max / 2.0 - min / 2.0)
        };
        (MAX * fraction.clamp(0.0, 1.0)).round() as u32
    };
    let hx = axis(extent.0, extent.2, x);
    let hy = axis(extent.1, extent.3, y);
    hilbert(hx, hy)
}

/// Sorts the features of a layer by the position of their geometries, as `gdal vector sort`
/// does. Features with null, empty or non-finite bounding boxes go last. Ties keep the input order (GDAL's
/// sort is not stable, so its order of ties may differ). Feature IDs are kept.
pub fn sort(layer: &Layer, method: SortMethod) -> Layer {
    let boxes: Vec<Option<(f64, f64, f64, f64)>> = layer
        .features
        .iter()
        .map(|f| {
            f.geometry
                .as_ref()
                .and_then(|g| g.bbox())
                .map(|b| (b.min_x, b.min_y, b.max_x, b.max_y))
                .filter(|b| [b.0, b.1, b.2, b.3].iter().all(|v| v.is_finite()))
        })
        .collect();
    let order: Vec<usize> = match method {
        SortMethod::Hilbert => {
            let extent = boxes.iter().flatten().fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |e, b| {
                (e.0.min(b.0), e.1.min(b.1), e.2.max(b.2), e.3.max(b.3))
            });
            let codes: Vec<Option<u32>> =
                boxes.iter().map(|b| b.map(|b| hilbert_code(extent, b.0 / 2.0 + b.2 / 2.0, b.1 / 2.0 + b.3 / 2.0))).collect();
            let mut order: Vec<usize> = (0..boxes.len()).collect();
            order.sort_by_key(|&i| (codes[i].is_none(), codes[i]));
            order
        }
        SortMethod::StrTree => {
            let mut items: Vec<usize> = (0..boxes.len()).filter(|&i| boxes[i].is_some()).collect();
            let b = |i: usize| boxes[i].expect("a bounding box");
            let n = items.len();
            if n > 1 {
                items.sort_by(|&p, &q| (b(p).0 + b(p).2).total_cmp(&(b(q).0 + b(q).2)));
                let slices = ((n as f64 / 10.0).ceil().sqrt()).ceil() as usize;
                let per_slice = (n as f64 / slices as f64).ceil() as usize;
                for slice in items.chunks_mut(per_slice.max(1)) {
                    slice.sort_by(|&p, &q| (b(p).1 + b(p).3).total_cmp(&(b(q).1 + b(q).3)));
                }
            }
            items.extend((0..boxes.len()).filter(|&i| boxes[i].is_none()));
            items
        }
    };
    let mut out = super::geometry::header(layer, layer.geom_type);
    out.features = order.into_iter().map(|i| layer.features[i].clone()).collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wkt(g: &Geometry) -> String {
        g.to_wkt()
    }

    #[test]
    fn parses_geometry_types() {
        let p = GeometryTypeSpec::parse;
        assert_eq!(p("POINTZM").unwrap(), GeometryTypeSpec { base: Some(GeometryType::Point), z: true, m: true });
        assert_eq!(p("multipolygon z").unwrap(), GeometryTypeSpec { base: Some(GeometryType::MultiPolygon), z: true, m: false });
        assert!(p("LINESTRING25D").unwrap().z);
        assert_eq!(p("POINTM").unwrap(), GeometryTypeSpec { base: Some(GeometryType::Point), z: false, m: true });
        assert_eq!(p("GEOMETRY").unwrap().base, None);
        assert_eq!(p("GEOMETRYZ").unwrap(), GeometryTypeSpec { base: None, z: true, m: false });
        assert!(matches!(p("CURVEPOLYGON"), Err(Error::Unsupported(_))));
        assert!(p("BLOB").is_err());
    }

    #[test]
    fn forces_types_like_ogr() {
        // The results of `gdal vector set-geom-type` (GDAL 3.12.2) on the same geometries.
        let line_m =
            Geometry::LineString(vec![Coord { x: 0.0, y: 0.0, z: None, m: Some(1.0) }, Coord { x: 1.0, y: 1.0, z: None, m: Some(2.0) }]);
        let mpz = GeometryTypeSpec::parse("MULTIPOLYGONZ").unwrap();
        let out = force_to(line_m.clone(), mpz);
        assert_eq!(GeometryTypeSpec::of(&out), GeometryTypeSpec { base: Some(GeometryType::LineString), z: true, m: false });
        // GDAL 3.13.3 flattens through POLYGON before applying MULTIPOLYGON Z,
        // so original Z ordinates become zero for both open and closed lines.
        for closed in [false, true] {
            let mut points = vec![Coord::xyz(0., 0., 5.), Coord::xyz(4., 0., 6.), Coord::xyz(4., 4., 7.)];
            if closed {
                points.push(Coord::xyz(0., 0., 5.));
            }
            let result = force_to(Geometry::LineString(points), mpz);
            assert!(result.all_coords().iter().all(|c| c.z == Some(0.)));
            assert_eq!(result.geom_type(), if closed { GeometryType::MultiPolygon } else { GeometryType::LineString });
        }
        // To POINT, an incompatible geometry keeps its dimensions.
        assert_eq!(force_to(line_m.clone(), GeometryTypeSpec::parse("POINT").unwrap()), line_m);
        for kind in ["GEOMETRY", "GEOMETRYZ", "GEOMETRYM", "GEOMETRYZM"] {
            let xy = Geometry::point(1., 2.);
            let xyz = Geometry::point_z(1., 2., 3.);
            let spec = GeometryTypeSpec::parse(kind).unwrap();
            assert_eq!(force_to(xy.clone(), spec), xy);
            assert_eq!(force_to(xyz.clone(), spec), xyz);
        }
        let point_z = Geometry::point_z(1.0, 2.0, 3.0);
        assert_eq!(force_to(point_z.clone(), GeometryTypeSpec::parse("POINT").unwrap()), Geometry::point(1.0, 2.0));
        assert_eq!(force_to(point_z, GeometryTypeSpec::parse("GEOMETRYCOLLECTION").unwrap()).geom_type(), GeometryType::GeometryCollection);
        let two =
            Geometry::MultiLineString(vec![vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 1.0)], vec![Coord::xy(1.0, 1.0), Coord::xy(2.0, 2.0)]]);
        assert_eq!(
            wkt(&force_to(two, GeometryTypeSpec::parse("LINESTRING").unwrap())),
            wkt(&Geometry::LineString(vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 1.0), Coord::xy(2.0, 2.0)]))
        );
        let gc = Geometry::GeometryCollection(vec![
            Geometry::point(1.0, 1.0),
            Geometry::LineString(vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 1.0)]),
        ]);
        let polygon = force_to(gc, GeometryTypeSpec::parse("POLYGON").unwrap());
        assert!(matches!(&polygon, Geometry::Polygon { exterior, .. } if exterior.0.is_empty()));
        // Two polygons forced to one keep every ring.
        let sq = |x: f64| (Ring(vec![Coord::xy(x, 0.0), Coord::xy(x + 1.0, 0.0), Coord::xy(x + 1.0, 1.0), Coord::xy(x, 0.0)]), vec![]);
        let Geometry::Polygon { interiors, .. } =
            force_to(Geometry::MultiPolygon(vec![sq(0.0), sq(5.0)]), GeometryTypeSpec::parse("POLYGON").unwrap())
        else {
            panic!()
        };
        assert_eq!(interiors.len(), 1);
    }

    #[test]
    fn set_geom_type_options() {
        let o = SetGeomTypeOptions { dim: Some(Dimension::Xym), ..Default::default() };
        let out = GeometryOp::SetGeomType(o).apply_geometry(Geometry::point_z(1.0, 2.0, 3.0), None).unwrap().unwrap();
        assert_eq!(out, Geometry::Point(Coord { x: 1.0, y: 2.0, z: None, m: Some(0.0) }));
        let skip = GeometryOp::SetGeomType(SetGeomTypeOptions {
            geometry_type: Some(GeometryTypeSpec::parse("POINT").unwrap()),
            skip: true,
            ..Default::default()
        });
        assert_eq!(skip.apply_geometry(Geometry::LineString(vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 1.0)]), None).unwrap(), None);
        let multi = GeometryOp::SetGeomType(SetGeomTypeOptions { multi: true, ..Default::default() });
        assert_eq!(multi.layer_type(Some(GeometryType::Polygon)), Some(GeometryType::MultiPolygon));
        assert_eq!(multi.layer_type(None), None);
        let bad = GeometryOp::SetGeomType(SetGeomTypeOptions {
            geometry_type: Some(GeometryTypeSpec::parse("POINT").unwrap()),
            multi: true,
            ..Default::default()
        });
        assert!(bad.validate().is_err());
        assert!(GeometryOp::SetGeomType(SetGeomTypeOptions { curve: true, ..Default::default() }).validate().is_err());
    }

    #[test]
    fn swaps_xy() {
        let g = Geometry::LineString(vec![Coord { x: 1.0, y: 2.0, z: Some(3.0), m: Some(4.0) }]);
        assert_eq!(swap_xy(g), Geometry::LineString(vec![Coord { x: 2.0, y: 1.0, z: Some(3.0), m: Some(4.0) }]));
    }

    #[test]
    fn formats_like_printf() {
        assert_eq!(format_g15(1.5), "1.5");
        assert_eq!(format_g15(2.0), "2");
        assert_eq!(format_g15(0.1), "0.1");
        assert_eq!(format_g15(1e20), "1e+20");
        assert_eq!(format_g15(123456789.125), "123456789.125");
        assert_eq!(format_g15(0.00001234), "1.234e-05");
    }

    fn layer(rows: &[(&str, Option<Geometry>)]) -> Layer {
        let mut l = Layer::new("t");
        l.geom_type = Some(GeometryType::Polygon);
        l.schema.add_field(crate::FieldDef::new("k", crate::FieldType::Text));
        l.schema.add_field(crate::FieldDef::new("v", crate::FieldType::Integer));
        for (i, (k, g)) in rows.iter().enumerate() {
            l.features.push(Feature {
                fid: i as u64 + 1,
                geometry: g.clone(),
                attributes: vec![FieldValue::Text(k.to_string()), FieldValue::Integer(if *k == "a" { 1 } else { i as i64 })],
            });
        }
        l
    }

    fn square(x: f64) -> Geometry {
        Geometry::polygon(vec![Coord::xy(x, 0.0), Coord::xy(x + 1.0, 0.0), Coord::xy(x + 1.0, 1.0), Coord::xy(x, 0.0)], vec![])
    }

    #[test]
    fn combines_groups() {
        assert!(combine(&Layer::new("empty"), &Default::default()).unwrap().is_empty());

        let l = layer(&[("b", Some(square(0.0))), ("a", Some(square(2.0))), ("a", Some(square(4.0))), ("c", None)]);
        let all = combine(&l, &Default::default()).unwrap();
        assert_eq!(all.features.len(), 1);
        assert!(matches!(&all.features[0].geometry, Some(Geometry::MultiPolygon(ps)) if ps.len() == 3));
        assert_eq!(all.schema.len(), 0);
        let by = combine(
            &l,
            &CombineOptions { group_by: vec!["k".into()], add_extra_fields: ExtraFields::SometimesIdentical, ..Default::default() },
        )
        .unwrap();
        let keys: Vec<_> = by.features.iter().map(|f| f.attributes[0].clone()).collect();
        assert_eq!(keys, [FieldValue::Text("a".into()), FieldValue::Text("b".into()), FieldValue::Text("c".into())]);
        assert_eq!(by.schema.len(), 2, "v is identical within group a");
        assert_eq!(by.features[0].attributes[1], FieldValue::Integer(1));
        assert!(matches!(&by.features[2].geometry, Some(Geometry::MultiPolygon(ps)) if ps.is_empty()));
        let always = combine(
            &l,
            &CombineOptions { group_by: vec!["k".into()], add_extra_fields: ExtraFields::AlwaysIdentical, ..Default::default() },
        )
        .unwrap();
        assert_eq!(always.schema.len(), 2);
        let nested = combine(&l, &CombineOptions { keep_nested: true, ..Default::default() }).unwrap();
        assert_eq!(nested.geom_type, Some(GeometryType::MultiPolygon), "single-part inputs stay typed");
        assert!(combine(&l, &CombineOptions { group_by: vec!["nope".into()], ..Default::default() }).is_err());
        // A line in a polygon layer cannot join a MultiPolygon, as in GDAL.
        let mut bad = l.clone();
        bad.features[0].geometry = Some(Geometry::LineString(vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 1.0)]));
        assert!(combine(&bad, &Default::default()).is_err());
    }

    #[test]
    fn sorts_spatially() {
        let l = layer(&[("a", Some(square(9.0))), ("b", None), ("c", Some(square(0.0))), ("d", Some(square(5.0)))]);
        let ids = |l: &Layer| l.features.iter().map(|f| f.fid).collect::<Vec<_>>();
        // Codes 1057951502, 3237015806 and 3573875973 (GDAL's C code) for x = 0.5, 9.5 and 5.5.
        assert_eq!(ids(&sort(&l, SortMethod::Hilbert)), [3, 1, 4, 2]);
        assert_eq!(ids(&sort(&l, SortMethod::StrTree)), [3, 4, 1, 2]);
        // GDAL's Hilbert code of the grid corners.
        assert_eq!(hilbert(0, 0), 0);
        for (x, y, code) in [
            (65534, 0, 4294967294),
            (0, 65534, 1431655764),
            (65534, 65534, 2863311528),
            (12345, 54321, 1555040834),
            (32767, 32767, 715827882),
        ] {
            assert_eq!(hilbert(x, y), code, "({x}, {y})");
        }
    }
    #[test]
    fn sorting_handles_non_finite_boxes_and_extreme_finite_extents() {
        assert_eq!(hilbert_code((-f64::MAX, 0., f64::MAX, 1.), 0., 0.5), hilbert(32767, 32767));
        let l = layer(&[("bad", Some(Geometry::point(f64::INFINITY, 1.))), ("valid", Some(Geometry::point(0., 0.))), ("null", None)]);
        for method in [SortMethod::Hilbert, SortMethod::StrTree] {
            let result = sort(&l, method);
            assert_eq!(result.features[0].fid, 2);
        }
    }
}
