//! GeoPackage rasters: tile pyramids (PNG or JPEG tiles) and 2D gridded coverages (16-bit PNG
//! or 32-bit float TIFF tiles), read at full resolution (the highest zoom level), over the extent
//! in `gpkg_contents`, as GDAL reads them.

use super::common;
use super::sqlite::{Db, Table, Value};
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::raster::RasterData;
use wbraster::{CrsInfo, DataType};

fn bad(path: &Path, msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("{}: {msg}", path.display()))
}

fn f(t: &Table, row: &[Value], col: &str) -> Option<f64> {
    t.get(row, col).and_then(Value::as_f64)
}

fn i(t: &Table, row: &[Value], col: &str) -> Option<i64> {
    t.get(row, col).and_then(Value::as_i64)
}

/// A decoded tile: `(width, height, bands, samples)`, pixel-interleaved, as f64.
type Tile = (usize, usize, usize, Vec<f64>);

/// Decodes a PNG, JPEG or TIFF tile.
fn decode_tile(b: &[u8]) -> Result<Tile> {
    let err = |e: &dyn std::fmt::Display| Error::InvalidData(format!("GeoPackage tile: {e}"));
    if b.starts_with(b"\x89PNG") {
        let mut reader = png::Decoder::new(std::io::Cursor::new(b)).read_info().map_err(|e| err(&e))?;
        let mut buf = vec![0u8; reader.output_buffer_size().ok_or_else(|| err(&"too large"))?];
        let info = reader.next_frame(&mut buf).map_err(|e| err(&e))?;
        let bands = info.color_type.samples();
        let (w, h) = (info.width as usize, info.height as usize);
        let values: Vec<f64> = match info.bit_depth {
            png::BitDepth::Sixteen => buf[..w * h * bands * 2].chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]]) as f64).collect(),
            png::BitDepth::Eight => buf[..w * h * bands].iter().map(|v| *v as f64).collect(),
            d => return Err(err(&format!("{d:?} PNG tiles are not supported"))),
        };
        // Palette tiles are expanded to RGB(A).
        if info.color_type == png::ColorType::Indexed {
            let palette = reader.info().palette.as_deref().unwrap_or_default().to_vec();
            let trns = reader.info().trns.as_deref().unwrap_or_default().to_vec();
            let rgba = values
                .iter()
                .flat_map(|&ix| {
                    let k = ix as usize;
                    let c = |j: usize| *palette.get(3 * k + j).unwrap_or(&0) as f64;
                    [c(0), c(1), c(2), *trns.get(k).unwrap_or(&255) as f64]
                })
                .collect();
            return Ok((w, h, 4, rgba));
        }
        return Ok((w, h, bands, values));
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let mut d = jpeg_decoder::Decoder::new(std::io::Cursor::new(b));
        let pixels = d.decode().map_err(|e| err(&e))?;
        let info = d.info().ok_or_else(|| err(&"no JPEG header"))?;
        let bands = pixels.len() / (info.width as usize * info.height as usize).max(1);
        return Ok((info.width as usize, info.height as usize, bands, pixels.into_iter().map(f64::from).collect()));
    }
    if b.starts_with(b"II*\0") || b.starts_with(b"MM\0*") {
        use tiff::decoder::{Decoder, DecodingResult};
        let mut d = Decoder::new(std::io::Cursor::new(b)).map_err(|e| err(&e))?;
        let (w, h) = d.dimensions().map_err(|e| err(&e))?;
        let values: Vec<f64> = match d.read_image().map_err(|e| err(&e))? {
            DecodingResult::F32(v) => v.into_iter().map(f64::from).collect(),
            DecodingResult::F64(v) => v,
            DecodingResult::U8(v) => v.into_iter().map(f64::from).collect(),
            DecodingResult::U16(v) => v.into_iter().map(f64::from).collect(),
            DecodingResult::I16(v) => v.into_iter().map(f64::from).collect(),
            DecodingResult::I32(v) => v.into_iter().map(f64::from).collect(),
            DecodingResult::U32(v) => v.into_iter().map(f64::from).collect(),
            _ => return Err(err(&"unsupported TIFF tile type")),
        };
        let bands = values.len() / (w as usize * h as usize).max(1);
        return Ok((w as usize, h as usize, bands, values));
    }
    Err(err(&"unsupported tile format (PNG, JPEG and TIFF are supported)"))
}

/// Maps a tile's samples (gray, gray and alpha, RGB or RGBA) to `n` bands, as GDAL does: gray
/// and RGB values for the first bands, alpha (opaque when the tile has none) for the last of 2
/// or 4.
fn select_bands(bands: usize, values: Vec<f64>, n: usize) -> Vec<f64> {
    if bands == n {
        return values;
    }
    let mut out = Vec::with_capacity(values.len() / bands.max(1) * n);
    for px in values.chunks(bands.max(1)) {
        let rgba = match px.len() {
            1 => [px[0], px[0], px[0], 255.0],
            2 => [px[0], px[0], px[0], px[1]],
            3 => [px[0], px[1], px[2], 255.0],
            _ => [px[0], px[1], px[2], px[3]],
        };
        match n {
            1 => out.push(rgba[0]),
            2 => out.extend([rgba[0], rgba[3]]),
            3 => out.extend(&rgba[..3]),
            _ => out.extend(rgba),
        }
    }
    out
}

/// Reads the first raster table of a GeoPackage.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let db = Db::new(std::fs::read(path)?)?;
    let contents = db.table("gpkg_contents")?;
    let row = contents
        .rows
        .iter()
        .find(|r| matches!(contents.get(r, "data_type").and_then(Value::as_str), Some("tiles" | "2d-gridded-coverage")))
        .ok_or_else(|| bad(path, "no raster tables"))?;
    let name = contents.get(row, "table_name").and_then(Value::as_str).unwrap_or_default().to_string();
    let gridded = contents.get(row, "data_type").and_then(Value::as_str) == Some("2d-gridded-coverage");
    let tms = db.table("gpkg_tile_matrix_set")?;
    let tms_row = tms
        .rows
        .iter()
        .find(|r| tms.get(r, "table_name").and_then(Value::as_str) == Some(&name))
        .ok_or_else(|| bad(path, "no tile matrix set"))?;
    let (tminx, tmaxy) = (f(&tms, tms_row, "min_x").unwrap_or(0.0), f(&tms, tms_row, "max_y").unwrap_or(0.0));
    let srs_id = i(&tms, tms_row, "srs_id").or_else(|| i(&contents, row, "srs_id"));
    let (minx, miny, maxx, maxy) = (
        f(&contents, row, "min_x").unwrap_or(tminx),
        f(&contents, row, "min_y").unwrap_or(f(&tms, tms_row, "min_y").unwrap_or(0.0)),
        f(&contents, row, "max_x").unwrap_or(f(&tms, tms_row, "max_x").unwrap_or(0.0)),
        f(&contents, row, "max_y").unwrap_or(tmaxy),
    );

    // The full-resolution tile matrix.
    let tm = db.table("gpkg_tile_matrix")?;
    let level = tm
        .rows
        .iter()
        .filter(|r| tm.get(r, "table_name").and_then(Value::as_str) == Some(&name))
        .max_by_key(|r| i(&tm, r, "zoom_level").unwrap_or(-1))
        .ok_or_else(|| bad(path, "no tile matrix"))?;
    let zoom = i(&tm, level, "zoom_level").unwrap_or(0);
    let (tw, th) = (i(&tm, level, "tile_width").unwrap_or(256) as usize, i(&tm, level, "tile_height").unwrap_or(256) as usize);
    let (rx, ry) = (f(&tm, level, "pixel_x_size").ok_or_else(|| bad(path, "no pixel size"))?, f(&tm, level, "pixel_y_size").unwrap_or(0.0));
    let cols = ((maxx - minx) / rx + 0.5) as usize;
    let rows = ((maxy - miny) / ry + 0.5) as usize;
    // The dataset's offset in the tile matrix's pixel grid.
    let xoff = ((minx - tminx) / rx + 0.5).floor() as i64;
    let yoff = ((tmaxy - maxy) / ry + 0.5).floor() as i64;

    // Gridded coverages: value = (stored * tile scale + tile offset) * scale + offset.
    let mut coverage = (1.0, 0.0, None::<f64>, DataType::U8);
    if gridded {
        let anc = db.table("gpkg_2d_gridded_coverage_ancillary")?;
        let a = anc.rows.iter().find(|r| anc.get(r, "tile_matrix_set_name").and_then(Value::as_str) == Some(&name));
        if let Some(a) = a {
            let (scale, offset) = (f(&anc, a, "scale").unwrap_or(1.0), f(&anc, a, "offset").unwrap_or(0.0));
            let float = anc.get(a, "datatype").and_then(Value::as_str) == Some("float");
            let t = if float {
                DataType::F32
            } else if scale == 1.0 && offset == 0.0 {
                DataType::U16
            } else if scale == 1.0 && offset == -32768.0 {
                DataType::I16
            } else {
                DataType::F32
            };
            coverage = (scale, offset, f(&anc, a, "data_null"), t);
        }
    }
    let tile_anc = if gridded { db.table("gpkg_2d_gridded_tile_ancillary").ok() } else { None };

    // GDAL records the band count and nodata value of tile pyramids in gpkg_metadata; without
    // them, tiles are read as RGBA.
    let md = db.table("gpkg_metadata").ok();
    let md_item = |key: &str| {
        md.as_ref()?.rows.iter().find_map(|r| {
            let xml = md.as_ref()?.get(r, "metadata").and_then(Value::as_str)?;
            let start = xml.find(&format!("<MDI key=\"{key}\">"))? + key.len() + 12;
            xml[start..].split('<').next().map(str::to_string)
        })
    };
    let band_count =
        if gridded { None } else { Some(md_item("BAND_COUNT").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(4).clamp(1, 4)) };
    let tile_nodata = if gridded { None } else { md_item("NODATA_VALUE").and_then(|v| common::parse_f64(&v)) };
    if gridded && coverage.3 == DataType::I16 && coverage.2 == Some(65535.0) {
        // GDAL stores Int16 nodata as the largest 16-bit value.
        coverage.2 = Some(-32768.0);
    }
    let null_raw = if gridded && coverage.3 == DataType::I16 { Some(65535.0) } else { coverage.2 };

    let tiles = db.table(&name)?;
    let mut grid: Option<(usize, Vec<f64>)> = None;
    for r in &tiles.rows {
        if i(&tiles, r, "zoom_level") != Some(zoom) {
            continue;
        }
        let (Some(tc), Some(tr), Some(Value::Blob(blob))) =
            (i(&tiles, r, "tile_column"), i(&tiles, r, "tile_row"), tiles.get(r, "tile_data"))
        else {
            continue;
        };
        let (w, h, bands, values) = decode_tile(blob)?;
        let (bands, values) = match band_count {
            Some(n) => (n, select_bands(bands, values, n)),
            None => (bands, values),
        };
        let (mut tscale, mut toffset) = (1.0, 0.0);
        if let (Some(ta), Some(id)) = (&tile_anc, i(&tiles, r, "id"))
            && let Some(row) =
                ta.rows.iter().find(|x| ta.get(x, "tpudt_name").and_then(Value::as_str) == Some(&name) && i(ta, x, "tpudt_id") == Some(id))
        {
            tscale = f(ta, row, "scale").unwrap_or(1.0);
            toffset = f(ta, row, "offset").unwrap_or(0.0);
        }
        let fill = if gridded { coverage.2.unwrap_or(0.0) } else { tile_nodata.unwrap_or(0.0) };
        let (nb, out) = grid.get_or_insert_with(|| (bands, vec![fill; cols * rows * bands]));
        let nb = *nb;
        for y in 0..h.min(th) {
            let gy = tr * th as i64 + y as i64 - yoff;
            if gy < 0 || gy >= rows as i64 {
                continue;
            }
            for x in 0..w.min(tw) {
                let gx = tc * tw as i64 + x as i64 - xoff;
                if gx < 0 || gx >= cols as i64 {
                    continue;
                }
                for b in 0..nb.min(bands) {
                    let mut v = values[(y * w + x) * bands + b];
                    if gridded {
                        v = if null_raw == Some(v) { coverage.2.unwrap_or(v) } else { (v * tscale + toffset) * coverage.0 + coverage.1 };
                    }
                    out[(b * rows + gy as usize) * cols + gx as usize] = v;
                }
            }
        }
    }
    let (bands, values) = grid.unwrap_or((1, vec![0.0; cols * rows]));
    let data_type = if gridded { coverage.3 } else { DataType::U8 };
    let data = RasterData::from_f64_vec(data_type, values);
    let crs = srs_id.and_then(|id| crs_for_srs(&db, id)).unwrap_or_default();
    let gt = [minx, rx, 0.0, maxy, 0.0, -ry];
    let nodata = if gridded { coverage.2 } else { tile_nodata };
    let raster = common::make_raster(cols, rows, bands, data, gt, crs, nodata)?;
    Ok(RasterDataset { raster, driver: RasterDriver::GPKG, has_nodata: nodata.is_some() })
}

/// The CRS of a `gpkg_spatial_ref_sys` entry.
fn crs_for_srs(db: &Db, id: i64) -> Option<CrsInfo> {
    let t = db.table("gpkg_spatial_ref_sys").ok()?;
    let row = t.rows.iter().find(|r| i(&t, r, "srs_id") == Some(id))?;
    let org = t.get(row, "organization").and_then(Value::as_str).unwrap_or_default();
    if org.eq_ignore_ascii_case("EPSG")
        && let Some(code) = i(&t, row, "organization_coordsys_id").filter(|c| *c > 0)
    {
        return Some(CrsInfo::from_epsg(code as u32));
    }
    t.get(row, "definition").and_then(Value::as_str).and_then(common::crs_from_text)
}
