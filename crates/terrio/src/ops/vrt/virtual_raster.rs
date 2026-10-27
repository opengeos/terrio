//! Lazy, serialized VRT operations and supplied-source resolution for WebAssembly.

use super::{VrtDataset, VrtSource, xml};
use crate::cog::WindowSource;
use crate::ops::warp::{Nodata, WarpOptions};
use crate::ops::{BandView, Grid, Resampling};
use crate::{Error, LazyRaster, Result, Window, WindowOptions};
use std::collections::HashMap;
use std::path::Path;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// A serialized virtual raster. Source files are opened only when their pixels are requested.
/// Built-in derived bands, kernel sources and affine/reprojected warped VRTs are supported.
#[derive(Debug, Clone)]
pub struct VirtualRaster {
    root: xml::Element,
    layout: VrtDataset,
    warped: bool,
    pansharpen: Option<crate::ops::pansharpen::vrt::PansharpenVrt>,
}

impl VirtualRaster {
    /// Parse a serialized VRT without opening its sources or executing embedded code.
    pub fn from_xml(text: &str) -> Result<Self> {
        let root = xml::parse(text)?;
        if root.attr("subClass") == Some("VRTPansharpenedDataset") {
            let pansharpen = crate::ops::pansharpen::vrt::PansharpenVrt::from_xml(text)?;
            // Pansharpen dimensions may be inferred from source headers at lazy open.
            let layout = VrtDataset { width: 0, height: 0, geo_transform: None, srs: None, bands: Vec::new() };
            return Ok(Self { root, layout, warped: false, pansharpen: Some(pansharpen) });
        }
        let mut plain = root.clone();
        let warped = match root.attr("subClass") {
            None => false,
            Some("VRTWarpedDataset") => true,
            Some(v) => return Err(unsupported(v)),
        };
        plain.attrs.retain(|(k, _)| k != "subClass");
        for band in plain.children.iter_mut().filter(|e| e.name == "VRTRasterBand") {
            match band.attr("subClass") {
                None | Some("VRTWarpedRasterBand") => {}
                Some("VRTDerivedRasterBand") => validate_function(band)?,
                Some(v) => return Err(unsupported(v)),
            }
            band.attrs.retain(|(k, _)| k != "subClass");
            for source in &mut band.children {
                if source.name == "KernelFilteredSource" {
                    validate_kernel(source)?;
                    source.name = "ComplexSource".into();
                }
                if let Some(b) = source.children.iter_mut().find(|e| e.name == "SourceBand")
                    && let Some(number) = b.text.trim().strip_prefix("mask,")
                {
                    b.text = number.to_string();
                }
            }
        }
        let layout = VrtDataset::from_xml(&plain.to_xml())?;
        if layout.width == 0 || layout.height == 0 || layout.width.checked_mul(layout.height).is_none() {
            return Err(invalid("invalid raster dimensions"));
        }
        if warped {
            let options = root.child("GDALWarpOptions").ok_or_else(|| invalid("warped raster has no GDALWarpOptions"))?;
            for name in ["Cutline", "SrcAlphaBand", "DstAlphaBand"] {
                if options.child(name).is_some() {
                    return Err(unsupported(&format!("warped {name}")));
                }
            }
            if descendant(options, "SrcGeoLocTransformer").is_some() || descendant(options, "GeoLocTransformer").is_some() {
                return Err(unsupported("serialized geolocation transformer"));
            }
            for option in options.children.iter().filter(|e| e.name == "Option" && e.attr("name") == Some("INIT_DEST")) {
                if option.text.trim() != "NO_DATA" && !(option.text.trim() == "0" && layout.bands[0].nodata.is_none()) {
                    return Err(unsupported("custom warped INIT_DEST"));
                }
            }
        }
        Ok(Self { root, layout, warped, pansharpen: None })
    }

    /// Create a serialized warp without reading the source pixels.
    /// Paths are made absolute so the description can be written in any directory.
    pub fn warped(source: &str, options: &WarpOptions) -> Result<Self> {
        if options.cutline.is_some() {
            return Err(unsupported("serialized cutlines"));
        }
        let src = crate::open_raster_lazy(source)?;
        let header = src.header();
        let options = crate::ops::warp::with_file_gcps(options, source, &header)?;
        let options = options.as_ref();
        if options.geoloc
            || options.geolocation.is_some()
            || (header.geolocation.is_some()
                && options.gcps.is_empty()
                && options.rpc.is_none()
                && options.homography.is_none()
                && options.src_transform.is_none())
        {
            return Err(unsupported("serialized geolocation transformer"));
        }
        let target = crate::ops::warp::virtual_header(&header, options)?;
        let levels = src.overviews();
        let source_overview = crate::ops::warp::virtual_source_overview(&header, &levels, options)?;
        let (scale_x, scale_y) = if let Some(level) = source_overview {
            let (width, height) = levels[level];
            if width == 0 || height == 0 {
                return Err(invalid("source overview has zero dimensions"));
            }
            (header.raster.cols as f64 / width as f64, header.raster.rows as f64 / height as f64)
        } else {
            (1., 1.)
        };
        let mut source_options = options.clone();
        for point in &mut source_options.gcps {
            point.pixel /= scale_x;
            point.line /= scale_y;
        }
        if let Some(rpc) = &mut source_options.rpc {
            rpc.samp_off /= scale_x;
            rpc.samp_scale /= scale_x;
            rpc.line_off /= scale_y;
            rpc.line_scale /= scale_y;
        }
        if let Some(matrix) = &mut source_options.homography {
            // H maps source pixels to georeferenced coordinates. Its first two
            // columns scale when the serialized source becomes a stored level.
            for row in 0..3 {
                matrix[row * 3] *= scale_x;
                matrix[row * 3 + 1] *= scale_y;
            }
        }
        let open_options = source_overview
            .map(|level| {
                let only = if matches!(options.overview, crate::ops::overview::OverviewSelection::Level(_)) { " only" } else { "" };
                format!("<OpenOptions><OOI key=\"OVERVIEW_LEVEL\">{level}{only}</OOI></OpenOptions>")
            })
            .unwrap_or_default();
        let source_level = match options.overview {
            crate::ops::overview::OverviewSelection::Auto => "AUTO".into(),
            crate::ops::overview::OverviewSelection::AutoOffset(offset) => format!("AUTO-{offset}"),
            crate::ops::overview::OverviewSelection::None => "NONE".into(),
            crate::ops::overview::OverviewSelection::Level(level) => level.to_string(),
        };
        let subdataset =
            ["NETCDF:", "HDF5:", "ZARR:", "GPKG:"].iter().any(|p| source.get(..p.len()).is_some_and(|v| v.eq_ignore_ascii_case(p)));
        let source = if subdataset {
            resolve_reference(&std::env::current_dir()?.join("__vrt__.vrt").to_string_lossy(), source, true)
        } else if crate::vfs::Location::parse(source).is_local() {
            std::fs::canonicalize(source)?.to_string_lossy().into_owned()
        } else {
            source.to_string()
        };
        let src_srs = options.src_srs.clone().unwrap_or_else(|| {
            if options.rpc.is_some() { crate::crs::SpatialRef::from_epsg(4326).unwrap_or_default() } else { header.crs() }
        });
        let dst_srs = target.crs();
        let srs = |s: &crate::crs::SpatialRef| s.to_wkt().or_else(|| s.authority_code()).unwrap_or_default();
        let gt = |v: [f64; 6]| v.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",");
        let original_gt = options.src_transform.unwrap_or_else(|| header.geo_transform());
        let srcgt = [
            original_gt[0],
            original_gt[1] * scale_x,
            original_gt[2] * scale_y,
            original_gt[3],
            original_gt[4] * scale_x,
            original_gt[5] * scale_y,
        ];
        let dstgt = target.geo_transform();
        let mut bands = String::new();
        let mut mappings = String::new();
        for band in 1..=header.raster.bands {
            let dest_nodata = target.band_nodata(band);
            let nd = dest_nodata.map(|v| format!("<NoDataValue>{}</NoDataValue>", super::fmt_number(v))).unwrap_or_default();
            bands += &format!(
                "<VRTRasterBand band=\"{band}\" dataType=\"{}\" subClass=\"VRTWarpedRasterBand\">{nd}</VRTRasterBand>",
                crate::ops::data_type_name(target.raster.data_type)
            );
            let srcnd = match &options.src_nodata {
                Nodata::None => None,
                Nodata::Auto => header.band_nodata(band),
                Nodata::Values(v) => v.get(band - 1).or(v.last()).copied(),
                Nodata::PerBand(v) => v.get(band - 1).or(v.last()).copied().flatten(),
            };
            let srcnd = srcnd
                .map(|v| format!("<SrcNoDataReal>{}</SrcNoDataReal><SrcNoDataImag>0</SrcNoDataImag>", super::fmt_number(v)))
                .unwrap_or_default();
            let dstnd = dest_nodata
                .map(|v| format!("<DstNoDataReal>{}</DstNoDataReal><DstNoDataImag>0</DstNoDataImag>", super::fmt_number(v)))
                .unwrap_or_default();
            mappings += &format!("<BandMapping src=\"{band}\" dst=\"{band}\">{srcnd}{dstnd}</BandMapping>");
        }
        let reprojection = if src_srs.is_unknown() || dst_srs.is_unknown() {
            String::new()
        } else {
            format!(
                "<ReprojectTransformer><ReprojectionTransformer><SourceSRS>{}</SourceSRS><TargetSRS>{}</TargetSRS></ReprojectionTransformer></ReprojectTransformer>",
                xml::escape(&srs(&src_srs)),
                xml::escape(&srs(&dst_srs))
            )
        };
        let georeferencing = if let Some(rpc) = &source_options.rpc {
            serialize_rpc(rpc, &source_options.rpc_options)?
        } else if source_options.homography.is_some() || !source_options.gcps.is_empty() {
            serialize_gcps(&source_options)?
        } else {
            format!(
                "<SrcGeoTransform>{}</SrcGeoTransform><SrcInvGeoTransform>{}</SrcInvGeoTransform>",
                gt(srcgt),
                gt(crate::ops::invert_gt(&srcgt).ok_or_else(|| invalid("singular source transform"))?)
            )
        };
        let transformer = format!(
            "<GenImgProjTransformer>{georeferencing}<DstGeoTransform>{}</DstGeoTransform><DstInvGeoTransform>{}</DstInvGeoTransform>{reprojection}</GenImgProjTransformer>",
            gt(dstgt),
            gt(crate::ops::invert_gt(&dstgt).ok_or_else(|| invalid("singular target transform"))?)
        );
        let transformer = if options.error_threshold > 0. {
            format!(
                "<ApproxTransformer><MaxError>{}</MaxError><BaseTransformer>{transformer}</BaseTransformer></ApproxTransformer>",
                options.error_threshold
            )
        } else {
            transformer
        };
        let unified_option = match options.unified_src_nodata {
            Some(true) => "YES",
            Some(false) => "NO",
            None if matches!(options.src_nodata, Nodata::Values(_)) => "YES",
            None => "PARTIAL",
        };
        let alg = match options.resampling {
            Resampling::Nearest => "NearestNeighbour",
            Resampling::Bilinear => "Bilinear",
            Resampling::Cubic => "Cubic",
            Resampling::CubicSpline => "CubicSpline",
            Resampling::Lanczos => "Lanczos",
            Resampling::Average => "Average",
            Resampling::Mode => "Mode",
            Resampling::Min => "Minimum",
            Resampling::Max => "Maximum",
            Resampling::Med => "Median",
            Resampling::Q1 => "Quartile1",
            Resampling::Q3 => "Quartile3",
            Resampling::Sum => "Sum",
            Resampling::Rms => "RootMeanSquare",
        };
        if options.kernel_version != crate::ops::warp::KernelVersion::Gdal312 || options.transform_options != Default::default() {
            return Err(crate::Error::Unsupported(
                "versioned kernels and strict coordinate-operation options require a materialized warp output".into(),
            ));
        }
        let mut extra_options = String::new();
        if let Some(extra) = options.source_extra {
            extra_options += &format!("<Option name=\"SOURCE_EXTRA\">{extra}</Option>");
        }
        if options.sample_steps_all {
            extra_options += "<Option name=\"SAMPLE_STEPS\">ALL</Option>";
        }
        Self::from_xml(&format!(
            "<VRTDataset rasterXSize=\"{}\" rasterYSize=\"{}\" subClass=\"VRTWarpedDataset\"><SRS>{}</SRS><GeoTransform>{}</GeoTransform>{bands}<BlockXSize>512</BlockXSize><BlockYSize>128</BlockYSize><SrcOvrLevel>{source_level}</SrcOvrLevel><GDALWarpOptions><WarpMemoryLimit>{}</WarpMemoryLimit><ResampleAlg>{alg}</ResampleAlg><WorkingDataType>Unknown</WorkingDataType><Option name=\"INIT_DEST\">NO_DATA</Option><Option name=\"UNIFIED_SRC_NODATA\">{unified_option}</Option>{extra_options}<SourceDataset relativeToVRT=\"0\">{}</SourceDataset>{open_options}<Transformer>{transformer}</Transformer><BandList>{mappings}</BandList></GDALWarpOptions></VRTDataset>",
            target.raster.cols,
            target.raster.rows,
            xml::escape(&srs(&dst_srs)),
            gt(dstgt),
            options.memory_limit,
            xml::escape(&source)
        ))
    }

    /// Serialize the description, preserving transformer definitions and source references.
    pub fn to_xml(&self) -> String {
        self.root.to_xml()
    }

    /// Open with an explicit source map. Keys match SourceFilename/SourceDataset exactly.
    /// This supports uploaded sources in WebAssembly without a filesystem.
    pub fn into_lazy(self, base: &Path, sources: HashMap<String, LazyRaster>) -> Result<LazyRaster> {
        self.into_lazy_reference(&base.join("__vrt__.vrt").to_string_lossy(), sources)
    }

    fn into_lazy_reference(self, base: &str, sources: HashMap<String, LazyRaster>) -> Result<LazyRaster> {
        if let Some(description) = self.pansharpen {
            return description.into_lazy_reference(base, sources);
        }
        let grid = Grid {
            width: self.layout.width,
            height: self.layout.height,
            geo_transform: self.layout.geo_transform.unwrap_or([0., 1., 0., 0., 0., -1.]),
        };
        let first = &self.layout.bands[0];
        let mut header = crate::ops::raster_header(
            grid,
            super::common_type(self.layout.bands.iter().map(|b| b.data_type)),
            self.layout.bands.len(),
            first.nodata,
            &self.layout.srs.clone().unwrap_or_default(),
        )?;
        header.driver = crate::formats::RasterDriver::VRT;
        header.band_nodata = self.layout.bands.iter().map(|b| b.nodata).collect();
        header.band_metadata = self
            .layout
            .bands
            .iter()
            .map(|b| crate::BandMetadata {
                color_interp: Some(b.color_interp.clone().unwrap_or_else(|| "Undefined".into())),
                ..Default::default()
            })
            .collect();
        if let Some(metadata) = self.root.child("Metadata") {
            header.raster.metadata =
                metadata.children.iter().filter_map(|e| e.attr("key").map(|k| (k.to_string(), e.text.clone()))).collect();
        }
        let mask = if let Some(band) = self.root.child("MaskBand").and_then(|e| e.child("VRTRasterBand")) {
            let mut root = self.root.clone();
            root.attrs.retain(|(k, _)| k != "subClass");
            root.children.retain(|e| e.name != "MaskBand" && e.name != "VRTRasterBand" && e.name != "GDALWarpOptions");
            root.children.push(band.clone());
            Some(std::sync::Arc::new(VirtualRaster::from_xml(&root.to_xml())?))
        } else {
            None
        };
        Ok(LazyRaster::from_source(
            header,
            Box::new(Reader {
                description: std::sync::Arc::new(self),
                mask,
                base: base.into(),
                cached: HashMap::new(),
                lru: Default::default(),
                sources,
            }),
        ))
    }
}

/// Open a local, archived or remote VRT and resolve relative sources from its location.
pub fn open_virtual_raster(path: &str) -> Result<LazyRaster> {
    let local = crate::vfs::resolve(path)?;
    let text = String::from_utf8(crate::memfs::read(local.path())?).map_err(|e| invalid(&format!("invalid XML encoding: {e}")))?;
    VirtualRaster::from_xml(&text)?.into_lazy_reference(path, HashMap::new())
}

#[derive(Default)]
struct InventoryState {
    users: usize,
    active: std::collections::HashSet<String>,
    completed: HashMap<String, Vec<String>>,
}
thread_local! { static INVENTORY: std::cell::RefCell<InventoryState> = std::cell::RefCell::new(InventoryState::default()); }

/// One synchronous overwrite-inventory traversal. Nested readers share its memoized
/// descriptions; dropping the outer scope releases every cached path.
pub(crate) struct SourceInventoryScope(std::marker::PhantomData<std::rc::Rc<()>>);
impl SourceInventoryScope {
    pub(crate) fn enter() -> Self {
        INVENTORY.with(|state| state.borrow_mut().users += 1);
        Self(std::marker::PhantomData)
    }
}
impl Drop for SourceInventoryScope {
    fn drop(&mut self) {
        INVENTORY.with(|state| {
            let mut state = state.borrow_mut();
            state.users -= 1;
            if state.users == 0 {
                *state = InventoryState::default();
            }
        });
    }
}

/// Inventory a reference without decoding its pixels. Nested VRT descriptions need only
/// XML reads; ordinary files, subdataset containers and archive files are listed directly.
pub(crate) fn source_input_paths(path: &str) -> Result<Vec<String>> {
    let file = subdataset_parts(path)?.map_or(path, |(_, file, _)| file);
    let location = crate::vfs::Location::parse(file);
    fn local_files(location: &crate::vfs::Location, paths: &mut Vec<String>) {
        match location {
            crate::vfs::Location::Local(file) => {
                paths.push(file.to_string_lossy().into_owned());
                let mask = format!("{}.msk", file.to_string_lossy());
                if Path::new(&mask).is_file() {
                    paths.push(mask);
                }
            }
            crate::vfs::Location::Zip { archive, .. } => local_files(archive, paths),
            _ => {}
        }
    }
    let mut paths = Vec::new();
    local_files(&location, &mut paths);
    if crate::formats::extension(&location.file_name()).as_deref() != Some("vrt") {
        return Ok(paths);
    }
    let _inventory = SourceInventoryScope::enter();
    // Preserve the reference's base: symlink aliases can share XML but resolve
    // relative sources from different directories, just like the reader itself.
    let key = file.to_string();
    let cached = INVENTORY.with(|state| {
        let mut state = state.borrow_mut();
        if state.active.contains(&key) {
            return Err(invalid("cyclic VRT sources"));
        }
        if let Some(paths) = state.completed.get(&key) {
            return Ok(Some(paths.clone()));
        }
        state.active.insert(key.clone());
        Ok(None)
    })?;
    if let Some(paths) = cached {
        return Ok(paths);
    }
    let result = (|| {
        paths.extend(LazyRaster::open(file)?.input_paths()?);
        paths.sort_unstable();
        paths.dedup();
        Ok(paths)
    })();
    INVENTORY.with(|state| {
        let mut state = state.borrow_mut();
        state.active.remove(&key);
        if let Ok(paths) = &result {
            state.completed.insert(key, paths.clone());
        }
    });
    result
}

// Parse the filename once for both relative-reference resolution and overwrite inventory.
// Zarr's unquoted form denotes an entire store; quoted forms may select an array.
fn subdataset_parts(name: &str) -> Result<Option<(&str, &str, &str)>> {
    for prefix in ["NETCDF:", "HDF5:", "ZARR:", "GPKG:"] {
        if name.get(..prefix.len()).is_some_and(|v| v.eq_ignore_ascii_case(prefix)) {
            let rest = &name[prefix.len()..];
            let (file, suffix) = if let Some(quoted) = rest.strip_prefix('"') {
                quoted.split_once('"').ok_or_else(|| invalid("unterminated source filename"))?
            } else if prefix == "ZARR:" {
                (rest, "")
            } else {
                let (file, _) = rest.rsplit_once(':').ok_or_else(|| invalid("subdataset has no variable"))?;
                (file, &rest[file.len()..])
            };
            if file.is_empty() || (!suffix.is_empty() && !suffix.starts_with(':')) {
                return Err(invalid("invalid source selector"));
            }
            return Ok(Some((prefix, file, suffix)));
        }
    }
    if let Some((prefix, _)) = name.split_once(':')
        && prefix.len() > 1
        && !name.contains("://")
        && prefix.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(unsupported("source inventory for this subdataset selector"));
    }
    Ok(None)
}

// GDALDataTypeUnionWithValue-style inference for nodata metadata.
fn union_value(dtype: DataType, value: f64) -> DataType {
    use DataType::*;
    if dtype == F32 && value as f32 as f64 == value {
        return F32;
    }
    if !value.is_finite() {
        return if matches!(dtype, F32 | F64) { dtype } else { F64 };
    }
    let native = if value.fract() != 0. {
        if value as f32 as f64 == value { F32 } else { F64 }
    } else if value >= 0. {
        if value <= u8::MAX as f64 {
            U8
        } else if value <= u16::MAX as f64 {
            U16
        } else if value <= u32::MAX as f64 {
            U32
        } else if value < 18446744073709551616. {
            U64
        } else {
            F64
        }
    } else if value >= i8::MIN as f64 {
        I8
    } else if value >= i16::MIN as f64 {
        I16
    } else if value >= i32::MIN as f64 {
        I32
    } else if value >= i64::MIN as f64 {
        I64
    } else {
        F64
    };
    super::common_type([dtype, native])
}

struct ParsedWarp {
    source: String,
    source_overview: Option<usize>,
    relative: bool,
    options: WarpOptions,
    pairs: Vec<(usize, usize)>,
    source_nodata: Vec<Option<f64>>,
    destination_nodata: Vec<Option<f64>>,
}

struct Reader {
    description: std::sync::Arc<VirtualRaster>,
    mask: Option<std::sync::Arc<VirtualRaster>>,
    base: String,
    sources: HashMap<String, LazyRaster>,
    cached: HashMap<String, LazyRaster>,
    lru: std::collections::VecDeque<String>,
}

impl Reader {
    fn source(&mut self, name: &str, relative: bool) -> Result<&mut LazyRaster> {
        // Uploaded source names override references exactly; persistent readers use
        // resolved paths so relativeToVRT=0/1 cannot alias different files.
        if self.sources.contains_key(name) {
            return Ok(self.sources.get_mut(name).expect("supplied source exists"));
        }
        let path = resolve_reference(&self.base, name, relative);
        if !self.cached.contains_key(&path) {
            let source = crate::open_raster_lazy(&path)?;
            if self.cached.len() >= 32
                && let Some(oldest) = self.lru.pop_front()
            {
                self.cached.remove(&oldest);
            }
            self.cached.insert(path.clone(), source);
        }
        if let Some(position) = self.lru.iter().position(|key| key == &path) {
            self.lru.remove(position);
        }
        self.lru.push_back(path.clone());
        Ok(self.cached.get_mut(&path).expect("source inserted"))
    }

    fn source_values(
        &mut self,
        src: &VrtSource,
        element: &xml::Element,
        window: Window,
        output_type: DataType,
    ) -> Result<Vec<Option<f64>>> {
        let source = self.source(&src.filename, src.relative_to_vrt)?;
        let (width, height) = (source.width(), source.height());
        let rect = if src.src_rect[0].is_nan() { [0., 0., width as f64, height as f64] } else { src.src_rect };
        if rect.iter().chain(src.dst_rect.iter()).any(|v| !v.is_finite())
            || rect[2] <= 0.
            || rect[3] <= 0.
            || src.dst_rect[2] <= 0.
            || src.dst_rect[3] <= 0.
        {
            return Err(invalid("invalid source/destination rectangle"));
        }
        let kernel = element.child("Kernel");
        let radius = kernel.map(kernel_size).transpose()?.unwrap_or(1) / 2;
        let pad = radius
            + match src.resampling {
                Resampling::Nearest => 1,
                Resampling::Bilinear => 2,
                _ => 4,
            };
        let mapx = |x: f64| rect[0] + (x - src.dst_rect[0]) * rect[2] / src.dst_rect[2];
        let mapy = |y: f64| rect[1] + (y - src.dst_rect[1]) * rect[3] / src.dst_rect[3];
        let x0 = (mapx(window.x as f64).floor() - pad as f64).max(0.).min(width as f64) as usize;
        let y0 = (mapy(window.y as f64).floor() - pad as f64).max(0.).min(height as f64) as usize;
        let x1 = (mapx((window.x + window.width) as f64).ceil() + pad as f64).max(0.).min(width as f64) as usize;
        let y1 = (mapy((window.y + window.height) as f64).ceil() + pad as f64).max(0.).min(height as f64) as usize;
        let mut out = vec![None; window.width * window.height];
        if x1 <= x0 || y1 <= y0 {
            return Ok(out);
        }
        let area = Window::new(x0, y0, x1 - x0, y1 - y0);
        let mask_band = element.child("SourceBand").is_some_and(|e| e.text.trim().starts_with("mask,"));
        let ds = source.read_window(area, &WindowOptions { bands: vec![src.band], overview: None })?;
        let mut data = if mask_band {
            let mask = source.read_mask(area, None)?.unwrap_or_else(|| {
                let values = ds.raster.data.to_f64_vec();
                values
                    .iter()
                    .map(|v| if ds.nodata().is_some_and(|nd| crate::ops::is_nodata(*v, nd)) { 0. } else { 255. })
                    .map(|v| v as u8)
                    .collect()
            });
            RasterData::F64(mask.into_iter().map(f64::from).collect())
        } else {
            ds.raster.data.clone()
        };
        if let Some(kernel) = kernel {
            data = RasterData::F64(filter(
                &data.to_f64_vec(),
                area.width,
                area.height,
                kernel,
                src.nodata.or(ds.nodata()),
                element.child("Function"),
            )?);
        }
        let values = match &data {
            RasterData::F64(v) => BandView::F64(v),
            _ => BandView::of(&ds.raster, 0),
        };
        let mut valid = src.nodata.filter(|_| src.complex).map(|nd| values.valid(nd));
        if element.child("UseMaskBand").is_some_and(|e| e.text.trim().eq_ignore_ascii_case("true") || e.text.trim() == "1")
            && let Some(mask) = source.read_mask(area, None)?
        {
            let v = valid.get_or_insert_with(|| vec![true; area.width * area.height]);
            for (valid, mask) in v.iter_mut().zip(mask) {
                *valid &= mask != 0;
            }
        }
        let req = super::SourceRead {
            values,
            valid: valid.as_deref(),
            width: area.width,
            height: area.height,
            src_type: if kernel.is_some() || mask_band { DataType::F64 } else { ds.raster.data_type },
            src_rect: [rect[0] - x0 as f64, rect[1] - y0 as f64, rect[2], rect[3]],
            dst_rect: [src.dst_rect[0] - window.x as f64, src.dst_rect[1] - window.y as f64, src.dst_rect[2], src.dst_rect[3]],
            resampling: src.resampling,
            working_float: src.complex,
            invalid_value: None,
        };
        let double = super::is_float_working(ds.raster.data_type)
            || matches!(output_type, DataType::I32 | DataType::U32 | DataType::I64 | DataType::U64 | DataType::F64);
        req.read(window.width, window.height, &mut out, |v| {
            let value = if let Some((offset, ratio)) = src.scale {
                let value = v * ratio + offset;
                if super::is_float_working(ds.raster.data_type) { value } else { value as f32 as f64 }
            } else {
                v
            };
            Some(if let Some(lut) = &src.lut {
                let value = super::lookup(lut, if double { value } else { value as f32 as f64 });
                if output_type == DataType::U8 {
                    f64::from((value as f32 + 0.5).clamp(0., 255.) as u8)
                } else if double {
                    value
                } else {
                    value as f32 as f64
                }
            } else {
                value
            })
        });
        Ok(out)
    }

    // GDAL evaluates warped VRT blocks, even for a subwindow. Keeping those block
    // bounds preserves the approximation endpoints and resampling scale.
    fn warped_blocks(&mut self, window: Window, bands: &[usize]) -> Result<RasterData> {
        let block = |name: &str, default: usize| {
            self.description.root.child(name).and_then(|e| e.text.trim().parse::<usize>().ok()).filter(|v| *v > 0).unwrap_or(default)
        };
        let (bw, bh) = (block("BlockXSize", 512), block("BlockYSize", 128));
        let (width, height) = (self.description.layout.width, self.description.layout.height);
        if !window.fits(width, height) {
            return Err(invalid("warped window is outside the raster"));
        }
        // GDAL evaluates the whole declared block for a small window too.
        // Bound its f64 sample buffers rather than allocating an untrusted area.
        let samples = bw.min(width).checked_mul(bh.min(height)).and_then(|n| n.checked_mul(self.description.layout.bands.len()));
        if samples.is_none_or(|n| n > 8 * 1024 * 1024) {
            return Err(unsupported("warped VRT blocks larger than 8 million samples"));
        }
        let parsed = self.parse_warp()?;
        let gt = self.description.layout.geo_transform.ok_or_else(|| invalid("missing warped geotransform"))?;
        let data_type = super::common_type(bands.iter().map(|&b| self.description.layout.bands[b - 1].data_type));
        // GDAL kernel CPLTestBool treats PARTIAL as true for output collision
        // avoidance, independently of PARTIAL's per-band source-validity masks.
        let unified_sink = self
            .description
            .root
            .child("GDALWarpOptions")
            .and_then(|w| w.children.iter().find(|o| o.name == "Option" && o.attr("name") == Some("UNIFIED_SRC_NODATA")))
            .is_some_and(|o| matches!(o.text.trim().to_ascii_uppercase().as_str(), "YES" | "PARTIAL"));
        let mut options = parsed.options;
        let declared = options.output_type;
        let mut inferred = super::common_type(self.description.layout.bands.iter().map(|b| b.data_type));
        let count = self.description.layout.bands.len();
        let source = self.source(&parsed.source, parsed.relative)?;
        inferred = super::common_type([inferred, source.header().raster.data_type]);
        for value in parsed.source_nodata.iter().chain(&parsed.destination_nodata).flatten() {
            inferred = union_value(inferred, *value);
        }
        let working = declared.unwrap_or(inferred);
        options.output_type = Some(working);
        let mut selection = vec![0; count];
        let mut nodata = vec![None; count];
        let mut destination_nodata = vec![None; count];
        for (index, (src, dst)) in parsed.pairs.iter().copied().enumerate() {
            if src == 0 || src > source.band_count() {
                return Err(invalid("invalid warped source band"));
            }
            selection[dst - 1] = src;
            nodata[dst - 1] = parsed.source_nodata[index];
            destination_nodata[dst - 1] = parsed.destination_nodata[index];
        }
        if selection.contains(&0) {
            return Err(invalid("unmapped warped band"));
        }
        options.src_nodata = Nodata::PerBand(nodata.into_iter().map(|v| v.map(|v| crate::ops::convert_value(v, working))).collect());
        // GDAL compares a working result with the raw double destination sentinel.
        // Only the fill value is converted to the working type by the warp plan.
        options.dst_nodata = Nodata::PerBand(destination_nodata);
        let mut output = vec![0.; window.width * window.height * bands.len()];
        for y in (window.y / bh * bh..window.y + window.height).step_by(bh) {
            for x in (window.x / bw * bw..window.x + window.width).step_by(bw) {
                let area = Window::new(x, y, bw.min(width - x), bh.min(height - y));
                options.dst_transform = Some([
                    gt[0] + x as f64 * gt[1] + y as f64 * gt[2],
                    gt[1],
                    gt[2],
                    gt[3] + x as f64 * gt[4] + y as f64 * gt[5],
                    gt[4],
                    gt[5],
                ]);
                options.size = Some((area.width, area.height));
                let result = crate::ops::warp::warp_lazy_bands_working(
                    source,
                    &selection,
                    &options,
                    Some(working),
                    unified_sink,
                    parsed.source_overview,
                )?;
                let values = crate::cog::cut(&result.raster, Window::full(area.width, area.height), bands).to_f64_vec();
                let (left, top, right, bottom) = (
                    x.max(window.x),
                    y.max(window.y),
                    (x + area.width).min(window.x + window.width),
                    (y + area.height).min(window.y + window.height),
                );
                for b in 0..bands.len() {
                    for row in top..bottom {
                        let src = b * area.width * area.height + (row - y) * area.width + left - x;
                        let dst = b * window.width * window.height + (row - window.y) * window.width + left - window.x;
                        output[dst..dst + right - left].copy_from_slice(&values[src..src + right - left]);
                    }
                }
            }
        }
        Ok(RasterData::from_f64_vec(data_type, output.into_iter().map(|v| crate::ops::warp_round(v, data_type)).collect()))
    }

    fn parse_warp(&self) -> Result<ParsedWarp> {
        let window = Window::full(self.description.layout.width, self.description.layout.height);
        let warp = self.description.root.child("GDALWarpOptions").ok_or_else(|| invalid("missing GDALWarpOptions"))?;
        let source = warp.child("SourceDataset").ok_or_else(|| invalid("missing SourceDataset"))?;
        let source_overview = warp
            .child("OpenOptions")
            .and_then(|options| options.children.iter().find(|option| option.name == "OOI" && option.attr("key") == Some("OVERVIEW_LEVEL")))
            .map(|option| {
                let words = option.text.split_whitespace().collect::<Vec<_>>();
                if words.len() > 2 || words.get(1).is_some_and(|suffix| !suffix.eq_ignore_ascii_case("only")) {
                    return Err(invalid("invalid source OVERVIEW_LEVEL"));
                }
                words
                    .first()
                    .ok_or_else(|| invalid("empty source OVERVIEW_LEVEL"))?
                    .parse::<usize>()
                    .map_err(|_| invalid("invalid source OVERVIEW_LEVEL"))
            })
            .transpose()?;
        let gt = self.description.layout.geo_transform.ok_or_else(|| invalid("warped VRT has no GeoTransform"))?;
        let srs = self.description.layout.srs.clone();
        let mut options = WarpOptions {
            dst_srs: srs,
            dst_transform: Some([
                gt[0] + window.x as f64 * gt[1] + window.y as f64 * gt[2],
                gt[1],
                gt[2],
                gt[3] + window.x as f64 * gt[4] + window.y as f64 * gt[5],
                gt[4],
                gt[5],
            ]),
            size: Some((window.width, window.height)),
            error_threshold: 0.,
            resampling: warp.child("ResampleAlg").map(|e| resampling(&e.text)).transpose()?.unwrap_or(Resampling::Nearest),
            output_type: warp
                .child("WorkingDataType")
                .filter(|e| !e.text.trim().eq_ignore_ascii_case("Unknown"))
                .map(|e| crate::ops::parse_data_type(e.text.trim()))
                .transpose()?,
            unified_src_nodata: None,
            src_nodata: Nodata::None,
            dst_nodata: Nodata::None,
            ..Default::default()
        };
        if let Some(v) = descendant(warp, "MaxError") {
            options.error_threshold = super::parse_number(&v.text)?;
        }
        if let Some(v) = descendant(warp, "SourceSRS").filter(|v| !v.text.trim().is_empty()) {
            options.src_srs = Some(crate::crs::SpatialRef::parse(v.text.trim())?);
        }
        if let Some(geo) = descendant(warp, "SrcGeoTransform") {
            let values = geo.text.split(',').map(super::parse_number).collect::<Result<Vec<_>>>()?;
            options.src_transform = Some(values.try_into().map_err(|_| invalid("SrcGeoTransform requires six numbers"))?);
        }
        if let Some(gcp) = descendant(warp, "GCPTransformer").or_else(|| descendant(warp, "TPSTransformer")) {
            if gcp.child("Reversed").is_some_and(|v| v.text.trim() != "0") {
                return Err(unsupported("reversed GCP transformer"));
            }
            let points = gcp.child("GCPList").ok_or_else(|| invalid("missing GCPList"))?;
            options.gcps = points
                .children
                .iter()
                .filter(|e| e.name == "GCP")
                .map(|g| {
                    let number = |name: &str| g.attr(name).ok_or_else(|| invalid("missing GCP coordinate")).and_then(super::parse_number);
                    Ok(crate::Gcp {
                        pixel: number("Pixel")?,
                        line: number("Line")?,
                        x: number("X")?,
                        y: number("Y")?,
                        z: g.attr("Z").map(super::parse_number).transpose()?.unwrap_or(0.),
                        ..Default::default()
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            options.gcp_method =
                if gcp.name == "TPSTransformer" { crate::ops::gcp::GcpMethod::Tps } else { crate::ops::gcp::GcpMethod::Polynomial };
            options.order = gcp
                .child("Order")
                .map(|e| e.text.trim().parse::<u32>().map_err(|_| invalid("invalid GCP order")))
                .transpose()?
                .unwrap_or(0);
            if options.src_srs.is_none() {
                options.src_srs = options.dst_srs.clone();
            }
        }
        if let Some(h) = descendant(warp, "Homography") {
            let values = h.text.split(',').map(super::parse_number).collect::<Result<Vec<_>>>()?;
            options.homography = Some(values.try_into().map_err(|_| invalid("homography requires nine coefficients"))?);
            if options.src_srs.is_none() {
                options.src_srs = options.dst_srs.clone();
            }
        }
        if let Some(rpc) = descendant(warp, "RPCTransformer") {
            let metadata = rpc.child("Metadata").ok_or_else(|| invalid("missing RPC metadata"))?;
            let items = metadata.children.iter().filter_map(|e| e.attr("key").map(|k| (k.to_string(), e.text.clone()))).collect::<Vec<_>>();
            options.rpc = Some(crate::ops::rpc::Rpc::from_metadata(&items)?);
            let mut rpc_options = Vec::new();
            for (xml, key) in [
                ("HeightOffset", "RPC_HEIGHT"),
                ("HeightScale", "RPC_HEIGHT_SCALE"),
                ("PixErrThreshold", "RPC_PIXEL_ERROR_THRESHOLD"),
                ("DEMPath", "RPC_DEM"),
                ("DEMInterpolation", "RPC_DEMINTERPOLATION"),
                ("DEMMissingValue", "RPC_DEM_MISSING_VALUE"),
                ("DEMApplyVDatumShift", "RPC_DEM_APPLY_VDATUM_SHIFT"),
                ("DEMSRS", "RPC_DEM_SRS"),
            ] {
                if let Some(v) = rpc.child(xml) {
                    rpc_options.push((key.to_string(), v.text.trim().to_string()));
                }
            }
            options.rpc_options = crate::ops::rpc::RpcOptions::from_options(&rpc_options)?;
        }
        if let Some(memory) = warp.child("WarpMemoryLimit") {
            options.memory_limit = super::parse_number(&memory.text)?;
        }
        for option in warp.children.iter().filter(|e| e.name == "Option") {
            match option.attr("name") {
                Some("UNIFIED_SRC_NODATA") => {
                    options.unified_src_nodata = match option.text.trim().to_ascii_uppercase().as_str() {
                        "YES" => Some(true),
                        "NO" => Some(false),
                        "PARTIAL" => None,
                        _ => return Err(invalid("unsupported UNIFIED_SRC_NODATA value")),
                    };
                }
                Some("SOURCE_EXTRA") => {
                    options.source_extra = Some(option.text.trim().parse().map_err(|_| invalid("invalid SOURCE_EXTRA"))?)
                }
                Some("SAMPLE_STEPS") => options.sample_steps_all = option.text.trim() == "ALL",
                Some("SKIP_NOSOURCE") => options.rpc_output_window = option.text.trim() == "NO",
                _ => {}
            }
        }
        let mapping = warp.child("BandList").ok_or_else(|| invalid("missing BandList"))?;
        let mut pairs = Vec::new();
        let mut source_nodata = Vec::new();
        let mut destination_nodata = Vec::new();
        for b in &mapping.children {
            let src = b.attr("src").and_then(|v| v.parse::<usize>().ok()).ok_or_else(|| invalid("invalid source band mapping"))?;
            let dst = b.attr("dst").and_then(|v| v.parse::<usize>().ok()).ok_or_else(|| invalid("invalid destination band mapping"))?;
            if dst == 0 || dst > self.description.layout.bands.len() {
                return Err(invalid("invalid destination band mapping"));
            }
            pairs.push((src, dst));
            destination_nodata.push(
                b.child("DstNoDataReal")
                    .map(|e| super::parse_number(&e.text))
                    .transpose()?
                    .or(self.description.layout.bands[dst - 1].nodata),
            );
            source_nodata.push(b.child("SrcNoDataReal").map(|e| super::parse_number(&e.text)).transpose()?);
        }
        if let Some(nd) = self.description.layout.bands[0].nodata {
            options.dst_nodata = Nodata::Values(vec![nd]);
        }
        Ok(ParsedWarp {
            source: source.text.trim().to_string(),
            source_overview,
            relative: source.attr("relativeToVRT") == Some("1"),
            options,
            pairs,
            source_nodata,
            destination_nodata,
        })
    }
}

impl WindowSource for Reader {
    fn band_data_type(&self, band: usize) -> Option<DataType> {
        band.checked_sub(1).and_then(|i| self.description.layout.bands.get(i)).map(|b| b.data_type)
    }

    fn input_paths(&self) -> Result<Vec<String>> {
        let _inventory = SourceInventoryScope::enter();
        let _depth = ReadDepth::enter()?;
        fn references<'a>(element: &'a xml::Element, out: &mut Vec<(&'a str, bool)>) {
            if matches!(element.name.as_str(), "SourceFilename" | "SourceDataset") {
                out.push((element.text.trim(), element.attr("relativeToVRT") == Some("1")));
            }
            for child in &element.children {
                references(child, out);
            }
        }
        let mut refs = Vec::new();
        references(&self.description.root, &mut refs);
        refs.sort_unstable();
        refs.dedup();
        let mut paths = Vec::new();
        let mut resolved = std::collections::HashSet::new();
        for (name, relative) in refs {
            if let Some(source) = self.sources.get(name) {
                // Supplied names can alias unrelated files: ask the actual reader.
                paths.extend(source.input_paths()?);
            } else {
                let path = resolve_reference(&self.base, name, relative);
                if resolved.insert(path.clone()) {
                    paths.extend(source_input_paths(&path)?);
                }
            }
        }
        paths.sort_unstable();
        paths.dedup();
        Ok(paths)
    }
    fn overviews(&self) -> Vec<(usize, usize)> {
        Vec::new()
    }
    fn block_size(&self) -> Option<(usize, usize)> {
        Some(if self.description.warped { (512, 128) } else { (128, 128) })
    }
    fn has_mask(&self) -> bool {
        self.description.root.child("MaskBand").is_some()
    }
    fn read_mask(&mut self, _level: usize, window: Window) -> Result<Option<Vec<u8>>> {
        let _depth = ReadDepth::enter()?;
        let Some(mask) = self.mask.clone() else {
            return Ok(None);
        };
        self.read_description(&mask, window, &[1])
            .map(|v| Some(v.to_f64_vec().into_iter().map(|v| if v == 0. { 0 } else { 255 }).collect()))
    }

    fn read(&mut self, _level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
        let _depth = ReadDepth::enter()?;
        if self.description.warped {
            return self.warped_blocks(window, bands);
        }
        let description = self.description.clone();
        self.read_description(&description, window, bands)
    }
}

impl Reader {
    fn read_description(&mut self, description: &VirtualRaster, window: Window, bands: &[usize]) -> Result<RasterData> {
        let mut values = Vec::with_capacity(window.width * window.height * bands.len());
        let elements = description.root.children.iter().filter(|e| e.name == "VRTRasterBand").collect::<Vec<_>>();
        for b in bands {
            let layout = description.layout.bands[*b - 1].clone();
            let element = &elements[*b - 1];
            let sources = element
                .children
                .iter()
                .filter(|e| matches!(e.name.as_str(), "SimpleSource" | "ComplexSource" | "AveragedSource" | "KernelFilteredSource"))
                .collect::<Vec<_>>();
            let mut inputs = Vec::new();
            let mut out = vec![layout.nodata.unwrap_or(0.); window.width * window.height];
            for (source, xml) in layout.sources.iter().zip(sources) {
                let input = self.source_values(source, xml, window, layout.data_type)?;
                if element.attr("subClass") == Some("VRTDerivedRasterBand") {
                    let transfer = element
                        .child("SourceTransferType")
                        .map(|e| crate::ops::parse_data_type(e.text.trim()))
                        .transpose()?
                        .unwrap_or(layout.data_type);
                    inputs.push(
                        input.into_iter().map(|v| crate::ops::convert_value(v.unwrap_or(layout.nodata.unwrap_or(0.)), transfer)).collect(),
                    );
                } else {
                    for (dst, value) in out.iter_mut().zip(input) {
                        if let Some(value) = value {
                            *dst = value;
                        }
                    }
                }
            }
            if element.attr("subClass") == Some("VRTDerivedRasterBand") {
                out = derived(element, &inputs, out.len())?;
            }
            values.extend(out.into_iter().map(|v| crate::ops::convert_value(v, layout.data_type)));
        }
        let data_type = super::common_type(bands.iter().map(|&b| description.layout.bands[b - 1].data_type));
        let values = values.into_iter().map(|v| crate::ops::convert_value(v, data_type)).collect();
        Ok(RasterData::from_f64_vec(data_type, values))
    }
}

thread_local! { static READ_DEPTH: std::cell::Cell<usize> = const {std::cell::Cell::new(0)}; }
struct ReadDepth;
impl ReadDepth {
    fn enter() -> Result<Self> {
        READ_DEPTH.with(|depth| {
            let current = depth.get();
            if current >= 64 {
                return Err(invalid("cyclic or excessively nested VRT sources"));
            }
            depth.set(current + 1);
            Ok(Self)
        })
    }
}
impl Drop for ReadDepth {
    fn drop(&mut self) {
        READ_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

pub(crate) fn resolve_reference(base: &str, name: &str, relative: bool) -> String {
    if let Ok(Some((prefix, file, suffix))) = subdataset_parts(name) {
        let file = resolve_reference(base, file, relative);
        return format!("{prefix}\"{file}\"{suffix}");
    }
    if relative { crate::stac::resolve_href(base, name) } else { name.to_string() }
}

fn invalid(s: &str) -> Error {
    Error::InvalidData(format!("VRT: {s}"))
}
fn unsupported(s: &str) -> Error {
    Error::Unsupported(format!("VRT: {s}"))
}
fn descendant<'a>(e: &'a xml::Element, name: &str) -> Option<&'a xml::Element> {
    if e.name == name {
        return Some(e);
    }
    e.children.iter().find_map(|e| descendant(e, name))
}
fn resampling(name: &str) -> Result<Resampling> {
    let normalized = name.trim().to_ascii_lowercase();
    Resampling::parse(match normalized.as_str() {
        "nearestneighbour" => "near",
        "minimum" => "min",
        "maximum" => "max",
        "median" => "med",
        "quartile1" => "q1",
        "quartile3" => "q3",
        "rootmeansquare" => "rms",
        v => v,
    })
}
fn validate_function(b: &xml::Element) -> Result<()> {
    if b.child("PixelFunctionLanguage").is_some_and(|e| !e.text.trim().eq_ignore_ascii_case("C")) {
        return Err(unsupported("external pixel-function language"));
    }
    let function = b.child("PixelFunctionType").ok_or_else(|| invalid("missing PixelFunctionType"))?.text.trim();
    if !matches!(
        function,
        "sum"
            | "diff"
            | "mul"
            | "div"
            | "mean"
            | "min"
            | "max"
            | "sqrt"
            | "abs"
            | "real"
            | "imag"
            | "conj"
            | "mod"
            | "inv"
            | "intensity"
            | "log10"
            | "dB"
            | "dB2amp"
            | "dB2pow"
            | "pow"
            | "exp"
    ) {
        return Err(unsupported(function));
    }
    let count = b
        .children
        .iter()
        .filter(|e| matches!(e.name.as_str(), "SimpleSource" | "ComplexSource" | "AveragedSource" | "KernelFilteredSource"))
        .count();
    if count == 0
        || (matches!(function, "diff" | "div") && count != 2)
        || (!matches!(function, "sum" | "diff" | "mul" | "div" | "mean" | "min" | "max") && count != 1)
    {
        return Err(invalid("invalid source count for pixel function"));
    }
    if let Some(transfer) = b.child("SourceTransferType") {
        crate::ops::parse_data_type(transfer.text.trim())?;
    }
    Ok(())
}
fn kernel_size(k: &xml::Element) -> Result<usize> {
    let size = k.child("Size").and_then(|e| e.text.trim().parse::<usize>().ok()).ok_or_else(|| invalid("invalid kernel size"))?;
    if size == 0 || size % 2 == 0 || size.checked_mul(size).is_none() {
        return Err(invalid("kernel size must be a positive odd number without dimension overflow"));
    }
    Ok(size)
}
fn validate_kernel(s: &xml::Element) -> Result<()> {
    let k = s.child("Kernel").ok_or_else(|| invalid("missing Kernel"))?;
    let size = kernel_size(k)?;
    let coefs = coefficients(k)?;
    if coefs.len() != size && coefs.len() != size * size {
        return Err(invalid("kernel coefficient count differs from size"));
    }
    if s.child("Function").is_some_and(|e| !matches!(e.text.trim(), "min" | "max" | "median" | "mode" | "stddev")) {
        return Err(unsupported("kernel function"));
    }
    Ok(())
}
fn coefficients(k: &xml::Element) -> Result<Vec<f64>> {
    let result = k
        .child("Coefs")
        .ok_or_else(|| invalid("missing kernel coefficients"))?
        .text
        .split_whitespace()
        .map(super::parse_number)
        .collect::<Result<Vec<_>>>()?;
    if result.iter().any(|v| !v.is_finite()) {
        return Err(invalid("nonfinite kernel coefficients"));
    }
    Ok(result)
}
// The arithmetic is shared with `gdal raster neighbors` ([`crate::ops::neighbors`]).
fn filter(
    input: &[f64],
    width: usize,
    height: usize,
    kernel: &xml::Element,
    nodata: Option<f64>,
    function: Option<&xml::Element>,
) -> Result<Vec<f64>> {
    use crate::ops::neighbors::{Kernel, NeighborMethod};
    let size = kernel_size(kernel)?;
    let coefs = coefficients(kernel)?;
    let normalized = kernel.attr("normalized").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let function = function.map(|f| NeighborMethod::parse(f.text.trim())).transpose()?.and_then(NeighborMethod::function);
    let k = Kernel { size, coefs, normalized, function };
    let input = input.iter().map(|v| *v as f32).collect::<Vec<_>>();
    let out = crate::ops::neighbors::filter(&input, width, height, &k, nodata.map(|v| v as f32));
    Ok(out.into_iter().map(f64::from).collect())
}
fn derived(element: &xml::Element, inputs: &[Vec<f64>], length: usize) -> Result<Vec<f64>> {
    let name = element.child("PixelFunctionType").ok_or_else(|| invalid("missing pixel function"))?.text.trim();
    let args = element.child("PixelFunctionArguments");
    let argument = |key: &str, default: f64| -> Result<f64> {
        args.and_then(|e| e.attr(key)).map(super::parse_number).transpose().map(|v| v.unwrap_or(default))
    };
    let k = argument("k", 0.)?;
    let power = if name == "pow" {
        args.and_then(|e| e.attr("power")).ok_or_else(|| invalid("pow requires power")).and_then(super::parse_number)?
    } else {
        1.
    };
    let fact = argument("fact", if name == "dB" { 20. } else { 1. })?;
    let base = argument("base", std::f64::consts::E)?;
    let factor = argument("k", 1.)?;
    let nodata = element.child("NoDataValue").map(|e| super::parse_number(&e.text)).transpose()?;
    let propagate = args.and_then(|e| e.attr("propagateNoData")).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    if inputs.is_empty() {
        return Err(invalid("pixel function has no sources"));
    }
    if matches!(name, "diff" | "div") && inputs.len() != 2 {
        return Err(invalid("pixel function requires two sources"));
    }
    let mut output = Vec::with_capacity(length);
    let mut values = Vec::with_capacity(inputs.len() + 1);
    for i in 0..length {
        values.clear();
        values.extend(inputs.iter().map(|v| v[i]));
        let is_nodata = |v: f64| nodata.is_some_and(|nd| crate::ops::is_nodata(v, nd));
        if (matches!(name, "sum" | "mul" | "mean" | "min" | "max") && propagate || matches!(name, "diff" | "div"))
            && values.iter().any(|v| is_nodata(*v))
        {
            output.push(nodata.unwrap_or(f64::NAN));
            continue;
        }
        let first = values[0];
        if matches!(name, "sum" | "mul" | "mean" | "min" | "max") {
            values.retain(|v| !is_nodata(*v));
        } else if is_nodata(first) && !matches!(name, "real" | "imag" | "conj" | "mod" | "intensity" | "dB2amp" | "dB2pow") {
            output.push(first);
            continue;
        }
        if matches!(name, "min" | "max") && args.and_then(|e| e.attr("k")).is_some() && !k.is_nan() {
            values.push(k);
        }
        if values.is_empty() {
            output.push(match name {
                "sum" => k,
                "mul" => factor,
                _ => nodata.unwrap_or(0.),
            });
            continue;
        }
        let result = match name {
            "sum" => values.iter().sum::<f64>() + k,
            "diff" => first - values[1],
            "mul" => values.iter().product::<f64>() * factor,
            "div" => {
                if values[1] == 0. {
                    f64::INFINITY
                } else {
                    first / values[1]
                }
            }
            "mean" => values.iter().sum::<f64>() / values.len() as f64,
            "min" => values.iter().copied().fold(f64::INFINITY, f64::min),
            "max" => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            "sqrt" => first.sqrt(),
            "mod" => first.abs(),
            "real" | "conj" => first,
            "imag" => 0.,
            "inv" => {
                if first == 0. {
                    f64::INFINITY
                } else {
                    factor / first
                }
            }
            "intensity" => first * first,
            "log10" => first.abs().log10(),
            "dB" => fact * first.abs().log10(),
            "dB2amp" => 10f64.powf(first / 20.),
            "dB2pow" => 10f64.powf(first / 10.),
            "pow" => first.powf(power),
            "exp" => base.powf(first * fact),
            _ => return Err(unsupported(name)),
        };
        output.push(result);
    }
    Ok(output)
}

fn serialize_gcps(options: &WarpOptions) -> Result<String> {
    use crate::ops::gcp::{GcpMethod, GcpTransformer};
    let transformer = if let Some(h) = options.homography {
        GcpTransformer::from_homography(h)?
    } else {
        GcpTransformer::new(&options.gcps, options.gcp_method, options.order)?
    };
    if let GcpTransformer::Homography(h) = transformer {
        return Ok(format!(
            "<SrcHomographyTransformer><HomographyTransformer><Homography>{}</Homography></HomographyTransformer></SrcHomographyTransformer>",
            h.0.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")
        ));
    }
    let points = options
        .gcps
        .iter()
        .map(|g| format!("<GCP Pixel=\"{}\" Line=\"{}\" X=\"{}\" Y=\"{}\" Z=\"{}\"/>", g.pixel, g.line, g.x, g.y, g.z))
        .collect::<String>();
    if options.gcp_method == GcpMethod::Tps {
        return Ok(format!(
            "<SrcTPSTransformer><TPSTransformer><Reversed>0</Reversed><GCPList>{points}</GCPList></TPSTransformer></SrcTPSTransformer>"
        ));
    }
    let order = if options.order == 0 { if options.gcps.len() >= 6 { 2 } else { 1 } } else { options.order };
    Ok(format!(
        "<SrcGCPTransformer><GCPTransformer><Order>{order}</Order><Reversed>0</Reversed><GCPList>{points}</GCPList></GCPTransformer></SrcGCPTransformer>"
    ))
}

fn serialize_rpc(rpc: &crate::ops::rpc::Rpc, options: &crate::ops::rpc::RpcOptions) -> Result<String> {
    let scalars = [
        ("ERR_BIAS", rpc.err_bias),
        ("ERR_RAND", rpc.err_rand),
        ("LINE_OFF", rpc.line_off),
        ("SAMP_OFF", rpc.samp_off),
        ("LAT_OFF", rpc.lat_off),
        ("LONG_OFF", rpc.long_off),
        ("HEIGHT_OFF", rpc.height_off),
        ("LINE_SCALE", rpc.line_scale),
        ("SAMP_SCALE", rpc.samp_scale),
        ("LAT_SCALE", rpc.lat_scale),
        ("LONG_SCALE", rpc.long_scale),
        ("HEIGHT_SCALE", rpc.height_scale),
        ("MIN_LONG", rpc.min_long),
        ("MIN_LAT", rpc.min_lat),
        ("MAX_LONG", rpc.max_long),
        ("MAX_LAT", rpc.max_lat),
    ];
    let mut metadata = scalars.iter().map(|(key, value)| format!("<MDI key=\"{key}\">{value}</MDI>")).collect::<String>();
    for (key, values) in [
        ("LINE_NUM_COEFF", rpc.line_num_coeff),
        ("LINE_DEN_COEFF", rpc.line_den_coeff),
        ("SAMP_NUM_COEFF", rpc.samp_num_coeff),
        ("SAMP_DEN_COEFF", rpc.samp_den_coeff),
    ] {
        metadata += &format!("<MDI key=\"{key}\">{}</MDI>", values.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" "));
    }
    let mut extra = String::new();
    if let Some(dem) = &options.dem {
        let path = if crate::vfs::Location::parse(dem).is_local() {
            std::fs::canonicalize(dem)?.to_string_lossy().into_owned()
        } else {
            dem.clone()
        };
        extra += &format!(
            "<DEMPath>{}</DEMPath><DEMApplyVDatumShift>{}</DEMApplyVDatumShift><DEMInterpolation>{}</DEMInterpolation>",
            xml::escape(&path),
            options.dem_apply_vdatum_shift,
            match options.dem_interpolation {
                crate::ops::rpc::DemInterpolation::Near => "near",
                crate::ops::rpc::DemInterpolation::Bilinear => "bilinear",
                crate::ops::rpc::DemInterpolation::Cubic => "cubic",
            }
        );
    }
    if let Some(v) = options.dem_missing_value {
        extra += &format!("<DEMMissingValue>{v}</DEMMissingValue>");
    }
    if let Some(v) = &options.dem_srs {
        extra += &format!("<DEMSRS>{}</DEMSRS>", xml::escape(v));
    }
    Ok(format!(
        "<SrcRPCTransformer><RPCTransformer><Reversed>0</Reversed><HeightOffset>{}</HeightOffset><HeightScale>{}</HeightScale><PixErrThreshold>{}</PixErrThreshold>{extra}<Metadata>{metadata}</Metadata></RPCTransformer></SrcRPCTransformer>",
        options.height.or(rpc.height_default).unwrap_or(0.),
        options.height_scale,
        options.pixel_error_threshold.unwrap_or(0.1)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RasterDataset;

    fn raster() -> RasterDataset {
        crate::ops::build_raster(
            Grid { width: 5, height: 5, geo_transform: [0., 1., 0., 5., 0., -1.] },
            vec![(0..25).map(f64::from).collect()],
            DataType::F64,
            None,
            &crate::crs::SpatialRef::default(),
            crate::ops::convert_value,
        )
        .unwrap()
    }
    fn document(band: &str) -> String {
        format!("<VRTDataset rasterXSize=\"5\" rasterYSize=\"5\"><GeoTransform>0,1,0,5,0,-1</GeoTransform>{band}</VRTDataset>")
    }
    fn sources() -> HashMap<String, LazyRaster> {
        HashMap::from([("source.tif".into(), LazyRaster::from(raster()))])
    }
    fn source(tag: &str, extra: &str) -> String {
        format!("<{tag}><SourceFilename>source.tif</SourceFilename><SourceBand>1</SourceBand>{extra}</{tag}>")
    }

    #[test]
    fn oversized_declared_blocks_fail_before_opening_sources() {
        let xml = r#"<VRTDataset rasterXSize="1000000" rasterYSize="1000000" subClass="VRTWarpedDataset"><GeoTransform>0,1,0,1,0,-1</GeoTransform><VRTRasterBand dataType="Float64"/><BlockXSize>1000000</BlockXSize><BlockYSize>1000000</BlockYSize><GDALWarpOptions><SourceDataset>must-not-be-opened.tif</SourceDataset><BandList><BandMapping src="1" dst="1"/></BandList></GDALWarpOptions></VRTDataset>"#;
        let mut raster = VirtualRaster::from_xml(xml).unwrap().into_lazy(Path::new(""), HashMap::new()).unwrap();
        let error = raster.read_window(Window::new(0, 0, 1, 1), &Default::default()).unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)));
        assert!(error.to_string().contains("8 million samples"));
    }

    #[test]
    fn source_cache_uses_resolved_references_and_lru() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("dataset.vrt").to_string_lossy().into_owned();
        let document =
            VirtualRaster::from_xml("<VRTDataset rasterXSize=\"1\" rasterYSize=\"1\"><VRTRasterBand dataType=\"Byte\"/></VRTDataset>")
                .unwrap();
        let mut reader = Reader {
            description: std::sync::Arc::new(document),
            mask: None,
            base: base.clone(),
            sources: HashMap::new(),
            cached: HashMap::new(),
            lru: Default::default(),
        };
        for index in 0..33 {
            std::fs::write(dir.path().join(format!("{index}.asc")), "ncols 1\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n7\n").unwrap();
        }
        for index in 0..32 {
            reader.source(&format!("{index}.asc"), true).unwrap();
        }
        reader.source("0.asc", true).unwrap();
        reader.source("32.asc", true).unwrap();
        assert_eq!(reader.cached.len(), 32);
        assert!(reader.cached.contains_key(&resolve_reference(&base, "0.asc", true)));
        assert!(!reader.cached.contains_key(&resolve_reference(&base, "1.asc", true)));
        // Absolute and relative aliases of the same reference share their reader.
        reader.source(&dir.path().join("0.asc").to_string_lossy(), false).unwrap();
        assert_eq!(reader.cached.len(), 32);
        assert_eq!(resolve_reference(&base, "0.asc", false), "0.asc");
        assert_ne!(resolve_reference(&base, "0.asc", true), "0.asc");
    }

    #[test]
    fn common_band_types_follow_gdal_union() {
        use DataType::*;
        assert_eq!(super::union_value(F32, f32::MIN as f64), F32);
        assert_eq!(super::union_value(F32, 16_777_217.), F64);
        for (a, b, expected) in [(U8, F32, F32), (U8, F64, F64), (U16, I16, I32), (U32, I32, I64), (U32, F32, F64), (U64, I64, F64)] {
            assert_eq!(super::super::common_type([a, b]), expected);
            assert_eq!(super::super::common_type([b, a]), expected);
        }
    }

    #[test]
    fn mixed_types_promote_without_losing_native_band_rounding() {
        let grid = Grid { width: 2, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] };
        let source = crate::ops::build_raster(
            grid,
            vec![vec![1.250000000001, 2.75]],
            DataType::F64,
            None,
            &crate::crs::SpatialRef::default(),
            crate::ops::convert_value,
        )
        .unwrap();
        let band = |dtype: &str, derived: bool| {
            format!(
                "<VRTRasterBand dataType=\"{dtype}\"{}>{}{}{}</VRTRasterBand>",
                if derived { " subClass=\"VRTDerivedRasterBand\"" } else { "" },
                if derived { "<PixelFunctionType>sum</PixelFunctionType><SourceTransferType>Float64</SourceTransferType>" } else { "" },
                self::source("SimpleSource", ""),
                if derived { self::source("SimpleSource", "") } else { String::new() }
            )
        };
        let text = format!(
            "<VRTDataset rasterXSize=\"2\" rasterYSize=\"1\">{}{}{}{}</VRTDataset>",
            band("Byte", false),
            band("Float32", false),
            band("Float64", false),
            band("Byte", true)
        );
        let dir = tempfile::tempdir().unwrap();
        crate::write_raster_dataset(&source, dir.path().join("source.tif").to_str().unwrap(), &Default::default()).unwrap();
        let plain =
            format!("<VRTDataset rasterXSize=\"2\" rasterYSize=\"1\">{}{}</VRTDataset>", band("Byte", false), band("Float64", false));
        let materialized = VrtDataset::from_xml(&plain).unwrap().materialize(dir.path()).unwrap();
        assert_eq!(materialized.raster.data_type, DataType::F64);
        assert_eq!(materialized.raster.data.to_f64_vec(), vec![1., 3., 1.250000000001, 2.75]);
        let document = VirtualRaster::from_xml(&text).unwrap();
        let mut lazy = document.into_lazy(Path::new(""), HashMap::from([("source.tif".into(), LazyRaster::from(source))])).unwrap();
        assert_eq!(lazy.header().raster.data_type, DataType::F64);
        let out = lazy.read().unwrap();
        assert_eq!(out.raster.data.to_f64_vec(), vec![1., 3., 1.25, 2.75, 1.250000000001, 2.75, 3., 6.]);
        for (band, dtype, expected) in
            [(1, DataType::U8, vec![1., 3.]), (2, DataType::F32, vec![1.25, 2.75]), (3, DataType::F64, vec![1.250000000001, 2.75])]
        {
            let out = lazy.read_window(Window::full(2, 1), &WindowOptions { bands: vec![band], ..Default::default() }).unwrap();
            assert_eq!(out.raster.data_type, dtype);
            assert_eq!(out.raster.data.to_f64_vec(), expected);
        }
    }

    #[test]
    fn warped_working_type_precedes_resampling_and_selected_buffer_conversion() {
        let source = || {
            crate::ops::build_raster(
                Grid { width: 3, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
                vec![vec![0., 0.25, -9999.], vec![1.250000000001, 2.75, 9.]],
                DataType::F64,
                None,
                &Default::default(),
                crate::ops::convert_value,
            )
            .unwrap()
        };
        for (working, value) in [
            ("<WorkingDataType>Float32</WorkingDataType>", 1.25),
            ("<WorkingDataType>Float64</WorkingDataType>", 1.250000000001),
            ("", 1.250000000001),
            ("<WorkingDataType>Unknown</WorkingDataType>", 1.250000000001),
        ] {
            let xml = format!(
                "<VRTDataset rasterXSize=\"3\" rasterYSize=\"1\" subClass=\"VRTWarpedDataset\"><GeoTransform>0,1,0,1,0,-1</GeoTransform><VRTRasterBand dataType=\"Byte\"><NoDataValue>0</NoDataValue></VRTRasterBand><VRTRasterBand dataType=\"Float64\"><NoDataValue>0</NoDataValue></VRTRasterBand><GDALWarpOptions>{working}<Option name=\"UNIFIED_SRC_NODATA\">PARTIAL</Option><SourceDataset>source.tif</SourceDataset><BandList><BandMapping src=\"1\" dst=\"1\"><SrcNoDataReal>-9999</SrcNoDataReal><DstNoDataReal>0</DstNoDataReal></BandMapping><BandMapping src=\"2\" dst=\"2\"><SrcNoDataReal>-9999</SrcNoDataReal><DstNoDataReal>0</DstNoDataReal></BandMapping></BandList></GDALWarpOptions></VRTDataset>"
            );
            let mut lazy = VirtualRaster::from_xml(&xml)
                .unwrap()
                .into_lazy(Path::new(""), HashMap::from([("source.tif".into(), LazyRaster::from(source()))]))
                .unwrap();
            let all = lazy.read().unwrap();
            assert_eq!(all.raster.data_type, DataType::F64);
            assert_eq!(all.raster.data.to_f64_vec(), vec![0., 0.25, 0., value, 2.75, 9.], "{working}");
            let byte = lazy.read_window(Window::full(3, 1), &WindowOptions { bands: vec![1], ..Default::default() }).unwrap();
            assert_eq!(byte.raster.data_type, DataType::U8);
            assert_eq!(byte.raster.data.to_f64_vec(), vec![0., 0., 0.]);
            let second = lazy.read_window(Window::full(3, 1), &WindowOptions { bands: vec![2], ..Default::default() }).unwrap();
            assert_eq!(second.raster.data.to_f64_vec(), vec![value, 2.75, 9.]);
            let invalid_xml = xml.replace("</GDALWarpOptions>", "<WorkingDataType>nonsense</WorkingDataType></GDALWarpOptions>");
            // Omitted working types infer from the source, band types and nodata values.
            if working.is_empty() {
                let mut bad = VirtualRaster::from_xml(&invalid_xml)
                    .unwrap()
                    .into_lazy(Path::new(""), HashMap::from([("source.tif".into(), LazyRaster::from(source()))]))
                    .unwrap();
                assert!(bad.read().unwrap_err().to_string().contains("data type"));
            }
        }
        assert_eq!(union_value(DataType::U8, -9999.), DataType::I16);
        assert_eq!(union_value(DataType::F32, 0.1), DataType::F64);
    }

    #[test]
    fn float_working_mask_casts_decimal_nodata_but_retains_metadata() {
        let raw = 1.23456789;
        for (source_nodata, destination) in [(raw, -99.), (raw, raw), (-9999., raw)] {
            let source = crate::ops::build_raster(
                Grid { width: 3, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
                vec![vec![raw, 2.5, 3.5]],
                DataType::F32,
                None,
                &Default::default(),
                crate::ops::convert_value,
            )
            .unwrap();
            let xml = format!(
                "<VRTDataset rasterXSize=\"3\" rasterYSize=\"1\" subClass=\"VRTWarpedDataset\"><GeoTransform>0,1,0,1,0,-1</GeoTransform><VRTRasterBand dataType=\"Float32\"><NoDataValue>{destination}</NoDataValue></VRTRasterBand><GDALWarpOptions><WorkingDataType>Float32</WorkingDataType><SourceDataset>source.tif</SourceDataset><BandList><BandMapping src=\"1\" dst=\"1\"><SrcNoDataReal>{source_nodata}</SrcNoDataReal><DstNoDataReal>{destination}</DstNoDataReal></BandMapping></BandList></GDALWarpOptions></VRTDataset>"
            );
            let mut lazy = VirtualRaster::from_xml(&xml)
                .unwrap()
                .into_lazy(Path::new(""), HashMap::from([("source.tif".into(), LazyRaster::from(source))]))
                .unwrap();
            assert_eq!(lazy.header().nodata(), Some(destination));
            let out = lazy.read().unwrap();
            assert_eq!(out.nodata(), Some(destination));
            assert_eq!(out.raster.data.to_f64_vec(), vec![(if source_nodata == raw { destination } else { raw }) as f32 as f64, 2.5, 3.5]);
        }
    }

    #[test]
    fn serialized_multiband_nodata_modes() {
        let source = || {
            crate::ops::build_raster(
                Grid { width: 1, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
                vec![vec![-9999.], vec![2.]],
                DataType::F64,
                Some(-9999.),
                &Default::default(),
                crate::ops::convert_value,
            )
            .unwrap()
        };
        for (label, src_nodata, unified, expected) in [
            ("partial", "<SrcNoDataReal>-9999</SrcNoDataReal>", "", vec![0., 2.]),
            ("ignore", "", "", vec![-9999., 2.]),
            ("unified", "<SrcNoDataReal>-9999</SrcNoDataReal>", "<Option name=\"UNIFIED_SRC_NODATA\">YES</Option>", vec![-9999., 2.]),
        ] {
            let xml = format!(
                "<VRTDataset rasterXSize=\"1\" rasterYSize=\"1\" subClass=\"VRTWarpedDataset\"><GeoTransform>0,1,0,1,0,-1</GeoTransform><VRTRasterBand dataType=\"Float64\"><NoDataValue>0</NoDataValue></VRTRasterBand><VRTRasterBand dataType=\"Float64\"><NoDataValue>0</NoDataValue></VRTRasterBand><GDALWarpOptions>{unified}<SourceDataset>source.tif</SourceDataset><BandList><BandMapping src=\"1\" dst=\"1\">{src_nodata}</BandMapping><BandMapping src=\"2\" dst=\"2\">{src_nodata}</BandMapping></BandList></GDALWarpOptions></VRTDataset>"
            );
            let mut lazy = VirtualRaster::from_xml(&xml)
                .unwrap()
                .into_lazy(Path::new(""), HashMap::from([("source.tif".into(), LazyRaster::from(source()))]))
                .unwrap();
            assert_eq!(lazy.read().unwrap().raster.data.to_f64_vec(), expected, "{label}");
        }
    }

    #[test]
    fn per_band_nodata_survives_tiff_and_window_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bands.tif");
        let mut ds = crate::ops::build_raster(
            Grid { width: 2, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
            vec![vec![1., 2.], vec![-202., 3.]],
            DataType::F64,
            None,
            &Default::default(),
            crate::ops::convert_value,
        )
        .unwrap();
        ds.band_nodata = vec![None, Some(-202.)];
        let stats = crate::info::band_statistics(&ds, 1).unwrap();
        assert_eq!((stats.min, stats.max, stats.valid_count), (3., 3., 1));
        let translated =
            crate::ops::translate::translate(&ds, &crate::ops::translate::TranslateOptions { bands: vec![2, 1, 2], ..Default::default() })
                .unwrap();
        assert_eq!(translated.band_nodata, vec![Some(-202.), None, Some(-202.)]);
        assert_eq!(translated.raster.data.to_f64_vec(), vec![-202., 3., 1., 2., -202., 3.]);
        let cleared = crate::ops::translate::translate(
            &ds,
            &crate::ops::translate::TranslateOptions { assign_nodata: Some(None), ..Default::default() },
        )
        .unwrap();
        assert_eq!(cleared.band_nodata, vec![None, None]);
        crate::write_raster_dataset(&ds, path.to_str().unwrap(), &Default::default()).unwrap();
        let mut lazy = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        assert_eq!(lazy.band_nodata(1), None);
        assert_eq!(lazy.band_nodata(2), Some(-202.));
        let selected = lazy.read_window(Window::new(0, 0, 2, 1), &WindowOptions { bands: vec![2, 1, 2], overview: None }).unwrap();
        assert_eq!(selected.band_nodata, vec![Some(-202.), None, Some(-202.)]);
        assert_eq!(selected.nodata(), Some(-202.));
        let all = lazy.read().unwrap();
        assert_eq!(all.band_nodata, ds.band_nodata);
        ds.band_nodata.clear();
        crate::write_raster_dataset(&ds, path.to_str().unwrap(), &Default::default()).unwrap();
        let lazy = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        assert_eq!(lazy.band_nodata(2), None);
    }

    #[test]
    fn generic_pam_does_not_replace_native_nodata() {
        let mut ds = raster();
        ds.has_nodata = true;
        ds.raster.nodata = 1.70141e38_f32 as f64;
        let pam = crate::drivers::common::parse_pam(
            "<PAMDataset><PAMRasterBand band=\"1\"><NoDataValue>-32768</NoDataValue></PAMRasterBand></PAMDataset>",
        );
        crate::drivers::nodata::apply(pam, &mut ds);
        assert_eq!(ds.nodata(), Some(1.70141e38_f32 as f64));
        assert!(ds.band_nodata.is_empty());
    }

    #[test]
    fn sidecar_updates_preserve_metadata_in_memory() {
        let files = crate::memfs::Mount::new([("bands.tif.aux.xml".into(), b"<PAMDataset><SRS>EPSG:4326</SRS><Metadata><MDI key=\"STATISTICS_MEAN\">12.5</MDI></Metadata><PAMRasterBand band=\"1\"><Description>first</Description><NoDataValue>-1</NoDataValue></PAMRasterBand></PAMDataset>".to_vec())]);
        let path = files.path().join("bands.tif");
        crate::drivers::common::write_band_nodata_pam(&path, &[Some(-101.), None]).unwrap();
        let xml = String::from_utf8(crate::memfs::read(&files.path().join("bands.tif.aux.xml")).unwrap()).unwrap();
        assert!(xml.contains("<SRS>EPSG:4326</SRS>"));
        assert!(xml.contains("STATISTICS_MEAN"));
        assert!(xml.contains("<Description>first</Description>"));
        let pam = crate::drivers::common::read_pam(&path).unwrap();
        assert_eq!(pam.band_nodata, vec![(1, -101.)]);
    }

    fn pyramid(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("levels.tif");
        let source = crate::ops::build_raster(
            Grid { width: 32, height: 32, geo_transform: [0., 1., 0., 32., 0., -1.] },
            vec![(0..1024).map(|i| ((i / 32) * (i / 32) + i % 32) as f64).collect()],
            DataType::F32,
            None,
            &crate::crs::SpatialRef::from_epsg(3857).unwrap(),
            crate::ops::convert_value,
        )
        .unwrap();
        crate::write_raster_dataset(
            &source,
            path.to_str().unwrap(),
            &crate::RasterWriteOptions { overviews: vec![2, 4, 8], overview_resampling: Some("average".into()), ..Default::default() },
        )
        .unwrap();
        path
    }

    #[test]
    fn serialized_overview_is_fixed_and_read_with_its_scaled_transform() {
        use crate::ops::overview::OverviewSelection;
        let directory = tempfile::tempdir().unwrap();
        let path = pyramid(directory.path());
        for (overview, expected_level) in [
            (OverviewSelection::Auto, Some(1)),
            (OverviewSelection::None, None),
            (OverviewSelection::Level(0), Some(0)),
            (OverviewSelection::Level(1), Some(1)),
        ] {
            let options = WarpOptions { size: Some((8, 8)), overview, error_threshold: 0., ..Default::default() };
            let document = VirtualRaster::warped(path.to_str().unwrap(), &options).unwrap();
            let reader = Reader {
                description: std::sync::Arc::new(document.clone()),
                mask: None,
                base: String::new(),
                sources: HashMap::new(),
                cached: HashMap::new(),
                lru: Default::default(),
            };
            let parsed = reader.parse_warp().unwrap();
            assert_eq!(parsed.source_overview, expected_level);
            assert_eq!(parsed.options.output_type, None); // Unknown infers from both source and target.
            let scale = expected_level.map(|level| (2 << level) as f64).unwrap_or(1.);
            assert_eq!(parsed.options.src_transform.unwrap(), [0., scale, 0., 32., 0., -scale]);
            let mut original = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
            let expected = crate::ops::warp::warp_lazy(&mut original, &options).unwrap();
            let mut lazy = document.clone().into_lazy(directory.path(), HashMap::new()).unwrap();
            assert_eq!(lazy.read().unwrap().raster.data.to_f64_vec(), expected.raster.data.to_f64_vec());
            let window = Window::new(2, 1, 3, 2);
            assert_eq!(
                lazy.read_window(window, &Default::default()).unwrap().raster.data.to_f64_vec(),
                crate::cog::cut(&expected.raster, window, &[1]).to_f64_vec()
            );
            if expected_level == Some(1) {
                let token = if matches!(overview, OverviewSelection::Level(_)) { "1 only</OOI>" } else { "1</OOI>" };
                let plain = document.to_xml().replace(token, "1</OOI>");
                let mut lazy = VirtualRaster::from_xml(&plain).unwrap().into_lazy(directory.path(), HashMap::new()).unwrap();
                assert_eq!(lazy.read().unwrap().raster.data.to_f64_vec(), expected.raster.data.to_f64_vec());
                for invalid_level in ["", "-1", "one", "1 extra", "1 only extra"] {
                    let text = document.to_xml().replace(token, &format!("{invalid_level}</OOI>"));
                    let reader = Reader {
                        description: std::sync::Arc::new(VirtualRaster::from_xml(&text).unwrap()),
                        mask: None,
                        base: String::new(),
                        sources: HashMap::new(),
                        cached: HashMap::new(),
                        lru: Default::default(),
                    };
                    assert!(reader.parse_warp().is_err());
                }
            }
        }
    }

    #[test]
    fn serialized_overview_scales_gcps_rpc_and_homography() {
        let directory = tempfile::tempdir().unwrap();
        let path = pyramid(directory.path());
        let points = [(0., 0.), (32., 0.), (0., 32.), (32., 32.)]
            .map(|(pixel, line)| crate::Gcp { pixel, line, x: pixel, y: 32. - line, ..Default::default() })
            .to_vec();
        let base = WarpOptions { size: Some((8, 8)), overview: crate::ops::overview::OverviewSelection::Level(1), ..Default::default() };
        for options in [
            WarpOptions { gcps: points.clone(), gcp_method: crate::ops::gcp::GcpMethod::Polynomial, ..base.clone() },
            WarpOptions { gcps: points, gcp_method: crate::ops::gcp::GcpMethod::Tps, ..base.clone() },
            WarpOptions { homography: Some([1., 0., 0., 0., -1., 32., 0., 0., 1.]), ..base.clone() },
        ] {
            let document = VirtualRaster::warped(path.to_str().unwrap(), &options).unwrap();
            let reader = Reader {
                description: std::sync::Arc::new(document),
                mask: None,
                base: String::new(),
                sources: HashMap::new(),
                cached: HashMap::new(),
                lru: Default::default(),
            };
            let parsed = reader.parse_warp().unwrap();
            assert_eq!(parsed.source_overview, Some(1));
            if let Some(matrix) = options.homography {
                let mut expected = matrix;
                for row in 0..3 {
                    expected[row * 3] *= 4.;
                    expected[row * 3 + 1] *= 4.;
                }
                assert_eq!(parsed.options.homography, Some(expected));
            } else {
                assert_eq!(parsed.options.gcps[1].pixel, 8.);
                assert_eq!(parsed.options.gcps[2].line, 8.);
            }
        }
        let coefficients = |position| (0..20).map(|i| if i == position { "1" } else { "0" }).collect::<Vec<_>>().join(" ");
        let mut items = [
            ("LINE_OFF", "15.5"),
            ("SAMP_OFF", "15.5"),
            ("LAT_OFF", "45"),
            ("LONG_OFF", "10"),
            ("HEIGHT_OFF", "0"),
            ("LINE_SCALE", "16"),
            ("SAMP_SCALE", "16"),
            ("LAT_SCALE", "0.016"),
            ("LONG_SCALE", "0.016"),
            ("HEIGHT_SCALE", "1"),
        ]
        .map(|(key, value)| (key.into(), value.into()))
        .to_vec();
        for (key, position) in [("LINE_NUM_COEFF", 2), ("SAMP_NUM_COEFF", 1), ("LINE_DEN_COEFF", 0), ("SAMP_DEN_COEFF", 0)] {
            items.push((key.into(), coefficients(position)));
        }
        let rpc = crate::ops::rpc::Rpc::from_metadata(&items).unwrap();
        let options = WarpOptions {
            rpc: Some(rpc.clone()),
            src_srs: Some(crate::crs::SpatialRef::from_epsg(4326).unwrap()),
            dst_srs: Some(crate::crs::SpatialRef::from_epsg(4326).unwrap()),
            extent: Some((10., 44.984, 10.032, 45.016)),
            ..base
        };
        let document = VirtualRaster::warped(path.to_str().unwrap(), &options).unwrap();
        let reader = Reader {
            description: std::sync::Arc::new(document),
            mask: None,
            base: String::new(),
            sources: HashMap::new(),
            cached: HashMap::new(),
            lru: Default::default(),
        };
        let scaled = reader.parse_warp().unwrap().options.rpc.unwrap();
        assert_eq!(scaled.line_off, rpc.line_off / 4.);
        assert_eq!(scaled.samp_off, rpc.samp_off / 4.);
        assert_eq!(scaled.line_scale, rpc.line_scale / 4.);
        assert_eq!(scaled.samp_scale, rpc.samp_scale / 4.);
        assert_eq!(scaled.line_num_coeff, rpc.line_num_coeff);
    }

    #[test]
    fn serialized_warp_preserves_sampling_options_and_rejects_creation_options() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.tif");
        crate::write_raster_dataset(&raster(), source.to_str().unwrap(), &Default::default()).unwrap();
        let options = WarpOptions { source_extra: Some(7), sample_steps_all: true, error_threshold: 0., ..Default::default() };
        let document = VirtualRaster::warped(source.to_str().unwrap(), &options).unwrap();
        let warp = document.root.child("GDALWarpOptions").unwrap();
        assert!(warp.children.iter().any(|e| e.attr("name") == Some("SOURCE_EXTRA") && e.text == "7"));
        assert!(warp.children.iter().any(|e| e.attr("name") == Some("SAMPLE_STEPS") && e.text == "ALL"));
        assert!(descendant(warp, "ApproxTransformer").is_none());
        let reader = Reader {
            description: std::sync::Arc::new(document),
            mask: None,
            base: String::new(),
            sources: HashMap::new(),
            cached: HashMap::new(),
            lru: Default::default(),
        };
        let parsed = reader.parse_warp().unwrap();
        assert_eq!(parsed.options.error_threshold, 0.);
        assert_eq!(parsed.options.source_extra, Some(7));
        assert!(parsed.options.sample_steps_all);
        let paths = vec![source.to_string_lossy().into_owned()];
        let out = dir.path().join("out.vrt");
        let write = crate::RasterWriteOptions { compression: Some("DEFLATE".into()), ..Default::default() };
        assert!(crate::ops::warp::warp_to_path(&paths, &options, out.to_str().unwrap(), &write).is_err());
        assert!(!out.exists());
        let mount = crate::memfs::Mount::new([]);
        let virtual_path = mount.path().join("warped.vrt");
        crate::ops::warp::warp_to_path(&paths, &options, virtual_path.to_str().unwrap(), &Default::default()).unwrap();
        assert!(crate::memfs::exists(&virtual_path));
        let mut opened = open_virtual_raster(virtual_path.to_str().unwrap()).unwrap();
        assert_eq!(opened.read().unwrap().raster.data.to_f64_vec(), raster().raster.data.to_f64_vec());
    }

    #[test]
    fn serializes_unknown_source_crs_into_known_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.tif");
        crate::write_raster_dataset(&raster(), path.to_str().unwrap(), &Default::default()).unwrap();
        let options = WarpOptions { dst_srs: Some(crate::crs::SpatialRef::from_epsg(4326).unwrap()), ..Default::default() };
        let document = VirtualRaster::warped(path.to_str().unwrap(), &options).unwrap();
        assert!(!document.to_xml().contains("<SourceSRS></SourceSRS>"));
        let mut lazy = document.into_lazy(dir.path(), HashMap::new()).unwrap();
        assert_eq!(lazy.read().unwrap().raster.data.to_f64_vec(), raster().raster.data.to_f64_vec());
    }

    #[test]
    fn supplied_sources_and_windows() {
        let text = document(&format!("<VRTRasterBand dataType=\"Float64\">{}</VRTRasterBand>", source("SimpleSource", "")));
        let mut lazy = VirtualRaster::from_xml(&text).unwrap().into_lazy(Path::new(""), sources()).unwrap();
        let out = lazy.read_window(Window::new(2, 1, 2, 2), &WindowOptions::default()).unwrap();
        assert_eq!(out.raster.data.to_f64_vec(), vec![7., 8., 12., 13.]);
        assert_eq!(out.geo_transform(), [2., 1., 0., 4., 0., -1.]);
    }

    #[test]
    fn derived_sum_and_serialized_roundtrip() {
        let text = document(&format!(
            "<VRTRasterBand dataType=\"Float64\" subClass=\"VRTDerivedRasterBand\"><PixelFunctionType>sum</PixelFunctionType><PixelFunctionArguments k=\"2\"/>{}{}</VRTRasterBand>",
            source("SimpleSource", ""),
            source("SimpleSource", "")
        ));
        let parsed = VirtualRaster::from_xml(&text).unwrap();
        let parsed = VirtualRaster::from_xml(&parsed.to_xml()).unwrap();
        let mut lazy = parsed.into_lazy(Path::new(""), sources()).unwrap();
        assert_eq!(
            lazy.read_window(Window::new(2, 1, 2, 2), &Default::default()).unwrap().raster.data.to_f64_vec(),
            vec![16., 18., 26., 28.]
        );
    }

    #[test]
    fn kernel_reads_halo_outside_requested_window() {
        let text = document(&format!(
            "<VRTRasterBand dataType=\"Float64\">{}</VRTRasterBand>",
            source("KernelFilteredSource", "<Kernel normalized=\"1\"><Size>3</Size><Coefs>1 1 1 1 1 1 1 1 1</Coefs></Kernel>")
        ));
        let mut lazy = VirtualRaster::from_xml(&text).unwrap().into_lazy(Path::new(""), sources()).unwrap();
        let all = lazy.read().unwrap();
        let part = lazy.read_window(Window::new(2, 1, 2, 2), &Default::default()).unwrap();
        assert_eq!(part.raster.data.to_f64_vec(), crate::cog::cut(&all.raster, Window::new(2, 1, 2, 2), &[1]).to_f64_vec());
        assert_eq!(part.raster.data.to_f64_vec(), vec![7., 8., 12., 13.]);
    }

    #[test]
    fn dataset_mask_is_read_by_window() {
        let text = document(&format!(
            "<VRTRasterBand dataType=\"Float64\">{}</VRTRasterBand><MaskBand><VRTRasterBand dataType=\"Byte\">{}</VRTRasterBand></MaskBand>",
            source("SimpleSource", ""),
            source("SimpleSource", "")
        ));
        let mut lazy = VirtualRaster::from_xml(&text).unwrap().into_lazy(Path::new(""), sources()).unwrap();
        assert!(lazy.has_mask());
        assert_eq!(lazy.read_mask(Window::new(0, 0, 2, 1), None).unwrap(), Some(vec![0, 255]));
    }

    #[test]
    fn cropped_virtual_reads_bound_source_io() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Counted(Arc<AtomicUsize>);
        impl WindowSource for Counted {
            fn overviews(&self) -> Vec<(usize, usize)> {
                Vec::new()
            }
            fn read(&mut self, _level: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
                self.0.fetch_add(w.width * w.height * bands.len(), Ordering::Relaxed);
                Ok(RasterData::F64(
                    bands
                        .iter()
                        .flat_map(|_| (w.y..w.y + w.height).flat_map(|y| (w.x..w.x + w.width).map(move |x| (x + y) as f64)))
                        .collect(),
                ))
            }
        }
        let reads = Arc::new(AtomicUsize::new(0));
        let grid = Grid { width: 1024, height: 1024, geo_transform: [0., 1., 0., 1024., 0., -1.] };
        let header = crate::ops::raster_header(grid, DataType::F64, 1, None, &Default::default()).unwrap();
        let source = LazyRaster::from_source(header, Box::new(Counted(reads.clone())));
        let xml = format!(
            "<VRTDataset rasterXSize=\"1024\" rasterYSize=\"1024\"><VRTRasterBand dataType=\"Float64\">{}</VRTRasterBand></VRTDataset>",
            self::source("SimpleSource", "")
        );
        let mut virtual_raster =
            VirtualRaster::from_xml(&xml).unwrap().into_lazy(Path::new(""), HashMap::from([("source.tif".into(), source)])).unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        let result = virtual_raster.read_window(Window::new(500, 500, 10, 10), &Default::default()).unwrap();
        assert_eq!(result.raster.data.to_f64_vec()[0], 1000.);
        assert_eq!(reads.load(Ordering::Relaxed), 144);
    }

    #[test]
    fn rejects_external_functions_and_invalid_kernels() {
        let python = document(
            "<VRTRasterBand subClass=\"VRTDerivedRasterBand\"><PixelFunctionType>sum</PixelFunctionType><PixelFunctionLanguage>Python</PixelFunctionLanguage></VRTRasterBand>",
        );
        assert!(VirtualRaster::from_xml(&python).is_err());
        let bad = document(&format!(
            "<VRTRasterBand>{}</VRTRasterBand>",
            source("KernelFilteredSource", "<Kernel><Size>2</Size><Coefs>1 1 1 1</Coefs></Kernel>")
        ));
        assert!(VirtualRaster::from_xml(&bad).is_err());
        // GDAL 3.13.3 rejects these KernelFilteredSource Function names, even
        // though sum/mean are supported by the separate neighbors operation.
        for function in ["sum", "mean", "mystery"] {
            let xml = document(&format!(
                "<VRTRasterBand>{}</VRTRasterBand>",
                source(
                    "KernelFilteredSource",
                    &format!("<Kernel><Size>3</Size><Coefs>1 1 1 1 1 1 1 1 1</Coefs></Kernel><Function>{function}</Function>")
                )
            ));
            assert!(VirtualRaster::from_xml(&xml).is_err(), "{function}");
        }
    }
}

#[cfg(test)]
mod inventory_tests {
    use super::*;
    #[test]
    fn supplied_alias_inventory_is_recursive_and_unknown_sources_fail_before_create() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Input {
            path: String,
            known: bool,
            reads: Arc<AtomicUsize>,
        }
        impl WindowSource for Input {
            fn overviews(&self) -> Vec<(usize, usize)> {
                Vec::new()
            }
            fn input_paths(&self) -> Result<Vec<String>> {
                if self.known { Ok(vec![self.path.clone()]) } else { Err(unsupported("source inventory unavailable")) }
            }
            fn read(&mut self, _: usize, _: Window, _: &[usize]) -> Result<RasterData> {
                assert_eq!(std::fs::read(&self.path).unwrap(), b"keep");
                self.reads.fetch_add(1, Ordering::Relaxed);
                Ok(RasterData::U8(vec![7]))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("actual.tif");
        let xml = "<VRTDataset rasterXSize=\"1\" rasterYSize=\"1\"><GeoTransform>0,1,0,1,0,-1</GeoTransform><VRTRasterBand dataType=\"Byte\"><SimpleSource><SourceFilename>unrelated-alias</SourceFilename></SimpleSource></VRTRasterBand></VRTDataset>";
        for known in [false, true] {
            std::fs::write(&destination, b"keep").unwrap();
            let reads = Arc::new(AtomicUsize::new(0));
            let header = crate::ops::raster_header(
                Grid { width: 1, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
                DataType::U8,
                1,
                None,
                &Default::default(),
            )
            .unwrap();
            let source = LazyRaster::from_source(
                header,
                Box::new(Input { path: destination.to_string_lossy().into_owned(), known, reads: reads.clone() }),
            );
            let vrt =
                VirtualRaster::from_xml(xml).unwrap().into_lazy(dir.path(), HashMap::from([("unrelated-alias".into(), source)])).unwrap();
            let mut layer = wbvector::Layer::new("tiles");
            layer.add_field(wbvector::FieldDef::new("location", wbvector::FieldType::Text));
            let mut feature = wbvector::Feature::with_geometry(
                0,
                wbvector::Geometry::polygon(
                    vec![
                        wbvector::Coord::xy(0., 0.),
                        wbvector::Coord::xy(1., 0.),
                        wbvector::Coord::xy(1., 1.),
                        wbvector::Coord::xy(0., 1.),
                    ],
                    vec![],
                ),
                1,
            );
            feature.attributes[0] = wbvector::FieldValue::Text("vrt-alias".into());
            layer.push(feature);
            let catalog = crate::catalog::RasterCatalog::from_lazy_layer(
                layer,
                HashMap::from([("vrt-alias".into(), vrt)]),
                crate::catalog::CatalogOptions {
                    resolution: Some((1., 1.)),
                    bands: Some(1),
                    data_type: Some(DataType::U8),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(reads.load(Ordering::Relaxed), 0);
            let result = catalog.into_lazy().write(destination.to_str().unwrap(), &Default::default());
            if known {
                result.unwrap();
                assert_eq!(reads.load(Ordering::Relaxed), 1);
                assert_eq!(crate::open_raster(destination.to_str().unwrap()).unwrap().raster.data.to_f64_vec(), vec![7.]);
            } else {
                assert!(result.unwrap_err().to_string().contains("inventory unavailable"));
                assert_eq!(reads.load(Ordering::Relaxed), 0);
                assert_eq!(std::fs::read(&destination).unwrap(), b"keep");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn memoized_symlink_aliases_preserve_each_reference_base() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let xml = dir.path().join("shared.vrt");
        std::fs::write(&xml, "<VRTDataset rasterXSize=\"1\" rasterYSize=\"1\"><VRTRasterBand dataType=\"Byte\"><SimpleSource><SourceFilename relativeToVRT=\"1\">tile.tif</SourceFilename></SimpleSource></VRTRasterBand></VRTDataset>").unwrap();
        for parent in [&a, &b] {
            std::os::unix::fs::symlink(&xml, parent.join("alias.vrt")).unwrap();
        }
        let _inventory = SourceInventoryScope::enter();
        for parent in [&a, &b] {
            let paths = source_input_paths(parent.join("alias.vrt").to_str().unwrap()).unwrap();
            assert!(paths.contains(&parent.join("tile.tif").to_string_lossy().into_owned()));
        }
    }

    #[cfg(remote)]
    #[test]
    fn remote_inventory_fetches_only_xml_and_deduplicates_catalog_locations() {
        use std::io::{BufRead, Write};
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        let dir = tempfile::tempdir().unwrap();
        let leaf = dir.path().join("unopened.tif");
        let source = |name: &str, relative: bool| {
            format!(
                "<SimpleSource><SourceFilename relativeToVRT=\"{}\">{name}</SourceFilename><SourceBand>1</SourceBand></SimpleSource>",
                u8::from(relative)
            )
        };
        let document = |sources: String| {
            format!(
                "<VRTDataset rasterXSize=\"1\" rasterYSize=\"1\"><VRTRasterBand dataType=\"Byte\">{sources}</VRTRasterBand></VRTDataset>"
            )
        };
        let root = document(format!("{}{}", source("a.vrt", true), source("b.vrt", true)));
        let parent = document(source("nested.vrt", true));
        let nested = document(source(leaf.to_str().unwrap(), false));
        let expected_bytes = root.len() + 2 * parent.len() + nested.len();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_stop = stop.clone();
        let thread_requests = requests.clone();
        let server = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Windows inherits the listener's nonblocking mode on accepted sockets.
                        stream.set_nonblocking(false).unwrap();
                        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                        let mut request = String::new();
                        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                        reader.read_line(&mut request).unwrap();
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).unwrap() == 0 || header.trim().is_empty() {
                                break;
                            }
                        }
                        let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                        let payload = match path.as_str() {
                            "/root.vrt" => &root,
                            "/a.vrt" | "/b.vrt" => &parent,
                            "/nested.vrt" => &nested,
                            _ => "",
                        };
                        thread_requests.lock().unwrap().push((path, payload.len()));
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload)
                            .unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(std::time::Duration::from_millis(1)),
                    Err(error) => panic!("{error}"),
                }
            }
        });
        let run = || -> Result<()> {
            let url = format!("http://{address}/root.vrt");
            let mut layer = wbvector::Layer::new("tiles");
            layer.add_field(wbvector::FieldDef::new("location", wbvector::FieldType::Text));
            for name in ["a", "b"] {
                let mut feature = wbvector::Feature::with_geometry(
                    layer.features.len() as u64,
                    wbvector::Geometry::polygon(
                        vec![
                            wbvector::Coord::xy(0., 0.),
                            wbvector::Coord::xy(1., 0.),
                            wbvector::Coord::xy(1., 1.),
                            wbvector::Coord::xy(0., 1.),
                        ],
                        vec![],
                    ),
                    1,
                );
                feature.attributes[0] = wbvector::FieldValue::Text(name.into());
                layer.push(feature);
            }
            let options = crate::catalog::CatalogOptions {
                resolution: Some((1., 1.)),
                bands: Some(1),
                data_type: Some(DataType::U8),
                ..Default::default()
            };
            let catalog = crate::catalog::RasterCatalog::from_sources(
                layer.clone(),
                HashMap::from([
                    ("a".into(), crate::catalog::CatalogInput::path(&url)),
                    ("b".into(), crate::catalog::CatalogInput::path(&url)),
                ]),
                options.clone(),
            )?
            .into_lazy();
            assert_eq!(catalog.input_paths()?, vec![leaf.to_string_lossy().into_owned()]);
            assert_eq!(catalog.input_paths()?, vec![leaf.to_string_lossy().into_owned()]);
            assert_eq!(requests.lock().unwrap().len(), 4);
            #[cfg(feature = "sqlite")]
            {
                for feature in &mut layer.features {
                    feature.attributes[0] = wbvector::FieldValue::Text(url.clone());
                }
                let index = dir.path().join("remote.gti.gpkg");
                crate::write_vector(&[layer], index.to_str().unwrap(), &Default::default())?;
                let catalog = crate::catalog::RasterCatalog::open(index.to_str().unwrap(), options)?.into_lazy();
                let paths = catalog.input_paths()?;
                assert!(paths.contains(&leaf.to_string_lossy().into_owned()));
                assert_eq!(requests.lock().unwrap().len(), 8);
            }
            Ok(())
        };
        let result = run();
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
        result.unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests.iter().all(|(path, _)| matches!(path.as_str(), "/root.vrt" | "/a.vrt" | "/b.vrt" | "/nested.vrt")));
        let passes = if cfg!(feature = "sqlite") { 2 } else { 1 };
        assert_eq!(requests.iter().map(|(_, bytes)| bytes).sum::<usize>(), expected_bytes * passes);
        assert!(expected_bytes < 4096);
        assert!(!leaf.exists());
    }

    #[test]
    fn inventories_subdatasets_masks_and_archives_without_opening_raster_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let mask = dir.path().join("mask.tif");
        std::fs::write(&mask, b"not raster pixels").unwrap();
        std::fs::write(dir.path().join("mask.tif.msk"), b"not raster pixels").unwrap();
        for selector in [
            "NETCDF:\"science.nc\":temperature",
            "HDF5:\"science.h5\"://group/data",
            "ZARR:\"science.zarr\":/data",
            "ZARR:\"science.zarr\"",
            "ZARR:science.zarr",
            "GPKG:science.gpkg:tiles",
        ] {
            let xml = format!(
                "<VRTDataset rasterXSize=\"1\" rasterYSize=\"1\"><VRTRasterBand dataType=\"Byte\"><SimpleSource><SourceFilename relativeToVRT=\"1\">{selector}</SourceFilename></SimpleSource></VRTRasterBand><MaskBand><VRTRasterBand dataType=\"Byte\"><SimpleSource><SourceFilename relativeToVRT=\"1\">mask.tif</SourceFilename></SimpleSource></VRTRasterBand></MaskBand></VRTDataset>"
            );
            let reader = VirtualRaster::from_xml(&xml).unwrap().into_lazy(dir.path(), HashMap::new()).unwrap();
            let paths = reader.input_paths().unwrap();
            // Resolved references can use forward slashes even on Windows.
            assert!(paths.iter().any(|path| Path::new(path) == mask));
            assert!(paths.iter().any(|path| Path::new(path) == dir.path().join("mask.tif.msk")));
            assert!(paths.iter().any(|p| p.contains("science.")
                && !p.contains("NETCDF:")
                && !p.contains("HDF5:")
                && !p.contains("ZARR:")
                && !p.contains("GPKG:")));
        }
        assert!(source_input_paths("UNSUPPORTED_DRIVER:source.dat:band").is_err());
        // An explicit filesystem path disambiguates literal driver-like names.
        assert!(source_input_paths("DATA:1.tif").is_err());
        assert_eq!(source_input_paths("./DATA:1.tif").unwrap(), vec!["./DATA:1.tif"]);
        let literal = dir.path().join("DATA:1.tif").to_string_lossy().into_owned();
        assert_eq!(source_input_paths(&literal).unwrap(), vec![literal]);
        let archive = dir.path().join("unopened.zip");
        let paths = source_input_paths(&format!("{}.zip!tile.tif", dir.path().join("unopened").display())).unwrap();
        assert_eq!(paths, vec![archive.to_string_lossy().into_owned()]);
    }
}
