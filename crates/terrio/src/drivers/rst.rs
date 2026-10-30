// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/idrisi/IdrisiDataset.cpp
// No code is translated from these sources.

//! Idrisi / TerrSet rasters: a `.rst` data file with a `.rdc` text header.

use super::common::{self, Interleave, Layout, RawRaster};
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::{CrsInfo, DataType, Raster};

/// Opens an Idrisi raster through its `.rst` or `.rdc` file, reading only the `.rdc` header now:
/// its pixels are read by window.
pub(crate) fn open(path: &Path) -> Result<RawRaster> {
    let rdc = common::sibling(path, "rdc");
    let data = common::sibling(path, "rst");
    let text = std::fs::read_to_string(&rdc)?;
    let keys: Vec<(String, String)> =
        text.lines().filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))).collect();
    let get = |k: &str| keys.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let num = |k: &str| get(k).and_then(common::parse_f64);
    let int = |k: &str| get(k).and_then(|v| v.parse::<usize>().ok());
    let bad = |k: &str| Error::InvalidData(format!("{}: missing '{k}'", rdc.display()));
    let (data_type, bands) = match get("data type").unwrap_or_default().to_ascii_lowercase().as_str() {
        "byte" => (DataType::U8, 1),
        "integer" => (DataType::I16, 1),
        "real" => (DataType::F32, 1),
        "rgb24" => (DataType::U8, 3),
        t => return Err(Error::Unsupported(format!("{}: Idrisi data type '{t}' is not supported", rdc.display()))),
    };
    if get("file type").is_some_and(|t| !t.eq_ignore_ascii_case("binary")) {
        return Err(Error::Unsupported(format!("{}: only binary Idrisi files are supported", rdc.display())));
    }
    let (cols, rows) = (int("columns").ok_or_else(|| bad("columns"))?, int("rows").ok_or_else(|| bad("rows"))?);
    let layout = Layout { cols, rows, bands, data_type, little_endian: true, interleave: Interleave::Bip, offset: 0, bottom_up: false };
    let (x0, x1, y0, y1) = (
        num("min. x").unwrap_or(0.0),
        num("max. x").unwrap_or(cols as f64),
        num("min. y").unwrap_or(0.0),
        num("max. y").unwrap_or(rows as f64),
    );
    let gt = [x0, (x1 - x0) / cols as f64, 0.0, y1, 0.0, -(y1 - y0) / rows as f64];
    let flag_def = get("flag def'n").unwrap_or_default().to_ascii_lowercase();
    let nodata = (flag_def == "missing data" || flag_def == "background").then(|| num("flag value")).flatten();
    let refsys = get("ref. system").unwrap_or_default();
    let crs = crs_from_ref(refsys, path)
        .or_else(|| {
            // GDAL writes the projected CRSs it cannot name with an empty reference system, and
            // reads them back as a local CRS in the reference units.
            let metres = get("ref. units").is_some_and(|u| matches!(u.to_ascii_lowercase().as_str(), "m" | "meter" | "meters" | "metre"));
            (refsys.is_empty() && metres).then(|| CrsInfo { epsg: None, wkt: Some(LOCAL_METRES.to_string()), proj4: None })
        })
        .unwrap_or_default();
    let raster = common::raster_header(cols, rows, bands, data_type, gt, crs, nodata)?;
    let raw = RawRaster::open(RasterDataset::with_driver(raster, RasterDriver::RST, nodata.is_some()), &data, layout, &[&rdc])?;
    // RGB24 is stored blue, green, red by pixel.
    Ok(if bands == 3 { raw.with_stored_bands(vec![2, 1, 0]) } else { raw })
}

/// The local CRS GDAL reads for an unnamed Idrisi reference system in metres.
pub(crate) const LOCAL_METRES: &str = r#"LOCAL_CS["Unknown",UNIT["metre",1],AXIS["Easting",EAST],AXIS["Northing",NORTH]]"#;

/// The CRS of an Idrisi reference system: `latlong`, `utm-11n`, or a `.ref` file's datum.
fn crs_from_ref(name: &str, path: &Path) -> Option<CrsInfo> {
    let n = name.to_ascii_lowercase();
    if n == "latlong" {
        return Some(CrsInfo::from_epsg(4326));
    }
    if let Some(z) = n.strip_prefix("utm-") {
        let (zone, hemi) = z.split_at(z.len().checked_sub(1)?);
        let zone: u32 = zone.parse().ok()?;
        return Some(CrsInfo::from_epsg(if hemi == "s" { 32700 } else { 32600 } + zone));
    }
    if n.is_empty() || n == "plane" {
        return None;
    }
    // A `.ref` file next to the raster: geographic CRSs on the common datums.
    let text = std::fs::read_to_string(path.with_file_name(format!("{name}.ref"))).ok()?;
    let get = |k: &str| {
        text.lines()
            .find(|l| l.to_ascii_lowercase().starts_with(k))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_ascii_lowercase())
    };
    if get("projection").as_deref() != Some("none") {
        return None;
    }
    let code = match get("datum")?.as_str() {
        "north_american_datum_1983" | "nad83" => 4269,
        "north_american_datum_1927" | "nad27" => 4267,
        "wgs_1984" | "wgs84" => 4326,
        _ => return None,
    };
    Some(CrsInfo::from_epsg(code))
}

/// Writes an Idrisi raster: `.rst` and `.rdc`. Byte, Int16 and Float32 are stored as they are;
/// other types become Int16 (Int8) or Float32.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    let (to, name) = match (r.data_type, r.bands) {
        (DataType::U8, 1) => (DataType::U8, "byte"),
        (DataType::U8, 3) => (DataType::U8, "RGB24"),
        (_, 1) if matches!(r.data_type, DataType::I16 | DataType::I8) => (DataType::I16, "integer"),
        (_, 1) => (DataType::F32, "real"),
        (_, n) => return Err(Error::Unsupported(format!("Idrisi rasters hold 1 band (or 3 Byte bands), not {n}"))),
    };
    let mut r = common::convert(r, to);
    if r.bands == 3
        && let wbraster::raster::RasterData::U8(v) = &mut r.data
    {
        let n = r.cols * r.rows;
        let (red, rest) = v.split_at_mut(n);
        red.swap_with_slice(&mut rest[n..]);
    }
    let layout = Layout {
        cols: r.cols,
        rows: r.rows,
        bands: r.bands,
        data_type: to,
        little_endian: true,
        interleave: Interleave::Bip,
        offset: 0,
        bottom_up: false,
    };
    let data = path.with_extension("rst");
    std::fs::write(&data, common::encode(&r, &layout))?;
    let crs = super::common::spatial_ref(&r.crs);
    let (refsys, units, pam) = match crs.epsg {
        Some(4326) => ("latlong".to_string(), "deg", false),
        Some(c @ 32601..=32660) => (format!("utm-{}n", c - 32600), "m", false),
        Some(c @ 32701..=32760) => (format!("utm-{}s", c - 32700), "m", false),
        _ => ("plane".to_string(), if crs.is_angular() { "deg" } else { "m" }, !crs.is_unknown()),
    };
    let values = r.data.to_f64_vec();
    let valid = values.iter().copied().filter(|v| !v.is_nan() && nodata.is_none_or(|nd| *v != nd));
    let (lo, hi) = valid.fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(v), b.max(v)));
    let (lo, hi) = if lo.is_finite() { (lo, hi) } else { (0.0, 0.0) };
    let mut h = format!(
        "file format : Idrisi Raster A.1\nfile title  : \ndata type   : {name}\nfile type   : binary\ncolumns     : {}\nrows        : {}\nref. system : {refsys}\nref. units  : {units}\nunit dist.  : 1\nmin. X      : {}\nmax. X      : {}\nmin. Y      : {}\nmax. Y      : {}\npos'n error : unspecified\nresolution  : {}\nmin. value  : {lo}\nmax. value  : {hi}\ndisplay min : {lo}\ndisplay max : {hi}\nvalue units : unspecified\nvalue error : unspecified\n",
        r.cols,
        r.rows,
        r.x_min,
        r.x_max(),
        r.y_min,
        r.y_max(),
        r.cell_size_x
    );
    match nodata {
        Some(nd) => h.push_str(&format!("flag value  : {}\nflag def'n  : missing data\n", common::format_value(nd, to))),
        None => h.push_str("flag value  : none\nflag def'n  : none\n"),
    }
    h.push_str("legend cats : 0\nlineage     : \ncomment     : \n");
    std::fs::write(path.with_extension("rdc"), h)?;
    if pam {
        common::write_pam(&data, &r.crs)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use wbraster::raster::RasterData;

    /// A 4 x 3 RGB24 raster as GDAL's RST driver writes it: blue, green and red by pixel, with
    /// red `10 + i`, green `100 + i` and blue `200 + i` for pixel `i`.
    fn write_rgb24(dir: &std::path::Path) -> std::path::PathBuf {
        let rdc = "file format : Idrisi Raster A.1\ndata type   : rgb24\nfile type   : binary\ncolumns     : 4\nrows        : 3\n\
                   ref. system : plane\nref. units  : m\nmin. X      : 0.0000000\nmax. X      : 4.0000000\nmin. Y      : 0.0000000\n\
                   max. Y      : 3.0000000\nflag value  : none\nflag def'n  : none\n";
        std::fs::write(dir.join("rgb.rdc"), rdc).unwrap();
        let bytes: Vec<u8> = (0..12u8).flat_map(|i| [200 + i, 100 + i, 10 + i]).collect();
        let rst = dir.join("rgb.rst");
        std::fs::write(&rst, bytes).unwrap();
        rst
    }

    #[test]
    fn reads_rgb24_bands_red_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_rgb24(dir.path());
        let path = path.to_str().unwrap();
        // The values GDAL 3.13 reads: band 1 red, band 2 green, band 3 blue.
        let band = |base: u8| -> Vec<u8> { (0..12).map(|i| base + i).collect() };
        let whole = crate::open_raster(path).unwrap();
        assert_eq!(whole.raster.bands, 3);
        let RasterData::U8(v) = &whole.raster.data else { panic!() };
        assert_eq!(v, &[band(10), band(100), band(200)].concat());

        let mut lazy = crate::open_raster_lazy(path).unwrap();
        assert!(lazy.dataset().is_none());
        let RasterData::U8(all) = lazy.read().unwrap().raster.data else { panic!() };
        assert_eq!(&all, v);
        // A window of some bands, in any order: rows 1 and 2, columns 1 to 3.
        for bands in [vec![1], vec![3], vec![3, 1], vec![2, 3, 1]] {
            let options = crate::WindowOptions { bands: bands.clone(), overview: None };
            let RasterData::U8(got) = lazy.read_window(crate::Window::new(1, 1, 3, 2), &options).unwrap().raster.data else { panic!() };
            let want: Vec<u8> = bands.iter().flat_map(|b| [5u8, 6, 7, 9, 10, 11].map(|i| [10, 100, 200][b - 1] + i)).collect();
            assert_eq!(got, want, "{bands:?}");
        }
    }
}
