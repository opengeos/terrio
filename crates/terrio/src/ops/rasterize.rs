//! Burning vector geometries into a raster, the equivalent of `gdal_rasterize`.
//!
//! The scanline polygon filler, the Bresenham line drawer and the "all touched" line walker are
//! ports of GDAL's (`alg/llrasterize.cpp`, MIT licensed), so that the same pixels are burned:
//! a pixel is inside a polygon when its centre is, lines burn the pixels Bresenham's algorithm
//! visits, and with `all_touched` every pixel a line or polygon edge passes through is burned.

use crate::crs::SpatialRef;
use crate::ops::sql::{Expression, field_index};
use crate::{Error, RasterDataset, Result};
use std::collections::HashSet;
use wbraster::{DataType, Raster, RasterConfig};
use wbvector::{Feature, FieldValue, Geometry, GeometryType, Layer};

/// How burned values combine with what is already in the raster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergeAlg {
    /// Replace the pixel value (the default).
    #[default]
    Replace,
    /// Add to the pixel value; each geometry adds once to a pixel.
    Add,
}

/// The output grid of a rasterization.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    /// The GDAL-style geotransform.
    pub geo_transform: [f64; 6],
    /// The width in pixels.
    pub cols: usize,
    /// The height in pixels.
    pub rows: usize,
    /// The CRS.
    pub crs: SpatialRef,
}

impl Grid {
    /// The grid of an existing raster.
    pub fn like(r: &RasterDataset) -> Self {
        Self { geo_transform: r.geo_transform(), cols: r.raster.cols, rows: r.raster.rows, crs: r.crs() }
    }
}

/// Options for [`rasterize`], named after `gdal_rasterize`'s.
#[derive(Debug, Clone, Default)]
pub struct RasterizeOptions {
    /// `-burn`: the values to burn, one per output band.
    pub burn_values: Vec<f64>,
    /// `-a`: burn the values of this attribute field instead.
    pub attribute: Option<String>,
    /// `-where`: only burn the features matching this OGR SQL expression.
    pub where_clause: Option<String>,
    /// `-tr`: the pixel size `(x, y)`.
    pub resolution: Option<(f64, f64)>,
    /// `-ts`: the size in pixels `(width, height)`; one of them may be 0 to keep the aspect ratio.
    pub size: Option<(usize, usize)>,
    /// `-te`: the extent `(min_x, min_y, max_x, max_y)`. By default, the layers' extent.
    pub extent: Option<(f64, f64, f64, f64)>,
    /// `-tap`: align the extent on multiples of the resolution.
    pub target_aligned: bool,
    /// Use this grid (size, georeferencing and CRS) instead of computing one, such as that of an
    /// existing raster (see [`Grid::like`]).
    pub grid: Option<Grid>,
    /// `-ot`: the output data type. Float64 by default.
    pub data_type: Option<DataType>,
    /// `-init`: the initial values, one per band (or one for all). 0 by default.
    pub init: Vec<f64>,
    /// `-a_nodata`: the nodata value of the output. 0 when not given, as `gdal_rasterize` sets.
    pub nodata: Option<f64>,
    /// `-at`: burn every pixel touched by a line or polygon.
    pub all_touched: bool,
    /// `-add`: add the burn values instead of replacing pixel values.
    pub merge: MergeAlg,
    /// `-a_srs`: the output CRS. Geometries are taken as being in it, without reprojecting.
    pub assign_srs: Option<SpatialRef>,
}

/// Burns the layers' geometries into a new raster.
///
/// The output grid comes from `options.grid`, or else from the extent (`-te`, or the layers'
/// extent widened by half a pixel on each side when only the resolution is given, as GDAL does)
/// and the resolution or size. Its CRS is `assign_srs`, or the first layer's; geometries in
/// another CRS than the grid's are reprojected, unless `assign_srs` is set.
pub fn rasterize(layers: &[Layer], options: &RasterizeOptions) -> Result<RasterDataset> {
    if layers.is_empty() {
        return Err(Error::InvalidArgument("no layer to rasterize".into()));
    }
    if options.attribute.is_some() && !options.burn_values.is_empty() {
        return Err(Error::InvalidArgument("give burn values or an attribute, not both".into()));
    }
    if options.attribute.is_none() && options.burn_values.is_empty() {
        return Err(Error::InvalidArgument("give a burn value or an attribute to burn".into()));
    }
    let grid = match &options.grid {
        Some(g) => g.clone(),
        None => compute_grid(layers, options)?,
    };
    let bands = options.burn_values.len().max(1);
    let dtype = options.data_type.unwrap_or(DataType::F64);
    let n = grid.cols * grid.rows;
    let mut bufs: Vec<Vec<f64>> = (0..bands)
        .map(|b| {
            let init = options.init.get(b).or(options.init.last()).copied().unwrap_or(0.0);
            vec![cast(init, dtype); n]
        })
        .collect();
    let [g0, g1, g2, g3, g4, g5] = grid.geo_transform;
    if g2 != 0.0 || g4 != 0.0 {
        return Err(Error::Unsupported("rotated grids are not supported".into()));
    }
    // As GDAL's `GDALInvGeoTransform`, for north-up grids.
    let inv = [-g0 / g1, 1.0 / g1, -g3 / g5, 1.0 / g5];
    for layer in layers {
        let reprojected;
        let layer = if options.assign_srs.is_none() && needs_reprojection(layer, &grid.crs) {
            reprojected = crate::vector::reproject_layer(layer, &grid.crs)?;
            &reprojected
        } else {
            layer
        };
        let filter = options.where_clause.as_deref().map(|w| Expression::parse(w)?.bind(&layer.schema)).transpose()?;
        let field = match &options.attribute {
            Some(a) => Some(
                field_index(&layer.schema, a)
                    .ok_or_else(|| Error::InvalidArgument(format!("failed to find field {a} on layer {}", layer.name)))?,
            ),
            None => None,
        };
        for f in &layer.features {
            let Some(g) = &f.geometry else { continue };
            if filter.as_ref().is_some_and(|e| !e.matches(f)) {
                continue;
            }
            let values: Vec<f64> = match field {
                Some(i) => vec![attribute_value(f, i); bands],
                None => options.burn_values.clone(),
            };
            let mut burner = Burner {
                bufs: &mut bufs,
                values: &values,
                cols: grid.cols,
                dtype,
                merge: options.merge,
                visited: None,
                fill_visited: false,
            };
            burn_shape(&mut burner, g, grid.cols as i32, grid.rows as i32, &inv, options.all_touched);
        }
    }
    let crs = &grid.crs;
    let cfg = RasterConfig {
        cols: grid.cols,
        rows: grid.rows,
        bands,
        x_min: g0,
        y_min: g3 + g5 * grid.rows as f64,
        cell_size: g1,
        cell_size_y: Some(-g5),
        nodata: options.nodata.unwrap_or(0.0),
        data_type: dtype,
        crs: wbraster::CrsInfo { epsg: crs.epsg, wkt: crs.to_wkt(), proj4: crs.proj4.clone() },
        metadata: Vec::new(),
    };
    let raster = Raster::from_data(cfg, bufs.concat())?;
    Ok(RasterDataset::new(raster, true))
}

fn needs_reprojection(layer: &Layer, crs: &SpatialRef) -> bool {
    let src = SpatialRef::from_layer(layer);
    !src.is_unknown() && !crs.is_unknown() && !src.same_as(crs)
}

/// The value to burn from a feature's field, as GDAL reads it: numbers as they are, text parsed as
/// a number, and 0 for nulls and anything else.
fn attribute_value(f: &Feature, i: usize) -> f64 {
    match f.attributes.get(i) {
        Some(FieldValue::Integer(v)) => *v as f64,
        Some(FieldValue::Float(v)) => *v,
        Some(FieldValue::Boolean(b)) => *b as i64 as f64,
        Some(FieldValue::Text(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Computes the output grid as `gdal_rasterize` does.
fn compute_grid(layers: &[Layer], o: &RasterizeOptions) -> Result<Grid> {
    let crs = match &o.assign_srs {
        Some(s) => s.clone(),
        None => SpatialRef::from_layer(&layers[0]),
    };
    let layers_extent = || -> Result<(f64, f64, f64, f64)> {
        layers
            .iter()
            .filter_map(crate::vector::layer_extent)
            .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
            .ok_or_else(|| Error::InvalidArgument("could not determine bounds: the layers have no geometries".into()))
    };
    let (mut nx, mut ny) = o.size.unwrap_or((0, 0));
    if (nx == 0) != (ny == 0) {
        // GDAL computes the missing size from the layers' extent, even with `-te`.
        let e = layers_extent()?;
        let (w, h) = (e.2 - e.0, e.3 - e.1);
        if w == 0.0 || h == 0.0 {
            return Err(Error::InvalidArgument("cannot calculate size from layer extent".into()));
        }
        if nx == 0 {
            nx = (w / (h / ny as f64)) as usize;
        } else {
            ny = (h / (w / nx as f64)) as usize;
        }
    }
    let (mut xres, mut yres) = o.resolution.unwrap_or((0.0, 0.0));
    let mut env = match o.extent {
        Some(e) => e,
        None => {
            let mut e = layers_extent()?;
            if !o.target_aligned && xres != 0.0 && yres != 0.0 {
                e = (e.0 - xres / 2.0, e.1 - yres / 2.0, e.2 + xres / 2.0, e.3 + yres / 2.0);
            }
            e
        }
    };
    if xres == 0.0 && yres == 0.0 {
        if nx == 0 || ny == 0 {
            return Err(Error::InvalidArgument("give the output size or resolution".into()));
        }
        xres = (env.2 - env.0) / nx as f64;
        yres = (env.3 - env.1) / ny as f64;
    } else if o.target_aligned && xres != 0.0 && yres != 0.0 {
        env = ((env.0 / xres).floor() * xres, (env.1 / yres).floor() * yres, (env.2 / xres).ceil() * xres, (env.3 / yres).ceil() * yres);
    }
    if xres == 0.0 || yres == 0.0 {
        return Err(Error::InvalidArgument("could not determine the resolution".into()));
    }
    if nx == 0 && ny == 0 {
        let fx = 0.5 + (env.2 - env.0) / xres;
        let fy = 0.5 + (env.3 - env.1) / yres;
        if !(fx >= 1.0 && fy >= 1.0 && fx < i32::MAX as f64 && fy < i32::MAX as f64) {
            return Err(Error::InvalidArgument(format!("invalid computed output raster size: {fx} x {fy}")));
        }
        nx = fx as usize;
        ny = fy as usize;
    }
    Ok(Grid { geo_transform: [env.0, xres, 0.0, env.3, 0.0, -yres], cols: nx, rows: ny, crs })
}

/// Converts a value to what a pixel of the data type holds, as GDAL's `GDALCopyWord` does:
/// integers are rounded half away from zero and clamped, NaN becomes 0.
fn cast(v: f64, dtype: DataType) -> f64 {
    let int = |lo: f64, hi: f64| {
        if v.is_nan() {
            0.0
        } else {
            let r = if v >= 0.0 { (v + 0.5).trunc() } else { (v - 0.5).trunc() };
            r.clamp(lo, hi)
        }
    };
    match dtype {
        DataType::U8 => int(0.0, 255.0),
        DataType::I8 => int(-128.0, 127.0),
        DataType::U16 => int(0.0, 65535.0),
        DataType::I16 => int(-32768.0, 32767.0),
        DataType::U32 => int(0.0, u32::MAX as f64),
        DataType::I32 => int(i32::MIN as f64, i32::MAX as f64),
        DataType::U64 => int(0.0, u64::MAX as f64),
        DataType::I64 => int(i64::MIN as f64, i64::MAX as f64),
        DataType::F32 => v as f32 as f64,
        DataType::F64 => v,
    }
}

/// Writes burn values into the band buffers (GDAL's `gvBurnPoint` and `gvBurnScanline`).
struct Burner<'a> {
    bufs: &'a mut [Vec<f64>],
    values: &'a [f64],
    cols: usize,
    dtype: DataType,
    merge: MergeAlg,
    /// In add mode, the pixels a geometry already burned, so that it adds once to each.
    visited: Option<HashSet<u64>>,
    fill_visited: bool,
}

impl Burner<'_> {
    fn burn(&mut self, y: i32, x: i32) {
        let idx = y as usize * self.cols + x as usize;
        for (buf, v) in self.bufs.iter_mut().zip(self.values) {
            let px = &mut buf[idx];
            *px = match self.merge {
                MergeAlg::Replace => cast(*v, self.dtype),
                MergeAlg::Add => cast(*px + v, self.dtype),
            };
        }
    }

    fn point(&mut self, y: i32, x: i32) {
        if let Some(set) = &mut self.visited {
            let key = ((y as u64) << 32) | x as u32 as u64;
            if set.contains(&key) {
                return;
            }
            if self.fill_visited {
                set.insert(key);
            }
        }
        self.burn(y, x);
    }

    fn scanline(&mut self, y: i32, x_start: i32, x_end: i32) {
        if x_start > x_end {
            return;
        }
        let x_start = x_start.max(0);
        let x_end = x_end.min(self.cols as i32 - 1);
        for x in x_start..=x_end {
            if self.merge == MergeAlg::Add
                && let Some(set) = &self.visited
                && set.contains(&(((y as u64) << 32) | x as u32 as u64))
            {
                continue;
            }
            self.burn(y, x);
        }
    }
}

/// Rings and lines in pixel coordinates: points, with the number of points of each part.
#[derive(Default)]
struct Parts {
    x: Vec<f64>,
    y: Vec<f64>,
    sizes: Vec<usize>,
}

impl Parts {
    fn push(&mut self, coords: impl Iterator<Item = (f64, f64)>) {
        let before = self.x.len();
        for (x, y) in coords {
            self.x.push(x);
            self.y.push(y);
        }
        self.sizes.push(self.x.len() - before);
    }

    /// Adds a ring clockwise and closed, as GDAL has them (drivers may drop the closing point).
    fn push_ring(&mut self, r: &[wbvector::Coord]) {
        if r.is_empty() {
            return;
        }
        let open = r.first().zip(r.last()).is_some_and(|(a, b)| a.x != b.x || a.y != b.y);
        let closing = open.then(|| (r[0].x, r[0].y));
        let mut coords: Vec<(f64, f64)> = r.iter().map(|c| (c.x, c.y)).chain(closing).collect();
        if !is_clockwise(r) {
            coords.reverse();
        }
        self.push(coords.into_iter());
    }
}

/// Collects the geometry's coordinates as GDAL's `GDALCollectRingsFromGeometry` does: lines in
/// reverse order, rings clockwise.
fn collect(g: &Geometry, p: &mut Parts) {
    match g {
        Geometry::Point(c) => p.push(std::iter::once((c.x, c.y))),
        Geometry::MultiPoint(cs) => cs.iter().for_each(|c| p.push(std::iter::once((c.x, c.y)))),
        Geometry::LineString(cs) => p.push(cs.iter().rev().map(|c| (c.x, c.y))),
        Geometry::MultiLineString(ls) => {
            for cs in ls.iter().filter(|l| !l.is_empty()) {
                p.push(cs.iter().rev().map(|c| (c.x, c.y)));
            }
        }
        Geometry::Polygon { exterior, interiors } => std::iter::once(exterior).chain(interiors).for_each(|r| p.push_ring(&r.0)),
        Geometry::MultiPolygon(ps) => {
            for (e, h) in ps {
                std::iter::once(e).chain(h).for_each(|r| p.push_ring(&r.0));
            }
        }
        Geometry::GeometryCollection(gs) => gs.iter().for_each(|g| collect(g, p)),
    }
}

/// Whether a ring turns clockwise, by its signed area.
fn is_clockwise(r: &[wbvector::Coord]) -> bool {
    let n = r.len();
    if n < 3 {
        return false;
    }
    let mut sum = 0.0;
    for i in 0..n {
        let (a, b) = (&r[i], &r[(i + 1) % n]);
        sum += (b.x - a.x) * (b.y + a.y);
    }
    sum > 0.0
}

/// Burns one geometry (GDAL's `gv_rasterize_one_shape`).
fn burn_shape(b: &mut Burner, g: &Geometry, nx: i32, ny: i32, inv: &[f64; 4], all_touched: bool) {
    if g.is_empty() {
        return;
    }
    let t = g.geom_type();
    if b.merge == MergeAlg::Replace
        && matches!(t, GeometryType::MultiLineString | GeometryType::MultiPolygon | GeometryType::GeometryCollection)
    {
        // In replace mode, each part is burned on its own.
        for part in crate::ops::vector_filter::explode(g) {
            burn_shape(b, &part, nx, ny, inv, all_touched);
        }
        return;
    }
    let mut p = Parts::default();
    collect(g, &mut p);
    // As GDAL's transformer, with its inverse geotransform: x = inv0 + X * inv1, y = inv3 + Y * inv5.
    for (x, y) in p.x.iter_mut().zip(p.y.iter_mut()) {
        *x = inv[0] + *x * inv[1];
        *y = inv[2] + *y * inv[3];
    }
    let add = b.merge == MergeAlg::Add;
    match t {
        GeometryType::Point | GeometryType::MultiPoint => image_point(b, nx, ny, &p),
        GeometryType::LineString | GeometryType::MultiLineString => {
            if add {
                b.visited = Some(HashSet::new());
                b.fill_visited = true;
            }
            if all_touched {
                image_line_all_touched(b, nx, ny, &p, add, false);
            } else {
                image_line(b, nx, ny, &p);
            }
        }
        _ => {
            if add {
                b.visited = Some(HashSet::new());
                b.fill_visited = true;
            }
            if all_touched {
                image_line_all_touched(b, nx, ny, &p, add, true);
            }
            b.fill_visited = false;
            filled_polygon(b, nx, ny, &p, add);
        }
    }
    b.visited = None;
    b.fill_visited = false;
}

/// GDAL's `GDALdllImagePoint`.
fn image_point(b: &mut Burner, nx: i32, ny: i32, p: &Parts) {
    for (&x, &y) in p.x.iter().zip(&p.y) {
        if 0.0 <= x && x < nx as f64 && 0.0 <= y && y < ny as f64 {
            b.point(y as i32, x as i32);
        }
    }
}

fn off_target(x: f64, y: f64, x_end: f64, y_end: f64, nx: i32, ny: i32) -> bool {
    (y < 0.0 && y_end < 0.0) || (y > ny as f64 && y_end > ny as f64) || (x < 0.0 && x_end < 0.0) || (x > nx as f64 && x_end > nx as f64)
}

fn in_int_range(v: f64) -> bool {
    v >= i32::MIN as f64 && v <= i32::MAX as f64
}

/// GDAL's `GDALdllImageLine`: Bresenham, not burning the end of a segment unless it is the last.
fn image_line(b: &mut Burner, nx: i32, ny: i32, p: &Parts) {
    let mut n = 0;
    for &size in &p.sizes {
        for j in 1..size {
            let (x, y, x_end, y_end) = (p.x[n + j - 1], p.y[n + j - 1], p.x[n + j], p.y[n + j]);
            if off_target(x, y, x_end, y_end, nx, ny) || ![x, y, x_end, y_end].into_iter().all(in_int_range) {
                continue;
            }
            let mut ix = x.floor() as i32;
            let mut iy = y.floor() as i32;
            let ix1 = x_end.floor() as i32;
            let iy1 = y_end.floor() as i32;
            let mut dx = (ix1 - ix).abs();
            let mut dy = (iy1 - iy).abs();
            let x_step = if ix > ix1 { -1 } else { 1 };
            let y_step = if iy > iy1 { -1 } else { 1 };
            if dx >= dy {
                let x_error = dy << 1;
                let y_error = x_error - (dx << 1);
                let mut error = x_error - dx;
                if j != size - 1 {
                    dx -= 1;
                }
                while dx >= 0 {
                    dx -= 1;
                    if 0 <= ix && ix < nx && 0 <= iy && iy < ny {
                        b.point(iy, ix);
                    }
                    ix += x_step;
                    if error > 0 {
                        iy += y_step;
                        error += y_error;
                    } else {
                        error += x_error;
                    }
                }
            } else {
                let x_error = dx << 1;
                let y_error = x_error - (dy << 1);
                let mut error = x_error - dy;
                if j != size - 1 {
                    dy -= 1;
                }
                while dy >= 0 {
                    dy -= 1;
                    if 0 <= ix && ix < nx && 0 <= iy && iy < ny {
                        b.point(iy, ix);
                    }
                    iy += y_step;
                    if error > 0 {
                        ix += x_step;
                        error += y_error;
                    } else {
                        error += x_error;
                    }
                }
            }
        }
        n += size;
    }
}

/// GDAL's `GDALdllImageLineAllTouched`: every pixel a segment passes through.
fn image_line_all_touched(b: &mut Burner, nx: i32, ny: i32, p: &Parts, avoid_same: bool, intersect_only: bool) {
    const EPSILON_INTERSECT_ONLY: f64 = 1e-4;
    let (nxf, nyf) = (nx as f64, ny as f64);
    let mut n = 0;
    for &size in &p.sizes {
        let mut last_burnt: HashSet<(i32, i32)>;
        let mut new_burnt: HashSet<(i32, i32)> = HashSet::new();
        let burn = |b: &mut Burner, last: &HashSet<(i32, i32)>, new: &mut HashSet<(i32, i32)>, iy: i32, ix: i32, check_new: bool| {
            if avoid_same {
                let yx = (iy, ix);
                if last.contains(&yx) || (check_new && new.contains(&yx)) {
                    return;
                }
                new.insert(yx);
            }
            b.point(iy, ix);
        };
        for j in 1..size {
            last_burnt = std::mem::take(&mut new_burnt);
            let (mut x, mut y, mut x_end, mut y_end) = (p.x[n + j - 1], p.y[n + j - 1], p.x[n + j], p.y[n + j]);
            if off_target(x, y, x_end, y_end, nx, ny) || ![x, y, x_end, y_end].into_iter().all(in_int_range) {
                continue;
            }
            if x > x_end {
                std::mem::swap(&mut x, &mut x_end);
                std::mem::swap(&mut y, &mut y_end);
            }
            // Vertical lines.
            if (x - x_end).abs() < 0.01 {
                if intersect_only
                    && (x - x.round()).abs() < EPSILON_INTERSECT_ONLY
                    && (x_end - x_end.round()).abs() < EPSILON_INTERSECT_ONLY
                {
                    continue;
                }
                if y_end < y {
                    std::mem::swap(&mut y, &mut y_end);
                }
                let ix = x_end.floor() as i32;
                let mut iy = y.floor() as i32;
                let mut iy_end = (y_end - EPSILON_INTERSECT_ONLY).floor() as i32;
                if ix < 0 || ix >= nx {
                    continue;
                }
                iy = iy.max(0);
                if iy_end >= ny {
                    iy_end = ny - 1;
                }
                while iy <= iy_end {
                    burn(b, &last_burnt, &mut new_burnt, iy, ix, false);
                    iy += 1;
                }
                continue;
            }
            // Horizontal lines.
            if (y - y_end).abs() < 0.01 {
                if intersect_only
                    && (y - y.round()).abs() < EPSILON_INTERSECT_ONLY
                    && (y_end - y_end.round()).abs() < EPSILON_INTERSECT_ONLY
                {
                    continue;
                }
                if x_end < x {
                    std::mem::swap(&mut x, &mut x_end);
                }
                let mut ix = x.floor() as i32;
                let iy = y.floor() as i32;
                let mut ix_end = (x_end - EPSILON_INTERSECT_ONLY).floor() as i32;
                if iy < 0 || iy >= ny {
                    continue;
                }
                ix = ix.max(0);
                if ix_end >= nx {
                    ix_end = nx - 1;
                }
                while ix <= ix_end {
                    burn(b, &last_burnt, &mut new_burnt, iy, ix, false);
                    ix += 1;
                }
                continue;
            }
            // General case: left to right, sloped.
            let slope = (y_end - y) / (x_end - x);
            if x_end > nxf {
                y_end -= (x_end - nxf) * slope;
                x_end = nxf;
            }
            if x < 0.0 {
                y += (0.0 - x) * slope;
                x = 0.0;
            }
            if y_end > y {
                if y < 0.0 {
                    x += (0.0 - y) / slope;
                    y = 0.0;
                }
                if y_end >= nyf {
                    x_end += (y_end - nyf) / slope;
                    if x_end > nxf {
                        x_end = nxf;
                    }
                }
            } else {
                if y >= nyf {
                    x += (nyf - y) / slope;
                    y = nyf;
                }
                if y_end < 0.0 {
                    x_end -= (y_end - 0.0) / slope;
                }
            }
            while x >= 0.0 && x < x_end {
                let ix = x.floor() as i32;
                let iy = y.floor() as i32;
                if iy >= 0 && iy < ny {
                    burn(b, &last_burnt, &mut new_burnt, iy, ix, true);
                }
                let mut step_x = (x + 1.0).floor() - x;
                let mut step_y = step_x * slope;
                if (y + step_y).floor() as i32 == iy {
                    x += step_x;
                    y += step_y;
                } else if slope < 0.0 {
                    step_y = iy as f64 - y;
                    if step_y > -0.000000001 {
                        step_y = -0.000000001;
                    }
                    step_x = step_y / slope;
                    x += step_x;
                    y += step_y;
                } else {
                    step_y = (iy + 1) as f64 - y;
                    if step_y < 0.000000001 {
                        step_y = 0.000000001;
                    }
                    step_x = step_y / slope;
                    x += step_x;
                    y += step_y;
                }
            }
        }
        n += size;
    }
}

/// GDAL's `GDALdllImageFilledPolygon`: even-odd scanline filling, pixel centres inside.
fn filled_polygon(b: &mut Burner, nx: i32, ny: i32, p: &Parts, avoid_same: bool) {
    let n = p.x.len();
    if n == 0 {
        return;
    }
    let (mut dminy, mut dmaxy) = (p.y[0], p.y[0]);
    for &y in &p.y[1..] {
        if y < dminy {
            dminy = y;
        } else if y > dmaxy {
            dmaxy = y;
        }
    }
    let miny = dminy.max(0.0) as i32;
    let maxy = dmaxy.min((ny - 1) as f64) as i32;
    let (minx, maxx) = (0, nx - 1);
    let mut ints: Vec<i32> = Vec::with_capacity(n);
    let mut ints2: Vec<i32> = Vec::new();
    for y in miny..=maxy {
        ints.clear();
        ints2.clear();
        let dy = y as f64 + 0.5;
        let (mut part, mut part_offset) = (0, 0);
        for i in 0..n {
            if i == part_offset + p.sizes[part] {
                part_offset += p.sizes[part];
                part += 1;
            }
            let (ind1, ind2) = if i == part_offset { (part_offset + p.sizes[part] - 1, part_offset) } else { (i - 1, i) };
            let (mut dy1, mut dy2) = (p.y[ind1], p.y[ind2]);
            if (dy1 < dy && dy2 < dy) || (dy1 > dy && dy2 > dy) {
                continue;
            }
            let (dx1, dx2);
            if dy1 < dy2 {
                dx1 = p.x[ind1];
                dx2 = p.x[ind2];
            } else if dy1 > dy2 {
                std::mem::swap(&mut dy1, &mut dy2);
                dx2 = p.x[ind1];
                dx1 = p.x[ind2];
            } else {
                // Horizontal segments at the bottom of the polygon are filled on their own.
                if p.x[ind1] > p.x[ind2] {
                    let h1 = (p.x[ind2] + 0.5).floor();
                    let h2 = (p.x[ind1] + 0.5).floor();
                    if h1 > maxx as f64 || h2 <= 0.0 {
                        continue;
                    }
                    let h1 = h1.max(0.0) as i32;
                    let h2 = h2.min(nx as f64) as i32;
                    if avoid_same {
                        ints2.push(h1);
                        ints2.push(h2);
                    } else {
                        b.scanline(y, h1, h2 - 1);
                    }
                }
                continue;
            }
            if dy < dy2 && dy >= dy1 {
                let intersect = ((dy - dy1) * (dx2 - dx1) / (dy2 - dy1) + dx1).clamp(i32::MIN as f64, i32::MAX as f64);
                ints.push((intersect + 0.5).floor() as i32);
            }
        }
        ints.sort_unstable();
        ints2.sort_unstable();
        for pair in ints.as_chunks::<2>().0 {
            if pair[0] <= maxx && pair[1] > minx {
                b.scanline(y, pair[0], pair[1] - 1);
            }
        }
        let mut i = 0;
        for pair in ints2.as_chunks::<2>().0 {
            if pair[0] <= maxx && pair[1] > minx {
                while i + 1 < ints.len() && ints[i] < pair[0] {
                    i += 2;
                }
                if i + 1 >= ints.len() || ints[i] != pair[0] {
                    b.scanline(y, pair[0], pair[1] - 1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Coord, FieldDef, FieldType};

    fn layer(geoms: Vec<Geometry>) -> Layer {
        let mut l = Layer::new("t");
        l.schema.add_field(FieldDef::new("v", FieldType::Integer));
        for (i, g) in geoms.into_iter().enumerate() {
            l.features.push(Feature { fid: i as u64, geometry: Some(g), attributes: vec![FieldValue::Integer(i as i64 + 1)] });
        }
        l
    }

    fn grid(cols: usize, rows: usize) -> Grid {
        Grid { geo_transform: [0.0, 1.0, 0.0, rows as f64, 0.0, -1.0], cols, rows, crs: SpatialRef::default() }
    }

    fn pixels(r: &RasterDataset) -> Vec<f64> {
        r.raster.band_to_vec_f64(0)
    }

    #[test]
    fn polygon_with_hole_by_pixel_centres() {
        let ring = |x0: f64, y0: f64, x1: f64, y1: f64| {
            vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1), Coord::xy(x0, y0)]
        };
        let poly = Geometry::polygon(ring(0.6, 0.6, 5.4, 5.4), vec![ring(2.0, 2.0, 4.0, 4.0)]);
        let o = RasterizeOptions { burn_values: vec![1.0], grid: Some(grid(6, 6)), data_type: Some(DataType::U8), ..Default::default() };
        let r = rasterize(&[layer(vec![poly.clone()])], &o).unwrap();
        // Rows top-down: centres 0.5 and 5.5 are outside; the hole covers centres 2.5 and 3.5.
        #[rustfmt::skip]
        let expected: [i32; 36] = [
            0, 0, 0, 0, 0, 0,
            0, 1, 1, 1, 1, 0,
            0, 1, 0, 0, 1, 0,
            0, 1, 0, 0, 1, 0,
            0, 1, 1, 1, 1, 0,
            0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(pixels(&r), expected.iter().map(|&v| v as f64).collect::<Vec<_>>());
        let o = RasterizeOptions { all_touched: true, ..o };
        let r = rasterize(&[layer(vec![poly])], &o).unwrap();
        assert_eq!(pixels(&r).iter().filter(|&&v| v == 1.0).count(), 36 - 4);
    }

    #[test]
    fn lines_points_attribute_and_add() {
        let line = Geometry::line_string(vec![Coord::xy(0.5, 0.5), Coord::xy(3.5, 0.5)]);
        let pts = Geometry::multi_point(vec![Coord::xy(0.5, 3.5), Coord::xy(9.0, 9.0)]);
        let o = RasterizeOptions { attribute: Some("V".into()), grid: Some(grid(4, 4)), init: vec![-1.0], ..Default::default() };
        let r = rasterize(&[layer(vec![line.clone(), pts])], &o).unwrap();
        let px = pixels(&r);
        assert_eq!(&px[12..16], &[1.0, 1.0, 1.0, 1.0]);
        assert_eq!(px[0], 2.0);
        assert_eq!(px.iter().filter(|&&v| v == -1.0).count(), 11);
        // Adding: two overlapping lines sum, but a line adds once to each pixel.
        let o = RasterizeOptions { burn_values: vec![2.0], grid: Some(grid(4, 4)), merge: MergeAlg::Add, ..Default::default() };
        let back = Geometry::line_string(vec![Coord::xy(0.5, 0.5), Coord::xy(3.5, 0.5), Coord::xy(0.5, 0.5)]);
        let r = rasterize(&[layer(vec![line, back])], &o).unwrap();
        assert_eq!(&pixels(&r)[12..16], &[4.0, 4.0, 4.0, 4.0]);
    }

    #[test]
    fn grid_like_gdal() {
        let l = layer(vec![Geometry::line_string(vec![Coord::xy(0.0, 0.0), Coord::xy(10.0, 5.0)])]);
        let o = RasterizeOptions { burn_values: vec![1.0], resolution: Some((1.0, 1.0)), ..Default::default() };
        let r = rasterize(std::slice::from_ref(&l), &o).unwrap();
        // The extent grows by half a pixel on each side.
        assert_eq!(r.geo_transform(), [-0.5, 1.0, 0.0, 5.5, 0.0, -1.0]);
        assert_eq!((r.raster.cols, r.raster.rows), (11, 6));
        let o = RasterizeOptions { burn_values: vec![1.0], size: Some((20, 0)), ..Default::default() };
        let r = rasterize(std::slice::from_ref(&l), &o).unwrap();
        assert_eq!((r.raster.cols, r.raster.rows), (20, 10));
        let o = RasterizeOptions { burn_values: vec![1.0], resolution: Some((3.0, 3.0)), target_aligned: true, ..Default::default() };
        let r = rasterize(&[l], &o).unwrap();
        assert_eq!(r.geo_transform(), [0.0, 3.0, 0.0, 6.0, 0.0, -3.0]);
        assert_eq!(cast(2.5, DataType::U8), 3.0);
        assert_eq!(cast(-2.5, DataType::I16), -3.0);
        assert_eq!(cast(300.0, DataType::U8), 255.0);
    }
}
