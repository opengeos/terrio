// SPDX-FileCopyrightText: Copyright (c) 2016, Hobu Inc. (info@hobu.co)
// SPDX-FileCopyrightText: Copyright (c) 2015, Even Rouault <even.rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2007, Andrey Kiselev <dron@ak4719.spb.edu>
// SPDX-FileCopyrightText: Copyright (c) 2009-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   PDAL 2.10.2:
//     io/private/GDALGrid.cpp
//   GDAL v3.13.3:
//     alg/delaunay.c
//     alg/gdalgrid.cpp

//! Gridding points into rasters (DEMs, DSMs, intensity or density grids), as PDAL's
//! `writers.gdal` does, or by interpolation, as GDAL's `gdal_grid` does.
//!
//! [`GridMethod::Statistics`] (the default) bins the points as `writers.gdal` does, below; it
//! reads the points in chunks ([`grid_reader`]), so files larger than memory grid in bounded
//! memory. [`GridMethod::Linear`] interpolates linearly within the triangles of the points'
//! Delaunay triangulation (a TIN), as `gdal_grid -a linear`; [`GridMethod::Nearest`] takes the
//! value of the nearest point, as `gdal_grid -a nearest`. Both take the value at each cell's
//! centre.
//!
//! Each point counts towards every cell whose centre is closer than the radius (by default the
//! cell's diagonal, `resolution × √2`), or, in bin mode, towards the cell it falls in only. Per
//! cell, the statistics are the minimum, maximum, mean, inverse-distance-weighted mean (weights
//! `1 / distanceᵖ`; a point at the centre gives its value), count and standard deviation of the
//! values of a dimension. Cells without points are nodata, or are filled from their neighbours
//! within `window_size` cells, weighted by the inverse of the distance in cells.

use super::PointCloud;
use crate::{Error, RasterDataset, Result};
use wbraster::{DataType, Raster, RasterConfig};

/// A statistic computed per cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GridStat {
    /// The smallest value.
    Min,
    /// The largest value.
    Max,
    /// The mean.
    Mean,
    /// The inverse-distance-weighted mean.
    Idw,
    /// The number of points.
    Count,
    /// The population standard deviation.
    Stdev,
}

impl GridStat {
    /// Parses `min`, `max`, `mean`, `idw`, `count` or `stdev`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "min" => GridStat::Min,
            "max" => GridStat::Max,
            "mean" => GridStat::Mean,
            "idw" => GridStat::Idw,
            "count" => GridStat::Count,
            "stdev" | "std" | "stddev" => GridStat::Stdev,
            other => return Err(Error::InvalidArgument(format!("unknown statistic '{other}' (min, max, mean, idw, count, stdev)"))),
        })
    }

    /// Parses a comma-separated list; `all` is every statistic.
    pub fn parse_list(s: &str) -> Result<Vec<Self>> {
        if s.trim().eq_ignore_ascii_case("all") {
            return Ok(Self::all());
        }
        s.split(',').filter(|p| !p.trim().is_empty()).map(Self::parse).collect()
    }

    /// Every statistic, in PDAL's band order.
    pub fn all() -> Vec<Self> {
        vec![GridStat::Min, GridStat::Max, GridStat::Mean, GridStat::Idw, GridStat::Count, GridStat::Stdev]
    }

    /// The name.
    pub fn name(self) -> &'static str {
        match self {
            GridStat::Min => "min",
            GridStat::Max => "max",
            GridStat::Mean => "mean",
            GridStat::Idw => "idw",
            GridStat::Count => "count",
            GridStat::Stdev => "stdev",
        }
    }
}

/// How to compute cell values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GridMethod {
    /// Statistics of the points near each cell, as PDAL's `writers.gdal`.
    #[default]
    Statistics,
    /// Linear interpolation in the Delaunay triangulation of the points (a TIN), as
    /// `gdal_grid -a linear`. Cells outside the triangulation take a near point's value, as GDAL
    /// finds it (see below), within `radius` when it is positive, or nodata when it is 0.
    Linear,
    /// The value of the nearest point, as `gdal_grid -a nearest`: of all the points, or, with a
    /// `radius`, of those in the square of that half-side around the cell centre (as GDAL's
    /// quadtree finds them; within the circle for 100 points or fewer). Of points as near, the
    /// last one's.
    Nearest,
}

impl GridMethod {
    /// Parses `statistics` (or `bin`), `linear` (or `tin`) or `nearest`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "statistics" | "stats" | "bin" | "pdal" => GridMethod::Statistics,
            "linear" | "tin" => GridMethod::Linear,
            "nearest" | "nearest_neighbor" | "nearest-neighbour" | "nn" => GridMethod::Nearest,
            o => return Err(Error::InvalidArgument(format!("unknown gridding method '{o}' (statistics, linear, nearest)"))),
        })
    }
}

/// How to grid points.
#[derive(Debug, Clone, PartialEq)]
pub struct GridOptions {
    /// The cell size.
    pub resolution: f64,
    /// How cell values are computed. Statistics by default.
    pub method: GridMethod,
    /// The statistics, one band each, written in PDAL's order (min, max, mean, idw, count,
    /// stdev). Empty means `mean`.
    pub stats: Vec<GridStat>,
    /// The dimension gridded. `z` by default.
    pub dimension: String,
    /// The distance from a cell centre within which points count. By default `resolution × √2`.
    /// Linear: the search radius of the nearest point outside the triangulation (negative: no
    /// limit, the default; 0: none, nodata). Nearest: the half-side of the square searched (by
    /// default, growing until it finds points).
    pub radius: Option<f64>,
    /// The power of the inverse distance weights. 1 by default.
    pub power: f64,
    /// Fill empty cells from the cells within this many cells. 0 (no filling) by default.
    pub window_size: usize,
    /// Count each point in the cell it falls in only, ignoring the radius.
    pub bin_mode: bool,
    /// The grid: `[min_x, min_y, max_x, max_y]`, with `min_x, min_y` the corner of the first cell
    /// and `floor((max - min) / resolution) + 1` cells each way, as in PDAL. By default, the
    /// points' bounds.
    pub bounds: Option<[f64; 4]>,
    /// The nodata value. -9999 by default.
    pub nodata: f64,
    /// The output data type. `Float64` by default.
    pub data_type: DataType,
}

impl Default for GridOptions {
    fn default() -> Self {
        Self {
            resolution: 1.0,
            method: GridMethod::Statistics,
            stats: Vec::new(),
            dimension: "z".into(),
            radius: None,
            power: 1.0,
            window_size: 0,
            bin_mode: false,
            bounds: None,
            nodata: -9999.0,
            data_type: DataType::F64,
        }
    }
}

/// The grid's extent: `[min_x, min_y, max_x, max_y]` and its size in cells, as PDAL sizes it.
fn extent(bounds: [f64; 4], res: f64) -> Result<(usize, usize)> {
    if !(res > 0.0 && res.is_finite()) {
        return Err(Error::InvalidArgument(format!("invalid resolution {res}")));
    }
    let width = ((bounds[2] - bounds[0]) / res).floor() + 1.0;
    let height = ((bounds[3] - bounds[1]) / res).floor() + 1.0;
    if !(width >= 1.0 && height >= 1.0) || width * height > 1e10 {
        return Err(Error::InvalidArgument(format!("invalid grid of {width} x {height} cells")));
    }
    Ok((width as usize, height as usize))
}

/// Grids the points of a cloud into a raster: one band per statistic, or one band of
/// interpolated values.
pub fn grid(cloud: &PointCloud, options: &GridOptions) -> Result<RasterDataset> {
    let bounds = match options.bounds {
        Some(b) => b,
        None => {
            let b = cloud.bounds().ok_or_else(|| Error::InvalidArgument("no points to grid".into()))?;
            [b[0], b[1], b[3], b[4]]
        }
    };
    if options.method != GridMethod::Statistics {
        let (x, y, v) = columns(cloud, &options.dimension)?;
        return interpolate(&x, &y, &v, bounds, &cloud.crs, options);
    }
    let mut g = GridBuilder::new(options, bounds, cloud.crs.clone())?;
    g.add(cloud)?;
    g.finish()
}

/// The x, y and gridded values of points.
fn columns(cloud: &PointCloud, dimension: &str) -> Result<(Vec<f64>, Vec<f64>, Vec<f64>)> {
    let xs = cloud.coords("x").ok_or_else(|| Error::InvalidArgument("the points have no x".into()))?;
    let ys = cloud.coords("y").ok_or_else(|| Error::InvalidArgument("the points have no y".into()))?;
    let values = cloud
        .dim(dimension)
        .ok_or_else(|| Error::InvalidArgument(format!("no dimension '{dimension}' (dimensions: {})", cloud.dimension_names().join(", "))))?
        .to_f64();
    Ok((xs.to_vec(), ys.to_vec(), values))
}

/// Grids the points of an open file, read in chunks: in bounded memory for the statistics (the
/// interpolation methods keep each point's x, y and value). The grid is `options.bounds`, or the
/// header's bounds when all the points are read from a LAS, LAZ or COPC file, or else the bounds
/// of the points read (in a first pass). `assign_srs` replaces the file's CRS.
pub fn grid_reader(
    reader: &mut super::PointCloudReader,
    read: &super::PointReadOptions,
    options: &GridOptions,
    assign_srs: Option<&crate::crs::SpatialRef>,
) -> Result<RasterDataset> {
    let crs = assign_srs.cloned().unwrap_or_else(|| reader.crs().clone());
    if options.method != GridMethod::Statistics {
        let (mut x, mut y, mut v) = (Vec::new(), Vec::new(), Vec::new());
        reader.start_chunks(read, 0)?;
        while let Some(chunk) = reader.next_chunk()? {
            let (a, b, c) = columns(&chunk, &options.dimension)?;
            x.extend(a);
            y.extend(b);
            v.extend(c);
        }
        let bounds = match options.bounds {
            Some(b) => b,
            None => {
                if x.is_empty() {
                    return Err(Error::InvalidArgument("no points to grid".into()));
                }
                let mm = |a: &[f64]| a.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |m, v| (m.0.min(*v), m.1.max(*v)));
                let ((x0, x1), (y0, y1)) = (mm(&x), mm(&y));
                [x0, y0, x1, y1]
            }
        };
        return interpolate(&x, &y, &v, bounds, &crs, options);
    }
    let all = read == &super::PointReadOptions::default() && matches!(reader.format_name(), "LAS" | "LAZ" | "COPC");
    let bounds = match options.bounds {
        Some(b) => b,
        None if all => {
            let h = reader.header();
            [h.min[0], h.min[1], h.max[0], h.max[1]]
        }
        None => {
            let mut b: Option<[f64; 4]> = None;
            reader.start_chunks(read, 0)?;
            while let Some(chunk) = reader.next_chunk()? {
                if let Some(c) = chunk.bounds() {
                    b = Some(match b {
                        None => [c[0], c[1], c[3], c[4]],
                        Some(a) => [a[0].min(c[0]), a[1].min(c[1]), a[2].max(c[3]), a[3].max(c[4])],
                    });
                }
            }
            b.ok_or_else(|| Error::InvalidArgument("no points to grid".into()))?
        }
    };
    let mut g = GridBuilder::new(options, bounds, crs)?;
    reader.start_chunks(read, 0)?;
    while let Some(chunk) = reader.next_chunk()? {
        g.add(&chunk)?;
    }
    g.finish()
}

/// Bins points into a grid, chunk by chunk, as PDAL's `writers.gdal` does.
pub struct GridBuilder {
    options: GridOptions,
    stats: Vec<GridStat>,
    x0: f64,
    y0: f64,
    w: usize,
    h: usize,
    radius: f64,
    acc: Accumulator,
    crs: crate::crs::SpatialRef,
}

impl GridBuilder {
    /// A grid of `bounds` (`[min_x, min_y, max_x, max_y]`, the first cell's corner at the
    /// minimum, as in PDAL).
    pub fn new(options: &GridOptions, bounds: [f64; 4], crs: crate::crs::SpatialRef) -> Result<Self> {
        let res = options.resolution;
        let (w, h) = extent(bounds, res)?;
        let mut stats = if options.stats.is_empty() { vec![GridStat::Mean] } else { options.stats.clone() };
        stats.sort();
        stats.dedup();
        let radius = options.radius.unwrap_or(res * std::f64::consts::SQRT_2);
        let acc = Accumulator::new(w, h, &stats);
        Ok(Self { options: options.clone(), stats, x0: bounds[0], y0: bounds[1], w, h, radius, acc, crs })
    }

    /// Adds points.
    pub fn add(&mut self, cloud: &PointCloud) -> Result<()> {
        let (xs, ys, values) = columns(cloud, &self.options.dimension)?;
        let (res, w, h, x0, y0) = (self.options.resolution, self.w, self.h, self.x0, self.y0);
        let reach = (self.radius / res).ceil() as i64 + 1;
        let power = self.options.power;
        for (i, v) in values.iter().enumerate() {
            let (x, y) = (xs[i], ys[i]);
            let ci = ((x - x0) / res).floor() as i64;
            let cj = ((y - y0) / res).floor() as i64;
            if self.options.bin_mode {
                if ci >= 0 && cj >= 0 && (ci as usize) < w && (cj as usize) < h {
                    self.acc.update(ci as usize + cj as usize * w, *v, 0.0, power);
                }
                continue;
            }
            for j in (cj - reach).max(0)..=(cj + reach).min(h as i64 - 1) {
                let cy = y0 + (j as f64 + 0.5) * res;
                for i2 in (ci - reach).max(0)..=(ci + reach).min(w as i64 - 1) {
                    let cx = x0 + (i2 as f64 + 0.5) * res;
                    let d = ((cx - x).powi(2) + (cy - y).powi(2)).sqrt();
                    if d < self.radius {
                        self.acc.update(i2 as usize + j as usize * w, *v, d, power);
                    }
                }
            }
        }
        Ok(())
    }

    /// The raster: one band per statistic, in PDAL's order.
    pub fn finish(mut self) -> Result<RasterDataset> {
        self.acc.finalize(self.options.window_size);
        let (w, h) = (self.w, self.h);
        let mut data = Vec::with_capacity(w * h * self.stats.len());
        for s in &self.stats {
            let band = self.acc.band(*s);
            push_rows(&mut data, band, w, h, &self.options);
        }
        let names = self.stats.iter().map(|s| s.name()).collect::<Vec<_>>().join(",");
        raster(data, w, h, self.stats.len(), self.x0, self.y0, &self.crs, &self.options, names)
    }
}

/// Appends a band's rows, top row first (row r is cell row h - 1 - r), NaN as nodata.
fn push_rows(data: &mut Vec<f64>, band: &[f64], w: usize, h: usize, options: &GridOptions) {
    for r in 0..h {
        let j = h - 1 - r;
        for i in 0..w {
            let v = band[i + j * w];
            data.push(if v.is_nan() { options.nodata } else { crate::ops::convert_value(v, options.data_type) });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn raster(
    data: Vec<f64>,
    w: usize,
    h: usize,
    bands: usize,
    x0: f64,
    y0: f64,
    crs: &crate::crs::SpatialRef,
    options: &GridOptions,
    names: String,
) -> Result<RasterDataset> {
    let cfg = RasterConfig {
        cols: w,
        rows: h,
        bands,
        x_min: x0,
        y_min: y0,
        cell_size: options.resolution,
        cell_size_y: Some(options.resolution),
        nodata: options.nodata,
        data_type: options.data_type,
        crs: wbraster::CrsInfo { epsg: crs.epsg, wkt: crs.to_wkt(), proj4: crs.proj4.clone() },
        metadata: vec![("BANDS".into(), names)],
    };
    Ok(RasterDataset::new(Raster::from_data(cfg, data)?, true))
}

/// Interpolates points at the cell centres of a grid: linearly in their Delaunay triangulation,
/// or from the nearest point, as `gdal_grid` does.
fn interpolate(
    xs: &[f64],
    ys: &[f64],
    vs: &[f64],
    bounds: [f64; 4],
    crs: &crate::crs::SpatialRef,
    options: &GridOptions,
) -> Result<RasterDataset> {
    let res = options.resolution;
    let (w, h) = extent(bounds, res)?;
    if xs.is_empty() {
        return Err(Error::InvalidArgument("no points to grid".into()));
    }
    let (x0, y0) = (bounds[0], bounds[1]);
    let center = |i: usize, j: usize| (x0 + (i as f64 + 0.5) * res, y0 + (j as f64 + 0.5) * res);
    // Cell (i, j) at i + j × w, row 0 at the minimum y.
    let mut out = vec![f64::NAN; w * h];
    let mut done = vec![false; w * h];
    let radius = options.radius;
    if options.method == GridMethod::Linear {
        let points: Vec<delaunator::Point> = xs.iter().zip(ys).map(|(x, y)| delaunator::Point { x: *x, y: *y }).collect();
        let t = delaunator::triangulate(&points);
        const EPS: f64 = 1e-10;
        for tri in t.triangles.as_chunks::<3>().0 {
            let (a, b, c) = (tri[0], tri[1], tri[2]);
            let (x1, y1, x2, y2, x3, y3) = (xs[a], ys[a], xs[b], ys[b], xs[c], ys[c]);
            // GDAL's barycentric coefficients; triangles of nearly no area are skipped.
            let denom = (y2 - y3) * (x1 - x3) + (x3 - x2) * (y1 - y3);
            if denom.abs() < 1e-5 {
                continue;
            }
            let (m1x, m1y) = ((y2 - y3) / denom, (x3 - x2) / denom);
            let (m2x, m2y) = ((y3 - y1) / denom, (x1 - x3) / denom);
            let lo_i = (((x1.min(x2).min(x3) - x0) / res - 0.5).ceil().max(0.0)) as usize;
            let hi_i = (((x1.max(x2).max(x3) - x0) / res - 0.5).floor()).min(w as f64 - 1.0);
            let lo_j = (((y1.min(y2).min(y3) - y0) / res - 0.5).ceil().max(0.0)) as usize;
            let hi_j = (((y1.max(y2).max(y3) - y0) / res - 0.5).floor()).min(h as f64 - 1.0);
            if hi_i < 0.0 || hi_j < 0.0 {
                continue;
            }
            for j in lo_j..=hi_j as usize {
                for i in lo_i..=hi_i as usize {
                    let k = i + j * w;
                    if done[k] {
                        continue;
                    }
                    let (px, py) = center(i, j);
                    let l1 = m1x * (px - x3) + m1y * (py - y3);
                    let l2 = m2x * (px - x3) + m2y * (py - y3);
                    let l3 = 1.0 - l1 - l2;
                    let inside = |l: f64| (-EPS..=1.0 + EPS).contains(&l);
                    if inside(l1) && inside(l2) && inside(l3) {
                        out[k] = l1 * vs[a] + l2 * vs[b] + l3 * vs[c];
                        done[k] = true;
                    }
                }
            }
        }
    }
    // Nearest neighbours: every cell for the nearest method; outside the triangulation for the
    // linear one, unless its radius is 0. As GDAL: the nearest method, without a radius, takes
    // the nearest point; with one, the nearest within it (on more than 100 points, the nearest
    // of the points in the square of that half-side, which GDAL searches with a quadtree). The
    // linear method takes the nearest of the points in the square of half-side `radius`, or
    // without a radius, in the smallest square of half-side `r × 2ᵏ` that has points, `r` being
    // the points' typical spacing. Of points as near, the last.
    enum Search {
        All,
        Circle(f64),
        Square(f64),
        Growing(f64),
    }
    let initial = || {
        let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
        for (x, y) in xs.iter().zip(ys) {
            lo = [lo[0].min(*x), lo[1].min(*y)];
            hi = [hi[0].max(*x), hi[1].max(*y)];
        }
        ((hi[0] - lo[0]) * (hi[1] - lo[1]) / xs.len() as f64).sqrt()
    };
    let search = match (options.method, radius) {
        (GridMethod::Linear, Some(0.0)) => None,
        (GridMethod::Linear, Some(r)) if r > 0.0 => Some(Search::Square(r)),
        // Points with no extent in x or y give no first square: search them all.
        (GridMethod::Linear, _) => Some(match initial() {
            r if r > 0.0 && r.is_finite() => Search::Growing(r),
            _ => Search::All,
        }),
        (_, Some(r)) if r > 0.0 && xs.len() > 100 => Some(Search::Square(r)),
        (_, Some(r)) if r > 0.0 => Some(Search::Circle(r)),
        _ => Some(Search::All),
    };
    if let Some(search) = search {
        let tree = super::kdtree::KdTree::new(xs.iter().zip(ys).map(|(x, y)| [*x, *y]).collect());
        let todo: Vec<usize> = (0..w * h).filter(|k| !done[*k]).collect();
        let find = |k: usize| -> f64 {
            let (px, py) = center(k % w, k / w);
            let d2 = |i: u32| (xs[i as usize] - px).powi(2) + (ys[i as usize] - py).powi(2);
            let mut best: Option<(f64, u32)> = None;
            let mut consider = |i: u32| {
                let d = d2(i);
                if best.is_none_or(|b| d < b.0 || (d == b.0 && i > b.1)) {
                    best = Some((d, i));
                }
            };
            match search {
                Search::Square(r) => tree.within_box(&[px - r, py - r], &[px + r, py + r], &mut consider),
                Search::Growing(r0) => {
                    let mut r = r0;
                    let mut hit = false;
                    while !hit && r > 0.0 && r.is_finite() {
                        tree.within_box(&[px - r, py - r], &[px + r, py + r], &mut |i| {
                            hit = true;
                            consider(i)
                        });
                        r *= 2.0;
                    }
                }
                Search::Circle(r) => tree.within_box(&[px - r, py - r], &[px + r, py + r], &mut |i| {
                    if d2(i) <= r * r {
                        consider(i)
                    }
                }),
                Search::All => {
                    let mut found = Vec::new();
                    tree.knn(&[px, py], 1, &mut found);
                    if let Some(&(d, _)) = found.first() {
                        // Every point as near.
                        let r = d.sqrt() * (1.0 + 1e-9) + 1e-12;
                        tree.within_box(&[px - r, py - r], &[px + r, py + r], &mut |i| {
                            if d2(i) == d {
                                consider(i)
                            }
                        });
                    }
                }
            }
            best.map_or(f64::NAN, |(_, i)| vs[i as usize])
        };
        #[cfg(not(target_family = "wasm"))]
        let found: Vec<f64> = {
            use rayon::prelude::*;
            todo.par_iter().map(|k| find(*k)).collect()
        };
        #[cfg(target_family = "wasm")]
        let found: Vec<f64> = todo.iter().map(|k| find(*k)).collect();
        for (k, v) in todo.into_iter().zip(found) {
            out[k] = v;
        }
    }
    let mut data = Vec::with_capacity(w * h);
    push_rows(&mut data, &out, w, h, options);
    let name = if options.method == GridMethod::Linear { "linear" } else { "nearest" };
    raster(data, w, h, 1, x0, y0, crs, options, name.into())
}

/// The running statistics of each cell, as PDAL's `GDALGrid` keeps them.
struct Accumulator {
    w: usize,
    h: usize,
    count: Vec<f64>,
    min: Option<Vec<f64>>,
    max: Option<Vec<f64>>,
    mean: Option<Vec<f64>>,
    m2: Option<Vec<f64>>,
    idw: Option<Vec<f64>>,
    idw_dist: Option<Vec<f64>>,
}

impl Accumulator {
    fn new(w: usize, h: usize, stats: &[GridStat]) -> Self {
        let n = w * h;
        let has = |s: GridStat| stats.contains(&s);
        Self {
            w,
            h,
            count: vec![0.0; n],
            min: has(GridStat::Min).then(|| vec![f64::MAX; n]),
            max: has(GridStat::Max).then(|| vec![f64::MIN; n]),
            mean: (has(GridStat::Mean) || has(GridStat::Stdev)).then(|| vec![0.0; n]),
            m2: has(GridStat::Stdev).then(|| vec![0.0; n]),
            idw: has(GridStat::Idw).then(|| vec![0.0; n]),
            idw_dist: has(GridStat::Idw).then(|| vec![0.0; n]),
        }
    }

    fn update(&mut self, k: usize, v: f64, dist: f64, power: f64) {
        self.count[k] += 1.0;
        let count = self.count[k];
        if let Some(m) = &mut self.min {
            m[k] = m[k].min(v);
        }
        if let Some(m) = &mut self.max {
            m[k] = m[k].max(v);
        }
        if let Some(mean) = &mut self.mean {
            let delta = v - mean[k];
            mean[k] += delta / count;
            if let Some(m2) = &mut self.m2 {
                m2[k] += delta * (v - mean[k]);
            }
        }
        if let (Some(idw), Some(dsum)) = (&mut self.idw, &mut self.idw_dist)
            && !dsum[k].is_nan()
        {
            if dist == 0.0 {
                idw[k] = v;
                dsum[k] = f64::NAN;
            } else {
                let wgt = 1.0 / dist.powf(power);
                idw[k] += v * wgt;
                dsum[k] += wgt;
            }
        }
    }

    fn finalize(&mut self, window: usize) {
        let n = self.w * self.h;
        if let Some(m2) = &mut self.m2 {
            for (m, c) in m2.iter_mut().zip(&self.count) {
                if *c > 0.0 {
                    *m = (*m / c).sqrt();
                }
            }
        }
        if let (Some(idw), Some(dsum)) = (&mut self.idw, &self.idw_dist) {
            for k in 0..n {
                if self.count[k] > 0.0 && !dsum[k].is_nan() {
                    idw[k] /= dsum[k];
                }
            }
        }
        for k in 0..n {
            if self.count[k] == 0.0 {
                if window > 0 {
                    self.window_fill(k % self.w, k / self.w, window);
                } else {
                    self.set_nodata(k);
                }
            }
        }
    }

    fn bands_mut(&mut self) -> [Option<&mut Vec<f64>>; 5] {
        [self.min.as_mut(), self.max.as_mut(), self.mean.as_mut(), self.idw.as_mut(), self.m2.as_mut()]
    }

    fn set_nodata(&mut self, k: usize) {
        for b in self.bands_mut().into_iter().flatten() {
            b[k] = f64::NAN;
        }
    }

    /// Fills an empty cell with the inverse-distance-weighted values of the non-empty cells in a
    /// window around it.
    fn window_fill(&mut self, di: usize, dj: usize, window: usize) {
        let (w, h) = (self.w, self.h);
        let k = di + dj * w;
        let (i0, i1) = (di.saturating_sub(window), (di + window + 1).min(w));
        let (j0, j1) = (dj.saturating_sub(window), (dj + window + 1).min(h));
        let mut sources = Vec::new();
        for i in i0..i1 {
            for j in j0..j1 {
                let s = i + j * w;
                if (i == di && j == dj) || self.count[s] == 0.0 {
                    continue;
                }
                let d = (j.abs_diff(dj)).max(i.abs_diff(di)) as f64;
                sources.push((s, d));
            }
        }
        let dist_sum: f64 = sources.iter().map(|(_, d)| 1.0 / d).sum();
        if dist_sum <= 0.0 {
            self.set_nodata(k);
            return;
        }
        for b in self.bands_mut().into_iter().flatten() {
            let mut acc = 0.0;
            for (s, d) in &sources {
                acc += b[*s] / d;
            }
            b[k] = acc / dist_sum;
        }
    }

    fn band(&self, s: GridStat) -> &[f64] {
        let v = match s {
            GridStat::Min => &self.min,
            GridStat::Max => &self.max,
            GridStat::Mean => &self.mean,
            GridStat::Idw => &self.idw,
            GridStat::Stdev => &self.m2,
            GridStat::Count => return &self.count,
        };
        v.as_deref().unwrap_or(&[])
    }
}
