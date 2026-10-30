// SPDX-FileCopyrightText: Copyright (c) 2000, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2010, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 1998, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2003, Frank Warmerdam <warmerdam@pobox.com>
// SPDX-FileCopyrightText: Copyright (c) 2008-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2001, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2008-2012, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2024, Javier Jimenez Shaw
// SPDX-FileCopyrightText: Copyright (c) 2019, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 1998, 2002, Frank Warmerdam <warmerdam@pobox.com>
// SPDX-FileCopyrightText: Copyright (c) 2007-2015, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 1999, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2007, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2014, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.12.1:
//     gcore/overview.cpp
//     gcore/rasterio.cpp
//     alg/gdalwarpkernel.cpp
//     alg/gdalresamplingkernels.h
//     frmts/gtiff/cogdriver.cpp
//     frmts/gtiff/geotiff.cpp
//     gcore/gdal_misc.cpp
//     gcore/gdalnodatamaskband.cpp
//     gcore/gdalrescaledalphaband.cpp
//     gcore/gdal_cpp_functions.h
//     gcore/gdalsse_priv.h

//! Overviews computed as GDAL 3.12.1 computes them, pixel for pixel.
//!
//! GDAL's COG driver (`frmts/gtiff/cogdriver.cpp`, `GDALCOGCreator::Create`) halves the image
//! until it fits in a tile and has `GTIFFBuildOverviewsEx` (`frmts/gtiff/gt_overview.cpp`) build
//! the levels with `GDALRegenerateOverviewsMultiBand` (`gcore/overview.cpp`): each level is
//! resampled from the level before it, a row of blocks at a time, by `GDALResampleChunk_Near`,
//! `_AverageOrRMS`, `_Mode` or `_Convolution`. The results depend on the order of the
//! floating-point operations (including the lanes of the SSE2 code, which GDAL's x86-64 builds
//! use), on the work data type (`GDALGetOvrWorkDataType`), on the blocks (rows filtered three at
//! a time, columns eight at a time), on the nodata mask and on `GDALCopyWords`' rounding: this
//! module follows each, function by function.

use crate::pixel::Pixel;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// An overview resampling method, as `GDALRegenerateOverviewsMultiBand` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Nearest,
    Average,
    Rms,
    Bilinear,
    Cubic,
    CubicSpline,
    Lanczos,
    Mode,
    Gauss,
}

impl Method {
    /// Parses a GDAL name, in any case: `NEAREST` (any name starting with `NEAR`), `AVERAGE`
    /// (starting with `AVER`), `RMS`, `BILINEAR`, `CUBIC`, `CUBICSPLINE`, `LANCZOS`, `MODE`,
    /// `GAUSS`.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        let u = s.trim().to_ascii_uppercase();
        if u.starts_with("NEAR") {
            return Some(Self::Nearest);
        }
        if u.starts_with("AVER") {
            return Some(Self::Average);
        }
        Some(match u.as_str() {
            "RMS" => Self::Rms,
            "BILINEAR" => Self::Bilinear,
            "CUBIC" => Self::Cubic,
            "CUBICSPLINE" => Self::CubicSpline,
            "LANCZOS" => Self::Lanczos,
            "MODE" => Self::Mode,
            "GAUSS" => Self::Gauss,
            _ => return None,
        })
    }

    /// The method GDAL's COG driver uses when none is given (`GetResampling` in cogdriver.cpp):
    /// nearest neighbour for paletted or complex rasters, else cubic.
    pub(crate) fn cog_default(paletted_or_complex: bool) -> Self {
        if paletted_or_complex { Self::Nearest } else { Self::Cubic }
    }

    /// The kernel radius (`GDALGetResampleFunction`: `GWKGetFilterRadius`, 1 for Gauss).
    fn radius(self) -> i64 {
        match self {
            Self::Bilinear | Self::Gauss => 1,
            Self::Cubic | Self::CubicSpline => 2,
            Self::Lanczos => 3,
            _ => 0,
        }
    }
}

/// The overview sizes of GDAL's COG driver (`GDALCOGCreator::Create`): the size halved, rounded
/// down (at least 1), until both dimensions fit in a `block`, or `count` levels
/// (`OVERVIEW_COUNT`, stopping at 1 x 1).
pub(crate) fn cog_sizes(cols: usize, rows: usize, block: usize, count: Option<usize>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if !(cols > block || rows > block || count.is_some_and(|c| c > 0)) {
        return out;
    }
    let (mut x, mut y) = (cols, rows);
    loop {
        match count {
            None if x <= block && y <= block => break,
            Some(c) if out.len() == c || (x == 1 && y == 1) => break,
            _ => {}
        }
        x = (x / 2).max(1);
        y = (y / 2).max(1);
        out.push((x, y));
    }
    out
}

/// The block size of the overviews GDAL computes (`GTIFFGetOverviewBlockSize`): the source's
/// blocks when they are square, a power of two from 64 to 4096; else 128.
pub(crate) fn ovr_block(source_block: Option<(usize, usize)>) -> usize {
    match source_block {
        Some((w, h)) if w == h && (64..=4096).contains(&w) && w.is_power_of_two() => w,
        _ => 128,
    }
}

/// `GDALGetOvrWorkDataType`: the type the source values are read as (`None`: their own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    U8,
    U16,
    F32,
    F64,
}

fn work(method: Method, t: DataType) -> Option<Work> {
    if matches!(method, Method::Nearest | Method::Mode) {
        return None;
    }
    if method == Method::Gauss {
        return Some(Work::F64);
    }
    Some(match t {
        DataType::U8 => Work::U8,
        DataType::U16 => Work::U16,
        DataType::I8 | DataType::I16 | DataType::F32 => Work::F32,
        _ => Work::F64,
    })
}

fn work_size(method: Method, t: DataType) -> i64 {
    match work(method, t) {
        None => t.size_bytes() as i64,
        Some(Work::U8) => 1,
        Some(Work::U16) => 2,
        Some(Work::F32) => 4,
        Some(Work::F64) => 8,
    }
}

/// How one overview level is computed from the level before it: the destination blocks
/// `GDALRegenerateOverviewsMultiBand` loops over and the source window each reads.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub(crate) method: Method,
    pub(crate) src: (usize, usize),
    pub(crate) dst: (usize, usize),
    rx: f64,
    ry: f64,
    /// `nKernelRadius * nOvrFactor`: the source pixels read around a block.
    margin: i64,
    /// The destination block (`nDstChunkXSize`, `nDstChunkYSize`).
    chunk: (usize, usize),
}

/// A destination block's source window along one axis: the first pixel and count read.
#[derive(Debug, Clone, Copy)]
struct Span {
    dst0: usize,
    dst_n: usize,
    q0: i64,
    qn: i64,
}

impl Plan {
    /// The plan of the level `dst` computed from `src`, for `bands` bands of type `t`, with
    /// overview blocks of `block` pixels (see [`ovr_block`]).
    pub(crate) fn new(method: Method, src: (usize, usize), dst: (usize, usize), bands: usize, block: usize, t: DataType) -> Self {
        let rx = src.0 as f64 / dst.0 as f64;
        let ry = src.1 as f64 / dst.1 as f64;
        let factor = 1i64.max(((0.5 + rx) as i64).max((0.5 + ry) as i64));
        let margin = method.radius() * factor;
        let wrk = work_size(method, t).max(1);
        // GDALRegenerateOverviewsMultiBand: the blocks are widened while the source they read
        // stays under 10 MB.
        const CHUNK_MAX: i64 = 10 * 1024 * 1024;
        let block = block as i64;
        let full_y = (src.1 as f64).min(2.0 + block as f64 * ry) as i64;
        let full_y_q = (src.1 as i64).min(full_y + 2 * margin);
        let mut cx = block;
        while cx < dst.0 as i64 {
            let full_x = (src.0 as f64).min(2.0 + (2 * cx) as f64 * rx) as i64;
            let full_x_q = (src.0 as i64).min(full_x + 2 * margin);
            if bands as i64 > CHUNK_MAX / full_x_q / full_y_q / wrk {
                break;
            }
            cx *= 2;
        }
        let cx = cx.min(dst.0 as i64) as usize;
        Self { method, src, dst, rx, ry, margin, chunk: (cx, block as usize) }
    }

    fn span(&self, dst0: usize, dst_n: usize, ratio: f64, src: usize, dst: usize) -> Span {
        let c0 = (dst0 as f64 * ratio) as i64;
        let mut c1 = ((dst0 + dst_n) as f64 * ratio).ceil() as i64;
        if c1 > src as i64 || dst0 + dst_n == dst {
            c1 = src as i64;
        }
        let mut q0 = c0 - self.margin;
        let mut qn = c1 - c0 + 2 * self.margin;
        if q0 < 0 {
            qn += q0;
            q0 = 0;
        }
        if qn + q0 > src as i64 {
            qn = src as i64 - q0;
        }
        Span { dst0, dst_n, q0, qn }
    }

    fn y_span(&self, y0: usize) -> Span {
        let n = self.chunk.1.min(self.dst.1 - y0);
        self.span(y0, n, self.ry, self.src.1, self.dst.1)
    }

    /// The destination rows of the block row from `y0`, and the source rows it reads.
    pub(crate) fn rows(&self, y0: usize) -> (usize, std::ops::Range<usize>) {
        let s = self.y_span(y0);
        (s.dst_n, s.q0 as usize..(s.q0 + s.qn) as usize)
    }

    /// The first source row the block rows from `y0` on read.
    pub(crate) fn first_row(&self, y0: usize) -> usize {
        if y0 >= self.dst.1 { self.src.1 } else { self.rows(y0).1.start }
    }
}

/// The nodata handling of the destination: `replaceValIfNodata` in
/// `GDALResampleChunk_ConvolutionT`, which keeps valid results off the nodata value.
#[derive(Clone, Copy)]
struct Replace {
    has: bool,
    int: bool,
    min: f64,
    max: f64,
    nodata: f64,
    nodata_i64: Option<i64>,
    replacement: f64,
}

/// `GDALGetNoDataReplacementValue`.
fn replacement_value(t: DataType, nodata: f64) -> f64 {
    let int = |min: f64, max: f64| {
        if !(nodata >= min && nodata <= max) {
            0.0
        } else if nodata == max {
            max - 1.0
        } else {
            nodata + 1.0
        }
    };
    match t {
        DataType::U8 => int(0.0, 255.0),
        DataType::I8 => int(-128.0, 127.0),
        DataType::U16 => int(0.0, 65535.0),
        DataType::I16 => int(-32768.0, 32767.0),
        DataType::U32 => int(0.0, 4294967295.0),
        DataType::I32 => int(-2147483648.0, 2147483647.0),
        DataType::U64 => int(0.0, u64::MAX as f64),
        DataType::I64 => int(i64::MIN as f64, i64::MAX as f64),
        DataType::F32 => {
            if nodata < f32::MIN as f64 || nodata > f32::MAX as f64 {
                0.0
            } else if nodata == f32::MAX as f64 {
                next_after_f32(nodata as f32, 0.0) as f64
            } else {
                next_after_f32(nodata as f32, f32::MAX) as f64
            }
        }
        DataType::F64 => {
            if nodata == f64::MAX {
                next_after_f64(nodata, 0.0)
            } else {
                next_after_f64(nodata, f64::MAX)
            }
        }
    }
}

fn next_after_f32(x: f32, toward: f32) -> f32 {
    if x.is_nan() || toward.is_nan() {
        return f32::NAN;
    }
    if x == toward {
        return toward;
    }
    if x == 0.0 {
        return if toward > 0.0 { f32::from_bits(1) } else { -f32::from_bits(1) };
    }
    let b = x.to_bits();
    f32::from_bits(if (x < toward) == (x > 0.0) { b + 1 } else { b - 1 })
}

fn next_after_f64(x: f64, toward: f64) -> f64 {
    if x.is_nan() || toward.is_nan() {
        return f64::NAN;
    }
    if x == toward {
        return toward;
    }
    if x == 0.0 {
        return if toward > 0.0 { f64::from_bits(1) } else { -f64::from_bits(1) };
    }
    let b = x.to_bits();
    f64::from_bits(if (x < toward) == (x > 0.0) { b + 1 } else { b - 1 })
}

/// A value rounded to the work type (`Twork`): `float` or `double`.
#[inline]
fn twork(v: f64, single: bool) -> f64 {
    if single { v as f32 as f64 } else { v }
}

impl Replace {
    fn new(t: DataType, nodata: Option<f64>, single: bool) -> Self {
        let (int, min, max) = match t {
            DataType::U8 => (true, 0.0, 255.0),
            DataType::I8 => (true, -128.0, 127.0),
            DataType::U16 => (true, 0.0, 65535.0),
            DataType::I16 => (true, -32768.0, 32767.0),
            DataType::U32 => (true, 0.0, 4294967295.0),
            DataType::I32 => (true, -2147483648.0, 2147483647.0),
            DataType::U64 => (true, 0.0, 18446744073709549568.0),
            DataType::I64 => (true, i64::MIN as f64, 9223372036854774784.0),
            _ if single => (false, f32::MIN as f64, f32::MAX as f64),
            _ => (false, f64::MIN, f64::MAX),
        };
        let nd = nodata.unwrap_or(0.0);
        let nodata_i64 = (int && nd.fract() == 0.0 && nd >= i64::MIN as f64 && nd < i64::MAX as f64).then_some(nd as i64);
        Self {
            has: nodata.is_some(),
            int,
            min: twork(min, single),
            max: twork(max, single),
            nodata: nd,
            nodata_i64,
            replacement: twork(if nodata.is_some() { replacement_value(t, nd) } else { nd }, single),
        }
    }

    #[inline]
    fn apply(&self, v: f64) -> f64 {
        if !self.has {
            return v;
        }
        let c = if v < self.min {
            self.min
        } else if v > self.max {
            self.max
        } else {
            v
        };
        if self.int {
            if let Some(n) = self.nodata_i64 {
                let r = c.round();
                if r >= i64::MIN as f64 && r <= self.max.max(9223372036854774784.0) && n == r as i64 {
                    return self.replacement;
                }
            }
        } else if self.nodata == c {
            return self.replacement;
        }
        c
    }
}

/// A pixel type of an overview: GDAL's conversions of work values to it (`GDALCopyWords`) and
/// its nodata mask (`GDALNoDataMaskBand`).
pub(crate) trait Sample: Pixel {
    const DT: DataType;
    /// `GDALCopyWords` of a `float` (`single`) or `double` work value.
    fn from_work(v: f64, single: bool) -> Self;
    /// Whether the nodata value is one of the type's values (`GDALNoDataMaskBand::IsNoDataInRange`).
    fn nodata_in_range(nd: f64) -> bool;
    /// Whether the value is the nodata value (`GDALNoDataMaskBand::IReadBlock`).
    fn is_nodata(self, nd: f64) -> bool;
    /// The mode kernel's value for empty windows (its type is unsigned for signed integers of 16
    /// bits and more, as `GDALResampleChunk_Mode` dispatches them).
    fn mode_nodata(nd: Option<f64>) -> Self;
    /// `IsSame` of the mode kernel.
    fn same(self, other: Self) -> bool {
        self == other
    }
}

macro_rules! unsigned_sample {
    ($t:ty, $dt:ident, $max:expr) => {
        impl Sample for $t {
            const DT: DataType = DataType::$dt;
            #[inline]
            fn from_work(v: f64, single: bool) -> Self {
                if v.is_nan() {
                    return 0;
                }
                if single {
                    let f = (v as f32 + 0.5f32).clamp(0.0, $max as f32);
                    f as $t
                } else {
                    (v + 0.5).clamp(0.0, $max as f64) as $t
                }
            }
            fn nodata_in_range(nd: f64) -> bool {
                nd >= 0.0 && nd <= $max as f64
            }
            #[inline]
            fn is_nodata(self, nd: f64) -> bool {
                self == nd as $t
            }
            fn mode_nodata(nd: Option<f64>) -> Self {
                nd.filter(|n| Self::nodata_in_range(*n)).map_or(0, |n| n as $t)
            }
        }
    };
}
unsigned_sample!(u8, U8, 255u32);
unsigned_sample!(u16, U16, 65535u32);
unsigned_sample!(u32, U32, 4294967295u64);
unsigned_sample!(u64, U64, 18446744073709551615u64);

macro_rules! signed_sample {
    ($t:ty, $dt:ident, $ut:ty) => {
        impl Sample for $t {
            const DT: DataType = DataType::$dt;
            #[inline]
            fn from_work(v: f64, single: bool) -> Self {
                if v.is_nan() {
                    return 0;
                }
                if single {
                    let f = v as f32;
                    let f = if f >= 0.0 { f + 0.5 } else { f - 0.5 };
                    f.clamp(<$t>::MIN as f32, <$t>::MAX as f32) as $t
                } else {
                    let d = if v > 0.0 { v + 0.5 } else { v - 0.5 };
                    d.clamp(<$t>::MIN as f64, <$t>::MAX as f64) as $t
                }
            }
            fn nodata_in_range(nd: f64) -> bool {
                nd >= <$t>::MIN as f64 && nd <= <$t>::MAX as f64
            }
            #[inline]
            fn is_nodata(self, nd: f64) -> bool {
                self == nd as $t
            }
            fn mode_nodata(nd: Option<f64>) -> Self {
                // GDAL's mode kernel reads these as the unsigned type of their size.
                match nd {
                    Some(n) if n >= <$ut>::MIN as f64 && n <= <$ut>::MAX as f64 => (n as $ut) as $t,
                    _ => 0,
                }
            }
        }
    };
}
signed_sample!(i16, I16, u16);
signed_sample!(i32, I32, u32);
signed_sample!(i64, I64, u64);

impl Sample for i8 {
    const DT: DataType = DataType::I8;
    #[inline]
    fn from_work(v: f64, single: bool) -> Self {
        if v.is_nan() {
            return 0;
        }
        let _ = single;
        let f = v as f32;
        let f = if f >= 0.0 { f + 0.5 } else { f - 0.5 };
        f.clamp(-128.0, 127.0) as i8
    }
    fn nodata_in_range(nd: f64) -> bool {
        (-128.0..=127.0).contains(&nd)
    }
    #[inline]
    fn is_nodata(self, nd: f64) -> bool {
        self == nd as i8
    }
    fn mode_nodata(nd: Option<f64>) -> Self {
        nd.filter(|n| Self::nodata_in_range(*n)).map_or(0, |n| n as i8)
    }
}

/// `ARE_REAL_EQUAL` of GDAL 3.12 (`gcore/gdal_cpp_functions.h`): equal, or closer than
/// `FLT_EPSILON * |a + b| * 2`, computed in `$t`. Doubles take FLT_EPSILON too, not DBL_EPSILON.
macro_rules! real_equal {
    ($a:expr, $b:expr, $t:ty) => {
        $a == $b || ($a - $b).abs() < f32::EPSILON as $t * ($a + $b).abs() * 2.0
    };
}

impl Sample for f32 {
    const DT: DataType = DataType::F32;
    #[inline]
    fn from_work(v: f64, _single: bool) -> Self {
        v as f32
    }
    fn nodata_in_range(nd: f64) -> bool {
        nd.is_nan() || nd.is_infinite() || (nd >= f32::MIN as f64 && nd <= f32::MAX as f64)
    }
    #[inline]
    fn is_nodata(self, nd: f64) -> bool {
        if nd.is_nan() {
            self.is_nan()
        } else {
            let n = nd as f32;
            real_equal!(self, n, f32)
        }
    }
    fn mode_nodata(nd: Option<f64>) -> Self {
        nd.filter(|n| Self::nodata_in_range(*n)).map_or(0.0, |n| n as f32)
    }
    fn same(self, other: Self) -> bool {
        self == other || (self.is_nan() && other.is_nan())
    }
}

impl Sample for f64 {
    const DT: DataType = DataType::F64;
    #[inline]
    fn from_work(v: f64, _single: bool) -> Self {
        v
    }
    fn nodata_in_range(_nd: f64) -> bool {
        true
    }
    #[inline]
    fn is_nodata(self, nd: f64) -> bool {
        if nd.is_nan() { self.is_nan() } else { real_equal!(self, nd, f64) }
    }
    fn mode_nodata(nd: Option<f64>) -> Self {
        nd.unwrap_or(0.0)
    }
    fn same(self, other: Self) -> bool {
        self == other || (self.is_nan() && other.is_nan())
    }
}

/// What a kernel computes: `GDALOverviewResampleArgs`, in source and destination pixels.
struct Args<'a> {
    rx: f64,
    ry: f64,
    /// The source window's offset past its first pixel (`dfSrcXDelta`, `dfSrcYDelta`): 0 for
    /// overviews; the fraction of a floating-point window in `RasterIOResampled`.
    sdx: f64,
    sdy: f64,
    /// The source window read (`nChunkXOff`, `nChunkXSize`...).
    cx0: i64,
    cxn: i64,
    cy0: i64,
    cyn: i64,
    /// The destination block (`nDstXOff`, `nDstXOff2`...).
    dx0: i64,
    dx1: i64,
    dy0: i64,
    dy1: i64,
    /// The source's validity, 0 or 255 per pixel (`pabyChunkNodataMask`).
    mask: Option<&'a [u8]>,
    nodata: Option<f64>,
    /// The overview's data type (`eOvrDataType`).
    dt: DataType,
    /// The overview's size (`nOvrXSize`, `nOvrYSize`): the buffer's, past its virtual offset, in
    /// `RasterIOResampled`.
    ovr: (i64, i64),
}

// The kernels of `GWKGetFilterFunc` and `GWKGetFilterFunc4Values` (alg/gdalwarpkernel.cpp and
// alg/gdalresamplingkernels.h).

#[inline]
fn cubic(x: f64) -> f64 {
    let a = x.abs();
    if a <= 1.0 {
        let x2 = x * x;
        x2 * (1.5 * a - 2.5) + 1.0
    } else if a <= 2.0 {
        let x2 = x * x;
        x2 * (-0.5 * a + 2.5) - 4.0 * a + 2.0
    } else {
        0.0
    }
}

/// `GWKBSpline`: six times the cubic B-spline.
#[inline]
fn bspline(x: f64) -> f64 {
    let xp2 = x + 2.0;
    let xp1 = x + 1.0;
    let xm1 = x - 1.0;
    let xp2c = xp2 * xp2 * xp2;
    if xp2 > 0.0 {
        (if xp1 > 0.0 {
            (if x > 0.0 { (if xm1 > 0.0 { -4.0 * xm1 * xm1 * xm1 } else { 0.0 }) + 6.0 * x * x * x } else { 0.0 }) + -4.0 * xp1 * xp1 * xp1
        } else {
            0.0
        }) + xp2c
    } else {
        0.0
    }
}

/// `GWKLanczosSinc`, of radius 3.
#[inline]
fn lanczos(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let pix = std::f64::consts::PI * x;
    let pix_over_r = pix / 3.0;
    let pix2_over_r = pix * pix_over_r;
    let s = pix_over_r.sin();
    let s2 = s * s;
    let num = (3.0 - 4.0 * s2) * s2;
    num / pix2_over_r
}

#[inline]
fn bilinear(x: f64) -> f64 {
    let a = x.abs();
    if a <= 1.0 { 1.0 - a } else { 0.0 }
}

#[inline]
fn filter(m: Method, x: f64) -> f64 {
    match m {
        Method::Bilinear => bilinear(x),
        Method::Cubic => cubic(x),
        Method::CubicSpline => bspline(x),
        _ => lanczos(x),
    }
}

/// The `4Values` filters: four weights in place, and their sum from the left.
#[inline]
fn filter4(m: Method, w: &mut [f64]) -> f64 {
    for v in w.iter_mut().take(4) {
        *v = filter(m, *v);
    }
    w[0] + w[1] + w[2] + w[3]
}

/// `XMMReg4Double::GetHorizSum` without AVX: `(a0 + a2) + (a1 + a3)`.
#[inline]
fn hsum(a: [f64; 4]) -> f64 {
    (a[0] + a[2]) + (a[1] + a[3])
}

/// A source value of a convolution: `GByte` and `GUInt16` have GDAL's SSE2 specializations,
/// floating-point values the generic code with rescaling.
trait ConvSrc: Copy + Send + Sync {
    const SSE: bool;
    const FLOAT: bool;
    /// The work type (`Twork`) is `float`.
    const SINGLE: bool;
    fn d(self) -> f64;
}

impl ConvSrc for u8 {
    const SSE: bool = true;
    const FLOAT: bool = false;
    const SINGLE: bool = true;
    #[inline]
    fn d(self) -> f64 {
        self as f64
    }
}

impl ConvSrc for u16 {
    const SSE: bool = true;
    const FLOAT: bool = false;
    const SINGLE: bool = true;
    #[inline]
    fn d(self) -> f64 {
        self as f64
    }
}

impl ConvSrc for f32 {
    const SSE: bool = false;
    const FLOAT: bool = true;
    const SINGLE: bool = true;
    #[inline]
    fn d(self) -> f64 {
        self as f64
    }
}

impl ConvSrc for f64 {
    const SSE: bool = false;
    const FLOAT: bool = true;
    const SINGLE: bool = false;
    #[inline]
    fn d(self) -> f64 {
        self
    }
}

/// `GDALResampleConvolutionHorizontalSSE2`: one row.
#[inline]
fn h_sse_row<T: ConvSrc>(p: &[T], w: &[f64], n: usize) -> f64 {
    let (mut a1, mut a2) = ([0.0f64; 4], [0.0f64; 4]);
    let mut i = 0;
    while i + 7 < n {
        for k in 0..4 {
            a1[k] += p[i + k].d() * w[i + k];
            a2[k] += p[i + 4 + k].d() * w[i + 4 + k];
        }
        i += 8;
    }
    for k in 0..4 {
        a1[k] += a2[k];
    }
    let mut v = hsum(a1);
    while i < n {
        v += p[i].d() * w[i];
        i += 1;
    }
    v
}

/// `GDALResampleConvolutionHorizontal_3rows_SSE2`, for one of its rows.
#[inline]
fn h_sse_3rows<T: ConvSrc>(p: &[T], w: &[f64], n: usize) -> f64 {
    let mut a = [0.0f64; 4];
    let mut i = 0;
    while i + 7 < n {
        for k in 0..4 {
            a[k] += p[i + k].d() * w[i + k];
        }
        for k in 0..4 {
            a[k] += p[i + 4 + k].d() * w[i + 4 + k];
        }
        i += 8;
    }
    let mut v = hsum(a);
    while i < n {
        v += p[i].d() * w[i];
        i += 1;
    }
    v
}

/// `GDALResampleConvolutionHorizontalPixelCountLess8_3rows_SSE2`, for one of its rows.
#[inline]
fn h_sse_less8<T: ConvSrc>(p: &[T], w: &[f64], n: usize) -> f64 {
    let mut a = [0.0f64; 4];
    let mut i = 0;
    while i + 3 < n {
        for k in 0..4 {
            a[k] += p[i + k].d() * w[i + k];
        }
        i += 4;
    }
    let mut v = hsum(a);
    while i < n {
        v += p[i].d() * w[i];
        i += 1;
    }
    v
}

/// `GDALResampleConvolutionHorizontalPixelCount4_3rows_SSE2`, for one of its rows.
#[inline]
fn h_sse_count4<T: ConvSrc>(p: &[T], w: &[f64]) -> f64 {
    hsum([p[0].d() * w[0], p[1].d() * w[1], p[2].d() * w[2], p[3].d() * w[3]])
}

/// `GDALResampleConvolutionHorizontal` (and `_3rows`, the same for each row): generic.
#[inline]
fn h_generic<T: ConvSrc>(p: &[T], w: &[f64], n: usize) -> f64 {
    let (mut v1, mut v2) = (0.0, 0.0);
    let mut i = 0;
    while i + 3 < n {
        v1 += p[i].d() * w[i];
        v1 += p[i + 1].d() * w[i + 1];
        v2 += p[i + 2].d() * w[i + 2];
        v2 += p[i + 3].d() * w[i + 3];
        i += 4;
    }
    while i < n {
        v1 += p[i].d() * w[i];
        i += 1;
    }
    v1 + v2
}

/// `GDALResampleConvolutionHorizontalWithMask`: SSE2 for `GByte` and `GUInt16`, else generic.
#[inline]
fn h_masked<T: ConvSrc>(p: &[T], m: &[u8], w: &[f64], n: usize) -> (f64, f64) {
    let mut i = 0;
    if T::SSE {
        let (mut a, mut aw) = ([0.0f64; 4], [0.0f64; 4]);
        while i + 3 < n {
            for k in 0..4 {
                let wk = w[i + k] * m[i + k] as f64;
                a[k] += p[i + k].d() * wk;
                aw[k] += wk;
            }
            i += 4;
        }
        let (mut v, mut ws) = (hsum(a), hsum(aw));
        while i < n {
            let wk = w[i] * m[i] as f64;
            v += p[i].d() * wk;
            ws += wk;
            i += 1;
        }
        (v, ws)
    } else {
        let (mut v, mut ws) = (0.0, 0.0);
        while i + 3 < n {
            let w0 = w[i] * m[i] as f64;
            let w1 = w[i + 1] * m[i + 1] as f64;
            let w2 = w[i + 2] * m[i + 2] as f64;
            let w3 = w[i + 3] * m[i + 3] as f64;
            v += p[i].d() * w0;
            v += p[i + 1].d() * w1;
            v += p[i + 2].d() * w2;
            v += p[i + 3].d() * w3;
            ws += w0 + w1 + w2 + w3;
            i += 4;
        }
        while i < n {
            let wk = w[i] * m[i] as f64;
            v += p[i].d() * wk;
            ws += wk;
            i += 1;
        }
        (v, ws)
    }
}

/// `GDALResampleConvolutionVertical`: one column of the horizontally filtered rows.
#[inline]
fn v_generic(h: &[f64], start: usize, stride: usize, w: &[f64], n: usize) -> f64 {
    let (mut v1, mut v2) = (0.0, 0.0);
    let mut i = 0;
    let mut j = start;
    while i + 3 < n {
        v1 += h[j] * w[i];
        v1 += h[j + stride] * w[i + 1];
        v2 += h[j + 2 * stride] * w[i + 2];
        v2 += h[j + 3 * stride] * w[i + 3];
        i += 4;
        j += 4 * stride;
    }
    while i < n {
        v1 += h[j] * w[i];
        i += 1;
        j += stride;
    }
    v1 + v2
}

/// The longest run of valid pixels.
#[inline]
fn longest_valid(m: impl Iterator<Item = bool>) -> i64 {
    let (mut run, mut best) = (0i64, 0i64);
    for valid in m {
        if valid {
            run += 1;
        } else if run > 0 {
            best = best.max(run);
            run = 0;
        }
    }
    best.max(run)
}

/// `ScaleValue` of the horizontal pass: undoes the rescaling of floating-point sums, and keeps
/// infinite inputs of one sign infinite.
#[inline]
fn scale_h<T: ConvSrc>(v: f64, mul: f64, neg: bool, inputs: &[T]) -> f64 {
    if !T::FLOAT {
        return v;
    }
    if v.is_finite() {
        return v.clamp(-f64::MAX / mul, f64::MAX / mul) * mul;
    }
    if neg && v.is_nan() {
        let positive = inputs[0].d() >= 0.0;
        for x in inputs {
            let x = x.d();
            if x.is_nan() || (x >= 0.0) != positive {
                return v;
            }
        }
        return inputs[0].d();
    }
    v
}

/// `GDALResampleChunk_ConvolutionT`: the values of the destination block, in the work type.
fn convolution<T: ConvSrc>(method: Method, a: &Args, chunk: &[T]) -> Vec<f64> {
    let radius = method.radius() as f64;
    let neg = matches!(method, Method::Cubic | Method::Lanczos);
    let rescale = matches!(method, Method::Cubic | Method::Lanczos | Method::CubicSpline);
    let mul = if rescale && T::FLOAT { 2.0 } else { 1.0 };
    let nodata = a.nodata.unwrap_or(0.0);
    let replace = Replace::new(a.dt, a.nodata, T::SINGLE);
    let dst_w = (a.dx1 - a.dx0) as usize;
    let dst_h = (a.dy1 - a.dy0) as usize;
    let x_scale = 1.0 / a.rx;
    let x_sw = if x_scale >= 1.0 { 1.0 } else { x_scale };
    let x_rad = radius / x_sw;
    let y_scale = 1.0 / a.ry;
    let y_sw = if y_scale >= 1.0 { 1.0 } else { y_scale };
    let y_rad = radius / y_sw;
    let (xn, ny) = (a.cxn as usize, a.cyn as usize);
    let mut hf = vec![0.0f64; ny * dst_w];
    let mut hm = if a.mask.is_some() { vec![0u8; ny * dst_w] } else { Vec::new() };
    let mut w = vec![0.0f64; (4.0 + 2.0 * x_rad.max(y_rad)) as usize + 8];

    // First pass: horizontal filter. The weights of each destination column are computed as
    // GDAL computes them, then applied row by row (GDAL loops over columns, then rows: each
    // value is computed alike either way).
    let right = a.cx0 + a.cxn;
    let less8 = x_rad < 4.0;
    let taps = w.len();
    let mut cols: Vec<(usize, usize)> = Vec::with_capacity(dst_w);
    let mut weights = vec![0.0f64; dst_w * taps];
    for xd in a.dx0..a.dx1 {
        let c = (xd as f64 + 0.5) * a.rx + a.sdx;
        let mut s0 = (c - x_rad + 0.5).floor() as i64;
        if s0 < a.cx0 {
            s0 = a.cx0;
        }
        let mut s1 = (c + x_rad + 0.5) as i64;
        if s1 > right {
            s1 = right;
        }
        let n = (s1 - s0).max(0) as usize;
        let w = &mut weights[(xd - a.dx0) as usize * taps..][..taps];
        let mut sum = 0.0;
        let mut k = 0usize;
        let mut x = x_sw * ((s0 as f64 - c) + 0.5);
        while (k as i64) < n as i64 - 3 {
            w[k] = x;
            x += x_sw;
            w[k + 1] = x;
            x += x_sw;
            w[k + 2] = x;
            x += x_sw;
            w[k + 3] = x;
            x += x_sw;
            sum += filter4(method, &mut w[k..k + 4]);
            k += 4;
        }
        while k < n {
            let v = filter(method, x);
            w[k] = v;
            sum += v;
            k += 1;
            x += x_sw;
        }
        if a.mask.is_none() && sum != 0.0 {
            let inv = 1.0 / (mul * sum);
            for v in w.iter_mut().take(n) {
                *v *= inv;
            }
        }
        cols.push(((s0 - a.cx0) as usize, n));
    }
    // GDAL's SSE2 code filters rows three at a time, then the last ones alone.
    let triples = ny / 3 * 3;
    for r in 0..ny {
        let row = &chunk[r * xn..(r + 1) * xn];
        let out = &mut hf[r * dst_w..(r + 1) * dst_w];
        match a.mask {
            None => {
                for (col, (o, &(off, n))) in out.iter_mut().zip(&cols).enumerate() {
                    let w = &weights[col * taps..][..taps];
                    let p = &row[off..off + n];
                    *o = if !T::SSE {
                        scale_h(h_generic(p, w, n), mul, neg, p)
                    } else if r >= triples {
                        h_sse_row(p, w, n)
                    } else if n == 4 {
                        h_sse_count4(p, w)
                    } else if less8 {
                        h_sse_less8(p, w, n)
                    } else {
                        h_sse_3rows(p, w, n)
                    };
                }
            }
            Some(mask) => {
                let mrow = &mask[r * xn..(r + 1) * xn];
                let hrow = &mut hm[r * dst_w..(r + 1) * dst_w];
                for (col, ((o, valid), &(off, n))) in out.iter_mut().zip(hrow.iter_mut()).zip(&cols).enumerate() {
                    let m = &mrow[off..off + n];
                    if neg && longest_valid(m.iter().map(|v| *v != 0)) < n as i64 / 2 {
                        *o = 0.0;
                        *valid = 0;
                        continue;
                    }
                    let (v, ws) = h_masked(&row[off..], m, &weights[col * taps..][..taps], n);
                    if ws > 0.0 {
                        *o = v / ws;
                        *valid = 1;
                    } else {
                        *o = 0.0;
                        *valid = 0;
                    }
                }
            }
        }
    }

    // Second pass: vertical filter.
    let mut out = vec![0.0f64; dst_h * dst_w];
    let bottom = a.cy0 + a.cyn;
    // The SSE2 code of 8 columns (`GDALResampleConvolutionVertical_8cols`) for a work type of
    // `float`, unless the source is `float` and its sums are rescaled.
    let eight = T::SINGLE && (!rescale || T::SSE);
    for yd in a.dy0..a.dy1 {
        let c = (yd as f64 + 0.5) * a.ry + a.sdy;
        let mut s0 = (c - y_rad + 0.5).floor() as i64;
        let mut s1 = (c + y_rad + 0.5) as i64;
        if s0 < a.cy0 {
            s0 = a.cy0;
        }
        if s1 > bottom {
            s1 = bottom;
        }
        let n = (s1 - s0).max(0) as usize;
        let mut sum = 0.0;
        let mut k = 0usize;
        let mut y = y_sw * ((s0 as f64 - c) + 0.5);
        while (k as i64) < n as i64 - 3 {
            w[k] = y;
            w[k + 1] = y + y_sw;
            w[k + 2] = y + 2.0 * y_sw;
            w[k + 3] = y + 3.0 * y_sw;
            sum += filter4(method, &mut w[k..k + 4]);
            k += 4;
            y += 4.0 * y_sw;
        }
        while k < n {
            let v = filter(method, y);
            w[k] = v;
            sum += v;
            k += 1;
            y += y_sw;
        }
        let row = &mut out[(yd - a.dy0) as usize * dst_w..][..dst_w];
        let base = (s0 - a.cy0) as usize * dst_w;
        if a.mask.is_none() {
            if sum != 0.0 {
                let inv = 1.0 / (mul * sum);
                for v in w.iter_mut().take(n) {
                    *v *= inv;
                }
            }
            if eight {
                let mut col = 0usize;
                while (col as i64) < dst_w as i64 - 7 {
                    for (k, o) in row[col..col + 8].iter_mut().enumerate() {
                        let mut acc = 0.0f64;
                        let mut j = base + col + k;
                        for wi in w.iter().take(n) {
                            acc += hf[j] * wi;
                            j += dst_w;
                        }
                        *o = replace.apply(acc as f32 as f64);
                    }
                    col += 8;
                }
                for (k, o) in row.iter_mut().enumerate().skip(col) {
                    *o = replace.apply(v_generic(&hf, base + k, dst_w, &w, n) as f32 as f64);
                }
            } else {
                let lim = if T::SINGLE { f32::MAX as f64 } else { f64::MAX };
                for (k, o) in row.iter_mut().enumerate() {
                    let mut v = v_generic(&hf, base + k, dst_w, &w, n);
                    if v.is_finite() {
                        v = v.clamp(-lim / mul, lim / mul) * mul;
                    } else if neg && v.is_nan() {
                        // As GDAL: NaN checks down the column, signs along the row.
                        let at = |i: usize| hf.get(base + k + i).copied().unwrap_or(f64::NAN);
                        let positive = hf[base + k] >= 0.0;
                        let mut keep = true;
                        for i in 0..n {
                            if hf[base + k + i * dst_w].is_nan() || (at(i) >= 0.0) != positive {
                                keep = false;
                                break;
                            }
                        }
                        if keep {
                            v = hf[base + k];
                        }
                    }
                    *o = replace.apply(twork(v, T::SINGLE));
                }
            }
        } else {
            for (k, o) in row.iter_mut().enumerate() {
                let (mut v, mut ws) = (0.0, 0.0);
                let mut j = base + k;
                let (mut run, mut best) = (0i64, 0i64);
                for wi in w.iter().take(n) {
                    let dw = wi * hm[j] as f64;
                    if neg {
                        if hm[j] != 0 {
                            run += 1;
                        } else if run > 0 {
                            best = best.max(run);
                            run = 0;
                        }
                    }
                    v += hf[j] * dw;
                    ws += dw;
                    j += dst_w;
                }
                if neg && best.max(run) < n as i64 / 2 {
                    *o = twork(nodata, T::SINGLE);
                    continue;
                }
                *o = if ws > 0.0 { replace.apply(twork(v / ws, T::SINGLE)) } else { twork(nodata, T::SINGLE) };
            }
        }
    }
    out
}

/// A work value of the average kernel: `GByte` and `GUInt16` (integer sums and rounding),
/// `float` and `double`.
trait AvgSrc: Copy + Send + Sync + PartialEq {
    const INT: bool;
    const SINGLE: bool;
    fn d(self) -> f64;
    /// `static_cast<T>` of a `double`.
    fn cast(v: f64) -> Self;
}

impl AvgSrc for u8 {
    const INT: bool = true;
    const SINGLE: bool = true;
    fn d(self) -> f64 {
        self as f64
    }
    fn cast(v: f64) -> Self {
        v as u8
    }
}

impl AvgSrc for u16 {
    const INT: bool = true;
    const SINGLE: bool = true;
    fn d(self) -> f64 {
        self as f64
    }
    fn cast(v: f64) -> Self {
        v as u16
    }
}

impl AvgSrc for f32 {
    const INT: bool = false;
    const SINGLE: bool = true;
    fn d(self) -> f64 {
        self as f64
    }
    fn cast(v: f64) -> Self {
        v as f32
    }
}

impl AvgSrc for f64 {
    const INT: bool = false;
    const SINGLE: bool = false;
    fn d(self) -> f64 {
        self
    }
    fn cast(v: f64) -> Self {
        v
    }
}

/// `ComputeIntegerRMS`: the integer whose square is closest to `sum / weight`.
fn integer_rms(sum: f64, weight: f64, max: f64) -> f64 {
    let q = sum / weight;
    let mut rms = q.sqrt().min(max).trunc();
    if 2.0 * rms * (rms + 1.0) + 1.0 < 2.0 * q {
        rms += 1.0;
    }
    rms
}

/// `ComputeIntegerRMS_4values<GByte, int>`.
fn integer_rms4_u8(sum_squares: i64) -> f64 {
    let q = (sum_squares + 1) / 4;
    let mut rms = ((q as f32).sqrt() as u8) as i64;
    if rms * (rms + 1) < q {
        rms += 1;
    }
    rms as f64
}

/// `ComputeIntegerRMS_4values<GUInt16, double>`.
fn integer_rms4_u16(sum_squares: f64) -> f64 {
    let q = sum_squares * 0.25;
    let mut rms = (q.sqrt() as u16) as u64;
    if rms * (rms + 1) < (q + 0.25) as u64 {
        rms += 1;
    }
    rms as f64
}

/// The `float` arithmetic of the 2x2 RMS of `float` values.
fn rms4_f32(v: [f32; 4]) -> f32 {
    let max = v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if max == 0.0 {
        0.0
    } else if max.is_infinite() {
        (v[0] + v[1] + v[2] + v[3]).abs()
    } else {
        let inv = 1.0f32 / max;
        let sq = |x: f32| (x * inv) * (x * inv);
        max * (0.25f32 * (sq(v[0]) + sq(v[1]) + sq(v[2]) + sq(v[3]))).sqrt()
    }
}

fn rms4_f64(v: [f64; 4]) -> f64 {
    let max = v.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    if max == 0.0 {
        0.0
    } else if max.is_infinite() {
        (v[0] + v[1] + v[2] + v[3]).abs()
    } else {
        let inv = 1.0 / max;
        let sq = |x: f64| (x * inv) * (x * inv);
        max * (0.25 * (sq(v[0]) + sq(v[1]) + sq(v[2]) + sq(v[3]))).sqrt()
    }
}

/// `GDALResampleChunk_AverageOrRMS_T`: the values of the destination block, in the work type.
fn average<T: AvgSrc>(rms: bool, a: &Args, chunk: &[T]) -> Vec<f64> {
    let has_nd = a.nodata.is_some();
    let nd = a.nodata.unwrap_or(0.0);
    let nd_t = T::cast(if has_nd { nd } else { 0.0 });
    let rep_t = T::cast(if has_nd { replacement_value(a.dt, nd) } else { 0.0 });
    let xn = a.cxn as usize;
    let (right, bottom) = (a.cx0 + a.cxn, a.cy0 + a.cyn);
    let dst_w = (a.dx1 - a.dx0) as usize;
    let dst_h = (a.dy1 - a.dy0) as usize;
    // Per destination column: the source columns, the weights of the first and last, and the
    // sum of the weights.
    let mut xs: Vec<(usize, usize, f64, f64, f64)> = Vec::with_capacity(dst_w);
    let mut two = true;
    let mut last = -1i64;
    for xd in a.dx0..a.dx1 {
        let f0 = a.sdx + xd as f64 * a.rx;
        let mut s0 = (f0 + 1e-8) as i64;
        let f1 = a.sdx + (xd + 1) as f64 * a.rx;
        let mut s1 = (f1 - 1e-8).ceil() as i64;
        if s0 < a.cx0 {
            s0 = a.cx0;
        }
        if s1 == s0 {
            s1 += 1;
        }
        if s1 > right {
            s1 = right;
        }
        let lw = if s1 == s0 + 1 { 1.0 } else { 1.0 - (f0 - s0 as f64) };
        let rw = 1.0 - (s1 as f64 - f1);
        let mut full = lw;
        if s0 + 1 < s1 {
            full += (s1 - s0 - 2) as f64;
            full += rw;
        }
        if s1 - s0 != 2 || (last >= 0 && last != s0) {
            two = false;
        }
        last = s1;
        xs.push(((s0 - a.cx0) as usize, (s1 - a.cx0) as usize, lw, rw, full));
    }
    let sq = |v: f64| v * v;
    let mut out = vec![0.0f64; dst_h * dst_w];
    for yd in a.dy0..a.dy1 {
        let f0 = a.sdy + yd as f64 * a.ry;
        let mut s0 = (f0 + 1e-8) as i64;
        if s0 < a.cy0 {
            s0 = a.cy0;
        }
        let f1 = a.sdy + (yd + 1) as f64 * a.ry;
        let mut s1 = (f1 - 1e-8).ceil() as i64;
        if s1 == s0 {
            s1 += 1;
        }
        if s1 > bottom {
            s1 = bottom;
        }
        let row = &mut out[(yd - a.dy0) as usize * dst_w..][..dst_w];
        if two && s1 == s0 + 2 && a.mask.is_none() {
            // A factor of 2 with regular spacing and no nodata: GDAL's optimized cases.
            let base = (s0 - a.cy0) as usize * xn + xs[0].0;
            let px = |i: usize| -> [T; 4] {
                let p = base + 2 * i;
                [chunk[p], chunk[p + 1], chunk[p + xn], chunk[p + xn + 1]]
            };
            for (i, o) in row.iter_mut().enumerate() {
                let v = px(i);
                *o = if T::INT {
                    if rms {
                        let s = sq(v[0].d()) + sq(v[1].d()) + sq(v[2].d()) + sq(v[3].d());
                        if std::mem::size_of::<T>() == 1 { integer_rms4_u8(s as i64) } else { integer_rms4_u16(s) }
                    } else {
                        let s = v[0].d() + v[1].d() + v[2].d() + v[3].d();
                        ((s as u64 + 2) / 4) as f64
                    }
                } else if T::SINGLE {
                    let f = v.map(|x| x.d() as f32);
                    if rms {
                        rms4_f32(f) as f64
                    } else if i < dst_w / 4 * 4 {
                        // AverageFloatSSE2: four at a time.
                        ((0.25f32 * f[0] + 0.25f32 * f[2]) + (0.25f32 * f[1] + 0.25f32 * f[3])) as f64
                    } else {
                        (0.25f32 * f[0] + 0.25f32 * f[1] + 0.25f32 * f[2] + 0.25f32 * f[3]) as f64
                    }
                } else {
                    let f = v.map(|x| x.d());
                    if rms {
                        rms4_f64(f)
                    } else if i < dst_w / 2 * 2 {
                        // AverageDoubleSSE2: two at a time.
                        (0.25 * f[0] + 0.25 * f[2]) + (0.25 * f[1] + 0.25 * f[3])
                    } else {
                        0.25 * f[0] + 0.25 * f[1] + 0.25 * f[2] + 0.25 * f[3]
                    }
                };
            }
            continue;
        }
        let bottom_w = if s0 + 1 == s1 { 1.0 } else { 1.0 - (f0 - s0 as f64) };
        let top_w = 1.0 - (s1 as f64 - f1);
        let (y0, y1) = ((s0 - a.cy0) as usize, (s1 - a.cy0) as usize);
        let mut full_col = bottom_w;
        if y0 + 1 < y1 {
            full_col += (y1 - y0 - 2) as f64;
            full_col += top_w;
        }
        for (i, o) in row.iter_mut().enumerate() {
            let (l, r, lw, rw, full) = xs[i];
            let (total, total_w, mul) = match a.mask {
                None => {
                    let (mul, inv) = if T::INT {
                        (1.0, 1.0)
                    } else if rms {
                        let mut m = 0.0f64;
                        for yy in y0..y1 {
                            for v in &chunk[yy * xn + l..yy * xn + r] {
                                let x = if T::SINGLE { (v.d() as f32).abs() as f64 } else { v.d().abs() };
                                m = m.max(x);
                            }
                        }
                        (m, if m > 0.0 && m.is_finite() { 1.0 / m } else { 1.0 })
                    } else {
                        let m = ((y1 - y0) * (r - l)) as f64;
                        (m, 1.0 / m)
                    };
                    let term = |v: T| if rms { sq(v.d() * inv) } else { v.d() * inv };
                    let mut total = 0.0;
                    let mut counter = (y1 - y0) as i64 - 1;
                    let mut wy = bottom_w;
                    let mut yy = y0;
                    loop {
                        let p = &chunk[yy * xn..];
                        let mut line = term(p[l]) * lw;
                        if l + 1 < r {
                            for v in &p[l + 1..r - 1] {
                                line += term(*v);
                            }
                            line += term(p[r - 1]) * rw;
                        }
                        total += line * wy;
                        counter -= 1;
                        if counter < 0 {
                            break;
                        }
                        yy += 1;
                        wy = if counter == 0 { top_w } else { 1.0 };
                    }
                    (total, full * full_col, mul)
                }
                Some(mask) => {
                    let mut count = 0usize;
                    let (mut total, mut total_w) = (0.0, 0.0);
                    let term = |v: T| if rms { sq(v.d()) } else { v.d() };
                    for yy in y0..y1 {
                        let p = &chunk[yy * xn..];
                        let m = &mask[yy * xn..];
                        let (mut line, mut line_w) = (0.0, 0.0);
                        if m[l] != 0 {
                            count += 1;
                            line_w = lw;
                            line = term(p[l]) * lw;
                        }
                        if l + 1 < r {
                            for ix in l + 1..r - 1 {
                                if m[ix] != 0 {
                                    count += 1;
                                    line_w += 1.0;
                                    line += term(p[ix]);
                                }
                            }
                            if m[r - 1] != 0 {
                                count += 1;
                                line_w += rw;
                                line += term(p[r - 1]) * rw;
                            }
                        }
                        let wy = if yy == y0 {
                            bottom_w
                        } else if yy + 1 == y1 {
                            top_w
                        } else {
                            1.0
                        };
                        total += line * wy;
                        total_w += line_w * wy;
                    }
                    if count == 0 {
                        *o = nd_t.d();
                        continue;
                    }
                    (total, total_w, 1.0)
                }
            };
            let v = if T::INT {
                if rms {
                    T::cast(integer_rms(total, total_w, if std::mem::size_of::<T>() == 1 { 255.0 } else { 65535.0 }))
                } else {
                    T::cast(total / total_w + 0.5)
                }
            } else if rms {
                T::cast(mul * (total / total_w).sqrt())
            } else {
                T::cast(mul * (total / total_w))
            };
            *o = if has_nd && v == nd_t { rep_t.d() } else { v.d() };
        }
    }
    out
}

/// `GDALResampleChunk_ModeT`.
fn mode<T: Sample>(a: &Args, chunk: &[T]) -> Vec<T> {
    let has_nd = a.nodata.is_some();
    let nd_t = T::mode_nodata(a.nodata);
    let xn = a.cxn as usize;
    let (right, bottom) = (a.cx0 + a.cxn, a.cy0 + a.cyn);
    let window = |d: i64, ratio: f64, delta: f64, c0: i64, end: i64| {
        let mut s0 = (delta + d as f64 * ratio + 1e-8) as i64;
        if s0 < c0 {
            s0 = c0;
        }
        let mut s1 = (delta + (d + 1) as f64 * ratio - 1e-8).ceil() as i64;
        if s1 == s0 {
            s1 += 1;
        }
        if s1 > end {
            s1 = end;
        }
        ((s0 - c0) as usize, (s1 - c0) as usize)
    };
    let byte = T::DT == DataType::U8;
    let mut out = Vec::with_capacity(((a.dx1 - a.dx0) * (a.dy1 - a.dy0)) as usize);
    let mut vals: Vec<T> = Vec::new();
    let mut counts: Vec<u32> = Vec::new();
    let mut hist = [0u32; 256];
    for yd in a.dy0..a.dy1 {
        let (y0, y1) = window(yd, a.ry, a.sdy, a.cy0, bottom);
        for xd in a.dx0..a.dx1 {
            let (x0, x1) = window(xd, a.rx, a.sdx, a.cx0, right);
            if byte {
                hist.fill(0);
                let (mut best, mut best_count) = (None, 0u32);
                for yy in y0..y1 {
                    for v in &chunk[yy * xn + x0..yy * xn + x1] {
                        if !has_nd || *v != nd_t {
                            let k = v.to_f64() as usize;
                            hist[k] += 1;
                            if hist[k] > best_count {
                                best = Some(*v);
                                best_count = hist[k];
                            }
                        }
                    }
                }
                out.push(best.unwrap_or(nd_t));
            } else {
                vals.clear();
                counts.clear();
                let mut i_max = 0usize;
                for yy in y0..y1 {
                    for x in x0..x1 {
                        if a.mask.is_some_and(|m| m[yy * xn + x] == 0) {
                            continue;
                        }
                        let v = chunk[yy * xn + x];
                        match vals.iter().position(|w| w.same(v)) {
                            Some(i) => {
                                counts[i] += 1;
                                if counts[i] > counts[i_max] {
                                    i_max = i;
                                }
                            }
                            None => {
                                if vals.is_empty() {
                                    i_max = 0;
                                }
                                vals.push(v);
                                counts.push(1);
                            }
                        }
                    }
                }
                out.push(if vals.is_empty() { nd_t } else { vals[i_max] });
            }
        }
    }
    out
}

/// `GDALResampleChunk_NearT`.
/// `GDALResampleChunk_Gauss` without a color table: a 3x3, 5x5 or 7x7 binomial kernel (by the
/// vertical ratio, for both axes) centred on the source pixels a destination pixel covers, its
/// integer weights summed over the pixels the mask leaves valid; the nodata value (0 without
/// one) where none is. The window's fraction (`dfSrcXDelta`) plays no part.
fn gauss(a: &Args, chunk: &[f64]) -> Vec<f64> {
    const M3: [i64; 9] = [1, 2, 1, 2, 4, 2, 1, 2, 1];
    const M5: [i64; 25] = [1, 4, 6, 4, 1, 4, 16, 24, 16, 4, 6, 24, 36, 24, 6, 4, 16, 24, 16, 4, 1, 4, 6, 4, 1];
    const M7: [i64; 49] = [
        1, 6, 15, 20, 15, 6, 1, 6, 36, 90, 120, 90, 36, 6, 15, 90, 225, 300, 225, 90, 15, 20, 120, 300, 400, 300, 120, 20, 15, 90, 225,
        300, 225, 90, 15, 6, 36, 90, 120, 90, 36, 6, 1, 6, 15, 20, 15, 6, 1,
    ];
    let factor = (0.5 + a.ry) as i64;
    let (dim, matrix): (i64, &[i64]) = if factor <= 2 {
        (3, &M3)
    } else if factor <= 4 {
        (5, &M5)
    } else {
        (7, &M7)
    };
    let nodata = a.nodata.unwrap_or(0.0);
    // The source span of destination pixel `d` along an axis, and the kernel's shift into it.
    let span = |d: i64, ratio: f64, c0: i64, c1: i64, last: bool| {
        let mut s0 = (0.5 + d as f64 * ratio) as i64;
        let mut s1 = (0.5 + (d + 1) as f64 * ratio) as i64 + 1;
        if s0 < c0 {
            s0 = c0;
            s1 += 1;
        }
        let size = s1 - s0;
        s0 = s0 + size / 2 - dim / 2;
        s1 = s0 + dim;
        if s1 > c1 || (ratio > 1.0 && last) {
            s1 = c1.min(s0 + dim);
        }
        let mut shift = 0;
        if s0 < c0 {
            shift = c0 - s0;
            s0 = c0;
        }
        (s0, s1, shift)
    };
    let mut out = Vec::with_capacity(((a.dx1 - a.dx0) * (a.dy1 - a.dy0)) as usize);
    for dy in a.dy0..a.dy1 {
        let (y0, y1, sy) = span(dy, a.ry, a.cy0, a.cy0 + a.cyn, dy == a.ovr.1 - 1);
        for dx in a.dx0..a.dx1 {
            let (x0, x1, sx) = span(dx, a.rx, a.cx0, a.cx0 + a.cxn, dx == a.ovr.0 - 1);
            let mut total = 0.0f64;
            let mut count: i64 = 0;
            for (j, y) in (y0..y1).enumerate() {
                let weights = (sy + j as i64) * dim + sx;
                for (i, x) in (x0..x1).enumerate() {
                    let k = ((y - a.cy0) * a.cxn + (x - a.cx0)) as usize;
                    if a.mask.is_none_or(|m| m[k] != 0) {
                        let w = matrix[(weights + i as i64) as usize];
                        total += chunk[k] * w as f64;
                        count += w;
                    }
                }
            }
            out.push(if count == 0 { nodata } else { total / count as f64 });
        }
    }
    out
}

fn near<T: Copy>(a: &Args, chunk: &[T]) -> Vec<T> {
    let xn = a.cxn as usize;
    let xs: Vec<usize> = (a.dx0..a.dx1).map(|x| ((0.5 + x as f64 * a.rx) as i64).max(a.cx0) as usize - a.cx0 as usize).collect();
    let mut out = Vec::with_capacity(xs.len() * (a.dy1 - a.dy0) as usize);
    for y in a.dy0..a.dy1 {
        let sy = ((0.5 + y as f64 * a.ry) as i64).max(a.cy0) as usize - a.cy0 as usize;
        let row = &chunk[sy * xn..];
        out.extend(xs.iter().map(|x| row[*x]));
    }
    out
}

/// The source window in the work type: the window itself when it is of that type. A `&Vec`, not a
/// slice: `Any` downcasts only sized types.
#[allow(clippy::ptr_arg)]
fn as_work<T: Sample, W: Copy + 'static>(win: &Vec<T>, f: impl Fn(&T) -> W) -> std::borrow::Cow<'_, [W]> {
    match (win as &dyn std::any::Any).downcast_ref::<Vec<W>>() {
        Some(v) => std::borrow::Cow::Borrowed(v.as_slice()),
        None => std::borrow::Cow::Owned(win.iter().map(f).collect()),
    }
}

/// Resamples one destination block of one band from its source window (`win`, in the source
/// type), as the job of `GDALRegenerateOverviewsMultiBand` does.
fn resample_block<T: Sample>(method: Method, a: &Args, win: &Vec<T>) -> Vec<T> {
    fn run<W: ConvSrc + AvgSrc>(m: Method, a: &Args, w: &[W]) -> Vec<f64> {
        match m {
            Method::Average => average(false, a, w),
            Method::Rms => average(true, a, w),
            _ => convolution(m, a, w),
        }
    }
    let Some(wt) = work(method, T::DT) else {
        return if method == Method::Mode { mode(a, win) } else { near(a, win) };
    };
    if method == Method::Gauss {
        // A `double` buffer, converted to the overview's type by `GDALCopyWords`.
        return gauss(a, &as_work(win, |v| v.to_f64())).into_iter().map(|v| T::from_work(v, false)).collect();
    }
    let vals = match wt {
        Work::U8 => run(method, a, &as_work(win, |v| v.to_f64() as u8)),
        Work::U16 => run(method, a, &as_work(win, |v| v.to_f64() as u16)),
        Work::F32 => run(method, a, &as_work(win, |v| v.to_f64() as f32)),
        Work::F64 => run(method, a, &as_work(win, |v| v.to_f64())),
    };
    let single = wt != Work::F64;
    vals.into_iter().map(|v| T::from_work(v, single)).collect()
}

/// What `GDALRegenerateOverviewsMultiBand` weights each band of a level by when it resamples
/// with a mask (`pabyChunkNodataMask`): a band that is a mask band (`IsMaskBand`: an alpha band)
/// by its own values, the others by their mask band (`GetMaskBand`) — the per-dataset mask
/// first, then the nodata mask, then the alpha band (of two- or four-band rasters), else 255.
/// It does so for every method but nearest neighbour, when the first band is a mask band or
/// its mask is not all valid.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Weights<'a> {
    /// The alpha band (0-based).
    pub(crate) alpha: Option<usize>,
    /// Whether the alpha band is the other bands' mask band (two or four bands, the alpha band
    /// Byte or UInt16, `GDALRasterBand::GetMaskBand`).
    pub(crate) alpha_masks: bool,
    /// The per-dataset mask over the source rows given, as GDAL reads it (0 for invalid).
    pub(crate) mask: Option<&'a [u8]>,
}

/// The weights of a band, by kind.
enum BandWeight {
    Own,
    Dataset,
    Nodata,
    Alpha(usize),
    AllValid,
}

impl Weights<'_> {
    /// `bUseNoDataMask`, and each band's weights.
    fn kinds<T: Sample>(&self, method: Method, bands: usize, nodata: Option<f64>) -> Option<Vec<BandWeight>> {
        let has_nodata = nodata.is_some_and(T::nodata_in_range);
        let alpha_mask = self.alpha.filter(|_| self.alpha_masks);
        let first_masked = self.mask.is_some() || has_nodata || alpha_mask.is_some_and(|a| a != 0);
        if method == Method::Nearest || !(self.alpha == Some(0) || first_masked) {
            return None;
        }
        Some(
            (0..bands)
                .map(|b| {
                    if self.alpha == Some(b) {
                        BandWeight::Own
                    } else if self.mask.is_some() {
                        BandWeight::Dataset
                    } else if has_nodata {
                        BandWeight::Nodata
                    } else if let Some(a) = alpha_mask {
                        BandWeight::Alpha(a)
                    } else {
                        BandWeight::AllValid
                    }
                })
                .collect(),
        )
    }
}

/// A value read as GDAL reads a band into a Byte buffer (`GDALCopyWords`) to use it as a mask.
fn mask_value<T: Sample>(v: T) -> u8 {
    u8::from_work(v.to_f64(), false)
}

/// `GDALRescaledAlphaBand`: a UInt16 alpha band as the other bands' mask, its values above 0
/// at least 1.
fn rescaled_alpha<T: Sample>(v: T) -> u8 {
    if T::DT == DataType::U16 {
        let x = v.to_f64() as u16;
        if x > 0 && x < 257 { 1 } else { (x / 257) as u8 }
    } else {
        mask_value(v)
    }
}

/// Computes one row of destination blocks of `plan.dst`, from `y0`: its rows, band after band.
/// `src` holds `rows` rows of every band of the level before it (band after band, all its
/// columns) from row `row0`, which must include [`Plan::rows`]; `weights.mask`, when given, the
/// same rows.
#[allow(clippy::too_many_arguments)]
pub(crate) fn block_row(
    plan: &Plan,
    src: &RasterData,
    row0: usize,
    rows: usize,
    bands: usize,
    nodata: Option<f64>,
    weights: Weights,
    y0: usize,
) -> RasterData {
    crate::pixel::map_data!(src, v => block_row_t(plan, v, row0, rows, bands, nodata, weights, y0))
}

#[allow(clippy::too_many_arguments)]
fn block_row_t<T: Sample>(
    plan: &Plan,
    src: &[T],
    row0: usize,
    rows: usize,
    bands: usize,
    nodata: Option<f64>,
    weights: Weights,
    y0: usize,
) -> Vec<T> {
    let ys = plan.y_span(y0);
    let (sw, dw) = (plan.src.0, plan.dst.0);
    let xs: Vec<Span> =
        (0..dw).step_by(plan.chunk.0).map(|x0| plan.span(x0, plan.chunk.0.min(dw - x0), plan.rx, plan.src.0, plan.dst.0)).collect();
    let kinds = weights.kinds::<T>(plan.method, bands, nodata);
    let parts = crate::par::map_range(bands * xs.len(), |j| {
        let (b, xsp) = (j / xs.len(), xs[j % xs.len()]);
        let band_rows = |b: usize| &src[b * sw * rows..(b + 1) * sw * rows];
        let band = band_rows(b);
        let (qx0, qxn) = (xsp.q0 as usize, xsp.qn as usize);
        let ranges: Vec<std::ops::Range<usize>> = (ys.q0 as usize..(ys.q0 + ys.qn) as usize)
            .map(|r| {
                let s = (r - row0) * sw + qx0;
                s..s + qxn
            })
            .collect();
        let mut win: Vec<T> = Vec::with_capacity(qxn * ys.qn as usize);
        for r in &ranges {
            win.extend_from_slice(&band[r.clone()]);
        }
        let mask: Option<Vec<u8>> = kinds.as_ref().map(|k| match k[b] {
            BandWeight::Own => win.iter().map(|v| mask_value(*v)).collect(),
            BandWeight::Dataset => {
                let m = weights.mask.unwrap_or(&[]);
                ranges.iter().flat_map(|r| m[r.clone()].iter().copied()).collect()
            }
            BandWeight::Nodata => {
                let nd = nodata.unwrap_or(0.0);
                win.iter().map(|v| if v.is_nodata(nd) { 0 } else { 255 }).collect()
            }
            BandWeight::Alpha(a) => {
                let alpha = band_rows(a);
                ranges.iter().flat_map(|r| alpha[r.clone()].iter().map(|v| rescaled_alpha(*v))).collect()
            }
            BandWeight::AllValid => vec![255; win.len()],
        });
        let args = Args {
            rx: plan.rx,
            ry: plan.ry,
            sdx: 0.0,
            sdy: 0.0,
            cx0: xsp.q0,
            cxn: xsp.qn,
            cy0: ys.q0,
            cyn: ys.qn,
            dx0: xsp.dst0 as i64,
            dx1: (xsp.dst0 + xsp.dst_n) as i64,
            dy0: ys.dst0 as i64,
            dy1: (ys.dst0 + ys.dst_n) as i64,
            mask: mask.as_deref(),
            nodata,
            dt: T::DT,
            ovr: (plan.dst.0 as i64, plan.dst.1 as i64),
        };
        resample_block(plan.method, &args, &win)
    });
    let n = ys.dst_n;
    let mut out = vec![T::default(); bands * n * dw];
    for (j, part) in parts.into_iter().enumerate() {
        let (b, xsp) = (j / xs.len(), xs[j % xs.len()]);
        for r in 0..n {
            out[b * n * dw + r * dw + xsp.dst0..][..xsp.dst_n].copy_from_slice(&part[r * xsp.dst_n..][..xsp.dst_n]);
        }
    }
    out
}

/// Computes a whole level from the whole level before it (band after band); `weights.mask`, when
/// given, covers that whole level.
pub(crate) fn level(plan: &Plan, src: &RasterData, bands: usize, nodata: Option<f64>, weights: Weights) -> RasterData {
    crate::pixel::map_data!(src, v => level_t(plan, v, bands, nodata, weights))
}

/// The next level of a per-dataset mask (0 for invalid pixels) of size `from`, as GDAL
/// regenerates a mask band's overviews: weighted by its own values (`IsMaskBand`).
pub(crate) fn mask_level(method: Method, src: &[u8], from: (usize, usize), to: (usize, usize), block: usize) -> Vec<u8> {
    let plan = Plan::new(method, from, to, 1, block, DataType::U8);
    let own = Weights { alpha: Some(0), ..Weights::default() };
    level_t(&plan, src, 1, None, own)
}

fn level_t<T: Sample>(plan: &Plan, src: &[T], bands: usize, nodata: Option<f64>, weights: Weights) -> Vec<T> {
    let (dw, total) = plan.dst;
    let starts: Vec<usize> = (0..total).step_by(plan.chunk.1).collect();
    let rows = crate::par::map_range(starts.len(), |i| block_row_t(plan, src, 0, plan.src.1, bands, nodata, weights, starts[i]));
    let mut out = vec![T::default(); bands * dw * total];
    for (y0, r) in starts.iter().zip(&rows) {
        let n = plan.chunk.1.min(total - y0);
        for b in 0..bands {
            out[b * dw * total + y0 * dw..][..n * dw].copy_from_slice(&r[b * n * dw..][..n * dw]);
        }
    }
    out
}

/// A read of a floating-point window into a buffer of another size, resampled as GDAL's
/// `GDALRasterBand::RasterIOResampled` (or, `dataset`, `GDALDataset::RasterIOResampled`) does it
/// (gcore/rasterio.cpp): the buffer in blocks of at most about a megapixel of source, each block
/// resampled by the kernels above from the source window around it, with the window's fraction
/// as the kernels' source offset, unless the buffer has a whole-pixel offset in the "virtual"
/// buffer of the whole level, which keeps sub-window reads equal to whole-level ones.
#[derive(Debug, Clone)]
pub(crate) struct ResampledRead {
    pub(crate) method: Method,
    /// The size of the level read (`nRasterXSize`, `nRasterYSize`).
    pub(crate) raster: (usize, usize),
    /// The integer window (`nXOff`, `nYOff`, `nXSize`, `nYSize`).
    pub(crate) window: (i64, i64, i64, i64),
    /// The floating-point window (`dfXOff`, `dfYOff`, `dfXSize`, `dfYSize`).
    pub(crate) fwindow: (f64, f64, f64, f64),
    /// The buffer's width and height.
    pub(crate) buf: (usize, usize),
    /// The band's block width (`nBlockXSize`), which steers the blocks.
    pub(crate) block_width: usize,
    /// Through `GDALDataset::RasterIOResampled`: one overview factor for both axes, no nodata
    /// replacement, and fully masked blocks filled with 0.
    pub(crate) dataset: bool,
}

/// One block of a resampled read: its buffer pixels and the source window read for it.
#[derive(Debug, Clone, Copy)]
struct ReadBlock {
    dx0: usize,
    dxn: usize,
    dy0: usize,
    dyn_: usize,
    qx0: i64,
    qxn: i64,
    qy0: i64,
    qyn: i64,
}

struct ReadGeometry {
    rx: f64,
    ry: f64,
    sdx: f64,
    sdy: f64,
    /// The buffer's offset in the virtual buffer, when whole (`nDestXOffVirtual`).
    vx: Option<i64>,
    vy: Option<i64>,
    blocks: Vec<ReadBlock>,
}

impl ResampledRead {
    fn geometry(&self) -> ReadGeometry {
        let (nx, ny, nxs, _) = self.window;
        let (mut fx, mut fy, fxs, fys) = self.fwindow;
        let (bw, bh) = (self.buf.0 as i64, self.buf.1 as i64);
        let (w, h) = (self.raster.0 as i64, self.raster.1 as i64);
        let rx = fxs / bw as f64;
        let ry = fys / bh as f64;
        let virtual_off = |off: f64, ratio: f64| {
            let d = off / ratio;
            ((d - ((d + 0.5) as i64) as f64).abs() < 1e-8).then_some((d + 0.5) as i64)
        };
        let vx = virtual_off(fx, rx);
        if vx.is_some() {
            fx = nx as f64;
        }
        let vy = virtual_off(fy, ry);
        if vy.is_some() {
            fy = ny as f64;
        }
        // Blocks of the buffer reading at most about a megapixel of the source.
        let (mut bx, mut by) = (bw, bh);
        loop {
            let cx = (3 + (bx as f64 * rx) as i64).min(w);
            let cy = (3 + (by as f64 * ry) as i64).min(h);
            if (bx == 1 && by == 1) || cx * cy <= 1024 * 1024 {
                break;
            }
            if cx >= nxs && nxs == self.block_width as i64 && by > 1 {
                by /= 2;
            } else if bx > 1 && (cx > cy || by == 1) {
                bx /= 2;
            } else {
                by /= 2;
            }
        }
        let factor = |r: f64| ((0.5 + r) as i64).max(1);
        let (fxo, fyo) = if self.dataset {
            let f = (((0.5 + rx) as i64).max((0.5 + ry) as i64)).max(1);
            (f, f)
        } else {
            (factor(rx), factor(ry))
        };
        let radius = self.method.radius();
        let span = |d0: i64, n: i64, ratio: f64, off: i64, size: i64, f: i64| {
            let c0 = off + (d0 as f64 * ratio) as i64;
            let c1 = (off + 1 + ((d0 + n) as f64 * ratio).ceil() as i64).min(size);
            let mut q0 = c0 - radius * f;
            let mut qn = c1 - c0 + 2 * radius * f;
            if q0 < 0 {
                qn += q0;
                q0 = 0;
            }
            if qn + q0 > size {
                qn = size - q0;
            }
            (q0, qn)
        };
        let mut blocks = Vec::new();
        let mut dy0 = 0;
        while dy0 < bh {
            let dyn_ = by.min(bh - dy0);
            let (qy0, qyn) = span(dy0, dyn_, ry, ny, h, fyo);
            let mut dx0 = 0;
            while dx0 < bw {
                let dxn = bx.min(bw - dx0);
                let (qx0, qxn) = span(dx0, dxn, rx, nx, w, fxo);
                blocks.push(ReadBlock { dx0: dx0 as usize, dxn: dxn as usize, dy0: dy0 as usize, dyn_: dyn_ as usize, qx0, qxn, qy0, qyn });
                dx0 += bx;
            }
            dy0 += by;
        }
        ReadGeometry { rx, ry, sdx: fx - nx as f64, sdy: fy - ny as f64, vx, vy, blocks }
    }

    /// The source region the read needs, `(x, y, width, height)` in the level's pixels.
    pub(crate) fn region(&self) -> (usize, usize, usize, usize) {
        let g = self.geometry();
        let x0 = g.blocks.iter().map(|b| b.qx0).min().unwrap_or(0).max(0);
        let y0 = g.blocks.iter().map(|b| b.qy0).min().unwrap_or(0).max(0);
        let x1 = g.blocks.iter().map(|b| b.qx0 + b.qxn).max().unwrap_or(0).max(x0);
        let y1 = g.blocks.iter().map(|b| b.qy0 + b.qyn).max().unwrap_or(0).max(y0);
        (x0 as usize, y0 as usize, (x1 - x0) as usize, (y1 - y0) as usize)
    }

    /// Resamples `bands` bands of the source `region` (see [`ResampledRead::region`]; band after
    /// band) into the buffer, band after band. A band's mask (0 for invalid pixels, or alpha
    /// values; `None` when all its pixels are valid) has the region's size.
    pub(crate) fn read(
        &self,
        src: &RasterData,
        region: (usize, usize, usize, usize),
        bands: usize,
        masks: &[Option<Vec<u8>>],
        nodata: Option<f64>,
    ) -> RasterData {
        crate::pixel::map_data!(src, v => self.read_t(v, region, bands, masks, nodata))
    }

    fn read_t<T: Sample>(
        &self,
        src: &[T],
        region: (usize, usize, usize, usize),
        bands: usize,
        masks: &[Option<Vec<u8>>],
        nodata: Option<f64>,
    ) -> Vec<T> {
        let g = self.geometry();
        let (bw, bh) = self.buf;
        let (rx0, ry0, rw, rh) = region;
        let nodata = if self.dataset { None } else { nodata };
        // A fully masked block: the nodata value (0 without), as `GDALCopyWords` converts it.
        let fill = if self.dataset { T::default() } else { T::from_work(nodata.unwrap_or(0.0), false) };
        let jobs: Vec<(usize, ReadBlock)> = (0..bands).flat_map(|b| g.blocks.iter().map(move |k| (b, *k))).collect();
        let parts = crate::par::map_range(jobs.len(), |j| {
            let (b, k) = jobs[j];
            let band = &src[b * rw * rh..(b + 1) * rw * rh];
            let (qx0, qxn, qy0, qyn) = (k.qx0 as usize - rx0, k.qxn as usize, k.qy0 as usize - ry0, k.qyn as usize);
            let mut win: Vec<T> = Vec::with_capacity(qxn * qyn);
            for r in qy0..qy0 + qyn {
                win.extend_from_slice(&band[r * rw + qx0..r * rw + qx0 + qxn]);
            }
            let mut mask: Option<Vec<u8>> = masks.get(b).and_then(|m| m.as_ref()).map(|m| {
                let mut out = Vec::with_capacity(qxn * qyn);
                for r in qy0..qy0 + qyn {
                    out.extend_from_slice(&m[r * rw + qx0..r * rw + qx0 + qxn]);
                }
                out
            });
            // GDAL skips fully masked blocks, and resamples fully valid ones without the mask.
            if let Some(m) = &mask
                && let Some(first) = m.first()
                && m.iter().all(|v| v == first)
            {
                if *first == 0 {
                    return vec![fill; k.dxn * k.dyn_];
                }
                mask = None;
            }
            let args = Args {
                rx: g.rx,
                ry: g.ry,
                sdx: g.sdx,
                sdy: g.sdy,
                cx0: k.qx0 - if g.vx.is_some() { 0 } else { self.window.0 },
                cxn: k.qxn,
                cy0: k.qy0 - if g.vy.is_some() { 0 } else { self.window.1 },
                cyn: k.qyn,
                dx0: k.dx0 as i64 + g.vx.unwrap_or(0),
                dx1: (k.dx0 + k.dxn) as i64 + g.vx.unwrap_or(0),
                dy0: k.dy0 as i64 + g.vy.unwrap_or(0),
                dy1: (k.dy0 + k.dyn_) as i64 + g.vy.unwrap_or(0),
                mask: mask.as_deref(),
                nodata,
                dt: T::DT,
                ovr: (g.vx.unwrap_or(0) + bw as i64, g.vy.unwrap_or(0) + bh as i64),
            };
            resample_block(self.method, &args, &win)
        });
        let mut out = vec![T::default(); bands * bw * bh];
        for ((b, k), part) in jobs.iter().zip(parts) {
            for r in 0..k.dyn_ {
                out[b * bw * bh + (k.dy0 + r) * bw + k.dx0..][..k.dxn].copy_from_slice(&part[r * k.dxn..][..k.dxn]);
            }
        }
        out
    }
}

/// The mask GDAL's `GDALNoDataMaskBand` gives values: 0 where they are the nodata value, else
/// 255. `None` when the nodata value is not one of the type's (the band is then all valid).
pub(crate) fn nodata_mask(values: &RasterData, nodata: f64) -> Option<Vec<u8>> {
    fn mask<T: Sample>(v: &[T], nd: f64) -> Option<Vec<u8>> {
        T::nodata_in_range(nd).then(|| v.iter().map(|x| if x.is_nodata(nd) { 0 } else { 255 }).collect())
    }
    crate::pixel::with_data!(values, v => mask(v, nodata))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The levels of GDAL's COG driver halve rounding down: 875 gives 437, not 438. (The pixels
    /// are compared with GDAL's in python/tests/test_cog_overviews.py.)
    #[test]
    fn cog_sizes_are_gdals() {
        let widths: Vec<usize> = cog_sizes(14000, 14000, 512, None).iter().map(|s| s.0).collect();
        assert_eq!(widths, [7000, 3500, 1750, 875, 437]);
        assert_eq!(cog_sizes(1001, 3, 512, None), [(500, 1)]);
        assert_eq!(cog_sizes(600, 600, 512, Some(3)), [(300, 300), (150, 150), (75, 75)]);
        assert_eq!(cog_sizes(4, 2, 512, Some(5)), [(2, 1), (1, 1)]);
        assert!(cog_sizes(512, 512, 512, None).is_empty());
        assert!(cog_sizes(5000, 10, 512, Some(0)).is_empty());
    }

    /// `GDALNoDataMaskBand` compares Float32 and Float64 values with the nodata value as GDAL
    /// 3.12's `ARE_REAL_EQUAL` does: closer than `FLT_EPSILON * |v + nodata| * 2`, FLT_EPSILON
    /// for doubles too (about 4.77e-3 about 9999), in the band's precision.
    #[test]
    fn nodata_masks_take_gdals_tolerance() {
        let f64s = RasterData::F64(vec![9999.0, 9999.000000000002, 9999.004, 9998.996, 9999.005, 9998.995, f64::NAN, 1.0]);
        assert_eq!(nodata_mask(&f64s, 9999.0), Some(vec![0, 0, 0, 0, 255, 255, 255, 255]));
        // In single precision: 9999.004 and 9999.005 are 9999.00390625 and 9999.0048828125.
        let f32s = RasterData::F32(vec![9999.0, 9999.001, 9999.004, 9999.005, 9998.995, f32::NAN]);
        assert_eq!(nodata_mask(&f32s, 9999.0), Some(vec![0, 0, 0, 255, 255, 255]));
        assert_eq!(nodata_mask(&RasterData::F64(vec![f64::NAN, 0.0]), f64::NAN), Some(vec![0, 255]));
        // About 0, only 0 is the nodata value.
        assert_eq!(nodata_mask(&RasterData::F64(vec![0.0, 1e-300, -0.0]), 0.0), Some(vec![0, 255, 0]));
    }
}
