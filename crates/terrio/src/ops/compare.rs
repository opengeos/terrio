//! Compare two rasters, as GDAL's `gdal raster compare` (and `gdalcompare.py`) do: their files
//! byte for byte, CRS, geotransform, sizes, bands (description, data type, nodata, color
//! interpretation, pixels), overviews, masks, metadata (default, `RPC` and `GEOLOCATION`
//! domains) and subdatasets.
//!
//! Every difference is one line of the report, worded as GDAL words it, and the difference count
//! is the number of lines (GDAL's exit status). Pixels are compared in strips of rows, so that a
//! large or remote raster is never held whole: only the strips (and overview levels) compared are
//! read. Besides GDAL's report, [`Comparison::bands`] gives the number of pixels compared and
//! differing and the largest absolute difference of every band, overview and mask compared,
//! including those with no difference.
//!
//! ```no_run
//! use terrio::ops::compare::{CompareOptions, compare};
//!
//! let c = compare("reference.tif", "output.tif", &CompareOptions { skip_binary: true, ..Default::default() })?;
//! print!("{}", c.to_text());
//! std::process::exit(c.difference_count() as i32);
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::crs::SpatialRef;
use crate::{Error, LazyRaster, Result, Window, WindowOptions};
use serde::Serialize;
use std::collections::BTreeMap;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// Which optional comparisons [`compare`] skips, as GDAL's `--skip-*` options. Sizes, band counts,
/// band descriptions, data types, color interpretations, nodata values and pixels are always
/// compared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompareOptions {
    /// Skip the byte-by-byte comparison of the two files (`--skip-binary`).
    pub skip_binary: bool,
    /// Skip the CRS comparison (`--skip-crs`).
    pub skip_crs: bool,
    /// Skip the geotransform comparison (`--skip-geotransform`).
    pub skip_geotransform: bool,
    /// Skip the overview comparison (`--skip-overview`).
    pub skip_overview: bool,
    /// Skip the comparison of the dataset's and the bands' default metadata domain
    /// (`--skip-metadata`).
    pub skip_metadata: bool,
    /// Skip the comparison of the `RPC` metadata domain (`--skip-rpc`).
    pub skip_rpc: bool,
    /// Skip the comparison of the `GEOLOCATION` metadata domain (`--skip-geolocation`).
    pub skip_geolocation: bool,
    /// Skip the comparison of subdatasets (`--skip-subdataset`).
    pub skip_subdataset: bool,
}

impl CompareOptions {
    /// Every optional comparison skipped (`--skip-all-optional`).
    pub fn skip_all_optional() -> Self {
        Self {
            skip_binary: true,
            skip_crs: true,
            skip_geotransform: true,
            skip_overview: true,
            skip_metadata: true,
            skip_rpc: true,
            skip_geolocation: true,
            skip_subdataset: true,
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Difference {
    /// What differs: `binary`, `subdataset_count`, `crs`, `geotransform`, `band_count`, `width`,
    /// `height`, `band_width`, `band_height`, `description`, `data_type`, `nodata`,
    /// `color_interpretation`, `pixels`, `max_difference`, `overview_count`, `mask_flags`,
    /// `metadata`.
    pub kind: &'static str,
    /// The band, overview or mask concerned, as GDAL names it (`1`, `overview of band 1`, `mask of
    /// band 1`), for band differences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub band: Option<String>,
    /// The subdataset concerned, from 1, for differences between subdatasets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdataset: Option<usize>,
    /// The report line, as GDAL writes it.
    pub message: String,
}

/// The pixel comparison of a band, an overview or a mask.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BandComparison {
    /// The band, as GDAL names it in its report: `1`, `overview of band 1`, `mask of band 1`.
    pub band: String,
    /// The subdataset, from 1, for bands of subdatasets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdataset: Option<usize>,
    /// The data type the values were compared in: the union of both bands' types, as GDAL's
    /// `GDALDataTypeUnion` (`Float32` for a `Byte` and a `Float32` band).
    pub data_type: String,
    /// The number of pixels compared.
    pub pixels: u64,
    /// The number of pixels that differ. A NaN in only one of the bands differs; NaNs in both do
    /// not.
    pub pixels_differing: u64,
    /// The largest absolute difference between two non-NaN values (0 when none differ).
    /// Infinities serialize as `"inf"`.
    #[serde(serialize_with = "serialize_float")]
    pub max_abs_diff: f64,
}

/// The result of [`compare`].
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Comparison {
    /// The differences, in GDAL's order: one per line of its report.
    pub differences: Vec<Difference>,
    /// The pixel comparisons made, including those that found no difference. Empty when the files
    /// are identical, or when sizes or band counts differ.
    pub bands: Vec<BandComparison>,
}

impl Comparison {
    /// The number of differences: GDAL's exit status.
    pub fn difference_count(&self) -> usize {
        self.differences.len()
    }

    /// The report as GDAL prints it: one line per difference.
    pub fn to_text(&self) -> String {
        self.differences.iter().map(|d| format!("{}\n", d.message)).collect()
    }

    /// The report as JSON: `{"difference_count", "differences", "bands"}`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.insert("difference_count".into(), self.difference_count().into());
        }
        v
    }

    fn push(&mut self, ctx: &Ctx, kind: &'static str, band: Option<&str>, message: String) {
        self.differences.push(Difference { kind, band: band.map(str::to_string), subdataset: ctx.subdataset, message });
    }
}

fn serialize_float<S: serde::Serializer>(v: &f64, s: S) -> std::result::Result<S::Ok, S::Error> {
    if v.is_finite() { s.serialize_f64(*v) } else { s.serialize_str(&c_float(*v)) }
}

/// Compares the rasters at two paths (local, remote or in archives), as `gdal raster compare
/// <reference> <input>`: the files byte for byte first (identical files have no differences,
/// and nothing else is compared), then the datasets, then their subdatasets.
pub fn compare(reference: &str, input: &str, options: &CompareOptions) -> Result<Comparison> {
    let mut out = Comparison::default();
    if !options.skip_binary {
        match binary_difference(reference, input)? {
            Some(None) => return Ok(out),
            Some(Some(message)) => out.push(&Ctx::default(), "binary", None, message),
            None => {}
        }
    }
    let mut r = crate::open_raster_lazy(reference)?;
    let mut i = crate::open_raster_lazy(input)?;
    let (ref_sub, in_sub) = (subdataset_names(&r), subdataset_names(&i));
    let mut skip_subdataset = options.skip_subdataset;
    if !skip_subdataset && ref_sub.len() != in_sub.len() {
        out.push(
            &Ctx::default(),
            "subdataset_count",
            None,
            format!("Reference dataset has {} subdataset(s) whereas input dataset has {} one(s).", ref_sub.len(), in_sub.len()),
        );
        skip_subdataset = true;
    }
    let paths = Paths { reference: Some(reference), input: Some(input) };
    compare_datasets(&mut out, &mut r, &mut i, options, &Ctx::default(), paths)?;
    if !skip_subdataset {
        for (n, (a, b)) in ref_sub.iter().zip(&in_sub).enumerate() {
            let mut sa = crate::open_raster_lazy(a)?;
            let mut sb = crate::open_raster_lazy(b)?;
            let ctx = Ctx { subdataset: Some(n + 1) };
            compare_datasets(&mut out, &mut sa, &mut sb, options, &ctx, Paths { reference: Some(a), input: Some(b) })?;
        }
    }
    Ok(out)
}

/// Compares two open rasters, as [`compare`] compares the datasets at two paths, without the
/// binary, `RPC` and subdataset comparisons (which need the files).
pub fn compare_rasters(reference: &mut LazyRaster, input: &mut LazyRaster, options: &CompareOptions) -> Result<Comparison> {
    let mut out = Comparison::default();
    compare_datasets(&mut out, reference, input, options, &Ctx::default(), Paths::default())?;
    Ok(out)
}

/// GDAL's binary comparison of two files' bytes: `None` when they are identical, else the report
/// line.
pub fn compare_bytes(reference: &[u8], input: &[u8]) -> Option<String> {
    if reference.len() != input.len() {
        return Some(size_message(reference.len() as u64, input.len() as u64));
    }
    (reference != input).then(|| "Reference file and input file differ at the binary level.".to_string())
}

fn size_message(a: u64, b: u64) -> String {
    format!("Reference file has size {a} bytes, whereas input file has size {b} bytes.")
}

#[derive(Default, Clone, Copy)]
struct Ctx {
    subdataset: Option<usize>,
}

#[derive(Default, Clone, Copy)]
struct Paths<'a> {
    reference: Option<&'a str>,
    input: Option<&'a str>,
}

/// What GDAL reads from a local file besides what the raster's reader holds: a GeoTIFF's
/// `TIFFTAG_*` items and RPC coefficients, and the metadata of a `.aux.xml` sidecar (PAM), which
/// overrides the file's.
#[derive(Default)]
struct Extras {
    dataset: Vec<(String, String)>,
    pam_dataset: Vec<(String, String)>,
    bands: BTreeMap<usize, Vec<(String, String)>>,
    rpc: Vec<(String, String)>,
    /// The `RESAMPLING` item of every overview band.
    overview: Vec<(String, String)>,
}

/// The default-domain `MDI` items of a PAM `Metadata` element's children.
fn mdi_items(e: &crate::ops::vrt::xml::Element) -> Vec<(String, String)> {
    e.children
        .iter()
        .filter(|m| m.name == "Metadata" && m.attr("domain").is_none_or(str::is_empty) && m.attr("format").is_none())
        .flat_map(|m| m.children.iter().filter(|i| i.name == "MDI").filter_map(|i| Some((i.attr("key")?.to_string(), i.text.clone()))))
        .collect()
}

/// The file and subdataset of a GDAL subdataset name: `NETCDF:"file.nc":var`, `HDF5:file.h5://ds`.
fn split_subdataset(name: &str) -> Option<(&str, &str)> {
    let (prefix, rest) = name.split_once(':')?;
    if prefix.len() < 2 || !prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    if let Some(rest) = rest.strip_prefix('"') {
        let (file, sub) = rest.split_once("\":")?;
        return Some((file, sub));
    }
    rest.split_once(':')
}

fn file_extras(path: Option<&str>) -> Extras {
    let mut out = Extras::default();
    let Some(path) = path else { return out };
    // A subdataset's PAM metadata is in its file's sidecar, under its name.
    if let Some((file, sub)) = split_subdataset(path)
        && std::path::Path::new(file).is_file()
    {
        let mut aux = std::ffi::OsString::from(file);
        aux.push(".aux.xml");
        if let Ok(xml) = std::fs::read_to_string(&aux)
            && let Ok(root) = crate::ops::vrt::xml::parse(&xml)
            && let Some(pam) =
                root.children.iter().find(|e| e.name == "Subdataset" && e.attr("name") == Some(sub)).and_then(|e| e.child("PAMDataset"))
        {
            add_pam(&mut out, pam);
        }
        return out;
    }
    let crate::vfs::Location::Local(p) = crate::vfs::Location::parse(path) else { return out };
    if !p.is_file() {
        return out;
    }
    if let Some(tags) = crate::geotiff::read_compare_geo_tags(&p) {
        out.dataset = tags.tiff_tag_items;
        out.overview = tags.overview_resampling.map(|r| vec![("RESAMPLING".to_string(), r)]).unwrap_or_default();
        out.rpc = tags.rpc.map(|r| rpc_items(&r)).unwrap_or_default();
        for (b, k, v) in tags.band_metadata {
            out.bands.entry(b + 1).or_default().push((k, v));
        }
    }
    let mut aux = p.as_os_str().to_owned();
    aux.push(".aux.xml");
    if let Ok(xml) = std::fs::read_to_string(&aux)
        && let Ok(root) = crate::ops::vrt::xml::parse(&xml)
    {
        add_pam(&mut out, &root);
    }
    out
}

/// Adds the metadata of a `PAMDataset` element.
fn add_pam(out: &mut Extras, root: &crate::ops::vrt::xml::Element) {
    out.pam_dataset = mdi_items(root);
    for band in root.children.iter().filter(|e| e.name == "PAMRasterBand") {
        if let Some(b) = band.attr("band").and_then(|b| b.parse::<usize>().ok()) {
            let file = out.bands.remove(&b).unwrap_or_default();
            out.bands.insert(b, merged(file, &mdi_items(band)));
        }
    }
}

/// What a band's metadata holds besides the reader's: the file's band items, and its overviews'.
#[derive(Default)]
struct BandExtras {
    items: Vec<(String, String)>,
    overview: Vec<(String, String)>,
}

/// Items with the overrides' values replacing theirs, as GDAL merges PAM metadata.
fn merged(mut items: Vec<(String, String)>, overrides: &[(String, String)]) -> Vec<(String, String)> {
    for (k, v) in overrides {
        match items.iter_mut().find(|(key, _)| key == k) {
            Some(item) => item.1 = v.clone(),
            None => items.push((k.clone(), v.clone())),
        }
    }
    items
}

/// A file opened for the binary comparison: its size and a reader.
struct Bytes {
    len: u64,
    reader: Box<dyn std::io::Read>,
}

fn open_bytes(path: &str, which: &str) -> Result<Option<Bytes>> {
    use crate::vfs::Location;
    let loc = Location::parse(path);
    match &loc {
        Location::Local(p) => {
            if crate::memfs::is_virtual(p) {
                let bytes = crate::memfs::read(p)?;
                return Ok(Some(Bytes { len: bytes.len() as u64, reader: Box::new(std::io::Cursor::new(bytes)) }));
            }
            if !p.is_file() {
                crate::warnings::warn(format!("{which} dataset '{path}' is not a file. Skipping binary file comparison"));
                return Ok(None);
            }
            let f = std::fs::File::open(p)?;
            Ok(Some(Bytes { len: f.metadata()?.len(), reader: Box::new(f) }))
        }
        #[cfg(remote)]
        Location::Url(_) | Location::S3 { .. } => {
            let remote = loc.remote().ok_or_else(|| Error::InvalidArgument("not a remote location".into()))?;
            let r = crate::http::RangeReader::open(&remote)?;
            Ok(Some(Bytes { len: r.len(), reader: Box::new(r) }))
        }
        #[cfg(not(target_family = "wasm"))]
        Location::Zip { inner: Some(_), .. } => {
            let local = crate::vfs::resolve(path)?;
            let f = std::fs::File::open(local.path())?;
            Ok(Some(Bytes { len: f.metadata()?.len(), reader: Box::new(f) }))
        }
        _ => {
            crate::warnings::warn(format!("{which} dataset '{path}' is not a file. Skipping binary file comparison"));
            Ok(None)
        }
    }
}

/// The binary comparison: `None` when it was skipped (a dataset that is not a file), `Some(None)`
/// for identical files, else the report line. Sizes are compared first, so that files of
/// different sizes are not read; equal-sized files are read a megabyte at a time.
fn binary_difference(reference: &str, input: &str) -> Result<Option<Option<String>>> {
    let Some(mut a) = open_bytes(reference, "Reference")? else { return Ok(None) };
    let Some(mut b) = open_bytes(input, "Input")? else { return Ok(None) };
    if a.len != b.len {
        return Ok(Some(Some(size_message(a.len, b.len))));
    }
    const CHUNK: usize = 1 << 20;
    let (mut ba, mut bb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    let mut left = a.len;
    while left > 0 {
        let n = left.min(CHUNK as u64) as usize;
        a.reader
            .read_exact(&mut ba[..n])
            .map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("failed to fully read reference file: {e}"))))?;
        b.reader
            .read_exact(&mut bb[..n])
            .map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("failed to fully read input file: {e}"))))?;
        if ba[..n] != bb[..n] {
            return Ok(Some(Some("Reference file and input file differ at the binary level.".into())));
        }
        left -= n as u64;
    }
    Ok(Some(None))
}

fn subdataset_names(r: &LazyRaster) -> Vec<String> {
    let md = r.metadata();
    let mut names = Vec::new();
    for n in 1.. {
        let key = format!("SUBDATASET_{n}_NAME");
        match md.iter().find(|(k, _)| *k == key) {
            Some((_, v)) => names.push(v.clone()),
            None => break,
        }
    }
    names
}

/// C's `std::to_string(double)`: `printf("%f")`, with `nan` and `inf`.
fn c_float(v: f64) -> String {
    if v.is_nan() {
        if v.is_sign_negative() { "-nan".into() } else { "nan".into() }
    } else if v.is_infinite() {
        if v > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        format!("{v:.6}")
    }
}

fn has_geo_transform(r: &LazyRaster) -> bool {
    let gt = r.geo_transform();
    gt != [0.0, 1.0, 0.0, 0.0, 0.0, 1.0] && gt.iter().all(|v| v.is_finite())
}

fn compare_crs(out: &mut Comparison, ctx: &Ctx, a: &SpatialRef, b: &SpatialRef) {
    match (a.is_unknown(), b.is_unknown()) {
        (true, true) => {}
        (true, false) => out.push(ctx, "crs", None, "Reference dataset has no CRS, but input dataset has one.".into()),
        (false, true) => out.push(ctx, "crs", None, "Reference dataset has a CRS, but input dataset has none.".into()),
        (false, false) if a.same_as(b) => {}
        (false, false) => out.push(
            ctx,
            "crs",
            None,
            format!(
                "Reference and input CRS are not equivalent. Reference one is '{}'. Input one is '{}'",
                a.to_wkt().unwrap_or_else(|| a.label()),
                b.to_wkt().unwrap_or_else(|| b.label())
            ),
        ),
    }
}

fn compare_geo_transform(out: &mut Comparison, ctx: &Ctx, a: &LazyRaster, b: &LazyRaster) {
    match (has_geo_transform(a), has_geo_transform(b)) {
        (false, false) => return,
        (false, true) => {
            return out.push(ctx, "geotransform", None, "Reference dataset has no geotransform, but input one has one.".into());
        }
        (true, false) => {
            return out.push(ctx, "geotransform", None, "Reference dataset has a geotransform, but input one has none.".into());
        }
        (true, true) => {}
    }
    let (ga, gb) = (a.geo_transform(), b.geo_transform());
    let differs = ga.iter().zip(&gb).any(|(r, i)| if *r != 0.0 { (r - i).abs() > 1e-10 * r.abs() } else { (r - i).abs() > 1e-10 });
    if differs {
        let join = |g: &[f64; 6]| g.iter().map(|v| c_float(*v)).collect::<Vec<_>>().join(",");
        out.push(
            ctx,
            "geotransform",
            None,
            format!(
                "Geotransform of reference and input dataset are not equivalent. Reference geotransform is ({}). Input geotransform is ({})",
                join(&ga),
                join(&gb)
            ),
        );
    }
}

/// Keys GDAL leaves out of metadata comparisons.
const IGNORED_KEYS: [&str; 3] = ["backend", "ERR_BIAS", "ERR_RAND"];

fn compare_metadata(out: &mut Comparison, ctx: &Ctx, domain: &str, a: &[(String, String)], b: &[(String, String)]) {
    let map = |items: &[(String, String)]| -> BTreeMap<String, String> {
        items.iter().filter(|(k, _)| !IGNORED_KEYS.contains(&k.as_str())).cloned().collect()
    };
    let (ma, mb) = (map(a), map(b));
    for (k, va) in &ma {
        match mb.get(k) {
            None => out.push(ctx, "metadata", None, format!("Reference metadata {domain} contains key '{k}' but input metadata does not.")),
            // Always the current date.
            Some(_) if k == "NITF_FDT" => {}
            Some(vb) => {
                let (va, vb) = if domain == "RPC" { (va.trim_matches(' '), vb.trim_matches(' ')) } else { (va.as_str(), vb.as_str()) };
                if va != vb {
                    out.push(
                        ctx,
                        "metadata",
                        None,
                        format!("Reference metadata {domain} has value '{va}' for key '{k}' but input metadata has value '{vb}'."),
                    );
                }
            }
        }
    }
    for k in mb.keys() {
        if !ma.contains_key(k) {
            out.push(ctx, "metadata", None, format!("Input metadata {domain} contains key '{k}' but reference metadata does not."));
        }
    }
}

/// The band of a `BAND_<n>#<key>` item, as terrio's netCDF reader names band items in the
/// dataset's metadata, and its key.
fn band_item(key: &str) -> Option<(usize, &str)> {
    let (band, key) = key.strip_prefix("BAND_")?.split_once('#')?;
    Some((band.parse().ok()?, key))
}

/// The dataset's metadata (default domain), without subdataset lists, band items and terrio's
/// own items.
fn dataset_metadata(r: &LazyRaster) -> Vec<(String, String)> {
    r.metadata()
        .into_iter()
        .filter(|(k, _)| {
            !(k.starts_with("SUBDATASET_") && (k.ends_with("_NAME") || k.ends_with("_DESC")))
                && !k.starts_with(crate::info::INTERNAL)
                && band_item(k).is_none()
        })
        .collect()
}

/// GDAL's `RPC` metadata items for the 92 coefficients of a GeoTIFF RPC tag.
pub(crate) fn rpc_items(rpc: &[f64]) -> Vec<(String, String)> {
    if rpc.len() != 92 {
        return Vec::new();
    }
    let g = |v: f64| crate::drivers::common::fmt_g(v, 15);
    let names = [
        "ERR_BIAS",
        "ERR_RAND",
        "LINE_OFF",
        "SAMP_OFF",
        "LAT_OFF",
        "LONG_OFF",
        "HEIGHT_OFF",
        "LINE_SCALE",
        "SAMP_SCALE",
        "LAT_SCALE",
        "LONG_SCALE",
        "HEIGHT_SCALE",
    ];
    let mut items: Vec<(String, String)> = names.iter().zip(rpc).map(|(k, v)| (k.to_string(), g(*v))).collect();
    for (i, k) in ["LINE_NUM_COEFF", "LINE_DEN_COEFF", "SAMP_NUM_COEFF", "SAMP_DEN_COEFF"].iter().enumerate() {
        let start = 12 + 20 * i;
        items.push((k.to_string(), rpc[start..start + 20].iter().map(|v| g(*v)).collect::<Vec<_>>().join(" ")));
    }
    items
}

fn geolocation_metadata(r: &LazyRaster) -> Vec<(String, String)> {
    r.dataset().and_then(|d| d.geolocation.as_ref()).map(|g| g.items()).unwrap_or_default()
}

fn check_real(r: &LazyRaster) -> Result<()> {
    if r.dataset().is_some_and(|d| d.complex) {
        return Err(Error::Unsupported("comparing complex rasters is not supported".into()));
    }
    Ok(())
}

fn compare_datasets(
    out: &mut Comparison,
    a: &mut LazyRaster,
    b: &mut LazyRaster,
    options: &CompareOptions,
    ctx: &Ctx,
    paths: Paths<'_>,
) -> Result<()> {
    check_real(a)?;
    check_real(b)?;
    if !options.skip_crs {
        compare_crs(out, ctx, &a.crs(), &b.crs());
    }
    if !options.skip_geotransform {
        compare_geo_transform(out, ctx, a, b);
    }
    let mut same = true;
    if a.band_count() != b.band_count() {
        out.push(
            ctx,
            "band_count",
            None,
            format!("Reference dataset has {} band(s), but input dataset has {}", a.band_count(), b.band_count()),
        );
        same = false;
    }
    if a.width() != b.width() {
        out.push(ctx, "width", None, format!("Reference dataset width is {}, but input dataset width is {}", a.width(), b.width()));
        same = false;
    }
    if a.height() != b.height() {
        out.push(ctx, "height", None, format!("Reference dataset height is {}, but input dataset height is {}", a.height(), b.height()));
        same = false;
    }
    let (ea, eb) = if options.skip_metadata && options.skip_rpc {
        (Extras::default(), Extras::default())
    } else {
        (file_extras(paths.reference), file_extras(paths.input))
    };
    if !options.skip_metadata {
        let md = |r: &LazyRaster, e: &Extras| {
            let mut items = dataset_metadata(r);
            items.extend(e.dataset.iter().cloned());
            merged(items, &e.pam_dataset)
        };
        compare_metadata(out, ctx, "(dataset default metadata domain)", &md(a, &ea), &md(b, &eb));
    }
    if !options.skip_rpc {
        compare_metadata(out, ctx, "RPC", &ea.rpc, &eb.rpc);
    }
    if !options.skip_geolocation {
        compare_metadata(out, ctx, "GEOLOCATION", &geolocation_metadata(a), &geolocation_metadata(b));
    }
    if !same {
        return Ok(());
    }
    let mut cache = LevelCache::default();
    for band in 1..=a.band_count() {
        let extra = |e: &'_ Extras| BandExtras { items: e.bands.get(&band).cloned().unwrap_or_default(), overview: e.overview.clone() };
        let extras = (extra(&ea), extra(&eb));
        compare_level(out, &mut cache, a, b, band, &band.to_string(), Level::Image(None), options, ctx, &extras)?;
    }
    Ok(())
}

/// A band, an overview of one or a mask: what GDAL compares as a band.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Level {
    /// Full resolution (`None`) or an overview.
    Image(Option<usize>),
    /// The per-dataset mask, at full resolution or of an overview.
    Mask(Option<usize>),
}

/// Pixel comparisons of whole levels, all bands at once: each strip of a level is read once for
/// every band compared.
#[derive(Default)]
struct LevelCache {
    levels: BTreeMap<Level, Vec<PixelStats>>,
}

#[derive(Clone, Copy)]
struct PixelStats {
    union: DataType,
    pixels: u64,
    differing: u64,
    max: Max,
}

#[derive(Clone, Copy)]
enum Max {
    Int(u128),
    Float(f64),
}

impl Max {
    fn as_f64(self) -> f64 {
        match self {
            Max::Int(v) => v as f64,
            Max::Float(v) => v,
        }
    }

    fn text(self) -> String {
        match self {
            Max::Int(v) => v.to_string(),
            Max::Float(v) if v.is_infinite() => "inf".into(),
            Max::Float(v) if v.is_nan() => "nan".into(),
            Max::Float(v) => c_float(v),
        }
    }
}

/// GDAL's `GDALDataTypeUnion`: the smallest type that holds the values of both.
pub(crate) fn data_type_union(a: DataType, b: DataType) -> DataType {
    use DataType::*;
    if a == b {
        return a;
    }
    let float = |t: DataType| matches!(t, F32 | F64);
    if float(a) || float(b) {
        let wide = |t: DataType| matches!(t, F64 | U32 | I32 | U64 | I64);
        return if wide(a) || wide(b) { F64 } else { F32 };
    }
    let signed = |t: DataType| matches!(t, I8 | I16 | I32 | I64);
    let bits = |t: DataType| t.size_bytes() * 8;
    let (sa, sb) = (signed(a), signed(b));
    if sa == sb {
        return if bits(a) >= bits(b) { a } else { b };
    }
    // One signed, one unsigned: the signed type of the unsigned one's bits plus one, at least
    // the signed one's.
    let (s, u) = if sa { (a, b) } else { (b, a) };
    let need = (bits(u) + 1).max(bits(s));
    match need {
        0..=8 => I8,
        9..=16 => I16,
        17..=32 => I32,
        33..=64 => I64,
        _ => F64,
    }
}

/// An integer value, exactly.
fn int_at(d: &RasterData, i: usize) -> i128 {
    match d {
        RasterData::U8(v) => v[i] as i128,
        RasterData::I8(v) => v[i] as i128,
        RasterData::U16(v) => v[i] as i128,
        RasterData::I16(v) => v[i] as i128,
        RasterData::U32(v) => v[i] as i128,
        RasterData::I32(v) => v[i] as i128,
        RasterData::U64(v) => v[i] as i128,
        RasterData::I64(v) => v[i] as i128,
        RasterData::F32(v) => v[i] as i128,
        RasterData::F64(v) => v[i] as i128,
    }
}

impl PixelStats {
    fn new(union: DataType) -> Self {
        let max = if matches!(union, DataType::F32 | DataType::F64) { Max::Float(0.0) } else { Max::Int(0) };
        Self { union, pixels: 0, differing: 0, max }
    }

    /// Compares `n` values from `offset` of two buffers, as GDAL's `CompareVectors` does in the
    /// union type.
    fn add(&mut self, a: &RasterData, b: &RasterData, offset: usize, n: usize) {
        self.pixels += n as u64;
        match (self.union, &mut self.max) {
            (DataType::F32, Max::Float(max)) => {
                for i in offset..offset + n {
                    let (x, y) = (a.get_f64(i) as f32, b.get_f64(i) as f32);
                    if x.is_nan() || y.is_nan() {
                        self.differing += (x.is_nan() != y.is_nan()) as u64;
                        continue;
                    }
                    if x == y {
                        continue;
                    }
                    let d = if x >= y { x - y } else { y - x };
                    if d > 0.0 {
                        self.differing += 1;
                        *max = max.max(f64::from(d));
                    }
                }
            }
            (DataType::F64, Max::Float(max)) => {
                for i in offset..offset + n {
                    let (x, y) = (a.get_f64(i), b.get_f64(i));
                    if x.is_nan() || y.is_nan() {
                        self.differing += (x.is_nan() != y.is_nan()) as u64;
                        continue;
                    }
                    if x == y {
                        continue;
                    }
                    let d = if x >= y { x - y } else { y - x };
                    if d > 0.0 {
                        self.differing += 1;
                        *max = max.max(d);
                    }
                }
            }
            (_, Max::Int(max)) => {
                for i in offset..offset + n {
                    let d = (int_at(a, i) - int_at(b, i)).unsigned_abs();
                    self.differing += (d > 0) as u64;
                    *max = (*max).max(d);
                }
            }
            _ => {}
        }
    }
}

fn level_size(r: &LazyRaster, overview: Option<usize>) -> Option<(usize, usize)> {
    match overview {
        None => Some((r.width(), r.height())),
        Some(i) => r.overviews().get(i).copied(),
    }
}

impl LevelCache {
    /// The pixel comparison of every band of a level (one entry for a mask level).
    fn get(&mut self, a: &mut LazyRaster, b: &mut LazyRaster, level: Level) -> Result<Option<&[PixelStats]>> {
        use std::collections::btree_map::Entry;
        match self.levels.entry(level) {
            Entry::Occupied(e) => Ok(Some(e.into_mut().as_slice())),
            Entry::Vacant(e) => {
                let stats = match level {
                    Level::Image(ovr) => compare_image(a, b, ovr)?,
                    Level::Mask(ovr) => compare_mask(a, b, ovr)?,
                };
                Ok(stats.map(|s| e.insert(s).as_slice()))
            }
        }
    }
}

fn compare_image(a: &mut LazyRaster, b: &mut LazyRaster, overview: Option<usize>) -> Result<Option<Vec<PixelStats>>> {
    let (Some((w, h)), Some(size_b)) = (level_size(a, overview), level_size(b, overview)) else { return Ok(None) };
    if (w, h) != size_b || w == 0 || h == 0 {
        return Ok(None);
    }
    let bands = a.band_count();
    let mut stats: Vec<_> =
        (1..=bands).map(|band| PixelStats::new(data_type_union(a.band_data_type(band), b.band_data_type(band)))).collect();
    // Keep one multiband read for ordinary homogeneous rasters. Mixed VRT bands
    // are read separately so integer bands never pass through a floating buffer.
    let homogeneous =
        (1..=bands).all(|band| a.band_data_type(band) == a.band_data_type(1) && b.band_data_type(band) == b.band_data_type(1));
    let groups: Vec<Vec<usize>> = if homogeneous { vec![(1..=bands).collect()] } else { (1..=bands).map(|band| vec![band]).collect() };
    for group in groups {
        let rows = super::strips::small_strip_rows(w, group.len(), 8, a.block_size().map(|(_, bh)| bh));
        let options = WindowOptions { bands: group.clone(), overview };
        let mut y = 0;
        while y < h {
            let n = rows.min(h - y);
            let window = Window::new(0, y, w, n);
            let ra = a.read_window(window, &options)?.raster.data;
            let rb = b.read_window(window, &options)?.raster.data;
            let plane = w * n;
            for (i, band) in group.iter().enumerate() {
                stats[*band - 1].add(&ra, &rb, i * plane, plane);
            }
            y += n;
        }
    }
    Ok(Some(stats))
}

fn compare_mask(a: &mut LazyRaster, b: &mut LazyRaster, overview: Option<usize>) -> Result<Option<Vec<PixelStats>>> {
    let (Some((w, h)), Some(size_b)) = (level_size(a, overview), level_size(b, overview)) else { return Ok(None) };
    if (w, h) != size_b || w == 0 || h == 0 {
        return Ok(None);
    }
    let mut stats = PixelStats::new(DataType::U8);
    let rows = super::strips::small_strip_rows(w, 1, 1, a.block_size().map(|(_, bh)| bh));
    let mut y = 0;
    while y < h {
        let n = rows.min(h - y);
        let window = Window::new(0, y, w, n);
        let ma = a.read_mask(window, overview)?.unwrap_or_else(|| vec![255; w * n]);
        let mb = b.read_mask(window, overview)?.unwrap_or_else(|| vec![255; w * n]);
        stats.add(&RasterData::U8(ma), &RasterData::U8(mb), 0, w * n);
        y += n;
    }
    Ok(Some(vec![stats]))
}

/// GDAL's mask flags of a band: `GMF_PER_DATASET` (2) for an internal or sidecar mask,
/// `GMF_NODATA` (8) for a nodata value, `GMF_ALPHA | GMF_PER_DATASET` (6) for the colour bands of
/// a gray+alpha or RGBA raster, else `GMF_ALL_VALID` (1).
fn mask_flags(r: &mut LazyRaster, band: usize) -> u32 {
    if r.has_mask() {
        return 2;
    }
    if let Some(nd) = r.band_nodata(band) {
        // A nodata value the data type cannot hold masks nothing, as GDAL's `IsNoDataInRange`.
        if nodata_in_range(nd, r.band_data_type(band)) {
            return 8;
        }
        return 1;
    }
    let n = r.band_count();
    if matches!(r.band_data_type(band), DataType::U8 | DataType::U16)
        && let Some(alpha) = r.alpha_band()
        && alpha == n
        && matches!(n, 2 | 4)
        && band != alpha
    {
        return 6;
    }
    1
}

/// GDAL's `GDALNoDataMaskBand::IsNoDataInRange`.
fn nodata_in_range(v: f64, t: DataType) -> bool {
    let range = |lo: f64, hi: f64| v >= lo && v <= hi;
    match t {
        DataType::U8 => range(0.0, 255.0),
        DataType::I8 => range(-128.0, 127.0),
        DataType::U16 => range(0.0, 65535.0),
        DataType::I16 => range(-32768.0, 32767.0),
        DataType::U32 => range(0.0, u32::MAX as f64),
        DataType::I32 => range(i32::MIN as f64, i32::MAX as f64),
        DataType::U64 => range(0.0, u64::MAX as f64),
        DataType::I64 => range(i64::MIN as f64, i64::MAX as f64),
        DataType::F32 => v.is_nan() || v.is_infinite() || range(f32::MIN as f64, f32::MAX as f64),
        DataType::F64 => true,
    }
}

/// What GDAL compares of one band, overview or mask besides its pixels.
struct BandProps {
    width: usize,
    height: usize,
    description: String,
    data_type: DataType,
    nodata: Option<f64>,
    color: String,
}

fn band_props(r: &LazyRaster, band: usize, level: Level) -> Option<BandProps> {
    let header = r.header();
    let meta = header.band_metadata.get(band - 1);
    Some(match level {
        Level::Image(ovr) => {
            let (width, height) = level_size(r, ovr)?;
            BandProps {
                width,
                height,
                description: if ovr.is_none() { meta.and_then(|m| m.description.clone()).unwrap_or_default() } else { String::new() },
                data_type: r.band_data_type(band),
                nodata: r.band_nodata(band),
                color: header.band_color_interp(band).to_string(),
            }
        }
        Level::Mask(ovr) => {
            let (width, height) = level_size(r, ovr)?;
            BandProps { width, height, description: String::new(), data_type: DataType::U8, nodata: None, color: "Undefined".into() }
        }
    })
}

fn band_items(r: &LazyRaster, band: usize, level: Level) -> Vec<(String, String)> {
    match level {
        Level::Image(None) => {
            let mut items = r.header().band_metadata.get(band - 1).map(|m| m.items.clone()).unwrap_or_default();
            items.extend(r.metadata().into_iter().filter_map(|(k, v)| match band_item(&k) {
                Some((b, key)) if b == band => Some((key.to_string(), v)),
                _ => None,
            }));
            items
        }
        _ => Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_level(
    out: &mut Comparison,
    cache: &mut LevelCache,
    a: &mut LazyRaster,
    b: &mut LazyRaster,
    band: usize,
    id: &str,
    level: Level,
    options: &CompareOptions,
    ctx: &Ctx,
    extras: &(BandExtras, BandExtras),
) -> Result<()> {
    let (Some(pa), Some(pb)) = (band_props(a, band, level), band_props(b, band, level)) else { return Ok(()) };
    let mut same_size = true;
    if pa.width != pb.width {
        out.push(ctx, "band_width", Some(id), format!("Reference band width is {}, but input band width is {}", pa.width, pb.width));
        same_size = false;
    }
    if pa.height != pb.height {
        out.push(ctx, "band_height", Some(id), format!("Reference band height is {}, but input band height is {}", pa.height, pb.height));
        same_size = false;
    }
    if pa.description != pb.description {
        out.push(
            ctx,
            "description",
            Some(id),
            format!("Reference band {id} has description {}, but input band has description {}", pa.description, pb.description),
        );
    }
    if pa.data_type != pb.data_type {
        out.push(
            ctx,
            "data_type",
            Some(id),
            format!(
                "Reference band {id} has data type {}, but input band has data type {}",
                crate::info::gdal_type_name(pa.data_type),
                crate::info::gdal_type_name(pb.data_type)
            ),
        );
    }
    match (pa.nodata, pb.nodata) {
        (None, None) => {}
        (Some(x), None) => {
            out.push(ctx, "nodata", Some(id), format!("Reference band {id} has nodata value {}, but input band has none.", c_float(x)))
        }
        (None, Some(y)) => out.push(
            ctx,
            "nodata",
            Some(id),
            format!("Reference band {id} has no nodata value, but input band has no data value {}.", c_float(y)),
        ),
        (Some(x), Some(y)) if (x.is_nan() && y.is_nan()) || x == y => {}
        (Some(x), Some(y)) => out.push(
            ctx,
            "nodata",
            Some(id),
            format!("Reference band {id} has nodata value {}, but input band has no data value {}.", c_float(x), c_float(y)),
        ),
    }
    if pa.color != pb.color {
        out.push(
            ctx,
            "color_interpretation",
            Some(id),
            format!("Reference band {id} has color interpretation {}, but input band has color interpretation {}", pa.color, pb.color),
        );
    }
    if !same_size {
        return Ok(());
    }
    if let Some(stats) = cache.get(a, b, level)? {
        let s = match level {
            Level::Image(_) => stats[band - 1],
            Level::Mask(_) => stats[0],
        };
        out.bands.push(BandComparison {
            band: id.to_string(),
            subdataset: ctx.subdataset,
            data_type: crate::info::gdal_type_name(s.union).to_string(),
            pixels: s.pixels,
            pixels_differing: s.differing,
            max_abs_diff: s.max.as_f64(),
        });
        if s.differing > 0 {
            out.push(ctx, "pixels", Some(id), format!("{id}: pixels differing: {}", s.differing));
            out.push(ctx, "max_difference", Some(id), format!("{id}: maximum pixel value difference: {}", s.max.text()));
        }
    }
    let (ovr_a, ovr_b) = (overview_count(a, level), overview_count(b, level));
    if !options.skip_overview {
        if ovr_a != ovr_b {
            out.push(
                ctx,
                "overview_count",
                Some(id),
                format!("Reference band {id} has {ovr_a} overview band(s), but input band has {ovr_b}"),
            );
        } else {
            for i in 0..ovr_a {
                let sub = match level {
                    Level::Image(_) => Level::Image(Some(i)),
                    Level::Mask(_) => Level::Mask(Some(i)),
                };
                compare_level(out, cache, a, b, band, &format!("overview of band {id}"), sub, options, ctx, extras)?;
            }
        }
    }
    let (fa, fb) = (level_mask_flags(a, band, level), level_mask_flags(b, band, level));
    if fa != fb {
        out.push(ctx, "mask_flags", Some(id), format!("Reference band {id} has mask flags = {fa} , but input band has mask flags = {fb}"));
    } else if fa == 2 {
        let mask = match level {
            Level::Image(ovr) => Level::Mask(ovr),
            Level::Mask(_) => unreachable!("a mask's flags are GMF_ALL_VALID"),
        };
        let none = (BandExtras::default(), BandExtras::default());
        compare_level(out, cache, a, b, band, &format!("mask of band {id}"), mask, options, ctx, &none)?;
    }
    if !options.skip_metadata {
        let items = |r: &LazyRaster, e: &BandExtras| match level {
            Level::Image(None) => merged(band_items(r, band, level), &e.items),
            Level::Image(Some(_)) => e.overview.clone(),
            Level::Mask(_) => Vec::new(),
        };
        let (ia, ib) = (items(a, &extras.0), items(b, &extras.1));
        compare_metadata(out, ctx, "(band default metadata domain)", &ia, &ib);
    }
    Ok(())
}

/// The overviews of a band (none for an overview); a mask has those of the image.
fn overview_count(r: &LazyRaster, level: Level) -> usize {
    match level {
        Level::Image(None) | Level::Mask(None) => r.overviews().len(),
        _ => 0,
    }
}

fn level_mask_flags(r: &mut LazyRaster, band: usize, level: Level) -> u32 {
    match level {
        Level::Image(_) => mask_flags(r, band),
        Level::Mask(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RasterDataset;
    use wbraster::{Raster, RasterConfig};

    #[cfg(remote)]
    #[test]
    fn truncated_binary_reads_propagate_errors_instead_of_differences() {
        use std::io::{BufRead, Write};
        for which in ["reference", "input"] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/truncated.bin", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let server = std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let mut count = 0;
                while count < 2 && start.elapsed().as_secs() < 10 {
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        }
                        Err(e) => panic!("{e}"),
                    };
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    loop {
                        line.clear();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                    }
                    let begin = count * 65536;
                    write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Length: 65536\r\nContent-Range: bytes {}-{}/131072\r\nConnection: close\r\n\r\n", begin, begin + 65535).unwrap();
                    stream.write_all(&vec![b'a'; if count == 0 { 65536 } else { 512 }]).unwrap();
                    count += 1;
                }
                count
            });
            let directory = tempfile::tempdir().unwrap();
            let local = directory.path().join("complete.bin");
            std::fs::write(&local, vec![b'a'; 131072]).unwrap();
            let local = local.to_str().unwrap();
            let result = if which == "reference" { binary_difference(&url, local) } else { binary_difference(local, &url) };
            let error = result.unwrap_err();
            assert!(matches!(error, Error::Io(_)), "{error:?}");
            assert!(error.to_string().contains(&format!("fully read {which}")), "{error}");
            assert_eq!(server.join().unwrap(), 2);
        }
    }

    fn raster(values: Vec<f64>, dt: DataType, nodata: Option<f64>) -> LazyRaster {
        let cfg = RasterConfig {
            cols: 3,
            rows: 2,
            bands: 1,
            x_min: 0.0,
            y_min: 0.0,
            cell_size: 1.0,
            nodata: nodata.unwrap_or(-9999.0),
            data_type: dt,
            ..Default::default()
        };
        let r = Raster::from_data(cfg, values).unwrap();
        LazyRaster::from(RasterDataset::new(r, nodata.is_some()))
    }

    #[test]
    fn mixed_vrt_bands_compare_in_their_native_types() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for (name, value) in [("a", 1234567.125), ("b", 1234567.25)] {
            let config = RasterConfig { cols: 1, rows: 1, bands: 1, data_type: DataType::F64, ..Default::default() };
            let source = RasterDataset::new(Raster::from_data(config, vec![value]).unwrap(), false);
            crate::write_raster_dataset(&source, dir.path().join(format!("{name}.tif")).to_str().unwrap(), &Default::default()).unwrap();
            let path = dir.path().join(format!("{name}.vrt"));
            std::fs::write(&path, format!(r#"<VRTDataset rasterXSize="1" rasterYSize="1"><VRTRasterBand dataType="Int32" band="1"><SimpleSource><SourceFilename relativeToVRT="1">{name}.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand><VRTRasterBand dataType="Float64" band="2"><SimpleSource><SourceFilename relativeToVRT="1">{name}.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"#)).unwrap();
            paths.push(path);
        }
        let result = compare(paths[0].to_str().unwrap(), paths[1].to_str().unwrap(), &CompareOptions::skip_all_optional()).unwrap();
        assert_eq!(result.bands[0].data_type, "Int32");
        assert_eq!(result.bands[1].data_type, "Float64");
        assert_eq!(result.bands[0].pixels_differing, 0);
        assert_eq!(result.bands[1].pixels_differing, 1);
    }

    #[test]
    fn unions_follow_gdal() {
        use DataType::*;
        assert_eq!(data_type_union(U8, I8), I16);
        assert_eq!(data_type_union(I8, U16), I32);
        assert_eq!(data_type_union(I16, U32), I64);
        assert_eq!(data_type_union(I8, U64), F64);
        assert_eq!(data_type_union(I32, F32), F64);
        assert_eq!(data_type_union(U16, F32), F32);
        assert_eq!(data_type_union(U8, U32), U32);
        assert_eq!(data_type_union(I16, I32), I32);
    }

    #[test]
    fn identical_rasters_have_no_difference() {
        let mut a = raster(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0], DataType::F32, None);
        let mut b = raster(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0], DataType::F32, None);
        let c = compare_rasters(&mut a, &mut b, &CompareOptions::default()).unwrap();
        assert_eq!(c.difference_count(), 0, "{:?}", c.differences);
        assert_eq!(c.bands.len(), 1);
        assert_eq!(c.bands[0].pixels, 6);
    }

    #[test]
    fn reports_pixels_types_and_nodata_as_gdal() {
        let mut a = raster(vec![1.0, 2.0, 3.0, 4.0, 5.0, f64::NAN], DataType::F32, None);
        let mut b = raster(vec![1.0, 2.5, 3.0, 4.0, 2.0, 6.0], DataType::F64, Some(0.0));
        let c = compare_rasters(&mut a, &mut b, &CompareOptions::default()).unwrap();
        let lines: Vec<&str> = c.differences.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(
            lines,
            [
                "Reference band 1 has data type Float32, but input band has data type Float64",
                "Reference band 1 has no nodata value, but input band has no data value 0.000000.",
                "1: pixels differing: 3",
                "1: maximum pixel value difference: 3.000000",
                "Reference band 1 has mask flags = 1 , but input band has mask flags = 8",
            ]
        );
        assert_eq!(c.bands[0].data_type, "Float64");
        assert_eq!(c.bands[0].pixels_differing, 3);
    }

    #[test]
    fn integer_differences_are_exact() {
        let mut a = raster(vec![0.0, 255.0, 3.0, 4.0, 5.0, 6.0], DataType::U8, None);
        let mut b = raster(vec![-128.0, 255.0, 3.0, 4.0, 5.0, 6.0], DataType::I8, None);
        let c = compare_rasters(&mut a, &mut b, &CompareOptions::skip_all_optional()).unwrap();
        assert_eq!(c.differences.last().unwrap().message, "1: maximum pixel value difference: 128");
        assert_eq!(c.bands[0].data_type, "Int16");
    }

    #[test]
    fn size_differences_stop_band_comparisons() {
        let mut a = raster(vec![0.0; 6], DataType::U8, None);
        let cfg = RasterConfig { cols: 2, rows: 3, bands: 1, cell_size: 1.0, data_type: DataType::U8, ..Default::default() };
        let mut b = LazyRaster::from(RasterDataset::new(Raster::from_data(cfg, vec![0.0; 6]).unwrap(), false));
        let c = compare_rasters(&mut a, &mut b, &CompareOptions::skip_all_optional()).unwrap();
        let lines: Vec<&str> = c.differences.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(
            lines,
            ["Reference dataset width is 3, but input dataset width is 2", "Reference dataset height is 2, but input dataset height is 3"]
        );
        assert!(c.bands.is_empty());
    }

    #[test]
    fn metadata_differences_name_both_values() {
        let mut out = Comparison::default();
        let a = vec![("A".to_string(), "1".to_string()), ("B".into(), "x".into()), ("backend".into(), "1".into())];
        let b = vec![("A".to_string(), "2".to_string()), ("C".into(), "y".into())];
        compare_metadata(&mut out, &Ctx::default(), "(dataset default metadata domain)", &a, &b);
        let lines: Vec<&str> = out.differences.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(
            lines,
            [
                "Reference metadata (dataset default metadata domain) has value '1' for key 'A' but input metadata has value '2'.",
                "Reference metadata (dataset default metadata domain) contains key 'B' but input metadata does not.",
                "Input metadata (dataset default metadata domain) contains key 'C' but reference metadata does not.",
            ]
        );
    }

    #[test]
    fn bytes_compare_sizes_then_contents() {
        assert_eq!(compare_bytes(b"abc", b"abc"), None);
        assert_eq!(compare_bytes(b"abc", b"abd").unwrap(), "Reference file and input file differ at the binary level.");
        assert_eq!(compare_bytes(b"abc", b"ab").unwrap(), "Reference file has size 3 bytes, whereas input file has size 2 bytes.");
    }

    #[test]
    fn rpc_items_follow_gdal_names() {
        let mut rpc = vec![0.0; 92];
        rpc[2] = 50.5;
        rpc[12] = 1.0;
        let items = rpc_items(&rpc);
        assert_eq!(items[2], ("LINE_OFF".to_string(), "50.5".to_string()));
        assert!(items[12].1.starts_with("1 0 0"));
        assert_eq!(items.len(), 16);
    }
}
