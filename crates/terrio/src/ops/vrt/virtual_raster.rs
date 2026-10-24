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
}

impl VirtualRaster {
    /// Parse a serialized VRT without opening its sources or executing embedded code.
    pub fn from_xml(text: &str) -> Result<Self> {
        let root = xml::parse(text)?;
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
        Ok(Self { root, layout, warped })
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
        let subdataset =
            ["NETCDF:", "HDF5:", "ZARR:", "GPKG:"].iter().any(|p| source.get(..p.len()).is_some_and(|v| v.eq_ignore_ascii_case(p)));
        let source = if subdataset {
            resolve_reference(&std::env::current_dir()?.join("__vrt__.vrt").to_string_lossy(), source, false)
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
        let srcgt = options.src_transform.unwrap_or_else(|| header.geo_transform());
        let dstgt = target.geo_transform();
        let mut bands = String::new();
        let mut mappings = String::new();
        for band in 1..=header.raster.bands {
            let dest_nodata = match &options.dst_nodata {
                Nodata::Auto => match &options.src_nodata {
                    Nodata::Auto => header.band_nodata(band),
                    Nodata::None => None,
                    v => v.value(band - 1),
                },
                Nodata::None => None,
                v => v.value(band - 1),
            };
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
        let georeferencing = if let Some(rpc) = &options.rpc {
            serialize_rpc(rpc, &options.rpc_options)?
        } else if options.homography.is_some() || !options.gcps.is_empty() {
            serialize_gcps(options)?
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
            Resampling::CubicSpline => "CubicSpline",
            v => v.name(),
        };
        Self::from_xml(&format!(
            "<VRTDataset rasterXSize=\"{}\" rasterYSize=\"{}\" subClass=\"VRTWarpedDataset\"><SRS>{}</SRS><GeoTransform>{}</GeoTransform>{bands}<BlockXSize>512</BlockXSize><BlockYSize>128</BlockYSize><GDALWarpOptions><WarpMemoryLimit>{}</WarpMemoryLimit><ResampleAlg>{alg}</ResampleAlg><WorkingDataType>{}</WorkingDataType><Option name=\"INIT_DEST\">NO_DATA</Option><Option name=\"UNIFIED_SRC_NODATA\">{unified_option}</Option><SourceDataset relativeToVRT=\"0\">{}</SourceDataset><Transformer>{transformer}</Transformer><BandList>{mappings}</BandList></GDALWarpOptions></VRTDataset>",
            target.raster.cols,
            target.raster.rows,
            xml::escape(&srs(&dst_srs)),
            gt(dstgt),
            options.memory_limit,
            crate::ops::data_type_name(target.raster.data_type),
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
        let grid = Grid {
            width: self.layout.width,
            height: self.layout.height,
            geo_transform: self.layout.geo_transform.unwrap_or([0., 1., 0., 0., 0., -1.]),
        };
        let first = &self.layout.bands[0];
        let mut header = crate::ops::raster_header(
            grid,
            first.data_type,
            self.layout.bands.len(),
            first.nodata,
            &self.layout.srs.clone().unwrap_or_default(),
        )?;
        header.driver = crate::formats::RasterDriver::VRT;
        header.band_nodata = self.layout.bands.iter().map(|b| b.nodata).collect();
        if let Some(metadata) = self.root.child("Metadata") {
            header.raster.metadata =
                metadata.children.iter().filter_map(|e| e.attr("key").map(|k| (k.to_string(), e.text.clone()))).collect();
        }
        Ok(LazyRaster::from_source(
            header,
            Box::new(Reader { description: self, base: base.into(), supplied: sources.keys().cloned().collect(), sources }),
        ))
    }
}

/// Open a local, archived or remote VRT and resolve relative sources from its location.
pub fn open_virtual_raster(path: &str) -> Result<LazyRaster> {
    let local = crate::vfs::resolve(path)?;
    let text = std::fs::read_to_string(local.path())?;
    VirtualRaster::from_xml(&text)?.into_lazy_reference(path, HashMap::new())
}

struct ParsedWarp {
    source: String,
    relative: bool,
    options: WarpOptions,
    pairs: Vec<(usize, usize)>,
    source_nodata: Vec<Option<f64>>,
    destination_nodata: Vec<Option<f64>>,
}

struct Reader {
    description: VirtualRaster,
    base: String,
    sources: HashMap<String, LazyRaster>,
    supplied: std::collections::HashSet<String>,
}

impl Reader {
    fn source(&mut self, name: &str, relative: bool) -> Result<&mut LazyRaster> {
        if !self.sources.contains_key(name) {
            // Bound the cache; callers can supply arbitrary numbers of virtual sources.
            if self.sources.len().saturating_sub(self.supplied.len()) >= 32
                && let Some(key) = self.sources.keys().find(|k| !self.supplied.contains(*k)).cloned()
            {
                self.sources.remove(&key);
            }
            let path = resolve_reference(&self.base, name, relative);
            self.sources.insert(name.into(), crate::open_raster_lazy(&path)?);
        }
        Ok(self.sources.get_mut(name).expect("source inserted"))
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
        let parsed = self.parse_warp()?;
        let gt = self.description.layout.geo_transform.ok_or_else(|| invalid("missing warped geotransform"))?;
        let data_type = self.description.layout.bands[0].data_type;
        let mut options = parsed.options;
        let count = self.description.layout.bands.len();
        let source = self.source(&parsed.source, parsed.relative)?;
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
        options.src_nodata = Nodata::PerBand(nodata);
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
                let result = crate::ops::warp::warp_lazy_bands(source, &selection, &options)?;
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
        Ok(RasterData::from_f64_vec(data_type, output))
    }

    fn parse_warp(&self) -> Result<ParsedWarp> {
        let window = Window::full(self.description.layout.width, self.description.layout.height);
        let warp = self.description.root.child("GDALWarpOptions").ok_or_else(|| invalid("missing GDALWarpOptions"))?;
        let source = warp.child("SourceDataset").ok_or_else(|| invalid("missing SourceDataset"))?;
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
            resampling: warp.child("ResampleAlg").map(|e| resampling(&e.text)).transpose()?.unwrap_or(Resampling::Nearest),
            output_type: Some(self.description.layout.bands[0].data_type),
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
            relative: source.attr("relativeToVRT") == Some("1"),
            options,
            pairs,
            source_nodata,
            destination_nodata,
        })
    }
}

impl WindowSource for Reader {
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
        let Some(mask) = self.description.root.child("MaskBand").and_then(|e| e.child("VRTRasterBand")) else {
            return Ok(None);
        };
        let mut root = self.description.root.clone();
        root.attrs.retain(|(k, _)| k != "subClass");
        root.children.retain(|e| e.name != "MaskBand" && e.name != "VRTRasterBand" && e.name != "GDALWarpOptions");
        root.children.push(mask.clone());
        let mask = VirtualRaster::from_xml(&root.to_xml())?;
        let description = std::mem::replace(&mut self.description, mask);
        let result = self.read(0, window, &[1]);
        self.description = description;
        result.map(|v| Some(v.to_f64_vec().into_iter().map(|v| if v == 0. { 0 } else { 255 }).collect()))
    }
    fn read(&mut self, _level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
        let _depth = ReadDepth::enter()?;
        if self.description.warped {
            return self.warped_blocks(window, bands);
        }
        let mut values = Vec::with_capacity(window.width * window.height * bands.len());
        let elements = self.description.root.children.iter().filter(|e| e.name == "VRTRasterBand").cloned().collect::<Vec<_>>();
        for b in bands {
            let layout = self.description.layout.bands[*b - 1].clone();
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
            values.extend(out);
        }
        let data_type = self.description.layout.bands[0].data_type;
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

fn resolve_reference(base: &str, name: &str, relative: bool) -> String {
    // GDAL subdataset names embed a filename; resolve that filename rather than
    // treating the complete driver-prefixed selector as a relative path.
    for prefix in ["NETCDF:", "HDF5:", "ZARR:", "GPKG:"] {
        if name.get(..prefix.len()).is_some_and(|v| v.eq_ignore_ascii_case(prefix)) {
            let rest = &name[prefix.len()..];
            let parts = if let Some(quoted) = rest.strip_prefix('"') { quoted.split_once("\":") } else { rest.rsplit_once(':') };
            if let Some((file, variable)) = parts {
                let file = resolve_reference(base, file, relative);
                return format!("{prefix}\"{file}\":{variable}");
            }
        }
    }
    if relative || (crate::vfs::Location::parse(name).is_local() && Path::new(name).is_relative()) {
        crate::stac::resolve_href(base, name)
    } else {
        name.to_string()
    }
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
    Resampling::parse(match name.trim() {
        "NearestNeighbour" => "near",
        "CubicSpline" => "cubicspline",
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
// Arithmetic and axis ordering follow GDAL's VRTKernelFilteredSource::FilterData
// (frmts/vrt/vrtfilters.cpp, MIT; attribution in NOTICE).
fn filter(
    input: &[f64],
    width: usize,
    height: usize,
    kernel: &xml::Element,
    nodata: Option<f64>,
    function: Option<&xml::Element>,
) -> Result<Vec<f64>> {
    let size = kernel_size(kernel)?;
    let coefs = coefficients(kernel)?;
    let radius = size / 2;
    let normalized = kernel.attr("normalized").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let separable = coefs.len() == size;
    let mut input = input.iter().map(|v| *v as f32).collect::<Vec<_>>();
    let nd = nodata.map(|v| v as f32);
    for axis in 0..if separable { 2 } else { 1 } {
        let mut output = vec![0f32; input.len()];
        for y in 0..height {
            for x in 0..width {
                let center = input[y * width + x];
                if nd == Some(center) {
                    output[y * width + x] = center;
                    continue;
                }
                let (mut sum, mut weight) = (0., 0.);
                let mut neighbors = Vec::new();
                for ky in 0..if separable && axis == 1 { 1 } else { size } {
                    for kx in 0..if separable && axis == 0 { 1 } else { size } {
                        let c = if separable { coefs[if axis == 0 { ky } else { kx }] } else { coefs[ky * size + kx] };
                        if c == 0. {
                            continue;
                        }
                        let yy = if separable && axis == 1 {
                            y
                        } else {
                            (y as isize + ky as isize - radius as isize).clamp(0, height as isize - 1) as usize
                        };
                        let xx = if separable && axis == 0 {
                            x
                        } else {
                            (x as isize + kx as isize - radius as isize).clamp(0, width as isize - 1) as usize
                        };
                        let value = input[yy * width + xx];
                        if nd.is_some() && (nd == Some(value) || value.is_nan()) {
                            continue;
                        }
                        let value = f64::from(value) * c;
                        sum += value;
                        weight += c;
                        neighbors.push(value);
                    }
                }
                let empty = nodata.unwrap_or(0.);
                let value = if let Some(f) = function {
                    neighborhood(f.text.trim(), &mut neighbors, empty)
                } else if !normalized {
                    sum
                } else if neighbors.is_empty() || weight == 0. {
                    empty
                } else {
                    sum / weight
                };
                output[y * width + x] = value as f32;
            }
        }
        input = output;
    }
    Ok(input.into_iter().map(f64::from).collect())
}
fn neighborhood(f: &str, values: &mut [f64], empty: f64) -> f64 {
    if values.is_empty() {
        return empty;
    }
    match f {
        "min" => values.iter().copied().fold(f64::MAX, f64::min),
        "max" => values.iter().copied().fold(-f64::MAX, f64::max),
        "median" => {
            values.sort_by(f64::total_cmp);
            let n = values.len();
            if n % 2 == 1 { values[n / 2] } else { (values[n / 2 - 1] + values[n / 2]) / 2. }
        }
        "stddev" => {
            let (mut mean, mut m2) = (0., 0.);
            for (i, v) in values.iter().enumerate() {
                let delta = v - mean;
                mean += delta / (i + 1) as f64;
                m2 += delta * (v - mean);
            }
            (m2 / values.len() as f64).sqrt()
        }
        "mode" => {
            let (mut best, mut count) = (values[0], 0);
            for v in values.iter() {
                let n = values.iter().filter(|w| *w == v).count();
                if n > count {
                    best = *v;
                    count = n;
                }
            }
            best
        }
        _ => empty,
    }
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
    }
}
