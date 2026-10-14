//! Gridding points into rasters (DEMs, DSMs, intensity or density grids), as PDAL's
//! `writers.gdal` does.
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

/// How to grid points.
#[derive(Debug, Clone, PartialEq)]
pub struct GridOptions {
    /// The cell size.
    pub resolution: f64,
    /// The statistics, one band each, written in PDAL's order (min, max, mean, idw, count,
    /// stdev). Empty means `mean`.
    pub stats: Vec<GridStat>,
    /// The dimension gridded. `z` by default.
    pub dimension: String,
    /// The distance from a cell centre within which points count. By default `resolution × √2`.
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

/// Grids the points of a cloud into a raster, one band per statistic.
pub fn grid(cloud: &PointCloud, options: &GridOptions) -> Result<RasterDataset> {
    let res = options.resolution;
    if !(res > 0.0 && res.is_finite()) {
        return Err(Error::InvalidArgument(format!("invalid resolution {res}")));
    }
    let mut stats = if options.stats.is_empty() { vec![GridStat::Mean] } else { options.stats.clone() };
    stats.sort();
    stats.dedup();
    let xs = cloud.coords("x").ok_or_else(|| Error::InvalidArgument("the points have no x".into()))?;
    let ys = cloud.coords("y").ok_or_else(|| Error::InvalidArgument("the points have no y".into()))?;
    let values = cloud
        .dim(&options.dimension)
        .ok_or_else(|| {
            Error::InvalidArgument(format!("no dimension '{}' (dimensions: {})", options.dimension, cloud.dimension_names().join(", ")))
        })?
        .to_f64();
    let bounds = match options.bounds {
        Some(b) => b,
        None => {
            let b = cloud.bounds().ok_or_else(|| Error::InvalidArgument("no points to grid".into()))?;
            [b[0], b[1], b[3], b[4]]
        }
    };
    let width = ((bounds[2] - bounds[0]) / res).floor() + 1.0;
    let height = ((bounds[3] - bounds[1]) / res).floor() + 1.0;
    if !(width >= 1.0 && height >= 1.0) || width * height > 1e10 {
        return Err(Error::InvalidArgument(format!("invalid grid of {width} x {height} cells")));
    }
    let (w, h) = (width as usize, height as usize);
    let (x0, y0) = (bounds[0], bounds[1]);
    let radius = options.radius.unwrap_or(res * std::f64::consts::SQRT_2);
    let mut g = Accumulator::new(w, h, &stats);
    let reach = (radius / res).ceil() as i64 + 1;
    for (i, v) in values.iter().enumerate() {
        let (x, y) = (xs[i], ys[i]);
        let ci = ((x - x0) / res).floor() as i64;
        let cj = ((y - y0) / res).floor() as i64;
        if options.bin_mode {
            if ci >= 0 && cj >= 0 && (ci as usize) < w && (cj as usize) < h {
                g.update(ci as usize + cj as usize * w, *v, 0.0, options.power);
            }
            continue;
        }
        for j in (cj - reach).max(0)..=(cj + reach).min(h as i64 - 1) {
            let cy = y0 + (j as f64 + 0.5) * res;
            for i2 in (ci - reach).max(0)..=(ci + reach).min(w as i64 - 1) {
                let cx = x0 + (i2 as f64 + 0.5) * res;
                let d = ((cx - x).powi(2) + (cy - y).powi(2)).sqrt();
                if d < radius {
                    g.update(i2 as usize + j as usize * w, *v, d, options.power);
                }
            }
        }
    }
    g.finalize(options.window_size);
    // Bands top row first: row r is cell row h - 1 - r.
    let nodata = options.nodata;
    let mut data = Vec::with_capacity(w * h * stats.len());
    for s in &stats {
        let band = g.band(*s);
        for r in 0..h {
            let j = h - 1 - r;
            for i in 0..w {
                let v = band[i + j * w];
                data.push(if v.is_nan() { nodata } else { crate::ops::convert_value(v, options.data_type) });
            }
        }
    }
    let crs = &cloud.crs;
    let cfg = RasterConfig {
        cols: w,
        rows: h,
        bands: stats.len(),
        x_min: x0,
        y_min: y0,
        cell_size: res,
        cell_size_y: Some(res),
        nodata,
        data_type: options.data_type,
        crs: wbraster::CrsInfo { epsg: crs.epsg, wkt: crs.to_wkt(), proj4: crs.proj4.clone() },
        metadata: vec![("BANDS".into(), stats.iter().map(|s| s.name()).collect::<Vec<_>>().join(","))],
    };
    Ok(RasterDataset::new(Raster::from_data(cfg, data)?, true))
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
