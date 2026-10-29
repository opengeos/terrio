//! Serialized, lazy GDAL pansharpened VRTs and bounded materialization.
use super::{PansharpenExtent, PansharpenNodata, PansharpenOptions, Plan, SpectralBand, blocks, plan};
use crate::cog::WindowSource;
use crate::ops::vrt::xml;
use crate::ops::{Resampling, data_type_name, parse_data_type};
use crate::{Error, LazyRaster, RasterDataset, RasterWriteOptions, Result, Window, WindowOptions};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use wbraster::raster::RasterData;

#[derive(Debug, Clone)]
struct Source {
    name: String,
    relative: bool,
    band: usize,
}
#[derive(Debug, Clone)]
enum Output {
    Sharp(usize),
    Pan(usize),
}

/// A GDAL-compatible pansharpened VRT. Parsing never reads source pixels.
/// Sources may be persistent paths/URLs or supplied lazy readers, including WASM uploads.
#[derive(Debug, Clone)]
pub struct PansharpenVrt {
    root: xml::Element,
    pan: Source,
    spectral: Vec<Source>,
    outputs: Vec<Output>,
    options: PansharpenOptions,
}
fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("pansharpened VRT: {message}"))
}
fn unsupported(message: impl std::fmt::Display) -> Error {
    Error::Unsupported(format!("pansharpened VRT: {message}"))
}
fn number<T: std::str::FromStr>(text: &str, name: &str) -> Result<T> {
    text.trim().parse().map_err(|_| invalid(format!("invalid {name}")))
}
fn source(element: &xml::Element) -> Result<Source> {
    let file = element.child("SourceFilename").ok_or_else(|| invalid("missing SourceFilename"))?;
    if file.text.trim().is_empty() {
        return Err(invalid("empty source filename"));
    }
    let band = element.child("SourceBand").map(|e| number(&e.text, "source band")).transpose()?.unwrap_or(1);
    if band == 0 {
        return Err(invalid("bands start at 1"));
    }
    Ok(Source {
        name: file.text.trim().into(),
        relative: file.attr("relativeToVRT").or_else(|| file.attr("relativetoVRT")) == Some("1"),
        band,
    })
}
fn resampling(text: &str) -> Result<Resampling> {
    let method = match text.trim().to_ascii_lowercase().as_str() {
        "nearestneighbour" | "nearestneighbor" => Resampling::Nearest,
        _ => Resampling::parse(text.trim())?,
    };
    if !matches!(
        method,
        Resampling::Nearest
            | Resampling::Bilinear
            | Resampling::Cubic
            | Resampling::CubicSpline
            | Resampling::Lanczos
            | Resampling::Average
    ) {
        return Err(unsupported("resampling algorithm"));
    }
    Ok(method)
}

impl PansharpenVrt {
    /// Parse GDAL's minimal or fully declared VRTPansharpenedDataset XML.
    /// Weighted Brovey, selected bands and an unsharpened pan passthrough are supported.
    /// Under a process-wide raster allow-list, only with the VRT driver allowed.
    pub fn from_xml(text: &str) -> Result<Self> {
        crate::formats::check_driver_allowed("VRT", "a VRT description", crate::formats::Kind::Raster)?;
        let root = xml::parse(text)?;
        if root.name != "VRTDataset" || root.attr("subClass") != Some("VRTPansharpenedDataset") {
            return Err(invalid("expected VRTPansharpenedDataset"));
        }
        let config = root.child("PansharpeningOptions").ok_or_else(|| invalid("missing PansharpeningOptions"))?;
        if config.child("Algorithm").is_some_and(|e| !e.text.trim().eq_ignore_ascii_case("WeightedBrovey")) {
            return Err(unsupported("only WeightedBrovey is supported"));
        }
        let pan = source(config.child("PanchroBand").ok_or_else(|| invalid("missing PanchroBand"))?)?;
        let mut spectral = Vec::new();
        let mut mapping = BTreeMap::new();
        for item in config.children.iter().filter(|e| e.name == "SpectralBand") {
            spectral.push(source(item)?);
            if let Some(dst) = item.attr("dstBand") {
                let dst = number::<usize>(dst, "dstBand")?;
                if dst == 0 {
                    return Err(invalid("destination bands start at 1; omit dstBand for an unmapped spectral input"));
                }
                if mapping.insert(dst, spectral.len()).is_some() {
                    return Err(invalid("duplicate destination band"));
                }
            }
        }
        if spectral.is_empty() || mapping.is_empty() {
            return Err(invalid("spectral inputs and output mappings are required"));
        }
        let weights = config
            .child("AlgorithmOptions")
            .and_then(|e| e.child("Weights"))
            .map(|e| {
                e.text
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|s| !s.is_empty())
                    .map(|v| number::<f64>(v, "weight"))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        if weights.iter().any(|v| !v.is_finite()) || (!weights.is_empty() && weights.len() != spectral.len()) {
            return Err(invalid("one finite weight is required per spectral input"));
        }
        let nodata = match config.child("NoData") {
            None => PansharpenNodata::Auto,
            Some(e) if e.text.trim().eq_ignore_ascii_case("NONE") => PansharpenNodata::None,
            Some(e) => {
                let v = number::<f64>(&e.text, "nodata")?;
                if !v.is_finite() {
                    return Err(unsupported("nonfinite nodata"));
                }
                PansharpenNodata::Value(v)
            }
        };
        let bits = config.child("BitDepth").map(|e| number::<u8>(&e.text, "bit depth")).transpose()?.filter(|v| *v != 0);
        let mut options = PansharpenOptions {
            pan_band: pan.band,
            weights,
            resampling: config.child("Resampling").map(|e| resampling(&e.text)).transpose()?.unwrap_or(Resampling::Cubic),
            bit_depth: bits,
            nodata,
            extent: config
                .child("SpatialExtentAdjustment")
                .map(|e| PansharpenExtent::parse(e.text.trim()))
                .transpose()?
                .unwrap_or_default(),
            ..Default::default()
        };
        let declared = root.children.iter().filter(|e| e.name == "VRTRasterBand").collect::<Vec<_>>();
        let count = declared.len().max(mapping.keys().last().copied().unwrap_or(0));
        if count > declared.len() + mapping.len() {
            return Err(invalid("hole in output band numbering"));
        }
        let mut declared_numbers = HashSet::new();
        for band in &declared {
            let index = band.attr("band").ok_or_else(|| invalid("missing output band number"))?;
            let index = number::<usize>(index, "output band number")?;
            if index == 0 || !declared_numbers.insert(index) {
                return Err(invalid("output band numbers must be unique and positive"));
            }
        }
        let mut outputs = Vec::with_capacity(count);
        let mut data_type = None;
        for band in 1..=count {
            let declaration = declared.iter().find(|e| e.attr("band").and_then(|v| v.trim().parse::<usize>().ok()) == Some(band));
            if let Some(e) = declaration
                && let Some(dtype) = e.attr("dataType")
            {
                let dtype = parse_data_type(dtype)?;
                if data_type.is_some_and(|old| old != dtype) {
                    return Err(unsupported("mixed output data types"));
                }
                data_type = Some(dtype);
            }
            if let Some(input) = mapping.get(&band) {
                if declaration.is_some_and(|e| e.attr("subClass") != Some("VRTPansharpenedRasterBand")) {
                    return Err(invalid("mapped output must be a VRTPansharpenedRasterBand"));
                }
                options.output_bands.push(*input);
                outputs.push(Output::Sharp(options.output_bands.len()));
            } else {
                let e = declaration.ok_or_else(|| invalid("hole in output band numbering"))?;
                let simple = e.child("SimpleSource").ok_or_else(|| unsupported("unmapped output band"))?;
                let input = source(simple)?;
                if input.name != pan.name || input.relative != pan.relative {
                    return Err(unsupported("only pan-source passthrough bands are supported"));
                }
                if simple.child("SrcRect").is_some() || simple.child("DstRect").is_some() {
                    return Err(unsupported("rectangles on pan passthrough bands"));
                }
                outputs.push(Output::Pan(input.band));
            }
        }
        options.output_type = data_type;
        for name in ["BlockXSize", "BlockYSize"] {
            if let Some(value) = root.child(name)
                && number::<usize>(&value.text, name)? == 0
            {
                return Err(invalid("block sizes must be positive"));
            }
        }
        Ok(Self { root, pan, spectral, outputs, options })
    }

    /// Create a VRT from raster metadata and persistent source names, without reading pixels.
    /// This also supports browser uploads: provide matching names to `into_lazy`/`openVrt`.
    pub fn from_rasters(
        pan_name: &str,
        pan: &RasterDataset,
        spectral: &[(&str, &RasterDataset, usize)],
        options: &PansharpenOptions,
    ) -> Result<Self> {
        let refs = spectral.iter().map(|(_, dataset, band)| SpectralBand { dataset, band: *band }).collect::<Vec<_>>();
        let layout = plan(pan, &refs, options)?;
        let mut inputs = spectral.iter().map(|(name, _, band)| ((*name).to_string(), *band)).collect::<Vec<_>>();
        let mut weights = layout.weights.clone();
        let mut destinations = vec![None; inputs.len()];
        for (position, input) in layout.output.iter().enumerate() {
            if destinations[*input - 1].is_none() {
                destinations[*input - 1] = Some(position + 1);
            } else {
                inputs.push(inputs[*input - 1].clone());
                weights.push(0.0);
                destinations.push(Some(position + 1));
            }
        }
        let mut bands = String::new();
        for b in 1..=layout.output.len() {
            let nd = layout.nodata.map(|v| format!("<NoDataValue>{v}</NoDataValue>")).unwrap_or_default();
            bands += &format!(
                "<VRTRasterBand band=\"{b}\" dataType=\"{}\" subClass=\"VRTPansharpenedRasterBand\">{nd}</VRTRasterBand>",
                data_type_name(layout.output_type)
            );
        }
        let mut sources = String::new();
        for ((name, band), dst) in inputs.iter().zip(destinations) {
            let dst = dst.map(|v| format!(" dstBand=\"{v}\"")).unwrap_or_default();
            sources += &format!(
                "<SpectralBand{dst}><SourceFilename relativeToVRT=\"0\">{}</SourceFilename><SourceBand>{band}</SourceBand></SpectralBand>",
                xml::escape(name)
            );
        }
        let nd = match options.nodata {
            PansharpenNodata::Auto => String::new(),
            PansharpenNodata::None => "<NoData>None</NoData>".into(),
            PansharpenNodata::Value(v) => format!("<NoData>{v}</NoData>"),
        };
        let bits = options.bit_depth.map(|v| format!("<BitDepth>{v}</BitDepth>")).unwrap_or_default();
        let extent = match options.extent {
            PansharpenExtent::Union => "Union",
            PansharpenExtent::Intersection => "Intersection",
            PansharpenExtent::None => "None",
        };
        let gt = layout.pan.grid.geo_transform.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",");
        let srs = pan.crs().to_wkt().map(|v| format!("<SRS>{}</SRS>", xml::escape(&v))).unwrap_or_default();
        let text = format!(
            "<VRTDataset subClass=\"VRTPansharpenedDataset\" rasterXSize=\"{}\" rasterYSize=\"{}\">{srs}<GeoTransform>{gt}</GeoTransform>{bands}<BlockXSize>512</BlockXSize><BlockYSize>512</BlockYSize><PansharpeningOptions><Algorithm>WeightedBrovey</Algorithm><AlgorithmOptions><Weights>{}</Weights></AlgorithmOptions><Resampling>{}</Resampling>{bits}{nd}<SpatialExtentAdjustment>{extent}</SpatialExtentAdjustment><PanchroBand><SourceFilename relativeToVRT=\"0\">{}</SourceFilename><SourceBand>{}</SourceBand></PanchroBand>{sources}</PansharpeningOptions></VRTDataset>",
            layout.pan.grid.width,
            layout.pan.grid.height,
            weights.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","),
            options.resampling.name(),
            xml::escape(pan_name),
            options.pan_band
        );
        Self::from_xml(&text)
    }

    /// Create a VRT from persistent paths/URLs using their existing lazy readers.
    /// GeoTIFF/COG sources open by header; formats without window readers may load at open.
    pub fn from_paths(pan: &str, spectral: &[(String, usize)], options: &PansharpenOptions) -> Result<Self> {
        let persistent = |name: &str| -> Result<String> {
            if ["NETCDF:", "HDF5:", "ZARR:", "GPKG:"]
                .iter()
                .any(|prefix| name.get(..prefix.len()).is_some_and(|v| v.eq_ignore_ascii_case(prefix)))
            {
                Ok(crate::ops::vrt::resolve_reference(&std::env::current_dir()?.join("__vrt__.vrt").to_string_lossy(), name, true))
            } else if crate::vfs::Location::parse(name).is_local() {
                Ok(std::fs::canonicalize(name)
                    .map_err(|e| std::io::Error::new(e.kind(), format!("{name}: {e}")))?
                    .to_string_lossy()
                    .into_owned())
            } else {
                Ok(name.into())
            }
        };
        let pan_name = persistent(pan)?;
        let pan = crate::open_raster_lazy(&pan_name)?.header();
        let mut headers = Vec::new();
        let mut names = Vec::new();
        for (name, _) in spectral {
            let name = persistent(name)?;
            headers.push(crate::open_raster_lazy(&name)?.header());
            names.push(name);
        }
        let refs = spectral.iter().enumerate().map(|(i, (_, band))| (names[i].as_str(), &headers[i], *band)).collect::<Vec<_>>();
        Self::from_rasters(&pan_name, &pan, &refs, options)
    }

    /// Serialize the description, preserving source references and options.
    pub fn to_xml(&self) -> String {
        self.root.to_xml()
    }
    /// Write the description; no source pixels or expanded output are materialized.
    pub fn write(&self, path: &str) -> Result<()> {
        let inputs = std::iter::once(&self.pan)
            .chain(&self.spectral)
            .map(|s| crate::ops::vrt::resolve_reference(path, &s.name, s.relative))
            .collect::<Vec<_>>();
        if crate::ops::strips::overwrites(path, &inputs) {
            return Err(invalid("VRT output would overwrite one of its required sources"));
        }
        std::fs::write(path, self.to_xml()).map_err(Into::into)
    }
    /// Open with optional supplied sources. Keys match SourceFilename exactly.
    pub fn into_lazy(self, base: &Path, sources: HashMap<String, LazyRaster>) -> Result<LazyRaster> {
        self.into_lazy_reference(&base.join("__vrt__.vrt").to_string_lossy(), sources)
    }
    pub(crate) fn into_lazy_reference(self, base: &str, sources: HashMap<String, LazyRaster>) -> Result<LazyRaster> {
        let _depth = OpenDepth::enter()?;
        let mut cache = Cache { base: base.into(), supplied: sources, sources: HashMap::new(), recent: VecDeque::new() };
        let mut headers = Vec::with_capacity(self.spectral.len() + 1);
        headers.push(cache.get(&self.pan)?.header());
        for source in &self.spectral {
            headers.push(cache.get(source)?.header());
        }
        let refs =
            headers[1..].iter().zip(&self.spectral).map(|(dataset, src)| SpectralBand { dataset, band: src.band }).collect::<Vec<_>>();
        let layout = plan(&headers[0], &refs, &self.options)?;
        for (name, expected) in [("rasterXSize", layout.pan.grid.width), ("rasterYSize", layout.pan.grid.height)] {
            if let Some(v) = self.root.attr(name)
                && number::<usize>(v, name)? != expected
            {
                return Err(invalid("declared dimensions disagree with pan footprint"));
            }
        }
        if self.outputs.iter().any(|output| matches!(output, Output::Pan(band) if *band > headers[0].raster.bands)) {
            return Err(invalid("pan passthrough band exceeds source band count"));
        }
        let mut grid = layout.pan.grid;
        if let Some(e) = self.root.child("GeoTransform") {
            let values = e.text.split(',').map(|v| number::<f64>(v, "GeoTransform")).collect::<Result<Vec<_>>>()?;
            if values.iter().any(|v| !v.is_finite()) {
                return Err(invalid("GeoTransform must be finite"));
            }
            grid.geo_transform = values.try_into().map_err(|_| invalid("GeoTransform needs six numbers"))?;
        }
        let srs =
            self.root.child("SRS").map(|e| crate::crs::SpatialRef::parse(e.text.trim())).transpose()?.unwrap_or_else(|| headers[0].crs());
        let mut header = crate::ops::raster_header(grid, layout.output_type, self.outputs.len(), layout.nodata, &srs)?;
        header.band_nodata = self
            .outputs
            .iter()
            .enumerate()
            .map(|(i, output)| {
                let declared = self
                    .root
                    .children
                    .iter()
                    .find(|e| e.name == "VRTRasterBand" && e.attr("band").and_then(|v| v.trim().parse::<usize>().ok()) == Some(i + 1));
                declared.and_then(|e| e.child("NoDataValue")).map(|e| number::<f64>(&e.text, "output nodata")).transpose().map(|value| {
                    value.or(match output {
                        Output::Sharp(_) => layout.nodata,
                        Output::Pan(_) => None,
                    })
                })
            })
            .collect::<Result<Vec<_>>>()?;
        header.driver = crate::formats::RasterDriver::VRT;
        if let Some(metadata) = self.root.child("Metadata") {
            header.raster.metadata =
                metadata.children.iter().filter_map(|e| e.attr("key").map(|key| (key.to_string(), e.text.clone()))).collect();
        }
        Ok(LazyRaster::from_source(header, Box::new(Reader { description: self, layout, headers, cache })))
    }
}

struct Cache {
    base: String,
    sources: HashMap<String, LazyRaster>,
    supplied: HashMap<String, LazyRaster>,
    recent: VecDeque<String>,
}
impl Cache {
    fn get(&mut self, source: &Source) -> Result<&mut LazyRaster> {
        if self.supplied.contains_key(&source.name) {
            return Ok(self.supplied.get_mut(&source.name).expect("supplied source"));
        }
        let key = crate::ops::vrt::resolve_reference(&self.base, &source.name, source.relative);
        if !self.sources.contains_key(&key) {
            let raster = crate::open_raster_lazy(&key)?;
            if self.sources.len() >= 32
                && let Some(name) = self.recent.pop_front()
            {
                self.sources.remove(&name);
            }
            self.sources.insert(key.clone(), raster);
        }
        self.recent.retain(|name| name != &key);
        self.recent.push_back(key.clone());
        Ok(self.sources.get_mut(&key).expect("inserted source"))
    }
}
struct Reader {
    description: PansharpenVrt,
    layout: Plan,
    headers: Vec<RasterDataset>,
    cache: Cache,
}
impl WindowSource for Reader {
    fn overviews(&self) -> Vec<(usize, usize)> {
        Vec::new()
    }
    fn block_size(&self) -> Option<(usize, usize)> {
        // XML block sizes are validated hints; request processing uses fixed,
        // bounded internal blocks independently of those hints.
        Some((blocks::BLOCK, blocks::BLOCK))
    }
    fn read(&mut self, _level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
        let sharp = bands
            .iter()
            .filter_map(|b| match self.description.outputs[*b - 1] {
                Output::Sharp(i) => Some(i),
                Output::Pan(_) => None,
            })
            .collect::<Vec<_>>();
        let size = window.width.checked_mul(window.height).ok_or_else(|| invalid("pansharpen window size overflow"))?;
        let count = size.checked_mul(bands.len()).ok_or_else(|| invalid("pansharpen output size overflow"))?;
        let mut result = blocks::allocate(self.layout.output_type, count)?;
        let values = if sharp.is_empty() {
            None
        } else {
            let layout = &self.layout;
            let options = &self.description.options;
            let headers = &self.headers;
            let description = &self.description;
            let cache = std::cell::RefCell::new(&mut self.cache);
            Some(blocks::request(
                layout,
                options,
                window,
                &sharp,
                |index, source_layout, area| {
                    let src = if index == 0 { &description.pan } else { &description.spectral[index - 1] };
                    blocks::aligned(
                        source_layout,
                        area,
                        |original| {
                            cache.borrow_mut().get(src)?.read_window(original, &WindowOptions { bands: vec![src.band], overview: None })
                        },
                        &headers[index],
                    )
                },
                |index, _, area| {
                    let src = &description.spectral[index - 1];
                    cache.borrow_mut().get(src)?.read_mask(area, None)
                },
            )?)
        };
        let mut pan_bands = bands
            .iter()
            .filter_map(|b| match self.description.outputs[*b - 1] {
                Output::Pan(band) => Some(band),
                Output::Sharp(_) => None,
            })
            .collect::<Vec<_>>();
        pan_bands.sort_unstable();
        pan_bands.dedup();
        let pan = if pan_bands.is_empty() {
            None
        } else {
            let source = &self.description.pan;
            let cache = &mut self.cache;
            Some(
                blocks::aligned_bands(
                    &self.layout.pan,
                    window,
                    |area| cache.get(source)?.read_window(area, &WindowOptions { bands: pan_bands.clone(), overview: None }),
                    &self.headers[0],
                    pan_bands.len(),
                )?
                .raster
                .data,
            )
        };
        let mut sharp_index = 0;
        for (out, band) in bands.iter().enumerate() {
            match self.description.outputs[*band - 1] {
                Output::Sharp(_) => {
                    let data = values.as_ref().expect("sharp planes");
                    blocks::copy_samples(&mut result, out * size, data, sharp_index * size, size);
                    sharp_index += 1;
                }
                Output::Pan(band) => {
                    let index = pan_bands.binary_search(&band).expect("requested pan band");
                    blocks::copy_samples(&mut result, out * size, pan.as_ref().expect("pan planes"), index * size, size);
                }
            }
        }
        Ok(result)
    }
}

thread_local! {static OPEN_DEPTH:std::cell::Cell<usize>=const{std::cell::Cell::new(0)};}
struct OpenDepth;
impl OpenDepth {
    fn enter() -> Result<Self> {
        OPEN_DEPTH.with(|depth| {
            if depth.get() >= 32 {
                return Err(invalid("recursive source graph"));
            }
            depth.set(depth.get() + 1);
            Ok(Self)
        })
    }
}
impl Drop for OpenDepth {
    fn drop(&mut self) {
        OPEN_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

/// Pansharpen persistent inputs to an output, with bounded windows for GeoTIFF/COG output.
/// If output overwrites an input, the result is gathered before replacing that file.
pub fn pansharpen_to_path(
    pan: &str,
    spectral: &[(String, usize)],
    options: &PansharpenOptions,
    path: &str,
    write_options: &RasterWriteOptions,
) -> Result<()> {
    let description = PansharpenVrt::from_paths(pan, spectral, options)?;
    let names = std::iter::once(pan.to_string()).chain(spectral.iter().map(|(name, _)| name.clone())).collect::<Vec<_>>();
    let mut source = description.into_lazy(Path::new(""), HashMap::new())?;
    if crate::ops::strips::overwrites(path, &names) {
        return crate::write_raster_dataset(&source.read()?, path, write_options);
    }
    let header = source.header();
    let mut writer = crate::ops::strips::StripWriter::create(&header, path, write_options)?;
    let bands = (1..=source.band_count()).collect::<Vec<_>>();
    let rows = crate::ops::strips::small_strip_rows(source.width(), bands.len(), source.data_type().size_bytes(), Some(512));
    for y in (0..source.height()).step_by(rows) {
        let height = rows.min(source.height() - y);
        let data = source
            .read_window(Window::new(0, y, source.width(), height), &WindowOptions { bands: bands.clone(), overview: None })?
            .raster
            .data;
        writer.write(data, height)?;
    }
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::vrt::VirtualRaster;
    use crate::ops::{Grid, raster_header};
    use std::sync::{Arc, Mutex};
    use wbraster::DataType;

    struct Counted {
        requests: Arc<Mutex<Vec<Window>>>,
        value: u16,
    }
    impl WindowSource for Counted {
        fn overviews(&self) -> Vec<(usize, usize)> {
            Vec::new()
        }
        fn read(&mut self, _: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
            self.requests.lock().unwrap().push(window);
            Ok(RasterData::U16(vec![self.value; window.width * window.height * bands.len()]))
        }
    }
    fn header(size: usize, resolution: f64) -> RasterDataset {
        raster_header(
            Grid { width: size, height: size, geo_transform: [0., resolution, 0., 2048., 0., -resolution] },
            DataType::U16,
            1,
            None,
            &Default::default(),
        )
        .unwrap()
    }
    fn passthrough_xml(dtype: &str, pan_relative: bool, output_relative: bool) -> String {
        format!(
            r#"<VRTDataset subClass="VRTPansharpenedDataset">
        <VRTRasterBand band="1" dataType="{dtype}"><SimpleSource><SourceFilename relativeToVRT="{}">pan</SourceFilename><SourceBand>2</SourceBand></SimpleSource></VRTRasterBand>
        <VRTRasterBand band="2" dataType="{dtype}"><SimpleSource><SourceFilename relativeToVRT="{}">pan</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand>
        <VRTRasterBand band="3" dataType="{dtype}"><SimpleSource><SourceFilename relativeToVRT="{}">pan</SourceFilename><SourceBand>2</SourceBand></SimpleSource></VRTRasterBand>
        <VRTRasterBand band="4" dataType="{dtype}" subClass="VRTPansharpenedRasterBand"/>
        <PansharpeningOptions><PanchroBand><SourceFilename relativeToVRT="{}">pan</SourceFilename></PanchroBand>
        <SpectralBand dstBand="4"><SourceFilename>spectral</SourceFilename></SpectralBand></PansharpeningOptions></VRTDataset>"#,
            u8::from(output_relative),
            u8::from(output_relative),
            u8::from(output_relative),
            u8::from(pan_relative)
        )
    }

    #[test]
    fn passthrough_source_identity_includes_reference_mode() {
        for relative in [false, true] {
            assert!(PansharpenVrt::from_xml(&passthrough_xml("UInt16", relative, relative)).is_ok());
            assert!(
                PansharpenVrt::from_xml(&passthrough_xml("UInt16", relative, !relative))
                    .unwrap_err()
                    .to_string()
                    .contains("only pan-source passthrough")
            );
        }
    }

    #[test]
    fn passthrough_batches_native_int64_planes_without_precision_loss() {
        type Calls = Arc<Mutex<Vec<(Window, Vec<usize>)>>>;
        struct Native {
            calls: Calls,
            signed: bool,
        }
        impl WindowSource for Native {
            fn overviews(&self) -> Vec<(usize, usize)> {
                Vec::new()
            }
            fn read(&mut self, _: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
                self.calls.lock().unwrap().push((window, bands.to_vec()));
                let base = 9_007_199_254_741_001u64;
                let values = bands.iter().flat_map(|b| vec![base + *b as u64; window.width * window.height]).collect::<Vec<_>>();
                Ok(if self.signed { RasterData::I64(values.into_iter().map(|v| -(v as i64)).collect()) } else { RasterData::U64(values) })
            }
        }
        for (dtype, name) in [(DataType::U64, "UInt64"), (DataType::I64, "Int64")] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut pan = header(2, 1.);
            pan.raster.data_type = dtype;
            pan.raster.bands = 2;
            let sources = HashMap::from([
                ("pan".into(), LazyRaster::from_source(pan, Box::new(Native { calls: calls.clone(), signed: dtype == DataType::I64 }))),
                ("spectral".into(), LazyRaster::from(header(2, 1.))),
            ]);
            let mut reader =
                PansharpenVrt::from_xml(&passthrough_xml(name, false, false)).unwrap().into_lazy(Path::new(""), sources).unwrap();
            assert!(calls.lock().unwrap().is_empty());
            let output = reader.read_window(Window::new(0, 0, 2, 2), &WindowOptions { bands: vec![3, 1, 2], overview: None }).unwrap();
            let expected = [vec![9_007_199_254_741_003u64; 4], vec![9_007_199_254_741_003; 4], vec![9_007_199_254_741_002; 4]].concat();
            match output.raster.data {
                RasterData::U64(values) => assert_eq!(values, expected),
                RasterData::I64(values) => assert_eq!(values, expected.into_iter().map(|v| -(v as i64)).collect::<Vec<_>>()),
                _ => panic!("native passthrough type changed"),
            }
            assert_eq!(*calls.lock().unwrap(), vec![(Window::new(0, 0, 2, 2), vec![1, 2])]);
        }
    }

    #[test]
    fn malformed_blocks_and_zero_destination_are_rejected() {
        let h = header(2, 1.);
        let xml = PansharpenVrt::from_rasters("pan", &h, &[("spectral", &h, 1)], &Default::default()).unwrap().to_xml();
        assert!(PansharpenVrt::from_xml(&xml.replace("dstBand=\"1\"", "dstBand=\"0\"")).is_err());
        let subset = PansharpenVrt::from_rasters(
            "pan",
            &h,
            &[("red", &h, 1), ("nir", &h, 1)],
            &PansharpenOptions { output_bands: vec![1], ..Default::default() },
        )
        .unwrap();
        assert_eq!(subset.spectral.len(), 2);
        assert_eq!(subset.outputs.len(), 1);
        for name in ["BlockXSize", "BlockYSize"] {
            for value in ["abc", "0", "-1", ""] {
                assert!(
                    PansharpenVrt::from_xml(&xml.replace(&format!("<{name}>512</{name}>"), &format!("<{name}>{value}</{name}>"))).is_err()
                );
            }
            assert!(PansharpenVrt::from_xml(&xml.replace(&format!("<{name}>512</{name}>"), &format!("<{name}> 32 </{name}>"))).is_ok());
        }
    }

    #[test]
    fn empty_band_request_still_checks_window_size_overflow() {
        let h = header(2, 1.);
        let description = PansharpenVrt::from_rasters("pan", &h, &[("spectral", &h, 1)], &Default::default()).unwrap();
        let layout = plan(&h, &[SpectralBand { dataset: &h, band: 1 }], &description.options).unwrap();
        let mut reader = Reader {
            description,
            layout,
            headers: vec![h.clone(), h],
            cache: Cache { base: String::new(), sources: HashMap::new(), supplied: HashMap::new(), recent: VecDeque::new() },
        };
        assert!(reader.read(0, Window::new(0, 0, usize::MAX, 2), &[]).unwrap_err().to_string().contains("window size overflow"));
    }

    #[test]
    fn missing_local_source_error_includes_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-pan.tif");
        let error = PansharpenVrt::from_paths(path.to_str().unwrap(), &[("unused".into(), 1)], &Default::default()).unwrap_err();
        assert!(error.to_string().contains(path.to_str().unwrap()));
    }

    #[test]
    fn source_cache_keeps_recent_handles_bounded_and_supplied_sources_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache {
            base: dir.path().join("description.vrt").to_string_lossy().into_owned(),
            sources: HashMap::new(),
            supplied: HashMap::from([("uploaded".into(), LazyRaster::from(header(1, 1.)))]),
            recent: VecDeque::new(),
        };
        let sources = (0..33)
            .map(|i| {
                let name = format!("source{i}.asc");
                std::fs::write(dir.path().join(&name), "ncols 1\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n7\n").unwrap();
                Source { name, relative: true, band: 1 }
            })
            .collect::<Vec<_>>();
        for source in &sources[..32] {
            cache.get(source).unwrap();
        }
        // A cache hit must reuse its handle even when the source can no longer be opened.
        cache.get(&sources[0]).unwrap();
        std::fs::remove_file(dir.path().join(&sources[0].name)).unwrap();
        cache.get(&sources[32]).unwrap();
        assert_eq!(cache.sources.len(), 32);
        assert_eq!(cache.recent.len(), 32);
        let evicted = crate::ops::vrt::resolve_reference(&cache.base, &sources[1].name, true);
        assert!(!cache.sources.contains_key(&evicted));
        assert_eq!(cache.get(&sources[0]).unwrap().width(), 1);
        assert_eq!(cache.get(&Source { name: "uploaded".into(), relative: false, band: 1 }).unwrap().width(), 1);
        assert_eq!(cache.supplied.len(), 1);
        assert_eq!(cache.recent.len(), 32);
    }
    #[test]
    fn source_cache_distinguishes_resolved_relative_and_unbased_names() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        let name = format!("{}/source.asc", dir.path().file_name().unwrap().to_string_lossy());
        let base = dir.path().join("nested/description.vrt");
        let relative = base.parent().unwrap().join(&name);
        std::fs::create_dir_all(relative.parent().unwrap()).unwrap();
        std::fs::write(dir.path().join("source.asc"), "ncols 1\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n7\n").unwrap();
        std::fs::write(relative, "ncols 2\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n8 9\n").unwrap();
        let mut cache =
            Cache { base: base.to_string_lossy().into_owned(), sources: HashMap::new(), supplied: HashMap::new(), recent: VecDeque::new() };
        assert_eq!(cache.get(&Source { name: name.clone(), relative: false, band: 1 }).unwrap().width(), 1);
        assert_eq!(cache.get(&Source { name, relative: true, band: 1 }).unwrap().width(), 2);
        assert_eq!(cache.sources.len(), 2);
    }
    #[test]
    fn writing_relative_description_cannot_overwrite_a_required_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pan.asc");
        std::fs::write(&path, "required source pixels").unwrap();
        let h = header(1, 1.);
        let description = PansharpenVrt::from_rasters("pan.asc", &h, &[("spectral.asc", &h, 1)], &Default::default()).unwrap();
        let xml = description.to_xml().replace("relativeToVRT=\"0\"", "relativeToVRT=\"1\"");
        let relative = PansharpenVrt::from_xml(&xml).unwrap();
        assert!(relative.write(path.to_str().unwrap()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "required source pixels");
    }
    #[test]
    fn whitespace_band_numbers_keep_declared_type_and_nodata() {
        let h = header(1, 1.);
        let options = PansharpenOptions { output_type: Some(DataType::F64), nodata: PansharpenNodata::Value(-9.), ..Default::default() };
        let description = PansharpenVrt::from_rasters("pan", &h, &[("spectral", &h, 1)], &options).unwrap();
        for number in [" 1 ", "01", "+1"] {
            let xml = description
                .to_xml()
                .replace("band=\"1\"", &format!("band=\"{number}\""))
                .replace("<NoDataValue>-9</NoDataValue>", "<NoDataValue>-17</NoDataValue>");
            let sources = HashMap::from([("pan".into(), LazyRaster::from(h.clone())), ("spectral".into(), LazyRaster::from(h.clone()))]);
            let lazy = PansharpenVrt::from_xml(&xml).unwrap().into_lazy(Path::new(""), sources).unwrap();
            assert_eq!(lazy.data_type(), DataType::F64);
            assert_eq!(lazy.band_nodata(1), Some(-17.));
        }
    }
    #[test]
    fn supplied_sources_open_without_pixels_and_read_only_window_halos() {
        let pan = header(2048, 1.);
        let spectral = header(1024, 2.);
        let o = PansharpenOptions { output_bands: vec![2, 1, 2], ..Default::default() };
        let description = PansharpenVrt::from_rasters("pan", &pan, &[("red", &spectral, 1), ("green", &spectral, 1)], &o).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sources = [("pan", pan, 120), ("red", spectral.clone(), 40), ("green", spectral, 80)]
            .into_iter()
            .map(|(name, header, value)| {
                (name.into(), LazyRaster::from_source(header, Box::new(Counted { requests: requests.clone(), value })))
            })
            .collect();
        let xml = description.to_xml();
        let mut lazy = VirtualRaster::from_xml(&xml).unwrap().into_lazy(Path::new(""), sources).unwrap();
        assert!(requests.lock().unwrap().is_empty());
        assert_eq!(lazy.band_count(), 3);
        let output = lazy.read_window(Window::new(1100, 1200, 12, 10), &WindowOptions { bands: vec![3, 2], overview: None }).unwrap();
        assert_eq!(output.raster.data.to_f64_vec(), [vec![160.; 120], vec![80.; 120]].concat());
        let calls = requests.lock().unwrap();
        assert!(calls.len() <= 5);
        assert!(calls.iter().all(|w| w.width <= 16 && w.height <= 16));
    }
    #[test]
    fn direct_masks_are_used_but_temporary_and_nodata_wrappers_drop_them() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Masked {
            reads: Arc<AtomicUsize>,
            value: u16,
        }
        impl WindowSource for Masked {
            fn overviews(&self) -> Vec<(usize, usize)> {
                Vec::new()
            }
            fn has_mask(&self) -> bool {
                true
            }
            fn read_mask(&mut self, _: usize, w: Window) -> Result<Option<Vec<u8>>> {
                self.reads.fetch_add(1, Ordering::Relaxed);
                Ok(Some(vec![0; w.width * w.height]))
            }
            fn read(&mut self, _: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
                Ok(RasterData::U16(vec![self.value; w.width * w.height * bands.len()]))
            }
        }
        for (nodata, resampling, expected, mask_reads, pan_size, pan_resolution) in [
            (PansharpenNodata::Auto, Resampling::Cubic, 0., 2, 4, 1.),
            (PansharpenNodata::None, Resampling::Cubic, 0., 2, 4, 1.),
            (PansharpenNodata::Value(0.), Resampling::Cubic, 53., 0, 4, 1.),
            (PansharpenNodata::None, Resampling::Nearest, 53., 0, 4, 1.),
            (PansharpenNodata::Auto, Resampling::Cubic, 53., 0, 2, 2.),
        ] {
            let pan = header(pan_size, pan_resolution);
            let spectral = header(2, 2.);
            let options = PansharpenOptions { nodata, resampling, ..Default::default() };
            let description =
                PansharpenVrt::from_rasters("pan", &pan, &[("red", &spectral, 1), ("green", &spectral, 1)], &options).unwrap();
            let reads = Arc::new(AtomicUsize::new(0));
            let sources = HashMap::from([
                ("pan".into(), LazyRaster::from_source(pan, Box::new(Counted { requests: Arc::new(Mutex::new(Vec::new())), value: 80 }))),
                ("red".into(), LazyRaster::from_source(spectral.clone(), Box::new(Masked { reads: reads.clone(), value: 20 }))),
                ("green".into(), LazyRaster::from_source(spectral, Box::new(Masked { reads: reads.clone(), value: 40 }))),
            ]);
            let mut lazy = description.into_lazy(Path::new(""), sources).unwrap();
            assert_eq!(lazy.read().unwrap().raster.data.get_f64(0), 53.);
            assert_eq!(reads.load(Ordering::Relaxed), 0);
            assert_eq!(
                lazy.read_window(Window::new(if pan_size == 2 { 0 } else { 1 }, 1, 2, 1), &Default::default())
                    .unwrap()
                    .raster
                    .data
                    .get_f64(0),
                expected
            );
            assert_eq!(reads.load(Ordering::Relaxed), mask_reads);
        }
    }

    #[test]
    fn average_fractional_request_clips_to_gdal_temporary_buffer() {
        let pan = crate::ops::build_raster(
            Grid { width: 8, height: 8, geo_transform: [0., 1., 0., 8., 0., -1.] },
            vec![vec![120.; 64]],
            DataType::U16,
            None,
            &Default::default(),
            |v, _| v,
        )
        .unwrap();
        let spectral = crate::ops::build_raster(
            Grid { width: 4, height: 4, geo_transform: [-0.5, 2., 0., 8.5, 0., -2.] },
            vec![(0..16).map(|v| (v % 4 + v / 4 + 2) as f64 * 10.).collect(), vec![30.; 16]],
            DataType::U16,
            None,
            &Default::default(),
            |v, _| v,
        )
        .unwrap();
        let o = PansharpenOptions { resampling: Resampling::Average, extent: PansharpenExtent::None, ..Default::default() };
        let description = PansharpenVrt::from_rasters("pan", &pan, &[("spectral", &spectral, 1), ("spectral", &spectral, 2)], &o).unwrap();
        let sources = HashMap::from([("pan".into(), LazyRaster::from(pan)), ("spectral".into(), LazyRaster::from(spectral))]);
        let mut lazy = description.into_lazy(Path::new(""), sources).unwrap();
        let out = lazy.read_window(Window::new(1, 1, 5, 5), &Default::default()).unwrap().raster.data.to_f64_vec();
        assert_eq!(out[0], 120.);
        assert_eq!(out[4], 144.);
        assert_eq!(out[24], 160.);
        // A one-column request does not use GDAL's MEM extraction path.
        let column = lazy.read_window(Window::new(1, 1, 1, 5), &Default::default()).unwrap();
        assert_eq!(column.raster.data.get_f64(4), 150.);
    }

    #[test]
    fn huge_virtual_union_can_open_and_read_a_small_window() {
        assert!(blocks::allocate(DataType::F64, usize::MAX).is_err());
        let pan = crate::ops::build_raster(
            Grid { width: 1, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
            vec![vec![80.]],
            DataType::U8,
            None,
            &Default::default(),
            |v, _| v,
        )
        .unwrap();
        let mut spectral = pan.clone();
        spectral.set_geo_transform([1_500_000_000., 1., 0., 1_500_000_001., 0., -1.]);
        let description = PansharpenVrt::from_rasters("pan", &pan, &[("spectral", &spectral, 1)], &Default::default()).unwrap();
        let sources = HashMap::from([("pan".into(), LazyRaster::from(pan)), ("spectral".into(), LazyRaster::from(spectral))]);
        let mut lazy = description.into_lazy(Path::new(""), sources).unwrap();
        assert!(lazy.width() > 1_000_000_000);
        assert_eq!(lazy.read_window(Window::new(0, 1_500_000_000, 1, 1), &Default::default()).unwrap().raster.data.to_f64_vec(), vec![0.]);
    }

    #[cfg(feature = "netcdf-hdf5")]
    #[test]
    fn persistent_subdataset_names_open_and_round_trip() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        let file = dir.path().join("spectral.nc");
        let dataset = crate::ops::build_raster(
            Grid { width: 2, height: 2, geo_transform: [0., 1., 0., 2., 0., -1.] },
            vec![vec![80.; 4]],
            DataType::U16,
            None,
            &Default::default(),
            |v, _| v,
        )
        .unwrap();
        crate::write_raster_dataset(&dataset, file.to_str().unwrap(), &Default::default()).unwrap();
        let path = dir.path().join("nested/sharp.vrt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for name in [file.as_path(), file.strip_prefix(&cwd).unwrap()] {
            let selector = format!("NETCDF:\"{}\":Band1", name.display());
            let description = PansharpenVrt::from_paths(&selector, &[(selector.clone(), 1)], &Default::default()).unwrap();
            description.write(path.to_str().unwrap()).unwrap();
            let mut lazy = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
            assert_eq!(lazy.read().unwrap().raster.data.to_f64_vec(), vec![80.; 4]);
        }
        assert_eq!(
            crate::ops::vrt::resolve_reference("/data/scene/description.vrt", "NETCDF:\"inputs.nc\":Band1", true),
            "NETCDF:\"/data/scene/inputs.nc\":Band1"
        );
    }

    #[test]
    fn minimal_xml_and_passthrough_band() {
        let text = "<VRTDataset subClass=\"VRTPansharpenedDataset\"><VRTRasterBand band=\"1\" dataType=\"UInt16\"><SimpleSource><SourceFilename>pan</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand><VRTRasterBand band=\"2\" dataType=\"UInt16\" subClass=\"VRTPansharpenedRasterBand\"/><PansharpeningOptions><PanchroBand><SourceFilename>pan</SourceFilename><SourceBand>1</SourceBand></PanchroBand><SpectralBand dstBand=\"2\"><SourceFilename>spectral</SourceFilename><SourceBand>1</SourceBand></SpectralBand></PansharpeningOptions></VRTDataset>";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sources = [("pan", header(2048, 1.), 120), ("spectral", header(1024, 2.), 40)]
            .into_iter()
            .map(|(name, header, value)| {
                (name.into(), LazyRaster::from_source(header, Box::new(Counted { requests: requests.clone(), value })))
            })
            .collect();
        let mut lazy = VirtualRaster::from_xml(text).unwrap().into_lazy(Path::new(""), sources).unwrap();
        let values = lazy.read_window(Window::new(200, 300, 2, 2), &Default::default()).unwrap().raster.data.to_f64_vec();
        assert_eq!(values, vec![120.; 8]);
    }
}
