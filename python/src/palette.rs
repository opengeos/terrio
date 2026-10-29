//! Palette operations use the shared quantizer and retain lazy-source validity masks.
use crate::ops::raster_path;
use crate::raster::{PyRaster, write_options};
use crate::{PyResultExt, invalid};
use numpy::{PyReadonlyArray2, PyUntypedArrayMethods};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use terrio::LazyRaster;
use terrio::ops::palette::{self, PaletteOptions};

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(rgb_to_palette, m)?)?;
    m.add_function(wrap_pyfunction!(palette_to_rgb, m)?)?;
    m.add_function(wrap_pyfunction!(attach_palette, m)?)?;
    m.add_function(wrap_pyfunction!(read_color_table, m)?)?;
    Ok(())
}
struct Destination<'a> {
    path: &'a str,
    other_sources: Vec<String>,
    overwrite: bool,
}
fn check_destination(source: &LazyRaster, path: Option<&str>, destination: Option<Destination<'_>>) -> terrio::Result<()> {
    if let Some(destination) = destination {
        let mut paths = destination.other_sources;
        paths.extend(source.file_list()?);
        if let Some(path) = path {
            paths.push(path.into());
        }
        palette::check_output(destination.path, &paths.iter().map(String::as_str).collect::<Vec<_>>(), destination.overwrite)?;
    }
    Ok(())
}
fn with_source<T: Send>(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    destination: Option<Destination<'_>>,
    convert: impl FnOnce(&mut LazyRaster) -> terrio::Result<T> + Send,
) -> PyResult<T> {
    if let Some(path) = raster_path(src)? {
        crate::with_warnings(py, || {
            let mut source = terrio::open_raster_lazy(&path)?;
            check_destination(&source, Some(&path), destination)?;
            convert(&mut source)
        })
    } else {
        let mut raster = src.cast::<PyRaster>()?.try_borrow_mut().map_err(|e| invalid(e.to_string()))?;
        let inner = &mut raster.inner;
        crate::with_warnings(py, || {
            check_destination(inner, None, destination)?;
            convert(inner)
        })
    }
}
fn table(py: Python<'_>, source: &Bound<'_, PyAny>, filename: Option<&str>) -> PyResult<Vec<[u8; 4]>> {
    if let Ok(table) = source.extract::<Vec<[u8; 4]>>() {
        return Ok(table);
    }
    if let Ok(bytes) = source.cast::<PyBytes>() {
        let data = bytes.as_bytes();
        let hint = if let Some(filename) = filename {
            filename
        } else if matches!(data.get(..4), Some(b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+")) {
            "palette.tif"
        } else if data.starts_with(b"\x89PNG") {
            "palette.png"
        } else if std::str::from_utf8(data).is_ok() {
            "palette.txt"
        } else {
            return Err(invalid("unrecognized palette bytes; supply a filename format hint"));
        };
        return palette::color_table_from_bytes(data.to_vec(), hint).py();
    }
    if source.cast::<PyRaster>().is_ok() {
        return with_source(py, source, None, |r| palette::color_table(&r.header()));
    }
    let path = source
        .extract::<std::path::PathBuf>()
        .map_err(|_| invalid("expected a palette path, Raster, bytes or RGBA sequence with Byte entries"))?;
    let path = path.into_os_string().into_string().map_err(|_| invalid("raster paths must be UTF-8"))?;
    crate::with_warnings(py, || palette::read_color_table(&path))
}
fn destination<'a>(
    py: Python<'_>,
    color_map: Option<&Bound<'_, PyAny>>,
    dst: Option<&'a str>,
    overwrite: bool,
) -> PyResult<Option<Destination<'a>>> {
    let Some(path) = dst else {
        return Ok(None);
    };
    // Reject an existing output before loading palettes or opening the source.
    palette::check_output(path, &[], overwrite).py()?;
    let mut paths = Vec::new();
    if let Some(source) = color_map
        && let Ok(raster) = source.cast::<PyRaster>()
    {
        paths.extend(raster.try_borrow().map_err(|e| invalid(e.to_string()))?.inner.file_list().py()?);
    }
    if let Some(source) = color_map
        && !source.is_instance_of::<PyBytes>()
        && let Ok(palette_path) = source.extract::<std::path::PathBuf>()
    {
        let palette_path = palette_path.into_os_string().into_string().map_err(|_| invalid("raster paths must be UTF-8"))?;
        paths.extend(crate::with_warnings(py, || palette::color_table_files(&palette_path))?);
    }
    Ok(Some(Destination { path, other_sources: paths, overwrite }))
}
#[allow(clippy::too_many_arguments)]
fn finish(
    py: Python<'_>,
    mut output: LazyRaster,
    dst: Option<&str>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    if let Some(path) = dst {
        let mut options = write_options(format, compress, tile_size, bigtiff)?;
        py.detach(|| {
            output.prepare_output_mask(path, &mut options, terrio::Window::full(output.width(), output.height()), None)?;
            terrio::write_raster_dataset(&output.read()?, path, &options)
        })
        .py()?;
    }
    Ok(PyRaster { inner: output })
}
/// Reads RGBA entries from a palette Raster/path, encoded bytes, or RGBA sequence.
///
/// Args:
///     source: Palette raster, native/VSI path, bytes, or sequence of RGBA entries.
///     filename: Format hint for bytes; TIFF/PNG and UTF-8 text are detected when omitted. TXT/CPT/QML/QLR are supported.
///
/// Returns:
///     list: RGBA entries from index zero, with source nodata transparency applied.
#[pyfunction]
#[pyo3(signature=(source, filename=None))]
fn read_color_table(py: Python<'_>, source: &Bound<'_, PyAny>, filename: Option<&str>) -> PyResult<Vec<Vec<u32>>> {
    Ok(table(py, source, filename)?.into_iter().map(|entry| entry.into_iter().map(u32::from).collect()).collect())
}
/// Converts RGB Byte bands to indexed color with GDAL-compatible median cut and diffusion.
///
/// Args:
///     src: Raster/path. The operation materializes its pixels; masks are retained for quantization.
///     dst: Optional output path. Existing outputs require overwrite; input paths/aliases are protected.
///     color_count: Number of generated colors, 2..256.
///     bit_depth: Histogram and lookup precision, 5 or 8.
///     dither: Apply integer error diffusion.
///     color_map: Palette Raster/path, encoded bytes, or RGBA sequence instead of generation.
///     output_nodata: Reserve this index for invalid pixels; otherwise automatically select zero.
///     mask: Optional two-dimensional uint8 validity mask, zero for invalid pixels.
///         Shared RGB nodata takes precedence over this mask, following GDAL.
///     format: Output driver for dst.
///     compress: Output compression for dst.
///     tile_size: GeoTIFF tile size for dst.
///     bigtiff: Write BigTIFF for dst.
///     overwrite: Permit replacing an existing destination.
///
/// Returns:
///     Raster: One Byte band, its color table and nodata, on the source grid.
#[pyfunction]
#[pyo3(signature=(src, dst=None, *, color_count=256, bit_depth=5, dither=true, color_map=None, output_nodata=None, mask=None, format=None, compress=None, tile_size=None, bigtiff=false, overwrite=false))]
#[allow(clippy::too_many_arguments)]
fn rgb_to_palette(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    color_count: usize,
    bit_depth: u8,
    dither: bool,
    color_map: Option<&Bound<'_, PyAny>>,
    output_nodata: Option<u8>,
    mask: Option<PyReadonlyArray2<'_, u8>>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
    overwrite: bool,
) -> PyResult<PyRaster> {
    let destination = destination(py, color_map, dst, overwrite)?;
    let mut options = PaletteOptions {
        color_count,
        bit_depth,
        dither,
        output_nodata,
        color_table: color_map.map(|v| table(py, v, None)).transpose()?,
        mask: None,
    };
    let mask = mask.map(|m| (m.shape().to_vec(), m.as_array().iter().copied().collect::<Vec<_>>()));
    let output = with_source(py, src, destination, move |source| {
        if let Some((shape, values)) = mask {
            if shape != [source.height(), source.width()] {
                return Err(terrio::Error::InvalidArgument("mask dimensions must match the raster".into()));
            }
            options.mask = Some(values);
        }
        palette::rgb_to_palette_lazy(source, &options).map(LazyRaster::from)
    })?;
    finish(py, output, dst, format, compress, tile_size, bigtiff)
}
/// Expands an indexed Raster/path to RGB, or RGBA with rgba=True, preserving dataset masks.
/// The full raster is materialized. Output options and overwrite behavior match rgb_to_palette.
#[pyfunction]
#[pyo3(signature=(src, dst=None, *, rgba=false, format=None, compress=None, tile_size=None, bigtiff=false, overwrite=false))]
#[allow(clippy::too_many_arguments)]
fn palette_to_rgb(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    rgba: bool,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
    overwrite: bool,
) -> PyResult<PyRaster> {
    let destination = destination(py, None, dst, overwrite)?;
    let output = with_source(py, src, destination, |source| palette::palette_to_rgb_lazy(source, rgba))?;
    finish(py, output, dst, format, compress, tile_size, bigtiff)
}
/// Attaches a color map without changing Byte/UInt16 indices, nodata or dataset masks.
/// color_map accepts a palette Raster/path, bytes or RGBA sequence. The operation materializes
/// the source; output options and overwrite behavior match rgb_to_palette.
#[pyfunction]
#[pyo3(signature=(src, color_map, dst=None, *, format=None, compress=None, tile_size=None, bigtiff=false, overwrite=false))]
#[allow(clippy::too_many_arguments)]
fn attach_palette(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    color_map: &Bound<'_, PyAny>,
    dst: Option<&str>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
    overwrite: bool,
) -> PyResult<PyRaster> {
    let destination = destination(py, Some(color_map), dst, overwrite)?;
    let colors = table(py, color_map, None)?;
    let output = with_source(py, src, destination, |source| palette::attach_palette_lazy(source, &colors))?;
    finish(py, output, dst, format, compress, tile_size, bigtiff)
}
