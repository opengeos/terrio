// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/gpkg/gdalgeopackagerasterband.cpp
//     ogr/ogrsf_frmts/gpkg/ogrgeopackagedatasource.cpp
// No code is translated from these sources.

//! GeoPackage rasters: tile pyramids (PNG or JPEG tiles) and 2D gridded coverages (16-bit PNG
//! or 32-bit float TIFF tiles), read at full resolution (the highest zoom level), over the extent
//! in `gpkg_contents`, as GDAL reads them; and written as GDAL writes them.
//!
//! Files holding vector layers and rasters open as their vector layers; their raster tables are
//! subdatasets, named as GDAL names them (`GPKG:file.gpkg:table`).

use super::common::xml_escape;
use super::sqlite::{Db, Table, Value};
use super::vector::gpkg_read::Tables;
use super::{common, nodata};
use crate::dataset::fallible;
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

/// Header-derived storage and the buffers used to decode/convert a tile (GDAL 3.13.3 reference).
struct TileLayout {
    width: usize,
    height: usize,
    bands: usize,
    values: Option<usize>,
    peak: Option<usize>,
}

impl TileLayout {
    fn new(width: usize, height: usize, bands: usize, raw: Option<usize>, extra: Option<usize>) -> Self {
        let values = buffer_bytes(&[width, height, bands], 8);
        let peak = values.zip(raw).zip(extra).and_then(|((values, raw), extra)| values.checked_add(raw)?.checked_add(extra));
        Self { width, height, bands, values, peak }
    }
}

fn buffer_bytes(dimensions: &[usize], sample_bytes: usize) -> Option<usize> {
    dimensions.iter().try_fold(sample_bytes, |n, &d| n.checked_mul(d)).filter(|&n| n <= isize::MAX as usize)
}

fn check_working_set(bytes: Option<usize>) -> Result<()> {
    fallible::check_usable(bytes, || "a GeoPackage raster working set is too large to allocate".into())
        .map(|_| ())
        .map_err(Error::ResourceLimit)
}

fn tile_values(len: usize, values: impl IntoIterator<Item = f64>) -> Result<Vec<f64>> {
    let mut out = Vec::new();
    fallible::reserve(&mut out, len, "GeoPackage tile samples")?;
    out.extend(values);
    Ok(out)
}

/// Decodes a PNG, JPEG or TIFF tile.
fn decode_tile(b: &[u8], mut check: impl FnMut(TileLayout) -> Result<()>) -> Result<Tile> {
    let err = |e: &dyn std::fmt::Display| Error::InvalidData(format!("GeoPackage tile: {e}"));
    if b.starts_with(b"\x89PNG") {
        let mut reader = png::Decoder::new(std::io::Cursor::new(b)).read_info().map_err(|e| err(&e))?;
        let header = reader.info();
        let (w, h) = (header.width as usize, header.height as usize);
        let indexed = header.color_type == png::ColorType::Indexed;
        let bands = if indexed { 4 } else { header.color_type.samples() };
        let indices = if indexed { buffer_bytes(&[w, h], 8) } else { Some(0) };
        check(TileLayout::new(w, h, bands, reader.output_buffer_size(), indices))?;
        let mut buf = fallible::filled(reader.output_buffer_size().ok_or_else(|| err(&"too large"))?, 0u8, "a GeoPackage PNG tile")?;
        let info = reader.next_frame(&mut buf).map_err(|e| err(&e))?;
        let bands = info.color_type.samples();
        let (w, h) = (info.width as usize, info.height as usize);
        let values: Vec<f64> = match info.bit_depth {
            png::BitDepth::Sixteen => {
                tile_values(w * h * bands, buf[..w * h * bands * 2].chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]]) as f64))?
            }
            png::BitDepth::Eight => tile_values(w * h * bands, buf[..w * h * bands].iter().map(|v| *v as f64))?,
            d => return Err(err(&format!("{d:?} PNG tiles are not supported"))),
        };
        // Palette tiles are expanded to RGB(A).
        if info.color_type == png::ColorType::Indexed {
            let palette = reader.info().palette.as_deref().unwrap_or_default();
            let trns = reader.info().trns.as_deref().unwrap_or_default();
            let rgba = tile_values(
                w * h * 4,
                values.iter().flat_map(|&ix| {
                    let k = ix as usize;
                    let c = |j: usize| *palette.get(3 * k + j).unwrap_or(&0) as f64;
                    [c(0), c(1), c(2), *trns.get(k).unwrap_or(&255) as f64]
                }),
            )?;
            return Ok((w, h, 4, rgba));
        }
        return Ok((w, h, bands, values));
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let mut header = jpeg_decoder::Decoder::new(std::io::Cursor::new(b));
        let shape = match header.read_info().ok().and_then(|_| header.info()) {
            Some(info) => (info.width as usize, info.height as usize, info.pixel_format.pixel_bytes()),
            // The native decoder also supports raw two-component baseline frames.
            None => crate::jpeg::frame_dimensions(b).map_err(|e| err(&e))?,
        };
        let (w, h, bands) = shape;
        check(TileLayout::new(w, h, bands, buffer_bytes(&[w, h, bands], 1), Some(0)))?;
        let img = crate::jpeg::decode_file(b)
            .map_err(|e| if crate::jpeg::is_resource_refusal(&e) { Error::ResourceLimit(e.to_string()) } else { err(&e) })?;
        return Ok((img.width, img.height, img.components, tile_values(img.data.len(), img.data.into_iter().map(f64::from))?));
    }
    if b.starts_with(b"II*\0") || b.starts_with(b"MM\0*") {
        use tiff::decoder::{Decoder, DecodingResult, DecodingSampleType};
        let mut d = Decoder::new(std::io::Cursor::new(b)).map_err(|e| err(&e))?;
        let (w, h) = d.dimensions().map_err(|e| err(&e))?;
        let layout = d.image_buffer_layout().map_err(|e| err(&e))?;
        let bytes = match layout.sample_type {
            Some(DecodingSampleType::U8) => 1,
            Some(DecodingSampleType::U16 | DecodingSampleType::I16) => 2,
            Some(DecodingSampleType::U32 | DecodingSampleType::I32 | DecodingSampleType::F32) => 4,
            Some(DecodingSampleType::F64) => 8,
            _ => return Err(err(&"unsupported TIFF tile type")),
        };
        // read_image and image_buffer_layout both read only the first plane of planar TIFFs.
        // Preserve that decoder's existing sample count rather than inventing extra bands.
        let pixels = fallible::len(&[w as usize, h as usize], "a GeoPackage TIFF tile")?;
        let bands = layout.len.div_ceil(bytes) / pixels.max(1);
        // Float64 decoding already owns the output vector; other sample types need a conversion buffer.
        let raw = if matches!(layout.sample_type, Some(DecodingSampleType::F64)) { 0 } else { layout.len };
        check(TileLayout::new(w as usize, h as usize, bands, Some(raw), Some(0)))?;
        let values: Vec<f64> = match d.read_image().map_err(|e| err(&e))? {
            DecodingResult::F32(v) => tile_values(v.len(), v.into_iter().map(f64::from))?,
            DecodingResult::F64(v) => v,
            DecodingResult::U8(v) => tile_values(v.len(), v.into_iter().map(f64::from))?,
            DecodingResult::U16(v) => tile_values(v.len(), v.into_iter().map(f64::from))?,
            DecodingResult::I16(v) => tile_values(v.len(), v.into_iter().map(f64::from))?,
            DecodingResult::I32(v) => tile_values(v.len(), v.into_iter().map(f64::from))?,
            DecodingResult::U32(v) => tile_values(v.len(), v.into_iter().map(f64::from))?,
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
fn select_bands(bands: usize, values: Vec<f64>, n: usize) -> Result<Vec<f64>> {
    if bands == n {
        return Ok(values);
    }
    let mut out = Vec::new();
    fallible::reserve(&mut out, fallible::len(&[values.len() / bands.max(1), n], "GeoPackage tile bands")?, "GeoPackage tile bands")?;
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
    Ok(out)
}

/// Reads the first raster table of a GeoPackage.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    read_db(&super::sqlite_edit::open_consistent(std::path::Path::new(path))?, None, path)
}

/// Reads a raster table of a GeoPackage held in memory (the first when `table` is `None`).
pub(crate) fn read_bytes(bytes: Vec<u8>, table: Option<&str>) -> Result<RasterDataset> {
    read_db(&Db::new(bytes)?, table, Path::new("GeoPackage"))
}

/// Opens a GeoPackage: its vector layers when it has some, otherwise its first raster.
pub(crate) fn open(path: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    crate::crs::prefetch();
    open_bytes(super::sqlite_edit::read_consistent(Path::new(path))?, path, options)
}

/// Opens a GeoPackage held in memory, `name` being its file name. Of its vector layers, those
/// `options` names, each with the features its spatial filter keeps, then the range of them
/// `skip_features` and `max_features` ask for.
pub(crate) fn open_bytes(bytes: Vec<u8>, name: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    use crate::drivers::vector::gpkg_read;
    let db = Db::new(bytes)?;
    let (layers, rasters) = gpkg_read::contents(&db)?;
    if layers.is_empty() && !rasters.is_empty() {
        let mut ds = read_db(&db, None, Path::new(name))?;
        nodata::apply_pam(name, &mut ds);
        return Ok(crate::Dataset::Raster(ds));
    }
    let read = gpkg_read::read_db_named(&db, options, Some(name))?;
    Ok(crate::Dataset::Vector(crate::dataset::finish_vector(read, name, crate::formats::VectorDriver::GPKG)))
}

/// The raster tables of a GeoPackage, as GDAL's subdatasets: `(name, description)`, the name
/// being `GPKG:<path>:<table>`.
pub(crate) fn subdatasets(path: &str) -> Vec<(String, String)> {
    let Ok(db) = super::sqlite_edit::read_consistent(Path::new(path)).and_then(Db::new) else { return Vec::new() };
    let Ok((_, rasters)) = crate::drivers::vector::gpkg_read::contents(&db) else { return Vec::new() };
    rasters
        .into_iter()
        .map(|c| {
            let desc = if c.description.is_empty() { c.name.clone() } else { c.description.clone() };
            (format!("GPKG:{path}:{}", c.name), format!("{} - {desc}", c.name))
        })
        .collect()
}

/// Splits a GDAL GeoPackage subdataset name, `GPKG:<path>:<table>`.
pub(crate) fn parse_subdataset(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("GPKG:").or_else(|| name.strip_prefix("gpkg:"))?;
    let (file, table) = rest.rsplit_once(':')?;
    (!file.is_empty() && !table.is_empty()).then(|| (file.to_string(), table.to_string()))
}

/// Reads a raster table (the first when `table` is `None`).
pub(crate) fn read_db(db: &Db, table: Option<&str>, path: &Path) -> Result<RasterDataset> {
    let contents = db.table("gpkg_contents")?;
    let row = contents
        .rows
        .iter()
        .find(|r| {
            matches!(contents.get(r, "data_type").and_then(Value::as_str), Some("tiles" | "2d-gridded-coverage"))
                && table.is_none_or(|t| contents.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(t)))
        })
        .ok_or_else(|| match table {
            Some(t) => bad(path, format!("no raster table '{t}'")),
            None => bad(path, "no raster tables"),
        })?;
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
    let tile_anc = if gridded { db.optional_table("gpkg_2d_gridded_tile_ancillary")? } else { None };

    // GDAL records the band count and nodata value of tile pyramids in gpkg_metadata; without
    // them, tiles are read as RGBA.
    let md = db.optional_table("gpkg_metadata")?;
    // The metadata attached to this table, when the references say which.
    let md_ids: Vec<i64> = db
        .optional_table("gpkg_metadata_reference")?
        .map(|t| {
            t.rows
                .iter()
                .filter(|r| t.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(&name)))
                .filter_map(|r| i(&t, r, "md_file_id"))
                .collect()
        })
        .unwrap_or_default();
    let md_item = |key: &str| {
        let md = md.as_ref()?;
        md.rows.iter().filter(|r| md_ids.is_empty() || i(md, r, "id").is_some_and(|id| md_ids.contains(&id))).find_map(|r| {
            let xml = md.get(r, "metadata").and_then(Value::as_str)?;
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
    let data_type = if gridded { coverage.3 } else { DataType::U8 };
    // What the tile matrix declares, before any tile is decoded (see `crate::declared`); a
    // coverage's bands come from its tiles, one by default.
    crate::declared::check(cols, rows, band_count.unwrap_or(1), data_type, false)?;

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
        let (w, h, bands, values) = decode_tile(blob, |tile| {
            let bands = band_count.unwrap_or(tile.bands);
            if grid.is_none() && band_count.is_none() {
                // Coverage bands come from the first tile's header, before pixels are decoded.
                crate::declared::check(cols, rows, bands, data_type, false)?;
            }
            let grid_bands = grid.as_ref().map_or(bands, |(n, _)| *n);
            let grid_bytes = buffer_bytes(&[cols, rows, grid_bands], 8);
            let retained = grid.as_ref().map_or(Some(0), |(_, v)| buffer_bytes(&[v.capacity()], 8));
            let remap = band_count.is_some_and(|n| n != tile.bands);
            let mapped = if remap { buffer_bytes(&[tile.width, tile.height, bands], 8) } else { Some(0) };
            let kept = if remap { mapped } else { tile.values };
            let output = if data_type == DataType::F64 { Some(0) } else { buffer_bytes(&[cols, rows, grid_bands], data_type.size_bytes()) };
            // The first tile is decoded before the grid exists. Subsequent tiles retain the grid;
            // remapping retains old/new samples, and final conversion retains the Float64 grid.
            let phases = [
                tile.peak.zip(retained).and_then(|(a, b)| a.checked_add(b)),
                tile.values.zip(mapped).zip(retained).and_then(|((a, b), c)| a.checked_add(b)?.checked_add(c)),
                grid_bytes.zip(kept).and_then(|(a, b)| a.checked_add(b)),
                grid_bytes.zip(output).and_then(|(a, b)| a.checked_add(b)),
            ];
            check_working_set(phases.into_iter().try_fold(0usize, |peak, bytes| Some(peak.max(bytes?))))
        })?;
        let (bands, values) = match band_count {
            Some(n) => (n, select_bands(bands, values, n)?),
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
        if grid.is_none() {
            let len = fallible::len(&[cols, rows, bands], "a GeoPackage raster grid")?;
            grid = Some((bands, fallible::filled(len, fill, "a GeoPackage raster grid")?));
        }
        let (nb, out) = grid.as_mut().unwrap();
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
    let (bands, values) = match grid {
        Some(grid) => grid,
        None => {
            let working = buffer_bytes(&[cols, rows], 8);
            let output = if data_type == DataType::F64 { Some(0) } else { buffer_bytes(&[cols, rows], data_type.size_bytes()) };
            check_working_set(working.zip(output).and_then(|(a, b)| a.checked_add(b)))?;
            (1, fallible::filled(fallible::len(&[cols, rows], "an empty GeoPackage raster")?, 0., "an empty GeoPackage raster")?)
        }
    };
    let data = if data_type == DataType::F64 {
        RasterData::F64(values)
    } else {
        let mut data = fallible::raster_data(data_type, values.len(), 0., "a GeoPackage raster output")?;
        crate::pixel::with_data!(&mut data, out => {
            for (dst, &value) in out.iter_mut().zip(&values) {
                *dst = value as _;
            }
        });
        data
    };
    let crs = srs_id.map(|id| crs_for_srs(db, id)).transpose()?.flatten().unwrap_or_default();
    let gt = [minx, rx, 0.0, maxy, 0.0, -ry];
    let nodata = if gridded { coverage.2 } else { tile_nodata };
    let raster = common::make_raster(cols, rows, bands, data, gt, crs, nodata)?;
    Ok(RasterDataset::with_driver(raster, RasterDriver::GPKG, nodata.is_some()))
}

/// The CRS of a `gpkg_spatial_ref_sys` entry.
fn crs_for_srs(db: &Db, id: i64) -> Result<Option<CrsInfo>> {
    let Some(t) = db.optional_table("gpkg_spatial_ref_sys")? else { return Ok(None) };
    let Some(row) = t.rows.iter().find(|r| i(&t, r, "srs_id") == Some(id)) else { return Ok(None) };
    let org = t.get(row, "organization").and_then(Value::as_str).unwrap_or_default();
    if org.eq_ignore_ascii_case("EPSG")
        && let Some(code) = i(&t, row, "organization_coordsys_id").filter(|c| *c > 0)
    {
        return Ok(Some(crate::crs::epsg_crs_info(code as u32)));
    }
    Ok(t.get(row, "definition").and_then(Value::as_str).and_then(common::crs_from_text))
}

/// The tile size GDAL writes.
const TILE: usize = 256;

/// How a raster's tiles are encoded.
#[derive(Debug, Clone, Copy, PartialEq)]
enum TileFormat {
    /// 8-bit PNG tiles: gray, gray and alpha, RGB or RGBA.
    Png,
    /// JPEG tiles (lossy): gray or RGB.
    Jpeg,
    /// 16-bit PNG tiles of a gridded coverage (Int16 and UInt16).
    Png16,
    /// 32-bit float TIFF tiles of a gridded coverage (Float32).
    Tiff,
}

/// Writes a raster as a GeoPackage: a tile pyramid for Byte rasters (1 to 4 bands), a 2D gridded
/// coverage for one-band Int16, UInt16 and Float32 rasters. `compression` picks the tiles of Byte
/// rasters: `PNG` (the default) or `JPEG` (GDAL's `TILE_FORMAT` creation option says the same).
/// `table` names the tile table (by default, the file name without its extension, as in GDAL).
pub(crate) fn write(r: &wbraster::Raster, nodata: Option<f64>, path: &Path, compression: Option<&str>, table: Option<&str>) -> Result<()> {
    let table = table.or_else(|| path.file_stem().and_then(|s| s.to_str())).filter(|s| !s.is_empty()).unwrap_or("raster");
    common::write_replacing(path, to_bytes(r, nodata, table, compression)?)?;
    Ok(())
}

/// Encodes a raster as a GeoPackage holding it in the tile table `table`.
pub(crate) fn to_bytes(r: &wbraster::Raster, nodata: Option<f64>, table: &str, compression: Option<&str>) -> Result<Vec<u8>> {
    let mut b = crate::drivers::vector::geopackage::Builder::new();
    add_raster(&mut b, r, nodata, table, compression)?;
    b.finish()
}

/// GDAL's tiling of a raster (its `CUSTOM` tiling scheme): the tile matrix set starts at the
/// raster's top left corner, and the full resolution is the zoom level whose square matrix of
/// 256-pixel tiles first covers it. Returns that zoom level.
fn zoom_levels(cols: usize, rows: usize) -> u32 {
    let mut z = 0;
    while (TILE << z) < cols.max(rows) {
        z += 1;
    }
    z
}

/// A level of the pyramid: band-major values.
struct Level {
    cols: usize,
    rows: usize,
    values: Vec<f64>,
}

/// The raster decimated by `f` (a power of two), picking each output pixel's nearest source
/// pixel, as GDAL's nearest-neighbour overviews do.
fn decimate(r: &wbraster::Raster, full: &[f64], f: usize) -> Level {
    let (cols, rows) = (r.cols.div_ceil(f), r.rows.div_ceil(f));
    let n = r.cols * r.rows;
    let mut values = Vec::with_capacity(cols * rows * r.bands);
    for b in 0..r.bands {
        for y in 0..rows {
            let sy = (y * f + f / 2).min(r.rows - 1);
            for x in 0..cols {
                let sx = (x * f + f / 2).min(r.cols - 1);
                values.push(full[b * n + sy * r.cols + sx]);
            }
        }
    }
    Level { cols, rows, values }
}

fn encode_png(w: usize, h: usize, bands: usize, sixteen: bool, data: &[u8]) -> Result<Vec<u8>> {
    let err = |e: png::EncodingError| Error::InvalidData(format!("GeoPackage tile: {e}"));
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(match bands {
            1 => png::ColorType::Grayscale,
            2 => png::ColorType::GrayscaleAlpha,
            3 => png::ColorType::Rgb,
            _ => png::ColorType::Rgba,
        });
        enc.set_depth(if sixteen { png::BitDepth::Sixteen } else { png::BitDepth::Eight });
        let mut wr = enc.write_header().map_err(err)?;
        wr.write_image_data(data).map_err(err)?;
        wr.finish().map_err(err)?;
    }
    Ok(out)
}

fn encode_jpeg(w: usize, h: usize, bands: usize, data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut out, 75);
    let color = if bands == 1 { jpeg_encoder::ColorType::Luma } else { jpeg_encoder::ColorType::Rgb };
    enc.encode(data, w as u16, h as u16, color).map_err(|e| Error::InvalidData(format!("GeoPackage tile: {e}")))?;
    Ok(out)
}

fn encode_tiff(w: usize, h: usize, values: Vec<f64>) -> Result<Vec<u8>> {
    let data = RasterData::from_f64_vec(DataType::F32, values);
    let tile = common::make_raster(w, h, 1, data, [0.0, 1.0, 0.0, 0.0, 0.0, -1.0], CrsInfo::default(), None)?;
    let opts = super::gtiff_write::TiffOptions { compression: super::gtiff_write::Compression::Lzw, ..Default::default() };
    super::gtiff_write::encode(&tile, &opts)
}

/// Statistics of a tile's valid values: `[min, max, mean, std_dev]`.
fn tile_stats(values: &[f64], valid: impl Fn(f64) -> bool) -> [Option<f64>; 4] {
    let (mut n, mut min, mut max, mut mean, mut m2) = (0u64, f64::INFINITY, f64::NEG_INFINITY, 0.0, 0.0);
    for &v in values.iter().filter(|v| valid(**v)) {
        n += 1;
        min = min.min(v);
        max = max.max(v);
        let d = v - mean;
        mean += d / n as f64;
        m2 += d * (v - mean);
    }
    if n == 0 { [None; 4] } else { [Some(min), Some(max), Some(mean), Some((m2 / n as f64).sqrt())] }
}

/// Adds a raster to a GeoPackage being built, as the tile table `table`.
pub(crate) fn add_raster(
    b: &mut crate::drivers::vector::geopackage::Builder,
    r: &wbraster::Raster,
    nodata: Option<f64>,
    table: &str,
    compression: Option<&str>,
) -> Result<()> {
    if r.cols == 0 || r.rows == 0 {
        return Err(Error::InvalidArgument("cannot write an empty raster".into()));
    }
    let format = match (r.data_type, r.bands) {
        (DataType::U8, 1..=4) => match compression.map(str::to_ascii_uppercase).as_deref() {
            None | Some("PNG") => TileFormat::Png,
            Some("JPEG") if matches!(r.bands, 1 | 3) => TileFormat::Jpeg,
            Some("JPEG") => return Err(Error::Unsupported("JPEG tiles hold 1 or 3 bands: write PNG tiles".into())),
            Some(c) => return Err(Error::InvalidArgument(format!("unknown GeoPackage tile format '{c}' (PNG, JPEG)"))),
        },
        (DataType::U8, n) => return Err(Error::Unsupported(format!("GeoPackage tiles hold 1 to 4 Byte bands, not {n}"))),
        (DataType::I16 | DataType::U16, 1) => TileFormat::Png16,
        (DataType::F32, 1) => TileFormat::Tiff,
        (t, n) => {
            return Err(Error::Unsupported(format!(
                "GeoPackage rasters are Byte (1 to 4 bands) or one band of Int16, UInt16 or Float32, not {n} band(s) of {}: convert the data type first",
                crate::info::gdal_type_name(t)
            )));
        }
    };
    if let Some(c) = compression {
        let suits = match format {
            TileFormat::Png16 => c.eq_ignore_ascii_case("PNG"),
            TileFormat::Tiff => c.eq_ignore_ascii_case("TIFF"),
            _ => true,
        };
        if !suits {
            let name = crate::info::gdal_type_name(r.data_type);
            return Err(Error::InvalidArgument(format!(
                "{name} GeoPackage tiles are {}, not {c}",
                if format == TileFormat::Tiff { "TIFF" } else { "PNG" }
            )));
        }
    }
    let gridded = matches!(format, TileFormat::Png16 | TileFormat::Tiff);

    // The CRS.
    let s = common::spatial_ref(&r.crs);
    let srs_id = b.srs_id(&s, s.epsg, super::wkt::gdal_wkt(&s));

    // Nodata as the tiles can hold it: Int16 nodata becomes -32768 (stored as 65535), as in GDAL;
    // UInt16 nodata must be a 16-bit value; Byte nodata a byte; Float32 NaN cannot be recorded.
    let nodata = match r.data_type {
        DataType::I16 => nodata,
        DataType::U16 | DataType::U8 => {
            let max = if r.data_type == DataType::U8 { 255.0 } else { 65535.0 };
            nodata.filter(|v| (0.0..=max).contains(v) && v.fract() == 0.0)
        }
        _ => nodata.filter(|v| v.is_finite()),
    };
    let is_nodata = |v: f64| nodata.is_some_and(|nd| v == nd || (v.is_nan() && nd.is_nan()));
    // Int16 values are shifted by 32768 (and by one more with -32768 as nodata, as GDAL does, so
    // that 65535 is free for nodata).
    let i16_offset = if nodata == Some(-32768.0) { 1.0 } else { 0.0 };

    let z = zoom_levels(r.cols, r.rows);
    let (px, py) = (r.cell_size_x, r.cell_size_y);
    let (x0, y0) = (r.x_min, r.y_max());
    let span = (TILE << z) as f64;
    let matrices: Vec<(i64, i64, i64, i64, i64, f64, f64)> = (0..=z)
        .map(|k| {
            let f = (1u64 << (z - k)) as f64;
            (i64::from(k), 1 << k, 1 << k, TILE as i64, TILE as i64, px * f, py * f)
        })
        .collect();

    let full = r.data.to_f64_vec();
    let mut tiles = Vec::new();
    let mut ancillary = Vec::new();
    let fill = nodata.unwrap_or(0.0);
    for k in (0..=z).rev() {
        let f = 1usize << (z - k);
        let level = if f == 1 { Level { cols: r.cols, rows: r.rows, values: full.clone() } } else { decimate(r, &full, f) };
        let n = level.cols * level.rows;
        for tr in 0..level.rows.div_ceil(TILE) {
            for tc in 0..level.cols.div_ceil(TILE) {
                // The tile's values, pixel-interleaved, with the area outside the raster filled.
                let mut values = vec![fill; TILE * TILE * r.bands];
                let mut any_data = false;
                for y in 0..TILE {
                    let gy = tr * TILE + y;
                    if gy >= level.rows {
                        break;
                    }
                    for x in 0..TILE {
                        let gx = tc * TILE + x;
                        if gx >= level.cols {
                            break;
                        }
                        for band in 0..r.bands {
                            let v = level.values[band * n + gy * level.cols + gx];
                            any_data |= !is_nodata(v);
                            values[(y * TILE + x) * r.bands + band] = v;
                        }
                    }
                }
                // GDAL leaves out tiles holding nodata only.
                if !any_data {
                    continue;
                }
                let blob = match format {
                    TileFormat::Png | TileFormat::Jpeg => {
                        let bytes: Vec<u8> = values.iter().map(|v| v.clamp(0.0, 255.0) as u8).collect();
                        if format == TileFormat::Png {
                            encode_png(TILE, TILE, r.bands, false, &bytes)?
                        } else {
                            encode_jpeg(TILE, TILE, r.bands, &bytes)?
                        }
                    }
                    TileFormat::Png16 => {
                        let signed = r.data_type == DataType::I16;
                        let mut bytes = Vec::with_capacity(values.len() * 2);
                        for &v in &values {
                            let stored = if is_nodata(v) {
                                if signed { 65535.0 } else { v }
                            } else if signed {
                                let s = v + 32768.0 - i16_offset;
                                // 65535 is nodata: the largest value moves down one, as in GDAL.
                                if nodata.is_some() && s >= 65535.0 { 65534.0 } else { s }
                            } else {
                                v
                            };
                            bytes.extend((stored.clamp(0.0, 65535.0) as u16).to_be_bytes());
                        }
                        encode_png(TILE, TILE, 1, true, &bytes)?
                    }
                    TileFormat::Tiff => encode_tiff(TILE, TILE, values.clone())?,
                };
                if gridded {
                    let offset = if r.data_type == DataType::I16 { i16_offset } else { 0.0 };
                    // Statistics over the tile's pixels inside the raster.
                    let inside: Vec<f64> = (0..TILE * TILE)
                        .filter(|p| tc * TILE + p % TILE < level.cols && tr * TILE + p / TILE < level.rows)
                        .map(|p| values[p])
                        .collect();
                    let stats = tile_stats(&inside, |v| !is_nodata(v) && v.is_finite());
                    ancillary.push((tiles.len(), 1.0, offset, stats));
                }
                tiles.push((i64::from(k), tc as i64, tr as i64, blob));
            }
        }
    }
    let extent = [x0, y0 - r.rows as f64 * py, x0 + r.cols as f64 * px, y0];
    let matrix_set = [x0, y0 - span * py, x0 + span * px, y0];
    let data_type = if gridded { "2d-gridded-coverage" } else { "tiles" };
    let ids = b.add_tiles(table, data_type, srs_id, extent, matrix_set, &matrices, tiles)?;

    // Metadata as GDAL records it: the band count and nodata value of tile pyramids, and the
    // dataset's metadata items.
    let mut xml = String::from("<GDALMultiDomainMetadata>\n");
    if !r.metadata.is_empty() {
        xml.push_str("  <Metadata>\n");
        for (k, v) in &r.metadata {
            xml.push_str(&format!("    <MDI key=\"{}\">{}</MDI>\n", xml_escape(k), xml_escape(v)));
        }
        xml.push_str("  </Metadata>\n");
    }
    if !gridded && (r.bands != 4 || nodata.is_some()) {
        xml.push_str("  <Metadata domain=\"IMAGE_STRUCTURE\">\n");
        if r.bands != 4 {
            xml.push_str(&format!("    <MDI key=\"BAND_COUNT\">{}</MDI>\n", r.bands));
        }
        if let Some(nd) = nodata {
            xml.push_str(&format!("    <MDI key=\"NODATA_VALUE\">{nd}</MDI>\n"));
        }
        xml.push_str("  </Metadata>\n");
    }
    xml.push_str("</GDALMultiDomainMetadata>\n");
    if xml.lines().count() > 2 {
        b.add_metadata(table, xml);
    }

    if gridded {
        let (datatype, offset, null) = match r.data_type {
            DataType::I16 => ("integer", -32768.0, nodata.map(|_| 65535.0)),
            DataType::U16 => ("integer", 0.0, nodata),
            _ => ("float", 0.0, nodata),
        };
        let tiles = ancillary.into_iter().map(|(i, scale, offset, stats)| (ids[i], scale, offset, stats)).collect();
        b.add_coverage(table, (datatype, 1.0, offset, null), tiles);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference: GDAL 3.13.3. Declared limits describe the returned bands, not unused tile channels.
    #[test]
    fn coverage_band_count_is_fixed_by_the_first_tile() {
        for first in [vec![7.], vec![7., 8., 9.]] {
            let bands = first.len();
            let mut tiles = Vec::new();
            for (col, values) in [first, if bands == 1 { vec![11., 12., 13.] } else { vec![11.] }].into_iter().enumerate() {
                let raster =
                    common::make_raster(1, 1, values.len(), RasterData::F32(values), common::NO_GEOTRANSFORM, Default::default(), None)
                        .unwrap();
                tiles.push((0, col as i64, 0, super::super::gtiff_write::encode(&raster, &Default::default()).unwrap()));
            }
            let mut builder = super::super::vector::geopackage::Builder::new();
            let extent = [0., 0., 2., 1.];
            let ids = builder.add_tiles("grid", "2d-gridded-coverage", 0, extent, extent, &[(0, 2, 1, 1, 1, 1., 1.)], tiles).unwrap();
            builder.add_coverage("grid", ("float", 1., 0., None), ids.into_iter().map(|id| (id, 1., 0., [None; 4])).collect());
            let db = Db::new(builder.finish().unwrap()).unwrap();
            let count = bands.to_string();
            let bytes = (2 * bands * 4).to_string();
            crate::config::with_options(&[("TERRIO_MAX_BANDS", &count), ("TERRIO_MAX_DECLARED_BYTES", &bytes)], || {
                let ds = read_db(&db, Some("grid"), Path::new("coverage.gpkg")).unwrap();
                assert_eq!(ds.raster.bands, bands);
                assert_eq!(ds.raster.data.to_f64_vec(), if bands == 1 { vec![7., 11.] } else { vec![7., 11., 8., 0., 9., 0.] });
            });
        }
    }

    /// Reference: GDAL 3.13.3. The Float64 grid and final typed array coexist on coverage reads.
    #[test]
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    fn coverage_working_grid_is_checked_even_without_tiles() {
        const CHILD: &str = "TERRIO_TEST_GPKG_WORKING_MEMORY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "drivers::gpkg::tests::coverage_working_grid_is_checked_even_without_tiles"])
                .env(CHILD, "1")
                .env_remove("TERRIO_MAX_DECLARED_BYTES")
                .env_remove("TERRIO_MAX_BANDS")
                .output()
                .unwrap();
            assert!(child.status.success(), "{}\n{}", String::from_utf8_lossy(&child.stdout), String::from_utf8_lossy(&child.stderr));
            return;
        }
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: valid pointers; only this isolated child's usable-memory policy is constrained.
        unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_RSS, &mut limit), 0);
            limit.rlim_cur = limit.rlim_max.min(128 << 20);
            assert_eq!(libc::setrlimit(libc::RLIMIT_RSS, &limit), 0);
        }
        let raster = common::make_raster(1, 1, 1, RasterData::F32(vec![7.]), common::NO_GEOTRANSFORM, Default::default(), None).unwrap();
        let tile = super::super::gtiff_write::encode(&raster, &Default::default()).unwrap();
        for with_tile in [true, false] {
            for size in [1536, 5000] {
                let mut builder = super::super::vector::geopackage::Builder::new();
                let extent = [0., 0., size as f64, size as f64];
                let tiles = if with_tile { vec![(0, 0, 0, tile.clone())] } else { Vec::new() };
                let ids =
                    builder.add_tiles("grid", "2d-gridded-coverage", 0, extent, extent, &[(0, size, size, 1, 1, 1., 1.)], tiles).unwrap();
                builder.add_coverage("grid", ("float", 1., 0., None), ids.into_iter().map(|id| (id, 1., 0., [None; 4])).collect());
                let db = Db::new(builder.finish().unwrap()).unwrap();
                let result = read_db(&db, Some("grid"), Path::new("coverage.gpkg"));
                if size == 1536 {
                    let ds = result.unwrap();
                    let RasterData::F32(values) = ds.raster.data else { panic!("expected Float32 coverage") };
                    assert_eq!(values.len(), (size as usize).pow(2));
                    assert_eq!(values[0], if with_tile { 7. } else { 0. });
                    assert!(values[1..].iter().all(|&v| v == 0.));
                } else {
                    let result = result.map(|_| ());
                    assert!(matches!(result, Err(Error::ResourceLimit(_))), "with_tile={with_tile}: {result:?}");
                }
            }
        }
        // A tiny destination can still need a large decoded tile. Its Float32 buffer
        // alone fits this policy, but its simultaneous Float64 conversion does not.
        let tags = [
            (256u16, 4u16, 5000u32),
            (257, 4, 5000),
            (258, 3, 32),
            (259, 3, 1),
            (262, 3, 1),
            (273, 4, 8 + 2 + 10 * 12 + 4),
            (277, 3, 1),
            (278, 4, 5000),
            (279, 4, 100_000_000),
            (339, 3, 3),
        ];
        let mut tile = b"II*\0\x08\0\0\0".to_vec();
        tile.extend_from_slice(&(tags.len() as u16).to_le_bytes());
        for (tag, kind, value) in tags {
            tile.extend_from_slice(&tag.to_le_bytes());
            tile.extend_from_slice(&kind.to_le_bytes());
            tile.extend_from_slice(&1u32.to_le_bytes());
            tile.extend_from_slice(&value.to_le_bytes());
        }
        tile.extend_from_slice(&[0; 8]);
        assert!(tiff::decoder::Decoder::new(std::io::Cursor::new(&tile)).is_ok());
        let mut builder = super::super::vector::geopackage::Builder::new();
        let extent = [0., 0., 1., 1.];
        let ids =
            builder.add_tiles("grid", "2d-gridded-coverage", 0, extent, extent, &[(0, 1, 1, 1, 1, 1., 1.)], vec![(0, 0, 0, tile)]).unwrap();
        builder.add_coverage("grid", ("float", 1., 0., None), vec![(ids[0], 1., 0., [None; 4])]);
        let db = Db::new(builder.finish().unwrap()).unwrap();
        let result = read_db(&db, Some("grid"), Path::new("coverage.gpkg")).map(|_| ());
        assert!(matches!(result, Err(Error::ResourceLimit(_))), "{result:?}");
    }

    /// Reference: GDAL 3.13.3. A valid tile header supplies bands before corrupt pixels are decoded.
    #[test]
    fn coverage_band_limits_precede_corrupt_tile_pixels() {
        let raster =
            common::make_raster(1, 1, 3, RasterData::F32(vec![1., 2., 3.]), common::NO_GEOTRANSFORM, Default::default(), None).unwrap();
        let mut tiff = super::super::gtiff_write::encode(&raster, &Default::default()).unwrap();
        assert_eq!(&tiff[..4], b"II*\0");
        let ifd = u32::from_le_bytes(tiff[4..8].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(tiff[ifd..ifd + 2].try_into().unwrap()) as usize;
        let entry = (0..count)
            .map(|i| ifd + 2 + i * 12)
            .find(|&i| matches!(u16::from_le_bytes(tiff[i..i + 2].try_into().unwrap()), 273 | 324))
            .unwrap();
        assert_eq!(u32::from_le_bytes(tiff[entry + 4..entry + 8].try_into().unwrap()), 1);
        let past_end = (tiff.len() as u32 + 1024).to_le_bytes();
        tiff[entry + 8..entry + 12].copy_from_slice(&past_end);
        assert!(tiff::decoder::Decoder::new(std::io::Cursor::new(&tiff)).is_ok());

        let rgb = encode_png(1, 1, 3, false, &[1, 2, 3]).unwrap();
        let mut palette = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut palette, 1, 1);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(vec![1, 2, 3]);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0]).unwrap();
            writer.finish().unwrap();
        }
        let corrupt_png = |mut bytes: Vec<u8>| {
            let idat = bytes.windows(4).position(|b| b == b"IDAT").unwrap();
            bytes[idat + 4] ^= 0xff;
            assert!(png::Decoder::new(std::io::Cursor::new(&bytes)).read_info().is_ok());
            bytes
        };
        for tile in [tiff, corrupt_png(rgb), corrupt_png(palette)] {
            let mut builder = super::super::vector::geopackage::Builder::new();
            let extent = [0., 0., 1024., 1024.];
            let ids = builder
                .add_tiles("grid", "2d-gridded-coverage", 0, extent, extent, &[(0, 1024, 1024, 1, 1, 1., 1.)], vec![(0, 0, 0, tile)])
                .unwrap();
            builder.add_coverage("grid", ("float", 1., 0., None), vec![(ids[0], 1., 0., [None; 4])]);
            let db = Db::new(builder.finish().unwrap()).unwrap();
            crate::config::with_options(&[("TERRIO_MAX_BANDS", ""), ("TERRIO_MAX_DECLARED_BYTES", "")], || {
                assert!(matches!(read_db(&db, Some("grid"), Path::new("coverage.gpkg")), Err(Error::InvalidData(_))));
                for (key, value) in [("TERRIO_MAX_BANDS", "1"), ("TERRIO_MAX_DECLARED_BYTES", "8388608")] {
                    crate::config::with_options(&[(key, value)], || {
                        let result = read_db(&db, Some("grid"), Path::new("coverage.gpkg")).map(|_| ());
                        assert!(matches!(&result, Err(Error::ResourceLimit(message)) if message.contains(key)), "{result:?}");
                    });
                }
            });
        }
    }

    /// Reference: GDAL 3.13.3. A coverage tile supplies the real count before the grid is allocated.
    #[test]
    fn coverage_limits_count_multiband_tiles_before_allocating_the_grid() {
        let raster =
            common::make_raster(1, 1, 3, RasterData::F32(vec![1., 2., 3.]), common::NO_GEOTRANSFORM, Default::default(), None).unwrap();
        let tile = super::super::gtiff_write::encode(&raster, &Default::default()).unwrap();
        let mut builder = super::super::vector::geopackage::Builder::new();
        let extent = [0., 0., 1024., 1024.];
        let ids = builder
            .add_tiles("grid", "2d-gridded-coverage", 0, extent, extent, &[(0, 1024, 1024, 1, 1, 1., 1.)], vec![(0, 0, 0, tile)])
            .unwrap();
        builder.add_coverage("grid", ("float", 1., 0., None), vec![(ids[0], 1., 0., [None; 4])]);
        let db = Db::new(builder.finish().unwrap()).unwrap();
        for (key, value) in [("TERRIO_MAX_BANDS", "1"), ("TERRIO_MAX_DECLARED_BYTES", "8388608")] {
            crate::config::with_options(&[(key, value)], || {
                let result = read_db(&db, Some("grid"), Path::new("coverage.gpkg"));
                assert!(matches!(result, Err(Error::ResourceLimit(_))), "{result:?}");
            });
        }
    }

    #[test]
    fn optional_raster_metadata_distinguishes_absence_from_corrupt_records() {
        use super::super::vector::sqlite as writer;
        for (data_type, name) in
            [(DataType::U8, "gpkg_metadata"), (DataType::U8, "gpkg_metadata_reference"), (DataType::U16, "gpkg_2d_gridded_tile_ancillary")]
        {
            let cfg = wbraster::RasterConfig { cols: 1, rows: 1, bands: 1, data_type, ..Default::default() };
            let raster = wbraster::Raster::from_data_native(cfg, RasterData::from_f64_vec(data_type, vec![7.0])).unwrap();
            let bytes = to_bytes(&raster, Some(0.0), "grid", None).unwrap();
            let db = Db::new(bytes.clone()).unwrap();
            let control = read_db(&db, Some("grid"), Path::new("raster.gpkg")).unwrap();
            assert_eq!(control.raster.data.to_f64_vec(), vec![7.0]);
            let schema = db.schema().unwrap();
            let root = schema.iter().find(|e| e.0 == "table" && e.1 == name).unwrap().3;
            let stored = db.stored_rows(root).unwrap();
            assert!(!stored.is_empty(), "{name}");
            let convert = |v: Value| match v {
                Value::Null => writer::Value::Null,
                Value::Int(v) => writer::Value::Int(v),
                Value::Real(v) => writer::Value::Real(v),
                Value::Text(v) => writer::Value::Text(v),
                Value::Blob(v) => writer::Value::Blob(v),
            };
            let values = stored[0].1.clone().into_iter().map(convert).collect::<Vec<_>>();
            let record = writer::record(&values);
            assert!(record[0] < 128 && record[1] < 128);
            let at = bytes.windows(record.len()).position(|b| b == record).unwrap();
            let mut corrupt = bytes.clone();
            corrupt[at + 1] = 10; // Reserved serial type: the table exists but its record is malformed.
            assert!(read_db(&Db::new(corrupt).unwrap(), Some("grid"), Path::new("raster.gpkg")).is_err(), "{name}");
            // Rebuild the same image with only the optional table absent.
            let tables = schema
                .into_iter()
                .filter(|e| e.0 == "table" && e.1 != name)
                .map(|e| writer::Table {
                    rows: db.stored_rows(e.3).unwrap().into_iter().map(|(id, row)| (id, row.into_iter().map(convert).collect())).collect(),
                    autoindexes: super::super::sqlite::autoindex_columns(&e.4),
                    name: e.1,
                    sql: e.4,
                })
                .collect::<Vec<_>>();
            let absent = writer::write_database(&tables, &[], 10400, 0x47504b47).unwrap();
            let fallback = read_db(&Db::new(absent).unwrap(), Some("grid"), Path::new("raster.gpkg")).unwrap();
            if name == "gpkg_metadata" {
                assert_eq!(fallback.raster.bands, 4);
                assert_eq!(fallback.nodata(), None);
            } else {
                assert_eq!(fallback.raster.data.to_f64_vec(), vec![7.0]);
            }
        }
    }

    #[test]
    fn malformed_srs_records_do_not_silently_drop_raster_crs() {
        use super::super::vector::sqlite as writer;
        let values = vec![
            writer::Value::Int(4326),
            writer::Value::Text("EPSG".into()),
            writer::Value::Int(4326),
            writer::Value::Text("broken".into()),
        ];
        let record = writer::record(&values);
        let table = writer::Table {
            name: "gpkg_spatial_ref_sys".into(),
            sql: "CREATE TABLE gpkg_spatial_ref_sys(srs_id INTEGER,organization TEXT,organization_coordsys_id INTEGER,definition TEXT)"
                .into(),
            rows: vec![(1, values)],
            autoindexes: vec![],
        };
        let mut bytes = writer::write_database(&[table], &[], 0, 0).unwrap();
        let at = bytes.windows(record.len()).position(|b| b == record).unwrap();
        bytes[at + record[0] as usize - 1] += 2;
        assert!(crs_for_srs(&Db::new(bytes).unwrap(), 4326).is_err());
        let absent = Db::new(writer::write_database(&[], &[], 0, 0).unwrap()).unwrap();
        assert!(crs_for_srs(&absent, 4326).unwrap().is_none());
    }

    #[test]
    fn layers_and_rasters_in_one_file() {
        let mut layer = wbvector::Layer::new("points").with_epsg(4326);
        layer.add_feature(Some(wbvector::Geometry::point(1.0, 2.0)), &[]).unwrap();
        let cfg = wbraster::RasterConfig { cols: 300, rows: 2, bands: 1, data_type: DataType::U16, cell_size: 0.5, ..Default::default() };
        let data = RasterData::from_f64_vec(DataType::U16, (0..600).map(f64::from).collect());
        let raster = wbraster::Raster::from_data_native(cfg, data).unwrap();
        let mut b = crate::drivers::vector::geopackage::Builder::new();
        b.add_layer(&layer).unwrap();
        add_raster(&mut b, &raster, None, "grid", None).unwrap();
        let bytes = b.finish().unwrap();
        let db = Db::new(bytes.clone()).unwrap();
        let (layers, rasters) = crate::drivers::vector::gpkg_read::contents(&db).unwrap();
        assert_eq!((layers.len(), rasters.len()), (1, 1));
        // Opens as its layers; the raster by its table.
        let crate::Dataset::Vector(v) = open_bytes(bytes.clone(), "m.gpkg", &Default::default()).unwrap() else { panic!() };
        assert_eq!(v.layers[0].name, "points");
        let r = read_bytes(bytes, Some("grid")).unwrap();
        assert_eq!((r.raster.cols, r.raster.rows, r.raster.data_type), (300, 2, DataType::U16));
        assert_eq!(r.raster.data.to_f64_vec(), raster.data.to_f64_vec());
        assert_eq!(parse_subdataset("GPKG:C:\\x.gpkg:grid"), Some(("C:\\x.gpkg".into(), "grid".into())));
    }

    /// `skip_features` and `max_features` keep that range of each layer's features, or of the
    /// layer named, in the table's order, counted after the spatial filter when there is one
    /// (#790): for a file, an archive member and bytes, as `memory::open_vector_bytes` and the
    /// in-memory codecs read them.
    #[test]
    fn a_feature_range_keeps_those_features_of_each_layer() {
        use std::io::Write as _;
        let points = |name: &str, n: u32| {
            let mut layer = wbvector::Layer::new(name).with_epsg(4326);
            for i in 0..n {
                layer.add_feature(Some(wbvector::Geometry::point(f64::from(i), 0.0)), &[]).unwrap();
            }
            layer
        };
        let mut b = crate::drivers::vector::geopackage::Builder::new();
        b.add_layer(&points("five", 5)).unwrap();
        b.add_layer(&points("three", 3)).unwrap();
        let bytes = b.finish().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("points.gpkg");
        std::fs::write(&path, &bytes).unwrap();
        let path = path.to_str().unwrap();
        let archive = dir.path().join("points.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        zip.start_file("points.gpkg", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(&bytes).unwrap();
        zip.finish().unwrap();
        let member = format!("/vsizip/{}/points.gpkg", archive.to_str().unwrap());
        // The x of each layer's points.
        let xs = |layers: &[wbvector::Layer]| {
            let x = |f: &wbvector::Feature| match &f.geometry {
                Some(wbvector::Geometry::Point(c)) => c.x,
                other => panic!("not a point: {other:?}"),
            };
            layers.iter().map(|l| (l.name.clone(), l.features.iter().map(x).collect::<Vec<_>>())).collect::<Vec<_>>()
        };
        let vector = |d: crate::Dataset| match d {
            crate::Dataset::Vector(v) => xs(&v.layers),
            crate::Dataset::Raster(_) => panic!("a raster"),
        };
        type Read<'a> = &'a dyn Fn(&crate::VectorOpenOptions) -> Vec<(String, Vec<f64>)>;
        let reads: [(&str, Read); 5] = [
            ("a file", &|o: &crate::VectorOpenOptions| xs(&crate::open_vector(path, o).unwrap().layers)),
            ("an archive member", &|o: &crate::VectorOpenOptions| xs(&crate::open_vector(&member, o).unwrap().layers)),
            ("open_vector_bytes", &|o: &crate::VectorOpenOptions| {
                xs(&crate::memory::open_vector_bytes(bytes.clone(), "points.gpkg", o).unwrap().layers)
            }),
            ("the in-memory codecs", &|o: &crate::VectorOpenOptions| {
                vector(crate::memory::codec::open(bytes.clone(), "points.gpkg", o).unwrap())
            }),
            ("open_bytes", &|o: &crate::VectorOpenOptions| vector(open_bytes(bytes.clone(), "points.gpkg", o).unwrap())),
        ];
        // (layers named, skip_features, max_features, spatial_filter) and the x each layer read
        // keeps.
        type Case = (&'static [&'static str], usize, Option<usize>, Option<crate::drivers::vector::gpkg_read::Rect>, Kept);
        type Kept = &'static [(&'static str, &'static [f64])];
        let middle = Some((0.5, -1.0, 3.5, 1.0));
        let cases: [Case; 9] = [
            (&[], 1, Some(2), None, &[("five", &[1.0, 2.0]), ("three", &[1.0, 2.0])]),
            (&[], 4, None, None, &[("five", &[4.0]), ("three", &[])]),
            (&[], 0, Some(1), None, &[("five", &[0.0]), ("three", &[0.0])]),
            (&[], 5, None, None, &[("five", &[]), ("three", &[])]),
            (&[], 0, None, None, &[("five", &[0.0, 1.0, 2.0, 3.0, 4.0]), ("three", &[0.0, 1.0, 2.0])]),
            (&[], 0, None, middle, &[("five", &[1.0, 2.0, 3.0]), ("three", &[1.0, 2.0])]),
            (&[], 1, Some(1), middle, &[("five", &[2.0]), ("three", &[2.0])]),
            (&[], 0, Some(1), middle, &[("five", &[1.0]), ("three", &[1.0])]),
            (&["three"], 1, Some(1), None, &[("three", &[1.0])]),
        ];
        let (mut got, mut want) = (Vec::new(), Vec::new());
        for (entry, read) in reads {
            for (layers, skip_features, max_features, spatial_filter, kept) in cases {
                let layers: Vec<String> = layers.iter().map(|l| l.to_string()).collect();
                let options =
                    crate::VectorOpenOptions { layers: layers.clone(), skip_features, max_features, spatial_filter, ..Default::default() };
                let case = (entry, layers, skip_features, max_features, spatial_filter.is_some());
                got.push((case.clone(), read(&options)));
                want.push((case, kept.iter().map(|(l, x)| (l.to_string(), x.to_vec())).collect::<Vec<_>>()));
            }
        }
        assert_eq!(got, want);
    }
}
