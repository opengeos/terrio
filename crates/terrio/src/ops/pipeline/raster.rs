//! Window/overview-preserving band and crop views used between pipeline steps.
use crate::cog::{StreamedSource, WindowSource};
use crate::ops::{
    Grid, Resampling,
    overview::OverviewSelection,
    warp::{WarpOptions, WarpSource, WarpSourceInfo},
};
use crate::{Error, LazyRaster, RasterDataset, Result, Window, WindowOptions};
use std::sync::Mutex;
use wbraster::{DataType, raster::RasterData};

pub(super) fn clear_statistics(dataset: &mut RasterDataset) {
    dataset.raster.metadata.retain(|(key, _)| !key.starts_with("STATISTICS_"));
    for band in &mut dataset.band_metadata {
        band.items.retain(|(key, _)| !key.starts_with("STATISTICS_"));
    }
}

struct View {
    referenced: bool,
    source: Mutex<LazyRaster>,
    bands: Vec<usize>,
    crop: Window,
    full_size: (usize, usize),
    overviews: Vec<(usize, usize)>,
    block: Option<(usize, usize)>,
    mask: bool,
    alpha: Option<usize>,
    band_types: Vec<DataType>,
}
impl View {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, LazyRaster>> {
        self.source.lock().map_err(|_| Error::InvalidData("pipeline raster lock poisoned".into()))
    }
    fn map_bands(&self, bands: &[usize]) -> Result<Vec<usize>> {
        bands
            .iter()
            .map(|b| {
                b.checked_sub(1)
                    .and_then(|i| self.bands.get(i))
                    .copied()
                    .ok_or_else(|| Error::InvalidArgument("invalid pipeline view band".into()))
            })
            .collect()
    }
    fn overview_coordinates(&self, source: &LazyRaster, level: usize, window: Window) -> Result<(Vec<usize>, Vec<usize>)> {
        let &(sw, sh) = source.overviews().get(level - 1).ok_or_else(|| Error::InvalidArgument("invalid source overview".into()))?;
        let &(vw, vh) = self.overviews.get(level - 1).ok_or_else(|| Error::InvalidArgument("invalid view overview".into()))?;
        let axis = |offset: usize, length: usize, start: usize, count: usize, view: usize, full: usize, source: usize| {
            (start..start + count)
                .map(|i| {
                    (((offset as f64 + (i as f64 + 0.5) * length as f64 / view as f64) * source as f64 / full as f64).floor() as usize)
                        .min(source - 1)
                })
                .collect()
        };
        Ok((
            axis(self.crop.x, self.crop.width, window.x, window.width, vw, self.full_size.0, sw),
            axis(self.crop.y, self.crop.height, window.y, window.height, vh, self.full_size.1, sh),
        ))
    }
    fn read_data(&self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
        let selected = self.map_bands(bands)?;
        let mut source = self.lock()?;
        if window.width == 0 || window.height == 0 {
            return Ok(RasterData::new_filled(source.data_type(), 0, 0.));
        }
        if self.crop == Window::full(self.full_size.0, self.full_size.1) || level == 0 {
            let window = Window::new(
                window.x + if level == 0 { self.crop.x } else { 0 },
                window.y + if level == 0 { self.crop.y } else { 0 },
                window.width,
                window.height,
            );
            return Ok(source.read_window(window, &WindowOptions { bands: selected, overview: level.checked_sub(1) })?.raster.data);
        }
        let (cols, rows) = self.overview_coordinates(&source, level, window)?;
        let options = WindowOptions { bands: selected.clone(), overview: Some(level - 1) };
        if let Some(data) = source.read_sampled(&cols, &rows, &options)? {
            return Ok(data);
        }
        let bounds = sample_bounds(&cols, &rows);
        let data = source.read_window(bounds, &options)?.raster.data;
        macro_rules! gather { ($($kind:ident),*) => { match data { $(RasterData::$kind(values) => RasterData::$kind(gather_values(&values, bounds, &cols, &rows, selected.len())),)* } }; }
        Ok(gather!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64))
    }
}
fn sample_bounds(cols: &[usize], rows: &[usize]) -> Window {
    Window::new(cols[0], rows[0], cols[cols.len() - 1] - cols[0] + 1, rows[rows.len() - 1] - rows[0] + 1)
}
fn gather_values<T: Copy>(values: &[T], bounds: Window, cols: &[usize], rows: &[usize], bands: usize) -> Vec<T> {
    let mut output = Vec::with_capacity(cols.len() * rows.len() * bands);
    for band in 0..bands {
        for row in rows {
            for col in cols {
                output.push(values[band * bounds.width * bounds.height + (row - bounds.y) * bounds.width + col - bounds.x]);
            }
        }
    }
    output
}

impl WindowSource for View {
    fn geotransform_present(&self) -> Option<bool> {
        Some(self.referenced)
    }
    fn band_data_type(&self, band: usize) -> Option<DataType> {
        band.checked_sub(1).and_then(|b| self.band_types.get(b)).copied()
    }
    fn alpha_band(&self) -> Option<usize> {
        self.alpha
    }
    fn whole_read_keeps_metadata(&self) -> bool {
        true
    }
    fn overviews(&self) -> Vec<(usize, usize)> {
        self.overviews.clone()
    }
    fn block_size(&self) -> Option<(usize, usize)> {
        self.block
    }
    fn has_mask(&self) -> bool {
        self.mask
    }
    fn input_paths(&self) -> Result<Vec<String>> {
        self.lock()?.input_paths()
    }
    fn streamed(&self) -> Option<&dyn StreamedSource> {
        Some(self)
    }
    fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
        self.read_data(level, window, bands)
    }
    fn read_mask(&mut self, level: usize, window: Window) -> Result<Option<Vec<u8>>> {
        if !self.mask {
            return Ok(None);
        }
        if window.width == 0 || window.height == 0 {
            return Ok(Some(vec![]));
        }
        if level == 0 || self.crop == Window::full(self.full_size.0, self.full_size.1) {
            let window = Window::new(
                window.x + if level == 0 { self.crop.x } else { 0 },
                window.y + if level == 0 { self.crop.y } else { 0 },
                window.width,
                window.height,
            );
            return self.lock()?.read_mask(window, level.checked_sub(1));
        }
        let mut source = self.lock()?;
        let (cols, rows) = self.overview_coordinates(&source, level, window)?;
        let bounds = sample_bounds(&cols, &rows);
        Ok(source.read_mask(bounds, Some(level - 1))?.map(|values| gather_values(&values, bounds, &cols, &rows, 1)))
    }
    fn read_sampled(&mut self, level: usize, cols: &[usize], rows: &[usize], bands: &[usize]) -> Result<Option<RasterData>> {
        if level > 0 && self.crop != Window::full(self.full_size.0, self.full_size.1) {
            return Ok(None);
        }
        let cols: Vec<_> = cols.iter().map(|x| x + if level == 0 { self.crop.x } else { 0 }).collect();
        let rows: Vec<_> = rows.iter().map(|y| y + if level == 0 { self.crop.y } else { 0 }).collect();
        self.lock()?.read_sampled(&cols, &rows, &WindowOptions { bands: self.map_bands(bands)?, overview: level.checked_sub(1) })
    }
}
impl StreamedSource for View {
    fn inputs(&self) -> Vec<String> {
        self.checked_inputs().unwrap_or_default()
    }
    fn checked_inputs(&self) -> Result<Vec<String>> {
        self.lock()?.input_paths()
    }
    fn read_rows(&self, window: Window, bands: &[usize]) -> Result<RasterData> {
        self.read_data(0, window, bands)
    }
}

pub(super) fn preserve_reference(source: LazyRaster, referenced: bool) -> Result<LazyRaster> {
    if referenced { Ok(source) } else { view_inner(source, vec![], None, Some(false)) }
}
pub(super) fn view(source: LazyRaster, bands: Vec<usize>, crop: Option<Window>) -> Result<LazyRaster> {
    view_inner(source, bands, crop, None)
}
fn view_inner(mut source: LazyRaster, bands: Vec<usize>, crop: Option<Window>, reference: Option<bool>) -> Result<LazyRaster> {
    let referenced = reference.unwrap_or_else(|| source.geotransform_present());
    let full_size = (source.width(), source.height());
    let crop = crop.unwrap_or_else(|| Window::full(full_size.0, full_size.1));
    if !crop.fits(full_size.0, full_size.1) || crop.width == 0 || crop.height == 0 {
        return Err(Error::InvalidArgument("pipeline crop is outside source dimensions".into()));
    }
    let bands = if bands.is_empty() { (1..=source.band_count()).collect() } else { bands };
    if bands.iter().any(|b| *b == 0 || *b > source.band_count()) {
        return Err(Error::InvalidArgument("pipeline band is out of range".into()));
    }
    if source.is_complex() {
        return Err(Error::Unsupported("pipeline band/crop views of complex rasters are not supported".into()));
    }
    let source_alpha = source.alpha_band();
    let alpha = source_alpha.and_then(|a| bands.iter().position(|b| *b == a).map(|i| i + 1));
    let mask = source.has_mask();
    let block = source.block_size();
    let original = source.header();
    let mut header = original.clone();
    header.raster.cols = crop.width;
    header.raster.rows = crop.height;
    header.raster.bands = bands.len();
    header.raster.data = RasterData::new_filled(source.data_type(), 0, 0.);
    let gt = source.level_geo_transform(None)?;
    header.set_geo_transform([
        gt[0] + crop.x as f64 * gt[1] + crop.y as f64 * gt[2],
        gt[1],
        gt[2],
        gt[3] + crop.x as f64 * gt[4] + crop.y as f64 * gt[5],
        gt[4],
        gt[5],
    ]);
    header.gcps = original
        .gcps
        .iter()
        .cloned()
        .map(|mut g| {
            g.pixel -= crop.x as f64;
            g.line -= crop.y as f64;
            g
        })
        .collect();
    if let Some(geolocation) = &mut header.geolocation {
        geolocation.pixel_offset -= crop.x as f64;
        geolocation.line_offset -= crop.y as f64;
    }
    header.band_metadata = bands
        .iter()
        .map(|b| {
            let mut metadata = original.band_metadata.get(b - 1).cloned().unwrap_or_default();
            let color = original.band_color_interp(*b);
            metadata.color_interp = Some(if bands.len() == 1 && color == "Undefined" { "Gray" } else { color }.to_string());
            metadata
        })
        .collect();
    header.band_nodata = bands.iter().map(|b| source.band_nodata(*b)).collect();
    header.has_nodata = header.band_nodata.first().copied().flatten().is_some();
    header.raster.nodata = header.band_nodata.first().copied().flatten().unwrap_or(-9999.);
    if bands != [1] {
        header.color_table.clear();
    }
    if crop != Window::full(full_size.0, full_size.1) {
        clear_statistics(&mut header);
    }
    let band_types = bands.iter().map(|b| source.band_data_type(*b)).collect();
    let overviews = source
        .overviews()
        .into_iter()
        .map(|(w, h)| {
            (
                ((crop.width as u128 * w as u128).div_ceil(full_size.0 as u128) as usize).max(1),
                ((crop.height as u128 * h as u128).div_ceil(full_size.1 as u128) as usize).max(1),
            )
        })
        .collect();
    let reader = View { referenced, source: Mutex::new(source), bands, crop, full_size, overviews, block, mask, alpha, band_types };
    Ok(LazyRaster::from_source(header, Box::new(reader)))
}

struct MaskSource {
    source: Mutex<LazyRaster>,
    overviews: Vec<(usize, usize)>,
}
impl WindowSource for MaskSource {
    fn overviews(&self) -> Vec<(usize, usize)> {
        self.overviews.clone()
    }
    fn whole_read_keeps_metadata(&self) -> bool {
        true
    }
    fn read(&mut self, level: usize, window: Window, _bands: &[usize]) -> Result<RasterData> {
        let source = self.source.get_mut().map_err(|_| Error::InvalidData("pipeline mask lock poisoned".into()))?;
        Ok(RasterData::U8(source.read_mask(window, level.checked_sub(1))?.unwrap_or_else(|| vec![255; window.width * window.height])))
    }
}
fn mask_source(source: &mut LazyRaster) -> Result<LazyRaster> {
    let full = source.header();
    let header = crate::ops::raster_header(
        Grid { width: source.width(), height: source.height(), geo_transform: full.geo_transform() },
        DataType::U8,
        1,
        None,
        &full.crs(),
    )?;
    let overviews = source.overviews();
    let source = source.shared();
    Ok(LazyRaster::from_source(header, Box::new(MaskSource { source: Mutex::new(source), overviews })))
}

pub(super) fn warp(source: &mut LazyRaster, options: &WarpOptions) -> Result<LazyRaster> {
    warp_inner(source, options, true)
}
pub(super) fn resize(source: &mut LazyRaster, options: &WarpOptions) -> Result<LazyRaster> {
    // Resize samples raw pixels and the validity plane independently, like GDAL
    // Translate; reprojection instead excludes masked pixels from interpolation.
    warp_inner(source, options, false)
}
/// A raster warped by its raw pixels, whatever its dataset mask says.
struct RawPixels<'a>(&'a mut LazyRaster);
impl WarpSource for RawPixels<'_> {
    fn with_dataset(&self, visit: &mut dyn FnMut(&RasterDataset)) -> Result<bool> {
        self.0.with_dataset(visit)
    }
    fn info(&mut self) -> Result<WarpSourceInfo> {
        self.0.info()
    }
    fn has_mask(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn read_window(&mut self, window: Window, options: &WindowOptions) -> Result<RasterDataset> {
        self.0.read_window(window, options)
    }
}
fn warp_inner(source: &mut LazyRaster, options: &WarpOptions, mask_pixels: bool) -> Result<LazyRaster> {
    let referenced = source.geotransform_present() || options.dst_srs.is_some();
    let original = source.header();
    let mut output = if mask_pixels {
        crate::ops::warp::warp_lazy(source, options)?
    } else {
        crate::ops::warp::warp_sources(&mut [&mut RawPixels(source)], options)?
    };
    // Independent source metadata survives changes of grid; derived statistics do not.
    output.raster.metadata = original.raster.metadata.clone();
    output.band_metadata = original.band_metadata.clone();
    clear_statistics(&mut output);
    output.color_table = original.color_table.clone();
    if !mask_pixels && LazyRaster::has_mask(source) {
        let mut mask = mask_source(source)?;
        let mut mask_options = options.clone();
        mask_options.output_type = Some(DataType::U8);
        mask_options.resampling = Resampling::Nearest;
        mask_options.overview = OverviewSelection::Auto;
        mask_options.src_nodata = crate::ops::warp::Nodata::None;
        mask_options.dst_nodata = crate::ops::warp::Nodata::None;
        let mask = crate::ops::warp::warp_lazy(&mut mask, &mask_options)?
            .raster
            .data
            .to_f64_vec()
            .into_iter()
            .map(|v| if v == 0. { 0 } else { 255 })
            .collect();
        preserve_reference(LazyRaster::from_masked_dataset(output, mask)?, referenced)
    } else {
        preserve_reference(LazyRaster::from(output), referenced)
    }
}
