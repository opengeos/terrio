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
    let reproject = sources.iter().any(|s| {
        let c = s.ds.crs();
        !c.is_unknown() && !dst.is_unknown() && !c.same_as(&dst)
    });
    if reproject {
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

    #[test]
    fn separate_stacks_bands() {
        let (a, b) = (tile(0.0, 4.0, 1.0), tile(0.0, 4.0, 2.0));
        let srcs = [MosaicSource { path: "a.tif".into(), ds: &a }, MosaicSource { path: "b.tif".into(), ds: &b }];
        let out = mosaic(&srcs, &MosaicOptions { separate: true, ..Default::default() }).unwrap();
        assert_eq!(out.raster.bands, 2);
        assert_eq!(out.raster.data.to_f64_vec()[16 + 5], 2.0);
    }
}
