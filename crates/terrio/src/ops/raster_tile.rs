//! Raster tile grids shared by directory tiling and retiling.
//!
//! Grid planning does not read pixels or enumerate every tile into memory. Coordinates
//! use top-left rows internally; TMS changes the stored row number, not georeferencing.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};

const MAX_ZOOM: u8 = 30;
const MAX_TILE_SIZE: usize = 16_384;

/// A global tile matrix set supported by the folder writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalScheme {
    /// EPSG:3857, one square tile at zoom zero.
    WebMercatorQuad,
    /// Longitude/latitude, two side-by-side tiles at zoom zero.
    WorldCrs84Quad,
}

/// Stored tile row numbering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Convention {
    /// Rows start at the north edge.
    #[default]
    Xyz,
    /// Rows start at the south edge.
    Tms,
}

/// Coordinate restrictions, expressed at the maximum zoom being generated.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TileLimits {
    /// First column, inclusive.
    #[serde(alias = "minX")]
    pub min_x: Option<u64>,
    /// Last column, inclusive.
    #[serde(alias = "maxX")]
    pub max_x: Option<u64>,
    /// First top-left row, inclusive.
    #[serde(alias = "minY")]
    pub min_y: Option<u64>,
    /// Last top-left row, inclusive.
    #[serde(alias = "maxY")]
    pub max_y: Option<u64>,
}

/// Options affecting the grid, independently of tile encoding and resampling.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GridOptions {
    /// Tile width and height in pixels.
    #[serde(alias = "tileSize")]
    pub tile_size: usize,
    /// First zoom to generate. By default, only the maximum zoom is generated.
    #[serde(alias = "minZoom")]
    pub min_zoom: Option<u8>,
    /// Last zoom to generate. By default, selected from the source resolution/size.
    #[serde(alias = "maxZoom")]
    pub max_zoom: Option<u8>,
    /// Restrict output coordinates at the maximum zoom.
    pub limits: TileLimits,
    /// Permit an empty plan when the extent/coordinate restrictions do not intersect.
    #[serde(alias = "noIntersectionOk")]
    pub no_intersection_ok: bool,
}

impl Default for GridOptions {
    fn default() -> Self {
        Self { tile_size: 256, min_zoom: None, max_zoom: None, limits: TileLimits::default(), no_intersection_ok: false }
    }
}

/// A north-up tile matrix; resolutions are positive and origins are top left.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TileMatrix {
    /// Zoom label.
    pub zoom: u8,
    /// Tile columns across the full matrix.
    pub columns: u64,
    /// Tile rows across the full matrix.
    pub rows: u64,
    /// Tile dimensions in pixels.
    pub tile_size: usize,
    /// Top-left coordinate in destination CRS units.
    pub origin: [f64; 2],
    /// Positive pixel resolutions in destination CRS units.
    pub resolution: [f64; 2],
}

/// Inclusive top-left tile coordinates for one zoom level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TileRange {
    /// First column.
    pub min_x: u64,
    /// Last column.
    pub max_x: u64,
    /// First top-left row.
    pub min_y: u64,
    /// Last top-left row.
    pub max_y: u64,
}

impl TileRange {
    /// Number of tiles, checked against integer overflow.
    pub fn count(self) -> Result<u64> {
        let width = self.max_x.checked_sub(self.min_x).and_then(|v| v.checked_add(1));
        let height = self.max_y.checked_sub(self.min_y).and_then(|v| v.checked_add(1));
        width.zip(height).and_then(|(w, h)| w.checked_mul(h)).ok_or_else(|| invalid("tile count overflow or reversed range"))
    }
}

/// One output grid and its stored filename coordinates.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TileCell {
    /// Zoom label.
    pub zoom: u8,
    /// Stored column.
    pub x: u64,
    /// Stored row, following the requested convention.
    pub y: u64,
    /// Output width and height in pixels.
    pub size: [usize; 2],
    /// Output affine transform in GDAL order.
    pub geo_transform: [f64; 6],
}

/// The matrix and selected tile coordinates of one output level.
#[derive(Debug, Clone, Serialize)]
pub struct TileLevel {
    /// Full matrix geometry.
    pub matrix: TileMatrix,
    /// Coordinates intersecting the source and requested limits.
    pub range: TileRange,
}

impl TileLevel {
    /// Iterate row by row without allocating tile descriptors for the whole level.
    pub fn cells(&self, convention: Convention) -> impl Iterator<Item = TileCell> + '_ {
        (self.range.min_y..=self.range.max_y).flat_map(move |row| {
            (self.range.min_x..=self.range.max_x).map(move |column| {
                let m = self.matrix;
                let y = match convention {
                    Convention::Xyz => row,
                    Convention::Tms => m.rows - 1 - row,
                };
                TileCell {
                    zoom: m.zoom,
                    x: column,
                    y,
                    size: [m.tile_size; 2],
                    geo_transform: [
                        m.origin[0] + column as f64 * m.tile_size as f64 * m.resolution[0],
                        m.resolution[0],
                        0.0,
                        m.origin[1] - row as f64 * m.tile_size as f64 * m.resolution[1],
                        0.0,
                        -m.resolution[1],
                    ],
                }
            })
        })
    }
}

/// A compact plan containing one descriptor per zoom, not per tile.
#[derive(Debug, Clone, Serialize)]
pub struct TilePlan {
    /// Selected levels, ordered from lowest to highest zoom.
    pub levels: Vec<TileLevel>,
}

impl TilePlan {
    /// A global matrix plan. `extent` and `source_resolution` are already in the
    /// matrix CRS; extent order is `[min_x, min_y, max_x, max_y]`.
    pub fn global(scheme: GlobalScheme, extent: [f64; 4], source_resolution: f64, options: &GridOptions) -> Result<Self> {
        validate_options(options)?;
        validate_extent(extent)?;
        if !source_resolution.is_finite() || source_resolution <= 0.0 {
            return Err(invalid("source resolution must be positive and finite"));
        }
        let (origin, world, base_columns) = match scheme {
            GlobalScheme::WebMercatorQuad => {
                let half = crate::tiles::MAX_GM;
                ([-half, half], [2.0 * half; 2], 1u64)
            }
            GlobalScheme::WorldCrs84Quad => ([-180.0, 90.0], [360.0, 180.0], 2),
        };
        let res0 = world[1] / options.tile_size as f64;
        let native = (res0.log2() - source_resolution.log2()).round().clamp(0.0, f64::from(MAX_ZOOM)) as u8;
        let (min_zoom, max_zoom) = zooms(options, native)?;
        let mut levels = Vec::new();
        for zoom in min_zoom..=max_zoom {
            let rows = 1u64 << zoom;
            let columns = base_columns * rows;
            let matrix = TileMatrix {
                zoom,
                columns,
                rows,
                tile_size: options.tile_size,
                origin,
                resolution: [world[0] / columns as f64 / options.tile_size as f64, world[1] / rows as f64 / options.tile_size as f64],
            };
            let right = origin[0] + world[0];
            let bottom = origin[1] - world[1];
            let clipped = [extent[0].max(origin[0]), extent[1].max(bottom), extent[2].min(right), extent[3].min(origin[1])];
            if clipped[0] >= clipped[2] || clipped[1] >= clipped[3] {
                continue;
            }
            let span = [matrix.resolution[0] * options.tile_size as f64, matrix.resolution[1] * options.tile_size as f64];
            let range = TileRange {
                min_x: snap((clipped[0] - origin[0]) / span[0]).floor().max(0.0) as u64,
                max_x: (snap((clipped[2] - origin[0]) / span[0]).ceil() as u64).saturating_sub(1).min(columns - 1),
                min_y: snap((origin[1] - clipped[3]) / span[1]).floor().max(0.0) as u64,
                max_y: (snap((origin[1] - clipped[1]) / span[1]).ceil() as u64).saturating_sub(1).min(rows - 1),
            };
            if let Some(range) = restrict(range, &options.limits, max_zoom - zoom) {
                levels.push(TileLevel { matrix, range });
            }
        }
        finish(levels, options)
    }

    /// A raster-adapted matrix. The maximum zoom always has the source resolution;
    /// an explicit maximum zoom changes its label. Lower levels use integer-decimated
    /// source dimensions, matching GDAL raster tiling and retiling.
    pub fn raster(width: usize, height: usize, gt: [f64; 6], options: &GridOptions) -> Result<Self> {
        validate_options(options)?;
        validate_grid(width, height, gt)?;
        let native = ((width.max(height) as f64 / options.tile_size as f64).log2().floor().max(0.0) as u8).min(MAX_ZOOM);
        let (min_zoom, max_zoom) = zooms(options, native)?;
        let mut levels = Vec::new();
        for zoom in min_zoom..=max_zoom {
            let factor = 1usize << (max_zoom - zoom);
            let columns = (width / factor).max(1).div_ceil(options.tile_size) as u64;
            let rows = (height / factor).max(1).div_ceil(options.tile_size) as u64;
            let matrix = TileMatrix {
                zoom,
                columns,
                rows,
                tile_size: options.tile_size,
                origin: [gt[0], gt[3]],
                resolution: [gt[1] * factor as f64, -gt[5] * factor as f64],
            };
            validate_matrix(matrix)?;
            let range = TileRange { min_x: 0, max_x: columns - 1, min_y: 0, max_y: rows - 1 };
            if let Some(range) = restrict(range, &options.limits, max_zoom - zoom) {
                levels.push(TileLevel { matrix, range });
            }
        }
        finish(levels, options)
    }

    /// The total number of output tiles, without enumerating them.
    pub fn count(&self) -> Result<u64> {
        self.levels.iter().try_fold(0u64, |sum, level| sum.checked_add(level.range.count()?).ok_or_else(|| invalid("tile count overflow")))
    }
}

/// A retiling grid retaining the raster's CRS and resolution, with cropped edge tiles.
#[derive(Debug, Clone)]
pub struct RetileGrid {
    width: usize,
    height: usize,
    gt: [f64; 6],
    tile_size: [usize; 2],
    overlap: usize,
    columns: usize,
    rows: usize,
    level: u8,
}

impl RetileGrid {
    /// Plan one pyramid level directly from the original raster grid, as gdal_retile
    /// does. Level zero is full resolution; each subsequent level doubles pixel size.
    pub fn new(width: usize, height: usize, gt: [f64; 6], tile_size: [usize; 2], overlap: usize, level: u8) -> Result<Self> {
        validate_grid(width, height, gt)?;
        if level > MAX_ZOOM || tile_size.iter().any(|&v| v == 0 || v > MAX_TILE_SIZE || overlap >= v) {
            return Err(invalid("invalid retile size, overlap or pyramid level"));
        }
        let factor = 1usize << level;
        let width = (width / factor).max(1);
        let height = (height / factor).max(1);
        let count = |n: usize, tile: usize| 1 + n.saturating_sub(tile).div_ceil(tile - overlap);
        let columns = count(width, tile_size[0]);
        let rows = count(height, tile_size[1]);
        (columns as u64).checked_mul(rows as u64).ok_or_else(|| invalid("tile count overflow"))?;
        let gt = [gt[0], gt[1] * factor as f64, 0.0, gt[3], 0.0, gt[5] * factor as f64];
        validate_grid(width, height, gt)?;
        Ok(Self { width, height, gt, tile_size, overlap, columns, rows, level })
    }

    /// Number of files at this level.
    pub fn count(&self) -> u64 {
        self.columns as u64 * self.rows as u64
    }

    /// Iterate cropped tiles row by row. `x` and `y` are zero-based column/row numbers.
    pub fn cells(&self) -> impl Iterator<Item = TileCell> + '_ {
        (0..self.rows).flat_map(move |row| {
            (0..self.columns).map(move |col| {
                let x = col * (self.tile_size[0] - self.overlap);
                let y = row * (self.tile_size[1] - self.overlap);
                TileCell {
                    zoom: self.level,
                    x: col as u64,
                    y: row as u64,
                    size: [self.tile_size[0].min(self.width - x), self.tile_size[1].min(self.height - y)],
                    geo_transform: [
                        self.gt[0] + x as f64 * self.gt[1],
                        self.gt[1],
                        0.0,
                        self.gt[3] + y as f64 * self.gt[5],
                        0.0,
                        self.gt[5],
                    ],
                }
            })
        })
    }
}

/// Pixel-buffer budgets for rendering a single tile. Decoder caches and already-loaded
/// caller-owned datasets are additional to these budgets.
#[derive(Debug, Clone, Copy)]
pub struct RenderLimits {
    /// Largest decoded source window requested from a lazy reader.
    pub max_source_window_bytes: usize,
    /// Largest output tile pixel buffer.
    pub max_output_bytes: usize,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self { max_source_window_bytes: 64 << 20, max_output_bytes: 256 << 20 }
    }
}

/// Render one tile using the windowed warp engine, retaining the source data type
/// unless `options.output_type` changes it. The cell overrides output grid controls.
///
/// This primitive renders one source; mosaicking, alpha and overview-generation
/// policy belong to the folder operation. Cutlines are refused because that warp
/// path loads a selected source level in full. Oversized lazy windows fail before
/// being read; source overviews or a finer destination grid can reduce them.
pub fn render_cell(
    source: &mut dyn super::warp::WarpSource,
    cell: &TileCell,
    options: &super::warp::WarpOptions,
    limits: RenderLimits,
) -> Result<crate::RasterDataset> {
    use super::warp::{WarpSource, WarpSourceInfo};
    validate_grid(cell.size[0], cell.size[1], cell.geo_transform)?;
    if cell.size.iter().any(|&v| v > MAX_TILE_SIZE) || limits.max_source_window_bytes == 0 || limits.max_output_bytes == 0 {
        return Err(invalid("invalid tile dimensions or rendering budgets"));
    }
    if options.cutline.is_some() || options.crop_to_cutline {
        return Err(Error::Unsupported("tile rendering does not load full cutline sources".into()));
    }
    // A shared loaded raster can supply these two scalars without a metadata/pixel
    // clone. The warp keeps that same borrowed-data fast path through the adapter.
    let mut shape = None;
    source.with_dataset(&mut |dataset| shape = Some((dataset.raster.bands, dataset.raster.data_type)))?;
    let (bands, dtype) = match shape {
        Some(shape) => shape,
        None => {
            let header = source.info()?.raster;
            (header.raster.bands, header.raster.data_type)
        }
    };
    if bands == 0 {
        return Err(invalid("the source has no bands"));
    }
    let bytes = |width: usize, height: usize, bands: usize, size: usize| {
        width.checked_mul(height).and_then(|n| n.checked_mul(bands)).and_then(|n| n.checked_mul(size))
    };
    let output_bytes = bytes(cell.size[0], cell.size[1], bands, options.output_type.unwrap_or(dtype).size_bytes());
    if output_bytes.is_none_or(|n| n > limits.max_output_bytes) {
        return Err(Error::Unsupported("the output tile exceeds the pixel-buffer budget".into()));
    }
    struct Bounded<'a> {
        inner: &'a mut dyn WarpSource,
        bands: usize,
        value_bytes: usize,
        limit: usize,
    }
    impl WarpSource for Bounded<'_> {
        fn with_dataset(&self, visit: &mut dyn FnMut(&crate::RasterDataset)) -> Result<bool> {
            self.inner.with_dataset(visit)
        }
        fn info(&mut self) -> Result<WarpSourceInfo> {
            self.inner.info()
        }
        fn read_window(&mut self, window: crate::Window, options: &crate::WindowOptions) -> Result<crate::RasterDataset> {
            let bands = if options.bands.is_empty() { self.bands } else { options.bands.len() };
            let needed =
                window.width.checked_mul(window.height).and_then(|n| n.checked_mul(bands)).and_then(|n| n.checked_mul(self.value_bytes));
            if needed.is_none_or(|n| n > self.limit) {
                return Err(Error::Unsupported(
                    "the source tile window exceeds the pixel-buffer budget; use source overviews or a finer grid".into(),
                ));
            }
            self.inner.read_window(window, options)
        }
    }
    let mut bounded = Bounded { inner: source, bands, value_bytes: dtype.size_bytes(), limit: limits.max_source_window_bytes };
    let mut options = options.clone();
    options.dst_transform = Some(cell.geo_transform);
    options.size = Some((cell.size[0], cell.size[1]));
    options.extent = None;
    options.extent_srs = None;
    options.resolution = None;
    options.target_aligned_pixels = false;
    super::warp::warp_sources(&mut [&mut bounded], &options)
}

fn invalid(message: &str) -> Error {
    Error::InvalidArgument(message.into())
}

fn validate_options(options: &GridOptions) -> Result<()> {
    if options.tile_size == 0 || options.tile_size > MAX_TILE_SIZE {
        return Err(invalid("tile size must be between 1 and 16384 pixels"));
    }
    let limits = &options.limits;
    if limits.min_x.zip(limits.max_x).is_some_and(|(a, b)| a > b) || limits.min_y.zip(limits.max_y).is_some_and(|(a, b)| a > b) {
        return Err(invalid("tile coordinate limits are reversed"));
    }
    Ok(())
}

fn zooms(options: &GridOptions, native: u8) -> Result<(u8, u8)> {
    let max = options.max_zoom.unwrap_or(native);
    let min = options.min_zoom.unwrap_or(max);
    if min > max || max > MAX_ZOOM {
        return Err(invalid("zoom range must satisfy 0 <= min <= max <= 30"));
    }
    Ok((min, max))
}

fn validate_extent(extent: [f64; 4]) -> Result<()> {
    if extent.iter().any(|v| !v.is_finite()) || extent[0] >= extent[2] || extent[1] >= extent[3] {
        return Err(invalid("extent must be finite and have positive width and height"));
    }
    Ok(())
}

fn validate_grid(width: usize, height: usize, gt: [f64; 6]) -> Result<()> {
    if width == 0 || height == 0 || gt.iter().any(|v| !v.is_finite()) || gt[1] <= 0.0 || gt[5] >= 0.0 || gt[2] != 0.0 || gt[4] != 0.0 {
        return Err(invalid("a nonempty north-up raster grid is required"));
    }
    validate_extent([gt[0], gt[3] + height as f64 * gt[5], gt[0] + width as f64 * gt[1], gt[3]])
}

fn validate_matrix(matrix: TileMatrix) -> Result<()> {
    let right = matrix.origin[0] + matrix.columns as f64 * matrix.tile_size as f64 * matrix.resolution[0];
    let bottom = matrix.origin[1] - matrix.rows as f64 * matrix.tile_size as f64 * matrix.resolution[1];
    validate_extent([matrix.origin[0], bottom, right, matrix.origin[1]])
}

fn snap(value: f64) -> f64 {
    let integer = value.round();
    if (value - integer).abs() <= 8.0 * f64::EPSILON * value.abs().max(1.0) { integer } else { value }
}

fn restrict(mut range: TileRange, limits: &TileLimits, shift: u8) -> Option<TileRange> {
    if let Some(v) = limits.min_x {
        range.min_x = range.min_x.max(v >> shift);
    }
    if let Some(v) = limits.max_x {
        range.max_x = range.max_x.min(v >> shift);
    }
    if let Some(v) = limits.min_y {
        range.min_y = range.min_y.max(v >> shift);
    }
    if let Some(v) = limits.max_y {
        range.max_y = range.max_y.min(v >> shift);
    }
    (range.min_x <= range.max_x && range.min_y <= range.max_y).then_some(range)
}

fn finish(levels: Vec<TileLevel>, options: &GridOptions) -> Result<TilePlan> {
    if levels.is_empty() && !options.no_intersection_ok {
        return Err(invalid("the input extent and requested tile coordinates do not intersect"));
    }
    let plan = TilePlan { levels };
    plan.count()?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    const GT: [f64; 6] = [10.0, 0.1, 0.0, 60.0, 0.0, -0.1];

    #[test]
    fn raster_grid_matches_gdal_313_native_and_decimated_levels() {
        for (width, height, zoom) in [(512, 512, 1), (300, 300, 0), (400, 400, 0), (1024, 256, 2), (256, 1024, 2), (1, 1024, 2), (1, 1, 0)]
        {
            let plan = TilePlan::raster(width, height, GT, &GridOptions::default()).unwrap();
            assert_eq!(plan.levels[0].matrix.zoom, zoom);
        }
        for (size, native, counts) in [(128, 2, vec![1, 4, 15]), (256, 1, vec![1, 6]), (512, 0, vec![2])] {
            let default = TilePlan::raster(513, 299, GT, &GridOptions { tile_size: size, ..Default::default() }).unwrap();
            assert_eq!(default.levels.len(), 1);
            assert_eq!(default.levels[0].matrix.zoom, native);
            let plan = TilePlan::raster(513, 299, GT, &GridOptions { tile_size: size, min_zoom: Some(0), ..Default::default() }).unwrap();
            assert_eq!(plan.levels.iter().map(|l| l.range.count().unwrap()).collect::<Vec<_>>(), counts);
            assert_eq!(plan.levels.last().unwrap().matrix.resolution, [0.1, 0.1]);
        }
    }

    #[test]
    fn explicit_raster_zoom_labels_native_resolution_and_tms_only_flips_names() {
        let plan = TilePlan::raster(513, 299, GT, &GridOptions { min_zoom: Some(0), max_zoom: Some(3), ..Default::default() }).unwrap();
        assert_eq!(plan.levels[0].matrix.resolution, [0.8, 0.8]);
        let level = plan.levels.last().unwrap();
        let xyz: Vec<_> = level.cells(Convention::Xyz).collect();
        let tms: Vec<_> = level.cells(Convention::Tms).collect();
        assert_eq!(xyz.len(), 6);
        assert_eq!((xyz[0].y, tms[0].y), (0, 1));
        for (a, b) in xyz.iter().zip(&tms) {
            assert_eq!(a.geo_transform, b.geo_transform);
        }
    }

    #[test]
    fn global_zoom_uses_geometric_nearest_and_edges_are_half_open() {
        for (resolution, zoom) in [(110_000.0, 1), (115_000.0, 0), (120_000.0, 0)] {
            let p = TilePlan::global(GlobalScheme::WebMercatorQuad, [0.0, 0.0, 1.0, 1.0], resolution, &Default::default()).unwrap();
            assert_eq!(p.levels[0].matrix.zoom, zoom);
        }
        let p = TilePlan::global(
            GlobalScheme::WorldCrs84Quad,
            [-180.0, 0.0, 0.0, 90.0],
            1.0,
            &GridOptions { max_zoom: Some(1), ..Default::default() },
        )
        .unwrap();
        assert_eq!(p.levels[0].range, TileRange { min_x: 0, max_x: 1, min_y: 0, max_y: 0 });
    }

    #[test]
    fn coordinate_restrictions_scale_from_maximum_zoom() {
        let options = GridOptions {
            min_zoom: Some(0),
            max_zoom: Some(3),
            limits: TileLimits { min_x: Some(4), max_x: Some(4), min_y: Some(2), max_y: Some(3) },
            ..Default::default()
        };
        let p = TilePlan::global(GlobalScheme::WorldCrs84Quad, [-180.0, -90.0, 180.0, 90.0], 1.0, &options).unwrap();
        assert_eq!(p.levels.last().unwrap().range.count().unwrap(), 2);
        assert_eq!(p.levels[0].range, TileRange { min_x: 0, max_x: 0, min_y: 0, max_y: 0 });
    }

    #[test]
    fn retile_crops_edges_and_preserves_overlap_at_each_level() {
        for (overlap, counts, final_size) in [(0, [15, 4, 1], [1, 43]), (16, [15, 6, 1], [65, 75])] {
            for level in 0..=2 {
                let grid = RetileGrid::new(513, 299, GT, [128, 128], overlap, level).unwrap();
                assert_eq!(grid.count(), counts[level as usize]);
                let last = grid.cells().last().unwrap();
                if level == 0 {
                    assert_eq!(last.size, final_size);
                }
                if level == 2 {
                    assert_eq!(last.size, [128, 74]);
                }
            }
        }
    }

    #[test]
    fn bad_grids_and_nonintersection_are_rejected_before_rendering() {
        assert!(TilePlan::raster(0, 1, GT, &Default::default()).is_err());
        assert!(RetileGrid::new(10, 10, GT, [128, 128], 128, 0).is_err());
        assert!(TilePlan::global(GlobalScheme::WorldCrs84Quad, [190.0, 0.0, 200.0, 1.0], 1.0, &Default::default()).is_err());
        let p = TilePlan::global(
            GlobalScheme::WorldCrs84Quad,
            [190.0, 0.0, 200.0, 1.0],
            1.0,
            &GridOptions { no_intersection_ok: true, ..Default::default() },
        )
        .unwrap();
        assert_eq!(p.count().unwrap(), 0);
        assert!(TilePlan::raster(2, 2, [f64::MAX, f64::MAX, 0.0, 1.0, 0.0, -1.0], &Default::default()).is_err());
        assert!(TileRange { min_x: 0, max_x: u64::MAX, min_y: 0, max_y: 1 }.count().is_err());
    }

    fn pixels(width: usize, height: usize, gt: [f64; 6], values: Vec<f64>) -> crate::RasterDataset {
        let config = wbraster::RasterConfig {
            cols: width,
            rows: height,
            bands: 1,
            x_min: gt[0],
            y_min: gt[3] + height as f64 * gt[5],
            cell_size: gt[1],
            cell_size_y: Some(-gt[5]),
            nodata: 65535.0,
            data_type: crate::DataType::U16,
            crs: wbraster::CrsInfo::from_epsg(3857),
            metadata: vec![],
        };
        crate::RasterDataset::new(wbraster::Raster::from_data(config, values).unwrap(), true)
    }

    struct Procedural {
        header: crate::RasterDataset,
        windows: Vec<crate::Window>,
    }
    impl Procedural {
        fn new() -> Self {
            let mut header = pixels(1, 1, [0.0, 1.0, 0.0, 1.0, 0.0, -1.0], vec![0.0]);
            header.raster.cols = 100_000;
            header.raster.rows = 100_000;
            header.raster.data = wbraster::raster::RasterData::new_filled(crate::DataType::U16, 0, 0.0);
            header.set_geo_transform([0.0, 1.0, 0.0, 100_000.0, 0.0, -1.0]);
            Self { header, windows: Vec::new() }
        }
    }
    impl super::super::warp::WarpSource for Procedural {
        fn info(&mut self) -> Result<super::super::warp::WarpSourceInfo> {
            Ok(super::super::warp::WarpSourceInfo { raster: self.header.clone(), overviews: Vec::new() })
        }
        fn read_window(&mut self, window: crate::Window, options: &crate::WindowOptions) -> Result<crate::RasterDataset> {
            assert!(options.overview.is_none());
            assert!(window.fits(100_000, 100_000));
            self.windows.push(window);
            let values =
                (0..window.height).flat_map(|y| (0..window.width).map(move |x| ((window.x + x + window.y + y) % 65500) as f64)).collect();
            Ok(pixels(window.width, window.height, [window.x as f64, 1.0, 0.0, 100_000.0 - window.y as f64, 0.0, -1.0], values))
        }
    }

    #[test]
    fn a_uint16_tile_reads_only_its_window_from_a_ten_billion_pixel_source() {
        let mut source = Procedural::new();
        let cell = TileCell { zoom: 0, x: 0, y: 0, size: [256, 128], geo_transform: [2000.0, 1.0, 0.0, 97000.0, 0.0, -1.0] };
        let out = render_cell(&mut source, &cell, &Default::default(), RenderLimits::default()).unwrap();
        assert_eq!(out.raster.data_type, crate::DataType::U16);
        assert_eq!(out.geo_transform(), cell.geo_transform);
        assert_eq!(out.nodata(), Some(65535.0));
        assert_eq!(out.raster.data.get_f64(0), 5000.0);
        assert_eq!(out.raster.data.get_f64(256 * 128 - 1), 5382.0);
        assert!(!source.windows.is_empty());
        assert!(source.windows.iter().map(|w| w.width * w.height).sum::<usize>() < 100_000);
    }

    #[test]
    fn rendering_budgets_stop_oversized_buffers_before_reading_pixels() {
        let cell = TileCell { zoom: 0, x: 0, y: 0, size: [256, 128], geo_transform: [2000.0, 1.0, 0.0, 97000.0, 0.0, -1.0] };
        for limits in
            [RenderLimits { max_source_window_bytes: 1, ..Default::default() }, RenderLimits { max_output_bytes: 1, ..Default::default() }]
        {
            let mut source = Procedural::new();
            assert!(render_cell(&mut source, &cell, &Default::default(), limits).is_err());
            assert!(source.windows.is_empty());
        }
    }

    #[test]
    fn already_loaded_tile_sources_are_borrowed_without_window_copies() {
        struct Loaded(crate::RasterDataset);
        impl super::super::warp::WarpSource for Loaded {
            fn with_dataset(&self, visit: &mut dyn FnMut(&crate::RasterDataset)) -> Result<bool> {
                visit(&self.0);
                Ok(true)
            }
            fn info(&mut self) -> Result<super::super::warp::WarpSourceInfo> {
                panic!("loaded source must not request cloned metadata")
            }
            fn read_window(&mut self, _: crate::Window, _: &crate::WindowOptions) -> Result<crate::RasterDataset> {
                panic!("loaded source must not copy windows")
            }
        }
        let mut source = Loaded(pixels(4, 4, [0.0, 1.0, 0.0, 4.0, 0.0, -1.0], (0..16).map(f64::from).collect()));
        let cell = TileCell { zoom: 0, x: 0, y: 0, size: [2, 2], geo_transform: [1.0, 1.0, 0.0, 3.0, 0.0, -1.0] };
        let out = render_cell(&mut source, &cell, &Default::default(), RenderLimits::default()).unwrap();
        assert_eq!(out.raster.data.to_f64_vec(), [5.0, 6.0, 9.0, 10.0]);
    }
}

mod folder;
pub use folder::*;
