//! GeoPackage rasters: tile pyramids (PNG or JPEG tiles) and 2D gridded coverages (16-bit PNG
//! or 32-bit float TIFF tiles), read at full resolution (the highest zoom level), over the extent
//! in `gpkg_contents`, as GDAL reads them; and written as GDAL writes them.
//!
//! Files holding vector layers and rasters open as their vector layers; their raster tables are
//! subdatasets, named as GDAL names them (`GPKG:file.gpkg:table`).

use super::sqlite::{Db, Table, Value};
use super::{common, nodata};
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
        let img = crate::jpeg::decode_file(b).map_err(|e| err(&e))?;
        return Ok((img.width, img.height, img.components, img.data.into_iter().map(f64::from).collect()));
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
    read_db(&Db::open(path)?, None, path)
}

/// Reads a raster table of a GeoPackage held in memory (the first when `table` is `None`).
pub(crate) fn read_bytes(bytes: Vec<u8>, table: Option<&str>) -> Result<RasterDataset> {
    read_db(&Db::new(bytes)?, table, Path::new("GeoPackage"))
}

/// Opens a GeoPackage: its vector layers when it has some, otherwise its first raster.
pub(crate) fn open(path: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    crate::crs::prefetch();
    open_bytes(crate::par::read_file(Path::new(path))?, path, options)
}

/// Opens a GeoPackage held in memory, `name` being its file name.
pub(crate) fn open_bytes(bytes: Vec<u8>, name: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    use crate::drivers::vector::gpkg_read;
    let db = Db::new(bytes)?;
    let (layers, rasters) = gpkg_read::contents(&db)?;
    if layers.is_empty() && !rasters.is_empty() {
        let mut ds = read_db(&db, None, Path::new(name))?;
        nodata::apply_pam(name, &mut ds);
        return Ok(crate::Dataset::Raster(ds));
    }
    let wanted: Vec<String> = options.layers.clone();
    let read = gpkg_read::read_db(&db, &wanted, options.spatial_filter)?;
    Ok(crate::Dataset::Vector(crate::dataset::finish_vector(read, name, crate::formats::VectorDriver::GPKG)))
}

/// The raster tables of a GeoPackage, as GDAL's subdatasets: `(name, description)`, the name
/// being `GPKG:<path>:<table>`.
pub(crate) fn subdatasets(path: &str) -> Vec<(String, String)> {
    let Ok(db) = std::fs::read(path).map_err(Error::from).and_then(Db::new) else { return Vec::new() };
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
fn read_db(db: &Db, table: Option<&str>, path: &Path) -> Result<RasterDataset> {
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
    let tile_anc = if gridded { db.table("gpkg_2d_gridded_tile_ancillary").ok() } else { None };

    // GDAL records the band count and nodata value of tile pyramids in gpkg_metadata; without
    // them, tiles are read as RGBA.
    let md = db.table("gpkg_metadata").ok();
    // The metadata attached to this table, when the references say which.
    let md_ids: Vec<i64> = db
        .table("gpkg_metadata_reference")
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
    let crs = srs_id.and_then(|id| crs_for_srs(db, id)).unwrap_or_default();
    let gt = [minx, rx, 0.0, maxy, 0.0, -ry];
    let nodata = if gridded { coverage.2 } else { tile_nodata };
    let raster = common::make_raster(cols, rows, bands, data, gt, crs, nodata)?;
    Ok(RasterDataset { raster, driver: RasterDriver::GPKG, has_nodata: nodata.is_some(), affine: None })
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
    std::fs::write(path, to_bytes(r, nodata, table, compression)?)?;
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

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
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
            xml.push_str(&format!("    <MDI key=\"{}\">{}</MDI>\n", escape_xml(k), escape_xml(v)));
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
}
