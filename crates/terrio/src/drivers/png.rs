// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/png/pngdataset.cpp
//     frmts/jpeg/jpgdataset.cpp
// No code is translated from these sources.

//! PNG rasters, read and written as GDAL does: 8- or 16-bit, 1 to 4 bands (gray, gray and
//! alpha, RGB, RGBA; palette indices as one band), a world file for the georeferencing, the
//! CRS in a `.aux.xml` sidecar, and a single-value `tRNS` chunk as the nodata value.

use super::common;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::raster::RasterData;
use wbraster::{DataType, Raster};

fn png_err(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("{}: {e}", path.display()))
}

/// Reads a PNG.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    let mut reader = decoder.read_info().map_err(|e| png_err(path, e))?;
    let info = reader.info().clone();
    let (cols, rows) = (info.width as usize, info.height as usize);
    let bands = match info.color_type {
        png::ColorType::Grayscale | png::ColorType::Indexed => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
    };
    let bits = info.bit_depth as usize;
    let mut buf = vec![0u8; reader.output_buffer_size().ok_or_else(|| png_err(path, "image too large"))?];
    let frame = reader.next_frame(&mut buf).map_err(|e| png_err(path, e))?;
    let line = frame.line_size;
    let n = cols * rows;
    let data = if bits == 16 {
        let mut v = vec![0u16; n * bands];
        for r in 0..rows {
            for c in 0..cols {
                for b in 0..bands {
                    let i = r * line + (c * bands + b) * 2;
                    v[b * n + r * cols + c] = u16::from_be_bytes([buf[i], buf[i + 1]]);
                }
            }
        }
        RasterData::U16(v)
    } else {
        let mut v = vec![0u8; n * bands];
        for r in 0..rows {
            for c in 0..cols {
                for b in 0..bands {
                    let bit = (c * bands + b) * bits;
                    let byte = buf[r * line + bit / 8];
                    v[b * n + r * cols + c] = if bits == 8 { byte } else { (byte >> (8 - bits - bit % 8)) & ((1 << bits) - 1) as u8 };
                }
            }
        }
        RasterData::U8(v)
    };
    // A grayscale tRNS chunk holds the one transparent (nodata) value.
    let nodata = match (&info.trns, info.color_type) {
        (Some(t), png::ColorType::Grayscale) if t.len() >= 2 => Some(u16::from_be_bytes([t[0], t[1]]) as f64),
        // The decoder keeps one byte for 8-bit images.
        (Some(t), png::ColorType::Grayscale) if t.len() == 1 => Some(t[0] as f64),
        _ => None,
    };
    let gt = read_world_file(path).unwrap_or(common::NO_GEOTRANSFORM);
    let raster = common::make_raster(cols, rows, bands, data, gt, Default::default(), nodata)?;
    let mut ds = RasterDataset::with_driver(raster, RasterDriver::PNG, nodata.is_some());
    if info.color_type == png::ColorType::Indexed
        && let Some(palette) = info.palette
    {
        ds.color_table = palette
            .as_chunks::<3>()
            .0
            .iter()
            .enumerate()
            .map(|(i, c)| [c[0], c[1], c[2], info.trns.as_ref().and_then(|a| a.get(i)).copied().unwrap_or(255)])
            .collect();
    }
    Ok(ds)
}

/// The geotransform of a world file next to `path` (`.wld`, or `.pgw`-style).
/// Reads a JPEG file with its world file, decoded as GDAL's JPEG driver decodes it (libjpeg's
/// output, see [`crate::jpeg`]).
pub(crate) fn read_jpeg(path: &Path) -> Result<RasterDataset> {
    let img = crate::jpeg::decode_file(&std::fs::read(path)?).map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
    let (cols, rows, bands) = (img.width, img.height, img.components);
    let n = cols * rows;
    let mut data = vec![0u8; n * bands];
    for (i, px) in img.data.chunks_exact(bands).enumerate() {
        for (b, v) in px.iter().enumerate() {
            data[b * n + i] = *v;
        }
    }
    let gt = read_world_file(path).unwrap_or(common::NO_GEOTRANSFORM);
    let raster = common::make_raster(cols, rows, bands, RasterData::U8(data), gt, Default::default(), None)?;
    Ok(RasterDataset::with_driver(raster, RasterDriver::JPEG, false))
}

pub(crate) fn read_world_file(path: &Path) -> Option<[f64; 6]> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let short = format!("{}{}w", &ext[..1], &ext[ext.len() - 1..]);
    let long = format!("{ext}w");
    let text = ["wld", short.as_str(), long.as_str()].iter().find_map(|e| std::fs::read_to_string(common::sibling(path, e)).ok())?;
    let v: Vec<f64> = text.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    if v.len() < 6 {
        return None;
    }
    // World files give the centre of the top-left pixel.
    Some([v[4] - v[0] / 2.0 - v[2] / 2.0, v[0], v[2], v[5] - v[1] / 2.0 - v[3] / 2.0, v[1], v[3]])
}

/// Writes a world file (`.wld`) for a raster.
pub(crate) fn write_world_file(r: &Raster, path: &Path) -> Result<()> {
    let (dx, dy) = (r.cell_size_x, -r.cell_size_y);
    let text = format!("{dx}\n0\n0\n{dy}\n{}\n{}\n", r.x_min + dx / 2.0, r.y_max() + dy / 2.0);
    std::fs::write(path.with_extension("wld"), text)?;
    Ok(())
}

/// Writes a PNG (Byte or UInt16, 1 to 4 bands), its world file and its CRS sidecar.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    let color = match r.bands {
        1 => png::ColorType::Grayscale,
        2 => png::ColorType::GrayscaleAlpha,
        3 => png::ColorType::Rgb,
        4 => png::ColorType::Rgba,
        n => return Err(Error::Unsupported(format!("PNG holds 1 to 4 bands, not {n}"))),
    };
    let sixteen = !matches!(r.data_type, DataType::U8 | DataType::I8);
    let converted;
    let r = if sixteen && r.data_type != DataType::U16 {
        converted = common::convert(r, DataType::U16);
        &converted
    } else if r.data_type == DataType::I8 {
        converted = common::convert(r, DataType::U8);
        &converted
    } else {
        r
    };
    let n = r.cols * r.rows;
    let mut bytes = Vec::with_capacity(n * r.bands * if sixteen { 2 } else { 1 });
    for p in 0..n {
        for b in 0..r.bands {
            match &r.data {
                RasterData::U8(v) => bytes.push(v[b * n + p]),
                RasterData::U16(v) => bytes.extend(v[b * n + p].to_be_bytes()),
                _ => unreachable!("converted above"),
            }
        }
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, r.cols as u32, r.rows as u32);
    enc.set_color(color);
    enc.set_depth(if sixteen { png::BitDepth::Sixteen } else { png::BitDepth::Eight });
    if let (Some(nd), png::ColorType::Grayscale) = (nodata, color)
        && nd >= 0.0
        && nd <= if sixteen { 65535.0 } else { 255.0 }
        && nd.fract() == 0.0
    {
        enc.set_trns((nd as u16).to_be_bytes().to_vec());
    }
    let mut w = enc.write_header().map_err(|e| png_err(path, e))?;
    w.write_image_data(&bytes).map_err(|e| png_err(path, e))?;
    w.finish().map_err(|e| png_err(path, e))?;
    write_world_file(r, path)?;
    common::write_pam(path, &r.crs)
}
