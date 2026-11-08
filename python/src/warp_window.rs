//! The source window of a `terrio.rio.WarpedVRT` region that `RegionWarp._warp_from` reads and
//! warps without the window leaving Rust: its alpha validity and its conversion to the working
//! type, as `WarpedVRT._source_window` takes them (`gdal_cast` in `terrio/rio/_resample.py`).

use terrio::DataType;
use wbraster::raster::RasterData;

/// An integer type a window is converted to.
trait Int: Copy {
    /// The type's smallest value.
    const MIN: i128;
    /// Its largest.
    const MAX: i128;
    /// `v`, between [`Int::MIN`] and [`Int::MAX`], in this type.
    fn from_i128(v: i128) -> Self;
}

macro_rules! int {
    ($($t:ty),*) => {$(
        impl Int for $t {
            const MIN: i128 = <$t>::MIN as i128;
            const MAX: i128 = <$t>::MAX as i128;
            fn from_i128(v: i128) -> Self {
                v as $t
            }
        }
    )*};
}
int!(u8, i8, u16, i16, u32, i32, u64, i64);

/// A sample of a source window, converted as `gdal_cast` converts it.
trait Sample: Copy {
    /// `astype(float32)`: rounded to the nearest `f32`.
    fn as_f32(self) -> f32;
    /// `astype(float64)`.
    fn as_f64(self) -> f64;
    /// Into an integer type: an integer saturated; a float rounded half away from zero in its own
    /// precision (`x + 0.5` or `x - 0.5`), clipped to the type's limits, NaN made 0, then
    /// truncated. Only for the limits the float type holds exactly (see [`converts`]).
    fn to_int<T: Int>(self) -> T;
}

macro_rules! int_sample {
    ($($t:ty),*) => {$(
        impl Sample for $t {
            fn as_f32(self) -> f32 {
                self as f32
            }
            fn as_f64(self) -> f64 {
                self as f64
            }
            fn to_int<T: Int>(self) -> T {
                T::from_i128((self as i128).clamp(T::MIN, T::MAX))
            }
        }
    )*};
}
int_sample!(u8, i8, u16, i16, u32, i32, u64, i64);

macro_rules! float_sample {
    ($($t:ty),*) => {$(
        impl Sample for $t {
            fn as_f32(self) -> f32 {
                self as f32
            }
            fn as_f64(self) -> f64 {
                self as f64
            }
            fn to_int<T: Int>(self) -> T {
                // `np.where(x >= 0, x + half, x - half)` in the float's precision, `np.clip`, which
                // keeps NaN, then NaN as 0 and `np.trunc`.
                let y = if self >= 0.0 { self + 0.5 } else { self - 0.5 };
                let y = if y.is_nan() { 0.0 } else { y.max(T::MIN as $t).min(T::MAX as $t) };
                T::from_i128(y.trunc() as i128)
            }
        }
    )*};
}
float_sample!(f32, f64);

/// Whether [`convert`] converts a window of type `from` to `to`. Not a float into an integer
/// type whose limits the float type does not hold exactly (`float32` into 32- and 64-bit integers,
/// `float64` into 64-bit ones): there `gdal_cast` clips to a rounded limit beyond the type's, and
/// NumPy's conversion of such a value depends on the platform.
pub(crate) fn converts(from: DataType, to: DataType) -> bool {
    use DataType::{F32, F64, I32, I64, U32, U64};
    !matches!((from, to), (F32, U32 | I32 | U64 | I64) | (F64, U64 | I64))
}

/// The window converted to the working type `to` as `gdal_cast(data, to)` converts it, or None
/// when [`converts`] does not.
pub(crate) fn convert(data: &RasterData, to: DataType) -> Option<RasterData> {
    if !converts(data.data_type(), to) {
        return None;
    }
    Some(match data {
        RasterData::U8(v) => cast(v, to),
        RasterData::I8(v) => cast(v, to),
        RasterData::U16(v) => cast(v, to),
        RasterData::I16(v) => cast(v, to),
        RasterData::U32(v) => cast(v, to),
        RasterData::I32(v) => cast(v, to),
        RasterData::U64(v) => cast(v, to),
        RasterData::I64(v) => cast(v, to),
        RasterData::F32(v) => cast(v, to),
        RasterData::F64(v) => cast(v, to),
    })
}

/// The samples `v` converted to `to`.
fn cast<S: Sample>(v: &[S], to: DataType) -> RasterData {
    fn ints<S: Sample, T: Int>(v: &[S]) -> Vec<T> {
        v.iter().map(|x| x.to_int::<T>()).collect()
    }
    match to {
        DataType::U8 => RasterData::U8(ints(v)),
        DataType::I8 => RasterData::I8(ints(v)),
        DataType::U16 => RasterData::U16(ints(v)),
        DataType::I16 => RasterData::I16(ints(v)),
        DataType::U32 => RasterData::U32(ints(v)),
        DataType::I32 => RasterData::I32(ints(v)),
        DataType::U64 => RasterData::U64(ints(v)),
        DataType::I64 => RasterData::I64(ints(v)),
        DataType::F32 => RasterData::F32(v.iter().map(|x| x.as_f32()).collect()),
        DataType::F64 => RasterData::F64(v.iter().map(|x| x.as_f64()).collect()),
    }
}

/// Which of the `n` pixels of band `band` (from 0) are not 0: the validity `_source_window` takes
/// from an alpha band (`data[alpha - 1] != 0`), before any conversion.
pub(crate) fn nonzero(data: &RasterData, band: usize, n: usize) -> Vec<bool> {
    fn of<S: Sample>(v: &[S], band: usize, n: usize) -> Vec<bool> {
        v[band * n..(band + 1) * n].iter().map(|x| x.as_f64() != 0.0).collect()
    }
    match data {
        RasterData::U8(v) => of(v, band, n),
        RasterData::I8(v) => of(v, band, n),
        RasterData::U16(v) => of(v, band, n),
        RasterData::I16(v) => of(v, band, n),
        RasterData::U32(v) => of(v, band, n),
        RasterData::I32(v) => of(v, band, n),
        RasterData::U64(v) => of(v, band, n),
        RasterData::I64(v) => of(v, band, n),
        RasterData::F32(v) => of(v, band, n),
        RasterData::F64(v) => of(v, band, n),
    }
}
