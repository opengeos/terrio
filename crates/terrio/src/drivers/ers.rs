// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/ers/ersdataset.cpp
// No code is translated from these sources.

//! ER Mapper rasters: a `.ers` text header and a band-interleaved-by-line data file.

use super::common::{self, Interleave, Layout, RawRaster};
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::{Path, PathBuf};
use wbraster::{CrsInfo, DataType, Raster};

/// Parses an `.ers` header into dotted keys (`RasterInfo.CellInfo.Xdimension`), without the
/// outer `DatasetHeader` block, and unquoted values.
fn parse(text: &str) -> Vec<(String, String)> {
    let mut path: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_suffix(" Begin").or_else(|| line.strip_suffix("\tBegin")) {
            path.push(name.trim().to_string());
        } else if line.ends_with(" End") || line.ends_with("\tEnd") {
            path.pop();
        } else if let Some((k, v)) = line.split_once('=') {
            let mut key: Vec<&str> = path.iter().skip(1).map(String::as_str).collect();
            key.push(k.trim());
            out.push((key.join(".").to_ascii_lowercase(), v.trim().trim_matches('"').to_string()));
        }
    }
    out
}

/// Converts ER Mapper's `deg:min:sec` notation to decimal degrees.
fn dms(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return s.parse().ok();
    }
    let (d, m, sec): (f64, f64, f64) = (parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?);
    let sign = if parts[0].trim_start().starts_with('-') { -1.0 } else { 1.0 };
    Some(sign * (d.abs() + m / 60.0 + sec / 3600.0))
}

/// The CRS of an ER Mapper projection and datum name, for the names GDAL writes.
fn crs_from_erm(projection: &str, datum: &str) -> Option<CrsInfo> {
    let p = projection.to_ascii_uppercase();
    let d = datum.to_ascii_uppercase();
    if let Some(code) = p.strip_prefix("EPSG:").and_then(|c| c.parse().ok()) {
        return Some(CrsInfo::from_epsg(code));
    }
    if p == "GEODETIC" {
        let code = match d.as_str() {
            "WGS84" => 4326,
            "NAD83" => 4269,
            "NAD27" => 4267,
            "GDA94" => 4283,
            "WGS72DOD" => 4322,
            _ => return d.strip_prefix("EPSG:").and_then(|c| c.parse().ok()).map(CrsInfo::from_epsg),
        };
        return Some(CrsInfo::from_epsg(code));
    }
    let utm = |prefix: &str| p.strip_prefix(prefix).and_then(|z| z.parse::<u32>().ok()).filter(|z| (1..=60).contains(z));
    if let Some(z) = utm("NUTM") {
        return Some(CrsInfo::from_epsg(match d.as_str() {
            "NAD83" => 26900 + z,
            "NAD27" => 26700 + z,
            _ => 32600 + z,
        }));
    }
    if let Some(z) = utm("SUTM") {
        return Some(CrsInfo::from_epsg(32700 + z));
    }
    None
}

/// Opens an ER Mapper raster through its `.ers` header, reading only the header now: its pixels
/// are read by window.
pub(crate) fn open(path: &Path) -> Result<RawRaster> {
    let h = parse(&std::fs::read_to_string(path)?);
    let get = |k: &str| h.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str());
    let int = |k: &str| get(k).and_then(|v| v.parse::<usize>().ok());
    let bad = |k: &str| Error::InvalidData(format!("{}: missing '{k}'", path.display()));
    let data_type = match get("rasterinfo.celltype").ok_or_else(|| bad("CellType"))? {
        "Unsigned8BitInteger" => DataType::U8,
        "Signed8BitInteger" => DataType::I8,
        "Unsigned16BitInteger" => DataType::U16,
        "Signed16BitInteger" => DataType::I16,
        "Unsigned32BitInteger" => DataType::U32,
        "Signed32BitInteger" => DataType::I32,
        "IEEE4ByteReal" => DataType::F32,
        "IEEE8ByteReal" => DataType::F64,
        t => return Err(Error::Unsupported(format!("{}: cell type {t} is not supported", path.display()))),
    };
    let layout = Layout {
        cols: int("rasterinfo.nrofcellsperline").ok_or_else(|| bad("NrOfCellsPerLine"))?,
        rows: int("rasterinfo.nroflines").ok_or_else(|| bad("NrOfLines"))?,
        bands: int("rasterinfo.nrofbands").unwrap_or(1),
        data_type,
        little_endian: !get("byteorder").is_some_and(|b| b.eq_ignore_ascii_case("MSBFirst")),
        interleave: Interleave::Bil,
        offset: int("headeroffset").unwrap_or(0),
        bottom_up: false,
    };
    let data_file = match get("datafile") {
        Some(f) => path.with_file_name(f),
        None => data_path(path),
    };
    let f = |k: &str| get(k).and_then(common::parse_f64);
    let (xd, yd) = (f("rasterinfo.cellinfo.xdimension").unwrap_or(1.0), f("rasterinfo.cellinfo.ydimension").unwrap_or(1.0));
    let origin = match (f("rasterinfo.registrationcoord.eastings"), f("rasterinfo.registrationcoord.northings")) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => match (f("rasterinfo.registrationcoord.metersx"), f("rasterinfo.registrationcoord.metersy")) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => {
                get("rasterinfo.registrationcoord.longitude").and_then(dms).zip(get("rasterinfo.registrationcoord.latitude").and_then(dms))
            }
        },
    };
    let gt = match origin {
        Some((x, y)) => {
            let (cx, cy) = (f("rasterinfo.registrationcellx").unwrap_or(0.0), f("rasterinfo.registrationcelly").unwrap_or(0.0));
            [x - cx * xd, xd, 0.0, y + cy * yd, 0.0, -yd]
        }
        None => common::NO_GEOTRANSFORM,
    };
    let crs = crs_from_erm(get("coordinatespace.projection").unwrap_or("RAW"), get("coordinatespace.datum").unwrap_or("WGS84"))
        .unwrap_or_default();
    let nodata = f("rasterinfo.nullcellvalue");
    let raster = common::raster_header(layout.cols, layout.rows, layout.bands, data_type, gt, crs, nodata)?;
    RawRaster::open(RasterDataset::with_driver(raster, RasterDriver::ERS, nodata.is_some()), &data_file, layout, &[path])
}

/// The data file of a header: the header's path without its extension.
fn data_path(header: &Path) -> PathBuf {
    header.with_extension("")
}

/// Writes an ER Mapper raster: the header at `path` (`.ers`) and the data file next to it.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    let cell_type = match r.data_type {
        DataType::U8 => "Unsigned8BitInteger",
        DataType::I8 => "Signed8BitInteger",
        DataType::U16 => "Unsigned16BitInteger",
        DataType::I16 => "Signed16BitInteger",
        DataType::U32 => "Unsigned32BitInteger",
        DataType::I32 => "Signed32BitInteger",
        DataType::F32 => "IEEE4ByteReal",
        DataType::F64 => "IEEE8ByteReal",
        DataType::U64 | DataType::I64 => return Err(Error::Unsupported("ER Mapper cannot hold 64-bit integers".into())),
    };
    let layout = Layout {
        cols: r.cols,
        rows: r.rows,
        bands: r.bands,
        data_type: r.data_type,
        little_endian: true,
        interleave: Interleave::Bil,
        offset: 0,
        bottom_up: false,
    };
    let header = path.with_extension("ers");
    std::fs::write(data_path(&header), common::encode(r, &layout))?;
    let crs = super::common::spatial_ref(&r.crs);
    let (projection, datum) = match crs.epsg {
        Some(4326) => ("GEODETIC".to_string(), "WGS84".to_string()),
        Some(4269) => ("GEODETIC".to_string(), "NAD83".to_string()),
        Some(c @ 32601..=32660) => (format!("NUTM{:02}", c - 32600), "WGS84".to_string()),
        Some(c @ 32701..=32760) => (format!("SUTM{:02}", c - 32700), "WGS84".to_string()),
        Some(c) => (format!("EPSG:{c}"), format!("EPSG:{c}")),
        None => ("RAW".to_string(), "RAW".to_string()),
    };
    let name = header.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let mut h = format!(
        "DatasetHeader Begin\n\tVersion\t= \"6.0\"\n\tName\t= \"{name}\"\n\tDataSetType\t= ERStorage\n\tDataType\t= Raster\n\tByteOrder\t= LSBFirst\n\
         \tCoordinateSpace Begin\n\t\tDatum\t= \"{datum}\"\n\t\tProjection\t= \"{projection}\"\n\t\tCoordinateType\t= EN\n\t\tUnits\t= \"METERS\"\n\t\tRotation\t= 0:0:0.0\n\tCoordinateSpace End\n\
         \tRasterInfo Begin\n\t\tCellType\t= {cell_type}\n\t\tNrOfLines\t= {}\n\t\tNrOfCellsPerLine\t= {}\n\t\tNrOfBands\t= {}\n\
         \t\tCellInfo Begin\n\t\t\tXdimension\t= {}\n\t\t\tYdimension\t= {}\n\t\tCellInfo End\n\
         \t\tRegistrationCoord Begin\n\t\t\tEastings\t= {}\n\t\t\tNorthings\t= {}\n\t\tRegistrationCoord End\n",
        r.rows,
        r.cols,
        r.bands,
        r.cell_size_x,
        r.cell_size_y,
        r.x_min,
        r.y_max()
    );
    if let Some(nd) = nodata {
        h.push_str(&format!("\t\tNullCellValue\t= {}\n", common::format_value(nd, r.data_type)));
    }
    h.push_str("\tRasterInfo End\nDatasetHeader End\n");
    std::fs::write(&header, h)?;
    // A CRS that ER Mapper cannot name goes to a GDAL sidecar, as GDAL does.
    if projection == "RAW" {
        common::write_pam(&header, &r.crs)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_keys_and_dms() {
        let h = parse(
            "DatasetHeader Begin\n\tRasterInfo Begin\n\t\tCellInfo Begin\n\t\t\tXdimension\t= 30\n\t\tCellInfo End\n\tRasterInfo End\nDatasetHeader End\n",
        );
        assert_eq!(h, vec![("rasterinfo.cellinfo.xdimension".to_string(), "30".to_string())]);
        assert!((dms("-115:12:29.19").unwrap() - -115.208108).abs() < 1e-6);
        assert_eq!(crs_from_erm("NUTM11", "WGS84").unwrap().epsg, Some(32611));
    }
}
