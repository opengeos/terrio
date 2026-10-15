//! SAGA binary grids: a `.sdat` data file and a `.sgrd` header (with a `.prj` CRS).

use super::common::{self, Interleave, Layout};
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::{DataType, Raster};

/// Reads a SAGA grid through its `.sdat` or `.sgrd` file.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let header = common::sibling(path, "sgrd");
    let data = common::sibling(path, "sdat");
    let text = std::fs::read_to_string(&header)?;
    let keys: Vec<(String, String)> =
        text.lines().filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_ascii_uppercase(), v.trim().to_string()))).collect();
    let get = |k: &str| keys.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let num = |k: &str| get(k).and_then(common::parse_f64);
    let int = |k: &str| get(k).and_then(|v| v.parse::<usize>().ok());
    let bad = |k: &str| Error::InvalidData(format!("{}: missing '{k}'", header.display()));
    let data_type = match get("DATAFORMAT").ok_or_else(|| bad("DATAFORMAT"))? {
        "BIT" | "BYTE_UNSIGNED" | "BYTE" => DataType::U8,
        "SHORTINT_UNSIGNED" => DataType::U16,
        "SHORTINT" => DataType::I16,
        "INTEGER_UNSIGNED" => DataType::U32,
        "INTEGER" => DataType::I32,
        "FLOAT" => DataType::F32,
        "DOUBLE" => DataType::F64,
        f => return Err(Error::Unsupported(format!("{}: SAGA data format {f} is not supported", header.display()))),
    };
    let (cols, rows) = (int("CELLCOUNT_X").ok_or_else(|| bad("CELLCOUNT_X"))?, int("CELLCOUNT_Y").ok_or_else(|| bad("CELLCOUNT_Y"))?);
    let layout = Layout {
        cols,
        rows,
        bands: 1,
        data_type,
        little_endian: !get("BYTEORDER_BIG").is_some_and(|v| v.eq_ignore_ascii_case("TRUE")),
        interleave: Interleave::Bsq,
        offset: int("DATAFILE_OFFSET").unwrap_or(0),
        bottom_up: !get("TOPTOBOTTOM").is_some_and(|v| v.eq_ignore_ascii_case("TRUE")),
    };
    let pixels = common::decode(&std::fs::read(&data)?, &layout)?;
    let cell = num("CELLSIZE").ok_or_else(|| bad("CELLSIZE"))?;
    let (xmin, ymin) = (num("POSITION_XMIN").unwrap_or(0.0), num("POSITION_YMIN").unwrap_or(0.0));
    // POSITION_XMIN/YMIN are the centre of the lower-left cell.
    let gt = [xmin - cell / 2.0, cell, 0.0, ymin - cell / 2.0 + cell * rows as f64, 0.0, -cell];
    let nodata = num("NODATA_VALUE");
    let crs = common::read_prj(&data).unwrap_or_default();
    let raster = common::make_raster(cols, rows, 1, pixels, gt, crs, nodata)?;
    Ok(RasterDataset { raster, driver: RasterDriver::SAGA, has_nodata: nodata.is_some(), affine: None })
}

/// Writes a SAGA grid: `.sdat`, `.sgrd` and `.prj`, bottom-up as GDAL writes them.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    if r.bands != 1 {
        return Err(Error::Unsupported(format!("SAGA grids hold one band, not {}", r.bands)));
    }
    if (r.cell_size_x - r.cell_size_y).abs() > 1e-10 * r.cell_size_x.abs() {
        return Err(Error::Unsupported("SAGA grids need square cells".into()));
    }
    let widened;
    let r = match r.data_type {
        DataType::I8 => {
            widened = common::convert(r, DataType::I16);
            &widened
        }
        DataType::U64 | DataType::I64 => {
            widened = common::convert(r, DataType::F64);
            &widened
        }
        _ => r,
    };
    let format = match r.data_type {
        DataType::U8 => "BYTE_UNSIGNED",
        DataType::U16 => "SHORTINT_UNSIGNED",
        DataType::I16 => "SHORTINT",
        DataType::U32 => "INTEGER_UNSIGNED",
        DataType::I32 => "INTEGER",
        DataType::F32 => "FLOAT",
        _ => "DOUBLE",
    };
    let data = path.with_extension("sdat");
    let layout = Layout {
        cols: r.cols,
        rows: r.rows,
        bands: 1,
        data_type: r.data_type,
        little_endian: true,
        interleave: Interleave::Bsq,
        offset: 0,
        bottom_up: true,
    };
    std::fs::write(&data, common::encode(r, &layout))?;
    let name = data.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let nd = nodata.map(|v| common::format_value(v, r.data_type)).unwrap_or_else(|| "-99999".into());
    let h = format!(
        "NAME\t= {name}\nDESCRIPTION\t=\nUNIT\t=\nDATAFILE_OFFSET\t= 0\nDATAFORMAT\t= {format}\nBYTEORDER_BIG\t= FALSE\nPOSITION_XMIN\t= {}\nPOSITION_YMIN\t= {}\nCELLCOUNT_X\t= {}\nCELLCOUNT_Y\t= {}\nCELLSIZE\t= {}\nZ_FACTOR\t= 1.000000\nNODATA_VALUE\t= {nd}\nTOPTOBOTTOM\t= FALSE\n",
        r.x_min + r.cell_size_x / 2.0,
        r.y_min + r.cell_size_y / 2.0,
        r.cols,
        r.rows,
        r.cell_size_x
    );
    std::fs::write(path.with_extension("sgrd"), h)?;
    common::write_prj(&data, &r.crs)
}
