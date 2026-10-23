//! Helpers for `terrio.rio`, the rasterio-compatible API: the raw tags of a TIFF's images, CRS
//! details, and the output grid GDAL suggests for a reprojection.

use crate::raster::GcpTuple;
use crate::{PyResultExt, invalid};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::io::{Read, Seek, SeekFrom};
use terrio::crs::SpatialRef;
use terrio::vfs::Location;

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(tiff_tags, m)?)?;
    m.add_function(wrap_pyfunction!(crs_info, m)?)?;
    m.add_function(wrap_pyfunction!(suggested_grid, m)?)?;
    m.add_function(wrap_pyfunction!(gcp_transform, m)?)?;
    m.add_class::<PyRpcTransformer>()?;
    Ok(())
}

/// The most images (IFDs) read.
const MAX_IFDS: usize = 512;
/// The largest tag value read, in bytes; larger values are skipped.
const MAX_VALUE: u64 = 1 << 22;
/// Tags whose values are not returned: the chunk offsets and byte counts.
const SKIPPED: [u16; 4] = [273, 279, 324, 325];

/// A tag's values.
enum TagValue {
    Ints(Vec<i64>),
    Floats(Vec<f64>),
    Ascii(String),
    Bytes(Vec<u8>),
}

/// The tags of each image of a TIFF.
type Ifds = Vec<Vec<(u16, TagValue)>>;

/// Reads the tags of every image of a TIFF stream. `None` when the stream is not a TIFF.
fn read_ifds<R: Read + Seek>(f: &mut R) -> std::io::Result<Option<Ifds>> {
    let mut header = [0u8; 16];
    f.seek(SeekFrom::Start(0))?;
    if f.read(&mut header[..8])? < 8 {
        return Ok(None);
    }
    let le = match &header[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Ok(None),
    };
    let rd = Reader { le };
    let big = match rd.u16(&header[2..4]) {
        42 => false,
        43 => true,
        _ => return Ok(None),
    };
    let mut next = if big {
        f.read_exact(&mut header[8..16])?;
        rd.u64(&header[8..16])
    } else {
        rd.u32(&header[4..8]) as u64
    };
    // GDAL's structural metadata (COGs), right after the header.
    let ghost = read_ghost(f, if big { 16 } else { 8 })?;
    let mut seen = Vec::new();
    let mut out: Ifds = Vec::new();
    while next != 0 && seen.len() < MAX_IFDS && !seen.contains(&next) {
        seen.push(next);
        f.seek(SeekFrom::Start(next))?;
        let count = if big {
            let mut b = [0u8; 8];
            f.read_exact(&mut b)?;
            rd.u64(&b)
        } else {
            let mut b = [0u8; 2];
            f.read_exact(&mut b)?;
            rd.u16(&b) as u64
        };
        let entry_len = if big { 20 } else { 12 };
        let mut entries = vec![0u8; count.min(4096) as usize * entry_len];
        f.read_exact(&mut entries)?;
        let mut b = [0u8; 8];
        let n = if big { 8 } else { 4 };
        f.read_exact(&mut b[..n])?;
        next = if big { rd.u64(&b) } else { rd.u32(&b) as u64 };
        let mut tags = Vec::new();
        for e in entries.chunks(entry_len) {
            let tag = rd.u16(&e[0..2]);
            if SKIPPED.contains(&tag) {
                continue;
            }
            let typ = rd.u16(&e[2..4]);
            let (n, inline) = if big { (rd.u64(&e[4..12]), &e[12..20]) } else { (rd.u32(&e[4..8]) as u64, &e[8..12]) };
            let size: u64 = match typ {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 | 13 => 4,
                5 | 10 | 12 | 16 | 17 | 18 => 8,
                _ => continue,
            };
            let Some(len) = n.checked_mul(size).filter(|l| *l <= MAX_VALUE) else { continue };
            let data = if len as usize <= inline.len() {
                inline[..len as usize].to_vec()
            } else {
                let off = if big { rd.u64(inline) } else { rd.u32(inline) as u64 };
                f.seek(SeekFrom::Start(off))?;
                let mut d = vec![0u8; len as usize];
                f.read_exact(&mut d)?;
                d
            };
            tags.push((tag, rd.value(typ, &data)));
        }
        out.push(tags);
    }
    if let (Some(g), Some(first)) = (ghost, out.first_mut()) {
        first.push((0, TagValue::Ascii(g)));
    }
    Ok(Some(out))
}

/// GDAL's structural metadata block (`GDAL_STRUCTURAL_METADATA_SIZE=...`) at `offset`, if any.
fn read_ghost<R: Read + Seek>(f: &mut R, offset: u64) -> std::io::Result<Option<String>> {
    const KEY: &[u8] = b"GDAL_STRUCTURAL_METADATA_SIZE=";
    f.seek(SeekFrom::Start(offset))?;
    let mut head = [0u8; 43];
    if f.read(&mut head)? < head.len() || !head.starts_with(KEY) {
        return Ok(None);
    }
    let size: usize = match std::str::from_utf8(&head[KEY.len()..KEY.len() + 6]).ok().and_then(|s| s.trim().parse().ok()) {
        Some(n) if n < 1 << 16 => n,
        _ => return Ok(None),
    };
    let mut body = vec![0u8; size];
    f.read_exact(&mut body)?;
    Ok(Some(String::from_utf8_lossy(&body).to_string()))
}

/// Decodes numbers in the file's byte order.
struct Reader {
    le: bool,
}

impl Reader {
    fn u16(&self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) }
    }

    fn u32(&self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
    }

    fn u64(&self, b: &[u8]) -> u64 {
        let a: [u8; 8] = b[..8].try_into().unwrap_or_default();
        if self.le { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) }
    }

    fn value(&self, typ: u16, d: &[u8]) -> TagValue {
        match typ {
            1 => TagValue::Ints(d.iter().map(|v| *v as i64).collect()),
            6 => TagValue::Ints(d.iter().map(|v| *v as i8 as i64).collect()),
            7 => TagValue::Bytes(d.to_vec()),
            2 => TagValue::Ascii(String::from_utf8_lossy(d).trim_end_matches('\0').to_string()),
            3 => TagValue::Ints(d.chunks(2).map(|c| self.u16(c) as i64).collect()),
            8 => TagValue::Ints(d.chunks(2).map(|c| self.u16(c) as i16 as i64).collect()),
            4 | 13 => TagValue::Ints(d.chunks(4).map(|c| self.u32(c) as i64).collect()),
            9 => TagValue::Ints(d.chunks(4).map(|c| self.u32(c) as i32 as i64).collect()),
            16 | 18 => TagValue::Ints(d.chunks(8).map(|c| self.u64(c) as i64).collect()),
            17 => TagValue::Ints(d.chunks(8).map(|c| self.u64(c) as i64).collect()),
            11 => TagValue::Floats(d.chunks(4).map(|c| f32::from_bits(self.u32(c)) as f64).collect()),
            12 => TagValue::Floats(d.chunks(8).map(|c| f64::from_bits(self.u64(c))).collect()),
            5 => TagValue::Floats(d.chunks(8).map(|c| self.u32(&c[..4]) as f64 / self.u32(&c[4..]).max(1) as f64).collect()),
            _ => {
                TagValue::Floats(d.chunks(8).map(|c| self.u32(&c[..4]) as i32 as f64 / (self.u32(&c[4..]) as i32).max(1) as f64).collect())
            }
        }
    }
}

/// Reads the tags of a TIFF's images, at a path or URL.
fn tags_of(path: &str) -> terrio::Result<Option<Ifds>> {
    let loc = Location::parse(path);
    let tags = match &loc {
        // WebAssembly (Pyodide) builds have no HTTP client.
        #[cfg(not(target_arch = "wasm32"))]
        Location::Url(_) | Location::S3 { .. } => {
            let remote = loc.remote().ok_or_else(|| terrio::Error::InvalidArgument(format!("'{path}' is not remote")))?;
            let mut r = terrio::http::RangeReader::open(&remote)?;
            read_ifds(&mut r)?
        }
        Location::Local(p) if p.is_file() => read_ifds(&mut std::io::BufReader::new(std::fs::File::open(p)?))?,
        Location::Local(_) => None,
        #[cfg(target_arch = "wasm32")]
        Location::Url(_) | Location::S3 { .. } => None,
        _ => {
            let local = terrio::vfs::resolve(path)?;
            read_ifds(&mut std::io::BufReader::new(std::fs::File::open(local.path())?))?
        }
    };
    Ok(tags)
}

/// Reads the tags of every image (IFD) of a TIFF file, without its pixels.
///
/// Args:
///     path: A local path, a URL, an `s3://` object or a zip archive member.
///
/// Returns:
///     list[dict[int, object]] | None: One dictionary per image, from tag numbers to values: a
///     `str` for ASCII tags, `bytes` for undefined ones, else a list of ints or floats. The chunk
///     offsets and byte counts are left out. The first image's key 0 holds GDAL's structural
///     metadata (of COGs), when the file has some. `None` when the file is not a TIFF.
///
/// Raises:
///     TerrioIOError: If the file cannot be read.
#[pyfunction]
fn tiff_tags<'py>(py: Python<'py>, path: &str) -> PyResult<Option<Bound<'py, PyList>>> {
    let Some(ifds) = py.detach(|| tags_of(path)).py()? else { return Ok(None) };
    let out = PyList::empty(py);
    for tags in ifds {
        let d = PyDict::new(py);
        for (tag, v) in tags {
            match v {
                TagValue::Ints(v) => d.set_item(tag, v)?,
                TagValue::Floats(v) => d.set_item(tag, v)?,
                TagValue::Ascii(s) => d.set_item(tag, s)?,
                TagValue::Bytes(b) => d.set_item(tag, pyo3::types::PyBytes::new(py, &b))?,
            }
        }
        out.append(d)?;
    }
    Ok(Some(out))
}

/// Describes a CRS.
///
/// Args:
///     crs: A CRS definition: `EPSG:4326`, WKT, a PROJ string...
///
/// Returns:
///     dict: `epsg` (int or None), `esri` (the ESRI code of a CRS without EPSG code, int or
///     None), `wkt` (str or None), `proj4` (str or None), `name`
///     (str or None) and `is_geographic` (bool).
///
/// Raises:
///     CRSError: If the CRS cannot be parsed.
#[pyfunction]
fn crs_info<'py>(py: Python<'py>, crs: &str) -> PyResult<Bound<'py, PyDict>> {
    let s = SpatialRef::parse(crs).py()?;
    let d = PyDict::new(py);
    d.set_item("epsg", s.epsg)?;
    d.set_item("esri", s.esri)?;
    d.set_item("wkt", s.to_wkt())?;
    d.set_item("proj4", s.proj4.clone())?;
    d.set_item("name", s.name())?;
    d.set_item("is_geographic", s.is_geographic())?;
    Ok(d)
}

/// The output grid GDAL suggests for a raster reprojected to another CRS
/// (`GDALSuggestedWarpOutput2()`).
///
/// Args:
///     width: The source width in pixels.
///     height: The source height in pixels.
///     transform: The source's GDAL-style geotransform.
///     src_crs: The source CRS.
///     dst_crs: The output CRS.
///     gcps: Ground control points georeferencing the source instead of `transform`:
///         `(pixel, line, x, y, z)` tuples.
///     order: The GCP polynomial order (0 picks GDAL's default).
///     gcp_method: `"auto"`, `"polynomial"`, `"tps"` or `"homography"`.
///     rpc: GDAL's `RPC` metadata items georeferencing the source instead (its CRS then WGS 84).
///     rpc_options: The RPC transformer's options, GDAL's `RPC_*` transformer options.
///
/// Returns:
///     tuple: The output geotransform (6 floats), width and height.
///
/// Raises:
///     CRSError: If a CRS cannot be parsed or the transformation fails.
#[pyfunction]
#[pyo3(signature = (width, height, transform, src_crs, dst_crs, gcps=None, order=0, gcp_method="auto", rpc=None, rpc_options=None))]
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn suggested_grid(
    py: Python<'_>,
    width: usize,
    height: usize,
    transform: [f64; 6],
    src_crs: &str,
    dst_crs: &str,
    gcps: Option<Vec<GcpTuple>>,
    order: u32,
    gcp_method: &str,
    rpc: Option<std::collections::HashMap<String, String>>,
    rpc_options: Option<std::collections::HashMap<String, String>>,
) -> PyResult<((f64, f64, f64, f64, f64, f64), usize, usize)> {
    let dst = SpatialRef::parse(dst_crs).py()?;
    if let Some(items) = rpc {
        let rpc = parse_rpc(items)?;
        let opts = parse_rpc_options(rpc_options)?;
        let (g, w, h) = py.detach(|| terrio::ops::warp::suggested_grid_rpc(width, height, &rpc, &opts, &dst)).py()?;
        return Ok(((g[0], g[1], g[2], g[3], g[4], g[5]), w, h));
    }
    let src = SpatialRef::parse(src_crs).py()?;
    let gcps = crate::raster::parse_gcps(gcps);
    let method = terrio::ops::gcp::GcpMethod::parse(gcp_method).py()?;
    if gcps.is_empty() && transform[1] * transform[5] - transform[2] * transform[4] == 0.0 {
        return Err(invalid("the source transform is degenerate"));
    }
    let (g, w, h) = py
        .detach(|| {
            if gcps.is_empty() {
                terrio::ops::warp::suggested_grid(width, height, transform, &src, &dst)
            } else {
                terrio::ops::warp::suggested_grid_gcps(width, height, &gcps, method, order, &src, &dst)
            }
        })
        .py()?;
    Ok(((g[0], g[1], g[2], g[3], g[4], g[5]), w, h))
}

/// Transforms coordinates with GCPs, as GDAL's GCP transformers do: from pixel/line to
/// georeferenced coordinates, or back with `inverse`.
///
/// Args:
///     gcps: The ground control points, `(pixel, line, x, y, z)` tuples.
///     xs: The first coordinates (columns, or map x with `inverse`).
///     ys: The second coordinates (rows, or map y).
///     inverse: Map coordinates to pixel/line.
///     method: `"polynomial"`, `"tps"`, `"homography"` or `"auto"`.
///     order: The polynomial order (0 picks GDAL's default).
///
/// Returns:
///     tuple: The transformed `(xs, ys)`.
///
/// Raises:
///     InvalidArgumentError: If the GCPs cannot be fitted.
#[pyfunction]
#[pyo3(signature = (gcps, xs, ys, inverse=false, method="polynomial", order=0))]
fn gcp_transform(
    gcps: Vec<GcpTuple>,
    xs: Vec<f64>,
    ys: Vec<f64>,
    inverse: bool,
    method: &str,
    order: u32,
) -> PyResult<(Vec<f64>, Vec<f64>)> {
    if xs.len() != ys.len() {
        return Err(invalid("xs and ys differ in length"));
    }
    let gcps = crate::raster::parse_gcps(Some(gcps));
    let method = terrio::ops::gcp::GcpMethod::parse(method).py()?;
    let t = terrio::ops::gcp::GcpTransformer::new(&gcps, method, order).py()?;
    Ok(xs.iter().zip(&ys).map(|(&x, &y)| if inverse { t.to_pixel(x, y) } else { t.to_geo(x, y) }).unzip())
}

/// GDAL's `RPC` metadata items as RPCs.
pub(crate) fn parse_rpc(items: std::collections::HashMap<String, String>) -> PyResult<terrio::ops::rpc::Rpc> {
    let items: Vec<(String, String)> = items.into_iter().collect();
    terrio::ops::rpc::Rpc::from_metadata(&items).py()
}

/// GDAL's `RPC_*` transformer options as the RPC transformer's options.
pub(crate) fn parse_rpc_options(options: Option<std::collections::HashMap<String, String>>) -> PyResult<terrio::ops::rpc::RpcOptions> {
    let items: Vec<(String, String)> = options.unwrap_or_default().into_iter().collect();
    terrio::ops::rpc::RpcOptions::from_options(&items).py()
}

/// GDAL's RPC transformer (`GDALCreateRPCTransformerV2`): pixel/line to WGS 84 longitude and
/// latitude by GDAL's iterative solution, and back by evaluating the polynomials, at the heights
/// given plus `RPC_HEIGHT` or a DEM's (`RPC_DEM`).
///
/// Args:
///     rpc: GDAL's `RPC` metadata items (`{"LINE_OFF": "...", ...}`).
///     options: GDAL's RPC transformer options (`RPC_HEIGHT`, `RPC_HEIGHT_SCALE`, `RPC_DEM`,
///         `RPC_DEMINTERPOLATION`, `RPC_DEM_MISSING_VALUE`, `RPC_DEM_SRS`,
///         `RPC_DEM_APPLY_VDATUM_SHIFT`, `RPC_PIXEL_ERROR_THRESHOLD`, `RPC_MAX_ITERATIONS`).
///
/// Raises:
///     InvalidArgumentError: If the RPCs or the options are invalid.
#[pyclass(module = "terrio", name = "RpcTransformer", frozen)]
pub(crate) struct PyRpcTransformer {
    inner: terrio::ops::rpc::RpcTransformer,
}

#[pymethods]
impl PyRpcTransformer {
    #[new]
    #[pyo3(signature = (rpc, options=None))]
    fn new(
        py: Python<'_>,
        rpc: std::collections::HashMap<String, String>,
        options: Option<std::collections::HashMap<String, String>>,
    ) -> PyResult<Self> {
        let rpc = parse_rpc(rpc)?;
        let opts = parse_rpc_options(options)?;
        let inner = py.detach(|| terrio::ops::rpc::RpcTransformer::new(&rpc, &opts)).py()?;
        Ok(Self { inner })
    }

    /// Transforms points: pixel/line to longitude/latitude, or back with `to_pixel`.
    ///
    /// Args:
    ///     xs: Columns (or longitudes).
    ///     ys: Rows (or latitudes).
    ///     zs: Heights above `RPC_HEIGHT` or the DEM, one per point.
    ///     to_pixel: Longitude/latitude to pixel/line.
    ///
    /// Returns:
    ///     tuple: The transformed `(xs, ys)` and whether each point succeeded (failed points
    ///     are infinite, as GDAL leaves them).
    #[pyo3(signature = (xs, ys, zs, to_pixel=false))]
    fn transform(
        &self,
        py: Python<'_>,
        mut xs: Vec<f64>,
        mut ys: Vec<f64>,
        zs: Vec<f64>,
        to_pixel: bool,
    ) -> PyResult<(Vec<f64>, Vec<f64>, Vec<bool>)> {
        if xs.len() != ys.len() || xs.len() != zs.len() {
            return Err(invalid("xs, ys and zs differ in length"));
        }
        let t = &self.inner;
        let ok = py.detach(|| {
            let mut ok = vec![true; xs.len()];
            if to_pixel {
                t.to_pixel_many(&mut xs, &mut ys, Some(&zs), &mut ok);
            } else {
                for i in 0..xs.len() {
                    match t.to_geo(xs[i], ys[i], zs[i]) {
                        Some((a, b)) => (xs[i], ys[i]) = (a, b),
                        None => ok[i] = false,
                    }
                }
            }
            for i in 0..xs.len() {
                if !ok[i] {
                    (xs[i], ys[i]) = (f64::INFINITY, f64::INFINITY);
                }
            }
            ok
        });
        Ok((xs, ys, ok))
    }
}
