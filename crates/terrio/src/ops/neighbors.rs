//! Focal statistics and convolution kernels: an equivalent of `gdal raster neighbors` (GDAL 3.12).
//!
//! Each output band is one kernel applied to one input band (every band by default, or the one
//! selected), in GDAL's order: for each input band, each kernel. Values are combined as GDAL's
//! `VRTKernelFilteredSource` combines them (the VRT kernel sources of [`super::vrt`] share this
//! implementation): source values are read as `Float32`, each neighbour is weighted by its kernel
//! coefficient, nodata and NaN neighbours (with a nodata value) and zero coefficients are left
//! out, and pixels equal to the nodata value stay nodata. Past the edges of the raster, the edge
//! pixels are repeated. Results are `Float32` values, converted to the output type as GDAL's
//! `GDALCopyWords` converts them.
//!
//! Built-in kernels are GDAL's: `u` and `v` (horizontal and vertical derivatives, halved),
//! `edge1`, `edge2`, `sharpen`, `gaussian` (3x3 or 5x5, normalized), `unsharp-masking` (5x5) and
//! `equal` (any odd size); explicit matrices are written `[[a,b,c],[d,e,f],[g,h,i]]`, row by row.

use super::Grid;
use crate::{Error, LazyRaster, RasterDataset, Result, Window, WindowOptions};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// How the weighted neighbours of a pixel are combined (`--method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeighborMethod {
    /// The sum of the weighted values (the default for kernels whose coefficients sum to 0).
    Sum,
    /// The weighted mean: the sum divided by the sum of the coefficients of the valid
    /// neighbours (the default for other kernels).
    Mean,
    /// The smallest weighted value.
    Min,
    /// The largest weighted value.
    Max,
    /// The population standard deviation of the weighted values.
    StdDev,
    /// The median of the weighted values (the mean of the two middle ones for an even count).
    Median,
    /// The most frequent weighted value (the first to reach the highest count on ties).
    Mode,
}

impl NeighborMethod {
    /// Parses a method name as GDAL names it: `sum`, `mean`, `min`, `max`, `stddev`, `median`
    /// or `mode`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "sum" => Self::Sum,
            "mean" => Self::Mean,
            "min" => Self::Min,
            "max" => Self::Max,
            "stddev" => Self::StdDev,
            "median" => Self::Median,
            "mode" => Self::Mode,
            _ => return Err(Error::InvalidArgument(format!("unknown method '{s}': sum, mean, min, max, stddev, median or mode"))),
        })
    }

    /// The function of a VRT kernel source (`<Function>`): `None` for sums and means.
    pub(crate) fn function(self) -> Option<Self> {
        (!matches!(self, Self::Sum | Self::Mean)).then_some(self)
    }
}

/// The output nodata tag (`--nodata`). Finite source nodata samples keep the
/// source value; changing or removing this tag does not rewrite those samples.
/// NaN nodata centres follow GDAL's neighborhood interpolation behavior.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum NeighborsNodata {
    /// The input band's nodata value, when it has one (the default).
    #[default]
    Source,
    /// No nodata value (`--nodata none`).
    None,
    /// This value.
    Value(f64),
}

impl NeighborsNodata {
    /// Parse the CLI/Python/JavaScript string form: a number, nan, inf, -inf or none.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "nan" => Self::Value(f64::NAN),
            "inf" => Self::Value(f64::INFINITY),
            "-inf" => Self::Value(f64::NEG_INFINITY),
            value => Self::Value(value.parse().map_err(|_| Error::InvalidArgument(format!("invalid nodata value '{s}'")))?),
        })
    }
}

/// Options for [`neighbors`], named after `gdal raster neighbors`'.
#[derive(Debug, Clone)]
pub struct NeighborsOptions {
    /// The band to filter, from 1; `None` filters every band (`-b`).
    pub band: Option<usize>,
    /// The kernels (`--kernel`, at least one): `u`, `v`, `edge1`, `edge2`, `sharpen`,
    /// `gaussian`, `unsharp-masking`, `equal`, or a matrix such as `[[1,0,1],[0,1,0],[1,0,1]]`.
    pub kernels: Vec<String>,
    /// The methods (`--method`): none for each kernel's default, one for every kernel, or one per
    /// kernel.
    pub methods: Vec<NeighborMethod>,
    /// The kernel size (`--size`, odd, 3 to 99); `None` for each kernel's default (3, or 5 for
    /// `unsharp-masking`, or the matrix's).
    pub size: Option<usize>,
    /// The output data type (`--ot`, `Float64` by default).
    pub output_type: DataType,
    /// The output nodata value.
    pub nodata: NeighborsNodata,
}

impl Default for NeighborsOptions {
    fn default() -> Self {
        Self {
            band: None,
            kernels: vec!["equal".into()],
            methods: Vec::new(),
            size: None,
            output_type: DataType::F64,
            nodata: NeighborsNodata::Source,
        }
    }
}

/// A square kernel and how its weighted values are combined.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Kernel {
    /// The width and height (odd).
    pub size: usize,
    /// `size * size` coefficients row by row, or `size` for a separable kernel (applied
    /// vertically, then horizontally).
    pub coefs: Vec<f64>,
    /// Whether sums are divided by the sum of the coefficients of the valid neighbours.
    pub normalized: bool,
    /// The function combining the weighted values (min, max, stddev, median, mode) instead of
    /// summing them.
    pub function: Option<NeighborMethod>,
}

/// GDAL's built-in kernels (`gdalalg_raster_neighbors.cpp`): size and integer coefficients.
fn builtin(name: &str) -> Option<(usize, &'static [i32])> {
    #[rustfmt::skip]
    const TABLE: &[(&str, usize, &[i32])] = &[
        ("u", 3, &[0, 0, 0, -1, 0, 1, 0, 0, 0]),
        ("v", 3, &[0, -1, 0, 0, 0, 0, 0, 1, 0]),
        ("edge1", 3, &[0, -1, 0, -1, 4, -1, 0, -1, 0]),
        ("edge2", 3, &[-1, -1, -1, -1, 8, -1, -1, -1, -1]),
        ("sharpen", 3, &[0, -1, 0, -1, 5, -1, 0, -1, 0]),
        ("gaussian-3x3", 3, &[1, 2, 1, 2, 4, 2, 1, 2, 1]),
        ("gaussian-5x5", 5, &[
            1, 4, 6, 4, 1,
            4, 16, 24, 16, 4,
            6, 24, 36, 24, 6,
            4, 16, 24, 16, 4,
            1, 4, 6, 4, 1,
        ]),
        ("unsharp-masking-5x5", 5, &[
            1, 4, 6, 4, 1,
            4, 16, 24, 16, 4,
            6, 24, -476, 24, 6,
            4, 16, 24, 16, 4,
            1, 4, 6, 4, 1,
        ]),
    ];
    TABLE.iter().find(|(n, _, _)| *n == name).map(|(_, s, c)| (*s, *c))
}

/// The coefficients of a built-in kernel: divided by their sum (`normalize`), or multiplied by
/// `weight` (GDAL's `GetKernelDef`).
fn builtin_coefs(name: &str, normalize: bool, weight: f64) -> (usize, Vec<f64>) {
    let (size, values) = builtin(name).expect("a built-in kernel");
    let sum: i32 = values.iter().sum();
    let w = if normalize { 1.0 / (sum as f64 + f64::MIN_POSITIVE) } else { weight };
    (size, values.iter().map(|v| *v as f64 * w).collect())
}

const NAMES: [&str; 8] = ["u", "v", "equal", "edge1", "edge2", "sharpen", "gaussian", "unsharp-masking"];

/// The values of a matrix kernel (`[[a,b],[c,d]]`), split on brackets, commas and spaces as GDAL
/// splits them.
fn matrix_values(kernel: &str) -> Result<Vec<f64>> {
    let values = kernel
        .split(['[', ']', ',', ' '])
        .filter(|t| !t.is_empty())
        .map(|t| t.parse::<f64>().map_err(|_| Error::InvalidArgument("non-numeric value found in the kernel".into())))
        .collect::<Result<Vec<_>>>()?;
    let side = ((values.len() as f64).sqrt() + 0.5) as usize;
    if values.len() % 2 != 1 || side * side != values.len() {
        return Err(Error::InvalidArgument("the number of values of a kernel matrix must be an odd square number".into()));
    }
    if values.iter().any(|v| !v.is_finite()) {
        return Err(Error::InvalidArgument("kernel coefficients must be finite".into()));
    }
    Ok(values)
}

/// The kernels of `o`, as GDAL builds them (`GDALRasterNeighborsAlgorithm::RunStep`).
pub(crate) fn kernels(o: &NeighborsOptions) -> Result<Vec<Kernel>> {
    if o.kernels.is_empty() {
        return Err(Error::InvalidArgument("at least one kernel is needed".into()));
    }
    if o.methods.len() > 1 && o.methods.len() != o.kernels.len() {
        return Err(Error::InvalidArgument("give one method, or exactly one per kernel".into()));
    }
    if let Some(size) = o.size
        && (!(3..=99).contains(&size) || size % 2 != 1)
    {
        return Err(Error::InvalidArgument("the kernel size must be an odd number from 3 to 99".into()));
    }
    let mut matrices = Vec::with_capacity(o.kernels.len());
    for k in &o.kernels {
        let k = k.trim();
        if k.starts_with('[') && k.ends_with(']') {
            matrices.push(Some(matrix_values(k)?));
        } else if NAMES.contains(&k) {
            matrices.push(None);
        } else {
            return Err(Error::InvalidArgument(format!(
                "unknown kernel '{k}': {} or a matrix [[v00,v10,...],...,[v0N,...,vNN]]",
                NAMES.join(", ")
            )));
        }
    }
    if let Some(size) = o.size {
        for (k, m) in o.kernels.iter().zip(&matrices) {
            let k = k.trim();
            let ok = match (k, m) {
                (_, Some(m)) => ((m.len() as f64).sqrt() + 0.5) as usize == size,
                ("gaussian", _) => size == 3 || size == 5,
                ("unsharp-masking", _) => size == 5,
                ("equal", _) => true,
                _ => size == 3,
            };
            if !ok {
                return Err(Error::InvalidArgument(format!("size {size} is not supported for kernel '{k}'")));
            }
        }
    }
    let zero_sum: Vec<bool> = matrices.iter().map(|m| m.as_ref().is_some_and(|m| m.iter().sum::<f64>().abs() < 1e-10)).collect();
    let methods: Vec<NeighborMethod> = match o.methods.as_slice() {
        [] => o
            .kernels
            .iter()
            .zip(&zero_sum)
            .map(|(k, z)| if *z || matches!(k.trim(), "u" | "v" | "edge1" | "edge2") { NeighborMethod::Sum } else { NeighborMethod::Mean })
            .collect(),
        [m] => vec![*m; o.kernels.len()],
        m => m.to_vec(),
    };
    for (zero, method) in zero_sum.iter().zip(&methods) {
        if *zero && *method == NeighborMethod::Mean {
            return Err(Error::InvalidArgument(
                "method 'mean' is not allowed for a kernel whose coefficients sum to zero: use 'sum'".into(),
            ));
        }
    }
    o.kernels
        .iter()
        .zip(matrices)
        .zip(methods)
        .map(|((k, m), method)| {
            let k = k.trim();
            let (size, coefs) = match (k, m) {
                (_, Some(m)) => (((m.len() as f64).sqrt() + 0.5) as usize, m),
                ("edge1" | "edge2" | "sharpen", _) => builtin_coefs(k, false, 1.0),
                ("u" | "v", _) => builtin_coefs(k, false, 0.5),
                ("equal", _) => {
                    let size = o.size.unwrap_or(3);
                    let n = size * size;
                    let w = if method == NeighborMethod::Mean { 1.0 / (n as f64 + f64::MIN_POSITIVE) } else { 1.0 };
                    (size, vec![w; n])
                }
                ("gaussian", _) => builtin_coefs(if o.size == Some(5) { "gaussian-5x5" } else { "gaussian-3x3" }, true, 0.0),
                _ => builtin_coefs("unsharp-masking-5x5", true, 0.0),
            };
            Ok(Kernel { size, coefs, normalized: method != NeighborMethod::Sum, function: method.function() })
        })
        .collect()
}

/// Applies a kernel to a `width` x `height` grid of `Float32` values, as GDAL's
/// `VRTKernelFilteredSource::FilterData` does, past its edges repeating the edge values.
/// `nodata` is the source nodata value (as `Float32`).
// Arithmetic and axis ordering follow GDAL's VRTKernelFilteredSource::FilterData
// (frmts/vrt/vrtfilters.cpp, MIT; attribution in NOTICE).
pub(crate) fn filter(input: &[f32], width: usize, height: usize, k: &Kernel, nodata: Option<f32>) -> Vec<f32> {
    // A 1x1 kernel is a full kernel applied once (GDAL 3.13.3), not two separable passes.
    let separable = k.coefs.len() == k.size && k.size != 1;
    let radius = (k.size / 2) as isize;
    let mut input = std::borrow::Cow::Borrowed(input);
    for axis in 0..if separable { 2 } else { 1 } {
        let src = &input;
        let mut output = vec![0f32; width * height];
        crate::par::chunks_mut(&mut output, width.max(1), |y, row| {
            let mut values = Vec::new();
            let mut counts: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
            for (x, out) in row.iter_mut().enumerate() {
                let center = src[y * width + x];
                if nodata == Some(center) {
                    *out = center;
                    continue;
                }
                let (rows, cols) = match (separable, axis) {
                    (false, _) => (-radius..=radius, -radius..=radius),
                    (true, 0) => (-radius..=radius, 0..=0),
                    _ => (0..=0, -radius..=radius),
                };
                let (mut sum, mut weight, mut valid) = (0.0f64, 0.0f64, 0usize);
                let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
                let (mut mean, mut m2) = (0.0f64, 0.0f64);
                let (mut mode, mut best) = (f64::NAN, 0usize);
                values.clear();
                if !counts.is_empty() {
                    counts.clear();
                }
                let mut i = 0;
                for dy in rows {
                    for dx in cols.clone() {
                        let c = if separable { k.coefs[(dy + dx + radius) as usize] } else { k.coefs[i] };
                        i += 1;
                        let yy = (y as isize + dy).clamp(0, height as isize - 1) as usize;
                        let xx = (x as isize + dx).clamp(0, width as isize - 1) as usize;
                        let v = src[yy * width + xx];
                        if nodata.is_some() && (nodata == Some(v) || v.is_nan()) {
                            continue;
                        }
                        if c == 0.0 {
                            continue;
                        }
                        let v = f64::from(v) * c;
                        valid += 1;
                        match k.function {
                            Some(NeighborMethod::Max) => max = max.max(v),
                            Some(NeighborMethod::Min) => min = min.min(v),
                            Some(NeighborMethod::StdDev) => {
                                let delta = v - mean;
                                mean += delta / valid as f64;
                                m2 += delta * (v - mean);
                            }
                            Some(NeighborMethod::Median) => values.push(v),
                            Some(NeighborMethod::Mode) => {
                                // GDAL counts values in a std::map: equal values (0 and -0
                                // too, merged by adding 0) share a count, and the first to
                                // exceed the best wins.
                                let n = if v.is_nan() {
                                    // Preserve the VRT counter: NaNs compare equal to no sample.
                                    0
                                } else {
                                    let count = counts.entry((v + 0.0).to_bits()).or_insert(0);
                                    *count += 1;
                                    *count
                                };
                                if n > best {
                                    best = n;
                                    mode = v;
                                }
                            }
                            _ => {
                                sum += v;
                                weight += c;
                            }
                        }
                    }
                }
                let empty = nodata.unwrap_or(0.0);
                *out = if valid == 0 && k.function.is_some() {
                    empty
                } else {
                    match k.function {
                        Some(NeighborMethod::Max) => max as f32,
                        Some(NeighborMethod::Min) => min as f32,
                        Some(NeighborMethod::Mode) => mode as f32,
                        Some(NeighborMethod::StdDev) => (m2 / valid as f64).sqrt() as f32,
                        Some(_) => {
                            values.sort_by(f64::total_cmp);
                            let n = values.len();
                            (if n % 2 == 1 { values[n / 2] } else { (values[n / 2 - 1] + values[n / 2]) / 2.0 }) as f32
                        }
                        None if !k.normalized => sum as f32,
                        None if valid == 0 || weight == 0.0 => empty,
                        None => (sum / weight) as f32,
                    }
                };
            }
        });
        input = std::borrow::Cow::Owned(output);
    }
    input.into_owned()
}

/// The output bands of `o` over a raster of `bands` bands: `(input band from 1, kernel index)`.
fn plan(o: &NeighborsOptions, bands: usize) -> Result<Vec<(usize, usize)>> {
    let inputs: Vec<usize> = match o.band {
        Some(b) if b == 0 || b > bands => return Err(Error::InvalidArgument(format!("no band {b}: the raster has {bands} band(s)"))),
        Some(b) => vec![b],
        None => (1..=bands).collect(),
    };
    Ok(inputs.iter().flat_map(|b| (0..o.kernels.len()).map(move |k| (*b, k))).collect())
}

/// Whether `v` is exactly representable in `dt` (GDAL's `GDALIsValueExactAs`).
fn exact_in(v: f64, dt: DataType) -> bool {
    match dt {
        DataType::F64 => true,
        DataType::F32 => v.is_nan() || v.is_infinite() || (v as f32) as f64 == v,
        _ => {
            let (lo, hi) = super::type_range(dt);
            v.is_finite() && v.fract() == 0.0 && v >= lo && v <= hi
        }
    }
}

/// What [`neighbors`] computes: the kernels, the output bands and their nodata values.
struct Plan {
    kernels: Vec<Kernel>,
    bands: Vec<(usize, usize)>,
    /// The nodata value of each output band.
    nodata: Vec<Option<f64>>,
    /// The nodata value of each input band (from 1), as `Float32`.
    source_nodata: Vec<Option<f32>>,
    radius: usize,
}

impl Plan {
    fn new(header: &RasterDataset, o: &NeighborsOptions) -> Result<Self> {
        if header.complex {
            return Err(Error::Unsupported("focal statistics of complex bands are not supported".into()));
        }
        let kernels = kernels(o)?;
        let bands = plan(o, header.band_count())?;
        let source_nodata: Vec<Option<f32>> = (1..=header.band_count()).map(|b| header.band_nodata(b).map(|v| v as f32)).collect();
        let nodata = bands
            .iter()
            .map(|(b, _)| {
                let v = match o.nodata {
                    NeighborsNodata::Value(v) => Some(v),
                    NeighborsNodata::None => None,
                    NeighborsNodata::Source => header.band_nodata(*b),
                };
                match v {
                    Some(v) if !exact_in(v, o.output_type) => Err(Error::InvalidArgument(format!(
                        "the output type {} cannot represent the nodata value {v}",
                        super::data_type_name(o.output_type)
                    ))),
                    v => Ok(v),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let radius = kernels.iter().map(|k| k.size / 2).max().unwrap_or(0);
        Ok(Self { kernels, bands, nodata, source_nodata, radius })
    }

    /// The output's metadata, without pixels.
    fn header(&self, header: &RasterDataset, o: &NeighborsOptions) -> Result<RasterDataset> {
        let first = self.nodata.first().copied().flatten();
        let mut out = super::raster_header(Grid::of(&header.raster), o.output_type, self.bands.len(), first, &header.crs())?;
        out.band_nodata = self.nodata.clone();
        if let Some(gt) = header.affine {
            out.set_geo_transform(gt);
        }
        Ok(out)
    }

    /// The output rows `out` (absolute) from `strip`, which holds rows `s0..` of the input bands
    /// `inputs` (from 1, in that order), in the output type, band after band.
    /// `index` gives the raster band of `strip` (from 0) holding an input band (from 1).
    fn compute(
        &self,
        strip: &RasterDataset,
        index: impl Fn(usize) -> usize,
        s0: usize,
        out: std::ops::Range<usize>,
        dt: DataType,
    ) -> RasterData {
        use crate::pixel::Pixel;
        let (w, h) = (strip.raster.cols, strip.raster.rows);
        let rows = out.end - out.start;
        let mut data = RasterData::new_filled(dt, w * rows * self.bands.len(), 0.0);
        let mut values: Option<(usize, Vec<f32>)> = None;
        for (i, (band, kernel)) in self.bands.iter().enumerate() {
            if values.as_ref().is_none_or(|(b, _)| b != band) {
                let v = super::band_f64(&strip.raster, index(*band)).iter().map(|v| *v as f32).collect();
                values = Some((*band, v));
            }
            let src = &values.as_ref().expect("band read").1;
            let result = filter(src, w, h, &self.kernels[*kernel], self.source_nodata[band - 1]);
            let part = &result[(out.start - s0) * w..(out.end - s0) * w];
            let base = i * w * rows;
            crate::pixel::with_data!(&mut data, d => {
                for (o, v) in d[base..base + w * rows].iter_mut().zip(part) {
                    *o = Pixel::from_f64(super::convert_value(f64::from(*v), dt));
                }
            });
        }
        data
    }
}

/// The input bands the output bands are computed from, from 1, in order.
fn input_bands(bands: &[(usize, usize)]) -> Vec<usize> {
    let mut v: Vec<usize> = bands.iter().map(|(b, _)| *b).collect();
    v.dedup();
    v
}

/// Computes focal statistics of a raster held in memory, as `gdal raster neighbors` does.
///
/// ```no_run
/// use terrio::ops::neighbors::{NeighborMethod, NeighborsOptions, neighbors};
///
/// let dem = terrio::open_raster("dem.tif")?;
/// let o = NeighborsOptions { kernels: vec!["equal".into()], size: Some(5), methods: vec![NeighborMethod::Max], ..Default::default() };
/// let max5 = neighbors(&dem, &o)?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn neighbors(ds: &RasterDataset, o: &NeighborsOptions) -> Result<RasterDataset> {
    let p = Plan::new(ds, o)?;
    let mut out = p.header(ds, o)?;
    let h = ds.raster.rows;
    out.raster.data = p.compute(ds, |b| b - 1, 0, 0..h, o.output_type);
    Ok(out)
}

/// Computes focal statistics and writes them to `path`, reading the input and writing a GeoTIFF
/// or COG a strip of rows at a time (with the kernel's margin rows): neither is held whole
/// (other formats are written whole). The values are those of [`neighbors`].
///
/// `path` must not be the file `src` reads.
pub fn neighbors_to_path(src: &mut LazyRaster, o: &NeighborsOptions, path: &str, options: &crate::RasterWriteOptions) -> Result<()> {
    let header = src.header();
    let p = Plan::new(&header, o)?;
    let inputs = input_bands(&p.bands).len();
    let rows = super::strips::strip_rows(src.width(), p.bands.len() + inputs, 8, src.block_size().map(|b| b.1));
    in_strips(src, o, p, path, options, rows)
}

/// [`neighbors_to_path`] in strips of `rows` rows.
fn in_strips(
    src: &mut LazyRaster,
    o: &NeighborsOptions,
    p: Plan,
    path: &str,
    options: &crate::RasterWriteOptions,
    rows: usize,
) -> Result<()> {
    let header = src.header();
    let (w, h) = (src.width(), src.height());
    let inputs = input_bands(&p.bands);
    let mut out = super::strips::StripWriter::create(&p.header(&header, o)?, path, options)?;
    let read = WindowOptions { bands: inputs.clone(), overview: None };
    // Rows `y0..y1` of strip `i`, read with the kernel's margin on each side.
    let r = p.radius;
    let span = |i: usize| {
        let (y0, y1) = (i * rows, ((i + 1) * rows).min(h));
        (y0, y1, y0.saturating_sub(r), (y1 + r).min(h))
    };
    super::strips::pipelined(
        h.div_ceil(rows),
        |i| {
            let (_, _, s0, s1) = span(i);
            src.read_window(Window::new(0, s0, w, s1 - s0), &read)
        },
        |i, strip| {
            let (y0, y1, s0, _) = span(i);
            let index = |b: usize| inputs.iter().position(|i| *i == b).unwrap_or(0);
            out.write(p.compute(&strip, index, s0, y0..y1, o.output_type), y1 - y0)
        },
    )?;
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel(coefs: Vec<f64>, normalized: bool, function: Option<NeighborMethod>) -> Kernel {
        let size = (coefs.len() as f64).sqrt() as usize;
        Kernel { size, coefs, normalized, function }
    }

    #[test]
    fn vrt_size_one_kernel_is_applied_once() {
        // GDAL 3.13.3 KernelFilteredSource: Size=1, Coefs=2, normalized=0.
        assert_eq!(filter(&[3.], 1, 1, &kernel(vec![2.], false, None), None), vec![6.]);
    }

    #[test]
    fn builtin_kernels_follow_gdal() {
        let o = NeighborsOptions { kernels: vec!["u".into(), "gaussian".into(), "equal".into()], ..Default::default() };
        let k = kernels(&o).unwrap();
        assert_eq!(k[0].coefs, vec![0., 0., 0., -0.5, 0., 0.5, 0., 0., 0.]);
        assert!(!k[0].normalized);
        assert!((k[1].coefs[4] - 4.0 / 16.0).abs() < 1e-15 && k[1].normalized);
        assert_eq!(k[2].coefs.len(), 9);
        let o = NeighborsOptions { kernels: vec!["unsharp-masking".into()], ..Default::default() };
        assert_eq!(kernels(&o).unwrap()[0].size, 5);
        let bad = NeighborsOptions { kernels: vec!["sharpen".into()], size: Some(5), ..Default::default() };
        assert!(kernels(&bad).is_err());
        let even = NeighborsOptions { kernels: vec!["[[1,1],[1,1]]".into()], ..Default::default() };
        assert!(kernels(&even).is_err());
        let zero = NeighborsOptions {
            kernels: vec!["[[0,1,0],[1,-4,1],[0,1,0]]".into()],
            methods: vec![NeighborMethod::Mean],
            ..Default::default()
        };
        assert!(kernels(&zero).is_err());
        let zero = NeighborsOptions { kernels: vec!["[[0,1,0],[1,-4,1],[0,1,0]]".into()], ..Default::default() };
        assert!(!kernels(&zero).unwrap()[0].normalized);
    }

    #[test]
    fn filter_repeats_edges_and_skips_nodata() {
        // 3x3 mean with a nodata pixel: the mean of the 8 others; the nodata pixel stays.
        let v = [1., 2., 3., 4., -1., 6., 7., 8., 9.];
        let k = kernel(vec![1.0; 9], true, None);
        let out = filter(&v, 3, 3, &k, Some(-1.0));
        assert_eq!(out[4], -1.0);
        // Corner (0,0): neighbours with repeated edges: 1,1,2 / 1,1,2 / 4,4,(nodata).
        assert_eq!(out[0], (1. + 1. + 2. + 1. + 1. + 2. + 4. + 4.) / 8.);
        // Max and min of the weighted values.
        let out = filter(&v, 3, 3, &kernel(vec![1.0; 9], true, Some(NeighborMethod::Max)), Some(-1.0));
        assert_eq!(out[1], 6.0);
    }

    #[test]
    fn strips_match_the_whole_raster() {
        let (w, h) = (23, 41);
        let values: Vec<f32> = (0..w * h).map(|i| ((i * 7919) % 211) as f32 - 3.0).collect();
        let grid = Grid { width: w, height: h, geo_transform: [0., 1., 0., h as f64, 0., -1.] };
        let ds = super::super::build_raster_data(grid, RasterData::F32(values), 1, Some(-3.0), &Default::default()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        for o in [
            NeighborsOptions { kernels: vec!["equal".into()], size: Some(9), methods: vec![NeighborMethod::Median], ..Default::default() },
            NeighborsOptions { kernels: vec!["gaussian".into(), "u".into()], size: Some(3), ..Default::default() },
        ] {
            let whole = neighbors(&ds, &o).unwrap();
            let path = dir.path().join("strips.tif");
            let path = path.to_str().unwrap();
            in_strips(&mut LazyRaster::from(ds.clone()), &o, Plan::new(&ds, &o).unwrap(), path, &Default::default(), 5).unwrap();
            let read = crate::open_raster(path).unwrap();
            assert_eq!(read.raster.data.to_f64_vec(), whole.raster.data.to_f64_vec());
            assert_eq!(read.band_nodata(1), Some(-3.0));
        }
    }

    #[test]
    fn mode_ties_go_to_the_first_value_to_lead() {
        // Row-major neighbours of the centre: 1 2 2 / 1 5 1 / 2 9 9 ... 1 and 2 reach 3 each;
        // 2 reaches 2 first, 1 reaches 3 first.
        let v = [1., 2., 2., 1., 5., 1., 2., 9., 9.];
        let out = filter(&v, 3, 3, &kernel(vec![1.0; 9], true, Some(NeighborMethod::Mode)), None);
        assert_eq!(out[4], 1.0);
        let v = [1., 2., 2., 1., 5., 3., 3., 9., 9.];
        let out = filter(&v, 3, 3, &kernel(vec![1.0; 9], true, Some(NeighborMethod::Mode)), None);
        // 2 reaches a count of 2 before 1 does.
        assert_eq!(out[4], 2.0);
    }
    #[test]
    fn mode_merges_signed_zero_and_keeps_nan_unequal() {
        let k = kernel(vec![1.0; 9], true, Some(NeighborMethod::Mode));
        let out = filter(&[1., 1., -0., 0., -0., 2., 3., 4., 5.], 3, 3, &k, None);
        assert_eq!(out[4], 0.0);
        let out = filter(&[1., 1., f32::NAN, f32::NAN, f32::NAN, 2., 3., 4., 5.], 3, 3, &k, None);
        assert_eq!(out[4], 1.0);
    }
    #[test]
    fn nan_nodata_center_is_interpolated_like_gdal_313() {
        let values = [1., 1., 1., 1., f32::NAN, 1., 1., 1., 1.];
        let out = filter(&values, 3, 3, &kernel(vec![1.; 9], true, None), Some(f32::NAN));
        assert_eq!(out[4], 1.);
    }

    #[test]
    fn parses_shared_nodata_forms() {
        assert_eq!(NeighborsNodata::parse(" NONE ").unwrap(), NeighborsNodata::None);
        assert_eq!(NeighborsNodata::parse(" -9999 ").unwrap(), NeighborsNodata::Value(-9999.));
        assert!(matches!(NeighborsNodata::parse("NaN").unwrap(), NeighborsNodata::Value(v) if v.is_nan()));
        assert_eq!(NeighborsNodata::parse("-INF").unwrap(), NeighborsNodata::Value(f64::NEG_INFINITY));
        assert!(NeighborsNodata::parse("missing").is_err());
    }

    #[test]
    fn all_nan_mode_without_nodata_matches_gdal_313() {
        let out = filter(&[f32::NAN; 9], 3, 3, &kernel(vec![1.; 9], true, Some(NeighborMethod::Mode)), None);
        assert!(out.iter().all(|v| v.is_nan()));
    }

    #[test]
    fn vrt_mode_ignores_leading_nan_without_nodata() {
        let out = filter(&[f32::NAN, 2., 3., 4., 5., 6., 7., 8., 9.], 3, 3, &kernel(vec![1.; 9], true, Some(NeighborMethod::Mode)), None);
        assert_eq!(out[4], 2.);
    }

    #[test]
    fn named_zero_sum_mean_matches_gdal_acceptance() {
        for name in ["u", "v", "edge1", "edge2"] {
            let o = NeighborsOptions { kernels: vec![name.into()], methods: vec![NeighborMethod::Mean], ..Default::default() };
            assert!(kernels(&o).is_ok());
        }
    }

    #[test]
    fn shared_mean_method_rejects_zero_sum_matrix() {
        let mut options = NeighborsOptions {
            kernels: vec!["equal".into(), "[[0,1,0],[1,-4,1],[0,1,0]]".into()],
            methods: vec![NeighborMethod::Mean],
            ..Default::default()
        };
        assert!(kernels(&options).is_err());
        options.methods = vec![NeighborMethod::Mean, NeighborMethod::Sum];
        assert!(kernels(&options).is_ok());
    }
}
