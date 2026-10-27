//! Gridding scattered points into a raster: the equivalent of `gdal_grid` and of GDAL 3.13's
//! `gdal vector grid`.
//!
//! The algorithms follow GDAL's `alg/gdalgrid.cpp` (MIT licensed) step by step so that the same
//! points reach each grid node in the same order: inverse distance to a power (with or without a
//! search ellipse), inverse distance to a power with nearest-neighbour search, moving average,
//! nearest neighbour, the data metrics (minimum, maximum, range, count, average distance and average
//! distance between points), with GDAL's per-quadrant variants, and linear interpolation in a
//! Delaunay triangulation.
//!
//! Grid nodes are the cell centres, computed per block of rows as `gdal_grid` computes them when
//! it writes a GeoTIFF. The search ellipse, its rotation, GDAL's choice between a quadtree and a
//! scan of every point (more than 100 points and a search radius), and the order the quadtree
//! returns points in (which decides ties and the order of sums) are reproduced: points are found
//! with an R-tree ([`rstar`]), then ordered as GDAL's `CPLQuadTree` would return them.
//!
//! Known differences from GDAL:
//! - GDAL computes `invdist` without a search ellipse, power 2 and no smoothing with AVX or SSE in
//!   single precision with approximate reciprocals (unless `GDAL_USE_AVX=NO` and
//!   `GDAL_USE_SSE=NO`); terrio always computes in double precision, as GDAL's scalar code does.
//! - `linear` triangulates with `spade` rather than qhull: the triangles are the same except
//!   for co-circular points, but the order of a triangle's vertices (and so the rounding of the
//!   interpolation) can differ.

use crate::crs::SpatialRef;
use crate::ops::sql::{Expression, field_index};
use crate::{Error, RasterDataset, Result};
use rstar::primitives::GeomWithData;
use rstar::{AABB, RTree};
use std::cmp::Ordering;
use wbraster::{DataType, RasterConfig};
use wbvector::{Coord, Feature, FieldValue, Geometry, Layer};

/// Above this many points, GDAL searches a quadtree rather than every point
/// (`GDAL_GRID_POINT_COUNT_THRESHOLD`).
const POINT_COUNT_THRESHOLD: usize = 100;
/// Squared distances below this are a node on a point: its value is taken.
const SINGULARITY: f64 = 0.0000000000001;
/// Tolerance of GDAL's barycentric point-in-triangle test.
const EPS: f64 = 1e-10;

/// A gridding algorithm, named as GDAL names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridMethod {
    /// `invdist`: inverse distance to a power.
    InverseDistance,
    /// `invdistnn`: inverse distance to a power, of the nearest points within a radius.
    InverseDistanceNearest,
    /// `average`: moving average of the points in the search ellipse.
    Average,
    /// `nearest`: the value of the nearest point.
    Nearest,
    /// `minimum`: the smallest value in the search ellipse.
    Minimum,
    /// `maximum`: the largest value in the search ellipse.
    Maximum,
    /// `range`: the largest minus the smallest value in the search ellipse.
    Range,
    /// `count`: the number of points in the search ellipse.
    Count,
    /// `average_distance`: the mean distance from the node to the points in the search ellipse.
    AverageDistance,
    /// `average_distance_pts`: the mean distance between the points in the search ellipse.
    AverageDistancePoints,
    /// `linear`: linear interpolation in the points' Delaunay triangulation.
    Linear,
}

impl GridMethod {
    /// Parses a method name: GDAL's (`invdist`, `invdistnn`, `average`, `nearest`, `minimum`,
    /// `maximum`, `range`, `count`, `average_distance`, `average_distance_pts`, `linear`), with
    /// `-` accepted for `_` and `average-distance-points` for `average_distance_pts`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "invdist" => Self::InverseDistance,
            "invdistnn" => Self::InverseDistanceNearest,
            "average" => Self::Average,
            "nearest" => Self::Nearest,
            "minimum" => Self::Minimum,
            "maximum" => Self::Maximum,
            "range" => Self::Range,
            "count" => Self::Count,
            "average_distance" => Self::AverageDistance,
            "average_distance_pts" | "average_distance_points" => Self::AverageDistancePoints,
            "linear" => Self::Linear,
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unknown gridding algorithm {other:?}: invdist, invdistnn, average, nearest, minimum, maximum, range, \
                     count, average_distance, average_distance_pts or linear"
                )));
            }
        })
    }

    /// GDAL's name of the method.
    pub fn name(self) -> &'static str {
        match self {
            Self::InverseDistance => "invdist",
            Self::InverseDistanceNearest => "invdistnn",
            Self::Average => "average",
            Self::Nearest => "nearest",
            Self::Minimum => "minimum",
            Self::Maximum => "maximum",
            Self::Range => "range",
            Self::Count => "count",
            Self::AverageDistance => "average_distance",
            Self::AverageDistancePoints => "average_distance_pts",
            Self::Linear => "linear",
        }
    }

    /// Whether the method is a data metric (which has no maximum number of points).
    fn is_metric_like(self) -> bool {
        matches!(self, Self::Minimum | Self::Maximum | Self::Range | Self::Count | Self::AverageDistance | Self::AverageDistancePoints)
    }

    /// Whether the method searches an ellipse (`radius1`, `radius2`, `angle`) rather than a
    /// circle (`radius`).
    fn uses_ellipse(self) -> bool {
        !matches!(self, Self::InverseDistanceNearest | Self::Linear)
    }
}

/// A gridding algorithm and its parameters, as GDAL's `-a name:key=value:...` gives them.
///
/// [`GridAlgorithm::new`] sets GDAL's defaults. Parameters a method does not use are ignored.
#[derive(Debug, Clone, PartialEq)]
pub struct GridAlgorithm {
    /// The method.
    pub method: GridMethod,
    /// The weighting power (`invdist`, `invdistnn`). 2 by default.
    pub power: f64,
    /// The smoothing parameter (`invdist`, `invdistnn`). 0 by default.
    pub smoothing: f64,
    /// The first semi-axis of the search ellipse (along x before rotation). 0, the default, with
    /// `radius2` 0, searches every point.
    pub radius1: f64,
    /// The second semi-axis of the search ellipse.
    pub radius2: f64,
    /// The rotation of the search ellipse, in degrees counter-clockwise.
    pub angle: f64,
    /// The search radius of `invdistnn` (1 by default) and of `linear`'s nearest-neighbour
    /// fallback outside the triangulation (-1 by default: the nearest point however far; 0: the
    /// nodata value).
    pub radius: f64,
    /// The most points used (`invdist` with an ellipse, `invdistnn`, and `average` per
    /// quadrant). 0 for no limit; `invdistnn`'s default is 12.
    pub max_points: u32,
    /// The fewest points needed for a value; below, the nodata value.
    pub min_points: u32,
    /// The fewest points needed in each quadrant around the node.
    pub min_points_per_quadrant: u32,
    /// The most points used from each quadrant.
    pub max_points_per_quadrant: u32,
    /// The value of nodes without enough points. 0 by default.
    pub nodata: f64,
    /// Whether `nodata` was given: [`vector_grid`] then declares it as the bands' nodata value,
    /// as `gdal_grid` does when the algorithm string has a `nodata` parameter.
    pub nodata_set: bool,
}

impl GridAlgorithm {
    /// The method with GDAL's default parameters.
    pub fn new(method: GridMethod) -> Self {
        Self {
            method,
            power: 2.0,
            smoothing: 0.0,
            radius1: 0.0,
            radius2: 0.0,
            angle: 0.0,
            radius: match method {
                GridMethod::InverseDistanceNearest => 1.0,
                _ => -1.0,
            },
            max_points: if method == GridMethod::InverseDistanceNearest { 12 } else { 0 },
            min_points: 0,
            min_points_per_quadrant: 0,
            max_points_per_quadrant: 0,
            nodata: 0.0,
            nodata_set: false,
        }
    }

    /// Parses GDAL's algorithm string, such as `invdist:power=3:radius1=1000:radius2=500`.
    ///
    /// As GDAL, `invdist` with `min_points_per_quadrant` or `max_points_per_quadrant` becomes
    /// `invdistnn` (with `radius`, or `radius1`, which must then equal `radius2`, as its radius),
    /// and `radius` sets both semi-axes of the methods that search an ellipse.
    pub fn parse(s: &str) -> Result<Self> {
        let mut algorithm = Self::parse_parameters(s)?;
        algorithm.validate()?;
        Ok(algorithm)
    }

    fn parse_parameters(s: &str) -> Result<Self> {
        fn get<'a>(params: &'a [(String, String)], k: &str) -> Option<&'a str> {
            params.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
        }
        fn num(params: &[(String, String)], k: &str) -> Result<Option<f64>> {
            get(params, k)
                .map(|v| v.parse::<f64>().map_err(|_| Error::InvalidArgument(format!("gridding parameter {k}={v:?} is not a number"))))
                .transpose()
        }
        let mut parts = s.split(':');
        let name = parts.next().unwrap_or_default();
        let mut method = GridMethod::parse(name)?;
        let mut params: Vec<(String, String)> = Vec::new();
        for p in parts {
            if p.is_empty() {
                continue;
            }
            let (k, v) = p.split_once('=').ok_or_else(|| Error::InvalidArgument(format!("gridding parameter {p:?} is not key=value")))?;
            params.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
        if method == GridMethod::InverseDistance
            && (get(&params, "min_points_per_quadrant").is_some() || get(&params, "max_points_per_quadrant").is_some())
        {
            // GDAL remaps invdist to invdistnn when per-quadrant parameters are given.
            if get(&params, "radius").is_none() {
                let (r1, r2) = (num(&params, "radius1")?.unwrap_or(1.0), num(&params, "radius2")?.unwrap_or(1.0));
                if r1 != r2 {
                    return Err(Error::Unsupported(
                        "radius1 != radius2 not supported when min_points_per_quadrant and/or max_points_per_quadrant is specified".into(),
                    ));
                }
            }
            if num(&params, "angle")?.unwrap_or(0.0) != 0.0 {
                return Err(Error::Unsupported(
                    "angle != 0 not supported when min_points_per_quadrant and/or max_points_per_quadrant is specified".into(),
                ));
            }
            if get(&params, "radius").is_none() {
                params.push(("radius".into(), get(&params, "radius1").unwrap_or("1").to_string()));
                params.retain(|(key, _)| key != "radius1" && key != "radius2");
            }
            params.retain(|(key, _)| key != "angle");
            method = GridMethod::InverseDistanceNearest;
        }
        let mut a = Self::new(method);
        let known: &[&str] = match method {
            GridMethod::InverseDistance => {
                &["power", "smoothing", "radius", "radius1", "radius2", "angle", "max_points", "min_points", "nodata"]
            }
            GridMethod::InverseDistanceNearest => &[
                "power",
                "smoothing",
                "radius",
                "max_points",
                "min_points",
                "nodata",
                "min_points_per_quadrant",
                "max_points_per_quadrant",
            ],
            GridMethod::Average => &[
                "radius",
                "radius1",
                "radius2",
                "angle",
                "min_points",
                "max_points",
                "nodata",
                "min_points_per_quadrant",
                "max_points_per_quadrant",
            ],
            GridMethod::Nearest => &["radius", "radius1", "radius2", "angle", "nodata"],
            GridMethod::Linear => &["radius", "nodata"],
            _ => &["radius", "radius1", "radius2", "angle", "min_points", "nodata", "min_points_per_quadrant", "max_points_per_quadrant"],
        };
        for (k, _) in &params {
            if !known.contains(&k.as_str()) {
                crate::warnings::warn(format!("gridding option {k} ignored"));
            }
        }
        params.retain(|(key, _)| known.contains(&key.as_str()));
        if let Some(v) = num(&params, "power")? {
            a.power = v;
        }
        if let Some(v) = num(&params, "smoothing")? {
            a.smoothing = v;
        }
        if method.uses_ellipse() {
            match num(&params, "radius")? {
                Some(r) => (a.radius1, a.radius2) = (r, r),
                None => {
                    a.radius1 = num(&params, "radius1")?.unwrap_or(0.0);
                    a.radius2 = num(&params, "radius2")?.unwrap_or(0.0);
                }
            }
            a.angle = num(&params, "angle")?.unwrap_or(0.0);
        } else if let Some(r) = num(&params, "radius")? {
            a.radius = r;
        }
        let count = |k: &str| -> Result<Option<u32>> {
            match num(&params, k)? {
                Some(v) if !(v.is_finite() && v >= 0.0) => {
                    Err(Error::InvalidArgument(format!("gridding parameter {k} must be a non-negative number of points")))
                }
                v => Ok(v.map(|v| v.min(u32::MAX as f64) as u32)),
            }
        };
        if let Some(v) = count("max_points")? {
            a.max_points = v;
        }
        if let Some(v) = count("min_points")? {
            a.min_points = v;
        }
        if let Some(v) = count("min_points_per_quadrant")? {
            a.min_points_per_quadrant = v;
        }
        if let Some(v) = count("max_points_per_quadrant")? {
            a.max_points_per_quadrant = v;
        }
        if let Some(v) = num(&params, "nodata")? {
            a.nodata = v;
            a.nodata_set = true;
        }
        Ok(a)
    }

    /// Sets the search radius as GDAL's `radius` parameter does: both semi-axes of the ellipse of
    /// the methods that search one, or the radius of `invdistnn` and `linear`.
    pub fn set_radius(&mut self, r: f64) {
        if self.method.uses_ellipse() {
            (self.radius1, self.radius2) = (r, r);
        } else {
            self.radius = r;
        }
    }

    /// Whether per-quadrant parameters are set.
    fn per_quadrant(&self) -> bool {
        self.min_points_per_quadrant != 0 || self.max_points_per_quadrant != 0
    }

    /// Checks the parameters as GDAL does, remapping `invdist` with per-quadrant parameters to
    /// `invdistnn`.
    pub fn validate(&mut self) -> Result<()> {
        for (name, value) in [
            ("power", self.power),
            ("smoothing", self.smoothing),
            ("radius", self.radius),
            ("radius1", self.radius1),
            ("radius2", self.radius2),
            ("angle", self.angle),
        ] {
            if !value.is_finite() {
                return Err(Error::InvalidArgument(format!("gridding parameter {name} must be finite")));
            }
        }
        let no_radius = self.radius1 == 0.0 && self.radius2 == 0.0;
        if self.method == GridMethod::InverseDistance && no_radius && !self.per_quadrant() && (self.min_points > 0 || self.max_points > 0) {
            return Err(Error::InvalidArgument("invdist needs a radius (or radius1 and radius2) with min-points or max-points".into()));
        }
        if self.method == GridMethod::Average && self.max_points > 0 && !self.per_quadrant() {
            return Err(Error::InvalidArgument(
                "average uses max-points only with min-points-per-quadrant or max-points-per-quadrant".into(),
            ));
        }
        if self.method == GridMethod::InverseDistance && self.per_quadrant() {
            if self.radius1 != self.radius2 {
                return Err(Error::Unsupported(
                    "radius1 != radius2 not supported when min_points_per_quadrant and/or max_points_per_quadrant is specified".into(),
                ));
            }
            if self.angle != 0.0 {
                return Err(Error::Unsupported(
                    "angle != 0 not supported when min_points_per_quadrant and/or max_points_per_quadrant is specified".into(),
                ));
            }
            self.method = GridMethod::InverseDistanceNearest;
            self.radius = self.radius1;
            // invdistnn's default.
            if self.max_points == 0 {
                self.max_points = 12;
            }
        }
        match self.method {
            GridMethod::InverseDistanceNearest if self.radius.is_nan() || self.radius <= 0.0 => {
                Err(Error::InvalidArgument("radius value should be strictly positive".into()))
            }
            GridMethod::Average
            | GridMethod::Minimum
            | GridMethod::Maximum
            | GridMethod::Range
            | GridMethod::Count
            | GridMethod::AverageDistance
                if self.per_quadrant() && !(self.radius1 > 0.0 && self.radius2 > 0.0) =>
            {
                Err(Error::InvalidArgument("radius value should be strictly positive when per quadrant parameters are specified".into()))
            }
            GridMethod::AverageDistancePoints if self.per_quadrant() => {
                Err(Error::Unsupported("algorithm average_distance_pts not supported when per quadrant parameters are specified".into()))
            }
            _ => Ok(()),
        }
    }

    /// GDAL's algorithm string for these parameters (for `gdal_grid -a`).
    pub fn to_gdal_string(&self) -> String {
        let mut s = format!("{}:nodata={}", self.method.name(), g17(self.nodata));
        let mut push = |k: &str, v: String| {
            s.push(':');
            s.push_str(k);
            s.push('=');
            s.push_str(&v);
        };
        let m = self.method;
        if matches!(m, GridMethod::InverseDistance | GridMethod::InverseDistanceNearest) {
            push("power", g17(self.power));
            push("smoothing", g17(self.smoothing));
        }
        if m.uses_ellipse() {
            push("radius1", g17(self.radius1));
            push("radius2", g17(self.radius2));
            push("angle", g17(self.angle));
        } else {
            push("radius", g17(self.radius));
        }
        if matches!(m, GridMethod::InverseDistance | GridMethod::InverseDistanceNearest | GridMethod::Average) {
            push("max_points", self.max_points.to_string());
        }
        if m != GridMethod::Nearest && m != GridMethod::Linear {
            push("min_points", self.min_points.to_string());
        }
        if m != GridMethod::InverseDistance && m != GridMethod::Nearest && m != GridMethod::Linear && m != GridMethod::AverageDistancePoints
        {
            push("min_points_per_quadrant", self.min_points_per_quadrant.to_string());
            push("max_points_per_quadrant", self.max_points_per_quadrant.to_string());
        }
        s
    }
}

/// Parameters overriding those of an algorithm, as `gdal vector grid`'s options give them
/// (`--power`, `--radius`...): unset ones keep the algorithm's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GridParams {
    /// `--power`.
    pub power: Option<f64>,
    /// `--smoothing`.
    pub smoothing: Option<f64>,
    /// `--radius`: both semi-axes of an ellipse, or the radius of `invdistnn` and `linear`.
    pub radius: Option<f64>,
    /// `--radius1`, with `radius2`.
    pub radius1: Option<f64>,
    /// `--radius2`, with `radius1`.
    pub radius2: Option<f64>,
    /// `--angle`.
    pub angle: Option<f64>,
    /// `--min-points`.
    pub min_points: Option<u32>,
    /// `--max-points`.
    pub max_points: Option<u32>,
    /// `--min-points-per-quadrant`.
    pub min_points_per_quadrant: Option<u32>,
    /// `--max-points-per-quadrant`.
    pub max_points_per_quadrant: Option<u32>,
    /// `--nodata`.
    pub nodata: Option<f64>,
}

impl GridAlgorithm {
    /// The algorithm `algorithm` (a method name or a GDAL string such as `invdist:power=3`) with
    /// the parameters `p`, checked as `gdal vector grid` checks its options.
    pub fn with_params(algorithm: &str, p: &GridParams) -> Result<Self> {
        let m = GridMethod::parse(algorithm.split(':').next().unwrap_or_default())?;
        let ellipse = m.uses_ellipse();
        let reject = |what: &str| Err(Error::InvalidArgument(format!("{what} does not apply to the {} algorithm", m.name())));
        if !matches!(m, GridMethod::InverseDistance | GridMethod::InverseDistanceNearest) && (p.power.is_some() || p.smoothing.is_some()) {
            return reject("the power and smoothing");
        }
        if !ellipse && (p.radius1.is_some() || p.radius2.is_some() || p.angle.is_some()) {
            return reject("radius1, radius2 and angle");
        }
        if matches!(m, GridMethod::Nearest | GridMethod::Linear) && (p.min_points.is_some() || p.max_points.is_some()) {
            return reject("min-points and max-points");
        }
        if matches!(m, GridMethod::Nearest | GridMethod::Linear | GridMethod::AverageDistancePoints)
            && (p.min_points_per_quadrant.is_some() || p.max_points_per_quadrant.is_some())
        {
            return reject("the per-quadrant limits");
        }
        if m.is_metric_like() && p.max_points.is_some() {
            return reject("max-points");
        }
        if p.radius.is_some() && (p.radius1.is_some() || p.radius2.is_some()) {
            return Err(Error::InvalidArgument("give radius, or radius1 and radius2, not both".into()));
        }
        if p.radius1.is_some() != p.radius2.is_some() {
            return Err(Error::InvalidArgument("radius1 and radius2 go together".into()));
        }
        // Keywords override the packed parameters before the single parser normalizes
        // defaults and per-quadrant remapping (including explicitly supplied zeroes).
        let mut combined = algorithm
            .split(':')
            .filter(|part| {
                let key = part.split('=').next().unwrap_or_default().trim();
                !((p.radius.is_some() && (key.eq_ignore_ascii_case("radius1") || key.eq_ignore_ascii_case("radius2")))
                    || (p.radius1.is_some() && key.eq_ignore_ascii_case("radius")))
            })
            .collect::<Vec<_>>()
            .join(":");
        for (key, value) in [
            ("power", p.power),
            ("smoothing", p.smoothing),
            ("radius", p.radius),
            ("radius1", p.radius1),
            ("radius2", p.radius2),
            ("angle", p.angle),
            ("nodata", p.nodata),
        ] {
            if let Some(value) = value {
                combined.push_str(&format!(":{key}={value}"));
            }
        }
        for (key, value) in [
            ("min_points", p.min_points),
            ("max_points", p.max_points),
            ("min_points_per_quadrant", p.min_points_per_quadrant),
            ("max_points_per_quadrant", p.max_points_per_quadrant),
        ] {
            if let Some(value) = value {
                combined.push_str(&format!(":{key}={value}"));
            }
        }
        Self::parse(&combined)
    }
}

fn g17(v: f64) -> String {
    format!("{v:?}")
}

/// The output grid: `cols × rows` cells over `[x_min, x_max] × [y_min, y_max]`, north up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridExtent {
    /// The west edge.
    pub x_min: f64,
    /// The east edge.
    pub x_max: f64,
    /// The south edge.
    pub y_min: f64,
    /// The north edge.
    pub y_max: f64,
    /// The width in cells.
    pub cols: usize,
    /// The height in cells.
    pub rows: usize,
}

/// Grids points into the cells of `extent`, row by row from the north, as `gdal_grid` does into
/// a GeoTIFF of `bands` bands of `data_type` (which decides its blocks, and so how cell centres
/// are rounded).
///
/// Returns `cols × rows` values, the north row first.
pub fn grid_points(
    x: &[f64],
    y: &[f64],
    z: &[f64],
    algorithm: &GridAlgorithm,
    extent: &GridExtent,
    data_type: DataType,
    bands: usize,
) -> Result<Vec<f64>> {
    let mut out = Vec::new();
    grid_points_into(x, y, z, algorithm, extent, (data_type, bands), &mut out)?;
    Ok(out)
}

/// Appends one band directly to the output allocation, avoiding a second full band buffer.
fn grid_points_into(
    x: &[f64],
    y: &[f64],
    z: &[f64],
    algorithm: &GridAlgorithm,
    extent: &GridExtent,
    layout: (DataType, usize),
    out: &mut Vec<f64>,
) -> Result<()> {
    let (data_type, bands) = layout;
    if x.len() != y.len() || x.len() != z.len() {
        return Err(Error::InvalidArgument("x, y and z must have the same length".into()));
    }
    if x.iter().chain(y).any(|v| !v.is_finite()) {
        return Err(Error::InvalidArgument("gridding point coordinates must be finite".into()));
    }
    if x.is_empty() {
        return Err(Error::InvalidArgument("no points to grid".into()));
    }
    if x.len() > u32::MAX as usize {
        return Err(Error::InvalidArgument("too many points to grid".into()));
    }
    let (cols, rows) = (extent.cols, extent.rows);
    if cols == 0 || rows == 0 {
        return Err(Error::InvalidArgument("output raster dimensions should have non-zero size".into()));
    }
    check_size(cols, rows, 1)?;
    check_extent(extent)?;
    let mut alg = algorithm.clone();
    alg.validate()?;
    if alg.method == GridMethod::Linear && x.iter().chain(y).any(|v| spade::validate_coordinate(*v).is_err()) {
        return Err(Error::InvalidArgument("linear gridding coordinates exceed the triangulation range".into()));
    }
    let mut engine = Engine::new(x, y, z, &alg);
    // The blocks `gdal_grid` processes: whole rows, as many as the GeoTIFF's strips grown to 16 MB.
    let size = data_type_size(data_type);
    let block_rows = block_rows(cols, rows, bands.max(1), size);
    // As gdal_grid, which flips the y extent to write north-up rows.
    let (top, bottom) = (extent.y_max, extent.y_min);
    let dx = (extent.x_max - extent.x_min) / cols as f64;
    let dy = (bottom - top) / rows as f64;
    let first = out.len();
    out.try_reserve_exact(cols * rows).map_err(|_| Error::InvalidArgument("cannot allocate the output grid".into()))?;
    out.resize(first + cols * rows, 0.0);
    let mut row0 = 0;
    while row0 < rows {
        let n = block_rows.min(rows - row0);
        let bx0 = extent.x_min;
        let bx1 = extent.x_min + dx * cols as f64;
        let by0 = top + dy * row0 as f64;
        let by1 = top + dy * (row0 + n) as f64;
        let block = Block { x0: bx0, y0: by0, dx: (bx1 - bx0) / cols as f64, dy: (by1 - by0) / n as f64, cols, rows: n };
        engine.process(&block, &mut out[first + row0 * cols..first + (row0 + n) * cols]);
        row0 += n;
    }
    Ok(())
}

/// Number of pairs visited by GDAL's two average-distance traversal variants.
/// Point arrays are limited to u32::MAX, so their pair count fits u64.
fn distance_pair_count(points: usize, include_self: bool, omit_last_self: bool) -> u64 {
    let n = points as u64;
    if include_self { n * (n + 1) / 2 - u64::from(omit_last_self && n > 0) } else { n * n.saturating_sub(1) / 2 }
}

fn check_extent(extent: &GridExtent) -> Result<()> {
    if ![extent.x_min, extent.x_max, extent.y_min, extent.y_max].iter().all(|v| v.is_finite()) {
        return Err(Error::InvalidArgument("the grid extent must be finite".into()));
    }
    if extent.x_min == extent.x_max || extent.y_min == extent.y_max {
        return Err(Error::InvalidArgument("the grid extent must have nonzero width and height".into()));
    }
    Ok(())
}

/// The most cells of an output grid (all bands): 2^26, 512 MiB of Float64.
const MAX_CELLS: usize = 1 << 26;

/// Rejects grids whose cells overflow or exceed [`MAX_CELLS`].
fn check_size(cols: usize, rows: usize, bands: usize) -> Result<()> {
    match cols.checked_mul(rows).and_then(|n| n.checked_mul(bands)) {
        Some(n) if n <= MAX_CELLS => Ok(()),
        _ => Err(Error::InvalidArgument(format!("a {cols} x {rows} grid of {bands} band(s) is too large (at most {MAX_CELLS} cells)"))),
    }
}

/// The rows of each block `gdal_grid` computes in: the GeoTIFF's default strips (about 8 KiB),
/// grown to fill 16 MB, as `ProcessLayer` grows them.
fn block_rows(cols: usize, rows: usize, bands: usize, size: usize) -> usize {
    let line = (cols * bands * size).max(1);
    let strip = (8192 / line).clamp(1, rows.max(1));
    let desired = 16 * 1024 * 1024;
    let per = desired / (cols * size).max(1);
    if strip < rows && strip < per { ((per / strip) * strip).clamp(1, rows) } else { strip }
}

fn data_type_size(dt: DataType) -> usize {
    match dt {
        DataType::U8 | DataType::I8 => 1,
        DataType::U16 | DataType::I16 => 2,
        DataType::U32 | DataType::I32 | DataType::F32 => 4,
        DataType::U64 | DataType::I64 | DataType::F64 => 8,
    }
}

/// A block of rows: its first cell's corner, its cell size and its size.
struct Block {
    x0: f64,
    y0: f64,
    dx: f64,
    dy: f64,
    cols: usize,
    rows: usize,
}

impl Block {
    fn centre(&self, i: usize, j: usize) -> (f64, f64) {
        (self.x0 + (i as f64 + 0.5) * self.dx, self.y0 + (j as f64 + 0.5) * self.dy)
    }
}

type Tree = RTree<GeomWithData<[f64; 2], u32>>;

/// The points, with the search structures of the algorithm.
struct Engine<'a> {
    x: &'a [f64],
    y: &'a [f64],
    z: &'a [f64],
    alg: &'a GridAlgorithm,
    /// The points, for finding those near a node.
    tree: Option<Tree>,
    /// Whether GDAL searches its quadtree: then points come in the quadtree's order, `rank`.
    quadtree: bool,
    rank: Vec<u32>,
    /// The initial half-side of the square searched for the nearest point.
    initial_radius: f64,
    triangles: Option<Triangulation>,
    cos: f64,
    sin: f64,
    rotated: bool,
}

impl<'a> Engine<'a> {
    fn new(x: &'a [f64], y: &'a [f64], z: &'a [f64], alg: &'a GridAlgorithm) -> Self {
        let n = x.len();
        let has_radius = alg.radius1 > 0.0 || alg.radius2 > 0.0;
        let quadtree = match alg.method {
            GridMethod::InverseDistance => false,
            GridMethod::InverseDistanceNearest => true,
            GridMethod::Nearest => n > POINT_COUNT_THRESHOLD && alg.angle == 0.0 && has_radius,
            GridMethod::AverageDistancePoints => n > POINT_COUNT_THRESHOLD && alg.angle == 0.0 && has_radius,
            GridMethod::Linear => false,
            _ => alg.per_quadrant() || (n > POINT_COUNT_THRESHOLD && has_radius),
        };
        let angle = alg.angle.to_radians_gdal();
        let rotated = angle != 0.0;
        let mut e = Self {
            x,
            y,
            z,
            alg,
            tree: None,
            quadtree: false,
            rank: Vec::new(),
            initial_radius: 0.0,
            triangles: None,
            cos: if rotated { angle.cos() } else { 0.0 },
            sin: if rotated { angle.sin() } else { 0.0 },
            rotated,
        };
        let no_search = alg.method == GridMethod::InverseDistance && alg.radius1 == 0.0 && alg.radius2 == 0.0;
        if !no_search {
            e.tree = Some(RTree::bulk_load((0..n).map(|i| GeomWithData::new([x[i], y[i]], i as u32)).collect()));
        }
        if quadtree {
            e.create_quadtree();
        }
        if alg.method == GridMethod::Linear {
            e.triangles = Some(Triangulation::new(x, y));
        }
        e
    }

    /// GDAL's `GDALGridContextCreateQuadTree`: the order of its quadtree and the initial search
    /// radius, "the typical dimension of a pixel of the point array".
    fn create_quadtree(&mut self) {
        let (x, y) = (self.x, self.y);
        let (mut minx, mut miny, mut maxx, mut maxy) = (x[0], y[0], x[0], y[0]);
        for i in 1..x.len() {
            if x[i] < minx {
                minx = x[i];
            }
            if y[i] < miny {
                miny = y[i];
            }
            if x[i] > maxx {
                maxx = x[i];
            }
            if y[i] > maxy {
                maxy = y[i];
            }
        }
        self.initial_radius = ((maxx - minx) * (maxy - miny) / x.len() as f64).sqrt();
        self.rank = quadtree_ranks(x, y, [minx, miny, maxx, maxy]);
        self.quadtree = true;
    }

    /// The points in the square of half-side `r` around `(px, py)`, as GDAL's quadtree search
    /// returns them: edges included, in the quadtree's order.
    fn quadtree_search(&self, px: f64, py: f64, r: f64, out: &mut Vec<u32>) {
        out.clear();
        let (minx, miny, maxx, maxy) = (px - r, py - r, px + r, py + r);
        if let Some(tree) = &self.tree {
            let m = margin(r, px, py);
            let env = AABB::from_corners([minx - m, miny - m], [maxx + m, maxy + m]);
            for p in tree.locate_in_envelope(&env) {
                let i = p.data;
                let (xi, yi) = (self.x[i as usize], self.y[i as usize]);
                if !(xi > maxx || xi < minx || yi > maxy || yi < miny) {
                    out.push(i);
                }
            }
        }
        out.sort_unstable_by_key(|&i| self.rank[i as usize]);
    }

    /// The points that can be in a search ellipse of semi-axes up to `r` around `(px, py)`, in
    /// their order (as GDAL's scan of every point). Every point when the ellipse has no bound.
    fn scan_candidates(&self, px: f64, py: f64, out: &mut Vec<u32>) {
        out.clear();
        let (r1, r2) = (self.alg.radius1, self.alg.radius2);
        // An ellipse with one zero semi-axis is a line through the node, of any length.
        let unbounded = (r1 == 0.0) != (r2 == 0.0) || (r1 == 0.0 && r2 == 0.0) || !r1.is_finite() || !r2.is_finite();
        match &self.tree {
            Some(tree) if !unbounded && self.x.len() > POINT_COUNT_THRESHOLD => {
                let r = r1.abs().max(r2.abs());
                let m = margin(r, px, py) + r * 1e-9;
                let env = AABB::from_corners([px - r - m, py - r - m], [px + r + m, py + r + m]);
                out.extend(tree.locate_in_envelope(&env).map(|p| p.data));
                out.sort_unstable();
            }
            _ => out.extend(0..self.x.len() as u32),
        }
    }

    /// The offsets of point `i` from the node, rotated as GDAL rotates them.
    fn offsets(&self, i: u32, px: f64, py: f64) -> (f64, f64) {
        let (rx, ry) = (self.x[i as usize] - px, self.y[i as usize] - py);
        if self.rotated { (rx * self.cos + ry * self.sin, ry * self.cos - rx * self.sin) } else { (rx, ry) }
    }

    /// Fills `out` with the block's values.
    fn process(&mut self, b: &Block, out: &mut [f64]) {
        if self.alg.method == GridMethod::Linear {
            self.linear(b, out);
            return;
        }
        if self.alg.method == GridMethod::AverageDistancePoints && !self.quadtree && self.unbounded_scan() {
            // Every point is in the ellipse of every node: the same value everywhere.
            let v = self.average_distance_points(0.0, 0.0, &mut Vec::new());
            out.fill(v);
            return;
        }
        if !self.quadtree
            && self.unbounded_scan()
            && matches!(
                self.alg.method,
                GridMethod::Average | GridMethod::Minimum | GridMethod::Maximum | GridMethod::Range | GridMethod::Count
            )
        {
            // Every point contributes at every node, in the same order.
            let v = self.ellipse_metric(self.alg.method, 0.0, 0.0, &mut Vec::new());
            out.fill(v);
            return;
        }
        let this = &*self;
        let rows = crate::par::map_range(b.rows, |j| {
            let mut scratch = Scratch::default();
            (0..b.cols)
                .map(|i| {
                    let (px, py) = b.centre(i, j);
                    this.node(px, py, &mut scratch)
                })
                .collect::<Vec<f64>>()
        });
        for (j, row) in rows.into_iter().enumerate() {
            out[j * b.cols..(j + 1) * b.cols].copy_from_slice(&row);
        }
    }

    fn unbounded_scan(&self) -> bool {
        let (r1, r2) = (self.alg.radius1, self.alg.radius2);
        r1 == 0.0 && r2 == 0.0
    }

    /// The value of the node at `(px, py)`.
    fn node(&self, px: f64, py: f64, s: &mut Scratch) -> f64 {
        let a = self.alg;
        match a.method {
            GridMethod::InverseDistance => {
                if a.radius1 == 0.0 && a.radius2 == 0.0 {
                    self.inverse_distance_no_search(px, py)
                } else {
                    self.inverse_distance(px, py, &mut s.ids)
                }
            }
            GridMethod::InverseDistanceNearest => self.inverse_distance_nearest(px, py, s),
            GridMethod::Nearest => self.nearest(px, py, a.radius1, a.radius2, a.angle != 0.0, &mut s.ids),
            GridMethod::AverageDistancePoints => self.average_distance_points(px, py, &mut s.ids),
            m if a.per_quadrant() => self.per_quadrant(m, px, py, s),
            m => self.ellipse_metric(m, px, py, &mut s.ids),
        }
    }

    /// `GDALGridInverseDistanceToAPowerNoSearch`.
    fn inverse_distance_no_search(&self, px: f64, py: f64) -> f64 {
        let a = self.alg;
        let power_div2 = a.power / 2.0;
        let s2 = a.smoothing * a.smoothing;
        let (mut nom, mut den) = (0.0, 0.0);
        let n = self.x.len();
        let mut i = 0;
        if power_div2 == 1.0 {
            if s2 > 0.0 {
                while i < n {
                    let (rx, ry) = (self.x[i] - px, self.y[i] - py);
                    let r2 = rx * rx + ry * ry + s2;
                    let inv = 1.0 / r2;
                    nom += inv * self.z[i];
                    den += inv;
                    i += 1;
                }
            } else {
                while i < n {
                    let (rx, ry) = (self.x[i] - px, self.y[i] - py);
                    let r2 = rx * rx + ry * ry;
                    if r2 < SINGULARITY {
                        break;
                    }
                    let inv = 1.0 / r2;
                    nom += inv * self.z[i];
                    den += inv;
                    i += 1;
                }
            }
        } else {
            while i < n {
                let (rx, ry) = (self.x[i] - px, self.y[i] - py);
                let r2 = rx * rx + ry * ry + s2;
                if r2 < SINGULARITY {
                    break;
                }
                let inv = 1.0 / r2.powf(power_div2);
                nom += inv * self.z[i];
                den += inv;
                i += 1;
            }
        }
        if i != n {
            self.z[i]
        } else if den == 0.0 {
            a.nodata
        } else {
            nom / den
        }
    }

    /// `GDALGridInverseDistanceToAPower`, with a search ellipse.
    fn inverse_distance(&self, px: f64, py: f64, ids: &mut Vec<u32>) -> f64 {
        let a = self.alg;
        let (r1sq, r2sq) = (a.radius1 * a.radius1, a.radius2 * a.radius2);
        let r12sq = r1sq * r2sq;
        let power_div2 = a.power / 2.0;
        let (mut nom, mut den) = (0.0, 0.0);
        let mut n = 0u32;
        self.scan_candidates(px, py, ids);
        for &i in ids.iter() {
            let (mut rx, mut ry) = (self.x[i as usize] - px, self.y[i as usize] - py);
            let r2 = rx * rx + ry * ry + a.smoothing * a.smoothing;
            if self.rotated {
                (rx, ry) = (rx * self.cos + ry * self.sin, ry * self.cos - rx * self.sin);
            }
            if r2sq * rx * rx + r1sq * ry * ry <= r12sq {
                if r2 < SINGULARITY {
                    return self.z[i as usize];
                }
                let inv = 1.0 / r2.powf(power_div2);
                nom += inv * self.z[i as usize];
                den += inv;
                n += 1;
                if a.max_points > 0 && n > a.max_points {
                    break;
                }
            }
        }
        if n < a.min_points || den == 0.0 { a.nodata } else { nom / den }
    }

    /// `GDALGridInverseDistanceToAPowerNearestNeighbor` and its per-quadrant variant.
    fn inverse_distance_nearest(&self, px: f64, py: f64, s: &mut Scratch) -> f64 {
        let a = self.alg;
        let s2 = a.smoothing * a.smoothing;
        let r_pow2 = a.radius * a.radius;
        let power_div2 = a.power / 2.0;
        self.quadtree_search(px, py, a.radius, &mut s.ids);
        for q in s.quadrants.iter_mut() {
            q.clear();
        }
        let per_quadrant = a.per_quadrant();
        for &i in s.ids.iter() {
            let (rx, ry) = (self.x[i as usize] - px, self.y[i as usize] - py);
            let r2 = rx * rx + ry * ry;
            let rs2 = r2 + s2;
            if rs2 < SINGULARITY {
                return self.z[i as usize];
            }
            if r2 <= r_pow2 {
                let q = if per_quadrant { quadrant(rx, ry) } else { 0 };
                s.quadrants[q].push((rs2, self.z[i as usize]));
            }
        }
        let (mut nom, mut den) = (0.0, 0.0);
        let mut per = [0u32; 4];
        let mut n = 0u32;
        sort_quadrants(&mut s.quadrants);
        round_robin(&s.quadrants, if per_quadrant { a.max_points_per_quadrant } else { 0 }, a.max_points, &mut per, &mut n, |r2, z| {
            let inv = 1.0 / r2.powf(power_div2);
            nom += inv * z;
            den += inv;
        });
        if (per_quadrant && a.min_points_per_quadrant > 0 && per.iter().any(|&c| c < a.min_points_per_quadrant))
            || n < a.min_points
            || den == 0.0
        {
            a.nodata
        } else {
            nom / den
        }
    }

    /// `GDALGridNearestNeighbor`, with the semi-axes `r1` and `r2`.
    fn nearest(&self, px: f64, py: f64, r1: f64, r2: f64, rotated: bool, ids: &mut Vec<u32>) -> f64 {
        let a = self.alg;
        let mut value = a.nodata;
        if self.quadtree {
            // GDAL's indexed nearest search uses a square, including for anisotropic
            // radii. Unlike the scan path, it does not filter candidates by an ellipse.
            let has_radius = r1 > 0.0 || r2 > 0.0;
            let mut search = if has_radius { r1.max(r2) } else { self.initial_radius };
            while search > 0.0 {
                self.quadtree_search(px, py, search, ids);
                if !ids.is_empty() {
                    let mut best = f64::MAX;
                    for &i in ids.iter() {
                        let (rx, ry) = (self.x[i as usize] - px, self.y[i as usize] - py);
                        let d2 = rx * rx + ry * ry;
                        if d2 <= best {
                            best = d2;
                            value = self.z[i as usize];
                        }
                    }
                    break;
                }
                if has_radius {
                    break;
                }
                search *= 2.0;
            }
            return value;
        }
        let (r1sq, r2sq) = (r1 * r1, r2 * r2);
        let r12sq = r1sq * r2sq;
        let (cos, sin) = if rotated { (self.cos, self.sin) } else { (0.0, 0.0) };
        self.nearest_candidates(px, py, r1, r2, ids);
        let mut best = f64::MAX;
        for &i in ids.iter() {
            let (mut rx, mut ry) = (self.x[i as usize] - px, self.y[i as usize] - py);
            if rotated {
                (rx, ry) = (rx * cos + ry * sin, ry * cos - rx * sin);
            }
            let (rxsq, rysq) = (rx * rx, ry * ry);
            if r2sq * rxsq + r1sq * rysq <= r12sq {
                let d2 = rxsq + rysq;
                if d2 <= best {
                    best = d2;
                    value = self.z[i as usize];
                }
            }
        }
        value
    }

    /// The points that can be nearest to the node within an ellipse of semi-axes `r1` and `r2`,
    /// in index order: those near enough to the nearest when the ellipse has no bound.
    fn nearest_candidates(&self, px: f64, py: f64, r1: f64, r2: f64, ids: &mut Vec<u32>) {
        ids.clear();
        let n = self.x.len();
        let Some(tree) = self.tree.as_ref().filter(|_| n > POINT_COUNT_THRESHOLD) else {
            ids.extend(0..n as u32);
            return;
        };
        let r = if r1 == 0.0 && r2 == 0.0 {
            // Every point is in the ellipse: the nearest, and those as near, rotation aside.
            match tree.nearest_neighbor(&[px, py]) {
                Some(p) => {
                    let (dx, dy) = (p.geom()[0] - px, p.geom()[1] - py);
                    (dx * dx + dy * dy).sqrt()
                }
                None => return,
            }
        } else if (r1 == 0.0) != (r2 == 0.0) || !r1.is_finite() || !r2.is_finite() {
            ids.extend(0..n as u32);
            return;
        } else {
            r1.abs().max(r2.abs())
        };
        let m = margin(r, px, py) + r * 1e-6;
        let env = AABB::from_corners([px - r - m, py - r - m], [px + r + m, py + r + m]);
        ids.extend(tree.locate_in_envelope(&env).map(|p| p.data));
        ids.sort_unstable();
    }

    /// The points GDAL examines for an ellipse method without per-quadrant parameters: those its
    /// quadtree returns, or every point.
    fn ellipse_candidates(&self, px: f64, py: f64, ids: &mut Vec<u32>) {
        if self.quadtree {
            self.quadtree_search(px, py, self.alg.radius1.max(self.alg.radius2), ids);
        } else {
            self.scan_candidates(px, py, ids);
        }
    }

    /// `GDALGridMovingAverage` and the data metrics (`GDALGridDataMetricMinimum`...).
    fn ellipse_metric(&self, m: GridMethod, px: f64, py: f64, ids: &mut Vec<u32>) -> f64 {
        let a = self.alg;
        let (r1sq, r2sq) = (a.radius1 * a.radius1, a.radius2 * a.radius2);
        let r12sq = r1sq * r2sq;
        self.ellipse_candidates(px, py, ids);
        let (mut acc, mut min, mut max) = (0.0, f64::MAX, -f64::MAX);
        let mut n = 0u32;
        for &i in ids.iter() {
            let (rx, ry) = self.offsets(i, px, py);
            if r2sq * rx * rx + r1sq * ry * ry <= r12sq {
                let z = self.z[i as usize];
                match m {
                    GridMethod::Average => acc += z,
                    GridMethod::Minimum => {
                        if min > z {
                            min = z;
                        }
                    }
                    GridMethod::Maximum => {
                        if max < z {
                            max = z;
                        }
                    }
                    GridMethod::Range => {
                        if min > z {
                            min = z;
                        }
                        if max < z {
                            max = z;
                        }
                    }
                    GridMethod::AverageDistance => acc += (rx * rx + ry * ry).sqrt(),
                    _ => {}
                }
                n += 1;
            }
        }
        finish_metric(m, a, n, acc, min, max)
    }

    /// The per-quadrant variants of the moving average and the data metrics.
    fn per_quadrant(&self, m: GridMethod, px: f64, py: f64, s: &mut Scratch) -> f64 {
        let a = self.alg;
        let (r1sq, r2sq) = (a.radius1 * a.radius1, a.radius2 * a.radius2);
        let r12sq = r1sq * r2sq;
        self.quadtree_search(px, py, a.radius1.max(a.radius2), &mut s.ids);
        for q in s.quadrants.iter_mut() {
            q.clear();
        }
        for &i in s.ids.iter() {
            let (rx, ry) = self.offsets(i, px, py);
            let (rxsq, rysq) = (rx * rx, ry * ry);
            if r2sq * rxsq + r1sq * rysq <= r12sq {
                s.quadrants[quadrant(rx, ry)].push((rxsq + rysq, self.z[i as usize]));
            }
        }
        sort_quadrants(&mut s.quadrants);
        let (mut acc, mut min, mut max) = (0.0, f64::MAX, -f64::MAX);
        let mut per = [0u32; 4];
        let mut n = 0u32;
        // Only the moving average stops at max_points.
        let max_points = if m == GridMethod::Average { a.max_points } else { 0 };
        round_robin(&s.quadrants, a.max_points_per_quadrant, max_points, &mut per, &mut n, |d2, z| match m {
            GridMethod::Average => acc += z,
            GridMethod::Minimum => {
                if min > z {
                    min = z;
                }
            }
            GridMethod::Maximum => {
                if max < z {
                    max = z;
                }
            }
            GridMethod::Range => {
                if min > z {
                    min = z;
                }
                if max < z {
                    max = z;
                }
            }
            GridMethod::AverageDistance => acc += d2.sqrt(),
            _ => {}
        });
        if a.min_points_per_quadrant > 0 && per.iter().any(|&c| c < a.min_points_per_quadrant) {
            return a.nodata;
        }
        finish_metric(m, a, n, acc, min, max)
    }

    /// `GDALGridDataMetricAverageDistancePts`.
    fn average_distance_points(&self, px: f64, py: f64, ids: &mut Vec<u32>) -> f64 {
        let a = self.alg;
        let (r1sq, r2sq) = (a.radius1 * a.radius1, a.radius2 * a.radius2);
        let r12sq = r1sq * r2sq;
        let (x, y) = (self.x, self.y);
        let mut acc = 0.0;
        let n;
        if self.quadtree {
            self.quadtree_search(px, py, a.radius1.max(a.radius2), ids);
            // As GDAL: unrotated offsets, and each point paired with itself too (the last one
            // returned excepted).
            let inside: Vec<bool> = ids
                .iter()
                .map(|&i| {
                    let (rx, ry) = (x[i as usize] - px, y[i as usize] - py);
                    r2sq * rx * rx + r1sq * ry * ry <= r12sq
                })
                .collect();
            let count = ids.len();
            n = distance_pair_count(inside.iter().filter(|v| **v).count(), true, inside.last() == Some(&true));
            for k in 0..count.saturating_sub(1) {
                if !inside[k] {
                    continue;
                }
                let i = ids[k] as usize;
                for j in k..count {
                    if inside[j] {
                        let ji = ids[j] as usize;
                        let (dx, dy) = (x[ji] - x[i], y[ji] - y[i]);
                        acc += (dx * dx + dy * dy).sqrt();
                    }
                }
            }
        } else {
            self.scan_candidates(px, py, ids);
            let inside: Vec<u32> = ids
                .iter()
                .copied()
                .filter(|&i| {
                    let (rx, ry) = self.offsets(i, px, py);
                    r2sq * rx * rx + r1sq * ry * ry <= r12sq
                })
                .collect();
            n = distance_pair_count(inside.len(), false, false);
            // GDAL's scan stops before the last point: it has no later point to pair with.
            for (k, &i) in inside.iter().enumerate() {
                for &j in &inside[k + 1..] {
                    let (dx, dy) = (x[j as usize] - x[i as usize], y[j as usize] - y[i as usize]);
                    acc += (dx * dx + dy * dy).sqrt();
                }
            }
        }
        if n < u64::from(a.min_points) || n == 0 { a.nodata } else { acc / n as f64 }
    }

    /// `GDALGridLinear` over a block: in a triangle, its plane; elsewhere the nodata value, or the
    /// nearest point (GDAL's quadtree search when cells on the block's edges are outside the
    /// triangulation, else its scan).
    fn linear(&mut self, b: &Block, out: &mut [f64]) {
        let a = self.alg;
        let (cols, rows) = (b.cols, b.rows);
        let t = self.triangles.as_ref().expect("triangulation");
        let mut done = vec![false; cols * rows];
        t.rasterize(self.z, b, out, &mut done);
        if !self.quadtree {
            // GDAL samples the block's edges: when all are in the triangulation, so is the rest.
            let mut need = false;
            for j in 0..rows {
                need |= !done[j * cols] || !done[j * cols + cols - 1];
            }
            for i in 1..cols.saturating_sub(1) {
                need |= !done[i] || !done[(rows - 1) * cols + i];
            }
            if need {
                self.create_quadtree();
            }
        }
        if a.radius == 0.0 {
            for (v, d) in out.iter_mut().zip(&done) {
                if !d {
                    *v = a.nodata;
                }
            }
            return;
        }
        let r = if a.radius < 0.0 || a.radius >= f64::MAX { 0.0 } else { a.radius };
        let todo: Vec<usize> = (0..cols * rows).filter(|k| !done[*k]).collect();
        let this = &*self;
        let found = crate::par::map_range(todo.len(), |k| {
            let (px, py) = b.centre(todo[k] % cols, todo[k] / cols);
            this.nearest(px, py, r, r, false, &mut Vec::new())
        });
        for (k, v) in todo.into_iter().zip(found) {
            out[k] = v;
        }
    }
}

/// The margin around a search square, for floating-point rounding in the R-tree.
fn margin(r: f64, px: f64, py: f64) -> f64 {
    (r.abs() + px.abs().max(py.abs())) * 1e-12
}

trait ToRadiansGdal {
    fn to_radians_gdal(self) -> f64;
}

impl ToRadiansGdal for f64 {
    /// Degrees to radians as GDAL (`M_PI / 180.0 * angle`).
    fn to_radians_gdal(self) -> f64 {
        std::f64::consts::PI / 180.0 * self
    }
}

/// The quadrant of an offset from the node, as GDAL numbers them.
fn quadrant(rx: f64, ry: f64) -> usize {
    (if rx >= 0.0 { 1 } else { 0 }) | ((if ry >= 0.0 { 1 } else { 0 }) << 1)
}

/// Sorts each quadrant's `(squared distance, value)` pairs by distance, keeping the order of
/// equal distances (as `std::multimap`).
fn sort_quadrants(q: &mut [Vec<(f64, f64)>; 4]) {
    for list in q.iter_mut() {
        list.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    }
}

/// GDAL's per-quadrant loop: the nearest point of quadrant 0, then of 1, 2 and 3, and again,
/// until every quadrant is exhausted or has given `max_per_quadrant` points (0: no limit), or
/// `max_points` points are taken (0: no limit). Without per-quadrant parameters, everything is
/// in quadrant 0.
fn round_robin(
    q: &[Vec<(f64, f64)>; 4],
    max_per_quadrant: u32,
    max_points: u32,
    per: &mut [u32; 4],
    n: &mut u32,
    mut f: impl FnMut(f64, f64),
) {
    let mut pos = [0usize; 4];
    let mut finished = 0u8;
    let mut iq = 0usize;
    loop {
        if pos[iq] == q[iq].len() || (max_per_quadrant > 0 && per[iq] >= max_per_quadrant) {
            finished |= 1 << iq;
            if finished == 0b1111 {
                break;
            }
            iq = (iq + 1) % 4;
            continue;
        }
        let (d2, z) = q[iq][pos[iq]];
        pos[iq] += 1;
        f(d2, z);
        *n += 1;
        per[iq] += 1;
        if max_points > 0 && *n >= max_points {
            break;
        }
        iq = (iq + 1) % 4;
    }
}

/// The value of a moving average or data metric from what was accumulated.
fn finish_metric(m: GridMethod, a: &GridAlgorithm, n: u32, acc: f64, min: f64, max: f64) -> f64 {
    match m {
        GridMethod::Count => {
            if n < a.min_points {
                a.nodata
            } else {
                n as f64
            }
        }
        _ if n < a.min_points || n == 0 => a.nodata,
        GridMethod::Average | GridMethod::AverageDistance => acc / n as f64,
        GridMethod::Minimum => min,
        GridMethod::Maximum => max,
        GridMethod::Range => max - min,
        _ => a.nodata,
    }
}

/// Per-thread buffers.
#[derive(Default)]
struct Scratch {
    ids: Vec<u32>,
    quadrants: [Vec<(f64, f64)>; 4],
}

/// The order in which GDAL's `CPLQuadTree` returns points (its `CPLQuadTreeSearch` lists the
/// features of a node, then those of its four sub-nodes in turn), built as `CPLQuadTreeInsert`
/// builds it: buckets of 8, split into quarters of 55 % of the node each way.
///
/// Returns each point's rank in that order.
fn quadtree_ranks(x: &[f64], y: &[f64], extent: [f64; 4]) -> Vec<u32> {
    struct Node {
        rect: [f64; 4],
        features: Vec<u32>,
        children: Option<[usize; 4]>,
    }
    const CAPACITY: usize = 8;
    const RATIO: f64 = 0.55;
    fn split(r: [f64; 4]) -> ([f64; 4], [f64; 4]) {
        let (mut a, mut b) = (r, r);
        let [minx, miny, maxx, maxy] = r;
        if (maxx - minx) > (maxy - miny) {
            let range = maxx - minx;
            a[2] = minx + range * RATIO;
            b[0] = maxx - range * RATIO;
        } else {
            let range = maxy - miny;
            a[3] = miny + range * RATIO;
            b[1] = maxy - range * RATIO;
        }
        (a, b)
    }
    fn same(a: &[f64; 4], b: &[f64; 4]) -> bool {
        a.iter().zip(b).all(|(p, q)| p.to_bits() == q.to_bits())
    }
    fn contains(r: &[f64; 4], px: f64, py: f64) -> bool {
        px >= r[0] && px <= r[2] && py >= r[1] && py <= r[3]
    }
    fn add(nodes: &mut Vec<Node>, mut n: usize, f: u32, px: f64, py: f64, x: &[f64], y: &[f64]) {
        loop {
            match nodes[n].children {
                None => {
                    if nodes[n].features.len() >= CAPACITY {
                        let rect = nodes[n].rect;
                        let (h1, h2) = split(rect);
                        let (q1, q2) = split(h1);
                        let (q3, q4) = split(h2);
                        let quads = [q1, q2, q3, q4];
                        if quads.iter().all(|q| !same(&rect, q)) && quads.iter().any(|q| contains(q, px, py)) {
                            let first = nodes.len();
                            for q in quads {
                                nodes.push(Node { rect: q, features: Vec::new(), children: None });
                            }
                            nodes[n].children = Some([first, first + 1, first + 2, first + 3]);
                            let old = std::mem::take(&mut nodes[n].features);
                            for o in old {
                                add(nodes, n, o, x[o as usize], y[o as usize], x, y);
                            }
                            continue;
                        }
                    }
                    nodes[n].features.push(f);
                    return;
                }
                Some(children) => match children.iter().find(|&&c| contains(&nodes[c].rect, px, py)) {
                    Some(&c) => n = c,
                    None => {
                        nodes[n].features.push(f);
                        return;
                    }
                },
            }
        }
    }
    let mut nodes = vec![Node { rect: extent, features: Vec::new(), children: None }];
    for i in 0..x.len() {
        add(&mut nodes, 0, i as u32, x[i], y[i], x, y);
    }
    let mut rank = vec![0u32; x.len()];
    let mut next = 0u32;
    let mut stack = vec![0usize];
    while let Some(n) = stack.pop() {
        for &f in &nodes[n].features {
            rank[f as usize] = next;
            next += 1;
        }
        if let Some(c) = nodes[n].children {
            stack.extend(c.iter().rev());
        }
    }
    rank
}

/// The Delaunay triangulation of the points, with GDAL's barycentric coefficients.
struct Triangulation {
    /// Per triangle: its vertices and `(m1x, m1y, m2x, m2y, x3, y3)`; degenerate ones are left
    /// out, as GDAL skips them.
    triangles: Vec<([usize; 3], [f64; 6])>,
    x: Vec<f64>,
    y: Vec<f64>,
}

impl Triangulation {
    fn new(x: &[f64], y: &[f64]) -> Self {
        use spade::{HasPosition, Point2, Triangulation as _};
        struct Vertex(Point2<f64>, usize);
        impl HasPosition for Vertex {
            type Scalar = f64;
            fn position(&self) -> Point2<f64> {
                self.0
            }
        }
        // The first of points at the same place is a vertex, as qhull keeps it.
        let mut seen = std::collections::HashSet::new();
        let vertices: Vec<Vertex> = (0..x.len())
            .filter(|&i| seen.insert(((x[i] + 0.0).to_bits(), (y[i] + 0.0).to_bits())))
            .filter(|&i| spade::validate_coordinate(x[i]).is_ok() && spade::validate_coordinate(y[i]).is_ok())
            .map(|i| Vertex(Point2::new(x[i], y[i]), i))
            .collect();
        let t: spade::DelaunayTriangulation<Vertex> = spade::DelaunayTriangulation::bulk_load(vertices).unwrap_or_else(|e| {
            crate::warnings::warn(format!("linear triangulation failed: {e}"));
            Default::default()
        });
        let faces: Vec<[usize; 3]> = t.inner_faces().map(|f| f.vertices().map(|v| v.data().1)).collect();
        let mut triangles = Vec::with_capacity(faces.len());
        for tri in &faces {
            let (a, b, c) = (tri[0], tri[1], tri[2]);
            let (x1, y1, x2, y2, x3, y3) = (x[a], y[a], x[b], y[b], x[c], y[c]);
            let denom = (y2 - y3) * (x1 - x3) + (x3 - x2) * (y1 - y3);
            // GDAL v3.13.3 alg/delaunay.c uses the absolute test fabs(dfDenom) < 1e-5
            // in GDALTriangulationComputeBarycentricCoefficients, including its unit dependence.
            if denom.abs() < 1e-5 {
                continue;
            }
            triangles.push(([a, b, c], [(y2 - y3) / denom, (x3 - x2) / denom, (y3 - y1) / denom, (x1 - x3) / denom, x3, y3]));
        }
        Self { triangles, x: x.to_vec(), y: y.to_vec() }
    }

    /// Interpolates the cells of the block whose centres are in a triangle, marking them done.
    fn rasterize(&self, z: &[f64], b: &Block, out: &mut [f64], done: &mut [bool]) {
        let (cols, rows) = (b.cols, b.rows);
        // Cell index ranges of a coordinate interval, for either sign of the cell size.
        let range = |lo: f64, hi: f64, o: f64, d: f64, n: usize| -> Option<(usize, usize)> {
            let (a, c) = ((lo - o) / d - 0.5, (hi - o) / d - 0.5);
            let (a, c) = if a <= c { (a, c) } else { (c, a) };
            let first = (a.floor() - 1.0).max(0.0);
            let last = (c.ceil() + 1.0).min(n as f64 - 1.0);
            (last >= first && last >= 0.0 && first.is_finite() && last.is_finite()).then_some((first as usize, last as usize))
        };
        for (v, c) in &self.triangles {
            let (xs, ys) = ([self.x[v[0]], self.x[v[1]], self.x[v[2]]], [self.y[v[0]], self.y[v[1]], self.y[v[2]]]);
            let (minx, maxx) = (xs[0].min(xs[1]).min(xs[2]), xs[0].max(xs[1]).max(xs[2]));
            let (miny, maxy) = (ys[0].min(ys[1]).min(ys[2]), ys[0].max(ys[1]).max(ys[2]));
            let (Some((i0, i1)), Some((j0, j1))) = (range(minx, maxx, b.x0, b.dx, cols), range(miny, maxy, b.y0, b.dy, rows)) else {
                continue;
            };
            for j in j0..=j1 {
                for i in i0..=i1 {
                    let k = j * cols + i;
                    if done[k] {
                        continue;
                    }
                    let (px, py) = b.centre(i, j);
                    let l1 = c[0] * (px - c[4]) + c[1] * (py - c[5]);
                    if !(-EPS..=1.0 + EPS).contains(&l1) {
                        continue;
                    }
                    let l2 = c[2] * (px - c[4]) + c[3] * (py - c[5]);
                    if !(-EPS..=1.0 + EPS).contains(&l2) {
                        continue;
                    }
                    let l3 = 1.0 - l1 - l2;
                    if !(-EPS..=1.0 + EPS).contains(&l3) {
                        continue;
                    }
                    out[k] = l1 * z[v[0]] + l2 * z[v[1]] + l3 * z[v[2]];
                    done[k] = true;
                }
            }
        }
    }
}

/// Options of [`vector_grid`], named after `gdal vector grid`'s (and `gdal_grid`'s).
#[derive(Debug, Clone)]
pub struct VectorGridOptions {
    /// The algorithm and its parameters (`-a`).
    pub algorithm: GridAlgorithm,
    /// `--zfield`: take the values from this field rather than from the geometries' Z.
    pub z_field: Option<String>,
    /// `--zoffset` (`-z_increase`): added to the values.
    pub z_offset: f64,
    /// `--zmultiply` (`-z_multiply`): the values (offset) are multiplied by it.
    pub z_multiply: f64,
    /// `--where`: only grid the features matching this OGR SQL expression.
    pub where_clause: Option<String>,
    /// `--bbox` (`-clipsrc`): only grid the points strictly inside this rectangle
    /// `(min_x, min_y, max_x, max_y)`.
    pub bbox: Option<(f64, f64, f64, f64)>,
    /// `--extent` (`-txe`, `-tye`): the grid's extent `(min_x, min_y, max_x, max_y)`. By default,
    /// the first layer's.
    pub extent: Option<(f64, f64, f64, f64)>,
    /// `--size` (`-outsize`): the size in cells. 256 × 256 when neither it nor the resolution is
    /// given.
    pub size: Option<(usize, usize)>,
    /// `--resolution` (`-tr`): the cell size, which needs the extent.
    pub resolution: Option<(f64, f64)>,
    /// `--ot`: the output data type. Float64 by default.
    pub data_type: DataType,
    /// `--crs` (`-a_srs`): the output CRS. By default, the first layer's.
    pub crs: Option<SpatialRef>,
}

impl Default for VectorGridOptions {
    fn default() -> Self {
        Self {
            algorithm: GridAlgorithm::new(GridMethod::InverseDistance),
            z_field: None,
            z_offset: 0.0,
            z_multiply: 1.0,
            where_clause: None,
            bbox: None,
            extent: None,
            size: None,
            resolution: None,
            data_type: DataType::F64,
            crs: None,
        }
    }
}

/// The points of a layer to grid, as `gdal_grid` collects them: every vertex of every geometry
/// (points, line and polygon vertices), with the value of `z_field` or the vertex's Z (0 for 2D
/// vertices, skipped when NaN), offset then multiplied. Features whose field is null are skipped.
pub fn layer_points(layer: &Layer, options: &VectorGridOptions) -> Result<(Vec<f64>, Vec<f64>, Vec<f64>)> {
    let filter = options.where_clause.as_deref().map(|w| Expression::parse(w)?.bind(&layer.schema)).transpose()?;
    let field = match &options.z_field {
        Some(f) => Some(
            field_index(&layer.schema, f)
                .ok_or_else(|| Error::InvalidArgument(format!("failed to find field {f} on layer {}", layer.name)))?,
        ),
        None => None,
    };
    let (mut xs, mut ys, mut zs) = (Vec::new(), Vec::new(), Vec::new());
    let (inc, mul) = (options.z_offset, options.z_multiply);
    let mut warned = false;
    for f in &layer.features {
        let Some(g) = &f.geometry else { continue };
        if filter.as_ref().is_some_and(|e| !e.matches(f)) {
            continue;
        }
        let value = match field {
            Some(i) => match field_value(f, i) {
                Some(v) => Some(v),
                None => continue,
            },
            None => None,
        };
        if value.is_none() && !warned && !crate::vector::geometry_has_z(g) {
            warned = true;
            crate::warnings::warn(format!(
                "at least one geometry of layer '{}' lacks a Z component: its points are gridded with Z 0 (set a Z field?)",
                layer.name
            ));
        }
        visit_coords(g, &mut |c: &Coord| {
            if let Some((x0, y0, x1, y1)) = options.bbox
                && !(c.x > x0 && c.x < x1 && c.y > y0 && c.y < y1)
            {
                return;
            }
            let z = match value {
                Some(v) => v,
                None => {
                    let z = c.z.unwrap_or(0.0);
                    if z.is_nan() {
                        return;
                    }
                    z
                }
            };
            if !c.x.is_finite() || !c.y.is_finite() {
                return;
            }
            xs.push(c.x);
            ys.push(c.y);
            zs.push((z + inc) * mul);
        });
    }
    Ok((xs, ys, zs))
}

/// A field as GDAL's `GetFieldAsDouble` reads it; None when null.
fn field_value(f: &Feature, i: usize) -> Option<f64> {
    match f.attributes.get(i) {
        None | Some(FieldValue::Null) => None,
        Some(FieldValue::Integer(v)) => Some(*v as f64),
        Some(FieldValue::Float(v)) => Some(*v),
        Some(FieldValue::Boolean(b)) => Some(*b as i64 as f64),
        Some(FieldValue::Text(s)) => Some(leading_number(s)),
        Some(_) => Some(0.0),
    }
}

/// The number at the start of a string, as C's `atof`; 0 when there is none.
pub(crate) fn leading_number(s: &str) -> f64 {
    let s = s.trim_start();
    let ends: Vec<usize> = s.char_indices().map(|(i, _)| i).skip(1).chain(std::iter::once(s.len())).collect();
    ends.into_iter().rev().find_map(|end| s[..end].parse::<f64>().ok()).unwrap_or(0.0)
}

/// Calls `f` with every vertex of a geometry, as GDAL's default geometry visitor (rings closed).
fn visit_coords(g: &Geometry, f: &mut dyn FnMut(&Coord)) {
    let ring = |r: &[Coord], f: &mut dyn FnMut(&Coord)| {
        r.iter().for_each(&mut *f);
        if let (Some(a), Some(b)) = (r.first(), r.last())
            && r.len() > 1
            && (a.x != b.x || a.y != b.y)
        {
            f(a);
        }
    };
    match g {
        Geometry::Point(c) => f(c),
        Geometry::MultiPoint(cs) | Geometry::LineString(cs) => cs.iter().for_each(f),
        Geometry::MultiLineString(ls) => ls.iter().for_each(|l| l.iter().for_each(&mut *f)),
        Geometry::Polygon { exterior, interiors } => std::iter::once(exterior).chain(interiors).for_each(|r| ring(&r.0, f)),
        Geometry::MultiPolygon(ps) => {
            for (e, holes) in ps {
                std::iter::once(e).chain(holes).for_each(|r| ring(&r.0, f));
            }
        }
        Geometry::GeometryCollection(gs) => gs.iter().for_each(|g| visit_coords(g, f)),
    }
}

/// Grids the points of each layer into a band of a new raster, as `gdal_grid` (and
/// `gdal vector grid`) does.
///
/// The extent is `options.extent`, or the first layer's (its features before the `where` filter
/// and the bounding box, as GDAL drivers with stored extents report it). The size is
/// `options.size`, or from `options.resolution` as `(|max - min| + res / 2) / res` cells (the cell
/// size is then the extent over that), or 256 × 256. Geometries are not reprojected.
pub fn vector_grid_from_layers(layers: &[Layer], sql: Option<&str>, named: bool, options: &VectorGridOptions) -> Result<RasterDataset> {
    if named && sql.is_some() {
        return Err(Error::InvalidArgument("give layer names or sql, not both".into()));
    }
    let selected;
    let layers = if let Some(sql) = sql {
        selected = super::vector_filter::translate(
            layers,
            &super::vector_filter::VectorTranslateOptions { sql: Some(sql.into()), ..Default::default() },
        )?;
        &selected
    } else {
        if !named && layers.len() != 1 {
            return Err(Error::InvalidArgument(format!(
                "the dataset has {} layers: choose layer names or sql ({})",
                layers.len(),
                layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
        layers
    };
    vector_grid(layers, options)
}

/// Grids already-selected layers using the supplied options.
pub fn vector_grid(layers: &[Layer], options: &VectorGridOptions) -> Result<RasterDataset> {
    if layers.is_empty() {
        return Err(Error::InvalidArgument("no layer to grid".into()));
    }
    if options.size.is_some() && options.resolution.is_some() {
        return Err(Error::InvalidArgument("the size and the resolution cannot be used at the same time".into()));
    }
    let mut algorithm = options.algorithm.clone();
    algorithm.validate()?;
    let (x0, y0, x1, y1) = match options.extent {
        Some(e) => e,
        None => {
            if options.resolution.is_some() {
                return Err(Error::InvalidArgument("the resolution needs the extent".into()));
            }
            crate::vector::layer_extent(&layers[0])
                .ok_or_else(|| Error::InvalidArgument(format!("no point geometry found on layer {}", layers[0].name)))?
        }
    };
    let (cols, rows) = match (options.size, options.resolution) {
        (Some((w, h)), _) => (w, h),
        (None, Some((rx, ry))) => {
            let w = ((x1 - x0).abs() + rx / 2.0) / rx;
            let h = ((y1 - y0).abs() + ry / 2.0) / ry;
            if !(w >= 1.0 && w <= i32::MAX as f64 && h >= 1.0 && h <= i32::MAX as f64) {
                return Err(Error::InvalidArgument("invalid output size: check the resolution".into()));
            }
            (w as usize, h as usize)
        }
        (None, None) => (256, 256),
    };
    if cols == 0 || rows == 0 {
        return Err(Error::InvalidArgument("output raster dimensions should have non-zero size".into()));
    }
    check_size(cols, rows, layers.len())?;
    let extent = GridExtent { x_min: x0, x_max: x1, y_min: y0.min(y1), y_max: y0.max(y1), cols, rows };
    check_extent(&extent)?;
    let bands = layers.len();
    let mut data = Vec::new();
    data.try_reserve_exact(cols * rows * bands).map_err(|_| Error::InvalidArgument("cannot allocate the output grid".into()))?;
    let dt = options.data_type;
    for layer in layers {
        let (xs, ys, zs) = layer_points(layer, options)?;
        if xs.is_empty() {
            return Err(Error::InvalidArgument(format!("no point geometry found on layer {}", layer.name)));
        }
        let first = data.len();
        grid_points_into(&xs, &ys, &zs, &algorithm, &extent, (dt, bands), &mut data)?;
        for value in &mut data[first..] {
            *value = crate::ops::convert_value(*value, dt);
        }
    }
    let crs = match &options.crs {
        Some(c) => c.clone(),
        None => SpatialRef::from_layer(&layers[0]),
    };
    // GDAL rounds Float32 band nodata to the stored sample precision. Integer
    // outputs retain GDAL's original declared value even when empty cells saturate.
    let band_nodata = algorithm.nodata_set.then_some(if dt == DataType::F32 { algorithm.nodata as f32 as f64 } else { algorithm.nodata });
    // GDAL's geotransform: (x_min, (x_max - x_min) / cols, 0, top, 0, (bottom - top) / rows).
    let (top, bottom) = (extent.y_max, extent.y_min);
    let gt = [x0, (x1 - x0) / cols as f64, 0.0, top, 0.0, (bottom - top) / rows as f64];
    let cfg = RasterConfig {
        cols,
        rows,
        bands,
        x_min: gt[0],
        y_min: gt[3] + gt[5] * rows as f64,
        cell_size: gt[1],
        cell_size_y: Some(-gt[5]),
        nodata: band_nodata.unwrap_or(algorithm.nodata),
        data_type: dt,
        crs: wbraster::CrsInfo { epsg: crs.epsg, wkt: crs.to_wkt(), proj4: crs.proj4.clone() },
        metadata: Vec::new(),
    };
    let raster = crate::pixel::raster_from_data(cfg, typed(data, dt))?;
    let mut ds = RasterDataset::new(raster, band_nodata.is_some());
    ds.set_geo_transform(gt);
    Ok(ds)
}

/// The values as a buffer of the data type (already converted to its range).
fn typed(v: Vec<f64>, dt: DataType) -> wbraster::raster::RasterData {
    use wbraster::raster::RasterData as R;
    match dt {
        DataType::U8 => R::U8(v.into_iter().map(|x| x as u8).collect()),
        DataType::I8 => R::I8(v.into_iter().map(|x| x as i8).collect()),
        DataType::U16 => R::U16(v.into_iter().map(|x| x as u16).collect()),
        DataType::I16 => R::I16(v.into_iter().map(|x| x as i16).collect()),
        DataType::U32 => R::U32(v.into_iter().map(|x| x as u32).collect()),
        DataType::I32 => R::I32(v.into_iter().map(|x| x as i32).collect()),
        DataType::U64 => R::U64(v.into_iter().map(|x| x as u64).collect()),
        DataType::I64 => R::I64(v.into_iter().map(|x| x as i64).collect()),
        DataType::F32 => R::F32(v.into_iter().map(|x| x as f32).collect()),
        DataType::F64 => R::F64(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(x: &[f64], y: &[f64], z: &[f64], a: &GridAlgorithm, e: [f64; 4], cols: usize, rows: usize) -> Vec<f64> {
        let extent = GridExtent { x_min: e[0], y_min: e[1], x_max: e[2], y_max: e[3], cols, rows };
        grid_points(x, y, z, a, &extent, DataType::F64, 1).unwrap()
    }

    #[test]
    fn direct_quadrant_options_reject_zero_search_radius() {
        let mut algorithm = GridAlgorithm { min_points_per_quadrant: 1, ..GridAlgorithm::new(GridMethod::InverseDistance) };
        assert!(algorithm.validate().is_err());
        let mut algorithm =
            GridAlgorithm { radius1: 2.0, radius2: 2.0, min_points_per_quadrant: 1, ..GridAlgorithm::new(GridMethod::InverseDistance) };
        algorithm.validate().unwrap();
        assert_eq!(algorithm.method, GridMethod::InverseDistanceNearest);
        assert_eq!(algorithm.radius, 2.0);
    }

    #[test]
    fn keyword_and_packed_quadrant_parameters_share_normalization() {
        for radius in [0., 2.] {
            let packed = GridAlgorithm::parse(&format!("invdist:radius1={radius}:radius2={radius}:min_points_per_quadrant=1"));
            let keywords = GridAlgorithm::with_params(
                "invdist",
                &GridParams { radius1: Some(radius), radius2: Some(radius), min_points_per_quadrant: Some(1), ..Default::default() },
            );
            assert_eq!(packed.is_ok(), keywords.is_ok());
            if let (Ok(packed), Ok(keywords)) = (packed, keywords) {
                assert_eq!(packed.to_gdal_string(), keywords.to_gdal_string());
            }
        }
        for limit in [0, 1] {
            let packed = GridAlgorithm::parse(&format!("invdist:min_points_per_quadrant={limit}:max_points=0")).unwrap();
            let keywords = GridAlgorithm::with_params(
                "invdist",
                &GridParams { min_points_per_quadrant: Some(limit), max_points: Some(0), ..Default::default() },
            )
            .unwrap();
            assert_eq!(packed.to_gdal_string(), keywords.to_gdal_string());
        }
        let options = GridParams { radius1: Some(2.), radius2: Some(2.), ..Default::default() };
        assert_eq!(GridAlgorithm::with_params("invdist:radius=3", &options).unwrap().radius1, 2.);
        let options = GridParams { radius: Some(4.), ..Default::default() };
        assert_eq!(GridAlgorithm::with_params("invdist:radius1=3:radius2=3", &options).unwrap().radius1, 4.);
        let _ = crate::warnings::take();
        GridAlgorithm::parse("invdist:min_points_per_quadrant=1:radius1=2:radius2=2").unwrap();
        assert!(crate::warnings::take().is_empty());
    }

    #[test]
    fn malformed_ignored_options_do_not_fail_parsing() {
        let options = GridAlgorithm::parse("nearest:power=abc:max_points=not-a-number").unwrap();
        assert_eq!(options.method, GridMethod::Nearest);
        assert!(GridAlgorithm::parse("invdist:power=abc").is_err());
        assert!(GridAlgorithm::parse("average:min_points=abc").is_err());
    }

    #[test]
    fn unbounded_metrics_are_constant_and_honor_minimum_points() {
        for (name, expected) in [("average", 4.0), ("minimum", 1.0), ("maximum", 9.0), ("range", 8.0), ("count", 3.0)] {
            let a = GridAlgorithm::parse(name).unwrap();
            assert_eq!(grid(&[1., 2., 8.], &[3., 7., 9.], &[1., 2., 9.], &a, [-100., -100., 100., 100.], 9, 4), vec![expected; 36]);
            let a = GridAlgorithm::parse(&format!("{name}:min_points=4:nodata=-1")).unwrap();
            assert_eq!(grid(&[1., 2., 8.], &[3., 7., 9.], &[1., 2., 9.], &a, [-100., -100., 100., 100.], 9, 4), vec![-1.; 36]);
        }
    }

    #[test]
    fn one_zero_axis_matches_gdal_scan_and_quadtree() {
        // GDAL 3.13.3 clips the quadtree query to a square even though the ellipse
        // degenerates into an unbounded line. Preserve its 100-point threshold.
        for (n, count, average, distance) in [(99, 99., 49., 33.333333333333336), (101, 3., 50., 0.8)] {
            let x = vec![0.; n];
            let y: Vec<f64> = (0..n).map(|i| i as f64 - (n / 2) as f64).collect();
            let z: Vec<f64> = (0..n).map(|i| i as f64).collect();
            for (name, expected) in [("count", count), ("average", average), ("average_distance_pts", distance)] {
                let a = GridAlgorithm::parse(&format!("{name}:radius1=0:radius2=1")).unwrap();
                assert_eq!(grid(&x, &y, &z, &a, [-0.5, -0.5, 0.5, 0.5], 1, 1), vec![expected]);
            }
        }
    }

    #[test]
    fn parses_gdal_strings() {
        let a = GridAlgorithm::parse("invdist:power=3:radius1=10:radius2=5:angle=30:max_points=4:nodata=-1").unwrap();
        assert_eq!(
            (a.method, a.power, a.radius1, a.radius2, a.angle, a.max_points, a.nodata),
            (GridMethod::InverseDistance, 3.0, 10.0, 5.0, 30.0, 4, -1.0)
        );
        let a = GridAlgorithm::parse("invdist:radius=7:min_points_per_quadrant=1").unwrap();
        assert_eq!((a.method, a.radius, a.max_points), (GridMethod::InverseDistanceNearest, 7.0, 12));
        assert!(GridAlgorithm::parse("invdist:radius1=1:radius2=2:max_points_per_quadrant=1").is_err());
        assert!(GridAlgorithm::parse("invdistnn:radius=0").is_err());
        assert!(GridAlgorithm::parse("average_distance_pts:radius=1:min_points_per_quadrant=1").is_err());
        assert_eq!(GridAlgorithm::parse("linear").unwrap().radius, -1.0);
        let a = GridAlgorithm::parse("average:radius=4").unwrap();
        assert_eq!((a.radius1, a.radius2), (4.0, 4.0));
        assert!(GridAlgorithm::parse("kriging").is_err());
        assert!(GridAlgorithm::parse("invdist:radius=1:max_points=-5").is_err());
        assert!(GridAlgorithm::parse("average:radius=1:min_points=nan").is_err());
    }

    #[test]
    fn rejects_huge_or_non_finite_grids() {
        let a = GridAlgorithm::new(GridMethod::Nearest);
        let e = GridExtent { x_min: 0.0, x_max: 1.0, y_min: 0.0, y_max: 1.0, cols: usize::MAX, rows: 2 };
        assert!(grid_points(&[0.5], &[0.5], &[1.0], &a, &e, DataType::F64, 1).is_err());
        let e = GridExtent { x_min: f64::NAN, cols: 1, rows: 1, ..e };
        assert!(grid_points(&[0.5], &[0.5], &[1.0], &a, &e, DataType::F64, 1).is_err());
    }

    #[test]
    fn nodes_are_cell_centres() {
        // One point per cell centre of a 4 × 3 grid over [0, 4] × [0, 3]: nearest returns them.
        let (mut x, mut y, mut z) = (vec![], vec![], vec![]);
        for j in 0..3 {
            for i in 0..4 {
                x.push(i as f64 + 0.5);
                y.push(j as f64 + 0.5);
                z.push((j * 10 + i) as f64);
            }
        }
        let v = grid(&x, &y, &z, &GridAlgorithm::new(GridMethod::Nearest), [0.0, 0.0, 4.0, 3.0], 4, 3);
        // North row first.
        assert_eq!(v, vec![20.0, 21.0, 22.0, 23.0, 10.0, 11.0, 12.0, 13.0, 0.0, 1.0, 2.0, 3.0]);
        // Inverse distance takes the value of a point on the node.
        let v = grid(&x, &y, &z, &GridAlgorithm::new(GridMethod::InverseDistance), [0.0, 0.0, 4.0, 3.0], 4, 3);
        assert_eq!(v[0], 20.0);
    }

    #[test]
    fn inverse_distance_matches_formula() {
        let (x, y, z) = ([0.0, 10.0], [0.0, 0.0], [1.0, 3.0]);
        let a = GridAlgorithm::new(GridMethod::InverseDistance);
        // The node (2.5, 0.5): weights 1 / r².
        let v = grid(&x, &y, &z, &a, [2.0, 0.0, 3.0, 1.0], 1, 1)[0];
        let (w1, w2) = (1.0 / (2.5f64 * 2.5 + 0.25), 1.0 / (7.5f64 * 7.5 + 0.25));
        assert_eq!(v, (w1 * 1.0 + w2 * 3.0) / (w1 + w2));
        // A search ellipse excluding the far point, and max_points counting as GDAL (one more).
        let mut a = GridAlgorithm::new(GridMethod::InverseDistance);
        a.set_radius(5.0);
        assert_eq!(grid(&x, &y, &z, &a, [2.0, 0.0, 3.0, 1.0], 1, 1)[0], 1.0);
        a.min_points = 2;
        assert_eq!(grid(&x, &y, &z, &a, [2.0, 0.0, 3.0, 1.0], 1, 1)[0], 0.0);
    }

    #[test]
    fn search_ellipse_rotates() {
        // A point 4 east of the node: inside an ellipse of 5 × 1, outside once turned 90°.
        let (x, y, z) = ([4.5], [0.5], [7.0]);
        let mut a = GridAlgorithm::new(GridMethod::Average);
        (a.radius1, a.radius2, a.nodata) = (5.0, 1.0, -1.0);
        assert_eq!(grid(&x, &y, &z, &a, [0.0, 0.0, 1.0, 1.0], 1, 1)[0], 7.0);
        a.angle = 90.0;
        assert_eq!(grid(&x, &y, &z, &a, [0.0, 0.0, 1.0, 1.0], 1, 1)[0], -1.0);
        let mut m = a.clone();
        m.method = GridMethod::Count;
        m.angle = 0.0;
        assert_eq!(grid(&x, &y, &z, &m, [0.0, 0.0, 1.0, 1.0], 1, 1)[0], 1.0);
    }

    #[test]
    fn metrics_and_quadrants() {
        let x = [1.0, -1.0, -1.0, 1.0, 2.0];
        let y = [1.0, 1.0, -1.0, -1.0, 2.0];
        let z = [1.0, 2.0, 3.0, 4.0, 10.0];
        let e = [-0.5, -0.5, 0.5, 0.5];
        let mut a = GridAlgorithm::new(GridMethod::Minimum);
        a.set_radius(5.0);
        assert_eq!(grid(&x, &y, &z, &a, e, 1, 1)[0], 1.0);
        a.method = GridMethod::Maximum;
        assert_eq!(grid(&x, &y, &z, &a, e, 1, 1)[0], 10.0);
        a.method = GridMethod::Range;
        assert_eq!(grid(&x, &y, &z, &a, e, 1, 1)[0], 9.0);
        a.method = GridMethod::Count;
        assert_eq!(grid(&x, &y, &z, &a, e, 1, 1)[0], 5.0);
        // One point per quadrant at most: the nearest of each.
        a.method = GridMethod::Average;
        a.max_points_per_quadrant = 1;
        assert_eq!(grid(&x, &y, &z, &a, e, 1, 1)[0], 2.5);
        a.max_points_per_quadrant = 0;
        a.min_points_per_quadrant = 2;
        assert_eq!(grid(&x, &y, &z, &a, e, 1, 1)[0], 0.0);
        a.method = GridMethod::AverageDistance;
        a.min_points_per_quadrant = 0;
        let d = (2.0f64).sqrt();
        assert!((grid(&x, &y, &z, &a, e, 1, 1)[0] - (4.0 * d + 8f64.sqrt()) / 5.0).abs() < 1e-12);
        a.method = GridMethod::AverageDistancePoints;
        let v = grid(&x[..4], &y[..4], &z[..4], &a, e, 1, 1)[0];
        assert!((v - (4.0 * 2.0 + 2.0 * 8f64.sqrt()) / 6.0).abs() < 1e-12);
    }

    #[test]
    fn linear_interpolates_planes() {
        // z = 2x + 3y on a triangle, and nodata outside it with radius 0.
        let (x, y) = ([0.0, 10.0, 0.0], [0.0, 0.0, 10.0]);
        let z: Vec<f64> = x.iter().zip(&y).map(|(x, y)| 2.0 * x + 3.0 * y).collect();
        let mut a = GridAlgorithm::new(GridMethod::Linear);
        a.radius = 0.0;
        a.nodata = -9.0;
        let v = grid(&x, &y, &z, &a, [0.0, 0.0, 10.0, 10.0], 10, 10);
        // The cell (0, 9) from the north, centre (0.5, 0.5).
        assert!((v[90] - 2.5).abs() < 1e-12);
        // The north-east corner is outside.
        assert_eq!(v[9], -9.0);
        // Without a radius, the nearest point.
        a.radius = -1.0;
        let v = grid(&x, &y, &z, &a, [0.0, 0.0, 10.0, 10.0], 10, 10);
        assert!(v[9] == z[1] || v[9] == z[2]);
    }

    #[test]
    fn quadtree_order_lists_nodes_depth_first() {
        // Nine points split the root: those in each quarter come after the root's.
        let x: Vec<f64> = (0..20).map(|i| (i % 5) as f64).collect();
        let y: Vec<f64> = (0..20).map(|i| (i / 5) as f64).collect();
        let r = quadtree_ranks(&x, &y, [0.0, 0.0, 4.0, 3.0]);
        let mut seen = r.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..20).collect::<Vec<u32>>());
    }

    #[test]
    fn nan_field_values_and_geometry_z_follow_gdal() {
        use wbvector::{FieldDef, FieldType};
        let mut layer = Layer::new("points");
        layer.schema.add_field(FieldDef::new("value", FieldType::Text));
        for (i, z, value) in [(0, f64::NAN, "nan"), (1, 5., "5")] {
            layer.features.push(Feature {
                fid: i,
                geometry: Some(Geometry::point_z(i as f64, 0., z)),
                attributes: vec![FieldValue::Text(value.into())],
            });
        }
        let mut options = VectorGridOptions {
            algorithm: GridAlgorithm::new(GridMethod::Average),
            z_field: Some("value".into()),
            extent: Some((-0.5, -0.5, 1.5, 0.5)),
            size: Some((1, 1)),
            ..Default::default()
        };
        assert!(vector_grid(&[layer.clone()], &options).unwrap().raster.get(0, 0, 0).is_nan());
        options.z_field = None;
        assert_eq!(vector_grid(&[layer], &options).unwrap().raster.get(0, 0, 0), 5.);
    }

    #[test]
    fn float32_nodata_matches_the_stored_empty_cell() {
        let mut layer = Layer::new("points");
        layer.features.push(Feature { fid: 0, geometry: Some(Geometry::point_z(100., 100., 1.)), attributes: vec![] });
        let options = VectorGridOptions {
            algorithm: GridAlgorithm::parse("nearest:radius=0.001:nodata=-9999.1").unwrap(),
            extent: Some((0., 0., 1., 1.)),
            size: Some((1, 1)),
            data_type: DataType::F32,
            ..Default::default()
        };
        let output = vector_grid(&[layer], &options).unwrap();
        assert_eq!(output.band_nodata(1), Some(-9999.1_f32 as f64));
        assert_eq!(output.raster.get(0, 0, 0), output.band_nodata(1).unwrap());
    }

    #[test]
    fn grids_layers() {
        use wbvector::{FieldDef, FieldType};
        let mut layer = Layer::new("pts");
        layer.schema.add_field(FieldDef::new("v", FieldType::Float));
        for (i, (x, y)) in [(0.0, 0.0), (4.0, 0.0), (0.0, 4.0), (4.0, 4.0)].into_iter().enumerate() {
            layer.features.push(Feature {
                fid: i as u64,
                geometry: Some(Geometry::point(x, y)),
                attributes: vec![FieldValue::Float(i as f64)],
            });
        }
        let options = VectorGridOptions {
            algorithm: GridAlgorithm::new(GridMethod::Nearest),
            z_field: Some("v".into()),
            size: Some((2, 2)),
            ..Default::default()
        };
        let ds = vector_grid(&[layer], &options).unwrap();
        assert_eq!(ds.geo_transform(), [0.0, 2.0, 0.0, 4.0, 0.0, -2.0]);
        assert!(!ds.has_nodata);
        let v: Vec<f64> = (0..2).flat_map(|r| (0..2).map(move |c| (r, c))).map(|(r, c)| ds.raster.get(0, r as isize, c as isize)).collect();
        assert_eq!(v, vec![2.0, 3.0, 0.0, 1.0]);
    }
    #[test]
    fn gdal_algorithm_string_roundtrips() {
        let a = GridAlgorithm::parse("invdist:power=3:smoothing=2:radius1=4:radius2=5:angle=6:nodata=-9").unwrap();
        let b = GridAlgorithm::parse(&a.to_gdal_string()).unwrap();
        assert_eq!((b.power, b.smoothing, b.radius1, b.radius2, b.angle, b.nodata), (3., 2., 4., 5., 6., -9.));
    }

    #[test]
    fn nearest_collinear_points_match_gdal() {
        let x: Vec<f64> = (0..150).map(|i| i as f64).collect();
        let y = vec![0.; 150];
        assert_eq!(grid(&x, &y, &x, &GridAlgorithm::new(GridMethod::Nearest), [0., -1., 150., 1.], 1, 1), vec![75.]);
    }

    #[test]
    fn quadrant_keyword_parameters_remap_before_radius_validation() {
        let params = GridParams { min_points_per_quadrant: Some(1), max_points: Some(5), ..Default::default() };
        let keyword = GridAlgorithm::with_params("invdist", &params).unwrap();
        let string = GridAlgorithm::parse("invdist:min_points_per_quadrant=1:max_points=5").unwrap();
        assert_eq!(keyword.to_gdal_string(), string.to_gdal_string());
        assert_eq!(keyword.method, GridMethod::InverseDistanceNearest);
        assert_eq!((keyword.radius, keyword.max_points), (1., 5));
    }

    #[test]
    fn linear_signed_zero_duplicates_keep_first_value_like_gdal() {
        let a = GridAlgorithm::parse("linear:radius=0").unwrap();
        assert_eq!(grid(&[-0., 0., 1., 0.], &[0., -0., 0., 1.], &[0., 100., 10., 20.], &a, [0., 0., 0.5, 0.5], 1, 1), vec![7.5]);
    }

    #[test]
    fn linear_collinear_points_have_no_triangulation() {
        let x: Vec<f64> = (0..150).map(|i| i as f64).collect();
        let y = vec![0.; 150];
        let a = GridAlgorithm::parse("linear:nodata=-9999").unwrap();
        assert_eq!(grid(&x, &y, &x, &a, [0., -1., 150., 1.], 1, 1), vec![-9999.]);
    }

    #[test]
    fn pair_counts_exceed_u32_without_overflow() {
        assert_eq!(distance_pair_count(100_000, false, false), 4_999_950_000);
        assert_eq!(distance_pair_count(100_000, true, false), 5_000_050_000);
        assert_eq!(distance_pair_count(100_000, true, true), 5_000_049_999);
    }

    #[test]
    fn extent_guards_preserve_negative_x_resolution() {
        let mut e = GridExtent { x_min: 0., x_max: 0., y_min: 0., y_max: 1., cols: 2, rows: 2 };
        assert!(check_extent(&e).is_err());
        e.x_min = 1.;
        assert!(check_extent(&e).is_ok());
        e.y_max = e.y_min;
        assert!(check_extent(&e).is_err());
    }

    #[test]
    fn tiny_linear_triangles_match_gdal_cutoff() {
        let mut a = GridAlgorithm::new(GridMethod::Linear);
        a.radius = 0.;
        a.nodata = -9.;
        assert_eq!(grid(&[0., 0.001, 0.], &[0., 0., 0.001], &[1., 2., 3.], &a, [0., 0., 0.001, 0.001], 2, 2), vec![-9.; 4]);
    }

    #[test]
    fn rejects_large_output_before_allocating() {
        assert!(check_size(100_000, 100_000, 1).is_err());
        let extent = GridExtent { x_min: 0., x_max: 1., y_min: 0., y_max: 1., cols: 100_000, rows: 100_000 };
        assert!(grid_points(&[0.], &[0.], &[1.], &GridAlgorithm::new(GridMethod::Nearest), &extent, DataType::F64, 1).is_err());
    }
    #[test]
    fn rejects_non_finite_parameters_and_xy_but_allows_nan_nodata() {
        for text in ["invdist:power=nan", "average:radius1=nan", "nearest:angle=inf", "invdist:smoothing=inf"] {
            assert!(GridAlgorithm::parse(text).is_err(), "{text}");
        }
        assert!(GridAlgorithm::parse("nearest:nodata=nan").is_ok());
        let extent = GridExtent { x_min: 0., x_max: 1., y_min: 0., y_max: 1., cols: 1, rows: 1 };
        assert!(grid_points(&[f64::NAN], &[0.], &[1.], &GridAlgorithm::new(GridMethod::Nearest), &extent, DataType::F64, 1).is_err());
    }
}
