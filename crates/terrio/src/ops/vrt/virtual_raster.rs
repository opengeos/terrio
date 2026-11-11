// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/vrt/vrtwarped.cpp
//     frmts/vrt/pixelfunctions.cpp
//     frmts/vrt/vrtfilters.cpp
// No code is translated from these sources.

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
    /// Parse a serialized VRT without opening its sources or executing embedded code. Under a
    /// process-wide raster allow-list, only with the VRT driver allowed.
    pub fn from_xml(text: &str) -> Result<Self> {
        crate::formats::check_driver_allowed("VRT", "a VRT description", crate::formats::Kind::Raster)?;
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
        // A warped VRT is resampled by whoever reads it, with the version configured there (as
        // GDAL's warped VRTs, with the reader's GDAL): a different explicit one cannot be kept.
        if options.kernel_version != crate::ops::warp::KernelVersion::configured()
            || options.grid_invariant != crate::ops::warp::grid_invariant_configured()
            || options.transform_options != Default::default()
        {
            return Err(crate::Error::Unsupported(
                "non-configured kernel versions or grid policies and strict coordinate-operation options require a materialized warp output".into(),
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
            xml::escape_filename(&source)
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
                levels: Default::default(),
                sources,
            }),
        ))
    }
}

/// Open a local, archived or remote VRT and resolve relative sources from its location.
pub fn open_virtual_raster(path: &str) -> Result<LazyRaster> {
    // Opened as a VRT, by its name or by its caller: only if the VRT driver is allowed.
    crate::formats::check_driver_allowed("VRT", path, crate::formats::Kind::Raster)?;
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
    let _scope = crate::stac::AssetReadPolicy::enter_nested(file)?;
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
pub(crate) fn subdataset_parts(name: &str) -> Result<Option<(&str, &str, &str)>> {
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
    /// The VRT's implicit overviews, found the first time they are asked for
    /// ([`Reader::implicit_overviews`]).
    levels: std::sync::OnceLock<Vec<(usize, usize)>>,
}

impl Reader {
    /// The VRT's implicit overviews, `(width, height)` largest first: those of its first source
    /// (of the first band that has one, in document order), at the VRT's scale. A level reads
    /// each source from its own level of the same scale or from the nearest finer one
    /// ([`Reader::source_values`]), so the first source alone is opened to list them, once, for
    /// its header. Empty for a warped or pansharpened VRT, a VRT with a kernel source, whose
    /// kernel spans full-resolution pixels, or a first source that cannot be opened.
    /// The listing is fixed for this reader's lifetime, including an empty listing after a
    /// failed source open; open a new reader to retry discovery after a transient failure.
    fn implicit_overviews(&self) -> Vec<(usize, usize)> {
        let description: &VirtualRaster = &self.description;
        let Ok(_depth) = ReadDepth::enter() else {
            return Vec::new();
        };
        let kernel = [Some(description), self.mask.as_deref()]
            .into_iter()
            .flatten()
            .flat_map(|d| d.root.children.iter().filter(|e| e.name == "VRTRasterBand"))
            .any(|band| band.children.iter().any(|s| s.name == "KernelFilteredSource"));
        if description.warped || description.pansharpen.is_some() || kernel {
            return Vec::new();
        }
        let Some(first) = description.layout.bands.iter().flat_map(|b| &b.sources).next() else {
            return Vec::new();
        };
        let path = resolve_reference(&self.base, &first.filename, first.relative_to_vrt);
        let opened;
        let source = match self.sources.get(&first.filename).or_else(|| self.cached.get(&path)) {
            Some(source) => source,
            None => match crate::open_raster_lazy(&path) {
                Ok(source) => {
                    opened = source;
                    &opened
                }
                Err(_) => return Vec::new(),
            },
        };
        let (sw, sh) = (source.width() as f64, source.height() as f64);
        let rect = if first.src_rect[0].is_nan() { [0., 0., sw, sh] } else { first.src_rect };
        // The first source's pixels in a VRT pixel, along x and y.
        let (rx, ry) = (rect[2] / first.dst_rect[2], rect[3] / first.dst_rect[3]);
        if !(rx.is_finite() && ry.is_finite() && rx > 0. && ry > 0.) {
            return Vec::new();
        }
        let (w, h) = (description.layout.width, description.layout.height);
        let mut levels: Vec<(usize, usize)> = Vec::new();
        for (ow, oh) in source.overviews() {
            // The VRT's pixels in a pixel of the level.
            let (fx, fy) = (sw / ow as f64 / rx, sh / oh as f64 / ry);
            let size = (level_size(w, fx), level_size(h, fy));
            let above = levels.last().copied().unwrap_or((w, h));
            if fx > 1. && fy > 1. && size.0 <= above.0 && size.1 <= above.1 && size != above {
                levels.push(size);
            }
        }
        levels
    }

    /// The size of level `level` of the VRT: its own at 0, its overview `level - 1` above.
    fn level(&self, level: usize) -> Result<(usize, usize)> {
        if level == 0 {
            return Ok((self.description.layout.width, self.description.layout.height));
        }
        self.levels.get_or_init(|| self.implicit_overviews()).get(level - 1).copied().ok_or_else(|| invalid("no such overview"))
    }

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

    /// The values of a source in a window of a level of the VRT, of size `level` (the VRT's own
    /// size at full resolution). At an overview, the source's `DstRect` is scaled to the level,
    /// and the source is read from its own level of the same scale, or from the nearest finer one
    /// (full resolution at worst), its `SrcRect` scaled to it: the resampling below brings a
    /// finer level to the VRT's.
    fn source_values(
        &mut self,
        src: &VrtSource,
        element: &xml::Element,
        window: Window,
        output_type: DataType,
        level: (usize, usize),
    ) -> Result<Vec<Option<f64>>> {
        let valid_rect = |r: &[f64; 4]| r.iter().all(|v| v.is_finite()) && r[2] > 0. && r[3] > 0.;
        if !valid_rect(&src.dst_rect) || (!src.src_rect[0].is_nan() && !valid_rect(&src.src_rect)) {
            return Err(invalid("invalid source/destination rectangle"));
        }
        let full = (self.description.layout.width, self.description.layout.height);
        let overview = level != full;
        let dst = if overview {
            let [x, y, w, h] = src.dst_rect;
            let (lw, lh, fw, fh) = (level.0 as f64, level.1 as f64, full.0 as f64, full.1 as f64);
            [x * lw / fw, y * lh / fh, w * lw / fw, h * lh / fh]
        } else {
            src.dst_rect
        };
        // GDAL's `VRTSimpleSource::GetSrcDstWindow()`: a request that misses the source's
        // destination rectangle leaves the source alone, unopened: a mosaic reads the windows
        // over its other sources when this one cannot be opened (a remote source that is down).
        let [dx, dy, dw, dh] = dst;
        let (wx, wy) = (window.x as f64, window.y as f64);
        if wx >= dx + dw || wy >= dy + dh || wx + window.width as f64 <= dx || wy + window.height as f64 <= dy {
            return Ok(vec![None; window.width * window.height]);
        }
        let source = self.source(&src.filename, src.relative_to_vrt)?;
        let (width, height) = (source.width(), source.height());
        let rect = if src.src_rect[0].is_nan() { [0., 0., width as f64, height as f64] } else { src.src_rect };
        if !valid_rect(&rect) {
            return Err(invalid("invalid source/destination rectangle"));
        }
        // The source's level: its overview of the level's scale, the one that maps to the level's
        // size as [`Reader::implicit_overviews`] maps the first source's (a size rounded up gives
        // the level's pixels slightly fewer of the source's pixels than that overview's hold).
        // Else, of its overviews (largest first), the last whose pixels hold no more of its
        // full-resolution pixels than a pixel of the VRT's level does.
        let (source_level, width, height, rect) = if overview {
            let need = (rect[2] / dst[2], rect[3] / dst[3]);
            // The source's full-resolution pixels in a pixel of the VRT.
            let (sx, sy) = (rect[2] / src.dst_rect[2], rect[3] / src.dst_rect[3]);
            let same_scale = |&(_, &(ow, oh)): &(usize, &(usize, usize))| {
                (level_size(full.0, width as f64 / ow as f64 / sx), level_size(full.1, height as f64 / oh as f64 / sy)) == level
            };
            let fits = |&(_, &(ow, oh)): &(usize, &(usize, usize))| {
                width as f64 / ow as f64 <= need.0 * (1. + 1e-9) && height as f64 / oh as f64 <= need.1 * (1. + 1e-9)
            };
            let overviews = source.overviews();
            match overviews.iter().enumerate().find(same_scale).or_else(|| overviews.iter().enumerate().rev().find(fits)) {
                Some((i, &(ow, oh))) => {
                    let (sw, sh, w, h) = (ow as f64, oh as f64, width as f64, height as f64);
                    (Some(i), ow, oh, [rect[0] * sw / w, rect[1] * sh / h, rect[2] * sw / w, rect[3] * sh / h])
                }
                None => (None, width, height, rect),
            }
        } else {
            (None, width, height, rect)
        };
        let kernel = element.child("Kernel");
        let radius = kernel.map(kernel_size).transpose()?.unwrap_or(1) / 2;
        // A copy picked by nearest neighbour (the same size, on whole pixels) needs no margin.
        let copy = rect[2] == dst[2] && rect[3] == dst[3] && [rect[0], rect[1], dst[0], dst[1]].iter().all(|v| v.fract() == 0.0);
        let pad = radius
            + match src.resampling {
                Resampling::Nearest if copy => 0,
                Resampling::Nearest => 1,
                Resampling::Bilinear => 2,
                _ => 4,
            };
        let mapx = |x: f64| rect[0] + (x - dst[0]) * rect[2] / dst[2];
        let mapy = |y: f64| rect[1] + (y - dst[1]) * rect[3] / dst[3];
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
        let ds = source.read_window(area, &WindowOptions { bands: vec![src.band], overview: source_level })?;
        let mut data = if mask_band {
            let mask = source.read_mask(area, source_level)?.unwrap_or_else(|| {
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
            && let Some(mask) = source.read_mask(area, source_level)?
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
            dst_rect: [dst[0] - window.x as f64, dst[1] - window.y as f64, dst[2], dst[3]],
            resampling: src.resampling,
            working_float: src.complex,
            invalid_value: None,
            within_rects: true,
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
            source: source.text.clone(),
            source_overview,
            relative: super::relative_reference_flag(source.attr("relativeToVRT"), false),
            options,
            pairs,
            source_nodata,
            destination_nodata,
        })
    }
}

impl WindowSource for Reader {
    fn geotransform_present(&self) -> Option<bool> {
        Some(self.description.layout.geo_transform.is_some())
    }

    fn band_data_type(&self, band: usize) -> Option<DataType> {
        band.checked_sub(1).and_then(|i| self.description.layout.bands.get(i)).map(|b| b.data_type)
    }

    fn input_paths(&self) -> Result<Vec<String>> {
        let _inventory = SourceInventoryScope::enter();
        let _depth = ReadDepth::enter()?;
        fn references<'a>(element: &'a xml::Element, raw: bool, out: &mut Vec<(&'a str, bool)>) {
            let raw = if element.name == "VRTRasterBand" { element.attr("subClass") == Some("VRTRawRasterBand") } else { raw };
            if matches!(element.name.as_str(), "SourceFilename" | "SourceDataset") {
                out.push((element.text.as_str(), super::relative_reference_flag(element.attr("relativeToVRT"), raw)));
            }
            for child in &element.children {
                references(child, raw, out);
            }
        }
        let mut refs = Vec::new();
        references(&self.description.root, false, &mut refs);
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
    /// The VRT's implicit overviews ([`Reader::implicit_overviews`]).
    fn overviews(&self) -> Vec<(usize, usize)> {
        self.levels.get_or_init(|| self.implicit_overviews()).clone()
    }
    fn block_size(&self) -> Option<(usize, usize)> {
        Some(if self.description.warped { (512, 128) } else { (128, 128) })
    }
    fn has_mask(&self) -> bool {
        self.description.root.child("MaskBand").is_some()
    }
    /// A window reads the parts of the sources that overlap it, the values a whole read gives
    /// them, when each source reads by window itself and is copied or picked by nearest
    /// neighbour. A resampled source's window is padded by a fixed margin, which a downsampling
    /// kernel can reach past; a source read whole is decoded again for every window once more
    /// sources are read than the reader keeps open. A source supplied or already opened is
    /// judged by how it reads ([`LazyRaster::reads_by_window`], recursively for a VRT), one not
    /// yet opened by its format, and a VRT file not at all. A GeoTIFF that turns out to be read
    /// whole (not in place) is then read correctly, only decoded again for each window once
    /// evicted. Warped and pansharpened VRTs are not described here.
    fn reads_by_window(&self) -> bool {
        let description = &self.description;
        if description.warped || description.pansharpen.is_some() {
            return false;
        }
        description.layout.bands.iter().flat_map(|b| &b.sources).all(|s| {
            let opened =
                self.sources.get(&s.filename).or_else(|| self.cached.get(&resolve_reference(&self.base, &s.filename, s.relative_to_vrt)));
            let windowed = match opened {
                Some(source) => source.reads_by_window(),
                None => crate::formats::RasterDriver::for_extension(&s.filename)
                    .is_some_and(|d| d.reads_by_window() && d != crate::formats::RasterDriver::VRT),
            };
            let copied = !s.src_rect[0].is_nan() && s.src_rect[2] == s.dst_rect[2] && s.src_rect[3] == s.dst_rect[3];
            windowed && (copied || s.resampling == Resampling::Nearest)
        })
    }
    /// The least common multiple of the sources' block heights, when every source is copied
    /// row for row with its blocks starting on the VRT's rows of that multiple (as a mosaic of
    /// tiled GeoTIFFs placed on their tiles is): strips of such rows read each block once.
    /// Otherwise the VRT's own block height.
    fn row_alignment(&mut self) -> Option<usize> {
        let own = self.block_size().map(|(_, h)| h);
        if !self.reads_by_window() {
            return own;
        }
        let sources: Vec<VrtSource> = self.description.layout.bands.iter().flat_map(|b| b.sources.iter().cloned()).collect();
        let mut rows = 1usize;
        let mut blocks = Vec::new();
        for s in &sources {
            let whole = |v: f64| v.fract() == 0.0;
            if s.src_rect[3] != s.dst_rect[3] || !whole(s.src_rect[1]) || !whole(s.dst_rect[1]) {
                return own;
            }
            let Some((_, h)) = self.source(&s.filename, s.relative_to_vrt).ok().and_then(|r| r.block_size()) else {
                return own;
            };
            rows = rows / gcd(rows, h) * h;
            if rows > 4096 {
                return own;
            }
            blocks.push((s.dst_rect[1] - s.src_rect[1], h));
        }
        // Each source's blocks start on VRT rows of a multiple of their height, so strips of a
        // multiple of every height start on blocks of every source.
        let aligned = blocks.iter().all(|&(shift, h)| shift.rem_euclid(h as f64) == 0.0);
        if aligned && rows > 1 { Some(rows) } else { own }
    }
    fn read_mask(&mut self, level: usize, window: Window) -> Result<Option<Vec<u8>>> {
        let _depth = ReadDepth::enter()?;
        let Some(mask) = self.mask.clone() else {
            return Ok(None);
        };
        let size = self.level(level)?;
        self.read_description(&mask, window, &[1], size)
            .map(|v| Some(v.to_f64_vec().into_iter().map(|v| if v == 0. { 0 } else { 255 }).collect()))
    }

    fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
        let _depth = ReadDepth::enter()?;
        if self.description.warped {
            return self.warped_blocks(window, bands);
        }
        let description = self.description.clone();
        let size = self.level(level)?;
        self.read_description(&description, window, bands, size)
    }
}

impl Reader {
    fn read_description(
        &mut self,
        description: &VirtualRaster,
        window: Window,
        bands: &[usize],
        level: (usize, usize),
    ) -> Result<RasterData> {
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
                let input = self.source_values(source, xml, window, layout.data_type, level)?;
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

/// The size of a level `factor` times coarser than `n` pixels: `n / factor` rounded up, or to the
/// nearest whole number when within a billionth of it, as a quotient of sizes holds rounding error.
fn level_size(n: usize, factor: f64) -> usize {
    let v = n as f64 / factor;
    let r = v.round();
    (if (v - r).abs() <= 1e-9 * v.max(1.) { r } else { v.ceil() }).max(1.) as usize
}

/// The greatest common divisor.
fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
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
    fn vrt_written_by_strips_is_the_vrt_read_whole() {
        // A mosaic of two GeoTIFFs, side by side, with dataset and band metadata: written by
        // strips, as `LazyRaster::write` writes a source read by window, it is the file the
        // whole mosaic is written as.
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
        let rgb = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/resampled_reads/rgb_mask.tif");
        let band = |b: usize| {
            let source = |x: usize| {
                format!(
                    "<SimpleSource><SourceFilename relativeToVRT=\"0\">{rgb}</SourceFilename><SourceBand>{b}</SourceBand>\
                     <SrcRect xOff=\"0\" yOff=\"0\" xSize=\"64\" ySize=\"48\"/>\
                     <DstRect xOff=\"{x}\" yOff=\"0\" xSize=\"64\" ySize=\"48\"/></SimpleSource>"
                )
            };
            format!(
                "<VRTRasterBand dataType=\"Byte\" band=\"{b}\"><Metadata><MDI key=\"STATISTICS_MAXIMUM\">238</MDI></Metadata>\
                 {}{}</VRTRasterBand>",
                source(0),
                source(64)
            )
        };
        let vrt = at("mosaic.vrt");
        std::fs::write(
            &vrt,
            format!(
                "<VRTDataset rasterXSize=\"128\" rasterYSize=\"48\"><GeoTransform>500000, 30, 0, 4100000, 0, -30</GeoTransform>\
                 <Metadata><MDI key=\"AREA_OR_POINT\">Area</MDI><MDI key=\"TIFFTAG_ARTIST\">mosaic</MDI></Metadata>{}</VRTDataset>",
                (1..=3).map(band).collect::<String>()
            ),
        )
        .unwrap();
        let mut src = open_virtual_raster(&vrt).unwrap();
        assert!(src.reads_by_window());
        let options = crate::RasterWriteOptions { driver: Some(crate::formats::RasterDriver::Cog), ..Default::default() };
        let (whole, strips) = (at("whole.tif"), at("strips.tif"));
        crate::write_raster_dataset(&src.read().unwrap(), &whole, &options).unwrap();
        src.write(&strips, &options).unwrap();
        assert!(std::fs::read(&whole).unwrap() == std::fs::read(&strips).unwrap());

        // Sources resampled other than by nearest neighbour, VRT sources and files read whole
        // are read whole: a window of them is not always the whole read's.
        let one = |file: &str, size: usize, resampling: &str| {
            format!(
                "<VRTDataset rasterXSize=\"{size}\" rasterYSize=\"{size}\"><VRTRasterBand dataType=\"Byte\" band=\"1\">\
                 <ComplexSource resampling=\"{resampling}\"><SourceFilename relativeToVRT=\"0\">{file}</SourceFilename>\
                 <SourceBand>1</SourceBand><SrcRect xOff=\"0\" yOff=\"0\" xSize=\"64\" ySize=\"48\"/>\
                 <DstRect xOff=\"0\" yOff=\"0\" xSize=\"{size}\" ySize=\"{size}\"/></ComplexSource></VRTRasterBand></VRTDataset>"
            )
        };
        let reads_by_window = |xml: String| {
            let path = at("one.vrt");
            std::fs::write(&path, xml).unwrap();
            open_virtual_raster(&path).unwrap().reads_by_window()
        };
        assert!(reads_by_window(one(rgb, 20, "nearest")));
        assert!(!reads_by_window(one(rgb, 20, "lanczos")));
        assert!(!reads_by_window(one(&vrt, 20, "nearest")));
        assert!(!reads_by_window(one("/elsewhere/image.png", 20, "nearest")));
        // Supplied sources are judged by how they read: a GeoTIFF read in place does, a VRT
        // downsampling its own source by Lanczos does not.
        let supplied = |name: &str, source: LazyRaster| {
            let xml = one(name, 20, "nearest");
            let sources = HashMap::from([(name.to_string(), source)]);
            VirtualRaster::from_xml(&xml).unwrap().into_lazy(dir.path(), sources).unwrap().reads_by_window()
        };
        assert!(supplied("rgb", crate::open_raster_lazy(rgb).unwrap()));
        let nested = at("nested.vrt");
        std::fs::write(&nested, one(rgb, 20, "lanczos")).unwrap();
        assert!(!supplied("nested", open_virtual_raster(&nested).unwrap()));
        // A shared handle reads as the raster it shares does.
        assert!(!open_virtual_raster(&nested).unwrap().shared().reads_by_window());
        assert!(open_virtual_raster(&vrt).unwrap().shared().reads_by_window());
        assert!(supplied("nested", open_virtual_raster(&vrt).unwrap().shared()));
        assert!(!supplied("nested", open_virtual_raster(&nested).unwrap().shared()));
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
            levels: Default::default(),
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

    /// A VRT over `sources` (absolute paths, at `DstRect`s `(x, y)` of `w` x `h` source pixels
    /// each, one for one), `width` x `height` Float32 pixels, with `extra` after its band.
    fn mosaic(width: usize, height: usize, sources: &[(&Path, usize, usize, usize, usize)], extra: &str) -> String {
        let simple = sources
            .iter()
            .map(|(path, x, y, w, h)| {
                format!(
                    "<SimpleSource><SourceFilename relativeToVRT=\"0\">{}</SourceFilename><SourceBand>1</SourceBand><SrcRect xOff=\"0\" yOff=\"0\" xSize=\"{w}\" ySize=\"{h}\"/><DstRect xOff=\"{x}\" yOff=\"{y}\" xSize=\"{w}\" ySize=\"{h}\"/></SimpleSource>",
                    path.display()
                )
            })
            .collect::<String>();
        format!(
            "<VRTDataset rasterXSize=\"{width}\" rasterYSize=\"{height}\"><GeoTransform>0,1,0,{height},0,-1</GeoTransform><VRTRasterBand dataType=\"Float32\" band=\"1\">{simple}</VRTRasterBand>{extra}</VRTDataset>"
        )
    }

    fn open(xml: &str) -> LazyRaster {
        VirtualRaster::from_xml(xml).unwrap().into_lazy(Path::new(""), HashMap::new()).unwrap()
    }

    fn level(raster: &mut LazyRaster, overview: Option<usize>, window: Window) -> Vec<f64> {
        raster.read_window(window, &WindowOptions { bands: vec![1], overview }).unwrap().raster.data.to_f64_vec()
    }

    /// A VRT that maps a source one for one has the source's overviews, and reads each from the
    /// source's own, pixel for pixel.
    #[test]
    fn a_vrt_has_its_source_s_overviews_and_reads_each_from_the_source_s() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        let mut vrt = open(&mosaic(32, 32, &[(&path, 0, 0, 32, 32)], ""));
        assert_eq!(vrt.overviews(), vec![(16, 16), (8, 8), (4, 4)]);
        assert_eq!(vrt.overviews(), source.overviews());
        for (i, (w, h)) in source.overviews().into_iter().enumerate() {
            let want = level(&mut source, Some(i), Window::full(w, h));
            let got = level(&mut vrt, Some(i), Window::full(w, h));
            assert_eq!(
                got.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                want.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                "overview {i}"
            );
        }
    }

    /// A VRT has the overviews of its first source: a source without one of them is read from its
    /// nearest finer level (full resolution at worst), brought to the VRT's level by the source's
    /// resampling, here nearest.
    #[test]
    fn a_mosaic_has_its_first_source_s_overviews_and_reads_a_source_without_one_from_a_finer_level() {
        let dir = tempfile::tempdir().unwrap();
        let first = pyramid(dir.path());
        // 32 x 32 pixels constant over blocks of 8 x 8, with an overview of 2 only.
        for (name, overviews) in [("coarse.tif", vec![2]), ("none.tif", vec![])] {
            let path = dir.path().join(name);
            let values = (0..1024).map(|i| ((i / 32 / 8) * 4 + (i % 32) / 8) as f64).collect();
            let grid = Grid { width: 32, height: 32, geo_transform: [32., 1., 0., 32., 0., -1.] };
            let raster = crate::ops::build_raster(
                grid,
                vec![values],
                DataType::F32,
                None,
                &crate::crs::SpatialRef::from_epsg(3857).unwrap(),
                crate::ops::convert_value,
            )
            .unwrap();
            let options = crate::RasterWriteOptions { overviews, overview_resampling: Some("average".into()), ..Default::default() };
            crate::write_raster_dataset(&raster, path.to_str().unwrap(), &options).unwrap();
            let mut first_source = crate::open_raster_lazy(first.to_str().unwrap()).unwrap();
            let mut vrt = open(&mosaic(64, 32, &[(&first, 0, 0, 32, 32), (&path, 32, 0, 32, 32)], ""));
            assert_eq!(vrt.overviews(), vec![(32, 16), (16, 8), (8, 4)], "{name}");
            for (i, n) in [(1usize, 8usize), (2, 4)] {
                let got = level(&mut vrt, Some(i), Window::full(2 * n, n));
                let want = level(&mut first_source, Some(i), Window::full(n, n));
                let left: Vec<f64> = (0..n).flat_map(|r| got[r * 2 * n..r * 2 * n + n].to_vec()).collect();
                assert_eq!(left, want, "{name}, overview {i}: the first source's own");
                let right: Vec<f64> = (0..n).flat_map(|r| got[r * 2 * n + n..(r + 1) * 2 * n].to_vec()).collect();
                let blocks = n / 4;
                let expected: Vec<f64> = (0..n * n).map(|k| ((k / n / blocks) * 4 + (k % n) / blocks) as f64).collect();
                assert_eq!(right, expected, "{name}, overview {i}: the second source at the level");
            }
        }
    }

    /// A tile-like read of a level of a mosaic reads the sources in its window alone, each from
    /// its own level of that scale, or the nearest finer one; listing the levels reads nothing.
    #[test]
    fn a_reduced_read_of_a_mosaic_reads_the_sources_in_its_window_at_their_level() {
        use std::sync::{Arc, Mutex};
        type Reads = Arc<Mutex<Vec<(usize, usize, Window)>>>;
        struct Leveled {
            name: usize,
            overviews: Vec<(usize, usize)>,
            reads: Reads,
        }
        impl WindowSource for Leveled {
            fn overviews(&self) -> Vec<(usize, usize)> {
                self.overviews.clone()
            }
            fn read(&mut self, level: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
                self.reads.lock().unwrap().push((self.name, level, w));
                Ok(RasterData::F64(vec![(self.name * 10 + level) as f64; w.width * w.height * bands.len()]))
            }
        }
        let reads: Reads = Arc::default();
        let grid = Grid { width: 64, height: 64, geo_transform: [0., 1., 0., 64., 0., -1.] };
        let mut sources = HashMap::new();
        let mut simple = String::new();
        for name in 0..4 {
            // The last source has no overview of a quarter.
            let overviews = if name == 3 { vec![(32, 32)] } else { vec![(32, 32), (16, 16)] };
            let header = crate::ops::raster_header(grid, DataType::F64, 1, None, &Default::default()).unwrap();
            let source = LazyRaster::from_source(header, Box::new(Leveled { name, overviews, reads: reads.clone() }));
            sources.insert(format!("s{name}.tif"), source);
            let (x, y) = (64 * (name % 2), 64 * (name / 2));
            simple += &format!(
                "<SimpleSource><SourceFilename>s{name}.tif</SourceFilename><SourceBand>1</SourceBand><SrcRect xOff=\"0\" yOff=\"0\" xSize=\"64\" ySize=\"64\"/><DstRect xOff=\"{x}\" yOff=\"{y}\" xSize=\"64\" ySize=\"64\"/></SimpleSource>"
            );
        }
        let xml = format!(
            "<VRTDataset rasterXSize=\"128\" rasterYSize=\"128\"><VRTRasterBand dataType=\"Float64\">{simple}</VRTRasterBand></VRTDataset>"
        );
        let mut vrt = VirtualRaster::from_xml(&xml).unwrap().into_lazy(Path::new(""), sources).unwrap();
        assert_eq!(vrt.overviews(), vec![(64, 64), (32, 32)]);
        assert!(reads.lock().unwrap().is_empty());
        // At a quarter, the first source's quarter of the level: its own overview of a quarter.
        assert_eq!(level(&mut vrt, Some(1), Window::new(0, 0, 16, 16)), vec![2.; 256]);
        assert_eq!(*reads.lock().unwrap(), vec![(0, 2, Window::new(0, 0, 16, 16))]);
        reads.lock().unwrap().clear();
        // The last source's quarter: its overview of a half, the nearest finer level.
        assert_eq!(level(&mut vrt, Some(1), Window::new(16, 16, 16, 16)), vec![31.; 256]);
        assert_eq!(*reads.lock().unwrap(), vec![(3, 1, Window::new(0, 0, 32, 32))]);
    }

    /// A VRT lists the overviews of its first source alone: another source, here missing, is not
    /// opened until a read reaches it.
    #[test]
    fn a_vrt_takes_its_overviews_from_its_first_source_alone() {
        let dir = tempfile::tempdir().unwrap();
        let first = pyramid(dir.path());
        let missing = dir.path().join("missing.tif");
        let mut vrt = open(&mosaic(64, 32, &[(&first, 0, 0, 32, 32), (&missing, 32, 0, 32, 32)], ""));
        assert_eq!(vrt.overviews(), vec![(32, 16), (16, 8), (8, 4)]);
        let mut source = crate::open_raster_lazy(first.to_str().unwrap()).unwrap();
        assert_eq!(level(&mut vrt, Some(0), Window::new(0, 0, 16, 16)), level(&mut source, Some(0), Window::full(16, 16)));
        assert!(vrt.read_window(Window::new(16, 0, 16, 16), &WindowOptions { bands: vec![1], overview: Some(0) }).is_err());
    }

    /// A VRT at another scale than its first source has the source's overviews at its own scale,
    /// and reads each from the source's overview of the same pixels.
    #[test]
    fn a_vrt_at_half_its_source_s_resolution_has_the_source_s_coarser_overviews() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let xml = format!(
            "<VRTDataset rasterXSize=\"16\" rasterYSize=\"16\"><GeoTransform>0,2,0,32,0,-2</GeoTransform><VRTRasterBand dataType=\"Float32\" band=\"1\"><SimpleSource><SourceFilename relativeToVRT=\"0\">{}</SourceFilename><SourceBand>1</SourceBand><SrcRect xOff=\"0\" yOff=\"0\" xSize=\"32\" ySize=\"32\"/><DstRect xOff=\"0\" yOff=\"0\" xSize=\"16\" ySize=\"16\"/></SimpleSource></VRTRasterBand></VRTDataset>",
            path.display()
        );
        let mut vrt = open(&xml);
        assert_eq!(vrt.overviews(), vec![(8, 8), (4, 4)]);
        let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        assert_eq!(level(&mut vrt, Some(0), Window::full(8, 8)), level(&mut source, Some(1), Window::full(8, 8)));
        assert_eq!(level(&mut vrt, Some(1), Window::full(4, 4)), level(&mut source, Some(2), Window::full(4, 4)));
    }

    /// A kernel spans full-resolution pixels: a VRT with a kernel source has no overviews.
    #[test]
    fn a_vrt_with_a_kernel_source_has_no_overviews() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let kernel = format!(
            "<VRTDataset rasterXSize=\"32\" rasterYSize=\"32\"><VRTRasterBand dataType=\"Float32\"><KernelFilteredSource><SourceFilename relativeToVRT=\"0\">{}</SourceFilename><SourceBand>1</SourceBand><Kernel><Size>3</Size><Coefs>0 0 0 0 1 0 0 0 0</Coefs></Kernel></KernelFilteredSource></VRTRasterBand></VRTDataset>",
            path.display()
        );
        assert_eq!(open(&kernel).overviews(), Vec::<(usize, usize)>::new());
    }

    /// The mask band of a VRT is read at an overview from its sources' overviews too.
    #[test]
    fn a_vrt_s_mask_band_is_read_at_an_overview() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let mask = format!(
            "<MaskBand><VRTRasterBand dataType=\"Byte\"><SimpleSource><SourceFilename relativeToVRT=\"0\">{}</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></MaskBand>",
            path.display()
        );
        let mut vrt = open(&mosaic(32, 32, &[(&path, 0, 0, 32, 32)], &mask));
        let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        for (i, (w, h)) in source.overviews().into_iter().enumerate() {
            let values = level(&mut source, Some(i), Window::full(w, h));
            let want: Vec<u8> = values.iter().map(|&v| if v as u8 == 0 { 0 } else { 255 }).collect();
            assert_eq!(vrt.read_mask(Window::full(w, h), Some(i)).unwrap(), Some(want), "overview {i}");
        }
    }

    /// A derived band reads its sources' overview pixels at an overview, and its pixel function,
    /// here `mul`, applies to them. A product of averages is not an average of products, so the
    /// pixels show what was multiplied. It has the overviews of its first source.
    #[test]
    fn a_derived_band_applies_its_pixel_function_to_its_sources_overview_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (pyramid(dir.path()), leveled(dir.path(), "b.tif", (32, 32), &[2, 4, 8], |_, column| (2 * column + 1) as f64, None));
        let xml = format!(
            "<VRTDataset rasterXSize=\"32\" rasterYSize=\"32\"><VRTRasterBand dataType=\"Float64\" subClass=\"VRTDerivedRasterBand\"><PixelFunctionType>mul</PixelFunctionType><SourceTransferType>Float64</SourceTransferType>{}{}</VRTRasterBand></VRTDataset>",
            simple_source(&a, "1", None),
            simple_source(&b, "1", None)
        );
        let mut vrt = open(&xml);
        assert_eq!(vrt.overviews(), vec![(16, 16), (8, 8), (4, 4)]);
        let mut a = crate::open_raster_lazy(a.to_str().unwrap()).unwrap();
        let mut b = crate::open_raster_lazy(b.to_str().unwrap()).unwrap();
        for (i, (w, h)) in a.overviews().into_iter().enumerate() {
            let window = Window::full(w, h);
            let want: Vec<f64> =
                level(&mut a, Some(i), window).into_iter().zip(level(&mut b, Some(i), window)).map(|(a, b)| a * b).collect();
            assert_eq!(level(&mut vrt, Some(i), window), want, "overview {i}");
        }
        // The top left pixel of the overview of a half: (0 + 1 + 1 + 2) / 4 times (1 + 3 + 1 + 3) / 4.
        assert_eq!(level(&mut vrt, Some(0), Window::new(0, 0, 1, 1)), vec![2.]);
    }

    /// A `ComplexSource`'s `LUT`, and its `ScaleOffset` and `ScaleRatio`, apply at an overview to
    /// the overview pixels it reads. The LUT doubles a value up to 16 and adds 16 above.
    #[test]
    fn a_complex_source_s_lut_and_scaling_apply_to_its_overview_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let band = |extra: &str| {
            format!(
                "<VRTRasterBand dataType=\"Float32\"><ComplexSource><SourceFilename relativeToVRT=\"0\">{}</SourceFilename><SourceBand>1</SourceBand>{extra}</ComplexSource></VRTRasterBand>",
                path.display()
            )
        };
        let xml = format!(
            "<VRTDataset rasterXSize=\"32\" rasterYSize=\"32\">{}{}</VRTDataset>",
            band("<LUT>0:0,16:32,1040:1056</LUT>"),
            band("<ScaleOffset>1</ScaleOffset><ScaleRatio>3</ScaleRatio>")
        );
        let mut vrt = open(&xml);
        assert_eq!(vrt.overviews(), vec![(16, 16), (8, 8), (4, 4)]);
        let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        let lut = |v: f64| if v <= 16. { 2. * v } else { v + 16. };
        let mut read = |overview: usize, window: Window, band: usize| {
            vrt.read_window(window, &WindowOptions { bands: vec![band], overview: Some(overview) }).unwrap().raster.data.to_f64_vec()
        };
        for (i, (w, h)) in source.overviews().into_iter().enumerate() {
            let window = Window::full(w, h);
            let pixels = level(&mut source, Some(i), window);
            assert_eq!(read(i, window, 1), pixels.iter().map(|&v| lut(v)).collect::<Vec<_>>(), "LUT, overview {i}");
            assert_eq!(read(i, window, 2), pixels.iter().map(|&v| 3. * v + 1.).collect::<Vec<_>>(), "scaling, overview {i}");
        }
        // The top left pixel of the overview of a half, (0 + 1 + 1 + 2) / 4 = 1: 2 by the LUT, 4 scaled.
        assert_eq!(read(0, Window::new(0, 0, 1, 1), 1), vec![2.]);
        assert_eq!(read(0, Window::new(0, 0, 1, 1), 2), vec![4.]);
    }

    /// A Float32 GeoTIFF of `width` x `height` pixels of `value(row, column)` at `dir/name`, with
    /// the overviews of `factors` by `average` (as [`pyramid`] writes its own) and, when given, an
    /// internal mask (0 for an invalid pixel, row by row).
    fn leveled(
        dir: &Path,
        name: &str,
        (width, height): (usize, usize),
        factors: &[usize],
        value: impl Fn(usize, usize) -> f64,
        mask: Option<Vec<u8>>,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let source = crate::ops::build_raster(
            Grid { width, height, geo_transform: [0., 1., 0., height as f64, 0., -1.] },
            vec![(0..width * height).map(|i| value(i / width, i % width)).collect()],
            DataType::F32,
            None,
            &crate::crs::SpatialRef::from_epsg(3857).unwrap(),
            crate::ops::convert_value,
        )
        .unwrap();
        let options = crate::RasterWriteOptions {
            overviews: factors.to_vec(),
            overview_resampling: Some("average".into()),
            mask,
            ..Default::default()
        };
        crate::write_raster_dataset(&source, path.to_str().unwrap(), &options).unwrap();
        path
    }

    /// A `SimpleSource` of band `band` (`1`, or `mask,1` for its mask) of `path`, its `SrcRect` and
    /// `DstRect` the same `rect` when given.
    fn simple_source(path: &Path, band: &str, rect: Option<[usize; 4]>) -> String {
        let rects = rect.map_or(String::new(), |[x, y, w, h]| {
            format!(
                "<SrcRect xOff=\"{x}\" yOff=\"{y}\" xSize=\"{w}\" ySize=\"{h}\"/><DstRect xOff=\"{x}\" yOff=\"{y}\" xSize=\"{w}\" ySize=\"{h}\"/>"
            )
        });
        format!(
            "<SimpleSource><SourceFilename relativeToVRT=\"0\">{}</SourceFilename><SourceBand>{band}</SourceBand>{rects}</SimpleSource>",
            path.display()
        )
    }

    /// A level of a VRT is its size divided by the VRT pixels a pixel of the level spans, rounded
    /// up: a 97 x 97 mosaic of a 64 x 64 source with overviews of 32 and 16 has levels of 49 and
    /// 25. Their last column and last row are the other source's, which covers the rest; the
    /// first source's part of the level of a half is its own overview.
    #[test]
    fn a_vrt_s_levels_are_rounded_up() {
        let dir = tempfile::tempdir().unwrap();
        let first = leveled(dir.path(), "first.tif", (64, 64), &[2, 4], |row, column| (row * 64 + column) as f64, None);
        let rest = leveled(dir.path(), "rest.tif", (97, 97), &[], |_, _| 5., None);
        let xml = format!(
            "<VRTDataset rasterXSize=\"97\" rasterYSize=\"97\"><VRTRasterBand dataType=\"Float32\">{}{}{}</VRTRasterBand></VRTDataset>",
            simple_source(&first, "1", Some([0, 0, 64, 64])),
            simple_source(&rest, "1", Some([64, 0, 33, 97])),
            simple_source(&rest, "1", Some([0, 64, 64, 33]))
        );
        let mut vrt = open(&xml);
        assert_eq!(vrt.overviews(), vec![(49, 49), (25, 25)]);
        for (i, n) in [(0usize, 49usize), (1, 25)] {
            let got = level(&mut vrt, Some(i), Window::full(n, n));
            let last_column: Vec<f64> = (0..n).map(|row| got[row * n + n - 1]).collect();
            assert_eq!(last_column, vec![5.; n], "overview {i}: the last column");
            assert_eq!(got[(n - 1) * n..].to_vec(), vec![5.; n], "overview {i}: the last row");
        }
        let mut source = crate::open_raster_lazy(first.to_str().unwrap()).unwrap();
        assert_eq!(level(&mut vrt, Some(0), Window::new(0, 0, 32, 32)), level(&mut source, Some(0), Window::full(32, 32)));
    }

    /// A `SimpleSource` of band 1 of `path`, its `SrcRect` `src` and its `DstRect` `dst`, with
    /// `attributes` on its element.
    fn placed_source(path: &Path, [sx, sy, sw, sh]: [usize; 4], [dx, dy, dw, dh]: [usize; 4], attributes: &str) -> String {
        format!(
            "<SimpleSource{attributes}><SourceFilename relativeToVRT=\"0\">{}</SourceFilename><SourceBand>1</SourceBand><SrcRect xOff=\"{sx}\" yOff=\"{sy}\" xSize=\"{sw}\" ySize=\"{sh}\"/><DstRect xOff=\"{dx}\" yOff=\"{dy}\" xSize=\"{dw}\" ySize=\"{dh}\"/></SimpleSource>",
            path.display()
        )
    }

    /// A VRT's first source is the first of the first band that has one: a VRT whose first band
    /// has none has its second band's source's overviews.
    #[test]
    fn a_vrt_s_first_source_is_that_of_its_first_band_with_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let xml = format!(
            "<VRTDataset rasterXSize=\"32\" rasterYSize=\"32\"><VRTRasterBand dataType=\"Float32\" band=\"1\"/><VRTRasterBand dataType=\"Float32\" band=\"2\">{}</VRTRasterBand></VRTDataset>",
            simple_source(&path, "1", None)
        );
        assert_eq!(open(&xml).overviews(), vec![(16, 16), (8, 8), (4, 4)]);
    }

    /// The first source's overviews are mapped through its `SrcRect`: a VRT of a 16 x 16 window of
    /// a 32 x 32 source, one for one, has the source's overviews of 16, 8 and 4 as its own of 8, 4
    /// and 2, and each reads the window's part of them.
    #[test]
    fn a_vrt_of_a_window_of_its_source_has_the_source_s_overviews_at_its_scale() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let xml = format!(
            "<VRTDataset rasterXSize=\"16\" rasterYSize=\"16\"><VRTRasterBand dataType=\"Float32\">{}</VRTRasterBand></VRTDataset>",
            placed_source(&path, [8, 8, 16, 16], [0, 0, 16, 16], "")
        );
        let mut vrt = open(&xml);
        assert_eq!(vrt.overviews(), vec![(8, 8), (4, 4), (2, 2)]);
        let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        for (i, n) in [(0usize, 8usize), (1, 4), (2, 2)] {
            assert_eq!(level(&mut vrt, Some(i), Window::full(n, n)), level(&mut source, Some(i), Window::new(n / 2, n / 2, n, n)));
        }
    }

    /// Levels no coarser than the VRT are dropped: a VRT at a quarter of its source's resolution
    /// has the source's overview of a quarter (as large as the VRT) and of a half (larger) as no
    /// level of its own, and the source's overview of an eighth as its overview of a half.
    #[test]
    fn a_vrt_has_no_level_as_large_as_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let xml = format!(
            "<VRTDataset rasterXSize=\"8\" rasterYSize=\"8\"><VRTRasterBand dataType=\"Float32\">{}</VRTRasterBand></VRTDataset>",
            placed_source(&path, [0, 0, 32, 32], [0, 0, 8, 8], "")
        );
        let mut vrt = open(&xml);
        assert_eq!(vrt.overviews(), vec![(4, 4)]);
        let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        assert_eq!(level(&mut vrt, Some(0), Window::full(4, 4)), level(&mut source, Some(2), Window::full(4, 4)));
    }

    /// Each level size is kept once: two overviews of the first source that round to the same
    /// size at the VRT's scale (4 and 3 pixels of a 40-pixel source, in a VRT of 30 of them, give
    /// 3 and 3) give one level.
    #[test]
    fn a_vrt_s_level_sizes_are_kept_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = leveled(dir.path(), "forty.tif", (40, 40), &[10, 14], |row, column| (row + column) as f64, None);
        let source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        assert_eq!(source.overviews(), vec![(4, 4), (3, 3)]);
        let xml = format!(
            "<VRTDataset rasterXSize=\"30\" rasterYSize=\"30\"><VRTRasterBand dataType=\"Float32\">{}</VRTRasterBand></VRTDataset>",
            simple_source(&path, "1", Some([0, 0, 30, 30]))
        );
        assert_eq!(open(&xml).overviews(), vec![(3, 3)]);
    }

    /// A source in memory whose reads give `name * 10 + level`, with the overviews it is given.
    struct Counted {
        name: usize,
        overviews: Vec<(usize, usize)>,
        reads: std::sync::Arc<std::sync::Mutex<Vec<(usize, usize, Window)>>>,
    }

    impl WindowSource for Counted {
        fn overviews(&self) -> Vec<(usize, usize)> {
            self.overviews.clone()
        }
        fn read(&mut self, level: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
            self.reads.lock().unwrap().push((self.name, level, w));
            Ok(RasterData::F64(vec![(self.name * 10 + level) as f64; w.width * w.height * bands.len()]))
        }
    }

    /// A source without the level of a VRT's overview is read from its nearest finer level: of
    /// its overviews of a half and a quarter, the quarter for the VRT's eighth. A source whose
    /// only overview is coarser than the level is read at full resolution.
    #[test]
    fn a_source_without_a_level_is_read_from_its_nearest_finer_one() {
        let reads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let grid = Grid { width: 64, height: 64, geo_transform: [0., 1., 0., 64., 0., -1.] };
        let mut sources = HashMap::new();
        let mut simple = String::new();
        for (name, overviews) in [(0, vec![(32, 32), (16, 16), (8, 8)]), (1, vec![(32, 32), (16, 16)]), (2, vec![(8, 8)])] {
            let header = crate::ops::raster_header(grid, DataType::F64, 1, None, &Default::default()).unwrap();
            let counted = Counted { name, overviews, reads: reads.clone() };
            sources.insert(format!("s{name}.tif"), LazyRaster::from_source(header, Box::new(counted)));
            let x = 64 * name;
            simple += &format!(
                "<SimpleSource><SourceFilename>s{name}.tif</SourceFilename><SourceBand>1</SourceBand><SrcRect xOff=\"0\" yOff=\"0\" xSize=\"64\" ySize=\"64\"/><DstRect xOff=\"{x}\" yOff=\"0\" xSize=\"64\" ySize=\"64\"/></SimpleSource>"
            );
        }
        let xml = format!(
            "<VRTDataset rasterXSize=\"192\" rasterYSize=\"64\"><VRTRasterBand dataType=\"Float64\">{simple}</VRTRasterBand></VRTDataset>"
        );
        let mut vrt = VirtualRaster::from_xml(&xml).unwrap().into_lazy(Path::new(""), sources).unwrap();
        assert_eq!(vrt.overviews(), vec![(96, 32), (48, 16), (24, 8)]);
        // The second source at the VRT's eighth: its quarter, not its half.
        assert_eq!(level(&mut vrt, Some(2), Window::new(8, 0, 8, 8)), vec![12.; 64]);
        assert_eq!(*reads.lock().unwrap(), vec![(1, 2, Window::new(0, 0, 16, 16))]);
        reads.lock().unwrap().clear();
        // The third source at the VRT's half: full resolution, its eighth being coarser.
        assert_eq!(level(&mut vrt, Some(0), Window::new(64, 0, 32, 32)), vec![20.; 1024]);
        assert_eq!(*reads.lock().unwrap(), vec![(2, 0, Window::new(0, 0, 64, 64))]);
    }

    /// A source read from a finer level is brought to the VRT's level by its own resampling:
    /// `average`, without an overview, gives each pixel of the VRT's overview of a half the mean of
    /// the 2 x 2 pixels it covers.
    #[test]
    fn a_source_read_from_a_finer_level_is_resampled_by_its_method() {
        let dir = tempfile::tempdir().unwrap();
        let first = pyramid(dir.path());
        let second = leveled(dir.path(), "second.tif", (32, 32), &[], |row, column| (row * 32 + column) as f64, None);
        let xml = format!(
            "<VRTDataset rasterXSize=\"64\" rasterYSize=\"32\"><VRTRasterBand dataType=\"Float32\">{}{}</VRTRasterBand></VRTDataset>",
            placed_source(&first, [0, 0, 32, 32], [0, 0, 32, 32], ""),
            placed_source(&second, [0, 0, 32, 32], [32, 0, 32, 32], " resampling=\"average\"")
        );
        let mut vrt = open(&xml);
        let got = level(&mut vrt, Some(0), Window::new(16, 0, 16, 16));
        let want: Vec<f64> = (0..256).map(|k| ((k / 16) * 64 + (k % 16) * 2) as f64 + 16.5).collect();
        assert_eq!(got, want);
    }

    /// A VRT whose first source cannot be opened has no overviews, whatever its other sources
    /// have.
    #[test]
    fn a_vrt_whose_first_source_cannot_be_opened_has_no_overviews() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let missing = dir.path().join("missing.tif");
        let xml = mosaic(64, 32, &[(&missing, 0, 0, 32, 32), (&path, 32, 0, 32, 32)], "");
        let vrt = open(&xml);
        assert_eq!(vrt.overviews(), Vec::<(usize, usize)>::new());
        std::fs::copy(&path, &missing).unwrap();
        // Failed discovery is fixed for this reader; a fresh reader retries it.
        assert_eq!(vrt.overviews(), Vec::<(usize, usize)>::new());
        assert_eq!(open(&xml).overviews(), vec![(32, 16), (16, 8), (8, 4)]);
    }

    /// A VRT lists its overviews once, opening its first source for them: they stay listed when
    /// the source's file is gone afterwards.
    #[test]
    fn a_vrt_lists_its_overviews_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let vrt = open(&mosaic(32, 32, &[(&path, 0, 0, 32, 32)], ""));
        assert_eq!(vrt.overviews(), vec![(16, 16), (8, 8), (4, 4)]);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(vrt.overviews(), vec![(16, 16), (8, 8), (4, 4)]);
    }

    /// Valid: the left half and the top quarter of 32 x 32 pixels.
    fn left_and_top() -> Vec<u8> {
        (0..32 * 32).map(|i| if i % 32 < 16 || i / 32 < 8 { 255 } else { 0 }).collect()
    }

    /// A source's own mask as the VRT's mask band (`<SourceBand>mask,1</SourceBand>`, as mosaics of
    /// masked COGs have it) is read at an overview from the source's mask at that level.
    #[test]
    fn a_source_s_own_mask_is_read_at_an_overview() {
        let dir = tempfile::tempdir().unwrap();
        let path = leveled(dir.path(), "masked.tif", (32, 32), &[2, 4], |row, column| (row * 32 + column) as f64, Some(left_and_top()));
        let xml = format!(
            "<VRTDataset rasterXSize=\"32\" rasterYSize=\"32\"><VRTRasterBand dataType=\"Float32\">{}</VRTRasterBand><MaskBand><VRTRasterBand dataType=\"Byte\">{}</VRTRasterBand></MaskBand></VRTDataset>",
            simple_source(&path, "1", None),
            simple_source(&path, "mask,1", None)
        );
        let mut vrt = open(&xml);
        assert_eq!(vrt.overviews(), vec![(16, 16), (8, 8)]);
        let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        for (i, (w, h)) in source.overviews().into_iter().enumerate() {
            let want = source.read_mask(Window::full(w, h), Some(i)).unwrap();
            assert!(want.as_ref().is_some_and(|m| m.contains(&0) && m.contains(&255)), "overview {i}: the source's mask");
            assert_eq!(vrt.read_mask(Window::full(w, h), Some(i)).unwrap(), want, "overview {i}");
        }
    }

    /// A source composited through its own mask (`<UseMaskBand>true</UseMaskBand>`) over another is
    /// composited at an overview by its mask at that level: its overview pixels where they are
    /// valid, the other source's elsewhere.
    #[test]
    fn a_source_used_through_its_mask_is_composited_by_it_at_an_overview() {
        let dir = tempfile::tempdir().unwrap();
        let background = leveled(dir.path(), "background.tif", (32, 32), &[2, 4], |_, _| 7., None);
        let path = leveled(dir.path(), "masked.tif", (32, 32), &[2, 4], |row, column| (row * 32 + column) as f64, Some(left_and_top()));
        let xml = format!(
            "<VRTDataset rasterXSize=\"32\" rasterYSize=\"32\"><VRTRasterBand dataType=\"Float32\">{}<ComplexSource><SourceFilename relativeToVRT=\"0\">{}</SourceFilename><SourceBand>1</SourceBand><UseMaskBand>true</UseMaskBand></ComplexSource></VRTRasterBand></VRTDataset>",
            simple_source(&background, "1", None),
            path.display()
        );
        let mut vrt = open(&xml);
        assert_eq!(vrt.overviews(), vec![(16, 16), (8, 8)]);
        let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        for (i, (w, h)) in source.overviews().into_iter().enumerate() {
            let mask = source.read_mask(Window::full(w, h), Some(i)).unwrap().unwrap();
            assert!(mask.contains(&0) && mask.contains(&255), "overview {i}: the source's mask");
            let pixels = level(&mut source, Some(i), Window::full(w, h));
            let want: Vec<f64> = pixels.iter().zip(&mask).map(|(&v, &m)| if m != 0 { v } else { 7. }).collect();
            assert_eq!(level(&mut vrt, Some(i), Window::full(w, h)), want, "overview {i}");
        }
    }

    /// A warped VRT is read through its warp, whose reads have no levels: it has no overviews,
    /// whatever its source has, even when one of its bands also lists a source, which its reads
    /// ignore.
    #[test]
    fn a_warped_vrt_has_no_overviews() {
        let dir = tempfile::tempdir().unwrap();
        let path = pyramid(dir.path());
        let xml = VirtualRaster::warped(path.to_str().unwrap(), &WarpOptions::default()).unwrap().to_xml();
        let stray = xml.replacen("</VRTRasterBand>", &format!("{}</VRTRasterBand>", simple_source(&path, "1", None)), 1);
        assert_ne!(stray, xml);
        for xml in [xml, stray] {
            let mut vrt = open(&xml);
            assert_eq!(vrt.overviews(), Vec::<(usize, usize)>::new());
            assert!(vrt.read_window(Window::new(0, 0, 1, 1), &WindowOptions { bands: vec![1], overview: Some(0) }).is_err());
            let warped = vrt.read_window(Window::full(32, 32), &WindowOptions { bands: vec![1], overview: None }).unwrap().raster.data;
            let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
            assert_eq!(warped.to_f64_vec(), level(&mut source, None, Window::full(32, 32)));
        }
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
                levels: Default::default(),
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
                        levels: Default::default(),
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
                levels: Default::default(),
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
            levels: Default::default(),
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
            levels: Default::default(),
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

    /// A mosaic with a source that cannot be opened (a remote source that is down): as GDAL's
    /// `VRTSimpleSource::GetSrcDstWindow()` leaves a source unopened when a request misses its
    /// destination rectangle, windows over the other source read its values, up to the edge of
    /// the missing one, on whichever side it lies; a window reaching the missing source fails to
    /// open it.
    #[test]
    fn windows_missing_an_unopenable_source_read_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let rects = |(x, y): (usize, usize)| {
            format!("<SrcRect xOff=\"0\" yOff=\"0\" xSize=\"5\" ySize=\"5\"/><DstRect xOff=\"{x}\" yOff=\"{y}\" xSize=\"5\" ySize=\"5\"/>")
        };
        let read = |lazy: &mut LazyRaster, w: Window| lazy.read_window(w, &WindowOptions::default()).map(|d| d.raster.data.to_f64_vec());
        // The readable source and the missing one side by side: the missing one right, left, below, above.
        for ((width, height), at, missing_at, across) in [
            ((10, 5), (0, 0), (5, 0), Window::new(4, 0, 2, 1)),
            ((10, 5), (5, 0), (0, 0), Window::new(4, 0, 2, 1)),
            ((5, 10), (0, 0), (0, 5), Window::new(0, 4, 1, 2)),
            ((5, 10), (0, 5), (0, 0), Window::new(0, 4, 1, 2)),
        ] {
            let missing = format!(
                "<SimpleSource><SourceFilename relativeToVRT=\"1\">missing.tif</SourceFilename><SourceBand>1</SourceBand>{}</SimpleSource>",
                rects(missing_at)
            );
            let text = format!(
                "<VRTDataset rasterXSize=\"{width}\" rasterYSize=\"{height}\"><GeoTransform>0,1,0,10,0,-1</GeoTransform>\
                 <VRTRasterBand dataType=\"Float64\"><NoDataValue>-1</NoDataValue>{}{missing}</VRTRasterBand></VRTDataset>",
                source("SimpleSource", &rects(at)),
            );
            let mut lazy = VirtualRaster::from_xml(&text).unwrap().into_lazy(dir.path(), sources()).unwrap();
            let (x, y) = at;
            assert_eq!(read(&mut lazy, Window::new(x, y, 5, 5)).unwrap(), (0..25).map(f64::from).collect::<Vec<_>>(), "{at:?}");
            assert_eq!(read(&mut lazy, Window::new(x + 1, y + 2, 3, 2)).unwrap(), vec![11., 12., 13., 16., 17., 18.], "{at:?}");
            let (mx, my) = missing_at;
            for w in [across, Window::new(mx, my, 1, 1), Window::new(0, 0, width, height)] {
                let error = read(&mut lazy, w).unwrap_err();
                assert!(matches!(&error, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound), "{at:?} {w:?}: {error}");
            }
        }
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
        // A copy reads the window alone, without a margin.
        assert_eq!(reads.load(Ordering::Relaxed), 100);
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
                // Preserve the catalog's validity too: one pass for pixels, one for its mask.
                assert_eq!(reads.load(Ordering::Relaxed), 2);
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
        // The second pass reads the same URLs: terrio's process-wide cache of remote files serves
        // them, as GDAL's `/vsicurl/` cache does. Held, it evicts nothing meanwhile.
        let _cache = crate::http::cache_test_guard();
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
                // Every document is in the cache: no request.
                assert_eq!(requests.lock().unwrap().len(), 4);
            }
            Ok(())
        };
        let result = run();
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
        result.unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests.iter().all(|(path, _)| matches!(path.as_str(), "/root.vrt" | "/a.vrt" | "/b.vrt" | "/nested.vrt")));
        assert_eq!(requests.iter().map(|(_, bytes)| bytes).sum::<usize>(), expected_bytes);
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
