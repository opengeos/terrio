//! Bilinear geolocation-array transformer for scientific swaths.
//!
//! Coordinates are interpolated between valid array nodes. Invalid cells are never filled.
//! An R-tree indexes cells for inverse lookup, avoiding a scan of the whole swath per pixel.
use crate::{Error, Geolocation, RasterDataset, Result};
use rstar::{AABB, RTree, RTreeObject};
use std::sync::Arc;

#[derive(Debug, Clone)]
struct Cell {
    col: usize,
    row: usize,
    envelope: AABB<[f64; 2]>,
}
impl RTreeObject for Cell {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}

/// Indexed forward and inverse interpolation of a dataset's geolocation arrays.
#[derive(Debug, Clone)]
pub struct GeolocationTransformer {
    metadata: Geolocation,
    width: usize,
    height: usize,
    source_size: (usize, usize),
    x: Arc<[f64]>,
    y: Arc<[f64]>,
    cells: Arc<RTree<Cell>>,
    longitude: bool,
    anchor: f64,
}
fn interpolate(v: &[f64; 4], u: f64, vpos: f64) -> f64 {
    v[0] * (1.0 - u) * (1.0 - vpos) + v[1] * u * (1.0 - vpos) + v[2] * (1.0 - u) * vpos + v[3] * u * vpos
}
fn unwrap(x: f64, anchor: f64) -> f64 {
    anchor + (x - anchor + 180.0).rem_euclid(360.0) - 180.0
}
fn bounds(n: usize, count: usize, offset: f64, step: f64, size: usize, pixel_center: bool) -> (f64, f64) {
    let center = if pixel_center { 0.5 } else { 0.0 };
    let lo = if n == 0 { ((-offset) / step - center).min(0.0) } else { 0.0 };
    let hi = if n + 2 == count { ((size as f64 - offset) / step - center - n as f64).max(1.0) } else { 1.0 };
    (lo, hi)
}
impl GeolocationTransformer {
    /// Opens the coordinate rasters using Terrio's drivers, without GDAL.
    pub fn open(metadata: &Geolocation, width: usize, height: usize) -> Result<Self> {
        let x = crate::open_raster(&metadata.x_dataset)?;
        let y = crate::open_raster(&metadata.y_dataset)?;
        Self::new(metadata, &x, &y, width, height)
    }
    /// Builds an index from coordinate rasters already available in memory (also in WASM).
    /// `width` and `height` are the dimensions of the source image, not of the arrays.
    pub fn new(metadata: &Geolocation, x: &RasterDataset, y: &RasterDataset, width: usize, height: usize) -> Result<Self> {
        if !metadata.pixel_step.is_finite()
            || !metadata.line_step.is_finite()
            || metadata.pixel_step <= 0.0
            || metadata.line_step <= 0.0
            || !metadata.pixel_offset.is_finite()
            || !metadata.line_offset.is_finite()
        {
            return Err(Error::InvalidArgument("geolocation offsets must be finite and steps must be positive".into()));
        }
        if x.raster.cols < 2 || x.raster.rows < 2 || x.raster.cols != y.raster.cols || x.raster.rows != y.raster.rows {
            return Err(Error::InvalidArgument("geolocation arrays must have matching dimensions of at least 2 by 2".into()));
        }
        let read = |ds: &RasterDataset, band: usize| -> Result<Vec<f64>> {
            if band == 0 || band > ds.raster.bands {
                return Err(Error::InvalidArgument("geolocation band is out of range".into()));
            }
            let mut values = super::band_f64(&ds.raster, band - 1);
            for v in &mut values {
                if !v.is_finite() || (ds.has_nodata && super::is_nodata(*v, ds.raster.nodata)) {
                    *v = f64::NAN;
                }
            }
            Ok(values)
        };
        let mut xs = read(x, metadata.x_band)?;
        let mut ys = read(y, metadata.y_band)?;
        let srs =
            if metadata.srs.is_empty() { crate::crs::SpatialRef::from_epsg(4326)? } else { crate::crs::SpatialRef::parse(&metadata.srs)? };
        let longitude = srs.is_geographic();
        let anchor = xs
            .iter()
            .zip(&ys)
            .find(|(x, y)| x.is_finite() && y.is_finite() && (!longitude || (x.abs() <= 540.0 && y.abs() <= 90.0)))
            .map(|(x, _)| *x)
            .unwrap_or(0.0);
        for (x, y) in xs.iter_mut().zip(&mut ys) {
            if longitude && (y.abs() > 90.0 || x.abs() > 540.0) {
                *x = f64::NAN;
                *y = f64::NAN;
            }
            if longitude && x.is_finite() {
                *x = unwrap(*x, anchor);
            }
        }
        let (w, h) = (x.raster.cols, x.raster.rows);
        let mut cells = Vec::new();
        for row in 0..h - 1 {
            for col in 0..w - 1 {
                let ids = [row * w + col, row * w + col + 1, (row + 1) * w + col, (row + 1) * w + col + 1];
                let mut cx = ids.map(|i| xs[i]);
                let cy = ids.map(|i| ys[i]);
                if cx.iter().chain(cy.iter()).any(|v| !v.is_finite()) {
                    continue;
                }
                if longitude {
                    let base = cx[0];
                    for x in &mut cx {
                        *x = unwrap(*x, base);
                    }
                }
                let (u0, u1) = bounds(col, w, metadata.pixel_offset, metadata.pixel_step, width, metadata.pixel_center);
                let (v0, v1) = bounds(row, h, metadata.line_offset, metadata.line_step, height, metadata.pixel_center);
                let mut lo = [f64::INFINITY; 2];
                let mut hi = [f64::NEG_INFINITY; 2];
                for (u, v) in [(u0, v0), (u1, v0), (u0, v1), (u1, v1)] {
                    let p = [interpolate(&cx, u, v), interpolate(&cy, u, v)];
                    for k in 0..2 {
                        lo[k] = lo[k].min(p[k]);
                        hi[k] = hi[k].max(p[k]);
                    }
                }
                cells.push(Cell { col, row, envelope: AABB::from_corners(lo, hi) });
            }
        }
        if cells.is_empty() {
            return Err(Error::InvalidData("geolocation arrays have no valid cells".into()));
        }
        Ok(Self {
            metadata: metadata.clone(),
            width: w,
            height: h,
            source_size: (width, height),
            x: xs.into(),
            y: ys.into(),
            cells: Arc::new(RTree::bulk_load(cells)),
            longitude,
            anchor,
        })
    }
    /// The coordinate reference system declared by the arrays.
    pub fn srs(&self) -> &str {
        &self.metadata.srs
    }
    /// Pixel/line to the arrays' CRS. Pixel-center metadata adds half a geolocation-array step.
    pub fn to_geo(&self, pixel: f64, line: f64) -> Option<(f64, f64)> {
        let m = &self.metadata;
        let center = if m.pixel_center { 0.5 } else { 0.0 };
        let u = (pixel - m.pixel_offset) / m.pixel_step - center;
        let v = (line - m.line_offset) / m.line_step - center;
        if !u.is_finite() || !v.is_finite() {
            return None;
        }
        let col = u.floor().clamp(0.0, (self.width - 2) as f64) as usize;
        let row = v.floor().clamp(0.0, (self.height - 2) as f64) as usize;
        let ids = [row * self.width + col, row * self.width + col + 1, (row + 1) * self.width + col, (row + 1) * self.width + col + 1];
        let mut x = ids.map(|i| self.x[i]);
        let y = ids.map(|i| self.y[i]);
        if x.iter().chain(y.iter()).any(|x| !x.is_finite()) {
            return None;
        }
        if self.longitude {
            let a = x[0];
            for x in &mut x {
                *x = unwrap(*x, a);
            }
        }
        Some((interpolate(&x, u - col as f64, v - row as f64), interpolate(&y, u - col as f64, v - row as f64)))
    }
    /// Coordinates to pixel/line, solving the bilinear cell with Newton iteration.
    /// Returns `None` outside the swath or in invalid cells.
    pub fn to_pixel(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let x = if self.longitude { unwrap(x, self.anchor) } else { x };
        for shift in [0.0, -360.0, 360.0] {
            if shift != 0.0 && !self.longitude {
                continue;
            }
            let x = x + shift;
            for c in self.cells.locate_in_envelope_intersecting(AABB::from_point([x, y])) {
                let ids = [
                    c.row * self.width + c.col,
                    c.row * self.width + c.col + 1,
                    (c.row + 1) * self.width + c.col,
                    (c.row + 1) * self.width + c.col + 1,
                ];
                let mut cx = ids.map(|i| self.x[i]);
                let cy = ids.map(|i| self.y[i]);
                if self.longitude {
                    let base = cx[0];
                    for x in &mut cx {
                        *x = unwrap(*x, base);
                    }
                }
                // Normalize each axis to this cell's coordinate range before solving. This
                // keeps the Jacobian and residual tolerances independent of CRS units.
                let ox = cx[0];
                let oy = cy[0];
                let scale_x = cx.iter().map(|v| (v - ox).abs()).fold(0.0, f64::max);
                let scale_y = cy.iter().map(|v| (v - oy).abs()).fold(0.0, f64::max);
                if scale_x == 0.0 || scale_y == 0.0 {
                    continue;
                }
                let cx = cx.map(|v| (v - ox) / scale_x);
                let cy = cy.map(|v| (v - oy) / scale_y);
                let x = (x - ox) / scale_x;
                let y = (y - oy) / scale_y;
                let m = &self.metadata;
                let (u0, u1) = bounds(c.col, self.width, m.pixel_offset, m.pixel_step, self.source_size.0, m.pixel_center);
                let (v0, v1) = bounds(c.row, self.height, m.line_offset, m.line_step, self.source_size.1, m.pixel_center);
                let (mut u, mut v) = (0.5, 0.5);
                for _ in 0..20 {
                    let (ex, ey) = (interpolate(&cx, u, v) - x, interpolate(&cy, u, v) - y);
                    let dxdu = (cx[1] - cx[0]) * (1.0 - v) + (cx[3] - cx[2]) * v;
                    let dxdv = (cx[2] - cx[0]) * (1.0 - u) + (cx[3] - cx[1]) * u;
                    let dydu = (cy[1] - cy[0]) * (1.0 - v) + (cy[3] - cy[2]) * v;
                    let dydv = (cy[2] - cy[0]) * (1.0 - u) + (cy[3] - cy[1]) * u;
                    let det = dxdu * dydv - dxdv * dydu;
                    if !det.is_finite() || det.abs() <= 16.0 * f64::EPSILON * (dxdu.abs() + dydu.abs()) * (dxdv.abs() + dydv.abs()) {
                        break;
                    }
                    let du = (ex * dydv - ey * dxdv) / det;
                    let dv = (ey * dxdu - ex * dydu) / det;
                    u -= du;
                    v -= dv;
                    if du.abs() + dv.abs() < 1e-10 {
                        break;
                    }
                }
                if u >= u0 - 1e-8
                    && u <= u1 + 1e-8
                    && v >= v0 - 1e-8
                    && v <= v1 + 1e-8
                    && (interpolate(&cx, u, v) - x).abs() + (interpolate(&cy, u, v) - y).abs() < 1e-9
                {
                    let m = &self.metadata;
                    let center = if m.pixel_center { 0.5 } else { 0.0 };
                    return Some((
                        (c.col as f64 + u + center) * m.pixel_step + m.pixel_offset,
                        (c.row as f64 + v + center) * m.line_step + m.line_offset,
                    ));
                }
            }
        }
        None
    }
}
