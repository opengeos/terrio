//! Writing a raster into an existing GeoTIFF, reprojected to its grid, as `gdal raster update`
//! does ([`update_raster`]): the source is warped onto the destination's grid over the part of it
//! the source covers (optionally within a clipping geometry), its valid pixels replace the
//! destination's, and the destination's overviews are refreshed there.
//!
//! Only the changed blocks of the destination are rewritten (appended to the file, as GDAL's
//! GTiff driver rewrites blocks), through a copy renamed over it. An error before that
//! replacement leaves the TIFF unchanged. External overviews are refreshed afterward;
//! failure there leaves the updated TIFF committed with stale external overviews.

use super::Resampling;
use super::overview::{Computed, RefreshOptions};
use super::warp::{Cutline, Nodata, WarpOptions};
use crate::drivers::gtiff_update::{self, Appender};
use crate::{Error, LazyRaster, Result, Window, WindowOptions};
use std::io::{Read, Seek, Write};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// How [`update_raster`] writes the source into the destination.
#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    /// The resampling (`-r`, nearest by default).
    pub resampling: Resampling,
    /// Write only within these polygons (`--geometry`, with its CRS).
    pub geometry: Option<Cutline>,
    /// Leave the destination's overviews as they are (`--no-update-overviews`).
    pub no_update_overviews: bool,
    /// The approximate transformer's error threshold in pixels (`--et`; GDAL's 0.125 by
    /// default).
    pub error_threshold: Option<f64>,
    /// Update a Cloud Optimized GeoTIFF, breaking its layout; refused otherwise.
    pub ignore_cog_layout_break: bool,
}

/// What [`update_raster`] changed: the window of the destination (`x`, `y`, `width`, `height`
/// in pixels) whose blocks were rewritten, `None` when the source does not overlap it or has no
/// valid pixels in it (nothing was rewritten).
pub type UpdatedWindow = Option<Window>;

/// Writes `src` into the local GeoTIFF `dst`.
pub fn update_raster(src: &mut LazyRaster, dst: &str, options: &UpdateOptions) -> Result<UpdatedWindow> {
    let file = gtiff_update::writable_local_file(dst)?;
    let mut target = LazyRaster::open(dst)?;
    if target.driver() != crate::formats::RasterDriver::GTiff || !target.is_lazy() {
        return Err(Error::Unsupported(format!("only GeoTIFFs are updated in place, not '{dst}'")));
    }
    let external = std::path::Path::new(&format!("{dst}.ovr")).is_file() && !has_internal_overviews(&file)?;
    let o = options.clone();
    let window = gtiff_update::update_file_at(&file, move |f, tmp| {
        let window = update_tiff(f, &mut target, src, &o)?;
        drop(target);
        if let Some((_, extent)) = window.filter(|_| !o.no_update_overviews && !external) {
            f.flush()?;
            let mut updated = LazyRaster::open_tiff(&tmp.to_string_lossy())?.ok_or_else(|| Error::InvalidData("not a TIFF".into()))?;
            if !updated.overviews().is_empty() {
                super::overview::refresh(f, &mut updated, &refresh_options(&o, extent, false))?;
            }
        }
        Ok(window)
    })?;
    if let Some((_, extent)) = window.filter(|_| !options.no_update_overviews && external) {
        super::overview::refresh_overviews(dst, &refresh_options(options, extent, true))?;
    }
    Ok(window.map(|w| w.0))
}

/// [`update_raster`] into a GeoTIFF held in memory: returns the updated file.
pub fn update_raster_bytes(src: &mut LazyRaster, dst: &[u8], options: &UpdateOptions) -> Result<Vec<u8>> {
    update_raster_bytes_owned(src, dst.to_vec(), options)
}

/// Owned-buffer update: shares the input with its lazy reader, retaining at most the
/// immutable reader and writable output file buffers at each stage. The output is a copy of
/// the input (the reader keeps the original), and an overview refresh copies the updated file
/// once more for its reader: peak memory is about two copies of the TIFF.
pub fn update_raster_bytes_owned(src: &mut LazyRaster, dst: Vec<u8>, options: &UpdateOptions) -> Result<Vec<u8>> {
    let bytes = std::sync::Arc::new(dst);
    let mut target =
        LazyRaster::from_tiff_shared_bytes(bytes.clone())?.ok_or_else(|| Error::InvalidData("the destination is not a TIFF".into()))?;
    let mut out = std::io::Cursor::new(bytes.as_ref().clone());
    drop(bytes);
    let window = update_tiff(&mut out, &mut target, src, options)?;
    let mut bytes = out.into_inner();
    drop(target);
    if let Some((_, extent)) = window.filter(|_| !options.no_update_overviews) {
        let mut updated = LazyRaster::from_tiff_bytes(bytes.clone())?.ok_or_else(|| Error::InvalidData("not a TIFF".into()))?;
        if !updated.overviews().is_empty() {
            let r = refresh_options(options, extent, false);
            let mut cursor = std::io::Cursor::new(bytes);
            super::overview::refresh(&mut cursor, &mut updated, &r)?;
            bytes = cursor.into_inner();
        }
    }
    Ok(bytes)
}

fn has_internal_overviews(path: &std::path::Path) -> Result<bool> {
    let t = gtiff_update::parse(&mut std::io::BufReader::new(std::fs::File::open(path)?))?;
    Ok(t.dirs.iter().any(|d| d.is_overview()))
}

/// How the destination's overviews are refreshed over the source's extent: as GDAL does, with
/// the update's resampling (the overviews' own when it is not one overviews are computed with).
fn refresh_options(o: &UpdateOptions, extent: [f64; 4], external: bool) -> RefreshOptions {
    let overview_method =
        matches!(o.resampling, Resampling::Nearest | Resampling::Average | Resampling::Mode | Resampling::Min | Resampling::Max);
    RefreshOptions {
        resampling: overview_method.then_some(o.resampling),
        bbox: vec![extent],
        external,
        // Safe only because `update_tiff` has already refused (or been allowed) the COG layout
        // break before this refresh: a new caller must run that check itself.
        ignore_cog_layout_break: true,
        ..Default::default()
    }
}

/// The window of the destination the source covers, in whole blocks, and the source's extent
/// in the destination's CRS.
fn covered_window(dst: &LazyRaster, src: &LazyRaster) -> Result<Option<(Window, [f64; 4])>> {
    for (r, which) in [(dst, "destination"), (src, "source")] {
        if !r.has_geo_transform() {
            return Err(Error::Unsupported(format!("the {which} has no geotransform of its own: it cannot be placed")));
        }
    }
    let (w, h) = (dst.width(), dst.height());
    let gt = src.geo_transform();
    let (sw, sh) = (src.width() as f64, src.height() as f64);
    let corners = [(0.0, 0.0), (sw, 0.0), (0.0, sh), (sw, sh)].map(|(c, l)| super::apply_gt(&gt, c, l));
    let b = (
        corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min),
        corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
        corners.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max),
        corners.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
    );
    let (s, d) = (src.crs(), dst.crs());
    let b = if s.is_unknown() || d.is_unknown() || s.same_as(&d) { b } else { super::transform_bounds(&super::Ct::new(&s, &d)?, b, 21)? };
    let inv = super::invert_gt(&dst.geo_transform())
        .ok_or_else(|| Error::InvalidData("the destination's geotransform cannot be inverted".into()))?;
    let px = [(b.0, b.1), (b.2, b.1), (b.0, b.3), (b.2, b.3)].map(|(x, y)| super::apply_gt(&inv, x, y));
    // A pixel of margin for the resampling kernels.
    let x0 = (px.iter().map(|p| p.0).fold(f64::INFINITY, f64::min).floor() - 1.0).max(0.0);
    let x1 = (px.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max).ceil() + 1.0).min(w as f64);
    let y0 = (px.iter().map(|p| p.1).fold(f64::INFINITY, f64::min).floor() - 1.0).max(0.0);
    let y1 = (px.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max).ceil() + 1.0).min(h as f64);
    if !(x0 < x1 && y0 < y1) {
        return Ok(None);
    }
    let (bw, bh) = dst.block_size().unwrap_or((w, h));
    let (x0, y0) = (x0 as usize / bw * bw, y0 as usize / bh * bh);
    let (x1, y1) = (((x1 as usize).div_ceil(bw) * bw).min(w), ((y1 as usize).div_ceil(bh) * bh).min(h));
    Ok(Some((Window::new(x0, y0, x1 - x0, y1 - y0), [b.0, b.1, b.2, b.3])))
}

/// Whether pixels already share an affine grid, up to roundoff in its integer origin offset.
fn pixel_aligned_translation(source: &[f64; 6], destination: &[f64; 6]) -> bool {
    if [1, 2, 4, 5].iter().any(|&i| source[i] != destination[i]) {
        return false;
    }
    let Some(inverse) = super::invert_gt(destination) else { return false };
    let (x, y) = super::apply_gt(&inverse, source[0], source[3]);
    x.is_finite() && y.is_finite() && (x - x.round()).abs() < 1e-7 && (y - y.round()).abs() < 1e-7
}

fn update_tiff<F: Read + Write + Seek>(
    file: &mut F,
    dst: &mut LazyRaster,
    src: &mut LazyRaster,
    o: &UpdateOptions,
) -> Result<Option<(Window, [f64; 4])>> {
    let tiff = gtiff_update::parse(file)?;
    let main = tiff
        .dirs
        .iter()
        .position(|d| !d.is_overview() && !d.is_mask())
        .ok_or_else(|| Error::InvalidData("the TIFF has no image".into()))?;
    let bands = dst.band_count();
    if src.band_count() != bands {
        return Err(Error::InvalidArgument(format!("the source has {} band(s), the destination {bands}", src.band_count())));
    }
    let Some((win, extent)) = covered_window(dst, src)? else { return Ok(None) };
    let gt = dst.geo_transform();
    let wgt = [
        gt[0] + win.x as f64 * gt[1] + win.y as f64 * gt[2],
        gt[1],
        gt[2],
        gt[3] + win.x as f64 * gt[4] + win.y as f64 * gt[5],
        gt[4],
        gt[5],
    ];
    let crs = dst.crs();
    let geometry = o.geometry.clone().map(|mut g| {
        if g.srs.is_none() && !crs.is_unknown() {
            g.srs = Some(crs.clone());
        }
        g
    });
    let wo = WarpOptions {
        dst_srs: (!crs.is_unknown()).then_some(crs),
        dst_transform: Some(wgt),
        size: Some((win.width, win.height)),
        // Averaging one exactly aligned source cell is a direct copy. Avoid allowing
        // affine roundoff to introduce a neighboring source cell at a shared edge.
        resampling: if o.resampling == Resampling::Average
            && (src.crs().same_as(&dst.crs()) || src.crs().is_unknown() && dst.crs().is_unknown())
            && pixel_aligned_translation(&src.geo_transform(), &dst.geo_transform())
        {
            Resampling::Nearest
        } else {
            o.resampling
        },
        dst_nodata: Nodata::Values(vec![f64::NAN]),
        output_type: Some(DataType::F64),
        cutline: geometry,
        error_threshold: o.error_threshold.unwrap_or(0.125),
        ..Default::default()
    };
    let warped = super::warp::warp_lazy(src, &wo)?.raster.data.to_f64_vec();
    // The destination's rows of the window, all columns, with the source's valid pixels.
    let (w, dt) = (dst.width(), dst.data_type());
    if matches!(dt, DataType::I64 | DataType::U64) {
        crate::warnings::warn("raster update warps through Float64; 64-bit integer values above 2^53 may lose precision".into());
    }
    let rows = win.y..win.y + win.height;
    let mut data = dst.read_window(Window::new(0, win.y, w, win.height), &WindowOptions::default())?.raster.data;
    let n = w * win.height;
    let wn = win.width * win.height;
    let mut changed = false;
    crate::pixel::with_data!(&mut data, d => {
        for b in 0..bands {
            for r in 0..win.height {
                for c in 0..win.width {
                    let v = warped[b * wn + r * win.width + c];
                    if !v.is_nan() {
                        d[b * n + r * w + win.x + c] = crate::pixel::Pixel::from_f64(super::warp_round(v, dt));
                        changed = true;
                    }
                }
            }
        }
    });
    if !changed {
        return Ok(None);
    }
    gtiff_update::break_cog_layout(file, tiff.big, o.ignore_cog_layout_break)?;
    let c = Computed { size: (w, dst.height()), rows, data, mask: None };
    let mut a = Appender::new(file, tiff.big)?;
    let mut dirs = tiff.dirs.clone();
    dirs[main] = super::overview::replace_chunks(&mut a, &tiff.dirs[main], &c, win.x..win.x + win.width, dt, bands, None)?;
    a.finish(&dirs)?;
    Ok(Some((win, extent)))
}

/// Reads a clipping geometry given as WKT or GeoJSON (`--geometry`), in `crs` (the
/// destination's when `None`).
pub fn parse_geometry(text: &str, crs: Option<crate::crs::SpatialRef>) -> Result<Cutline> {
    let t = text.trim();
    let ds = if t.starts_with('{') {
        crate::memory::open_vector_bytes(t.as_bytes().to_vec(), "geometry.geojson", &Default::default())?
    } else {
        let csv = format!("WKT\n\"{}\"\n", t.replace('"', "\"\""));
        crate::memory::open_vector_bytes(csv.into_bytes(), "geometry.csv", &Default::default())?
    };
    let mut c = Cutline::from_vector(&ds)?;
    // The given CRS, else the destination's (set when the update starts).
    c.srs = crs;
    Ok(c)
}

#[cfg(test)]
mod update_boundary_tests {
    use super::pixel_aligned_translation;

    #[test]
    fn only_integer_offsets_on_identical_pixel_grids_are_direct_copies() {
        let destination = [0., 1., 0., 12., 0., -1.];
        assert!(pixel_aligned_translation(&[4., 1., 0., 8., 0., -1.], &destination));
        assert!(pixel_aligned_translation(&[4. + 1e-12, 1., 0., 8., 0., -1.], &destination));
        assert!(!pixel_aligned_translation(&[4.25, 1., 0., 8., 0., -1.], &destination));
        assert!(!pixel_aligned_translation(&[4., 2., 0., 8., 0., -2.], &destination));
    }

    #[test]
    fn rasters_without_their_own_geotransform_are_refused() {
        use crate::drivers::gtiff_write as gw;
        let cfg = wbraster::RasterConfig { cols: 4, rows: 3, bands: 1, data_type: wbraster::DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data(cfg, vec![1.0; 12]).unwrap();
        let referenced = gw::encode(&r, &gw::TiffOptions::default()).unwrap();
        let unreferenced = gw::encode(&r, &gw::TiffOptions { georeferenced: false, ..Default::default() }).unwrap();
        for (src, dst, which) in [(&referenced, &unreferenced, "destination"), (&unreferenced, &referenced, "source")] {
            let mut src = crate::LazyRaster::from_tiff_bytes(src.clone()).unwrap().unwrap();
            let err = super::update_raster_bytes(&mut src, dst, &Default::default()).unwrap_err();
            assert!(matches!(&err, crate::Error::Unsupported(m) if m.contains(which)), "{err}");
        }
    }
}
