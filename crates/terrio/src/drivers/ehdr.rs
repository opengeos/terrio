// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/raw/ehdrdataset.cpp
// No code is translated from these sources.

//! Esri `.hdr` labelled rasters (EHdr): BIL, BIP and BSQ data files such as `.bil` and `.flt`,
//! with a `.hdr` header and a `.prj` CRS, read and written as GDAL does.

use super::common::{self, Interleave, Layout, RawRaster};
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::{DataType, Raster};

/// Whether `path` is a data file with an EHdr header (a `.hdr` next to it that is not ENVI's).
pub(crate) fn is_ehdr(path: &Path) -> bool {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("hdr")) {
        return false;
    }
    let hdr = common::sibling(path, "hdr");
    std::fs::read_to_string(hdr).ok().is_some_and(|t| {
        !t.starts_with("ENVI") && t.lines().any(|l| l.split_whitespace().next().is_some_and(|k| k.eq_ignore_ascii_case("ncols")))
    })
}

/// [`open`] for a data file `raw_opener` recognised by the EHdr header beside it, whatever driver
/// its name points to: refused when the process-wide allow-list leaves EHdr out, as a data file
/// named for another driver (`h.img`, Imagine's) reaches this point as that driver's file.
pub(crate) fn open_recognised(path: &Path) -> Result<RawRaster> {
    crate::formats::check_driver_allowed("EHdr", &path.to_string_lossy(), crate::formats::Kind::Raster)?;
    open(path)
}

/// Opens an EHdr data file, reading only its header now: its pixels are read by window.
pub(crate) fn open(path: &Path) -> Result<RawRaster> {
    let hdr = common::sibling(path, "hdr");
    let text = std::fs::read_to_string(&hdr).map_err(|e| Error::InvalidData(format!("{}: {e}", hdr.display())))?;
    let mut keys = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if let (Some(k), Some(v)) = (it.next(), it.next()) {
            keys.push((k.to_ascii_lowercase(), v.to_string()));
        }
    }
    let get = |k: &str| keys.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let num = |k: &str| get(k).and_then(common::parse_f64);
    let int = |k: &str| get(k).and_then(|v| v.parse::<usize>().ok());
    let bad = |k: &str| Error::InvalidData(format!("{}: missing '{k}'", hdr.display()));
    let (cols, rows) = (int("ncols").ok_or_else(|| bad("ncols"))?, int("nrows").ok_or_else(|| bad("nrows"))?);
    let bands = int("nbands").unwrap_or(1).max(1);
    let nodata = num("nodata").or_else(|| num("nodata_value"));

    // The data type, inferred as GDAL does when the header leaves it out.
    let mut pixel = get("pixeltype").and_then(|p| p.chars().next()).map(|c| c.to_ascii_uppercase()).unwrap_or('N');
    let mut bits = int("nbits");
    if bits.is_none() && pixel == 'N' {
        let size = std::fs::metadata(path)?.len() as usize;
        let per_pixel = size / (cols * rows * bands).max(1);
        if per_pixel > 0 && per_pixel != 3 && per_pixel <= 8 {
            bits = Some(per_pixel * 8);
        }
        if per_pixel == 4 {
            pixel = 'F';
        }
    }
    if pixel == 'N' && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("flt")) {
        pixel = 'F';
    }
    if pixel == 'N' && nodata.is_some_and(|n| n < 0.0) {
        pixel = 'S';
    }
    let data_type = match (bits, pixel) {
        (Some(16), 'S') => DataType::I16,
        (Some(16), _) => DataType::U16,
        (Some(32), 'S') => DataType::I32,
        (Some(32), 'F') => DataType::F32,
        (Some(32), _) => DataType::U32,
        (Some(8), 'S') => DataType::I8,
        (Some(8), _) => DataType::U8,
        (None, 'F') => DataType::F32,
        (None, _) => DataType::U8,
        (Some(b), _) => return Err(Error::Unsupported(format!("{}: NBITS {b} is not supported", hdr.display()))),
    };
    let interleave = match get("layout").map(str::to_ascii_uppercase).as_deref() {
        Some("BIP") => Interleave::Bip,
        Some("BSQ") => Interleave::Bsq,
        _ => Interleave::Bil,
    };
    let row_bytes = cols * data_type.size_bytes();
    let standard = int("bandrowbytes").is_none_or(|b| b == row_bytes)
        && int("totalrowbytes").is_none_or(|t| t == row_bytes * bands || interleave == Interleave::Bsq)
        && int("bandgapbytes").is_none_or(|g| g == 0);
    if !standard {
        return Err(Error::Unsupported(format!("{}: padded rows are not supported", hdr.display())));
    }
    let little = get("byteorder").and_then(|b| b.chars().next()).is_some_and(|c| matches!(c.to_ascii_uppercase(), 'I' | 'L'));
    let layout =
        Layout { cols, rows, bands, data_type, little_endian: little, interleave, offset: int("skipbytes").unwrap_or(0), bottom_up: false };

    // Georeferencing: ULXMAP/ULYMAP are pixel centres; xllcorner/yllcorner corners.
    let (xdim, ydim) = match num("cellsize") {
        Some(c) => (c, c),
        None => (num("xdim").unwrap_or(1.0), num("ydim").unwrap_or(1.0)),
    };
    let mut center = true;
    let mut ulx = 0.5;
    let mut uly = 0.5;
    for (k, v) in &keys {
        let Some(v) = common::parse_f64(v) else { continue };
        match k.as_str() {
            "ulxmap" | "xllcenter" => ulx = v,
            "xllcorner" => {
                ulx = v;
                center = false;
            }
            "ulymap" => uly = v,
            _ => {}
        }
    }
    let yll = keys.iter().find_map(|(k, v)| (k == "yllcorner" || k == "yllcenter").then(|| (k == "yllcorner", common::parse_f64(v))));
    if let Some((corner, Some(y))) = yll {
        if corner {
            center = false;
        }
        uly = if center { y + (rows as f64 - 1.0) * ydim } else { y + rows as f64 * ydim };
    }
    let gt = if ulx != 0.5 || uly != 0.5 || xdim != 1.0 || ydim != 1.0 {
        if center { [ulx - xdim * 0.5, xdim, 0.0, uly + ydim * 0.5, 0.0, -ydim] } else { [ulx, xdim, 0.0, uly, 0.0, -ydim] }
    } else {
        common::NO_GEOTRANSFORM
    };
    let crs = common::read_prj(path).unwrap_or_default();
    let raster = common::raster_header(cols, rows, bands, data_type, gt, crs, nodata)?;
    RawRaster::open(RasterDataset::with_driver(raster, RasterDriver::EHdr, nodata.is_some()), path, layout, &[&hdr])
}

/// Writes an EHdr raster: the BIL data file at `path`, a `.hdr` and a `.prj`.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    // EHdr holds up to 32-bit samples; 64-bit floats are narrowed, as for Esri `.flt` grids.
    let narrowed;
    let r = match r.data_type {
        DataType::F64 => {
            narrowed = common::convert(r, DataType::F32);
            &narrowed
        }
        DataType::U64 | DataType::I64 => return Err(Error::Unsupported("EHdr cannot hold 64-bit integers".into())),
        _ => r,
    };
    let size = r.data_type.size_bytes();
    let row_bytes = r.cols * size;
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
    std::fs::write(path, common::encode(r, &layout))?;
    let pixel_type = match r.data_type {
        DataType::F32 => "FLOAT",
        DataType::I8 | DataType::I16 | DataType::I32 => "SIGNEDINT",
        _ => "UNSIGNEDINT",
    };
    let mut h = format!(
        "BYTEORDER      I\nLAYOUT         BIL\nNROWS          {}\nNCOLS          {}\nNBANDS         {}\nNBITS          {}\nBANDROWBYTES   {row_bytes}\nTOTALROWBYTES  {}\nPIXELTYPE      {pixel_type}\n",
        r.rows,
        r.cols,
        r.bands,
        size * 8,
        row_bytes * r.bands
    );
    h.push_str(&format!(
        "ULXMAP         {}\nULYMAP         {}\nXDIM           {}\nYDIM           {}\n",
        r.x_min + r.cell_size_x * 0.5,
        r.y_max() - r.cell_size_y * 0.5,
        r.cell_size_x,
        r.cell_size_y
    ));
    if let Some(nd) = nodata {
        h.push_str(&format!("NODATA         {}\n", common::format_value(nd, r.data_type)));
    }
    std::fs::write(path.with_extension("hdr"), h)?;
    common::write_prj(path, &r.crs)
}
