//! Raster to XYZ text, as GDAL's `gdal2xyz.py`: a line per pixel with the X and Y of its center
//! and the values of the selected bands. The raster is read a strip of rows at a time and the
//! text written as it is made, so that neither is ever held whole.
//!
//! Lines and numbers are as `gdal2xyz.py` writes them: coordinates as `%.10g` when the raster
//! looks geographic (its origin and extent within 180 units) and `%.3f` otherwise; values as `%d`
//! when the first band is `Int32` or `UInt32`, else `%g`, converted to the first band's type.
//!
//! ```no_run
//! use terrio::ops::xyz::{XyzOptions, write_xyz};
//!
//! let mut dem = terrio::open_raster_lazy("dem.tif")?;
//! let mut out = std::io::BufWriter::new(std::fs::File::create("dem.xyz")?);
//! let lines = write_xyz(&mut dem, &mut out, &XyzOptions { skip_nodata: true, ..Default::default() })?;
//! println!("{lines} lines");
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::{Error, LazyRaster, Result, Window, WindowOptions};
use std::fmt::Write as _;
use std::io::Write;
use wbraster::DataType;

/// Options of [`write_xyz`], as `gdal2xyz.py`'s.
#[derive(Debug, Clone, PartialEq)]
pub struct XyzOptions {
    /// Bands to write, from 1 (`-b`). Empty means all bands (`-allbands`). The default is band 1,
    /// as `gdal2xyz.py`'s.
    pub bands: Vec<usize>,
    /// Write every `skip.0`-th column and every `skip.1`-th row (`skip`), from the window's first.
    pub skip: (usize, usize),
    /// The window `[x_offset, y_offset, width, height]` to write (`-srcwin`); the whole raster
    /// by default.
    pub srcwin: Option<[usize; 4]>,
    /// Leave out pixels whose values all equal the source nodata (`-skipnodata`).
    pub skip_nodata: bool,
    /// The source nodata value: one for all bands, or one per band (`-srcnodata`). By default,
    /// the bands' nodata values (when every band has one).
    pub src_nodata: Option<Vec<f64>>,
    /// Write these values instead of nodata pixels' (`-dstnodata`): one for all bands, or one per
    /// band. Ignored with `skip_nodata`, or without a source nodata.
    pub dst_nodata: Option<Vec<f64>>,
    /// The separator between values: a space, or a comma for `-csv`.
    pub delimiter: String,
}

impl Default for XyzOptions {
    fn default() -> Self {
        Self { bands: vec![1], skip: (1, 1), srcwin: None, skip_nodata: false, src_nodata: None, dst_nodata: None, delimiter: " ".into() }
    }
}

/// A value converted to a pixel type, as NumPy converts one into an array of it.
fn cast(v: f64, t: DataType) -> f64 {
    match t {
        DataType::F32 => v as f32 as f64,
        DataType::F64 => v,
        DataType::U8 => v as u8 as f64,
        DataType::I8 => v as i8 as f64,
        DataType::U16 => v as u16 as f64,
        DataType::I16 => v as i16 as f64,
        DataType::U32 => v as u32 as f64,
        DataType::I32 => v as i32 as f64,
        DataType::U64 => v as u64 as f64,
        DataType::I64 => v as i64 as f64,
    }
}

/// Per-band values from one (for all bands) or one per band.
fn per_band(values: &[f64], bands: usize, what: &str) -> Result<Vec<f64>> {
    match values.len() {
        1 => Ok(vec![values[0]; bands]),
        n if n == bands => Ok(values.to_vec()),
        n => Err(Error::InvalidArgument(format!("{what}: {n} values for {bands} bands (give one, or one per band)"))),
    }
}

/// Python's `"%g" % v`.
fn fmt_g(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    crate::drivers::common::fmt_g(v, 6)
}

/// Checks `options` against `raster` without reading pixels: what [`write_xyz`] would refuse, so
/// that a caller can check them before creating its output.
pub fn check_xyz(raster: &LazyRaster, options: &XyzOptions) -> Result<()> {
    if raster.dataset().is_some_and(|d| d.complex) {
        return Err(Error::Unsupported("writing complex rasters as XYZ is not supported".into()));
    }
    let n = if options.bands.is_empty() { raster.band_count() } else { options.bands.len() };
    if let Some(b) = options.bands.iter().find(|b| **b == 0 || **b > raster.band_count()) {
        return Err(Error::InvalidArgument(format!("band {b} does not exist (the raster has {})", raster.band_count())));
    }
    if options.skip.0 == 0 || options.skip.1 == 0 {
        return Err(Error::InvalidArgument("skip must be at least 1".into()));
    }
    let dt = raster.band_data_type(options.bands.first().copied().unwrap_or(1));
    let source_nodata = options.src_nodata.clone().or_else(|| {
        let bands: Vec<_> = if options.bands.is_empty() { (1..=raster.band_count()).collect() } else { options.bands.clone() };
        bands.iter().map(|b| raster.band_nodata(*b)).collect::<Option<Vec<_>>>()
    });
    if !matches!(dt, DataType::F32 | DataType::F64) && source_nodata.as_ref().is_some_and(|values| values.iter().any(|v| !v.is_finite())) {
        return Err(Error::InvalidArgument(
            "non-finite source nodata cannot be converted to the first selected integer band's type".into(),
        ));
    }
    let (w, h) = (raster.width(), raster.height());
    let [x_off, y_off, x_size, y_size] = options.srcwin.unwrap_or([0, 0, w, h]);
    if x_size == 0 || y_size == 0 || !Window::new(x_off, y_off, x_size, y_size).fits(w, h) {
        return Err(Error::InvalidArgument(format!("srcwin {x_off} {y_off} {x_size} {y_size} is outside the {w}x{h} raster")));
    }
    if let Some(v) = &options.src_nodata {
        per_band(v, n, "srcnodata")?;
    }
    if let Some(v) = &options.dst_nodata {
        per_band(v, n, "dstnodata")?;
    }
    Ok(())
}

/// Writes the XYZ lines of `raster` to `out`, returning the number of lines.
pub fn write_xyz<W: Write>(raster: &mut LazyRaster, out: &mut W, options: &XyzOptions) -> Result<u64> {
    check_xyz(raster, options)?;
    let bands: Vec<usize> = if options.bands.is_empty() { (1..=raster.band_count()).collect() } else { options.bands.clone() };
    let (sx, sy) = options.skip;
    let (w, h) = (raster.width(), raster.height());
    let [x_off, y_off, x_size, y_size] = options.srcwin.unwrap_or([0, 0, w, h]);
    let gt = raster.geo_transform();
    let geographic = gt[0].abs() < 180.0 && gt[3].abs() < 180.0 && (w as f64 * gt[1]).abs() < 180.0 && (h as f64 * gt[5]).abs() < 180.0;
    // Values are converted to the first band's type, as gdal2xyz.py's array of it holds them.
    let dt = bands.first().map_or_else(|| raster.data_type(), |b| raster.band_data_type(*b));
    let integer_format = matches!(dt, DataType::I32 | DataType::U32);
    let n = bands.len();
    let src_nodata: Option<Vec<f64>> = match &options.src_nodata {
        Some(v) => Some(per_band(v, n, "srcnodata")?),
        None => bands.iter().map(|b| raster.band_nodata(*b)).collect(),
    }
    .map(|v| v.into_iter().map(|x| cast(x, dt)).collect());
    let dst_nodata = match (&options.dst_nodata, &src_nodata) {
        (Some(v), Some(_)) => Some(per_band(v, n, "dstnodata")?.into_iter().map(|x| cast(x, dt)).collect::<Vec<_>>()),
        _ => None,
    };
    let skip_nodata = options.skip_nodata && src_nodata.is_some();
    let replace = !skip_nodata && dst_nodata.is_some();
    let delim = options.delimiter.as_str();
    let block = raster.block_size().map(|(_, bh)| bh.max(1));
    let rows = super::strips::small_strip_rows(x_size, n, 8, block);
    let read = WindowOptions { bands: bands.clone(), overview: None };
    let mut lines = 0u64;
    let mut text = String::new();
    let mut values = vec![0.0; n];
    let mut y = y_off;
    let end = y_off + y_size;
    while y < end {
        // Decode each needed physical block row once, while jumping over block
        // rows that contain no selected sample. Keep the strip memory budget.
        let count = match block {
            Some(block) if sy > 1 => rows.min(block - y % block).min(end - y),
            _ => rows.min(end - y),
        };
        let data = raster.read_window(Window::new(x_off, y, x_size, count), &read)?.raster.data;
        let plane = x_size * count;
        for r in 0..count {
            let row = y + r;
            if (row - y_off) % sy != 0 {
                continue;
            }
            text.clear();
            for c in (0..x_size).step_by(sx) {
                for (b, v) in values.iter_mut().enumerate() {
                    *v = cast(data.get_f64(b * plane + r * x_size + c), dt);
                }
                let mut shown: &[f64] = &values;
                if let Some(nd) = &src_nodata
                    && (skip_nodata || replace)
                    && values.iter().zip(nd).all(|(v, n)| v == n || (v.is_nan() && n.is_nan()))
                {
                    if skip_nodata {
                        continue;
                    }
                    shown = dst_nodata.as_deref().unwrap_or(&values);
                }
                let x = (x_off + c) as f64 + 0.5;
                let yc = row as f64 + 0.5;
                let gx = gt[0] + x * gt[1] + yc * gt[2];
                let gy = gt[3] + x * gt[4] + yc * gt[5];
                if geographic {
                    text.push_str(&crate::drivers::common::fmt_g(gx, 10));
                    text.push_str(delim);
                    text.push_str(&crate::drivers::common::fmt_g(gy, 10));
                } else {
                    for (i, coordinate) in [gx, gy].into_iter().enumerate() {
                        if i > 0 {
                            text.push_str(delim);
                        }
                        if coordinate.is_finite() {
                            write!(text, "{coordinate:.3}").unwrap();
                        } else {
                            text.push_str(&fmt_g(coordinate));
                        }
                    }
                }
                text.push_str(delim);
                for (i, v) in shown.iter().enumerate() {
                    if i > 0 {
                        text.push_str(delim);
                    }
                    if integer_format && v.is_finite() {
                        write!(text, "{}", *v as i64).unwrap();
                    } else {
                        text.push_str(&fmt_g(*v));
                    }
                }
                text.push('\n');
                lines += 1;
            }
            out.write_all(text.as_bytes())?;
        }
        y = if sy > 1 { y_off.saturating_add((y + count - y_off).div_ceil(sy).saturating_mul(sy)) } else { y + count };
    }
    out.flush()?;
    Ok(lines)
}

/// Writes XYZ to a neighboring temporary file and replaces the output only after
/// successful reads and an explicit flush. The caller must reject input aliases.
#[cfg(not(target_family = "wasm"))]
pub fn write_xyz_file(raster: &mut LazyRaster, path: &std::path::Path, options: &XyzOptions) -> Result<u64> {
    let resolved =
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) { Some(std::fs::canonicalize(path)?) } else { None };
    let path = resolved.as_deref().unwrap_or(path);
    let existing_permissions = match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => return Err(Error::InvalidArgument("XYZ output must be a regular file".into())),
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut temporary = builder.tempfile_in(parent)?;
    let lines = {
        let mut output = std::io::BufWriter::new(temporary.as_file_mut());
        let lines = write_xyz(raster, &mut output, options)?;
        output.flush()?;
        lines
    };
    if let Some(permissions) = existing_permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.persist(path).map_err(|e| Error::Io(e.error))?;
    Ok(lines)
}

/// File output is unavailable in WebAssembly; use the text-returning XYZ API.
#[cfg(target_family = "wasm")]
pub fn write_xyz_file(_raster: &mut LazyRaster, _path: &std::path::Path, _options: &XyzOptions) -> Result<u64> {
    Err(Error::Unsupported("file output is unavailable in WebAssembly; return XYZ text instead".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RasterDataset;
    use wbraster::{Raster, RasterConfig};

    #[test]
    #[cfg(unix)]
    fn atomic_xyz_output_preserves_pipes() {
        use std::os::unix::fs::FileTypeExt;
        let directory = tempfile::tempdir().unwrap();
        let pipe = directory.path().join("pipe");
        assert!(std::process::Command::new("mkfifo").arg(&pipe).status().unwrap().success());
        let mut source = raster(DataType::U8, None);
        assert!(write_xyz_file(&mut source, &pipe, &Default::default()).is_err());
        assert!(std::fs::metadata(pipe).unwrap().file_type().is_fifo());
    }

    #[test]
    fn projected_nonfinite_coordinates_use_python_spelling() {
        let mut dataset = raster(DataType::F32, None).read().unwrap();
        dataset.set_geo_transform([f64::NAN, 1., 0., 0., 0., -1.]);
        let mut source = LazyRaster::from(dataset);
        let mut bytes = Vec::new();
        write_xyz(&mut source, &mut bytes, &XyzOptions { bands: vec![1], ..Default::default() }).unwrap();
        assert!(String::from_utf8(bytes).unwrap().starts_with("nan -0.500 "));
    }

    fn raster(dt: DataType, nodata: Option<f64>) -> LazyRaster {
        let cfg = RasterConfig {
            cols: 3,
            rows: 2,
            bands: 2,
            x_min: 1000.0,
            y_min: 2000.0,
            cell_size: 10.0,
            nodata: nodata.unwrap_or(-9999.0),
            data_type: dt,
            ..Default::default()
        };
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 10.5, 20.0, 30.0, 40.0, 50.0, 6.0];
        LazyRaster::from(RasterDataset::new(Raster::from_data(cfg, values).unwrap(), nodata.is_some()))
    }

    fn xyz(r: &mut LazyRaster, o: &XyzOptions) -> String {
        let mut out = Vec::new();
        write_xyz(r, &mut out, o).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn file_output_uses_normal_creation_mode_and_preserves_existing_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let baseline = dir.path().join("baseline");
        std::fs::write(&baseline, "").unwrap();
        let expected = std::fs::metadata(baseline).unwrap().permissions().mode() & 0o777;
        let path = dir.path().join("samples.xyz");
        let mut source = raster(DataType::I32, None);
        write_xyz_file(&mut source, &path, &Default::default()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, expected);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_xyz_file(&mut source, &path, &Default::default()).unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o640);
    }

    #[test]
    fn writes_pixel_centres_and_values() {
        let mut r = raster(DataType::F32, None);
        assert_eq!(xyz(&mut r, &XyzOptions::default()).lines().next().unwrap(), "1005.000 2015.000 1");
        let all = XyzOptions { bands: vec![], delimiter: ",".into(), ..Default::default() };
        assert_eq!(xyz(&mut r, &all).lines().next().unwrap(), "1005.000,2015.000,1,10.5");
        let skip = XyzOptions { skip: (2, 2), ..Default::default() };
        assert_eq!(xyz(&mut r, &skip), "1005.000 2015.000 1\n1025.000 2015.000 3\n");
        let win = XyzOptions { srcwin: Some([1, 1, 2, 1]), ..Default::default() };
        assert_eq!(xyz(&mut r, &win), "1015.000 2005.000 5\n1025.000 2005.000 6\n");
    }

    #[test]
    fn nodata_is_skipped_or_replaced_when_every_band_matches() {
        let mut r = raster(DataType::I32, Some(6.0));
        let all = XyzOptions { bands: vec![], skip_nodata: true, ..Default::default() };
        assert_eq!(xyz(&mut r, &all).lines().count(), 5);
        let replace = XyzOptions { bands: vec![], dst_nodata: Some(vec![-1.0]), ..Default::default() };
        assert_eq!(xyz(&mut r, &replace).lines().last().unwrap(), "1025.000 2005.000 -1 -1");
        // Int32 values print as integers, after conversion to the first band's type.
        assert_eq!(xyz(&mut r, &XyzOptions { bands: vec![], ..Default::default() }).lines().next().unwrap(), "1005.000 2015.000 1 10");
    }

    #[test]
    fn rejects_windows_outside_the_raster() {
        let mut r = raster(DataType::U8, None);
        let o = XyzOptions { srcwin: Some([2, 0, 2, 1]), ..Default::default() };
        assert!(write_xyz(&mut r, &mut Vec::new(), &o).is_err());
    }
    #[test]
    fn mixed_vrt_uses_first_selected_band_type() {
        let dir = tempfile::tempdir().unwrap();
        let config = RasterConfig { cols: 1, rows: 1, bands: 1, data_type: DataType::F64, ..Default::default() };
        let dataset = RasterDataset::new(Raster::from_data(config, vec![1234567.25]).unwrap(), false);
        crate::write_raster_dataset(&dataset, dir.path().join("source.tif").to_str().unwrap(), &Default::default()).unwrap();
        let vrt = r#"<VRTDataset rasterXSize="1" rasterYSize="1"><VRTRasterBand dataType="Int32" band="1"><SimpleSource><SourceFilename relativeToVRT="1">source.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand><VRTRasterBand dataType="Float64" band="2"><SimpleSource><SourceFilename relativeToVRT="1">source.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"#;
        let path = dir.path().join("mixed.vrt");
        std::fs::write(&path, vrt).unwrap();
        let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        let mut source = source.shared();
        let info =
            crate::ops::pixel_info::pixel_info(&mut source, &[crate::ops::pixel_info::Position::new(0., 0.)], &Default::default()).unwrap();
        let json = info.to_geojson();
        assert_eq!(json["features"][0]["properties"]["bands"][1]["raw_value"], 1234567.25);
        assert!(xyz(&mut source, &XyzOptions { bands: vec![1], ..Default::default() }).ends_with(" 1234567\n"));
        assert!(xyz(&mut source, &XyzOptions { bands: vec![2], ..Default::default() }).ends_with(" 1.23457e+06\n"));
        drop(source);
        let with_nodata = vrt
            .replace("band=\"1\">", "band=\"1\"><NoDataValue>0</NoDataValue>")
            .replace("band=\"2\">", "band=\"2\"><NoDataValue>nan</NoDataValue>");
        std::fs::write(&path, with_nodata).unwrap();
        let mut source = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        let mut output = Vec::new();
        let error =
            write_xyz(&mut source, &mut output, &XyzOptions { bands: vec![], skip_nodata: true, ..Default::default() }).unwrap_err();
        assert!(error.to_string().contains("non-finite source nodata"));
        assert!(output.is_empty());
    }
}
