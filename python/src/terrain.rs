//! Terrain analysis and raster algebra: `dem`, `calc`, `proximity` and `sieve`, with keyword
//! arguments named after the command-line options.

use crate::ops::{finish_raster, input_raster};
use crate::raster::PyRaster;
use crate::{PyResultExt, invalid};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use terrio::RasterDataset;
use terrio::ops::calc::{CalcExtent, CalcInput, CalcNodata, CalcOptions};
use terrio::ops::dem::{ColorMap, ColorSelection, DemOptions, DemProduct, GradientAlg, Shading, TriAlg, pam_statistics};
use terrio::ops::parse_data_type;
use terrio::ops::proximity::{ProximityOptions, parse_values};
use terrio::ops::sieve::{SieveMask, SieveOptions};

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(dem, m)?)?;
    m.add_function(wrap_pyfunction!(calc, m)?)?;
    m.add_function(wrap_pyfunction!(proximity, m)?)?;
    m.add_function(wrap_pyfunction!(sieve, m)?)?;
    m.add_function(wrap_pyfunction!(fill_nodata, m)?)?;
    m.add_function(wrap_pyfunction!(viewshed, m)?)?;
    Ok(())
}

/// Computes a terrain product of a DEM, like `terrio dem` (GDAL's `gdaldem`).
///
/// Args:
///     src: The DEM: a path or a `Raster`.
///     processing: `"hillshade"`, `"slope"`, `"aspect"`, `"color-relief"`, `"TRI"`, `"TPI"` or
///         `"roughness"`.
///     dst: Also write the result to this path.
///     color_file: For color relief, the color file: lines of `value R G B [A]` or
///         `value colorname`, where the value may be `nv` (nodata) or a percentage.
///     band: The band to process, from 1.
///     z: Vertical exaggeration (hillshade).
///     scale: Ratio of vertical to horizontal units (hillshade, slope); `111120` for a
///         geographic DEM in metres. By default derived from the CRS.
///     xscale: The ratio along x (with `yscale`).
///     yscale: The ratio along y (with `xscale`).
///     azimuth: Azimuth of the light, in degrees (hillshade).
///     altitude: Altitude of the light, in degrees (hillshade).
///     combined: Combined shading (hillshade).
///     multidirectional: Multidirectional shading (hillshade).
///     igor: Igor's shading (hillshade).
///     alg: `"Horn"` or `"ZevenbergenThorne"` (hillshade, slope, aspect), `"Riley"` or
///         `"Wilson"` (TRI).
///     percent: Slope as a percentage rather than in degrees.
///     trigonometric: Aspect as a trigonometric angle rather than an azimuth.
///     zero_for_flat: Aspect of 0 rather than nodata on flat areas.
///     compute_edges: Compute values at the edges and next to nodata too.
///     alpha: Add an alpha band (color relief).
///     exact_color_entry: Only color values with an entry in the color file (color relief).
///     nearest_color_entry: Use the nearest entry's color rather than interpolating (color
///         relief).
///     format: The output format's short name, for `dst`. `"VRT"` (or a `.vrt` `dst`) writes a
///         color relief as a VRT of lookup tables over `src`, which must then be a path, as
///         `gdaldem color-relief -of VRT` does.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: The result, in memory (for a VRT, its pixels as read from it).
#[pyfunction]
#[pyo3(signature = (
    src, processing, dst=None, *, color_file=None, band=1, z=1.0, scale=None, xscale=None, yscale=None, azimuth=None,
    altitude=None, combined=false, multidirectional=false, igor=false, alg=None, percent=false, trigonometric=false,
    zero_for_flat=false, compute_edges=false, alpha=false, exact_color_entry=false, nearest_color_entry=false,
    format=None, compress=None, tile_size=None, bigtiff=false
))]
#[allow(clippy::too_many_arguments)]
fn dem(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    processing: &str,
    dst: Option<&str>,
    color_file: Option<&str>,
    band: usize,
    z: f64,
    scale: Option<f64>,
    xscale: Option<f64>,
    yscale: Option<f64>,
    azimuth: Option<f64>,
    altitude: Option<f64>,
    combined: bool,
    multidirectional: bool,
    igor: bool,
    alg: Option<&str>,
    percent: bool,
    trigonometric: bool,
    zero_for_flat: bool,
    compute_edges: bool,
    alpha: bool,
    exact_color_entry: bool,
    nearest_color_entry: bool,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    if [combined, multidirectional, igor].iter().filter(|b| **b).count() > 1 {
        return Err(invalid("combined, multidirectional and igor exclude each other"));
    }
    if multidirectional && azimuth.is_some() {
        return Err(invalid("multidirectional and azimuth exclude each other"));
    }
    if igor && altitude.is_some() {
        return Err(invalid("igor and altitude exclude each other"));
    }
    if exact_color_entry && nearest_color_entry {
        return Err(invalid("exact_color_entry and nearest_color_entry exclude each other"));
    }
    let color = processing.eq_ignore_ascii_case("color-relief");
    let product = if color { None } else { Some(DemProduct::parse(processing).py()?) };
    if color != color_file.is_some() {
        return Err(invalid("color_file is needed by color-relief, and only by it"));
    }
    let mut o = DemOptions {
        band,
        z,
        scale,
        xscale,
        yscale,
        azimuth: azimuth.unwrap_or(315.0),
        altitude: altitude.unwrap_or(45.0),
        shading: if combined {
            Shading::Combined
        } else if multidirectional {
            Shading::MultiDirectional
        } else if igor {
            Shading::Igor
        } else {
            Shading::Standard
        },
        slope_percent: percent,
        trigonometric,
        zero_for_flat,
        compute_edges,
        alpha,
        color_selection: if exact_color_entry {
            ColorSelection::Exact
        } else if nearest_color_entry {
            ColorSelection::Nearest
        } else {
            ColorSelection::Interpolate
        },
        ..Default::default()
    };
    if let Some(alg) = alg {
        match product {
            Some(DemProduct::Tri) => o.tri = TriAlg::parse(alg).py()?,
            Some(DemProduct::Hillshade | DemProduct::Slope | DemProduct::Aspect) => o.gradient = GradientAlg::parse(alg).py()?,
            _ => return Err(invalid("alg applies to hillshade, slope, aspect and TRI")),
        }
    }
    let path = src.extract::<String>().ok();
    let ds = input_raster(py, src)?;
    let vrt = match (format, dst) {
        (Some(f), _) => f.eq_ignore_ascii_case("VRT"),
        (None, Some(d)) => d.to_ascii_lowercase().ends_with(".vrt"),
        (None, None) => false,
    };
    if vrt {
        let (Some(file), Some(src_path), Some(out_path)) = (color_file, path.as_deref(), dst) else {
            return Err(invalid("a VRT output needs color-relief, a source path and dst"));
        };
        let out = py
            .detach(|| {
                let text = std::fs::read_to_string(file)
                    .map_err(|e| terrio::Error::InvalidArgument(format!("cannot read the color file '{file}': {e}")))?;
                let map = ColorMap::parse(&text, &ds, band, pam_statistics(src_path, band))?;
                std::fs::write(out_path, terrio::ops::dem::color_relief_vrt(&ds, src_path, &map, &o, Some(out_path))?.to_xml())?;
                terrio::open_raster(out_path)
            })
            .py()?;
        return finish_raster(py, out, None, None, None, None, false);
    }
    let out = py
        .detach(|| match (product, color_file) {
            (Some(p), _) => terrio::ops::dem::dem(&ds, p, &o),
            (None, Some(file)) => {
                let text = std::fs::read_to_string(file)
                    .map_err(|e| terrio::Error::InvalidArgument(format!("cannot read the color file '{file}': {e}")))?;
                let range = path.as_deref().and_then(|p| pam_statistics(p, band));
                terrio::ops::dem::color_relief(&ds, &ColorMap::parse(&text, &ds, band, range)?, &o)
            }
            (None, None) => unreachable!("checked"),
        })
        .py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Evaluates band math over rasters, like `terrio calc` (GDAL's `gdal_calc.py`), with NumPy's
/// typing rules so that results match `gdal_calc.py`.
///
/// Inputs are keyword arguments naming them in the expressions, with their bands as
/// `<name>_band`: `calc("(B - A) / (B + A)", A=red, B=nir, A_band=3)`.
///
/// Args:
///     calc: An expression, or a list of expressions (one per output band). They use NumPy
///         syntax: arithmetic, comparisons, `&`, `|`, `~`, `and`, `or`, `not`, and functions
///         such as `where`, `clip`, `minimum`, `maximum`, `abs`, `sqrt`, `exp`, `log`, `log10`,
///         `sin`... (also prefixed with `numpy.` or `np.`), and `A.astype(float32)`.
///     dst: Also write the result to this path.
///     type: The output data type. By default, the smallest type holding the inputs' types.
///     nodata: The output nodata value, or `"none"`. By default, a value depending on the type.
///     hide_nodata: Ignore the inputs' nodata values.
///     extent: For inputs with different extents: `"ignore"` (sizes must match), `"fail"`,
///         `"union"` or `"intersect"`.
///     projwin: Compute over this window `(ulx, uly, lrx, lry)`.
///     all_bands: Process every band of this input, giving one output band each.
///     projection_check: Fail when the inputs' CRSs differ.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///     **inputs: The inputs (paths or `Raster`s) and their bands (`<name>_band`, from 1).
///
/// Returns:
///     Raster: The result, in memory.
#[pyfunction]
#[pyo3(signature = (
    calc, dst=None, *, r#type=None, nodata=None, hide_nodata=false, extent=None, projwin=None, all_bands=None,
    projection_check=false, format=None, compress=None, tile_size=None, bigtiff=false, **inputs
))]
#[allow(clippy::too_many_arguments)]
fn calc(
    py: Python<'_>,
    calc: &Bound<'_, PyAny>,
    dst: Option<&str>,
    r#type: Option<&str>,
    nodata: Option<&Bound<'_, PyAny>>,
    hide_nodata: bool,
    extent: Option<&str>,
    projwin: Option<(f64, f64, f64, f64)>,
    all_bands: Option<String>,
    projection_check: bool,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
    inputs: Option<&Bound<'_, PyDict>>,
) -> PyResult<PyRaster> {
    let exprs: Vec<String> = match calc.extract::<String>() {
        Ok(s) => vec![s],
        Err(_) => calc.extract().map_err(|_| invalid("calc is an expression or a list of expressions"))?,
    };
    let mut named: Vec<(String, RasterDataset)> = Vec::new();
    let mut bands: Vec<(String, usize)> = Vec::new();
    if let Some(inputs) = inputs {
        for (k, v) in inputs.iter() {
            let key: String = k.extract()?;
            match key.strip_suffix("_band") {
                Some(name) if v.extract::<usize>().is_ok() => bands.push((name.to_string(), v.extract()?)),
                _ => named.push((key, input_raster(py, &v)?)),
            }
        }
    }
    if named.is_empty() {
        return Err(invalid("no input: pass rasters as keyword arguments, such as A=\"a.tif\""));
    }
    for (n, _) in &bands {
        if !named.iter().any(|(m, _)| m == n) {
            return Err(invalid(format!("a band is given for {n}, which is not an input")));
        }
    }
    let rank = |n: &str| -> usize {
        match n.as_bytes() {
            [c] if c.is_ascii_lowercase() => (c - b'a') as usize,
            [c] if c.is_ascii_uppercase() => 26 + (c - b'A') as usize,
            _ => 52,
        }
    };
    named.sort_by_key(|(n, _)| rank(n));
    let nodata = match nodata {
        None => CalcNodata::Default,
        Some(v) => match v.extract::<f64>() {
            Ok(x) => CalcNodata::Value(x),
            Err(_) if v.extract::<String>().is_ok_and(|s| s.eq_ignore_ascii_case("none")) => CalcNodata::None,
            Err(_) => return Err(invalid("nodata is a number or 'none'")),
        },
    };
    let o = CalcOptions {
        calc: exprs,
        output_type: r#type.map(parse_data_type).transpose().py()?,
        nodata,
        hide_nodata,
        extent: extent.map(CalcExtent::parse).transpose().py()?.unwrap_or_default(),
        projwin,
        all_bands,
        projection_check,
    };
    let out = py
        .detach(|| {
            let inputs: Vec<CalcInput> = named
                .iter()
                .map(|(n, ds)| CalcInput { name: n.clone(), ds, band: bands.iter().rev().find(|(m, _)| m == n).map_or(1, |(_, b)| *b) })
                .collect();
            terrio::ops::calc::calc(&inputs, &o)
        })
        .py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Computes the distance from every pixel to the nearest target pixel, like `terrio proximity`
/// (GDAL's `gdal_proximity.py`).
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     dst: Also write the result to this path.
///     band: The band, from 1.
///     values: The target values (a list of integers or a `"1,2,3"` string). By default, every
///         non-zero pixel.
///     distunits: `"PIXEL"` or `"GEO"` (georeferenced units).
///     maxdist: The largest distance computed; farther pixels get `nodata`.
///     nodata: The value of pixels beyond `maxdist` or without target (65535 by default).
///     use_input_nodata: Keep the input's nodata pixels as `nodata`.
///     fixed_buf_val: Give pixels within `maxdist` this value instead of their distance.
///     ot: The output data type (Float32 by default).
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: The distances, in memory.
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, band=1, values=None, distunits="PIXEL", maxdist=None, nodata=None, use_input_nodata=false,
    fixed_buf_val=None, ot="Float32", format=None, compress=None, tile_size=None, bigtiff=false
))]
#[allow(clippy::too_many_arguments)]
fn proximity(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    band: usize,
    values: Option<&Bound<'_, PyAny>>,
    distunits: &str,
    maxdist: Option<f64>,
    nodata: Option<f64>,
    use_input_nodata: bool,
    fixed_buf_val: Option<f64>,
    ot: &str,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    let values = match values {
        None => Vec::new(),
        Some(v) => match v.extract::<String>() {
            Ok(s) => parse_values(&s),
            Err(_) => v.extract::<Vec<i32>>().map_err(|_| invalid("values is a list of integers or a '1,2,3' string"))?,
        },
    };
    let geo_units = match distunits.to_ascii_uppercase().as_str() {
        "PIXEL" => false,
        "GEO" => true,
        _ => return Err(invalid(format!("unrecognized distunits '{distunits}' (PIXEL, GEO)"))),
    };
    let o = ProximityOptions {
        band,
        values,
        geo_units,
        max_distance: maxdist,
        nodata,
        use_input_nodata,
        fixed_value: fixed_buf_val,
        output_type: parse_data_type(ot).py()?,
    };
    let ds = input_raster(py, src)?;
    let out = py.detach(|| terrio::ops::proximity::proximity(&ds, &o)).py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Merges raster areas smaller than a threshold into their largest neighbour, like
/// `terrio sieve` (GDAL's `gdal_sieve.py`).
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     dst: Also write the result to this path.
///     band: The band, from 1.
///     threshold: Merge areas smaller than this many pixels.
///     connectedness: 4 (sides only) or 8 (corners too).
///     mask: `"default"` (the band's nodata pixels are left out), `"none"` (every pixel takes
///         part), or a raster (path or `Raster`) whose band 1 is 0 where pixels are left out.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: The sieved raster, in memory.
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, band=1, threshold=2, connectedness=4, mask=None, format=None, compress=None, tile_size=None,
    bigtiff=false
))]
#[allow(clippy::too_many_arguments)]
fn sieve(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    band: usize,
    threshold: usize,
    connectedness: u8,
    mask: Option<&Bound<'_, PyAny>>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    if connectedness != 4 && connectedness != 8 {
        return Err(invalid("connectedness is 4 or 8"));
    }
    let mask_ds = match mask {
        Some(m) if !m.extract::<String>().is_ok_and(|s| s.eq_ignore_ascii_case("default") || s.eq_ignore_ascii_case("none")) => {
            Some(input_raster(py, m)?)
        }
        _ => None,
    };
    let none = mask.and_then(|m| m.extract::<String>().ok()).is_some_and(|s| s.eq_ignore_ascii_case("none"));
    let ds = input_raster(py, src)?;
    let out = py
        .detach(|| {
            let mask = match (&mask_ds, none) {
                (Some(m), _) => SieveMask::Raster(m),
                (None, true) => SieveMask::None,
                (None, false) => SieveMask::Default,
            };
            terrio::ops::sieve::sieve(&ds, &SieveOptions { band, threshold, eight_connected: connectedness == 8, mask })
        })
        .py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Single-observer viewshed on a projected DEM.
///
/// `ox`, `oy` are observer coordinates in the DEM CRS; `oz` and `tz` are heights above
/// the DEM. `md` is maximum distance (0 unlimited); `cc` is curvature coefficient
/// (1 minus refraction, 0 flat Earth). `om` is NORMAL, DEM or GROUND. Numeric nodata
/// is treated as elevation, as in GDAL; nonfinite elevations are rejected.
#[pyfunction]
#[pyo3(signature = (src, ox, oy, dst=None, *, oz=2.0, tz=0.0, md=0.0, cc=None, om="NORMAL", ov=0, band=1,
    format=None, compress=None, tile_size=None, bigtiff=false))]
#[allow(clippy::too_many_arguments)]
fn viewshed(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    ox: f64,
    oy: f64,
    dst: Option<&str>,
    oz: f64,
    tz: f64,
    md: f64,
    cc: Option<f64>,
    om: &str,
    ov: u8,
    band: usize,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    use terrio::ops::viewshed::{ViewshedMode, ViewshedOptions};
    let o = ViewshedOptions {
        band,
        observer_x: ox,
        observer_y: oy,
        observer_height: oz,
        target_height: tz,
        max_distance: md,
        curvature_coefficient: cc,
        mode: ViewshedMode::parse(om).py()?,
        out_of_range: ov,
    };
    let ds = input_raster(py, src)?;
    let out = py.detach(|| terrio::ops::viewshed::viewshed(&ds, &o)).py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Fills raster holes using GDAL's four-quadrant interpolation.
///
/// Args:
///     src: A raster path or Raster.
///     dst: Also write the single-band result to this path.
///     band: The band to fill, from 1.
///     mask: A raster path or Raster whose band 1 is zero at holes. Without one,
///         use the source's internal/sidecar mask, otherwise its nodata value.
///     max_distance: Search distance in pixels; 0 searches the whole image.
///     interpolation: "inv_dist" or "nearest".
///     smoothing_iterations: Number of 3x3 smoothing passes on filled pixels.
///     format: Output format for dst.
///     compress: GeoTIFF compression for dst.
///     tile_size: GeoTIFF tile size for dst.
///     bigtiff: Write a BigTIFF for dst.
///
/// Returns:
///     Raster: One band, with the source's type, nodata and georeferencing.
#[pyfunction]
#[pyo3(signature = (src, dst=None, *, band=1, mask=None, max_distance=100.0,
    interpolation="inv_dist", smoothing_iterations=0, format=None, compress=None,
    tile_size=None, bigtiff=false))]
#[allow(clippy::too_many_arguments)]
fn fill_nodata(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    band: usize,
    mask: Option<&Bound<'_, PyAny>>,
    max_distance: f64,
    interpolation: &str,
    smoothing_iterations: usize,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    use terrio::ops::fill_nodata::{FillNodataOptions, Interpolation, raster_mask};
    let method = Interpolation::parse(interpolation).py()?;
    let use_default_mask = mask.is_none();
    let (ds, default_mask) = match crate::ops::raster_path(src)? {
        Some(path) => crate::with_warnings(py, || {
            let mut r = terrio::open_raster_lazy(&path)?;
            let window = terrio::Window::new(0, 0, r.width(), r.height());
            let bytes = if use_default_mask { r.read_mask(window, None)? } else { None };
            Ok((r.read()?, bytes))
        })?,
        None => {
            let mut r = src.cast::<PyRaster>().map_err(|_| invalid("expected a path or a terrio.Raster"))?.borrow_mut();
            let window = terrio::Window::new(0, 0, r.inner.width(), r.inner.height());
            let bytes = if mask.is_none() { r.inner.read_mask(window, None).py()? } else { None };
            (r.inner.read().py()?, bytes)
        }
    };
    let bytes = match mask {
        Some(m) => Some(raster_mask(&input_raster(py, m)?, ds.raster.cols, ds.raster.rows).py()?),
        None => default_mask,
    };
    let options = FillNodataOptions { band, mask: bytes.as_deref(), max_distance, interpolation: method, smoothing_iterations };
    let out = py.detach(|| terrio::ops::fill_nodata::fill_nodata(&ds, &options)).py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}
