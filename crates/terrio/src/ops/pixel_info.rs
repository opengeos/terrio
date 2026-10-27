//! Pixel values at positions, as GDAL's `gdal raster pixel-info` and `gdallocationinfo`: given as
//! column/line, as coordinates in the raster's CRS or in another CRS, or as the points (or
//! centroids) of a vector layer; read by nearest neighbour, or interpolated bilinearly, by cubic
//! convolution or by cubic B-spline as GDAL's `GDALRasterInterpolateAtPoint` does (pixel centers at
//! half-integer positions, nodata as GDAL treats it, kernels shifted inside the raster at edges).
//!
//! Only the 64 x 64 pixel blocks around the positions are read (the blocks GDAL caches), so
//! that querying a few positions of a large or remote raster reads a few blocks.
//!
//! The results print as GDAL prints them: GeoJSON (`gdal raster pixel-info`'s default), CSV
//! (GDAL 3.13's columns), `gdallocationinfo`'s text report and its `-valonly` values; or as a
//! vector layer of points.
//!
//! ```no_run
//! use terrio::ops::pixel_info::{PixelInfoOptions, Position, PositionCrs, pixel_info};
//!
//! let mut dem = terrio::open_raster_lazy("dem.tif")?;
//! let options = PixelInfoOptions { position_crs: Some(PositionCrs::parse("EPSG:4326")?), ..Default::default() };
//! let info = pixel_info(&mut dem, &[Position::new(-105.5, 39.5)], &options)?;
//! println!("{:?}", info.locations[0].bands[0].value);
//! print!("{}", info.to_text());
//! # Ok::<(), terrio::Error>(())
//! ```

use super::Resampling;
use crate::crs::{SpatialRef, Transformer};
use crate::{Error, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, LazyRaster, Result, Window, WindowOptions};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use wbraster::DataType;

/// What positions are expressed in.
#[derive(Debug, Clone, PartialEq)]
pub enum PositionCrs {
    /// Column and line, from the raster's top left corner (`pixel`, the default).
    Pixel,
    /// Coordinates in the raster's CRS (`dataset`).
    Dataset,
    /// Coordinates in this CRS, longitude/easting first.
    Crs(SpatialRef),
}

impl PositionCrs {
    /// Parses `pixel`, `dataset` or a CRS definition (`EPSG:4326`, `WGS84`, WKT...).
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "pixel" => Self::Pixel,
            "dataset" => Self::Dataset,
            _ => Self::Crs(SpatialRef::parse(s)?),
        })
    }
}

/// Which attribute fields of a vector layer of positions are copied to the output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum IncludeFields {
    /// Every field (`ALL`, the default).
    #[default]
    All,
    /// No field (`NONE`).
    None,
    /// These fields, by name.
    Only(Vec<String>),
}

impl IncludeFields {
    /// Parses GDAL's `--include-field` values: `ALL`, `NONE` or field names.
    pub fn parse(names: &[String]) -> Self {
        match names {
            [] => Self::All,
            [n] if n.eq_ignore_ascii_case("ALL") => Self::All,
            [n] if n.eq_ignore_ascii_case("NONE") => Self::None,
            names => Self::Only(names.to_vec()),
        }
    }
}

/// Options of [`pixel_info`] and [`pixel_info_layer`].
#[derive(Debug, Clone, Default)]
pub struct PixelInfoOptions {
    /// Bands to query, from 1. Empty means all bands.
    pub bands: Vec<usize>,
    /// Query this overview (from 0) rather than full resolution. Pixel positions are still those
    /// of the full-resolution raster.
    pub overview: Option<usize>,
    /// What positions are in. `None` uses the layer's CRS for [`pixel_info_layer`] when it
    /// has one, otherwise column/line, matching GDAL's default.
    pub position_crs: Option<PositionCrs>,
    /// `Nearest` (the default), `Bilinear`, `Cubic` or `CubicSpline`.
    pub resampling: Resampling,
    /// Put the (unscaled) value as the Z of GeoJSON point geometries, when one band is queried.
    pub promote_pixel_value_to_z: bool,
    /// The fields of a layer of positions copied to the output.
    pub include_fields: IncludeFields,
}

/// A position to query.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    /// Column, or X (longitude, easting).
    pub x: f64,
    /// Line, or Y (latitude, northing).
    pub y: f64,
    /// Text that followed the coordinates on the line it was read from, reported as is.
    pub extra: Option<String>,
}

impl Position {
    /// A position without extra text.
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y, extra: None }
    }

    /// Positions from text, as GDAL reads them from its standard input: an `X Y` pair per line,
    /// possibly followed by other text, split as GDAL's `CSLTokenizeString` splits (on spaces,
    /// double-quoted strings kept whole without their quotes). Reading stops at an empty line, as
    /// GDAL's does.
    pub fn parse_lines(text: &str) -> Result<Vec<Self>> {
        let mut out = Vec::new();
        for (n, line) in text.split_inclusive('\n').enumerate() {
            if line.starts_with('\n') || line.starts_with("\r\n") {
                break;
            }
            let tokens = tokenize(line);
            if tokens.len() < 2 {
                return Err(Error::InvalidArgument(format!("Not enough values at line {}", n + 1)));
            }
            let extra = tokens[2..].join(" ");
            let extra = extra.trim_end();
            out.push(Self { x: c_atof(&tokens[0]), y: c_atof(&tokens[1]), extra: (!extra.is_empty()).then(|| extra.to_string()) });
        }
        Ok(out)
    }
}

/// GDAL's `CSLTokenizeString`: tokens separated by spaces, double-quoted strings (with
/// backslash escapes) taken whole without their quotes, empty tokens dropped.
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        let mut token = String::new();
        let mut quoted = false;
        while let Some(c) = chars.next() {
            if !quoted && c == ' ' {
                break;
            }
            if c == '"' {
                quoted = !quoted;
                continue;
            }
            if quoted
                && c == '\\'
                && let Some(&next) = chars.peek()
            {
                token.push(next);
                chars.next();
                continue;
            }
            token.push(c);
        }
        if !token.is_empty() {
            tokens.push(token);
        }
        if chars.peek().is_none() {
            return tokens;
        }
    }
}

/// C's `atof`: the longest numeric prefix, 0 when there is none.
fn c_atof(s: &str) -> f64 {
    let s = s.trim_start();
    let bytes = s.as_bytes();
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    for special in ["infinity", "inf", "nan"] {
        if s.get(end..end + special.len()).is_some_and(|p| p.eq_ignore_ascii_case(special)) {
            return s[..end + special.len()].parse().unwrap_or(0.0);
        }
    }
    let start = end;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if bytes.get(end) == Some(&b'.') {
        end += 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
    }
    if end == start {
        return 0.0;
    }
    let exponent = end;
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        end += 1;
        if matches!(bytes.get(end), Some(b'+' | b'-')) {
            end += 1;
        }
        let digits = end;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == digits {
            end = exponent;
        }
    }
    s[..end].parse().unwrap_or(0.0)
}

/// The value of one band at a position.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BandValue {
    /// The band, from 1.
    pub band: usize,
    /// The pixel value, interpolated unless nearest; `None` outside the raster, at nodata (any
    /// nodata pixel of a bilinear kernel), or when every pixel of a cubic kernel is nodata.
    pub value: Option<f64>,
    /// The value with the band's scale and offset applied.
    pub unscaled: Option<f64>,
    /// The band's nodata value.
    #[serde(skip)]
    pub nodata: Option<f64>,
    /// The band's scale and offset are not 1 and 0.
    #[serde(skip)]
    pub scaled: bool,
    /// The band has an integer data type (GeoJSON writes integer raw values).
    #[serde(skip)]
    pub integer: bool,
}

/// The result at one position.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Location {
    /// The position as given.
    pub input: [f64; 2],
    /// Extra text of the position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
    /// The column, fractional, at full resolution.
    pub column: f64,
    /// The line, fractional, at full resolution.
    pub line: f64,
    /// The position in the raster's CRS (from its geotransform), when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xy: Option<[f64; 2]>,
    /// The attributes of the position's feature (for a layer of positions), in field order.
    #[serde(skip)]
    pub attributes: Vec<FieldValue>,
    /// The values of the bands queried.
    pub bands: Vec<BandValue>,
    /// The GeoJSON geometry: in the raster's CRS when it has an EPSG code, else in WGS 84.
    #[serde(skip)]
    pub geojson_xy: Option<[f64; 2]>,
    /// The output coordinates of CSV and vector outputs, in [`PixelInfo::output_crs`] (the input
    /// position when there is no output CRS).
    #[serde(skip)]
    pub output_xy: Option<[f64; 2]>,
}

impl Location {
    /// Whether the position is off the raster, as `gdallocationinfo` reckons it.
    pub fn is_off_raster(&self, width: usize, height: usize) -> bool {
        !self.column.is_finite()
            || !self.line.is_finite()
            || self.column.floor() < 0.0
            || self.line.floor() < 0.0
            || self.column > width as f64 + 1e-5
            || self.line > height as f64 + 1e-5
    }
}

/// The result of [`pixel_info`] or [`pixel_info_layer`].
#[derive(Debug, Clone)]
pub struct PixelInfo {
    /// One location per position (features without a geometry are skipped).
    pub locations: Vec<Location>,
    /// The bands queried.
    pub bands: Vec<usize>,
    /// The raster's full-resolution width and height.
    pub size: (usize, usize),
    /// The resampling method.
    pub resampling: Resampling,
    /// The fields of the layer of positions copied, with their indexes in the layer.
    pub fields: Vec<FieldDef>,
    /// Whether the positions came from a layer (whose fields replace `extra_content`).
    pub from_layer: bool,
    /// The layer's name, for vector outputs.
    pub layer_name: Option<String>,
    /// The CRS of CSV and vector outputs: the position CRS when one was given, else the
    /// layer's (or the raster's) for a layer of positions; `None` otherwise.
    pub output_crs: Option<SpatialRef>,
    /// The GeoJSON `crs` member's name: `urn:ogc:def:crs:EPSG::<code>`, or
    /// `urn:ogc:def:crs:OGC:1.3:CRS84`; `None` when GeoJSON geometries are null.
    pub geojson_crs: Option<String>,
    /// Whether GeoJSON geometries get the value as Z.
    pub promote_to_z: bool,
}

/// GDAL's `ARE_REAL_EQUAL`, as nodata is matched.
fn are_real_equal(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() < f64::from(f32::EPSILON) * (a + b).abs() * 2.0
}

/// GDAL's `GDALInvGeoTransform`.
pub(crate) fn invert_geo_transform(gt: &[f64; 6]) -> Option<[f64; 6]> {
    if gt[2] == 0.0 && gt[4] == 0.0 && gt[1] != 0.0 && gt[5] != 0.0 {
        return Some([-gt[0] / gt[1], 1.0 / gt[1], 0.0, -gt[3] / gt[5], 0.0, 1.0 / gt[5]]);
    }
    let det = gt[1] * gt[5] - gt[2] * gt[4];
    let magnitude = gt[1].abs().max(gt[2].abs()).max(gt[4].abs().max(gt[5].abs()));
    if det.abs() <= 1e-10 * magnitude * magnitude {
        return None;
    }
    let inv = 1.0 / det;
    Some([
        (gt[2] * gt[3] - gt[0] * gt[5]) * inv,
        gt[5] * inv,
        -gt[2] * inv,
        (-gt[1] * gt[3] + gt[0] * gt[4]) * inv,
        -gt[4] * inv,
        gt[1] * inv,
    ])
}

fn apply(gt: &[f64; 6], x: f64, y: f64) -> (f64, f64) {
    (gt[0] + x * gt[1] + y * gt[2], gt[3] + x * gt[4] + y * gt[5])
}

/// GDAL's cubic convolution kernel (Keys, a = -0.5).
fn cubic_kernel(x: f64) -> f64 {
    let a = x.abs();
    if a <= 1.0 {
        x * x * (1.5 * a - 2.5) + 1.0
    } else if a <= 2.0 {
        x * x * (-0.5 * a + 2.5) - 4.0 * a + 2.0
    } else {
        0.0
    }
}

/// GDAL's cubic B-spline kernel.
fn cubic_spline_kernel(v: f64) -> f64 {
    if v > 2.0 {
        return 0.0;
    }
    let cube = |t: f64| if t <= 0.0 { 0.0 } else { t * t * t };
    (1.0 / 6.0) * (cube(v + 2.0) - 4.0 * cube(v + 1.0) + 6.0 * cube(v) - 4.0 * cube(v - 1.0))
}

/// The edge length of the blocks read and cached, as GDAL's.
const BLOCK: usize = 64;
/// Cache budget across every queried band; at least one block can be read.
const MAX_CACHE_BYTES: usize = 128 << 20;

fn max_cached_blocks(bands: usize) -> usize {
    (MAX_CACHE_BYTES / BLOCK.saturating_mul(BLOCK).saturating_mul(bands).saturating_mul(8).max(1)).max(1)
}

/// Reads pixel windows of some bands of one level, by cached 64 x 64 blocks.
struct Blocks<'a> {
    raster: &'a mut LazyRaster,
    bands: Vec<usize>,
    overview: Option<usize>,
    width: usize,
    height: usize,
    cache: HashMap<(usize, usize), (usize, Vec<f64>)>,
}

impl Blocks<'_> {
    /// The values of band `b` (an index into `bands`) in an `n` x `n` window from `(x, y)`, which
    /// lies within the level.
    fn window(&mut self, b: usize, x: usize, y: usize, n: usize) -> Result<Vec<f64>> {
        let mut out = vec![0.0; n * n];
        for j in 0..n {
            for i in 0..n {
                let (c, r) = (x + i, y + j);
                let key = (c / BLOCK, r / BLOCK);
                if !self.cache.contains_key(&key) {
                    if self.cache.len() >= max_cached_blocks(self.bands.len()) {
                        self.cache.clear();
                    }
                    let (bx, by) = (key.0 * BLOCK, key.1 * BLOCK);
                    let (w, h) = (BLOCK.min(self.width - bx), BLOCK.min(self.height - by));
                    let options = WindowOptions { bands: self.bands.clone(), overview: self.overview };
                    let data = self.raster.read_window(Window::new(bx, by, w, h), &options)?.raster.data.to_f64_vec();
                    self.cache.insert(key, (w, data));
                }
                let (w, data) = &self.cache[&key];
                let h = data.len() / (w * self.bands.len()).max(1);
                out[j * n + i] = data[b * w * h + (r % BLOCK) * w + c % BLOCK];
            }
        }
        Ok(out)
    }
}

/// GDAL's `dragReadDataInBorder`: shifts a kernel read inside the raster back over its edges.
fn drag(values: &mut [f64], out_x: i64, out_y: i64, n: usize) {
    let shift = |values: &mut [f64], mut out: i64, along_x: bool| {
        while out < 0 {
            for j in 0..n {
                for ii in 0..n - 1 {
                    let i = n - ii - 2;
                    let (src, dst) = if along_x { (n * j + i, n * j + i + 1) } else { (n * i + j, n * (i + 1) + j) };
                    values[dst] = values[src];
                }
            }
            out += 1;
        }
        while out > 0 {
            for j in 0..n {
                for i in 0..n - 1 {
                    let (src, dst) = if along_x { (n * j + i + 1, n * j + i) } else { (n * (i + 1) + j, n * i + j) };
                    values[dst] = values[src];
                }
            }
            out -= 1;
        }
    };
    shift(values, out_x, true);
    shift(values, out_y, false);
}

/// GDAL's `GDALInterpolateAtPointImpl` for one band of the level `blocks` reads.
fn interpolate(blocks: &mut Blocks<'_>, b: usize, nodata: Option<f64>, x: f64, y: f64, method: Resampling) -> Result<Option<f64>> {
    let (w, h) = (blocks.width, blocks.height);
    let (mut x, mut y) = (x, y);
    if method == Resampling::Nearest {
        if x >= w as f64 && x <= w as f64 + 1e-5 {
            x -= 0.25;
        }
        if y >= h as f64 && y <= h as f64 + 1e-5 {
            y -= 0.25;
        }
    }
    if !(x >= 0.0 && x <= w as f64 && y >= 0.0 && y <= h as f64) {
        return Ok(None);
    }
    let mut method = method;
    if (w < 4 || h < 4) && matches!(method, Resampling::Cubic | Resampling::CubicSpline) {
        method = Resampling::Bilinear;
    }
    if (w < 2 || h < 2) && method == Resampling::Bilinear {
        method = Resampling::Nearest;
    }
    let is_nodata = |v: f64| nodata.is_some_and(|nd| are_real_equal(nd, v));
    let out_of_border = |d: i64, size: usize, k: usize| -> i64 {
        let mut o = 0;
        if d < 0 {
            o = d;
        }
        if d + k as i64 >= size as i64 {
            o = d + k as i64 - size as i64;
        }
        o
    };
    match method {
        Resampling::Bilinear | Resampling::Cubic | Resampling::CubicSpline => {
            let k = if method == Resampling::Bilinear { 2 } else { 4 };
            let (fx, fy) = (x - 0.5, y - 0.5);
            let (dx, dy) = (fx.floor() as i64, fy.floor() as i64);
            let (tx, ty) = (fx - dx as f64, fy - dy as f64);
            let (nx, ny) = if k == 2 { (dx, dy) } else { (dx - 1, dy - 1) };
            let (ox, oy) = (out_of_border(nx, w, k), out_of_border(ny, h, k));
            let (sx, sy) = (nx - ox, ny - oy);
            if sx < 0 || sy < 0 || sx as usize + k > w || sy as usize + k > h {
                return Ok(None);
            }
            let mut v = blocks.window(b, sx as usize, sy as usize, k)?;
            drag(&mut v, ox, oy, k);
            if k == 2 {
                if v.iter().any(|&p| is_nodata(p)) {
                    return Ok(None);
                }
                let xz1 = v[0] * (1.0 - tx) + v[1] * tx;
                let xz2 = v[2] * (1.0 - tx) + v[3] * tx;
                return Ok(Some(xz1 * (1.0 - ty) + xz2 * ty));
            }
            let (mut sum, mut weights) = (0.0, 0.0);
            for ki in 0..4 {
                for kj in 0..4 {
                    let (px, py) = ((kj as i64 - 1) as f64 - tx, (ki as i64 - 1) as f64 - ty);
                    let weight = if method == Resampling::CubicSpline {
                        cubic_spline_kernel(px) * cubic_spline_kernel(py)
                    } else {
                        cubic_kernel(px) * cubic_kernel(py)
                    };
                    let p = v[kj + ki * 4];
                    if is_nodata(p) {
                        continue;
                    }
                    sum += p * weight;
                    weights += weight;
                }
            }
            Ok((weights != 0.0).then(|| sum / weights))
        }
        _ => {
            let (c, r) = (x as usize, y as usize);
            if c >= w || r >= h {
                return Ok(None);
            }
            let v = blocks.window(b, c, r, 1)?[0];
            Ok((!is_nodata(v)).then_some(v))
        }
    }
}

fn check_options(r: &LazyRaster, options: &PixelInfoOptions) -> Result<Vec<usize>> {
    if !matches!(options.resampling, Resampling::Nearest | Resampling::Bilinear | Resampling::Cubic | Resampling::CubicSpline) {
        return Err(Error::InvalidArgument("only nearest, bilinear, cubic and cubicspline interpolation methods are allowed".into()));
    }
    if r.band_count() == 0 {
        return Err(Error::InvalidArgument("Source dataset has no raster band.".into()));
    }
    if r.dataset().is_some_and(|d| d.complex) {
        return Err(Error::Unsupported("querying complex rasters is not supported".into()));
    }
    if let Some(o) = options.overview {
        let n = r.overviews().len();
        if n == 0 {
            return Err(Error::InvalidArgument("Source dataset has no overviews. Argument 'overview' must not be specified.".into()));
        }
        if o >= n {
            return Err(Error::InvalidArgument(format!(
                "Source dataset has only {n} overview level{}. 'overview' value must be strictly lower than this number.",
                if n > 1 { "s" } else { "" }
            )));
        }
    }
    let bands: Vec<usize> = if options.bands.is_empty() { (1..=r.band_count()).collect() } else { options.bands.clone() };
    if let Some(b) = bands.iter().find(|b| **b == 0 || **b > r.band_count()) {
        return Err(Error::InvalidArgument(format!("band {b} does not exist (the raster has {})", r.band_count())));
    }
    Ok(bands)
}

/// Queries the bands of `raster` at `positions`.
pub fn pixel_info(raster: &mut LazyRaster, positions: &[Position], options: &PixelInfoOptions) -> Result<PixelInfo> {
    let crs = options.position_crs.clone().unwrap_or(PositionCrs::Pixel);
    let output_crs = match &crs {
        PositionCrs::Crs(s) => Some(s.clone()),
        _ => None,
    };
    let points: Vec<(Position, Vec<FieldValue>)> = positions.iter().map(|p| (p.clone(), Vec::new())).collect();
    query(raster, &points, &crs, output_crs, Vec::new(), false, None, options)
}

/// Queries the bands of `raster` at the points of a layer (the centroids of other geometries),
/// copying the selected fields. Features with no geometry or an empty one are skipped.
pub fn pixel_info_layer(raster: &mut LazyRaster, layer: &Layer, options: &PixelInfoOptions) -> Result<PixelInfo> {
    let layer_crs = SpatialRef::from_layer(layer);
    let crs = match &options.position_crs {
        Some(c) => c.clone(),
        None if !layer_crs.is_unknown() => PositionCrs::Crs(layer_crs.clone()),
        None => PositionCrs::Pixel,
    };
    let output_crs = match &crs {
        PositionCrs::Crs(s) => Some(s.clone()),
        _ if !layer_crs.is_unknown() => Some(layer_crs),
        _ => Some(raster.crs()).filter(|c| !c.is_unknown()),
    };
    let fields = layer.schema.fields();
    let indices: Vec<usize> = match &options.include_fields {
        IncludeFields::All => (0..fields.len()).collect(),
        IncludeFields::None => Vec::new(),
        IncludeFields::Only(names) => names
            .iter()
            .map(|n| {
                layer
                    .schema
                    .field_index(n)
                    .ok_or_else(|| Error::InvalidArgument(format!("Field '{n}' does not exist in layer '{}'", layer.name)))
            })
            .collect::<Result<_>>()?,
    };
    let centroids = super::geometry::centroids(layer)?;
    let mut points = Vec::new();
    for (f, c) in layer.features.iter().zip(&centroids.features) {
        let Some(Geometry::Point(p)) = &c.geometry else { continue };
        let attributes = indices.iter().map(|&i| f.attributes.get(i).cloned().unwrap_or(FieldValue::Null)).collect();
        points.push((Position::new(p.x, p.y), attributes));
    }
    let defs = indices.iter().map(|&i| fields[i].clone()).collect();
    query(raster, &points, &crs, output_crs, defs, true, Some(layer.name.clone()), options)
}

#[allow(clippy::too_many_arguments)]
fn query(
    raster: &mut LazyRaster,
    points: &[(Position, Vec<FieldValue>)],
    crs: &PositionCrs,
    output_crs: Option<SpatialRef>,
    fields: Vec<FieldDef>,
    from_layer: bool,
    layer_name: Option<String>,
    options: &PixelInfoOptions,
) -> Result<PixelInfo> {
    let bands = check_options(raster, options)?;
    let src_crs = raster.crs();
    let gt = raster.geo_transform();
    let has_gt = gt != [0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    let mut inv = None;
    if *crs != PositionCrs::Pixel {
        if src_crs.is_unknown() {
            return Err(Error::InvalidArgument("Dataset has no CRS. Only 'position-crs' = 'pixel' is supported.".into()));
        }
        if !has_gt {
            return Err(Error::InvalidArgument("Cannot get geotransform".into()));
        }
        inv = Some(invert_geo_transform(&gt).ok_or_else(|| Error::InvalidArgument("Cannot invert geotransform".into()))?);
    }
    let to_raster = match crs {
        PositionCrs::Crs(s) => Some(Transformer::new(s, &src_crs)?),
        _ => None,
    };
    let to_output = match &output_crs {
        Some(o) if !src_crs.is_unknown() && has_gt => Some(Transformer::new(&src_crs, o)?),
        _ => None,
    };
    // GeoJSON geometries: in the raster's CRS when it has an EPSG code, else in WGS 84.
    let (geojson_crs, to_wgs84) = match (src_crs.is_unknown() || !has_gt, src_crs.authority()) {
        (true, _) => (None, None),
        (false, Some(("EPSG", 4326))) => (Some("urn:ogc:def:crs:OGC:1.3:CRS84".to_string()), None),
        (false, Some(("EPSG", code))) => (Some(format!("urn:ogc:def:crs:EPSG::{code}")), None),
        (false, _) => match Transformer::new(&src_crs, &SpatialRef::from_epsg(4326)?) {
            Ok(t) => (Some("urn:ogc:def:crs:OGC:1.3:CRS84".to_string()), Some(t)),
            Err(_) => (None, None),
        },
    };
    let header = raster.header();
    let props: Vec<(Option<f64>, f64, f64, bool)> = bands
        .iter()
        .map(|&b| {
            let m = header.band_metadata.get(b - 1);
            (
                raster.band_nodata(b),
                m.and_then(|m| m.scale).unwrap_or(1.0),
                m.and_then(|m| m.offset).unwrap_or(0.0),
                !matches!(raster.band_data_type(b), DataType::F32 | DataType::F64),
            )
        })
        .collect();
    let (full_w, full_h) = (raster.width(), raster.height());
    let (lw, lh) = match options.overview {
        Some(o) => raster.overviews()[o],
        None => (full_w, full_h),
    };
    let mut blocks = Blocks { raster, bands: bands.clone(), overview: options.overview, width: lw, height: lh, cache: HashMap::new() };
    let mut locations = Vec::with_capacity(points.len());
    for (p, attributes) in points {
        let (mut x, mut y) = (p.x, p.y);
        if let Some(t) = &to_raster {
            // Input-transform errors are fatal, as GDAL 3.13.3: there is no target
            // pixel to query. Optional output-label transforms may instead be omitted.
            (x, y) = t.transform(x, y).map_err(|e| Error::Crs(format!("cannot transform ({}, {}): {e}", p.x, p.y)))?;
        }
        let (column, line) = match &inv {
            Some(inv) => apply(inv, x, y),
            None => (x, y),
        };
        let (qx, qy) = if options.overview.is_some() {
            (column / full_w as f64 * lw as f64, line / full_h as f64 * lh as f64)
        } else {
            (column, line)
        };
        let mut values = Vec::with_capacity(bands.len());
        for (i, &band) in bands.iter().enumerate() {
            let (nodata, scale, offset, integer) = props[i];
            let value = if column.is_finite() && line.is_finite() {
                interpolate(&mut blocks, i, nodata, qx, qy, options.resampling)?
            } else {
                None
            };
            values.push(BandValue {
                band,
                value,
                unscaled: value.map(|v| v * scale + offset),
                nodata,
                scaled: scale != 1.0 || offset != 0.0,
                integer,
            });
        }
        let xy = has_gt.then(|| apply(&gt, column, line)).map(|(x, y)| [x, y]);
        let geojson_xy = match (&geojson_crs, xy) {
            (Some(_), Some([x, y])) => match &to_wgs84 {
                Some(t) => t.transform(x, y).ok().map(|(x, y)| [x, y]),
                None => Some([x, y]),
            },
            _ => None,
        };
        let output_xy = match (&output_crs, &to_output, xy) {
            (None, _, _) => Some([p.x, p.y]),
            (Some(_), Some(t), Some([x, y])) => t.transform(x, y).ok().map(|(x, y)| [x, y]),
            _ => None,
        };
        locations.push(Location {
            input: [p.x, p.y],
            extra: p.extra.clone(),
            column,
            line,
            xy,
            attributes: attributes.clone(),
            bands: values,
            geojson_xy,
            output_xy,
        });
    }
    Ok(PixelInfo {
        locations,
        bands,
        size: (full_w, full_h),
        resampling: options.resampling,
        fields,
        from_layer,
        layer_name,
        output_crs,
        geojson_crs,
        promote_to_z: options.promote_pixel_value_to_z,
    })
}

/// C's `printf("%.15g")`.
fn g15(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    crate::drivers::common::fmt_g(v, 15)
}

/// Stand-ins for the non-finite numbers JSON cannot hold, replaced by GDAL's `NaN`, `Infinity`
/// and `-Infinity` tokens in [`PixelInfo::to_geojson_string`].
const NON_FINITE: [&str; 3] = ["NaN", "Infinity", "-Infinity"];

fn json_number(v: f64, marker: &str) -> Value {
    match serde_json::Number::from_f64(v) {
        Some(n) => Value::Number(n),
        None if marker.is_empty() => Value::Null,
        None if v.is_nan() => Value::String(format!("{marker}NaN")),
        None if v > 0.0 => Value::String(format!("{marker}Infinity")),
        None => Value::String(format!("{marker}-Infinity")),
    }
}

fn field_json(v: &FieldValue, marker: &str) -> Value {
    match v {
        FieldValue::Integer(i) => json!(i),
        FieldValue::Float(f) => json_number(*f, marker),
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => json!(s),
        FieldValue::Boolean(b) => json!(b),
        FieldValue::Blob(b) => json!(b.iter().map(|x| format!("{x:02X}")).collect::<String>()),
        FieldValue::Null => Value::Null,
    }
}

fn field_text(v: &FieldValue) -> String {
    match v {
        FieldValue::Integer(i) => i.to_string(),
        FieldValue::Float(f) => g15(*f),
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.clone(),
        FieldValue::Boolean(b) => (*b as u8).to_string(),
        FieldValue::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
        FieldValue::Null => String::new(),
    }
}

/// A CSV string field, as OGR's CSV driver quotes them: when it holds a comma, a quote or a line
/// break, or reads as a number.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) || (!s.trim().is_empty() && s.trim().parse::<f64>().is_ok()) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

impl PixelInfo {
    /// The number of locations off the raster (`gdallocationinfo` then exits with 1).
    pub fn off_raster_count(&self) -> usize {
        self.locations.iter().filter(|l| l.is_off_raster(self.size.0, self.size.1)).count()
    }

    /// A GeoJSON feature collection with non-finite numeric values represented as null.
    /// Use [`Self::to_geojson_string`] for GDAL's non-standard NaN/Infinity tokens.
    pub fn to_geojson(&self) -> Value {
        self.geojson_with_marker("")
    }

    fn geojson_with_marker(&self, marker: &str) -> Value {
        let json_number = |v| json_number(v, marker);
        let mut collection = Map::new();
        collection.insert("type".into(), json!("FeatureCollection"));
        if let Some(name) = &self.geojson_crs {
            collection.insert("crs".into(), json!({"type": "name", "properties": {"name": name}}));
        }
        let features: Vec<Value> = self
            .locations
            .iter()
            .map(|l| {
                let mut props = Map::new();
                props.insert("input_coordinate".into(), json!([json_number(l.input[0]), json_number(l.input[1])]));
                if let Some(e) = &l.extra {
                    props.insert("extra_content".into(), json!(e));
                }
                props.insert("column".into(), json_number(l.column));
                props.insert("line".into(), json_number(l.line));
                for (def, v) in self.fields.iter().zip(&l.attributes) {
                    props.insert(def.name.clone(), field_json(v, marker));
                }
                let bands: Vec<Value> = l
                    .bands
                    .iter()
                    .map(|b| {
                        let mut o = Map::new();
                        o.insert("band_number".into(), json!(b.band));
                        if let (Some(v), Some(u)) = (b.value, b.unscaled) {
                            o.insert("raw_value".into(), if b.integer { json!(v as i64) } else { json_number(v) });
                            o.insert("unscaled_value".into(), json_number(u));
                        }
                        Value::Object(o)
                    })
                    .collect();
                props.insert("bands".into(), Value::Array(bands));
                let geometry = match l.geojson_xy {
                    Some([x, y]) => {
                        let mut coords = vec![json_number(x), json_number(y)];
                        if self.promote_to_z
                            && l.bands.len() == 1
                            && let Some(z) = l.bands[0].unscaled
                        {
                            coords.push(json_number(z));
                        }
                        json!({"type": "Point", "coordinates": coords})
                    }
                    None => Value::Null,
                };
                json!({"type": "Feature", "properties": Value::Object(props), "geometry": geometry})
            })
            .collect();
        collection.insert("features".into(), Value::Array(features));
        Value::Object(collection)
    }

    /// GeoJSON as text, pretty-printed, with NaN and infinite values written as GDAL's
    /// non-standard `NaN`, `Infinity` and `-Infinity` tokens.
    pub fn to_geojson_string(&self) -> String {
        let (value, marker) = self.to_geojson_marked();
        let mut text = serde_json::to_string_pretty(&value).expect("JSON values serialize");
        let encoded = serde_json::to_string(&marker).expect("strings serialize");
        if text.contains(&encoded[..encoded.len() - 1]) {
            for token in NON_FINITE {
                let stand_in = serde_json::to_string(&format!("{marker}{token}")).expect("strings serialize");
                text = text.replace(&stand_in, token);
            }
        }
        text
    }

    /// GeoJSON with a collision-free string marker for nonfinite numeric values.
    /// Consumers may decode only strings beginning with the returned marker.
    pub fn to_geojson_marked(&self) -> (Value, String) {
        // Inspect user strings directly, without first building/serializing the collection.
        let mut strings: Vec<&str> = self.fields.iter().map(|field| field.name.as_str()).collect();
        strings.extend(self.geojson_crs.iter().map(String::as_str));
        for location in &self.locations {
            strings.extend(location.extra.iter().map(String::as_str));
            for attribute in &location.attributes {
                if let FieldValue::Text(value) | FieldValue::Date(value) | FieldValue::DateTime(value) = attribute {
                    strings.push(value.as_str());
                }
            }
        }
        let mut counter = 0usize;
        let marker = loop {
            let candidate = format!("\0terrio-nonfinite-{counter}:");
            if strings.iter().all(|value| !value.contains(&candidate)) {
                break candidate;
            }
            counter += 1;
        };
        (self.geojson_with_marker(&marker), marker)
    }

    /// The CSV GDAL 3.13 writes: `geom_x,geom_y`, the layer's fields (or `extra_content`),
    /// `column,line` and `band_N_raw_value,band_N_unscaled_value` per band; numbers as `%.15g`.
    pub fn to_csv(&self) -> String {
        let mut header = vec!["geom_x".to_string(), "geom_y".to_string()];
        if self.from_layer {
            header.extend(self.fields.iter().map(|f| csv_field(&f.name)));
        } else {
            header.push("extra_content".into());
        }
        header.extend(["column".to_string(), "line".to_string()]);
        for b in &self.bands {
            header.push(format!("band_{b}_raw_value"));
            header.push(format!("band_{b}_unscaled_value"));
        }
        let mut s = header.join(",");
        s.push('\n');
        for l in &self.locations {
            let mut row = match l.output_xy {
                Some([x, y]) => vec![g15(x), g15(y)],
                None => vec![String::new(), String::new()],
            };
            if self.from_layer {
                row.extend(l.attributes.iter().map(|v| match v {
                    FieldValue::Text(t) | FieldValue::Date(t) | FieldValue::DateTime(t) => csv_field(t),
                    v => field_text(v),
                }));
            } else {
                row.push(l.extra.as_deref().map(csv_field).unwrap_or_default());
            }
            row.push(g15(l.column));
            row.push(g15(l.line));
            for b in &l.bands {
                row.push(b.value.map(g15).unwrap_or_default());
                row.push(b.unscaled.map(g15).unwrap_or_default());
            }
            s.push_str(&row.join(","));
            s.push('\n');
        }
        s
    }

    /// `gdallocationinfo`'s report: per location, its column and line, then each band's value
    /// (and descaled value when the band has a scale or offset), or a note that it is off the
    /// raster.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        let nearest = self.resampling == Resampling::Nearest;
        for l in &self.locations {
            s.push_str("Report:\n");
            let (p, ln) = if nearest && l.column.is_finite() && l.line.is_finite() {
                ((l.column.floor() as i64).to_string(), (l.line.floor() as i64).to_string())
            } else {
                (g15(l.column), g15(l.line))
            };
            let _ = writeln!(s, "  Location: ({p}P,{ln}L)");
            if let Some(e) = &l.extra {
                let _ = writeln!(s, "  Extra input: {e}");
            }
            if l.is_off_raster(self.size.0, self.size.1) {
                s.push_str("\nLocation is off this file! No further details to report.\n");
                continue;
            }
            for b in &l.bands {
                let _ = writeln!(s, "  Band {}:", b.band);
                match (b.value, b.nodata) {
                    (Some(v), _) => {
                        let _ = writeln!(s, "    Value: {}", g15(v));
                        if b.scaled
                            && let Some(u) = b.unscaled
                        {
                            let _ = writeln!(s, "    Descaled Value: {}", g15(u));
                        }
                    }
                    (None, Some(nd)) => {
                        let _ = writeln!(s, "    Value: {}", g15(nd));
                    }
                    (None, None) => {}
                }
            }
        }
        s
    }

    /// `gdallocationinfo -valonly`: a line per location with its band values, separated by
    /// `separator` (a line break by default, as GDAL's); empty values off the raster.
    pub fn to_values(&self, separator: &str) -> String {
        let mut s = String::new();
        for l in &self.locations {
            let off = l.is_off_raster(self.size.0, self.size.1);
            let values: Vec<String> =
                l.bands.iter().map(|b| if off { String::new() } else { b.value.or(b.nodata).map(g15).unwrap_or_default() }).collect();
            s.push_str(&values.join(separator));
            if let Some(e) = &l.extra
                && separator != "\n"
            {
                s.push_str(separator);
                s.push_str(e);
            }
            s.push('\n');
        }
        s
    }

    /// The locations as a layer of points (GDAL 3.13's vector output): the layer's fields or
    /// `extra_content`, `column`, `line` and `band_N_raw_value`, `band_N_unscaled_value`, with
    /// points at [`Location::output_xy`] in [`PixelInfo::output_crs`].
    /// Returns an error when a copied field collides with a computed output column.
    pub fn to_layer(&self, name: &str) -> Result<Layer> {
        let mut names = std::collections::BTreeSet::new();
        for name in ["column".to_string(), "line".to_string()]
            .into_iter()
            .chain(self.bands.iter().flat_map(|b| [format!("band_{b}_raw_value"), format!("band_{b}_unscaled_value")]))
        {
            names.insert(name.to_ascii_lowercase());
        }
        if self.from_layer {
            for field in &self.fields {
                if !names.insert(field.name.to_ascii_lowercase()) {
                    return Err(Error::InvalidArgument(format!(
                        "position field '{}' collides with a vector output field; select fewer fields or rename it",
                        field.name
                    )));
                }
            }
        }

        let mut layer = Layer::new(name).with_geom_type(GeometryType::Point);
        if let Some(c) = &self.output_crs {
            c.apply_to_layer(&mut layer);
        }
        if self.from_layer {
            for f in &self.fields {
                layer.add_field(f.clone());
            }
        } else {
            layer.add_field(FieldDef::new("extra_content", FieldType::Text));
        }
        layer.add_field(FieldDef::new("column", FieldType::Float));
        layer.add_field(FieldDef::new("line", FieldType::Float));
        for b in &self.bands {
            layer.add_field(FieldDef::new(format!("band_{b}_raw_value"), FieldType::Float));
            layer.add_field(FieldDef::new(format!("band_{b}_unscaled_value"), FieldType::Float));
        }
        let float = |v: Option<f64>| v.map_or(FieldValue::Null, FieldValue::Float);
        for (i, l) in self.locations.iter().enumerate() {
            let mut attributes =
                if self.from_layer { l.attributes.clone() } else { vec![l.extra.clone().map_or(FieldValue::Null, FieldValue::Text)] };
            attributes.push(FieldValue::Float(l.column));
            attributes.push(FieldValue::Float(l.line));
            for b in &l.bands {
                attributes.push(float(b.value));
                attributes.push(float(b.unscaled));
            }
            let geometry = l.output_xy.map(|[x, y]| Geometry::Point(wbvector::Coord::xy(x, y)));
            layer.features.push(Feature { fid: i as u64 + 1, geometry, attributes });
        }
        Ok(layer)
    }

    /// The locations as JSON: an array of `{input, extra, column, line, xy, bands: [{band,
    /// value, unscaled}]}`.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(&self.locations).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RasterDataset;
    use wbraster::{Raster, RasterConfig};

    fn raster(cols: usize, rows: usize, nodata: Option<f64>) -> LazyRaster {
        let cfg = RasterConfig {
            cols,
            rows,
            bands: 1,
            x_min: 100.0,
            y_min: 200.0,
            cell_size: 10.0,
            nodata: nodata.unwrap_or(-9999.0),
            data_type: DataType::F32,
            crs: SpatialRef::from_epsg(32611).unwrap().to_crs_info(),
            ..Default::default()
        };
        let values = (0..cols * rows).map(|i| i as f64).collect();
        LazyRaster::from(RasterDataset::new(Raster::from_data(cfg, values).unwrap(), nodata.is_some()))
    }

    fn value(r: &mut LazyRaster, x: f64, y: f64, method: Resampling) -> Option<f64> {
        let o = PixelInfoOptions { resampling: method, ..Default::default() };
        pixel_info(r, &[Position::new(x, y)], &o).unwrap().locations[0].bands[0].value
    }

    #[test]
    fn layer_without_crs_uses_pixels_and_reserved_geojson_names_match_gdal() {
        let mut raster = raster(5, 4, None);
        let mut layer = Layer::new("sites").with_geom_type(GeometryType::Point);
        layer.add_field(FieldDef::new("line", FieldType::Text));
        layer.features.push(Feature {
            fid: 0,
            geometry: Some(Geometry::Point(wbvector::Coord::xy(2.5, 1.5))),
            attributes: vec![FieldValue::Text("survey".into())],
        });
        let info = pixel_info_layer(&mut raster, &layer, &Default::default()).unwrap();
        assert_eq!(info.locations[0].bands[0].value, Some(7.0));
        assert_eq!(info.to_geojson()["features"][0]["properties"]["line"], "survey");
        assert!(info.to_layer("samples").unwrap_err().to_string().contains("collides"));
    }

    #[test]
    fn interpolated_integer_geojson_truncates_raw_but_keeps_unscaled() {
        let cfg = RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::I32, ..Default::default() };
        let mut raster = LazyRaster::from(RasterDataset::new(Raster::from_data(cfg, vec![1., 2., 3., 4.]).unwrap(), false));
        let info =
            pixel_info(&mut raster, &[Position::new(1., 1.)], &PixelInfoOptions { resampling: Resampling::Bilinear, ..Default::default() })
                .unwrap();
        let json = info.to_geojson();
        assert_eq!(json["features"][0]["properties"]["bands"][0]["raw_value"], 2);
        assert_eq!(json["features"][0]["properties"]["bands"][0]["unscaled_value"], 2.5);
    }

    #[test]
    fn nearest_reads_the_pixel_containing_the_position() {
        let mut r = raster(5, 4, None);
        assert_eq!(value(&mut r, 2.7, 1.2, Resampling::Nearest), Some(7.0));
        // GDAL lets the right and bottom edges in.
        assert_eq!(value(&mut r, 5.0, 4.0, Resampling::Nearest), Some(19.0));
        assert_eq!(value(&mut r, 5.1, 1.0, Resampling::Nearest), None);
        assert_eq!(value(&mut r, -0.1, 1.0, Resampling::Nearest), None);
    }

    #[test]
    fn bilinear_interpolates_between_centres() {
        let mut r = raster(5, 4, None);
        // The corner of pixels 6, 7, 11 and 12: their mean.
        assert_eq!(value(&mut r, 2.0, 2.0, Resampling::Bilinear), Some(9.0));
        // At a pixel centre: the pixel.
        assert_eq!(value(&mut r, 1.5, 1.5, Resampling::Cubic), Some(6.0));
        // Edges shift the kernel inside.
        assert!(value(&mut r, 0.1, 0.1, Resampling::CubicSpline).is_some());
    }

    #[test]
    fn nodata_fails_bilinear_and_is_skipped_by_cubic() {
        let mut r = raster(5, 4, Some(7.0));
        assert_eq!(value(&mut r, 2.5, 1.5, Resampling::Nearest), None);
        assert_eq!(value(&mut r, 2.0, 2.0, Resampling::Bilinear), None);
        assert!(value(&mut r, 2.0, 2.0, Resampling::Cubic).is_some());
    }

    #[test]
    fn dataset_coordinates_and_reports() {
        let mut r = raster(5, 4, None);
        let o = PixelInfoOptions { position_crs: Some(PositionCrs::Dataset), ..Default::default() };
        let info = pixel_info(&mut r, &[Position::new(125.0, 225.0), Position::new(0.0, 0.0)], &o).unwrap();
        assert_eq!(info.locations[0].column, 2.5);
        assert_eq!(info.locations[0].line, 1.5);
        assert_eq!(
            info.to_text(),
            "Report:\n  Location: (2P,1L)\n  Band 1:\n    Value: 7\nReport:\n  Location: (-10P,24L)\n\nLocation is off this file! No further details to report.\n"
        );
        assert_eq!(info.off_raster_count(), 1);
        let g = info.to_geojson();
        assert_eq!(g["crs"]["properties"]["name"], "urn:ogc:def:crs:EPSG::32611");
        assert_eq!(g["features"][0]["geometry"]["coordinates"], json!([125.0, 225.0]));
        assert_eq!(g["features"][0]["properties"]["bands"][0]["raw_value"], json!(7.0));
        assert!(g["features"][1]["properties"]["bands"][0].get("raw_value").is_none());
        assert_eq!(info.to_csv().lines().nth(1).unwrap(), "125,225,,2.5,1.5,7,7");
        assert_eq!(info.to_values("\n"), "7\n\n");
    }

    #[test]
    fn nan_nodata_queries_keep_nan_like_gdal_313() {
        let config = crate::RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::F32, nodata: f64::NAN, ..Default::default() };
        let mut source = LazyRaster::from(crate::RasterDataset::new(crate::Raster::from_data(config, vec![f64::NAN; 4]).unwrap(), true));
        for resampling in [Resampling::Nearest, Resampling::Bilinear, Resampling::Cubic, Resampling::CubicSpline] {
            let info = pixel_info(&mut source, &[Position::new(0.5, 0.5)], &PixelInfoOptions { resampling, ..Default::default() }).unwrap();
            assert!(info.locations[0].bands[0].value.is_some_and(f64::is_nan));
        }
    }

    #[test]
    fn numeric_prefix_scan_handles_long_suffixes_and_incomplete_exponents() {
        assert_eq!(c_atof(&format!("12{}", "q".repeat(100_000))), 12.);
        assert_eq!(c_atof("1e+oops"), 1.);
        assert_eq!(c_atof("-1.5e2tail"), -150.);
        assert!(c_atof("-infinityrest").is_infinite());
    }

    #[test]
    fn parses_positions_as_gdal_reads_standard_input() {
        let p = Position::parse_lines("1 2 hello  world\n3.5 4e1 a, \"b c\"\n\n5 6\n").unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].extra.as_deref(), Some("hello world"));
        assert_eq!((p[1].x, p[1].y), (3.5, 40.0));
        assert_eq!(p[1].extra.as_deref(), Some("a, b c"));
        assert!(Position::parse_lines("1\n").is_err());
    }

    #[test]
    fn inverse_geotransform_matches_gdal() {
        let inv = invert_geo_transform(&[100.0, 10.0, 0.0, 240.0, 0.0, -10.0]).unwrap();
        assert_eq!(inv, [-10.0, 0.1, 0.0, 24.0, 0.0, -0.1]);
        assert!(invert_geo_transform(&[0.0, 1.0, 1.0, 0.0, 1.0, 1.0]).is_none());
    }
    #[test]
    fn nonfinite_locations_are_off_raster() {
        let mut r = raster(2, 2, None);
        let mut info = pixel_info(&mut r, &[Position::new(0.5, 0.5)], &Default::default()).unwrap();
        info.locations[0].column = f64::NAN;
        assert_eq!(info.off_raster_count(), 1);
        assert!(info.to_text().contains("(nanP,"));
        assert!(info.to_json()[0]["column"].is_null());
    }

    #[test]
    fn an_untransformable_input_position_is_fatal_as_gdal() {
        let config =
            RasterConfig { cols: 2, rows: 2, bands: 1, crs: SpatialRef::from_epsg(3857).unwrap().to_crs_info(), ..Default::default() };
        let mut raster = LazyRaster::from(RasterDataset::new(Raster::from_data(config, vec![1.; 4]).unwrap(), false));
        let options = PixelInfoOptions { position_crs: Some(PositionCrs::Crs(SpatialRef::from_epsg(4326).unwrap())), ..Default::default() };
        assert!(pixel_info(&mut raster, &[Position::new(0., 0.)], &options).is_ok());
        assert!(pixel_info(&mut raster, &[Position::new(0., 0.), Position::new(0., 100.)], &options).is_err());
    }

    #[test]
    fn non_finite_marker_does_not_rewrite_user_strings() {
        let mut raster = raster(2, 2, None);
        for extra in ["\0NaN", "\0terrio-nonfinite-0:NaN"] {
            let mut position = Position::new(0.5, 0.5);
            position.extra = Some(extra.into());
            let info = pixel_info(&mut raster, &[position], &Default::default()).unwrap();
            let text = info.to_geojson_string();
            let parsed: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed["features"][0]["properties"]["extra_content"], extra);
        }
    }

    #[test]
    fn public_geojson_values_use_null_for_nonfinite_numbers() {
        let mut raster = raster(2, 2, None);
        let mut info = pixel_info(&mut raster, &[Position::new(0.5, 0.5)], &Default::default()).unwrap();
        info.locations[0].bands[0].value = Some(f64::NAN);
        info.locations[0].bands[0].unscaled = Some(f64::INFINITY);
        let value = info.to_geojson();
        assert!(value["features"][0]["properties"]["bands"][0]["raw_value"].is_null());
        assert!(value["features"][0]["properties"]["bands"][0]["unscaled_value"].is_null());
        let text = info.to_geojson_string();
        assert!(text.contains("NaN") && text.contains("Infinity"));
    }

    #[test]
    fn cache_budget_accounts_for_all_bands() {
        for bands in [1, 10, 100] {
            assert!(max_cached_blocks(bands) * BLOCK * BLOCK * bands * 8 <= MAX_CACHE_BYTES);
        }
    }
}
