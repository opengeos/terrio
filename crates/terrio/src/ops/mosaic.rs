//! Mosaics: an equivalent of `gdalbuildvrt` (a [`VrtDataset`] over many rasters) and of
//! `gdal_merge.py` (the same, read into one raster). Sources in different CRSs are reprojected
//! with [`warp`].

use super::vrt::{VrtBand, VrtDataset, VrtSource};
use super::warp::{Nodata, WarpOptions, warp};
use super::{Grid, Resampling};
use crate::crs::SpatialRef;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::raster::RasterData;

/// How the mosaic's resolution is chosen from the sources' (`gdalbuildvrt -resolution`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ResolutionStrategy {
    /// The average of the sources' resolutions.
    #[default]
    Average,
    /// The finest resolution.
    Highest,
    /// The coarsest resolution.
    Lowest,
    /// A given resolution `(x, y)`, both positive.
    User(f64, f64),
}

impl ResolutionStrategy {
    /// Parses `highest`, `lowest` or `average`.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "average" => Ok(Self::Average),
            "highest" => Ok(Self::Highest),
            "lowest" => Ok(Self::Lowest),
            other => Err(Error::InvalidArgument(format!("unknown resolution strategy '{other}' (highest, lowest, average)"))),
        }
    }
}

/// Options for [`build_vrt`] and [`mosaic`].
#[derive(Debug, Clone, Default)]
pub struct MosaicOptions {
    /// The output resolution.
    pub resolution: ResolutionStrategy,
    /// The output extent `(min_x, min_y, max_x, max_y)`; the union of the sources' by default.
    pub extent: Option<(f64, f64, f64, f64)>,
    /// Align the extent on multiples of the resolution (`-tap`).
    pub target_aligned_pixels: bool,
    /// Put each source in its own band instead of mosaicking them (`-separate`).
    pub separate: bool,
    /// The source bands to use, from 1 (`-b`). All by default (the first with `separate`).
    pub bands: Vec<usize>,
    /// Source nodata: pixels with this value are not drawn (`-srcnodata`).
    pub src_nodata: Nodata,
    /// The output nodata (`-vrtnodata`). By default, the source nodata.
    pub dst_nodata: Nodata,
    /// Resampling, when sources are resampled or reprojected.
    pub resampling: Resampling,
    /// Reproject into this CRS. By default, the first source's; sources in other CRSs are
    /// reprojected into it.
    pub dst_srs: Option<SpatialRef>,
}

/// A source of a mosaic: its path (as it will be written in a VRT) and its contents.
pub struct MosaicSource<'a> {
    /// The path.
    pub path: String,
    /// The raster.
    pub ds: &'a RasterDataset,
}

/// Builds a virtual mosaic of rasters in the same CRS, as `gdalbuildvrt` does. Paths are written
/// relative to `vrt_dir` when they are inside it.
pub fn build_vrt(sources: &[MosaicSource], o: &MosaicOptions, vrt_dir: Option<&Path>) -> Result<VrtDataset> {
    let first = sources.first().ok_or_else(|| Error::InvalidArgument("no source rasters".into()))?;
    let srs = first.ds.crs();
    for s in sources {
        if !s.ds.crs().is_unknown() && !srs.is_unknown() && !s.ds.crs().same_as(&srs) {
            return Err(Error::InvalidArgument(format!(
                "'{}' is in another CRS ({}, not {}): reproject it, or write a raster instead of a VRT",
                s.path,
                s.ds.crs(),
                srs
            )));
        }
        let gt = s.ds.geo_transform();
        if gt[2] != 0.0 || gt[4] != 0.0 || gt[5] >= 0.0 {
            return Err(Error::Unsupported(format!("'{}' is not north-up", s.path)));
        }
    }
    // Resolution and extent.
    let (mut we, mut ns) = (0.0f64, 0.0f64);
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (i, s) in sources.iter().enumerate() {
        let gt = s.ds.geo_transform();
        let (x0, y0, x1, y1) = Grid::of(&s.ds.raster).bounds();
        min_x = min_x.min(x0);
        min_y = min_y.min(y0);
        max_x = max_x.max(x1);
        max_y = max_y.max(y1);
        match o.resolution {
            ResolutionStrategy::Average => {
                let n = (i + 1) as f64;
                we += (gt[1] - we) / n;
                ns += (gt[5] - ns) / n;
            }
            ResolutionStrategy::Highest => {
                if i == 0 {
                    (we, ns) = (gt[1], gt[5]);
                } else {
                    we = we.min(gt[1]);
                    ns = ns.max(gt[5]);
                }
            }
            ResolutionStrategy::Lowest => {
                if i == 0 {
                    (we, ns) = (gt[1], gt[5]);
                } else {
                    we = we.max(gt[1]);
                    ns = ns.min(gt[5]);
                }
            }
            ResolutionStrategy::User(x, y) => (we, ns) = (x, -y),
        }
    }
    if let Some(e) = o.extent {
        (min_x, min_y, max_x, max_y) = e;
    }
    if o.target_aligned_pixels {
        min_x = (min_x / we).floor() * we;
        max_x = (max_x / we).ceil() * we;
        min_y = (min_y / -ns).floor() * -ns;
        max_y = (max_y / -ns).ceil() * -ns;
    }
    let width = (0.5 + (max_x - min_x) / we) as usize;
    let height = (0.5 + (max_y - min_y) / -ns) as usize;
    if width == 0 || height == 0 {
        return Err(Error::InvalidArgument("the mosaic would be empty".into()));
    }

    let band_list = |ds: &RasterDataset| -> Vec<usize> {
        if !o.bands.is_empty() {
            o.bands.clone()
        } else if o.separate {
            vec![1]
        } else {
            (1..=ds.raster.bands).collect()
        }
    };
    let nbands = band_list(first.ds).len();
    if !o.separate && sources.iter().any(|s| band_list(s.ds).iter().any(|b| *b > s.ds.raster.bands)) {
        return Err(Error::InvalidArgument("a source lacks one of the requested bands".into()));
    }
    let src_nodata = |s: &MosaicSource, b: usize| match &o.src_nodata {
        Nodata::Auto => s.ds.nodata(),
        Nodata::None => None,
        Nodata::Values(v) => v.get(b).or(v.last()).copied(),
    };
    let vrt_nodata = |b: usize| match &o.dst_nodata {
        Nodata::Auto => sources.iter().find_map(|s| src_nodata(s, b)),
        Nodata::None => None,
        Nodata::Values(v) => v.get(b).or(v.last()).copied(),
    };
    let mut bands: Vec<VrtBand> = Vec::new();
    if !o.separate {
        for b in 0..nbands {
            bands.push(VrtBand { data_type: first.ds.raster.data_type, nodata: vrt_nodata(b), color_interp: None, sources: Vec::new() });
        }
    }
    for (i, s) in sources.iter().enumerate() {
        let Some((src_rect, dst_rect)) = src_dst_window(s.ds, we, ns, (min_x, min_y, max_x, max_y), width, height) else { continue };
        let (path, relative) = source_path(&s.path, vrt_dir);
        for (k, &b) in band_list(s.ds).iter().enumerate() {
            if b == 0 || b > s.ds.raster.bands {
                return Err(Error::InvalidArgument(format!("'{}' has no band {b}", s.path)));
            }
            let target = if o.separate { i } else { k };
            let nd = src_nodata(s, if o.separate { i } else { k });
            let src = VrtSource {
                filename: path.clone(),
                relative_to_vrt: relative,
                band: b,
                src_rect,
                dst_rect,
                complex: nd.is_some(),
                nodata: nd,
                scale: None,
                lut: None,
                resampling: o.resampling,
                properties: Some((s.ds.raster.cols, s.ds.raster.rows, s.ds.raster.data_type)),
            };
            if o.separate {
                bands.push(VrtBand { data_type: s.ds.raster.data_type, nodata: vrt_nodata(i), color_interp: None, sources: vec![src] });
            } else {
                bands[target].sources.push(src);
            }
        }
    }
    let srs = (!srs.is_unknown()).then_some(srs);
    Ok(VrtDataset { width, height, geo_transform: Some([min_x, we, 0.0, max_y, 0.0, ns]), srs, bands })
}

/// `gdalbuildvrt`'s `GetSrcDstWin()`: where a source falls in the mosaic.
fn src_dst_window(ds: &RasterDataset, we: f64, ns: f64, ext: (f64, f64, f64, f64), tw: usize, th: usize) -> Option<([f64; 4], [f64; 4])> {
    let gt = ds.geo_transform();
    let (w, h) = (ds.raster.cols as f64, ds.raster.rows as f64);
    let (min_x, min_y, max_x, max_y) = ext;
    if gt[0] + w * gt[1] <= min_x || gt[0] >= max_x || gt[3] + h * gt[5] >= max_y || gt[3] <= min_y {
        return None;
    }
    let (sxo, dxo) = if gt[0] < min_x { ((min_x - gt[0]) / gt[1], 0.0) } else { (0.0, (gt[0] - min_x) / we) };
    let (syo, dyo) = if max_y < gt[3] { ((gt[3] - max_y) / -gt[5], 0.0) } else { (0.0, (max_y - gt[3]) / -ns) };
    let mut sxs = w - sxo.max(0.0);
    let mut sys = h - syo.max(0.0);
    let (rx, ry) = (gt[1] / we, gt[5] / ns);
    let mut dxs = sxs * rx;
    let mut dys = sys * ry;
    if dxo + dxs > tw as f64 {
        dxs = tw as f64 - dxo;
        sxs = dxs / rx;
    }
    if dyo + dys > th as f64 {
        dys = th as f64 - dyo;
        sys = dys / ry;
    }
    (sxs > 0.0 && dxs > 0.0 && sys > 0.0 && dys > 0.0).then_some(([sxo, syo, sxs, sys], [dxo, dyo, dxs, dys]))
}

/// The path to write in a VRT, relative to the VRT's directory when inside it.
pub(crate) fn source_path(path: &str, vrt_dir: Option<&Path>) -> (String, bool) {
    let Some(dir) = vrt_dir else { return (absolute(path), false) };
    let abs = absolute(path);
    let dir_abs = absolute(&dir.to_string_lossy());
    match Path::new(&abs).strip_prefix(&dir_abs) {
        Ok(rel) if !dir_abs.is_empty() => (rel.to_string_lossy().replace('\\', "/"), true),
        _ => (abs, false),
    }
}

fn absolute(path: &str) -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if let Ok(p) = std::path::absolute(if path.is_empty() { "." } else { path }) {
        return p.to_string_lossy().into_owned();
    }
    path.to_string()
}

/// Mosaics rasters into one, as `gdal_merge.py` (or `gdalbuildvrt` followed by
/// `gdal_translate`) does. When a source's CRS differs from the output's, all sources are
/// reprojected with [`warp`] instead.
pub fn mosaic(sources: &[MosaicSource], o: &MosaicOptions) -> Result<RasterDataset> {
    let first = sources.first().ok_or_else(|| Error::InvalidArgument("no source rasters".into()))?;
    let dst = o.dst_srs.clone().unwrap_or_else(|| first.ds.crs());
    if reprojected(sources, o) {
        if o.separate {
            return Err(Error::Unsupported("--separate with sources in different CRSs is not supported".into()));
        }
        let wo = WarpOptions {
            dst_srs: Some(dst),
            resolution: match o.resolution {
                ResolutionStrategy::User(x, y) => Some((x, y)),
                _ => None,
            },
            extent: o.extent,
            target_aligned_pixels: o.target_aligned_pixels,
            resampling: o.resampling,
            src_nodata: o.src_nodata.clone(),
            dst_nodata: o.dst_nodata.clone(),
            ..Default::default()
        };
        let refs: Vec<&RasterDataset> = sources.iter().map(|s| s.ds).collect();
        return warp(&refs, &wo);
    }
    let vrt = build_vrt(sources, o, None)?;
    // Read from the datasets already in memory rather than reopening the files.
    vrt_from_memory(&vrt, sources)
}

/// Whether the sources are reprojected: some are in another CRS than the output's.
fn reprojected(sources: &[MosaicSource], o: &MosaicOptions) -> bool {
    let Some(first) = sources.first() else { return false };
    let dst = o.dst_srs.clone().unwrap_or_else(|| first.ds.crs());
    sources.iter().any(|s| {
        let c = s.ds.crs();
        !c.is_unknown() && !dst.is_unknown() && !c.same_as(&dst)
    })
}

/// Mosaics rasters as [`mosaic`] does and writes the mosaic to `path`. When every source is
/// copied pixel for pixel (same CRS and resolution, whole-pixel offsets), they are read by windows
/// and a GeoTIFF output written a strip of rows at a time: neither the sources nor the output are
/// held whole. Otherwise the sources are read whole.
///
/// ```no_run
/// use terrio::ops::mosaic::{MosaicOptions, mosaic_to_path};
///
/// let tiles = ["n39w105.tif".to_string(), "n39w106.tif".to_string()];
/// mosaic_to_path(&tiles, &MosaicOptions::default(), "mosaic.tif", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn mosaic_to_path(paths: &[String], o: &MosaicOptions, path: &str, options: &crate::RasterWriteOptions) -> Result<()> {
    let mut lazy = paths.iter().map(|p| crate::open_raster_lazy(p)).collect::<Result<Vec<_>>>()?;
    let headers: Vec<RasterDataset> = lazy.iter().map(|l| l.header()).collect();
    let sources: Vec<MosaicSource> = paths.iter().zip(&headers).map(|(p, ds)| MosaicSource { path: p.clone(), ds }).collect();
    let plan = if reprojected(&sources, o) { None } else { copy_plan(&sources, o)? };
    let Some((vrt, copies)) = plan else {
        drop(lazy);
        let full = paths.iter().map(|p| crate::open_raster(p)).collect::<Result<Vec<_>>>()?;
        let sources: Vec<MosaicSource> = paths.iter().zip(&full).map(|(p, ds)| MosaicSource { path: p.clone(), ds }).collect();
        return crate::write_raster_dataset(&mosaic(&sources, o)?, path, options);
    };
    let rows = super::strips::strip_rows(vrt.width, vrt.bands.len(), vrt.bands[0].data_type.size_bytes(), None);
    copy_in_strips(&vrt, &copies, &mut lazy, path, options, rows)
}

/// Writes a mosaic whose sources are all copied pixel for pixel, in strips of `rows` rows.
fn copy_in_strips(
    vrt: &VrtDataset,
    copies: &[Copy],
    lazy: &mut [crate::LazyRaster],
    path: &str,
    options: &crate::RasterWriteOptions,
    rows: usize,
) -> Result<()> {
    let (w, h, nbands) = (vrt.width, vrt.height, vrt.bands.len());
    let data_type = vrt.bands[0].data_type;
    let grid = Grid { width: w, height: h, geo_transform: vrt.geo_transform.unwrap_or([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]) };
    let header = super::raster_header(grid, data_type, nbands, vrt.bands[0].nodata, &vrt.srs.clone().unwrap_or_default())?;
    let mut out = super::strips::StripWriter::create(&header, path, options)?;
    let span = |i: usize| (i * rows, ((i + 1) * rows).min(h));
    super::strips::pipelined(
        h.div_ceil(rows),
        // Each copy's rows in the strip, read from its source.
        |i| {
            let (y0, y1) = span(i);
            let mut reads = Vec::new();
            for (k, c) in copies.iter().enumerate() {
                let (r0, r1) = (c.copy.dst_rows.start.max(y0), c.copy.dst_rows.end.min(y1));
                if r0 >= r1 {
                    continue;
                }
                let window = crate::Window::new(c.copy.src.0, c.copy.src.1 + r0 - c.copy.dst_rows.start, c.copy.cols, r1 - r0);
                let read = crate::WindowOptions { bands: vec![c.band], ..Default::default() };
                reads.push((k, r0, lazy[c.source].read_window(window, &read)?));
            }
            Ok(reads)
        },
        |i, reads| {
            let (y0, y1) = span(i);
            let n = w * (y1 - y0);
            let mut data = RasterData::new_filled(data_type, n * nbands, 0.0);
            for (b, band) in vrt.bands.iter().enumerate() {
                crate::pixel::with_data!(&mut data, v => {
                    let fill = super::convert_value(band.nodata.unwrap_or(0.0), data_type);
                    v[b * n..(b + 1) * n].iter_mut().for_each(|x| *x = fill as _);
                });
            }
            // In the sources' order: later ones on top.
            for (k, r0, ds) in &reads {
                let c = &copies[*k];
                let values = super::BandView::of(&ds.raster, 0);
                let cols = c.copy.cols;
                crate::pixel::with_data!(&mut data, v => {
                    let band = &mut v[c.out_band * n..(c.out_band + 1) * n];
                    let rows = &mut band[(r0 - y0) * w..(r0 - y0 + ds.raster.rows) * w];
                    crate::par::chunks_mut(rows, w, |r, line| {
                        for (x, o) in line[c.copy.dst_x..c.copy.dst_x + cols].iter_mut().enumerate() {
                            let value = values.get(r * cols + x);
                            if c.nodata.is_none_or(|nd| !super::is_nodata(value, nd)) {
                                *o = super::convert_value(value, data_type) as _;
                            }
                        }
                    });
                });
            }
            out.write(data, y1 - y0)
        },
    )?;
    out.finish()
}

/// A source band copied into a band of the mosaic.
struct Copy {
    /// The source, by index.
    source: usize,
    /// Its band, from 1.
    band: usize,
    /// The mosaic's band, from 0.
    out_band: usize,
    /// Its pixels with this value are not copied.
    nodata: Option<f64>,
    copy: super::vrt::PlainCopy,
}

/// The mosaic's VRT, and the copies that fill it in drawing order; `None` when a source is
/// resampled.
fn copy_plan(sources: &[MosaicSource], o: &MosaicOptions) -> Result<Option<(VrtDataset, Vec<Copy>)>> {
    let vrt = build_vrt(sources, o, None)?;
    let mut copies = Vec::new();
    for (b, band) in vrt.bands.iter().enumerate() {
        for src in &band.sources {
            let source = sources
                .iter()
                .position(|s| absolute(&s.path) == src.filename)
                .ok_or_else(|| Error::InvalidData(format!("unknown source '{}'", src.filename)))?;
            let ds = sources[source].ds;
            let read = super::vrt::SourceRead {
                values: super::BandView::F64(&[]),
                valid: None,
                width: ds.raster.cols,
                height: ds.raster.rows,
                src_type: ds.raster.data_type,
                src_rect: src.src_rect,
                dst_rect: src.dst_rect,
                resampling: src.resampling,
                working_float: src.complex,
                invalid_value: None,
            };
            match read.plain_copy(vrt.width, vrt.height) {
                Ok(Some(copy)) => copies.push(Copy { source, band: src.band, out_band: b, nodata: src.nodata, copy }),
                Ok(None) => {}
                Err(()) => return Ok(None),
            }
        }
    }
    Ok(Some((vrt, copies)))
}

/// Reads a VRT whose sources are already open.
fn vrt_from_memory(vrt: &VrtDataset, sources: &[MosaicSource]) -> Result<RasterDataset> {
    use super::vrt::SourceRead;
    let n = vrt.width * vrt.height;
    // The output in its type, the values converted as they are stored.
    let data_type = vrt.bands[0].data_type;
    let mut out = RasterData::new_filled(data_type, n * vrt.bands.len(), 0.0);
    for (b, band) in vrt.bands.iter().enumerate() {
        crate::pixel::with_data!(&mut out, v => {
            let fill = super::convert_value(band.nodata.unwrap_or(0.0), data_type);
            v[b * n..(b + 1) * n].iter_mut().for_each(|x| *x = fill as _);
        });
        for src in &band.sources {
            let ds = sources
                .iter()
                .find(|s| absolute(&s.path) == src.filename)
                .map(|s| s.ds)
                .ok_or_else(|| Error::InvalidData(format!("unknown source '{}'", src.filename)))?;
            let values = super::BandView::of(&ds.raster, src.band - 1);
            let valid: Option<Vec<bool>> = src.nodata.map(|nd| values.valid(nd));
            let read = SourceRead {
                values,
                valid: valid.as_deref(),
                width: ds.raster.cols,
                height: ds.raster.rows,
                src_type: ds.raster.data_type,
                src_rect: src.src_rect,
                dst_rect: src.dst_rect,
                resampling: src.resampling,
                working_float: src.complex,
                invalid_value: None,
            };
            crate::pixel::with_data!(&mut out, v => {
                read.read(vrt.width, vrt.height, &mut v[b * n..(b + 1) * n], |x| super::convert_value(x, data_type) as _);
            });
        }
    }
    let grid = Grid { width: vrt.width, height: vrt.height, geo_transform: vrt.geo_transform.unwrap_or([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]) };
    super::build_raster_data(grid, out, vrt.bands.len(), vrt.bands[0].nodata, &vrt.srs.clone().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbraster::{DataType, Raster, RasterConfig};

    fn tile(x0: f64, y1: f64, value: f64) -> RasterDataset {
        let cfg = RasterConfig {
            cols: 4,
            rows: 4,
            bands: 1,
            x_min: x0,
            y_min: y1 - 4.0,
            cell_size: 1.0,
            cell_size_y: Some(1.0),
            nodata: -1.0,
            data_type: DataType::I16,
            crs: wbraster::CrsInfo::from_epsg(32610),
            metadata: vec![],
        };
        let mut data = vec![value; 16];
        data[0] = -1.0;
        RasterDataset::new(Raster::from_data(cfg, data).unwrap(), true)
    }

    #[test]
    fn mosaics_adjacent_tiles() {
        let (a, b) = (tile(0.0, 4.0, 1.0), tile(4.0, 4.0, 2.0));
        let srcs = [MosaicSource { path: "a.tif".into(), ds: &a }, MosaicSource { path: "b.tif".into(), ds: &b }];
        let out = mosaic(&srcs, &MosaicOptions::default()).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows), (8, 4));
        let v = out.raster.data.to_f64_vec();
        assert_eq!(&v[..8], &[-1.0, 1.0, 1.0, 1.0, -1.0, 2.0, 2.0, 2.0]);
        assert_eq!(out.nodata(), Some(-1.0));
        let vrt = build_vrt(&srcs, &MosaicOptions::default(), None).unwrap();
        assert_eq!(vrt.bands[0].sources[1].dst_rect, [4.0, 0.0, 4.0, 4.0]);
        assert!(vrt.bands[0].sources[0].complex);
    }

    #[test]
    fn overlapping_tiles_later_wins_except_nodata() {
        let (a, b) = (tile(0.0, 4.0, 1.0), tile(2.0, 4.0, 2.0));
        let srcs = [MosaicSource { path: "a.tif".into(), ds: &a }, MosaicSource { path: "b.tif".into(), ds: &b }];
        let out = mosaic(&srcs, &MosaicOptions::default()).unwrap();
        assert_eq!(out.raster.cols, 6);
        // b's nodata pixel at (2, 0) lets a's value show through.
        assert_eq!(&out.raster.data.to_f64_vec()[..6], &[-1.0, 1.0, 1.0, 2.0, 2.0, 2.0]);
    }

    /// Overlapping tiles, offset in both directions, copied a few rows at a time, are the
    /// mosaic read whole.
    #[test]
    fn strips_give_the_whole_mosaic() {
        let dir = tempfile::tempdir().unwrap();
        let shifted = |x0: f64, y1: f64, k: f64| {
            let mut t = tile(x0, y1, 0.0);
            let v: Vec<f64> = (0..16).map(|i| if i % 5 == 0 { -1.0 } else { k * 100.0 + i as f64 }).collect();
            (0..16).for_each(|i| t.raster.set(0, i / 4, i % 4, v[i as usize]).unwrap());
            t
        };
        let tiles = [shifted(0.0, 4.0, 1.0), shifted(2.0, 6.0, 2.0), shifted(5.0, 3.0, 3.0)];
        let paths: Vec<String> = (0..3).map(|i| dir.path().join(format!("t{i}.tif")).to_str().unwrap().to_string()).collect();
        for (t, p) in tiles.iter().zip(&paths) {
            crate::write_raster_dataset(t, p, &Default::default()).unwrap();
        }
        let srcs: Vec<MosaicSource> = paths.iter().zip(&tiles).map(|(p, ds)| MosaicSource { path: p.clone(), ds }).collect();
        for o in [MosaicOptions::default(), MosaicOptions { separate: true, ..Default::default() }] {
            let whole = mosaic(&srcs, &o).unwrap();
            let (vrt, copies) = copy_plan(&srcs, &o).unwrap().expect("copied pixel for pixel");
            for rows in [1, 2, 3, 100] {
                let out = dir.path().join("out.tif");
                let mut lazy: Vec<crate::LazyRaster> = paths.iter().map(|p| crate::open_raster_lazy(p).unwrap()).collect();
                copy_in_strips(&vrt, &copies, &mut lazy, out.to_str().unwrap(), &Default::default(), rows).unwrap();
                let back = crate::open_raster(out.to_str().unwrap()).unwrap();
                assert_eq!(back.raster.data.to_f64_vec(), whole.raster.data.to_f64_vec(), "separate {} rows {rows}", o.separate);
                assert_eq!((back.geo_transform(), back.nodata()), (whole.geo_transform(), whole.nodata()));
            }
            mosaic_to_path(&paths, &o, dir.path().join("all.tif").to_str().unwrap(), &Default::default()).unwrap();
        }
        // Resampled sources are read whole.
        let o = MosaicOptions { resolution: ResolutionStrategy::User(0.5, 0.5), ..Default::default() };
        assert!(copy_plan(&srcs, &o).unwrap().is_none());
        let out = dir.path().join("resampled.tif");
        mosaic_to_path(&paths, &o, out.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(
            crate::open_raster(out.to_str().unwrap()).unwrap().raster.data.to_f64_vec(),
            mosaic(&srcs, &o).unwrap().raster.data.to_f64_vec()
        );
    }

    #[test]
    fn separate_stacks_bands() {
        let (a, b) = (tile(0.0, 4.0, 1.0), tile(0.0, 4.0, 2.0));
        let srcs = [MosaicSource { path: "a.tif".into(), ds: &a }, MosaicSource { path: "b.tif".into(), ds: &b }];
        let out = mosaic(&srcs, &MosaicOptions { separate: true, ..Default::default() }).unwrap();
        assert_eq!(out.raster.bands, 2);
        assert_eq!(out.raster.data.to_f64_vec()[16 + 5], 2.0);
    }
}
