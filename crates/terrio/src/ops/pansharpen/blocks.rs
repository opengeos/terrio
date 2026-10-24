//! Bounded working buffers shared by materialized and virtual pansharpening.
use super::{PansharpenExtent, PansharpenNodata, PansharpenOptions, Plan, SourceLayout, SpectralBand, bad, process_aligned};
use crate::ops::{BandView, Grid, build_raster};
use crate::{RasterDataset, Result, Window};
use wbraster::raster::RasterData;

pub(crate) const BLOCK: usize = 512;

pub(crate) fn window_grid(grid: Grid, window: Window) -> Grid {
    let mut gt = grid.geo_transform;
    gt[0] += window.x as f64 * gt[1];
    gt[3] += window.y as f64 * gt[5];
    Grid { width: window.width, height: window.height, geo_transform: gt }
}

/// Copy the requested part of an adjusted source, padding outside the original with zero.
/// Only the intersection is passed to `read`, so lazy sources do not fetch padded pixels.
pub(crate) fn aligned(
    layout: &SourceLayout,
    window: Window,
    mut read: impl FnMut(Window) -> Result<RasterDataset>,
    header: &RasterDataset,
) -> Result<RasterDataset> {
    let (ox, oy) = layout.offset;
    let left = (window.x as isize + ox).max(0).min(layout.source_grid.width as isize) as usize;
    let top = (window.y as isize + oy).max(0).min(layout.source_grid.height as isize) as usize;
    let right = ((window.x + window.width) as isize + ox).max(0).min(layout.source_grid.width as isize) as usize;
    let bottom = ((window.y + window.height) as isize + oy).max(0).min(layout.source_grid.height as isize) as usize;
    let grid = window_grid(layout.grid, window);
    let mut values = vec![0.0; window.width * window.height];
    if right > left && bottom > top {
        let source = read(Window::new(left, top, right - left, bottom - top))?;
        let view = BandView::of(&source.raster, 0);
        let dx = (left as isize - ox - window.x as isize) as usize;
        let dy = (top as isize - oy - window.y as isize) as usize;
        for y in 0..bottom - top {
            for x in 0..right - left {
                values[(dy + y) * window.width + dx + x] = view.get(y * (right - left) + x);
            }
        }
    }
    build_raster(grid, vec![values], header.raster.data_type, layout.nodata, &header.crs(), |v, _| v)
}

#[derive(Clone, Copy)]
struct Sampling {
    x: f64,
    y: f64,
    rx: f64,
    ry: f64,
    right: usize,
    bottom: usize,
}

/// Keep GDAL's fractional sampling rectangle fixed over a request while splitting its
/// working buffers into blocks. In particular, clipping at a source's right/bottom edge
/// must not change the interpolation ratio independently for each internal block.
pub(crate) fn request(
    plan: &Plan,
    options: &PansharpenOptions,
    window: Window,
    bands: &[usize],
    mut read: impl FnMut(usize, &SourceLayout, Window) -> Result<RasterDataset>,
) -> Result<RasterData> {
    let grid = window_grid(plan.pan.grid, window);
    let gt = grid.geo_transform;
    let samples = plan
        .spectral
        .iter()
        .map(|layout| {
            let sgt = layout.grid.geo_transform;
            let (mut x, mut y) = ((gt[0] - sgt[0]) / sgt[1], (gt[3] - sgt[3]) / sgt[5]);
            if x.abs() < 1e-8 {
                x = 0.0;
            }
            if y.abs() < 1e-8 {
                y = 0.0;
            }
            if x <= -1.0 || y <= -1.0 {
                return Err(bad("pansharpen request is outside spectral input"));
            }
            let width = (window.width as f64 * gt[1] / sgt[1]).min(layout.grid.width as f64 - x);
            let height = (window.height as f64 * gt[5] / sgt[5]).min(layout.grid.height as f64 - y);
            if width <= 0.0 || height <= 0.0 {
                return Err(bad("pansharpen request has no spectral coverage"));
            }
            // GDAL first extracts an integer-sized MEM buffer for upsampling.
            // Average has radius zero: a fractional last pixel may lie outside
            // that buffer, and the averaging kernel clips its coverage there.
            let radius = options.resampling.radius();
            let limited = options.resampling == crate::ops::Resampling::Average
                && width < window.width as f64
                && height < window.height as f64
                && window.height > 1;
            let right = if limited {
                ((x as isize + (width + 0.49999).max(1.0) as isize + 1 + 2 * radius as isize).max(0) as usize).min(layout.grid.width)
            } else {
                layout.grid.width
            };
            let bottom = if limited {
                ((y as isize + (height + 0.49999).max(1.0) as isize + 1 + 2 * radius as isize).max(0) as usize).min(layout.grid.height)
            } else {
                layout.grid.height
            };
            Ok(Sampling { x, y, rx: width / window.width as f64, ry: height / window.height as f64, right, bottom })
        })
        .collect::<Result<Vec<_>>>()?;
    let count = window
        .width
        .checked_mul(window.height)
        .and_then(|n| n.checked_mul(bands.len()))
        .ok_or_else(|| bad("pansharpen output is too large"))?;
    let mut output = RasterData::new_filled(plan.output_type, count, 0.0);
    let mut operation = options.clone();
    operation.pan_band = 1;
    operation.extent = PansharpenExtent::None;
    operation.weights = plan.weights.clone();
    operation.output_bands = bands.iter().map(|b| plan.output[*b - 1]).collect();
    operation.output_type = Some(plan.output_type);
    operation.nodata = plan.nodata.map_or(PansharpenNodata::None, PansharpenNodata::Value);
    for y in (0..window.height).step_by(BLOCK) {
        for x in (0..window.width).step_by(BLOCK) {
            let width = BLOCK.min(window.width - x);
            let height = BLOCK.min(window.height - y);
            let pan = read(0, &plan.pan, Window::new(window.x + x, window.y + y, width, height))?;
            let pgt = pan.geo_transform();
            let mut spectral = Vec::with_capacity(samples.len());
            for (i, (sampling, layout)) in samples.iter().zip(&plan.spectral).enumerate() {
                let (sx, sy) = (sampling.x + x as f64 * sampling.rx, sampling.y + y as f64 * sampling.ry);
                let radius = options.resampling.radius().max(1) as f64;
                let padx = (radius * sampling.rx.max(1.0)).ceil() + 2.0;
                let pady = (radius * sampling.ry.max(1.0)).ceil() + 2.0;
                let left = (sx.floor() - padx).max(0.0).min(layout.grid.width as f64) as usize;
                let top = (sy.floor() - pady).max(0.0).min(layout.grid.height as f64) as usize;
                let right = ((sx + width as f64 * sampling.rx).ceil() + padx).max(0.0).min(sampling.right as f64) as usize;
                let bottom = ((sy + height as f64 * sampling.ry).ceil() + pady).max(0.0).min(sampling.bottom as f64) as usize;
                if right <= left || bottom <= top {
                    return Err(bad("pansharpen block has no spectral coverage"));
                }
                let mut src = read(i + 1, layout, Window::new(left, top, right - left, bottom - top))?;
                // Encode the fixed request sampling ratio in this temporary source's
                // affine metadata. Its pixel values still come from the original grid.
                let dx = pgt[1] / sampling.rx;
                let dy = pgt[5] / sampling.ry;
                src.set_geo_transform([pgt[0] - (sx - left as f64) * dx, dx, 0.0, pgt[3] - (sy - top as f64) * dy, 0.0, dy]);
                spectral.push(src);
            }
            let refs = spectral.iter().map(|dataset| SpectralBand { dataset, band: 1 }).collect::<Vec<_>>();
            let block = process_aligned(&pan, &refs, &operation)?;
            let raw = block.raster.data.to_f64_vec();
            for band in 0..bands.len() {
                for row in 0..height {
                    for col in 0..width {
                        let source = band * width * height + row * width + col;
                        let target = band * window.width * window.height + (y + row) * window.width + x + col;
                        output.set_f64(target, raw[source]);
                    }
                }
            }
        }
    }
    Ok(output)
}

pub(crate) fn in_memory(
    pan: &RasterDataset,
    spectral: &[SpectralBand<'_>],
    options: &PansharpenOptions,
    plan: &Plan,
) -> Result<RasterData> {
    let bands = (1..=plan.output.len()).collect::<Vec<_>>();
    let window = Window::new(0, 0, plan.pan.grid.width, plan.pan.grid.height);
    request(plan, options, window, &bands, |index, layout, area| {
        let (source, band) = if index == 0 { (pan, options.pan_band) } else { (spectral[index - 1].dataset, spectral[index - 1].band) };
        aligned(
            layout,
            area,
            |original| {
                let view = BandView::of(&source.raster, band - 1);
                let mut values = Vec::with_capacity(original.width * original.height);
                for y in original.y..original.y + original.height {
                    for x in original.x..original.x + original.width {
                        values.push(view.get(y * source.raster.cols + x));
                    }
                }
                build_raster(
                    window_grid(Grid::of(&source.raster), original),
                    vec![values],
                    source.raster.data_type,
                    source.nodata(),
                    &source.crs(),
                    |v, _| v,
                )
            },
            source,
        )
    })
}
