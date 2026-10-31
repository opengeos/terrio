//! Previous-level retile sampling, checked against GDAL 3.13.3 gdal_retile.
//! Native stores read emitted parents; write-only callbacks regenerate evicted
//! parents deterministically. The cache retains at most 64 MiB and 256 tiles.
//! Independently implemented to match GDAL behavior, without translating its code.

use super::*;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

type Key = (u8, u64, u64);
const CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Cache {
    tiles: BTreeMap<Key, (Arc<RasterDataset>, usize)>,
    order: VecDeque<Key>,
    bytes: usize,
}
impl Cache {
    fn get(&mut self, key: Key) -> Option<Arc<RasterDataset>> {
        let tile = self.tiles.get(&key)?.0.clone();
        self.order.retain(|k| *k != key);
        self.order.push_back(key);
        Some(tile)
    }
    fn insert(&mut self, key: Key, tile: Arc<RasterDataset>, encoded_bytes: usize) {
        let bytes =
            tile.raster.data.len().saturating_mul(tile.raster.data_type.size_bytes()).saturating_add(encoded_bytes).saturating_add(4096);
        if bytes > CACHE_BYTES {
            return;
        }
        if let Some((_, old)) = self.tiles.remove(&key) {
            self.bytes -= old;
            self.order.retain(|k| *k != key);
        }
        while self.bytes + bytes > CACHE_BYTES || self.tiles.len() >= 256 {
            let Some(old) = self.order.pop_front() else { break };
            if let Some((_, size)) = self.tiles.remove(&old) {
                self.bytes -= size;
            }
        }
        self.bytes += bytes;
        self.order.push_back(key);
        self.tiles.insert(key, (tile, bytes));
    }
}

fn write_options(options: &RetileOptions) -> RasterWriteOptions {
    RasterWriteOptions {
        driver: Some(if options.format.eq_ignore_ascii_case("cog") {
            crate::formats::RasterDriver::Cog
        } else {
            crate::formats::RasterDriver::GTiff
        }),
        creation_options: options.creation_options.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        ..Default::default()
    }
}

fn path(plan: &RetilePlan, cell: &TileCell) -> String {
    let level = plan.levels.iter().find(|level| level.level == cell.zoom).expect("planned retile level");
    format!("{}{}", plan.directory(cell.zoom), plan.cell(level, cell.y as usize, cell.x as usize).0)
}

pub(super) fn encoded(
    root: &mut Mosaic<'_, '_>,
    plan: &RetilePlan,
    options: &RetileOptions,
    cache: &mut Cache,
    sink: &mut dyn FolderSink,
    cell: &TileCell,
) -> Result<Vec<u8>> {
    let dataset = render(root, plan, options, cache, sink, cell)?;
    let name = path(plan, cell);
    let bytes = crate::memory::write_raster_bytes(&dataset, &name, &write_options(options))?;
    // Decode the emitted encoding, including lossy creation options, rather than
    // caching the pre-encoding samples. Higher levels see the actual parent data.
    if options.levels > cell.zoom
        && bytes.len().saturating_add(dataset.raster.data.len().saturating_mul(dataset.raster.data_type.size_bytes())) < CACHE_BYTES
    {
        let decoded = decode(bytes.clone(), &name, cell, dataset.raster.bands, dataset.raster.data_type)?;
        cache.insert((cell.zoom, cell.x, cell.y), decoded, bytes.len());
    }
    Ok(bytes)
}

fn parent(
    root: &mut Mosaic<'_, '_>,
    plan: &RetilePlan,
    options: &RetileOptions,
    cache: &mut Cache,
    sink: &mut dyn FolderSink,
    cell: &TileCell,
) -> Result<Arc<RasterDataset>> {
    let key = (cell.zoom, cell.x, cell.y);
    if let Some(tile) = cache.get(key) {
        return Ok(tile);
    }
    let name = path(plan, cell);
    let bytes = match sink.get(&name)? {
        Some(bytes) => bytes,
        None if sink.exists(&name)? => return Err(invalid("retile resume requires parent tile read-back from the sink")),
        None => encoded(root, plan, options, cache, sink, cell)?,
    };
    if let Some(tile) = cache.get(key) {
        return Ok(tile);
    }
    let dtype = options.output_type.as_ref().map(|s| super::super::parse_data_type(s)).transpose()?.unwrap_or(root.base.raster.data_type);
    let tile = decode(bytes.clone(), &name, cell, root.base.raster.bands, dtype)?;
    cache.insert(key, tile.clone(), bytes.len());
    Ok(tile)
}

fn decode(bytes: Vec<u8>, name: &str, cell: &TileCell, bands: usize, dtype: crate::DataType) -> Result<Arc<RasterDataset>> {
    let mut reader = crate::memory::open_raster_lazy_bytes(bytes, name)?;
    if [reader.width(), reader.height()] != cell.size || reader.band_count() != bands || reader.data_type() != dtype || reader.is_complex()
    {
        return Err(invalid("parent tile dimensions, bands or type differ from the retile layout"));
    }
    Ok(Arc::new(reader.read()?))
}

fn render(
    root: &mut Mosaic<'_, '_>,
    plan: &RetilePlan,
    options: &RetileOptions,
    cache: &mut Cache,
    sink: &mut dyn FolderSink,
    cell: &TileCell,
) -> Result<RasterDataset> {
    let dtype = options.output_type.as_ref().map(|s| super::super::parse_data_type(s)).transpose()?;
    let warp = WarpOptions {
        resampling: Resampling::parse(&options.resampling)?,
        output_type: dtype,
        kernel_version: super::super::warp::KernelVersion::Gdal313,
        error_threshold: 0.,
        unified_src_nodata: Some(false),
        ..Default::default()
    };
    let mut dataset = if cell.zoom == 0 && dtype.is_none_or(|d| d == root.base.raster.data_type) {
        let g = root.base.geo_transform();
        root.read_window(
            Window::new(
                ((cell.geo_transform[0] - g[0]) / g[1]).round() as usize,
                ((cell.geo_transform[3] - g[3]) / g[5]).round() as usize,
                cell.size[0],
                cell.size[1],
            ),
            &Default::default(),
        )?
    } else if cell.zoom == 0 || (cell.zoom == 1 && options.pyramid_only) {
        let header = root.base.clone();
        let mut source = Crop::new(root, &header, cell)?;
        render_cell(&mut source, cell, &warp, RenderLimits::default())?
    } else {
        let previous = plan.levels.iter().find(|l| l.level + 1 == cell.zoom).ok_or_else(|| invalid("missing parent retile level"))?;
        let mut header = root.base.clone();
        header.raster.cols = previous.size[0];
        header.raster.rows = previous.size[1];
        header.raster.data_type = dtype.unwrap_or(root.base.raster.data_type);
        header.set_geo_transform(previous.geo_transform);
        let mut source = Previous { root, plan, options, cache, sink, level: previous, header: header.clone() };
        let mut crop = Crop::new(&mut source, &header, cell)?;
        render_cell(&mut crop, cell, &warp, RenderLimits::default())?
    };
    // Base GTiff creation uses the driver's color defaults. In pyramid-only
    // mode, the first level inherits the original input's color interpretations.
    dataset.band_metadata = if options.pyramid_only {
        (1..=root.base.raster.bands)
            .map(|band| crate::BandMetadata { color_interp: Some(root.base.band_color_interp(band).to_string()), ..Default::default() })
            .collect()
    } else if dataset.raster.data_type == crate::DataType::U8 && dataset.raster.bands == 4 {
        ["Red", "Green", "Blue", "Alpha"]
            .into_iter()
            .map(|color| crate::BandMetadata { color_interp: Some(color.into()), ..Default::default() })
            .collect()
    } else {
        Vec::new()
    };
    dataset.color_table = root.base.color_table.clone();
    Ok(dataset)
}

struct Crop<'a> {
    source: &'a mut dyn WarpSource,
    header: RasterDataset,
    offset: [usize; 2],
}
impl<'a> Crop<'a> {
    fn new(source: &'a mut dyn WarpSource, original: &RasterDataset, cell: &TileCell) -> Result<Self> {
        let g = original.geo_transform();
        let offset = [((cell.geo_transform[0] - g[0]) / g[1]).round() as usize, ((cell.geo_transform[3] - g[3]) / g[5]).round() as usize];
        let mut header = original.clone();
        header.raster.cols = (cell.size[0] as f64 * cell.geo_transform[1] / g[1]).round() as usize;
        header.raster.rows = (cell.size[1] as f64 * cell.geo_transform[5] / g[5]).round() as usize;
        if !Window::new(offset[0], offset[1], header.raster.cols, header.raster.rows).fits(original.raster.cols, original.raster.rows) {
            return Err(invalid("retile parent window is outside its grid"));
        }
        header.set_geo_transform([cell.geo_transform[0], g[1], 0., cell.geo_transform[3], 0., g[5]]);
        Ok(Self { source, header, offset })
    }
}
impl WarpSource for Crop<'_> {
    fn has_mask(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn info(&mut self) -> Result<WarpSourceInfo> {
        Ok(WarpSourceInfo { raster: self.header.clone(), overviews: Vec::new() })
    }
    fn read_window(&mut self, window: Window, options: &WindowOptions) -> Result<RasterDataset> {
        self.source.read_window(Window::new(window.x + self.offset[0], window.y + self.offset[1], window.width, window.height), options)
    }
}

struct Previous<'a, 'r, 's> {
    root: &'a mut Mosaic<'r, 's>,
    plan: &'a RetilePlan,
    options: &'a RetileOptions,
    cache: &'a mut Cache,
    sink: &'a mut dyn FolderSink,
    level: &'a RetileLevel,
    header: RasterDataset,
}
impl WarpSource for Previous<'_, '_, '_> {
    fn has_mask(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn info(&mut self) -> Result<WarpSourceInfo> {
        Ok(WarpSourceInfo { raster: self.header.clone(), overviews: Vec::new() })
    }
    fn read_window(&mut self, window: Window, options: &WindowOptions) -> Result<RasterDataset> {
        if options.overview.is_some() || !options.bands.is_empty() || !window.fits(self.header.raster.cols, self.header.raster.rows) {
            return Err(invalid("invalid retile parent window"));
        }
        let g = self.header.geo_transform();
        let gt = [g[0] + window.x as f64 * g[1], g[1], 0., g[3] + window.y as f64 * g[5], 0., g[5]];
        let bands = self.header.raster.bands;
        let dtype = self.header.raster.data_type;
        let mut out = super::super::raster_header(
            Grid { width: window.width, height: window.height, geo_transform: gt },
            dtype,
            bands,
            self.header.nodata(),
            &self.header.crs(),
        )?;
        let count =
            window.width.checked_mul(window.height).and_then(|n| n.checked_mul(bands)).ok_or_else(|| invalid("parent window overflow"))?;
        if count.checked_mul(dtype.size_bytes()).is_none_or(|n| n > RenderLimits::default().max_source_window_bytes) {
            return Err(invalid("parent window exceeds its buffer budget"));
        }
        out.raster.data = crate::dataset::fallible::raster_data(dtype, count, self.header.nodata().unwrap_or(0.), "retile parent window")?;
        if window.width == 0 || window.height == 0 {
            return Ok(out);
        }
        let step = [self.plan.tile_size[0] - self.plan.overlap, self.plan.tile_size[1] - self.plan.overlap];
        let first = |start: usize, tile: usize, stride: usize| if start < tile { 0 } else { (start - tile + 1).div_ceil(stride) };
        let cols = first(window.x, self.plan.tile_size[0], step[0])..=((window.x + window.width - 1) / step[0]).min(self.level.columns - 1);
        let rows = first(window.y, self.plan.tile_size[1], step[1])..=((window.y + window.height - 1) / step[1]).min(self.level.rows - 1);
        for row in rows {
            for col in cols.clone() {
                let (_, cell) = self.plan.cell(self.level, row, col);
                let tile = parent(self.root, self.plan, self.options, self.cache, self.sink, &cell)?;
                let origin = [col * step[0], row * step[1]];
                let x0 = origin[0].max(window.x);
                let y0 = origin[1].max(window.y);
                let x1 = (origin[0] + cell.size[0]).min(window.x + window.width);
                let y1 = (origin[1] + cell.size[1]).min(window.y + window.height);
                copy(
                    &mut out,
                    &tile,
                    Window::new(x0 - window.x, y0 - window.y, x1 - x0, y1 - y0),
                    Window::new(x0 - origin[0], y0 - origin[1], x1 - x0, y1 - y0),
                )?;
            }
        }
        Ok(out)
    }
}

fn copy(out: &mut RasterDataset, input: &RasterDataset, dst: Window, src: Window) -> Result<()> {
    let out_size = [out.raster.cols, out.raster.rows];
    let in_size = [input.raster.cols, input.raster.rows];
    let bands = out.raster.bands;
    let size = [dst.width, dst.height];
    let dst = [dst.x, dst.y];
    let src = [src.x, src.y];
    macro_rules! typed {($($kind:ident),*)=>{match(&mut out.raster.data,&input.raster.data){$((RasterData::$kind(out),RasterData::$kind(input))=>{
        for b in 0..bands {for y in 0..size[1] {
            let a=b*out_size[0]*out_size[1]+(dst[1]+y)*out_size[0]+dst[0];
            let z=b*in_size[0]*in_size[1]+(src[1]+y)*in_size[0]+src[0];
            out[a..a+size[0]].copy_from_slice(&input[z..z+size[0]]);
        }}
        Ok(())
    },)* _=>Err(invalid("retile parent sample type differs from the layout"))}};}
    typed!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64)
}
