//! Raster creation, overview management and in-place update bindings.
use crate::ops::raster_path;
use crate::raster::{PyRaster, write_options};
use crate::{PyResultExt, invalid, with_warnings};
use pyo3::prelude::*;
use terrio::ops::{Resampling, create, edit, overview, update};

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(create_raster, m)?)?;
    m.add_function(wrap_pyfunction!(edit_raster, m)?)?;
    m.add_function(wrap_pyfunction!(move_raster, m)?)?;
    m.add_function(wrap_pyfunction!(add_overviews, m)?)?;
    m.add_function(wrap_pyfunction!(delete_overviews, m)?)?;
    m.add_function(wrap_pyfunction!(refresh_overviews, m)?)?;
    m.add_function(wrap_pyfunction!(update_raster, m)?)?;
    m.add_function(wrap_pyfunction!(validate_cog, m)?)?;
    Ok(())
}

/// Validates a Cloud Optimized GeoTIFF with rio-cogeo's `cog_validate` checks, read from the
/// file's structure without GDAL: the full-resolution image and overviews tiled (above 512
/// pixels), overviews sorted by decimation, the image directories first (the main one within 300
/// bytes) and the image data after them, from the smallest overview to the full resolution.
///
/// The default profile retains rio-cogeo's results and diagnostic messages.
///
/// Args:
///     path: A local path (`str` or `os.PathLike`), HTTP(S) URL or S3 URI (read with range
///         requests), or uploaded TIFF bytes. Bytes cannot include external sidecars.
///     strict: Treat warnings as errors.
///     profile: "rio-cogeo" (the default) or "gdal" for GDAL 3.13 structural rules.
///     full_check: With profile="gdal", inspect every block's structural promises.
///         None enables full checks for local paths and bytes, and disables them for URLs.
///
/// Returns:
///     tuple[bool, list[str], list[str]]: Whether the file is a valid COG, the errors and the
///     warnings, as `rio_cogeo.cog_validate`.
///
/// Raises:
///     TerrioError: If the file cannot be read.
#[pyfunction]
#[pyo3(signature = (path, *, strict=false, profile="rio-cogeo", full_check=None))]
fn validate_cog(
    py: Python<'_>,
    path: &Bound<'_, PyAny>,
    strict: bool,
    profile: &str,
    full_check: Option<bool>,
) -> PyResult<(bool, Vec<String>, Vec<String>)> {
    let options = terrio::CogValidationOptions {
        strict,
        full_check,
        profile: match profile {
            "rio-cogeo" => terrio::CogValidationProfile::RioCogeo,
            "gdal" => terrio::CogValidationProfile::Gdal,
            _ => return Err(invalid("profile must be rio-cogeo or gdal")),
        },
    };
    let v = if let Ok(bytes) = path.cast::<pyo3::types::PyBytes>() {
        let bytes = bytes.as_bytes();
        with_warnings(py, || terrio::validate_cog_bytes(bytes, &options))?
    } else {
        let path = raster_path(path)?.ok_or_else(|| invalid("validate_cog takes a path or bytes, not a terrio.Raster"))?;
        with_warnings(py, || terrio::validate_cog_with_options(&path, &options))?
    };
    Ok((v.is_valid, v.errors, v.warnings))
}

fn source(py: Python<'_>, src: &Bound<'_, PyAny>) -> PyResult<terrio::LazyRaster> {
    match raster_path(src)? {
        Some(path) => with_warnings(py, || terrio::open_raster_lazy(&path)),
        None => Ok(src.cast::<PyRaster>()?.borrow_mut().inner.shared()),
    }
}

/// Creates a filled raster, optionally writing a file a strip at a time.
/// `like` supplies header defaults; explicit options override them. File outputs
/// can copy template overview levels and remain lazy in the returned Raster.
#[pyfunction]
#[pyo3(signature = (dst=None, *, like=None, size=None, band_count=None, ot=None, crs=None, bbox=None, nodata=None, unset_nodata=false, burn=None, metadata=None, copy_metadata=false, copy_overviews=false, overwrite=false, format=None, compress=None, tile_size=None, bigtiff=false))]
#[allow(clippy::too_many_arguments)]
fn create_raster(
    py: Python<'_>,
    dst: Option<&str>,
    like: Option<&Bound<'_, PyAny>>,
    size: Option<(usize, usize)>,
    band_count: Option<usize>,
    ot: Option<&str>,
    crs: Option<&str>,
    bbox: Option<[f64; 4]>,
    nodata: Option<f64>,
    unset_nodata: bool,
    burn: Option<Vec<f64>>,
    metadata: Option<std::collections::BTreeMap<String, String>>,
    copy_metadata: bool,
    copy_overviews: bool,
    overwrite: bool,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    if dst.is_none() && (copy_overviews || overwrite || format.is_some() || compress.is_some() || tile_size.is_some() || bigtiff) {
        return Err(invalid("file-output options require dst"));
    }
    let template = like.map(|src| source(py, src)).transpose()?;
    let o = create::CreateOptions {
        size,
        band_count,
        data_type: ot.map(terrio::ops::parse_data_type).transpose().py()?,
        crs: crs.map(terrio::crs::SpatialRef::parse).transpose().py()?,
        bbox,
        nodata,
        unset_nodata,
        burn: burn.unwrap_or_default(),
        metadata: metadata.unwrap_or_default().into_iter().collect(),
        copy_metadata,
        copy_overviews,
        overwrite,
    };
    if let Some(path) = dst {
        let write = write_options(format, compress, tile_size, bigtiff)?;
        with_warnings(py, || create::create_raster_file_from_template(path, template.as_ref(), &o, &write))?;
        return Ok(PyRaster { inner: with_warnings(py, || terrio::open_raster_lazy(path))? });
    }
    Ok(PyRaster::from_dataset(with_warnings(py, || create::create_raster(template.as_ref(), &o))?))
}

/// Adds internal or external GeoTIFF overview levels. Returns their pixel sizes.
#[pyfunction]
#[pyo3(signature = (path, levels=None, *, min_size=256, resampling="nearest", external=false, compression=None, ignore_cog_layout_break=false))]
#[allow(clippy::too_many_arguments)]
fn add_overviews(
    py: Python<'_>,
    path: &str,
    levels: Option<Vec<usize>>,
    min_size: usize,
    resampling: &str,
    external: bool,
    compression: Option<String>,
    ignore_cog_layout_break: bool,
) -> PyResult<Vec<(usize, usize)>> {
    let o = overview::OverviewOptions {
        levels: levels.unwrap_or_default(),
        min_size,
        resampling: Resampling::parse(resampling).py()?,
        external,
        compression,
        ignore_cog_layout_break,
    };
    with_warnings(py, || overview::add_overviews(path, &o))
}

/// Deletes internal overview links or a validated external overview sidecar.
#[pyfunction]
#[pyo3(signature = (path, *, external=false, ignore_cog_layout_break=false))]
fn delete_overviews(py: Python<'_>, path: &str, external: bool, ignore_cog_layout_break: bool) -> PyResult<()> {
    with_warnings(py, || overview::delete_overviews(path, &overview::DeleteOptions { external, ignore_cog_layout_break }))
}

/// Refreshes existing overview levels, optionally over bounding boxes or template extents.
#[pyfunction]
#[pyo3(signature = (path, levels=None, *, resampling=None, bbox=None, like=None, external=false, ignore_cog_layout_break=false))]
#[allow(clippy::too_many_arguments)]
fn refresh_overviews(
    py: Python<'_>,
    path: &str,
    levels: Option<Vec<usize>>,
    resampling: Option<&str>,
    bbox: Option<Vec<[f64; 4]>>,
    like: Option<Vec<String>>,
    external: bool,
    ignore_cog_layout_break: bool,
) -> PyResult<Vec<(usize, usize)>> {
    let mut o = overview::RefreshOptions {
        levels: levels.unwrap_or_default(),
        resampling: resampling.map(Resampling::parse).transpose().py()?,
        bbox: bbox.unwrap_or_default(),
        external,
        ignore_cog_layout_break,
    };
    with_warnings(py, || {
        if let Some(like) = like {
            o.bbox.extend(overview::like_bounds(path, &like)?);
        }
        overview::refresh_overviews(path, &o)
    })
}

/// Warps valid source samples into an existing local GeoTIFF and refreshes its overviews.
/// Returns the affected pixel window or None when no values were updated.
#[pyfunction]
#[pyo3(signature = (src, dst, *, resampling="nearest", geometry=None, geometry_crs=None, no_update_overviews=false, error_threshold=None, ignore_cog_layout_break=false))]
#[allow(clippy::too_many_arguments)]
fn update_raster(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: &str,
    resampling: &str,
    geometry: Option<&str>,
    geometry_crs: Option<&str>,
    no_update_overviews: bool,
    error_threshold: Option<f64>,
    ignore_cog_layout_break: bool,
) -> PyResult<Option<(usize, usize, usize, usize)>> {
    if geometry.is_none() && geometry_crs.is_some() {
        return Err(invalid("geometry_crs requires geometry"));
    }
    let crs = geometry_crs.map(terrio::crs::SpatialRef::parse).transpose().py()?;
    let options = update::UpdateOptions {
        resampling: Resampling::parse(resampling).py()?,
        geometry: geometry.map(|g| update::parse_geometry(g, crs)).transpose().py()?,
        no_update_overviews,
        error_threshold,
        ignore_cog_layout_break,
    };
    let mut source = source(py, src)?;
    with_warnings(py, || update::update_raster(&mut source, dst, &options).map(|w| w.map(|w| (w.x, w.y, w.width, w.height))))
}

/// Edits GeoTIFF georeferencing, nodata and metadata in place without resampling.
#[pyfunction]
#[pyo3(signature = (path, *, crs=None, unset_crs=false, geotransform=None, bbox=None, gcps=None, nodata=None, unset_nodata=false, metadata=None, unset_metadata=None, unset_all_metadata=false, stats=false, approx_stats=false, unset_stats=false, histogram=false, ignore_cog_layout_break=false))]
#[allow(clippy::too_many_arguments)]
fn edit_raster(
    py: Python<'_>,
    path: &str,
    crs: Option<&str>,
    unset_crs: bool,
    geotransform: Option<[f64; 6]>,
    bbox: Option<[f64; 4]>,
    gcps: Option<Vec<crate::raster::GcpTuple>>,
    nodata: Option<f64>,
    unset_nodata: bool,
    metadata: Option<std::collections::BTreeMap<String, String>>,
    unset_metadata: Option<Vec<String>>,
    unset_all_metadata: bool,
    stats: bool,
    approx_stats: bool,
    unset_stats: bool,
    histogram: bool,
    ignore_cog_layout_break: bool,
) -> PyResult<()> {
    let options = edit::EditOptions {
        crs: crs.map(terrio::crs::SpatialRef::parse).transpose().py()?,
        unset_crs,
        geo_transform: geotransform,
        bbox,
        gcps: crate::raster::parse_gcps(gcps),
        nodata,
        unset_nodata,
        metadata: metadata.unwrap_or_default().into_iter().collect(),
        unset_metadata: unset_metadata.unwrap_or_default(),
        unset_all_metadata,
        stats,
        approx_stats,
        unset_stats,
        histogram,
        ignore_cog_layout_break,
    };
    with_warnings(py, || edit::edit_raster(path, &options))
}

/// Reports or changes GeoTIFF georeferencing in another CRS without resampling.
#[pyfunction]
#[pyo3(signature = (path, t_srs, *, s_srs=None, error_threshold=None, ignore_cog_layout_break=false))]
fn move_raster(
    py: Python<'_>,
    path: &str,
    t_srs: &str,
    s_srs: Option<&str>,
    error_threshold: Option<f64>,
    ignore_cog_layout_break: bool,
) -> PyResult<Py<PyAny>> {
    let options = edit::MoveOptions {
        t_srs: terrio::crs::SpatialRef::parse(t_srs).py()?,
        s_srs: s_srs.map(terrio::crs::SpatialRef::parse).transpose().py()?,
        error_threshold,
        ignore_cog_layout_break,
    };
    let report = with_warnings(py, || edit::move_raster(path, &options))?;
    let result = pyo3::types::PyDict::new(py);
    result.set_item("geotransform", report.geo_transform)?;
    result.set_item("corners", report.corners)?;
    result.set_item("errors", report.errors)?;
    result.set_item("max_error", report.max_error)?;
    result.set_item("updated", report.updated)?;
    Ok(result.into_any().unbind())
}
