// SPDX-FileCopyrightText: Copyright (c) 2015, Even Rouault <even.rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2015, Airbus DS Geo SA (weighted Brovey algorithm)
// SPDX-FileCopyrightText: Copyright (c) 1998, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.12.2:
//     alg/gdalpansharpen.cpp
//     gcore/rasterio.cpp

//! Bounded working buffers shared by materialized and virtual pansharpening.
use super::{PansharpenExtent, PansharpenNodata, PansharpenOptions, Plan, SourceLayout, SpectralBand, bad, process_aligned};
use crate::ops::{BandView, Grid, build_raster};
use crate::{RasterDataset, Result, Window};
use wbraster::raster::RasterData;

pub(crate) const BLOCK: usize = 512;

/// Fallible typed output allocation, including capacity-overflow checks.
pub(super) fn allocate(dtype: wbraster::DataType, count: usize) -> Result<RasterData> {
    use wbraster::DataType;
    Ok(match dtype {
        DataType::U8 => RasterData::U8(super::filled(count, 0u8)?),
        DataType::I8 => RasterData::I8(super::filled(count, 0i8)?),
        DataType::U16 => RasterData::U16(super::filled(count, 0u16)?),
        DataType::I16 => RasterData::I16(super::filled(count, 0i16)?),
        DataType::U32 => RasterData::U32(super::filled(count, 0u32)?),
        DataType::I32 => RasterData::I32(super::filled(count, 0i32)?),
        DataType::U64 => RasterData::U64(super::filled(count, 0u64)?),
        DataType::I64 => RasterData::I64(super::filled(count, 0i64)?),
        DataType::F32 => RasterData::F32(super::filled(count, 0f32)?),
        DataType::F64 => RasterData::F64(super::filled(count, 0f64)?),
    })
}

pub(crate) fn window_grid(grid: Grid, window: Window) -> Grid {
    let mut gt = grid.geo_transform;
    gt[0] += window.x as f64 * gt[1];
    gt[3] += window.y as f64 * gt[5];
    Grid { width: window.width, height: window.height, geo_transform: gt }
}

/// Copy native samples in bulk when possible, retaining the existing conversion otherwise.
pub(super) fn copy_samples(dst: &mut RasterData, dst_offset: usize, src: &RasterData, src_offset: usize, count: usize) {
    macro_rules! copy {
        ($($variant:ident),*) => {
            match (dst, src) {
                $((RasterData::$variant(dst), RasterData::$variant(src)) =>
                    dst[dst_offset..dst_offset+count].copy_from_slice(&src[src_offset..src_offset+count]),)*
                (dst, src) => {
                    for i in 0..count { dst.set_f64(dst_offset+i, src.get_f64(src_offset+i)); }
                }
            }
        };
    }
    copy!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
}

/// Copy the requested part of an adjusted source, padding outside the original with zero.
/// Only the intersection is passed to `read`, so lazy sources do not fetch padded pixels.
pub(crate) fn aligned(
    layout: &SourceLayout,
    window: Window,
    read: impl FnMut(Window) -> Result<RasterDataset>,
    header: &RasterDataset,
) -> Result<RasterDataset> {
    aligned_bands(layout, window, read, header, 1)
}

/// Align selected bands together, preserving native samples in copied and padded rows.
pub(crate) fn aligned_bands(
    layout: &SourceLayout,
    window: Window,
    mut read: impl FnMut(Window) -> Result<RasterDataset>,
    header: &RasterDataset,
    bands: usize,
) -> Result<RasterDataset> {
    let (ox, oy) = layout.offset;
    let left = (window.x as isize + ox).max(0).min(layout.source_grid.width as isize) as usize;
    let top = (window.y as isize + oy).max(0).min(layout.source_grid.height as isize) as usize;
    let right = ((window.x + window.width) as isize + ox).max(0).min(layout.source_grid.width as isize) as usize;
    let bottom = ((window.y + window.height) as isize + oy).max(0).min(layout.source_grid.height as isize) as usize;
    let grid = window_grid(layout.grid, window);
    let size = window.width.checked_mul(window.height).ok_or_else(|| bad("pansharpen buffer size overflow"))?;
    let mut values = allocate(header.raster.data_type, size.checked_mul(bands).ok_or_else(|| bad("pansharpen band size overflow"))?)?;
    if right > left && bottom > top {
        let source = read(Window::new(left, top, right - left, bottom - top))?;
        let dx = (left as isize - ox - window.x as isize) as usize;
        let dy = (top as isize - oy - window.y as isize) as usize;
        let source_width = right - left;
        let source_size = source_width * (bottom - top);
        for band in 0..bands {
            for y in 0..bottom - top {
                copy_samples(
                    &mut values,
                    band * size + (dy + y) * window.width + dx,
                    &source.raster.data,
                    band * source_size + y * source_width,
                    source_width,
                );
            }
        }
    }
    crate::ops::build_raster_data(grid, values, bands, layout.nodata, &header.crs())
}

#[derive(Clone, Copy)]
struct Sampling {
    x: f64,
    y: f64,
    rx: f64,
    ry: f64,
    right: usize,
    bottom: usize,
    extracted: bool,
    copied: bool,
    origin: [f64; 8],
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
    mut read_mask: impl FnMut(usize, &SourceLayout, Window) -> Result<Option<Vec<u8>>>,
) -> Result<RasterData> {
    let samples = plan
        .spectral
        .iter()
        .map(|layout| {
            let sgt = layout.grid.geo_transform;
            let pg = plan.pan.grid.geo_transform;
            // Use GDAL's inverse-transform composition and operation order.
            let invx = 1.0 / sgt[1];
            let invy = 1.0 / sgt[5];
            let x = (invx * pg[0] - sgt[0] / sgt[1]) + window.x as f64 * (invx * pg[1]);
            let y = (invy * pg[3] - sgt[3] / sgt[5]) + window.y as f64 * (invy * pg[5]);
            if x <= -1. || y <= -1. {
                return Err(bad("pansharpen request is outside spectral input"));
            }
            let width = (window.width as f64 * (invx * pg[1])).min(layout.grid.width as f64 - x);
            let height = (window.height as f64 * (invy * pg[5])).min(layout.grid.height as f64 - y);
            if width <= 0.0 || height <= 0.0 {
                return Err(bad("pansharpen request has no spectral coverage"));
            }
            // GDAL first extracts an integer-sized MEM buffer for upsampling.
            // Average has radius zero: a fractional last pixel may lie outside
            // that buffer, and the averaging kernel clips its coverage there.
            let radius = options.resampling.radius();
            let extracted = ((width + 0.49999) as usize).max(1) < window.width
                && ((height + 0.49999) as usize).max(1) < window.height
                && window.height > 1
                && options.resampling != crate::ops::Resampling::Nearest;
            let limited = options.resampling == crate::ops::Resampling::Average && extracted;
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
            let copied = x == x.trunc() && y == y.trunc() && width == window.width as f64 && height == window.height as f64;
            let rx = width / window.width as f64;
            let ry = height / window.height as f64;
            let mut frame = [0., 0., -(x as isize) as f64, -(y as isize) as f64, x - (x as isize) as f64, y - (y as isize) as f64, rx, ry];
            for (value, ratio, base, dest, source, delta) in [
                (x, rx, if extracted { (x as isize - radius as isize).max(0) } else { 0 }, 0, 2, 4),
                (y, ry, if extracted { (y as isize - radius as isize).max(0) } else { 0 }, 1, 3, 5),
            ] {
                let virtual_off = (value - base as f64) / ratio;
                let rounded = (virtual_off + 0.5) as isize;
                if (virtual_off - rounded as f64).abs() < 1e-8 {
                    frame[dest] = rounded as f64;
                    frame[source] = -(base as f64);
                    frame[delta] = 0.;
                }
            }
            Ok(Sampling { x, y, rx, ry, right, bottom, extracted, copied, origin: frame })
        })
        .collect::<Result<Vec<_>>>()?;
    let count = window
        .width
        .checked_mul(window.height)
        .and_then(|n| n.checked_mul(bands.len()))
        .ok_or_else(|| bad("pansharpen output is too large"))?;
    let mut output = allocate(plan.output_type, count)?;
    // GDAL wraps all spectral bands when a common nodata override is needed.
    // Those SimpleSource wrappers do not propagate explicit dataset masks.
    let nodata_wrapper = plan.nodata.is_some_and(|nd| plan.spectral.iter().any(|source| source.nodata != Some(nd)));
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
            let mut spectral = Vec::with_capacity(samples.len());
            let mut reads = Vec::with_capacity(samples.len());
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
                let area = Window::new(left, top, right - left, bottom - top);
                let mut src = read(i + 1, layout, area)?;
                let mask = if !sampling.extracted
                    && !sampling.copied
                    && !layout.adjusted
                    && !nodata_wrapper
                    && options.resampling != crate::ops::Resampling::Nearest
                {
                    read_mask(i + 1, layout, area)?
                } else {
                    None
                };
                let mut origin = sampling.origin;
                origin[0] += x as f64;
                origin[1] += y as f64;
                origin[2] += left as f64;
                origin[3] += top as f64;
                reads.push(super::SpectralRead {
                    rect: [sx - left as f64, sy - top as f64, width as f64 * sampling.rx, height as f64 * sampling.ry],
                    mask,
                    copied: sampling.copied,
                    origin,
                });
                if sampling.extracted {
                    // RasterIO fills GDAL's MEM buffer in the pan working type
                    // before resampling, including rounding/clipping mixed inputs.
                    if src.raster.data_type != plan.work {
                        let count = src.raster.data.len();
                        let mut converted = allocate(plan.work, count)?;
                        for i in 0..count {
                            converted.set_f64(i, super::convert_value(src.raster.data.get_f64(i), plan.work));
                        }
                        src.raster.data = converted;
                        src.raster.data_type = plan.work;
                    }
                    // GDAL's temporary MEM upsampling bands receive only the
                    // operation's common nodata; original per-source nodata is dropped.
                    src.band_nodata = vec![plan.nodata];
                    src.has_nodata = plan.nodata.is_some();
                    src.raster.nodata = plan.nodata.unwrap_or(0.0);
                }
                spectral.push(src);
            }
            let refs = spectral.iter().map(|dataset| SpectralBand { dataset, band: 1 }).collect::<Vec<_>>();
            let block = process_aligned(&pan, &refs, &operation, &reads)?;
            let raw = &block.raster.data;
            for band in 0..bands.len() {
                for row in 0..height {
                    for col in 0..width {
                        let source = band * width * height + row * width + col;
                        let target = band * window.width * window.height + (y + row) * window.width + x + col;
                        output.set_f64(target, raw.get_f64(source));
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
    request(
        plan,
        options,
        window,
        &bands,
        |index, layout, area| {
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
        },
        |_, _, _| Ok(None),
    )
}
