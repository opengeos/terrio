//! Zone membership on one raster grid, computed once and shared by every band of every raster on
//! that grid: each zone's pixels as row spans, with their coverage fractions.

use super::super::rasterize::{Grid, RasterizeOptions, rasterize};
use crate::crs::{SpatialRef, Transformer};
use crate::{DataType, Error, Feature, Layer, LazyRaster, Result};

/// A raster grid: zones computed for one grid apply to any raster with an equal key.
#[derive(Debug, Clone)]
pub(super) struct GridKey {
    pub cols: usize,
    pub rows: usize,
    pub geo_transform: [f64; 6],
    pub crs: SpatialRef,
}

impl GridKey {
    pub fn of(raster: &LazyRaster) -> Self {
        Self { cols: raster.width(), rows: raster.height(), geo_transform: raster.geo_transform(), crs: raster.crs() }
    }

    /// Same dimensions, geotransform and CRS (or both CRSs unknown).
    pub fn same(&self, other: &Self) -> bool {
        self.cols == other.cols
            && self.rows == other.rows
            && self.geo_transform == other.geo_transform
            && ((self.crs.is_unknown() && other.crs.is_unknown()) || self.crs.same_as(&other.crs))
    }
}

/// `len` pixels of one row from column `col`, each covered by the fraction `coverage` (1 in
/// binary modes). Fractional partial cells are spans of one pixel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Span {
    pub row: u32,
    pub col: u32,
    pub len: u32,
    pub coverage: f64,
}

/// One feature's pixels: spans in row-major order within rows `row_start..row_end`.
#[derive(Debug, Clone)]
pub(super) struct Zone {
    pub feature: usize,
    pub row_start: usize,
    pub row_end: usize,
    pub spans: Vec<Span>,
}

impl Zone {
    /// The zone's pixel count, in 64 bits: a zone can cover more than 2^32 pixels of a 32-bit
    /// target's grid.
    pub fn pixels(&self) -> u64 {
        self.spans.iter().map(|s| u64::from(s.len)).sum()
    }
}

/// The zones of a layer on one grid, sorted by first row. Features with null or empty geometry,
/// or no pixel in the grid, have no zone.
#[derive(Debug)]
pub(super) struct ZoneIndex {
    pub zones: Vec<Zone>,
}

/// Builds the zones of `layer` on `grid`, transforming boundaries from the layer's CRS.
pub(super) fn build(layer: &Layer, grid: &GridKey, all_touched: bool, fractional: bool) -> Result<ZoneIndex> {
    let src = SpatialRef::from_layer(layer);
    let needs_transform = match (src.is_unknown(), grid.crs.is_unknown()) {
        (true, true) => false,
        (true, false) | (false, true) => {
            return Err(Error::Crs("zones and raster must both declare a CRS, or both have no CRS".into()));
        }
        _ => !src.same_as(&grid.crs),
    };
    if needs_transform {
        Transformer::new(&src, &grid.crs)?;
    }
    // Spans hold rows and columns as u32.
    if grid.cols > u32::MAX as usize || grid.rows > u32::MAX as usize {
        return Err(Error::InvalidArgument(format!("zonal statistics take grids of at most {} rows and columns", u32::MAX)));
    }
    let inv = super::super::invert_gt(&grid.geo_transform).ok_or_else(|| Error::InvalidArgument("cannot invert the raster grid".into()))?;
    let indices: Vec<usize> = (0..layer.features.len()).collect();
    let zones = crate::par::map_init(
        &indices,
        || needs_transform.then(|| Transformer::new(&src, &grid.crs)).transpose(),
        |transform, &i| -> Result<Option<Zone>> {
            let transform = transform.as_ref().map_err(|e| Error::Crs(e.to_string()))?.as_ref();
            let f = &layer.features[i];
            zone(f, i, grid, &inv, transform, all_touched, fractional).map_err(|e| Error::InvalidArgument(format!("zone {}: {e}", f.fid)))
        },
    );
    let mut zones: Vec<Zone> = zones.into_iter().filter_map(Result::transpose).collect::<Result<_>>()?;
    zones.sort_by_key(|z| (z.row_start, z.feature));
    Ok(ZoneIndex { zones })
}

fn zone(
    f: &Feature,
    feature: usize,
    grid: &GridKey,
    inv: &[f64; 6],
    transform: Option<&Transformer>,
    all_touched: bool,
    fractional: bool,
) -> Result<Option<Zone>> {
    let Some(geometry) = f.geometry.as_ref().filter(|g| !g.is_empty()) else { return Ok(None) };
    let mut geometry = geometry.clone();
    crate::vector::map_coords(&mut geometry, &mut |c| {
        let (x, y) = match transform {
            Some(t) => t.transform(c.x, c.y)?,
            None => (c.x, c.y),
        };
        let (col, row) = super::super::apply_gt(inv, x, y);
        if !col.is_finite() || !row.is_finite() {
            return Err(Error::Crs("zone boundary could not be projected into the raster grid".into()));
        }
        c.x = col;
        c.y = -row;
        Ok(())
    })?;
    if fractional {
        super::super::topology::require_valid(&geometry, f.fid)?;
    }
    let Some(bbox) = geometry.bbox() else { return Ok(None) };
    let col = (bbox.min_x.floor() - 1.0).clamp(0.0, grid.cols as f64) as usize;
    let row = ((-bbox.max_y).floor() - 1.0).clamp(0.0, grid.rows as f64) as usize;
    let end_col = (bbox.max_x.ceil() + 1.0).clamp(0.0, grid.cols as f64) as usize;
    let end_row = ((-bbox.min_y).ceil() + 1.0).clamp(0.0, grid.rows as f64) as usize;
    if col >= end_col || row >= end_row {
        return Ok(None);
    }
    let (cols, rows) = (end_col - col, end_row - row);
    // A pixel mask becomes spans as it is: a byte per pixel of the zone's bounding box.
    let spans = if fractional {
        spans(&super::fractional::coverage(&geometry, col, row, cols, rows), col, row, cols)
    } else {
        let mut layer = Layer::new("zone");
        layer.features.push(Feature { fid: 1, geometry: Some(geometry), attributes: Vec::new() });
        let options = RasterizeOptions {
            burn_values: vec![1.0],
            data_type: Some(DataType::U8),
            all_touched,
            grid: Some(Grid { cols, rows, geo_transform: [col as f64, 1.0, 0.0, -(row as f64), 0.0, -1.0], crs: SpatialRef::default() }),
            ..Default::default()
        };
        let mask = rasterize(&[layer], &options)?;
        let crate::raster::RasterData::U8(pixels) = mask.raster.data else { unreachable!() };
        spans(&pixels, col, row, cols)
    };
    if spans.is_empty() {
        return Ok(None);
    }
    let row_start = spans[0].row as usize;
    let row_end = spans.last().map_or(row_start, |s| s.row as usize + 1);
    Ok(Some(Zone { feature, row_start, row_end, spans }))
}

/// Row-major spans of a cropped coverage window at (`col`, `row`): runs of full coverage merged,
/// each partial cell alone, uncovered cells left out.
fn spans<T: Copy + Into<f64>>(coverage: &[T], col: usize, row: usize, cols: usize) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (r, line) in coverage.chunks(cols).enumerate() {
        for (c, v) in line.iter().enumerate() {
            let v: f64 = (*v).into();
            if v == 0.0 {
                continue;
            }
            let (row, col) = ((row + r) as u32, (col + c) as u32);
            if let Some(last) = spans.last_mut().filter(|s| v == 1.0 && s.coverage == 1.0 && s.row == row && s.col + s.len == col) {
                last.len += 1;
            } else {
                spans.push(Span { row, col, len: 1, coverage: v });
            }
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::{Span, spans};

    #[test]
    fn spans_merge_full_runs_and_keep_partial_cells_alone() {
        let coverage = [0., 1., 1., 0.5, 1., 1., 0., 0., 0.25];
        let got = spans(&coverage, 10, 20, 3);
        let s = |row, col, len, coverage| Span { row, col, len, coverage };
        assert_eq!(got, [s(20, 11, 2, 1.), s(21, 10, 1, 0.5), s(21, 11, 2, 1.), s(22, 12, 1, 0.25)]);
    }
}
