// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/raw/envidataset.cpp
// No code is translated from these sources.

//! ENVI labelled rasters: a raw data file with a `.hdr` text header. Opened through either file,
//! as GDAL opens the data file.

use super::common::{self, Interleave, Layout, RawRaster};
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::{Path, PathBuf};
use wbraster::{CrsInfo, DataType, Raster};

/// Extensions of ENVI data files, tried when opening through the header.
const DATA_EXTENSIONS: &[&str] = &["", "dat", "img", "bsq", "bil", "bip", "raw", "bin", "envi"];

/// Whether `path` is an ENVI header, or a data file with an ENVI header next to it.
pub(crate) fn is_envi(path: &Path) -> bool {
    // An Imagine file can sit next to the header of another raster with the same name.
    !super::hfa::is_hfa(path) && header_path(path).is_some()
}

/// The header of an ENVI data file (`name.hdr` or `name.ext.hdr`), or the path itself when it is
/// a header.
fn header_path(path: &Path) -> Option<PathBuf> {
    let is_header = |p: &Path| std::fs::read(p).ok().is_some_and(|b| b.starts_with(b"ENVI"));
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("hdr")) {
        return is_header(path).then(|| path.to_path_buf());
    }
    let mut appended = path.as_os_str().to_owned();
    appended.push(".hdr");
    [PathBuf::from(appended), common::sibling(path, "hdr")].into_iter().find(|p| p.is_file() && is_header(p))
}

/// The data file of an ENVI header.
fn data_path(header: &Path) -> Result<PathBuf> {
    let stem = header.with_extension("");
    // `name.ext.hdr` describes `name.ext`.
    if stem.extension().is_some() && stem.is_file() {
        return Ok(stem);
    }
    for ext in DATA_EXTENSIONS {
        for e in [ext.to_string(), ext.to_ascii_uppercase()] {
            let p = if e.is_empty() { stem.clone() } else { header.with_extension(&e) };
            if p.is_file() && p != header {
                return Ok(p);
            }
        }
    }
    Err(Error::InvalidData(format!("{}: no ENVI data file next to the header", header.display())))
}

/// Parses an ENVI header into lowercase keys and values, with `{...}` values joined across lines.
fn parse_header(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = text.lines().skip(1);
    while let Some(line) = lines.next() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let mut value = v.trim().to_string();
        if value.starts_with('{') {
            while !value.contains('}') {
                match lines.next() {
                    Some(l) => {
                        value.push('\n');
                        value.push_str(l);
                    }
                    None => break,
                }
            }
            value = value.trim_start_matches('{').trim().trim_end_matches('}').trim().to_string();
        }
        out.push((k.trim().to_ascii_lowercase(), value));
    }
    out
}

/// Opens an ENVI raster through its data file or its header, reading only the header now: its
/// pixels are read by window.
pub(crate) fn open(path: &Path) -> Result<RawRaster> {
    let header = header_path(path).ok_or_else(|| Error::InvalidData(format!("{}: no ENVI header", path.display())))?;
    let data = if header == path { data_path(&header)? } else { path.to_path_buf() };
    let h = parse_header(&std::fs::read_to_string(&header)?);
    let get = |k: &str| h.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let num = |k: &str| get(k).and_then(|v| v.trim().parse::<usize>().ok());
    let bad = |what: &str| Error::InvalidData(format!("{}: missing or invalid '{what}'", header.display()));
    let data_type = match num("data type").ok_or_else(|| bad("data type"))? {
        1 => DataType::U8,
        2 => DataType::I16,
        3 => DataType::I32,
        4 => DataType::F32,
        5 => DataType::F64,
        12 => DataType::U16,
        13 => DataType::U32,
        14 => DataType::I64,
        15 => DataType::U64,
        t => return Err(Error::Unsupported(format!("{}: ENVI data type {t} is not supported", header.display()))),
    };
    let interleave = match get("interleave").unwrap_or("bsq").to_ascii_lowercase().as_str() {
        "bil" => Interleave::Bil,
        "bip" => Interleave::Bip,
        _ => Interleave::Bsq,
    };
    let layout = Layout {
        cols: num("samples").ok_or_else(|| bad("samples"))?,
        rows: num("lines").ok_or_else(|| bad("lines"))?,
        bands: num("bands").unwrap_or(1),
        data_type,
        little_endian: num("byte order").unwrap_or(0) == 0,
        interleave,
        offset: num("header offset").unwrap_or(0),
        bottom_up: false,
    };
    let nodata = get("data ignore value").and_then(common::parse_f64);
    let (gt, map_crs) = get("map info").and_then(parse_map_info).unwrap_or((common::NO_GEOTRANSFORM, None));
    let crs = get("coordinate system string").and_then(common::crs_from_text).or(map_crs).unwrap_or_default();
    let mut raster = common::raster_header(layout.cols, layout.rows, layout.bands, data_type, gt, crs, nodata)?;
    if let Some(names) = get("band names") {
        for (i, name) in names.split(',').map(str::trim).filter(|n| !n.is_empty()).enumerate() {
            raster.metadata.push((format!("band_{}_name", i + 1), name.to_string()));
        }
    }
    RawRaster::open(RasterDataset::with_driver(raster, RasterDriver::ENVI, nodata.is_some()), &data, layout, &[&header])
}

/// The geotransform and (for UTM and geographic grids) CRS of a `map info` value.
fn parse_map_info(v: &str) -> Option<([f64; 6], Option<CrsInfo>)> {
    let f: Vec<&str> = v.split(',').map(str::trim).collect();
    let n = |i: usize| f.get(i).and_then(|s| s.parse::<f64>().ok());
    let (rx, ry, x, y, sx, sy) = (n(1)?, n(2)?, n(3)?, n(4)?, n(5)?, n(6)?);
    let upside_down =
        f.iter().any(|s| s.strip_prefix("rotation=").and_then(|r| r.trim().parse::<f64>().ok()).is_some_and(|r| r.abs() == 180.0));
    let gt = if upside_down {
        [x - (rx - 1.0) * sx, sx, 0.0, y - (ry - 1.0) * sy, 0.0, sy]
    } else {
        [x - (rx - 1.0) * sx, sx, 0.0, y + (ry - 1.0) * sy, 0.0, -sy]
    };
    let datum = f
        .iter()
        .skip(7)
        .find(|s| !s.contains('=') && s.parse::<f64>().is_err() && !s.eq_ignore_ascii_case("north") && !s.eq_ignore_ascii_case("south"));
    let datum = datum.map(|d| d.to_ascii_lowercase()).unwrap_or_default();
    let projection = f[0].to_ascii_lowercase();
    let crs = if projection == "utm" {
        let zone = f.get(7).and_then(|s| s.parse::<u32>().ok())?;
        let south = f.get(8).is_some_and(|s| s.eq_ignore_ascii_case("south"));
        let code = match datum.as_str() {
            "north america 1983" if !south => 26900 + zone,
            "north america 1927" if !south => 26700 + zone,
            _ if south => 32700 + zone,
            _ => 32600 + zone,
        };
        Some(CrsInfo::from_epsg(code))
    } else if projection == "geographic lat/lon" {
        Some(CrsInfo::from_epsg(match datum.as_str() {
            "north america 1983" => 4269,
            "north america 1927" => 4267,
            "wgs-72" => 4322,
            _ => 4326,
        }))
    } else {
        None
    };
    Some((gt, crs))
}

/// Writes an ENVI raster: the data file at `path` and its header next to it (`.hdr`), as GDAL does.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    let (data_path, header) = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("hdr")) {
        (path.with_extension("dat"), path.to_path_buf())
    } else {
        (path.to_path_buf(), path.with_extension("hdr"))
    };
    // ENVI has no signed 8-bit type: widen it.
    let widened;
    let r = if r.data_type == DataType::I8 {
        widened = common::convert(r, DataType::I16);
        &widened
    } else {
        r
    };
    let code = match r.data_type {
        DataType::U8 => 1,
        DataType::I16 => 2,
        DataType::I32 => 3,
        DataType::F32 => 4,
        DataType::F64 => 5,
        DataType::U16 => 12,
        DataType::U32 => 13,
        DataType::I64 => 14,
        DataType::U64 => 15,
        DataType::I8 => 2,
    };
    let layout = Layout {
        cols: r.cols,
        rows: r.rows,
        bands: r.bands,
        data_type: r.data_type,
        little_endian: true,
        interleave: Interleave::Bsq,
        offset: 0,
        bottom_up: false,
    };
    std::fs::write(&data_path, common::encode(r, &layout))?;

    let name = data_path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let mut h = format!(
        "ENVI\ndescription = {{\n{name}}}\nsamples = {}\nlines   = {}\nbands   = {}\nheader offset = 0\nfile type = ENVI Standard\ndata type = {code}\ninterleave = bsq\nbyte order = 0\n",
        r.cols, r.rows, r.bands
    );
    let location = format!("1, 1, {}, {}, {}, {}", r.x_min, r.y_max(), r.cell_size_x, r.cell_size_y);
    let crs = super::common::spatial_ref(&r.crs);
    let utm = crs.epsg.and_then(|c| match c {
        32601..=32660 => Some((c - 32600, "North", "WGS-84")),
        32701..=32760 => Some((c - 32700, "South", "WGS-84")),
        26901..=26923 => Some((c - 26900, "North", "North America 1983")),
        26701..=26722 => Some((c - 26700, "North", "North America 1927")),
        _ => None,
    });
    if let Some((zone, hemisphere, datum)) = utm {
        h.push_str(&format!("map info = {{UTM, {location}, {zone}, {hemisphere}, {datum}}}\n"));
    } else if crs.epsg == Some(4326) {
        h.push_str(&format!("map info = {{Geographic Lat/Lon, {location}, WGS-84}}\n"));
    } else {
        h.push_str(&format!("map info = {{Arbitrary, {location}, 0, North}}\n"));
    }
    if let Some(wkt) = common::esri_wkt(&r.crs) {
        h.push_str(&format!("coordinate system string = {{{wkt}}}\n"));
    }
    if let Some(nd) = nodata {
        h.push_str(&format!("data ignore value = {}\n", common::format_value(nd, r.data_type)));
    }
    std::fs::write(header, h)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_map_info() {
        let (gt, crs) = parse_map_info("UTM, 1.000, 1.000, 500000.000, 4100000.000, 30, 30, 11, North, WGS-84, units=Meters").unwrap();
        assert_eq!(gt, [500000.0, 30.0, 0.0, 4100000.0, 0.0, -30.0]);
        assert_eq!(crs.unwrap().epsg, Some(32611));
        let (gt, _) = parse_map_info("Arbitrary, 2, 3, 100, 200, 10, 10, 0, North").unwrap();
        assert_eq!(gt, [90.0, 10.0, 0.0, 220.0, 0.0, -10.0]);
    }

    #[test]
    fn round_trips_through_the_data_file_and_header() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u16> = (0..12).collect();
        let r = common::make_raster(
            3,
            2,
            2,
            wbraster::raster::RasterData::U16(data.clone()),
            [500000.0, 30.0, 0.0, 4100000.0, 0.0, -30.0],
            CrsInfo::from_epsg(32611),
            None,
        )
        .unwrap();
        let p = dir.path().join("x.envi");
        write(&r, Some(7.0), &p).unwrap();
        for open in [p.clone(), dir.path().join("x.hdr")] {
            let ds = super::open(&open).and_then(common::RawRaster::read).unwrap();
            assert_eq!(ds.raster.data_u16().unwrap(), &data[..]);
            assert_eq!(ds.nodata(), Some(7.0));
            assert_eq!(ds.crs().epsg, Some(32611));
            assert_eq!(ds.geo_transform(), [500000.0, 30.0, 0.0, 4100000.0, 0.0, -30.0]);
        }
    }
}
