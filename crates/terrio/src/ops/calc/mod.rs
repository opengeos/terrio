//! Raster algebra: an equivalent of `gdal_calc.py`.
//!
//! Expressions are evaluated band by band over named inputs with NumPy's typing rules (see
//! [`eval`]), as `gdal_calc.py` evaluates them with NumPy, so that results match it pixel for
//! pixel:
//!
//! - an output pixel is nodata when any input pixel is its band's nodata value (unless
//!   `hide_nodata`), and the result is blended with the output nodata value as `gdal_calc.py`
//!   blends it;
//! - the output type defaults to the smallest GDAL type that holds all the inputs' types, and
//!   the output nodata value to `gdal_calc.py`'s default for that type;
//! - results are converted to the output type with GDAL's rounding and clamping;
//! - inputs with different extents (but the same resolution) are aligned on the union or the
//!   intersection of their extents, or on a window, through [`mosaic`](super::mosaic).
//!
//! The expression language is the subset of Python and NumPy that raster algebra uses:
//! arithmetic (`+ - * / // % **`), comparisons (chainable), `&`, `|`, `^`, `~`, `and`, `or`,
//! `not`, and functions: `where`, `clip`, `minimum`/`maximum` (also `min`/`max` of two or more
//! values), `abs`, `sqrt`, `exp`, `log`, `log10`, `log2`, the trigonometric and hyperbolic
//! functions (`sin`, `arcsin` or `asin`, `arctan2` or `atan2`...), `floor`, `ceil`, `trunc`,
//! `round`, `isnan`, `isinf`, `isfinite`, `logical_and`/`or`/`xor`/`not`, `fmod`, `hypot`, type
//! conversions (`A.astype(float32)`, `float32(A)`) and the constants `pi`, `e`, `nan`, `inf`.
//! Functions may be prefixed with `numpy.` or `np.`. Nothing else is evaluated.

pub mod eval;
pub mod expr;

pub use eval::DType;
pub use expr::Expr;

use super::mosaic::{MosaicOptions, MosaicSource, mosaic};
use super::{Grid, build_raster};
use crate::{Error, RasterDataset, Result};
use eval::{Arr, Data, Evaluator, Val};
use std::borrow::Cow;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// An input of [`calc`]: a band of a raster, known by a name in the expressions.
#[derive(Debug, Clone)]
pub struct CalcInput<'a> {
    /// The name (`A`, `B`... or any identifier).
    pub name: String,
    /// The raster.
    pub ds: &'a RasterDataset,
    /// The band, from 1.
    pub band: usize,
}

/// How inputs with different extents are handled (`--extent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CalcExtent {
    /// Inputs must have the same size; the output takes the georeferencing of the last input.
    #[default]
    Ignore,
    /// Inputs must have the same size and geotransform.
    Fail,
    /// The union of the inputs' extents, filled with nodata (or 0) outside each input.
    Union,
    /// The intersection of the inputs' extents.
    Intersect,
}

impl CalcExtent {
    /// Parses `ignore`, `fail`, `union` or `intersect`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "ignore" => Self::Ignore,
            "fail" => Self::Fail,
            "union" => Self::Union,
            "intersect" => Self::Intersect,
            other => return Err(Error::InvalidArgument(format!("unknown extent '{other}' (ignore, fail, union, intersect)"))),
        })
    }
}

/// The output nodata value (`--NoDataValue`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum CalcNodata {
    /// `gdal_calc.py`'s default for the output type (255 for Byte, -32768 for Int16, the
    /// largest value for floating-point types...), or none with `hide_nodata`.
    #[default]
    Default,
    /// No nodata value.
    None,
    /// This value.
    Value(f64),
}

/// Options for [`calc`], named after `gdal_calc.py`'s.
#[derive(Debug, Clone, Default)]
pub struct CalcOptions {
    /// The expressions (`--calc`), one per output band (or one for all bands with `all_bands`).
    pub calc: Vec<String>,
    /// The output type (`--type`). By default the smallest type holding all the inputs' types.
    pub output_type: Option<DataType>,
    /// The output nodata value (`--NoDataValue`).
    pub nodata: CalcNodata,
    /// Ignore the inputs' nodata values (`--hideNoData`).
    pub hide_nodata: bool,
    /// How inputs with different extents are handled (`--extent`).
    pub extent: CalcExtent,
    /// Compute over this window `(ulx, uly, lrx, lry)`, aligned on the first input's pixels
    /// (`--projwin`).
    pub projwin: Option<(f64, f64, f64, f64)>,
    /// Process every band of this input, giving one output band each (`--allBands`).
    pub all_bands: Option<String>,
    /// Fail when the inputs' CRSs differ (`--projectionCheck`).
    pub projection_check: bool,
}

/// GDAL's `GDALDataTypeUnion()`: the smallest type holding both types.
pub fn data_type_union(a: DataType, b: DataType) -> DataType {
    let info = |d: DataType| -> (u32, bool, bool) {
        match d {
            DataType::U8 => (8, false, false),
            DataType::I8 => (8, true, false),
            DataType::U16 => (16, false, false),
            DataType::I16 => (16, true, false),
            DataType::U32 => (32, false, false),
            DataType::I32 => (32, true, false),
            DataType::U64 => (64, false, false),
            DataType::I64 => (64, true, false),
            DataType::F32 => (32, true, true),
            DataType::F64 => (64, true, true),
        }
    };
    let ((b0, s0, f0), (b1, s1, f1)) = (info(a), info(b));
    let bits = if f0 != f1 {
        let (fb, ib) = if f0 { (b0, b1) } else { (b1, b0) };
        fb.max(2 * ib)
    } else if s0 != s1 {
        if !s0 && b0 < b1 {
            b1
        } else if !s1 && b1 < b0 {
            b0
        } else {
            let (sb, ub) = if s0 { (b0, b1) } else { (b1, b0) };
            sb.max(2 * ub)
        }
    } else {
        b0.max(b1)
    };
    let (signed, floating) = (s0 || s1, f0 || f1);
    if floating {
        return if bits <= 32 { DataType::F32 } else { DataType::F64 };
    }
    match (bits, signed) {
        (..=8, false) => DataType::U8,
        (..=16, false) => DataType::U16,
        (..=32, false) => DataType::U32,
        (..=64, false) => DataType::U64,
        (..=8, true) => DataType::I8,
        (..=16, true) => DataType::I16,
        (..=32, true) => DataType::I32,
        (..=64, true) => DataType::I64,
        _ => DataType::F64,
    }
}

/// `gdal_calc.py`'s default output nodata value of a type, as the Python number it uses.
fn default_nodata(dt: DataType) -> Option<Val> {
    Some(match dt {
        DataType::U8 => Val::Int(255),
        DataType::U16 => Val::Int(65535),
        DataType::I16 => Val::Int(-32768),
        DataType::U32 => Val::Int(4294967293),
        DataType::I32 => Val::Int(-2147483647),
        DataType::F32 => Val::Float(3.402823466e38),
        DataType::F64 => Val::Float(f64::MAX),
        DataType::I8 | DataType::U64 | DataType::I64 => return None,
    })
}

fn dtype_of(dt: DataType) -> DType {
    match dt {
        DataType::U8 => DType::U8,
        DataType::I8 => DType::I8,
        DataType::U16 => DType::U16,
        DataType::I16 => DType::I16,
        DataType::U32 => DType::U32,
        DataType::I32 => DType::I32,
        DataType::U64 => DType::U64,
        DataType::I64 => DType::I64,
        DataType::F32 => DType::F32,
        DataType::F64 => DType::F64,
    }
}

/// Reads pixels `start..end` of a band as an array.
fn read_chunk(r: &wbraster::Raster, band: usize, start: usize, end: usize) -> Arr {
    let off = (band - 1) * r.rows * r.cols;
    let (s, e) = (off + start, off + end);
    macro_rules! ints {
        ($v:expr) => {
            Data::I($v[s..e].iter().map(|x| *x as i128).collect())
        };
    }
    let data = match &r.data {
        RasterData::U8(v) => ints!(v),
        RasterData::I8(v) => ints!(v),
        RasterData::U16(v) => ints!(v),
        RasterData::I16(v) => ints!(v),
        RasterData::U32(v) => ints!(v),
        RasterData::I32(v) => ints!(v),
        RasterData::U64(v) => ints!(v),
        RasterData::I64(v) => ints!(v),
        RasterData::F32(v) => Data::F(v[s..e].iter().map(|x| *x as f64).collect()),
        RasterData::F64(v) => Data::F(v[s..e].to_vec()),
    };
    Arr { dt: dtype_of(r.data_type), data }
}

/// Converts an array to an output type as GDAL's `GDALCopyWords()` does (NumPy booleans are
/// written as bytes), returning each value as `f64`.
fn to_output(a: &Arr, out: DataType, n: usize) -> Vec<f64> {
    let int_out = !matches!(out, DataType::F32 | DataType::F64);
    let (lo, hi) = super::type_range(out);
    let unsigned = matches!(out, DataType::U8 | DataType::U16 | DataType::U32 | DataType::U64);
    let conv = |i: usize| -> f64 {
        let i = if a.len() == 1 { 0 } else { i };
        match &a.data {
            Data::B(v) => v[i] as u8 as f64,
            Data::I(v) => {
                let x = v[i];
                if int_out {
                    (x.clamp(lo as i128, hi as i128)) as f64
                } else if out == DataType::F32 {
                    x as f32 as f64
                } else {
                    x as f64
                }
            }
            Data::F(v) => {
                let x = v[i];
                if !int_out {
                    return if out == DataType::F32 { x as f32 as f64 } else { x };
                }
                if x.is_nan() {
                    return 0.0;
                }
                let r = if a.dt == DType::F32 {
                    let x = x as f32;
                    (if unsigned || x > 0.0 { x + 0.5 } else { x - 0.5 }) as f64
                } else if unsigned || x > 0.0 {
                    x + 0.5
                } else {
                    x - 0.5
                };
                r.clamp(lo, hi).trunc()
            }
        }
    };
    (0..n).map(conv).collect()
}

/// Python's `%` on floats.
fn py_mod(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r != 0.0 && ((r < 0.0) != (b < 0.0)) { r + b } else { r }
}

/// `osgeo_utils`' `GeoRectangle`: `(min_x, min_y, width, height)`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn from_min_max(min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Self {
        let (w, h) = (max_x - min_x, max_y - min_y);
        Self { x: min_x, y: min_y, w: if w <= 0.0 { 0.0 } else { w }, h: if h <= 0.0 { 0.0 } else { h } }
    }

    fn of(gt: &[f64; 6], w: usize, h: usize) -> Self {
        let (mut x, mut y, mut rw, mut rh) = (gt[0], gt[3], w as f64 * gt[1], h as f64 * gt[5]);
        if rw <= 0.0 {
            x += rw;
            rw = -rw;
        }
        if rh <= 0.0 {
            y += rh;
            rh = -rh;
        }
        Self { x, y, w: rw, h: rh }
    }

    fn max_x(&self) -> f64 {
        self.x + self.w
    }

    fn max_y(&self) -> f64 {
        self.y + self.h
    }

    fn union(&self, o: &Rect) -> Rect {
        Self::from_min_max(self.x.min(o.x), self.max_x().max(o.max_x()), self.y.min(o.y), self.max_y().max(o.max_y()))
    }

    fn intersect(&self, o: &Rect) -> Rect {
        Self::from_min_max(self.x.max(o.x), self.max_x().min(o.max_x()), self.y.max(o.y), self.max_y().min(o.max_y()))
    }

    /// The pixel-aligned rectangle around this one (`GeoRectangle.align`).
    fn align(&self, gt: &[f64; 6]) -> Rect {
        let left = self.x - py_mod(self.x - gt[0], gt[1]);
        let right = self.max_x() + (gt[1] - py_mod(self.max_x() - gt[0], gt[1]));
        let bottom = self.y + (gt[5] - py_mod(self.y - gt[3], gt[5]));
        let top = self.max_y() - py_mod(self.max_y() - gt[3], gt[5]);
        let (w, h) = (right - left, top - bottom);
        Rect { x: left, y: bottom, w: if w <= 0.0 { 0.0 } else { w }, h: if h <= 0.0 { 0.0 } else { h } }
    }
}

/// Evaluates expressions over input bands, as `gdal_calc.py` does.
pub fn calc(inputs: &[CalcInput], o: &CalcOptions) -> Result<RasterDataset> {
    if o.calc.is_empty() {
        return Err(Error::InvalidArgument("no calculation provided".into()));
    }
    if inputs.is_empty() {
        return Err(Error::InvalidArgument("no input raster".into()));
    }
    let exprs = o.calc.iter().map(|c| Expr::parse(c)).collect::<Result<Vec<_>>>()?;
    for (i, a) in inputs.iter().enumerate() {
        if inputs[..i].iter().any(|b| b.name == a.name) {
            return Err(Error::InvalidArgument(format!("input '{}' is given twice", a.name)));
        }
        if a.band == 0 || a.band > a.ds.raster.bands {
            return Err(Error::InvalidArgument(format!("input '{}' has no band {}", a.name, a.band)));
        }
    }
    let names: Vec<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
    for e in &exprs {
        for n in e.names() {
            if !names.contains(&n.as_str())
                && DType::parse(&n).is_none()
                && !matches!(
                    n.as_str(),
                    "pi" | "e" | "nan" | "NaN" | "NAN" | "inf" | "Inf" | "Infinity" | "PINF" | "NINF" | "infty" | "euler_gamma"
                )
            {
                return Err(Error::InvalidArgument(format!("name '{n}' is not defined (inputs: {})", names.join(", "))));
            }
        }
    }

    // Check the inputs' sizes and georeferencing.
    let ignore = o.extent == CalcExtent::Ignore && o.projwin.is_none();
    let first = inputs[0].ds;
    let dims = (first.raster.cols, first.raster.rows);
    let mut gt_check = first.geo_transform();
    let mut differ = false;
    for a in inputs {
        let d = (a.ds.raster.cols, a.ds.raster.rows);
        if d != dims {
            differ = true;
            if matches!(o.extent, CalcExtent::Ignore | CalcExtent::Fail) && o.projwin.is_none() {
                return Err(Error::InvalidArgument(format!(
                    "the size of input '{}' ({}x{}) differs from the other inputs' ({}x{}): use an extent mode",
                    a.name, d.0, d.1, dims.0, dims.1
                )));
            }
        }
        if o.projection_check && !a.ds.crs().same_as(&first.crs()) {
            return Err(Error::InvalidArgument(format!("the CRS of input '{}' differs from the other inputs'", a.name)));
        }
        let gt = a.ds.geo_transform();
        if ignore {
            gt_check = gt;
        } else if gt != gt_check {
            let g0 = &first.geo_transform();
            let eps = 0.000001;
            let offset = eps >= (g0[0] - gt[0]).abs() + (g0[3] - gt[3]).abs();
            let pixel = eps >= (g0[1] - gt[1]).abs() + (g0[5] - gt[5]).abs();
            let rotation = eps >= (g0[2] - gt[2]).abs() + (g0[4] - gt[4]).abs();
            if !(offset && pixel && rotation) {
                differ = true;
                let reason = if !offset && o.extent == CalcExtent::Fail {
                    Some("different offset")
                } else if !pixel {
                    Some("different pixel size")
                } else if !rotation {
                    Some("different rotation")
                } else if g0[2] != 0.0 || g0[4] != 0.0 || gt[2] != 0.0 || gt[4] != 0.0 {
                    Some("non zero rotation")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    return Err(Error::InvalidArgument(format!("the geotransform of input '{}' is incompatible ({reason})", a.name)));
                }
            }
        }
    }

    // All bands of one input.
    let mut all_bands_index = None;
    let band_count = match &o.all_bands {
        Some(name) => {
            if exprs.len() > 1 {
                return Err(Error::InvalidArgument("all_bands implies a single expression".into()));
            }
            let i = inputs
                .iter()
                .position(|a| &a.name == name)
                .ok_or_else(|| Error::InvalidArgument(format!("all_bands: no input named '{name}'")))?;
            let n = inputs[i].ds.raster.bands;
            if n > 1 {
                all_bands_index = Some(i);
            }
            n
        }
        None => exprs.len(),
    };

    // Align inputs with different extents.
    let mut sources: Vec<Cow<RasterDataset>> = inputs.iter().map(|a| Cow::Borrowed(a.ds)).collect();
    let (mut width, mut height) = dims;
    if !matches!(o.extent, CalcExtent::Fail) && !ignore && (differ || o.projwin.is_some()) {
        let rect = match o.projwin {
            Some((ulx, uly, lrx, lry)) => Rect::from_min_max(ulx, lrx, lry, uly).align(&first.geo_transform()),
            None => {
                let mut r: Option<Rect> = None;
                for a in inputs {
                    let e = Rect::of(&a.ds.geo_transform(), a.ds.raster.cols, a.ds.raster.rows);
                    r = Some(match r {
                        None => e,
                        Some(r) if o.extent == CalcExtent::Union => r.union(&e),
                        Some(r) => r.intersect(&e),
                    });
                }
                r.expect("inputs")
            }
        };
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return Err(Error::InvalidArgument("the requested extent is empty".into()));
        }
        let mo = MosaicOptions { extent: Some((rect.x, rect.y, rect.max_x(), rect.max_y())), ..Default::default() };
        for (s, a) in sources.iter_mut().zip(inputs) {
            let m = mosaic(&[MosaicSource { path: a.name.clone(), ds: a.ds }], &mo)?;
            *s = Cow::Owned(m);
        }
        gt_check = sources[0].geo_transform();
        (width, height) = (sources[0].raster.cols, sources[0].raster.rows);
    }

    // The output type and nodata value.
    let out_type = match o.output_type {
        Some(t) => t,
        None => inputs.iter().map(|a| a.ds.raster.data_type).reduce(data_type_union).expect("inputs"),
    };
    let out_nodata: Option<Val> = match o.nodata {
        CalcNodata::Default if o.hide_nodata => None,
        CalcNodata::Default => default_nodata(out_type),
        CalcNodata::None => None,
        CalcNodata::Value(v) => Some(Val::Float(v)),
    };
    let in_nodata: Vec<Option<f64>> = sources.iter().map(|s| if o.hide_nodata { None } else { s.nodata() }).collect();

    // Evaluate in chunks of rows.
    let n = width * height;
    let chunk = (262_144 / width.max(1)).max(1) * width;
    let nchunks = n.div_ceil(chunk.max(1));
    let mut bands = Vec::with_capacity(band_count);
    for b in 0..band_count {
        let expr = &exprs[if exprs.len() > 1 { b } else { 0 }];
        let run = |c: usize| -> Result<Vec<f64>> {
            let (start, end) = (c * chunk, ((c + 1) * chunk).min(n));
            let len = end - start;
            let mut mask: Option<Vec<bool>> = None;
            let mut vars: Vec<(String, Val)> = Vec::with_capacity(inputs.len());
            for (i, (a, s)) in inputs.iter().zip(&sources).enumerate() {
                let band = if all_bands_index == Some(i) { b + 1 } else { a.band };
                let arr = read_chunk(&s.raster, band, start, end);
                if let Some(nd) = in_nodata[i] {
                    let eq = eval::binary_compare_eq(&Val::Arr(arr.clone()), &Val::Float(nd))?;
                    let m = mask.get_or_insert_with(|| vec![false; len]);
                    for (m, e) in m.iter_mut().zip(eq) {
                        *m |= e;
                    }
                }
                vars.push((a.name.clone(), Val::Arr(arr)));
            }
            let lookup = |name: &str| vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
            let mut result = Evaluator { lookup: &lookup }.eval(expr)?;
            let is_array = matches!(&result, Val::Arr(a) if a.len() == len);
            match (&mask, &out_nodata) {
                (Some(m), Some(nd)) => {
                    let keep = Val::Arr(Arr { dt: DType::I64, data: Data::I(m.iter().map(|x| (!x) as i128).collect()) });
                    let masked = Val::Arr(Arr { dt: DType::I64, data: Data::I(m.iter().map(|x| *x as i128).collect()) });
                    let kept = eval::binary(expr::BinOp::Mul, &keep, &result)?;
                    let filled = eval::binary(expr::BinOp::Mul, nd, &masked)?;
                    result = eval::binary(expr::BinOp::Add, &kept, &filled)?;
                }
                _ if !is_array => {
                    let ones = Val::Arr(Arr { dt: DType::F64, data: Data::F(vec![1.0; len]) });
                    result = eval::binary(expr::BinOp::Mul, &ones, &result)?;
                }
                _ => {}
            }
            let arr = match result {
                Val::Arr(a) if a.dt == DType::F16 => a.cast(DType::F32),
                Val::Arr(a) => a,
                weak => eval::binary(expr::BinOp::Mul, &Val::Arr(Arr { dt: DType::F64, data: Data::F(vec![1.0; len]) }), &weak)?.into_arr(),
            };
            Ok(to_output(&arr, out_type, len))
        };
        let chunks = run_chunks(nchunks, &run)?;
        bands.push(chunks.into_iter().flatten().collect::<Vec<f64>>());
    }
    let grid = Grid { width, height, geo_transform: gt_check };
    let nodata = out_nodata.map(|v| match v {
        Val::Int(i) => i as f64,
        // GDAL reads a Float32 nodata value this close to the largest float as that float.
        Val::Float(f) if out_type == DataType::F32 && (f as f32).abs() == f32::MAX => f as f32 as f64,
        Val::Float(f) => f,
        Val::Arr(_) => unreachable!("Python number"),
    });
    build_raster(grid, bands, out_type, nodata, &first.crs(), |v, _| v)
}

/// Runs `f` on chunks `0..n`, in parallel where threads are available.
fn run_chunks(n: usize, f: &(dyn Fn(usize) -> Result<Vec<f64>> + Sync)) -> Result<Vec<Vec<f64>>> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let threads = std::thread::available_parallelism().map_or(1, |t| t.get()).min(16).min(n.max(1));
        if threads > 1 {
            let next = AtomicUsize::new(0);
            let results: Mutex<Vec<Option<Result<Vec<f64>>>>> = Mutex::new((0..n).map(|_| None).collect());
            std::thread::scope(|s| {
                for _ in 0..threads {
                    s.spawn(|| {
                        loop {
                            let c = next.fetch_add(1, Ordering::Relaxed);
                            if c >= n {
                                break;
                            }
                            let r = f(c);
                            results.lock().expect("no panic while holding the lock")[c] = Some(r);
                        }
                    });
                }
            });
            return results
                .into_inner()
                .expect("no panic while holding the lock")
                .into_iter()
                .map(|r| r.expect("every chunk ran"))
                .collect();
        }
    }
    (0..n).map(f).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(values: Vec<f64>, w: usize, h: usize, dt: DataType, nodata: Option<f64>, x0: f64) -> RasterDataset {
        let grid = Grid { width: w, height: h, geo_transform: [x0, 1.0, 0.0, h as f64, 0.0, -1.0] };
        build_raster(grid, vec![values], dt, nodata, &crate::crs::SpatialRef::default(), super::super::convert_value).unwrap()
    }

    fn input<'a>(name: &str, ds: &'a RasterDataset) -> CalcInput<'a> {
        CalcInput { name: name.into(), ds, band: 1 }
    }

    #[test]
    fn ndvi_like() {
        let red = ds(vec![10.0, 20.0, 0.0, 40.0], 2, 2, DataType::U8, Some(0.0), 0.0);
        let nir = ds(vec![30.0, 20.0, 50.0, 200.0], 2, 2, DataType::U8, Some(0.0), 0.0);
        let o = CalcOptions {
            calc: vec!["(B.astype(float32) - A) / (B.astype(float32) + A)".into()],
            output_type: Some(DataType::F32),
            ..Default::default()
        };
        let out = calc(&[input("A", &red), input("B", &nir)], &o).unwrap();
        assert_eq!(out.raster.get(0, 0, 0), 0.5);
        assert_eq!(out.raster.get(0, 0, 1), 0.0);
        assert_eq!(out.raster.get(0, 1, 0), f32::MAX as f64);
        assert_eq!(out.nodata(), Some(f32::MAX as f64));
        // Byte arithmetic wraps, as in NumPy.
        let o = CalcOptions { calc: vec!["A + B".into()], ..Default::default() };
        let out = calc(&[input("A", &red), input("B", &nir)], &o).unwrap();
        assert_eq!(out.raster.data_type, DataType::U8);
        assert_eq!(out.raster.get(0, 1, 1), 240.0);
        assert_eq!(out.raster.get(0, 1, 0), 255.0);
        let o = CalcOptions { calc: vec!["A".into(), "B > 25".into()], hide_nodata: true, ..Default::default() };
        let out = calc(&[input("A", &red), input("B", &nir)], &o).unwrap();
        assert_eq!(out.raster.bands, 2);
        assert_eq!(out.nodata(), None);
        assert_eq!(out.raster.get(1, 0, 0), 1.0);
        assert_eq!(out.raster.get(1, 0, 1), 0.0);
    }

    #[test]
    fn extents() {
        let a = ds(vec![1.0; 4], 2, 2, DataType::I16, Some(-1.0), 0.0);
        let b = ds(vec![2.0; 4], 2, 2, DataType::I16, Some(-1.0), 1.0);
        let base = CalcOptions { calc: vec!["A + B".into()], ..Default::default() };
        assert!(calc(&[input("A", &a), input("B", &b)], &CalcOptions { extent: CalcExtent::Fail, ..base.clone() }).is_err());
        let u = calc(&[input("A", &a), input("B", &b)], &CalcOptions { extent: CalcExtent::Union, ..base.clone() }).unwrap();
        assert_eq!((u.raster.cols, u.raster.rows), (3, 2));
        assert_eq!(u.raster.get(0, 0, 1), 3.0);
        assert_eq!(u.raster.get(0, 0, 0), -32768.0);
        let i = calc(&[input("A", &a), input("B", &b)], &CalcOptions { extent: CalcExtent::Intersect, ..base.clone() }).unwrap();
        assert_eq!((i.raster.cols, i.raster.rows), (1, 2));
        assert_eq!(i.geo_transform()[0], 1.0);
        let c = ds(vec![2.0; 6], 3, 2, DataType::I16, None, 0.0);
        assert!(calc(&[input("A", &a), input("C", &c)], &base).is_err());
    }

    #[test]
    fn type_union() {
        assert_eq!(data_type_union(DataType::U8, DataType::I16), DataType::I16);
        assert_eq!(data_type_union(DataType::U16, DataType::I16), DataType::I32);
        assert_eq!(data_type_union(DataType::U8, DataType::F32), DataType::F32);
        assert_eq!(data_type_union(DataType::I32, DataType::F32), DataType::F64);
        assert_eq!(data_type_union(DataType::U32, DataType::I8), DataType::I64);
    }

    #[test]
    fn output_conversion() {
        let a = Arr { dt: DType::F64, data: Data::F(vec![2.5, -2.5, 300.0, f64::NAN, -0.4]) };
        assert_eq!(to_output(&a, DataType::U8, 5), vec![3.0, 0.0, 255.0, 0.0, 0.0]);
        assert_eq!(to_output(&a, DataType::I16, 5), vec![3.0, -3.0, 300.0, 0.0, 0.0]);
        assert_eq!(py_mod(-7.5, 2.0), 0.5);
        assert_eq!(py_mod(1.0, -3.0), -2.0);
    }
}
