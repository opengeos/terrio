//! Bounded raster retiling and recursive pyramids. Reference: GDAL 3.13.3
//! `gdal_retile`: top-origin one-based filenames, row directories, overlap and
//! integer-decimated pyramid dimensions. Later sources overwrite earlier ones,
//! including nodata; physical dataset masks are not copied by this operation.
//! Independently implemented to match GDAL behavior, without translating its code.

mod pyramid;

use super::raster_tile::{FolderSink, RenderLimits, TileCell, TileReport, render_cell};
use super::warp::{Nodata, WarpOptions, WarpSource, WarpSourceInfo};
use super::{Grid, Resampling};
use crate::crs::SpatialRef;
use crate::{Error, RasterDataset, RasterWriteOptions, Result, Window, WindowOptions};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use wbraster::raster::RasterData;

/// The source name supplies the default filename prefix, never a persisted URL.
pub struct RetileSource<'a> {
    /// A filename or label. URL query strings/fragments are removed from the prefix.
    pub name: String,
    /// An open reader; source windows remain lazy. Metadata and pixels must stay
    /// stable throughout generation, including across output callbacks.
    pub source: &'a mut dyn WarpSource,
}

/// Retile options shared by native and callback sinks.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetileOptions {
    /// Output pixel width and height, default 256 by 256.
    #[serde(alias = "tileSize")]
    pub tile_size: [usize; 2],
    /// Shared pixels between adjacent tiles; less than both tile dimensions.
    pub overlap: usize,
    /// Additional pyramid levels; base level is zero.
    pub levels: u8,
    /// Omit the base level.
    #[serde(alias = "pyramidOnly")]
    pub pyramid_only: bool,
    /// Store each level/row in its own directory, including level zero.
    #[serde(alias = "rowDirectories")]
    pub row_directories: bool,
    /// Optional output filename prefix, otherwise the first source's stem.
    pub prefix: Option<String>,
    /// GTiff or COG. Both preserve affine georeferencing and native sample types.
    pub format: String,
    /// Optional GDAL-style sample type.
    #[serde(alias = "outputType")]
    pub output_type: Option<String>,
    /// near/nearest, bilinear, cubic, cubicspline or lanczos for pyramids.
    pub resampling: String,
    /// Assign the source CRS; does not reproject coordinates.
    #[serde(alias = "sourceSrs")]
    pub source_srs: Option<String>,
    /// Raster writer creation options.
    #[serde(alias = "creationOptions")]
    pub creation_options: BTreeMap<String, String>,
    /// Per-level CSV basename, with GDAL's five georeferencing columns.
    pub csv: Option<String>,
    /// Single-character CSV delimiter.
    #[serde(alias = "csvDelimiter")]
    pub csv_delimiter: String,
    /// Per-level tile index basename (.shp, .geojson or .gpkg).
    #[serde(alias = "tileIndex")]
    pub tile_index: Option<String>,
    /// Index attribute carrying the filename relative to the level directory.
    #[serde(alias = "tileIndexField")]
    pub tile_index_field: String,
    /// Bound output work before reading pixels.
    #[serde(alias = "maxTiles")]
    pub max_tiles: u64,
}
impl Default for RetileOptions {
    fn default() -> Self {
        Self {
            tile_size: [256, 256],
            overlap: 0,
            levels: 0,
            pyramid_only: false,
            row_directories: false,
            prefix: None,
            format: "GTiff".into(),
            output_type: None,
            resampling: "nearest".into(),
            source_srs: None,
            creation_options: BTreeMap::new(),
            csv: None,
            csv_delimiter: ";".into(),
            tile_index: None,
            tile_index_field: "location".into(),
            max_tiles: 1_000_000,
        }
    }
}

/// One recursive pyramid level; size excludes trailing incomplete samples.
#[derive(Debug, Clone, Serialize)]
pub struct RetileLevel {
    /// Pyramid number, with zero for the base grid.
    pub level: u8,
    /// Pixel width and height at this level.
    pub size: [usize; 2],
    /// Number of output tile columns.
    pub columns: usize,
    /// Number of output tile rows.
    pub rows: usize,
    /// Top-left origin and level pixel resolution.
    pub geo_transform: [f64; 6],
}
/// Compact metadata plan; no per-tile records or source pixels.
#[derive(Debug, Clone, Serialize)]
pub struct RetilePlan {
    /// Validated output filename stem.
    pub prefix: String,
    /// Declared coordinate reference system, if known.
    pub crs: Option<String>,
    /// Maximum output tile width and height.
    pub tile_size: [usize; 2],
    /// Overlap between adjacent tiles in pixels.
    pub overlap: usize,
    /// Whether each level has row subdirectories.
    pub row_directories: bool,
    /// Selected output levels, in increasing order.
    pub levels: Vec<RetileLevel>,
    /// Total number of image tiles.
    pub planned: u64,
}
impl RetilePlan {
    /// Tiles, row by row, with the level-relative name and grid.
    pub fn cells<'a>(&'a self, level: &'a RetileLevel) -> impl Iterator<Item = (String, TileCell)> + 'a {
        (0..level.rows).flat_map(move |row| (0..level.columns).map(move |column| self.cell(level, row, column)))
    }
    fn cell(&self, level: &RetileLevel, row: usize, column: usize) -> (String, TileCell) {
        let digits = level.columns.max(level.rows).to_string().len();
        let x = column * (self.tile_size[0] - self.overlap);
        let y = row * (self.tile_size[1] - self.overlap);
        let name = format!("{}_{:0digits$}_{:0digits$}.tif", self.prefix, row + 1, column + 1);
        let name = if self.row_directories { format!("{}/{name}", row + 1) } else { name };
        let g = level.geo_transform;
        (
            name,
            TileCell {
                zoom: level.level,
                x: column as u64,
                y: row as u64,
                size: [self.tile_size[0].min(level.size[0] - x), self.tile_size[1].min(level.size[1] - y)],
                geo_transform: [g[0] + x as f64 * g[1], g[1], 0., g[3] + y as f64 * g[5], 0., g[5]],
            },
        )
    }
    fn directory(&self, level: u8) -> String {
        if level == 0 && !self.row_directories { String::new() } else { format!("{level}/") }
    }
}
fn invalid(message: &str) -> Error {
    Error::InvalidArgument(message.into())
}
fn component(name: &str) -> Result<()> {
    if name.len() > 128
        || name.is_empty()
        || name.contains(['/', '\\', '\0', ':'])
        || matches!(name, "." | "..")
        || name.starts_with(".terrio-")
    {
        return Err(invalid("retile filenames must be nonempty single path components"));
    }
    Ok(())
}
fn prefix(name: &str) -> String {
    let name = name.split(['?', '#']).next().unwrap_or(name).rsplit(['/', '\\']).next().unwrap_or(name);
    std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).unwrap_or("tile").to_string()
}
fn extent(ds: &RasterDataset) -> [f64; 4] {
    let g = ds.geo_transform();
    [g[0], g[3] + ds.raster.rows as f64 * g[5], g[0] + ds.raster.cols as f64 * g[1], g[3]]
}
fn count(size: usize, tile: usize, overlap: usize) -> usize {
    size.saturating_sub(overlap).div_ceil(tile - overlap).max(1)
}

/// Validate every source and option using metadata only.
pub fn plan(sources: &mut [RetileSource<'_>], options: &RetileOptions) -> Result<RetilePlan> {
    Ok(prepare(sources, options)?.0)
}
fn prepare(sources: &mut [RetileSource<'_>], o: &RetileOptions) -> Result<(RetilePlan, Vec<RasterDataset>)> {
    if sources.is_empty()
        || o.tile_size.iter().any(|&n| n == 0 || n > 16384 || o.overlap >= n)
        || o.levels > 30
        || o.max_tiles == 0
        || o.pyramid_only && o.levels == 0
    {
        return Err(invalid("invalid source count, tile dimensions, overlap, pyramid levels or tile limit"));
    }
    let name = o.prefix.clone().unwrap_or_else(|| prefix(&sources[0].name));
    component(&name)?;
    let method = Resampling::parse(&o.resampling)?;
    if !matches!(method, Resampling::Nearest | Resampling::Bilinear | Resampling::Cubic | Resampling::CubicSpline | Resampling::Lanczos) {
        return Err(invalid("retile resampling must be nearest, bilinear, cubic, cubicspline or lanczos"));
    }
    if !matches!(o.format.to_ascii_lowercase().as_str(), "gtiff" | "tiff" | "tif" | "cog") {
        return Err(invalid("retile supports GTiff and COG output"));
    }
    if let Some(dtype) = &o.output_type {
        super::parse_data_type(dtype)?;
    }
    if o.csv_delimiter.chars().count() != 1 || o.csv_delimiter.contains(['\r', '\n', '"', '\0']) {
        return Err(invalid("CSV delimiter must be one non-quote, non-newline character"));
    }
    if let Some(csv) = &o.csv {
        component(csv)?;
        if !csv.to_ascii_lowercase().ends_with(".csv") {
            return Err(invalid("CSV filename must end in .csv"));
        }
    }
    if let Some(index) = &o.tile_index {
        component(index)?;
        let ext = index.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        if !matches!(ext.as_str(), "shp" | "geojson" | "gpkg") {
            return Err(invalid("tile indexes support .shp, .geojson or .gpkg"));
        }
        if ext == "shp" && o.tile_index_field.len() > 10 {
            return Err(invalid("Shapefile tile index fields must fit in 10 bytes"));
        }
    }
    if o.tile_index_field.is_empty() || o.tile_index_field.contains('\0') {
        return Err(invalid("tile index field must be nonempty"));
    }
    let assigned = o.source_srs.as_ref().map(|s| SpatialRef::parse(s)).transpose()?;
    let mut headers = Vec::new();
    for input in sources.iter_mut() {
        let mut h = input.source.info()?.raster;
        h.raster.data = RasterData::new_filled(h.raster.data_type, 0, 0.);
        if let Some(srs) = &assigned {
            h.raster.crs = srs.to_crs_info();
        }
        headers.push(h);
    }
    let first = &headers[0];
    let mut bounds = extent(first);
    let first_gt = first.geo_transform();
    for h in &headers {
        let g = h.geo_transform();
        let b = extent(h);
        if h.raster.cols == 0
            || h.raster.rows == 0
            || h.raster.bands == 0
            || h.complex
            || !h.gcps.is_empty()
            || h.geolocation.is_some()
            || !g.iter().all(|v| v.is_finite())
            || g[1] <= 0.
            || g[5] >= 0.
            || g[2] != 0.
            || g[4] != 0.
        {
            return Err(invalid("retile requires nonempty, north-up affine, non-complex inputs"));
        }
        if h.raster.bands != first.raster.bands
            || h.crs().is_unknown() != first.crs().is_unknown()
            || (!h.crs().is_unknown() && !h.crs().same_as(&first.crs()))
        {
            return Err(invalid("retile sources must have matching band counts and CRS"));
        }
        bounds = [bounds[0].min(b[0]), bounds[1].min(b[1]), bounds[2].max(b[2]), bounds[3].max(b[3])];
    }
    let dtype = o.output_type.as_ref().map(|s| super::parse_data_type(s)).transpose()?.unwrap_or(first.raster.data_type);
    if o.tile_size[0]
        .checked_mul(o.tile_size[1])
        .and_then(|n| n.checked_mul(first.raster.bands))
        .and_then(|n| n.checked_mul(dtype.size_bytes()))
        .is_none_or(|n| n > RenderLimits::default().max_output_bytes)
    {
        return Err(invalid("retile output tile exceeds its pixel-buffer budget"));
    }
    let dimensions = [(bounds[2] - bounds[0]) / first_gt[1], (bounds[3] - bounds[1]) / -first_gt[5]];
    if dimensions.iter().any(|v| !v.is_finite() || *v < 0.5 || *v > usize::MAX as f64 / 2.) {
        return Err(invalid("retile mosaic dimensions exceed the address space"));
    }
    let size = [dimensions[0].round() as usize, dimensions[1].round() as usize];
    let mut levels = Vec::new();
    let mut planned = 0u64;
    for level in u8::from(o.pyramid_only)..=o.levels {
        let factor = 1usize << level;
        let size = [size[0] / factor, size[1] / factor];
        if size.contains(&0) {
            return Err(invalid("retile pyramid level would have a zero dimension"));
        }
        let columns = count(size[0], o.tile_size[0], o.overlap);
        let rows = count(size[1], o.tile_size[1], o.overlap);
        let n = (columns as u64).checked_mul(rows as u64).ok_or_else(|| invalid("retile count overflow"))?;
        planned = planned.checked_add(n).filter(|n| *n <= o.max_tiles).ok_or_else(|| invalid("retile plan exceeds max_tiles"))?;
        if (o.csv.is_some() || o.tile_index.is_some()) && n > 100_000 {
            return Err(invalid("per-level manifests are limited to 100,000 tile records"));
        }
        let g = [bounds[0], first_gt[1] * factor as f64, 0., bounds[3], 0., first_gt[5] * factor as f64];
        if !g.iter().all(|v| v.is_finite()) {
            return Err(invalid("retile level resolution overflow"));
        }
        levels.push(RetileLevel { level, size, columns, rows, geo_transform: g });
    }
    let crs = first.crs();
    let crs = crs.authority_code().or_else(|| crs.to_wkt()).or(crs.proj4);
    let plan =
        RetilePlan { prefix: name, crs, tile_size: o.tile_size, overlap: o.overlap, row_directories: o.row_directories, levels, planned };
    if o.csv.is_some() {
        for level in &plan.levels {
            csv_text(plan.cells(level), o)?;
        }
    }
    Ok((plan, headers))
}

/// Generate tiles and optional per-level manifests without retaining encoded images.
pub fn generate(sources: &mut [RetileSource<'_>], o: &RetileOptions, sink: &mut dyn FolderSink) -> Result<TileReport> {
    let (plan, headers) = prepare(sources, o)?;
    let first = &headers[0];
    let bounds = headers.iter().map(extent).collect::<Vec<_>>();
    let mut base = first.clone();
    let g = plan.levels[0].geo_transform;
    let factor = (1usize << plan.levels[0].level) as f64;
    let minx = bounds.iter().map(|b| b[0]).fold(f64::INFINITY, f64::min);
    let maxx = bounds.iter().map(|b| b[2]).fold(f64::NEG_INFINITY, f64::max);
    let miny = bounds.iter().map(|b| b[1]).fold(f64::INFINITY, f64::min);
    let maxy = bounds.iter().map(|b| b[3]).fold(f64::NEG_INFINITY, f64::max);
    let base_gt = [minx, g[1] / factor, 0., maxy, 0., g[5] / factor];
    base.raster.cols = ((maxx - minx) / base_gt[1]).round() as usize;
    base.raster.rows = ((maxy - miny) / -base_gt[5]).round() as usize;
    base.set_geo_transform(base_gt);
    base.band_nodata.clear();
    let index = RTree::bulk_load(
        bounds
            .iter()
            .enumerate()
            .map(|(i, b)| GeomWithData::new(Rectangle::from_aabb(AABB::from_corners([b[0], b[1]], [b[2], b[3]])), i))
            .collect(),
    );
    let mut mosaic = Mosaic { sources, headers: &headers, base, index };
    let mut cache = pyramid::Cache::default();
    let mut report = TileReport { planned: plan.planned, ..Default::default() };
    for level in &plan.levels {
        let directory = plan.directory(level.level);
        let mut records = Vec::new();
        for (relative, cell) in plan.cells(level) {
            let name = format!("{directory}{relative}");
            if sink.exists(&name)? {
                report.resumed += 1;
            } else {
                let bytes = pyramid::encoded(&mut mosaic, &plan, o, &mut cache, sink, &cell)?;
                sink.put(&name, &bytes)?;
                report.written += 1;
            }
            if o.csv.is_some() || o.tile_index.is_some() {
                records.push((relative, cell));
            }
        }
        manifests(&plan, &directory, &records, o, sink)?;
    }
    Ok(report)
}

struct Mosaic<'a, 's> {
    sources: &'a mut [RetileSource<'s>],
    headers: &'a [RasterDataset],
    base: RasterDataset,
    index: RTree<GeomWithData<Rectangle<[f64; 2]>, usize>>,
}
/// Raw retile copies intentionally ignore physical masks and nodata validity,
/// matching GDAL's ReadRaster/WriteRaster overdraw. The final pyramid warp applies
/// the mosaic's declared nodata while resampling.
struct Raw<'a> {
    source: &'a mut dyn WarpSource,
    crs: SpatialRef,
}
impl WarpSource for Raw<'_> {
    fn has_mask(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn info(&mut self) -> Result<WarpSourceInfo> {
        let mut i = self.source.info()?;
        i.raster.raster.crs = self.crs.to_crs_info();
        Ok(i)
    }
    fn read_window(&mut self, w: Window, o: &WindowOptions) -> Result<RasterDataset> {
        let mut d = self.source.read_window(w, o)?;
        d.raster.crs = self.crs.to_crs_info();
        Ok(d)
    }
}
impl WarpSource for Mosaic<'_, '_> {
    fn has_mask(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn info(&mut self) -> Result<WarpSourceInfo> {
        Ok(WarpSourceInfo { raster: self.base.clone(), overviews: Vec::new() })
    }
    fn read_window(&mut self, w: Window, o: &WindowOptions) -> Result<RasterDataset> {
        if o.overview.is_some() || !o.bands.is_empty() {
            return Err(invalid("retile mosaic windows use full-resolution complete bands"));
        }
        if !w.fits(self.base.raster.cols, self.base.raster.rows) {
            return Err(invalid("retile window is outside the mosaic"));
        }
        let g = self.base.geo_transform();
        let gt = [g[0] + w.x as f64 * g[1], g[1], 0., g[3] + w.y as f64 * g[5], 0., g[5]];
        let bands = self.base.raster.bands;
        let dtype = self.base.raster.data_type;
        let n = w.width.checked_mul(w.height).and_then(|n| n.checked_mul(bands)).ok_or_else(|| invalid("retile window overflow"))?;
        let bytes = n.checked_mul(dtype.size_bytes()).ok_or_else(|| invalid("retile window byte overflow"))?;
        if bytes > RenderLimits::default().max_source_window_bytes {
            return Err(invalid("retile source window exceeds its buffer budget; use smaller tiles"));
        }
        let mut out = super::raster_header(
            Grid { width: w.width, height: w.height, geo_transform: gt },
            dtype,
            bands,
            self.base.nodata(),
            &self.base.crs(),
        )?;
        out.raster.data = crate::dataset::fallible::raster_data(dtype, n, self.base.nodata().unwrap_or(0.), "retile mosaic window")?;
        let env = AABB::from_corners([gt[0], gt[3] + w.height as f64 * gt[5]], [gt[0] + w.width as f64 * gt[1], gt[3]]);
        let mut candidates: Vec<_> = self.index.locate_in_envelope_intersecting(env).map(|e| e.data).collect();
        candidates.sort_unstable();
        for i in candidates {
            let b = extent(&self.headers[i]);
            let x0 = ((b[0] - gt[0]) / gt[1] + 0.5).floor().clamp(0., w.width as f64) as usize;
            let x1 = ((b[2] - gt[0]) / gt[1] + 0.5).floor().clamp(0., w.width as f64) as usize;
            let y0 = ((gt[3] - b[3]) / -gt[5] + 0.5).floor().clamp(0., w.height as f64) as usize;
            let y1 = ((gt[3] - b[1]) / -gt[5] + 0.5).floor().clamp(0., w.height as f64) as usize;
            for y in (y0..y1).step_by(2048) {
                for x in (x0..x1).step_by(2048) {
                    let size = [2048.min(x1 - x), 2048.min(y1 - y)];
                    let cell = TileCell {
                        zoom: 0,
                        x: 0,
                        y: 0,
                        size,
                        geo_transform: [gt[0] + x as f64 * gt[1], gt[1], 0., gt[3] + y as f64 * gt[5], 0., gt[5]],
                    };
                    let source_gt = self.headers[i].geo_transform();
                    let sx = (cell.geo_transform[0] - source_gt[0]) / source_gt[1];
                    let sy = (cell.geo_transform[3] - source_gt[3]) / source_gt[5];
                    let data = if source_gt[1] == gt[1]
                        && source_gt[5] == gt[5]
                        && (sx - sx.round()).abs() < 1e-8
                        && (sy - sy.round()).abs() < 1e-8
                        && sx >= 0.
                        && sy >= 0.
                        && self.headers[i].raster.data_type == dtype
                    {
                        self.sources[i]
                            .source
                            .read_window(Window::new(sx.round() as usize, sy.round() as usize, size[0], size[1]), &Default::default())?
                    } else {
                        let mut raw = Raw { source: &mut *self.sources[i].source, crs: self.base.crs() };
                        render_cell(
                            &mut raw,
                            &cell,
                            &WarpOptions {
                                output_type: Some(dtype),
                                src_nodata: Nodata::None,
                                dst_nodata: Nodata::None,
                                resampling: Resampling::Nearest,
                                error_threshold: 0.,
                                ..Default::default()
                            },
                            RenderLimits::default(),
                        )?
                    };
                    if data.raster.cols != size[0]
                        || data.raster.rows != size[1]
                        || data.raster.bands != bands
                        || data.raster.data.len() != size[0] * size[1] * bands
                    {
                        return Err(invalid("retile reader returned a malformed window"));
                    }
                    blit(&mut out.raster.data, w.width, w.height, &data.raster.data, size, [x, y], bands)?;
                }
            }
        }
        Ok(out)
    }
}
fn blit(
    out: &mut RasterData,
    width: usize,
    height: usize,
    input: &RasterData,
    size: [usize; 2],
    offset: [usize; 2],
    bands: usize,
) -> Result<()> {
    macro_rules! copy {($($variant:ident),*)=>{match(out,input){$( (RasterData::$variant(dst),RasterData::$variant(src))=>{
        for b in 0..bands {for y in 0..size[1] {let d=b*width*height+(offset[1]+y)*width+offset[0];let s=b*size[0]*size[1]+y*size[0];dst[d..d+size[0]].copy_from_slice(&src[s..s+size[0]]);}}
    },)* _=>return Err(invalid("retile window sample type mismatch"))}};}
    copy!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
    Ok(())
}
fn csv_text(records: impl Iterator<Item = (String, TileCell)>, o: &RetileOptions) -> Result<String> {
    let mut text = String::new();
    for (name, c) in records {
        let g = c.geo_transform;
        let d = &o.csv_delimiter;
        let name = if name.contains([d.chars().next().unwrap(), '"', '\r', '\n']) {
            format!("\"{}\"", name.replace('"', "\"\""))
        } else {
            name.clone()
        };
        text.push_str(&format!(
            "{name}{d}{:.6}{d}{:.6}{d}{:.6}{d}{:.6}\n",
            g[0],
            g[0] + g[1] * c.size[0] as f64,
            g[3] + g[5] * c.size[1] as f64,
            g[3]
        ));
        if text.len() > 16 * 1024 * 1024 {
            return Err(invalid("retile CSV exceeds its byte budget"));
        }
    }
    Ok(text)
}
fn manifests(
    plan: &RetilePlan,
    directory: &str,
    records: &[(String, TileCell)],
    o: &RetileOptions,
    sink: &mut dyn FolderSink,
) -> Result<()> {
    if let Some(name) = &o.csv {
        let path = format!("{directory}{name}");
        if !sink.exists(&path)? {
            let text = csv_text(records.iter().cloned(), o)?;
            sink.put(&path, text.as_bytes())?;
        }
    }
    if let Some(name) = &o.tile_index {
        let mut layer = crate::Layer::new(std::path::Path::new(name).file_stem().and_then(|v| v.to_str()).unwrap_or("tiles"));
        if let Some(crs) = &plan.crs {
            SpatialRef::parse(crs)?.apply_to_layer(&mut layer);
        }
        let mut field = crate::FieldDef::new(&o.tile_index_field, crate::FieldType::Text);
        field.width = 254;
        layer.schema.add_field(field);
        for (name, c) in records {
            let g = c.geo_transform;
            let x = g[0] + g[1] * c.size[0] as f64;
            let y = g[3] + g[5] * c.size[1] as f64;
            layer.features.push(crate::Feature {
                fid: layer.features.len() as u64 + 1,
                attributes: vec![crate::FieldValue::Text(name.clone())],
                geometry: Some(crate::Geometry::Polygon {
                    exterior: wbvector::Ring(vec![
                        wbvector::Coord::xy(g[0], g[3]),
                        wbvector::Coord::xy(x, g[3]),
                        wbvector::Coord::xy(x, y),
                        wbvector::Coord::xy(g[0], y),
                        wbvector::Coord::xy(g[0], g[3]),
                    ]),
                    interiors: Vec::new(),
                }),
            });
        }
        let path = format!("{directory}{name}");
        if name.to_ascii_lowercase().ends_with(".shp") {
            let stem = name.rsplit_once('.').unwrap().0;
            let bytes = crate::memory::write_vector_bytes(&[layer], &format!("{stem}.shp.zip"), &Default::default())?;
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
            for i in 0..zip.len() {
                let mut entry = zip.by_index(i)?;
                let path = format!("{directory}{}", entry.name()?);
                if !sink.exists(&path)? {
                    let mut bytes = Vec::new();
                    std::io::Read::read_to_end(&mut entry, &mut bytes)?;
                    sink.put(&path, &bytes)?;
                }
            }
        } else if !sink.exists(&path)? {
            let bytes = crate::memory::write_vector_bytes(&[layer], name, &Default::default())?;
            sink.put(&path, &bytes)?;
        }
    }
    Ok(())
}

/// Write a confined local directory, preserving existing files with resume.
pub fn to_directory(sources: &mut [RetileSource<'_>], o: &RetileOptions, output: &std::path::Path, resume: bool) -> Result<TileReport> {
    plan(sources, o)?;
    let mut sink = super::raster_tile::folder::DirectorySink::new(output, resume)?;
    generate(sources, o, &mut sink)
}
