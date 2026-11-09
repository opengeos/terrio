//! A region's source window made ready for [`RegionWarp::warp`](super::RegionWarp::warp): the
//! pixels an alpha band leaves out, and the window converted to the warp's working type.
//!
//! A window is converted to a working type value by value:
//!
//! - an integer into an integer type is saturated at the type's limits;
//! - a float into an integer type is rounded half away from zero in its own precision
//!   (`x + 0.5` or `x - 0.5`), clipped to the type's limits, made 0 if NaN, then truncated;
//! - any value into `float32` is rounded to the nearest `f32`, and into `float64` to the nearest
//!   `f64`.
//!
//! A float type is not converted into an integer type whose limits it does not hold exactly
//! (`float32` into 32- and 64-bit integers, `float64` into 64-bit ones): a limit rounded to the
//! float type lies outside the integer type's range, so the clipped value would not fit it.
//! [`converts`] tells which pairs [`convert`] takes.

use crate::{Error, Result};
use wbraster::DataType;
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

/// A sample of a source window.
trait Sample: Copy {
    /// Rounded to the nearest `f32`.
    fn as_f32(self) -> f32;
    /// Rounded to the nearest `f64`.
    fn as_f64(self) -> f64;
    /// Into an integer type, as the module describes. For a float, only into a type whose
    /// limits it holds exactly (see [`converts`]).
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
                // Half away from zero in the float's own precision, then clipped (NaN kept by
                // the comparisons, then made 0) and truncated.
                let y = if self >= 0.0 { self + 0.5 } else { self - 0.5 };
                let y = if y.is_nan() { 0.0 } else { y.max(T::MIN as $t).min(T::MAX as $t) };
                T::from_i128(y.trunc() as i128)
            }
        }
    )*};
}
float_sample!(f32, f64);

/// Whether [`convert`] converts a window of type `from` to the working type `to`: for every
/// pair of types but a float type into an integer type whose limits it does not hold exactly
/// (`float32` into 32- and 64-bit integers, `float64` into 64-bit ones).
pub fn converts(from: DataType, to: DataType) -> bool {
    use DataType::{F32, F64, I32, I64, U32, U64};
    !matches!((from, to), (F32, U32 | I32 | U64 | I64) | (F64, U64 | I64))
}

/// A window converted to the working type `to`, value by value as the module describes.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if [`converts`] does not take the window's type to `to`.
pub fn convert(data: &RasterData, to: DataType) -> Result<RasterData> {
    let from = data.data_type();
    if !converts(from, to) {
        return Err(Error::InvalidArgument(format!("a {from:?} window is not converted to {to:?}")));
    }
    Ok(match data {
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

/// The pixels an alpha band leaves in a window: those whose alpha is not 0, in the window's own
/// type, before any conversion. An alpha of 0.1 leaves its pixel in, even for a working type
/// that would round it to 0; NaN leaves it in, and -0.0 leaves it out.
///
/// `data` holds the window's bands, `pixels` values each, one band after the other, and `band`
/// is the alpha band's, numbered from 0.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the window holds no band `band` of `pixels` values.
pub fn alpha_validity(data: &RasterData, band: usize, pixels: usize) -> Result<Vec<bool>> {
    fn of<S: Sample>(v: &[S], band: usize, pixels: usize) -> Result<Vec<bool>> {
        let range = band.checked_mul(pixels).and_then(|start| Some(start..start.checked_add(pixels)?));
        let values = range.and_then(|r| v.get(r)).ok_or_else(|| {
            Error::InvalidArgument(format!("a window of {} values has no band {} of {pixels} pixels", v.len(), band as u128 + 1))
        })?;
        Ok(values.iter().map(|x| x.as_f64() != 0.0).collect())
    }
    match data {
        RasterData::U8(v) => of(v, band, pixels),
        RasterData::I8(v) => of(v, band, pixels),
        RasterData::U16(v) => of(v, band, pixels),
        RasterData::I16(v) => of(v, band, pixels),
        RasterData::U32(v) => of(v, band, pixels),
        RasterData::I32(v) => of(v, band, pixels),
        RasterData::U64(v) => of(v, band, pixels),
        RasterData::I64(v) => of(v, band, pixels),
        RasterData::F32(v) => of(v, band, pixels),
        RasterData::F64(v) => of(v, band, pixels),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPES: [DataType; 10] = [
        DataType::U8,
        DataType::I8,
        DataType::U16,
        DataType::I16,
        DataType::U32,
        DataType::I32,
        DataType::U64,
        DataType::I64,
        DataType::F32,
        DataType::F64,
    ];

    /// Whether two windows hold the same type and values, floats compared bit for bit but NaN,
    /// whose payload is not compared.
    fn same(a: &RasterData, b: &RasterData) -> bool {
        fn floats<F: Copy + Into<f64>>(x: &[F], y: &[F]) -> bool {
            let bits = |v: F| -> Option<u64> {
                let v: f64 = v.into();
                (!v.is_nan()).then_some(v.to_bits())
            };
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| bits(*a) == bits(*b))
        }
        match (a, b) {
            (RasterData::U8(x), RasterData::U8(y)) => x == y,
            (RasterData::I8(x), RasterData::I8(y)) => x == y,
            (RasterData::U16(x), RasterData::U16(y)) => x == y,
            (RasterData::I16(x), RasterData::I16(y)) => x == y,
            (RasterData::U32(x), RasterData::U32(y)) => x == y,
            (RasterData::I32(x), RasterData::I32(y)) => x == y,
            (RasterData::U64(x), RasterData::U64(y)) => x == y,
            (RasterData::I64(x), RasterData::I64(y)) => x == y,
            (RasterData::F32(x), RasterData::F32(y)) => floats(x, y),
            (RasterData::F64(x), RasterData::F64(y)) => floats(x, y),
            _ => false,
        }
    }

    /// Asserts that `data` converted to the type of `want` gives `want`.
    fn converts_to(data: RasterData, want: RasterData) {
        let got = convert(&data, want.data_type()).unwrap();
        assert!(same(&got, &want), "{data:?} gave {got:?}, not {want:?}");
    }

    /// A window of one value 7 in type `t`.
    fn seven(t: DataType) -> RasterData {
        match t {
            DataType::U8 => RasterData::U8(vec![7]),
            DataType::I8 => RasterData::I8(vec![7]),
            DataType::U16 => RasterData::U16(vec![7]),
            DataType::I16 => RasterData::I16(vec![7]),
            DataType::U32 => RasterData::U32(vec![7]),
            DataType::I32 => RasterData::I32(vec![7]),
            DataType::U64 => RasterData::U64(vec![7]),
            DataType::I64 => RasterData::I64(vec![7]),
            DataType::F32 => RasterData::F32(vec![7.0]),
            DataType::F64 => RasterData::F64(vec![7.0]),
        }
    }

    #[test]
    fn every_pair_converts_but_floats_into_integers_whose_limits_they_round() {
        use DataType::{F32, F64, I32, I64, U32, U64};
        let refused = [(F32, U32), (F32, I32), (F32, U64), (F32, I64), (F64, U64), (F64, I64)];
        for from in TYPES {
            for to in TYPES {
                let expected = !refused.contains(&(from, to));
                assert_eq!(converts(from, to), expected, "{from:?} -> {to:?}");
                match convert(&seven(from), to) {
                    Ok(got) => assert!(expected && same(&got, &seven(to)), "{from:?} -> {to:?}: {got:?}"),
                    Err(e) => assert!(!expected && matches!(e, Error::InvalidArgument(_)), "{from:?} -> {to:?}: {e:?}"),
                }
            }
        }
    }

    #[test]
    fn a_float_rounds_half_away_from_zero_in_its_own_precision() {
        // 0.49999997 is the f32 just below 0.5: plus 0.5 in f32 it is 1, as is the f64 just below
        // 0.5 plus 0.5 in f64. Halves go away from zero.
        converts_to(
            RasterData::F32(vec![0.499_999_97, 0.5, 1.5, 2.5, -0.5, -1.5, -2.5, 2.499_999_8]),
            RasterData::I8(vec![1, 1, 2, 3, -1, -2, -3, 2]),
        );
        converts_to(RasterData::F64(vec![0.499_999_999_999_999_94, 0.5, -0.5, 2.5, -2.5, 0.25]), RasterData::I16(vec![1, 1, -1, 3, -3, 0]));
    }

    #[test]
    fn a_float_into_an_integer_type_is_clipped_and_nan_made_zero() {
        let v = vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e30, -1e30, 255.5, 254.49, -0.4];
        converts_to(RasterData::F64(v.clone()), RasterData::U8(vec![0, 255, 0, 255, 0, 255, 254, 0]));
        converts_to(RasterData::F64(v.clone()), RasterData::I16(vec![0, 32767, -32768, 32767, -32768, 256, 254, 0]));
        let f32s = v.iter().map(|x| *x as f32).collect();
        converts_to(RasterData::F32(f32s), RasterData::U16(vec![0, 65535, 0, 65535, 0, 256, 254, 0]));
        // float64 into 32-bit integers, whose limits it holds exactly.
        let v = vec![4_294_967_295.4, 4_294_967_295.5, -2_147_483_648.5, 2_147_483_646.5];
        converts_to(RasterData::F64(v.clone()), RasterData::U32(vec![u32::MAX, u32::MAX, 0, 2_147_483_647]));
        converts_to(RasterData::F64(v), RasterData::I32(vec![i32::MAX, i32::MAX, i32::MIN, i32::MAX]));
    }

    #[test]
    fn an_integer_saturates_into_a_narrower_type() {
        converts_to(RasterData::I32(vec![-1, 70000, 255, 0]), RasterData::U8(vec![0, 255, 255, 0]));
        converts_to(RasterData::U64(vec![u64::MAX, 5]), RasterData::I64(vec![i64::MAX, 5]));
        converts_to(RasterData::I64(vec![i64::MIN, -1, i64::MAX]), RasterData::U32(vec![0, 0, u32::MAX]));
        converts_to(RasterData::I8(vec![-128, 127]), RasterData::U16(vec![0, 127]));
        converts_to(RasterData::U16(vec![65535, 128]), RasterData::I8(vec![127, 127]));
    }

    #[test]
    fn a_value_into_a_float_type_rounds_to_the_nearest() {
        converts_to(RasterData::U64(vec![(1 << 53) + 1]), RasterData::F64(vec![9_007_199_254_740_992.0]));
        converts_to(RasterData::U32(vec![16_777_217]), RasterData::F32(vec![16_777_216.0]));
        converts_to(RasterData::F64(vec![0.1, f64::NAN]), RasterData::F32(vec![0.1, f32::NAN]));
        converts_to(RasterData::F32(vec![0.1, f32::NAN]), RasterData::F64(vec![f64::from(0.1f32), f64::from(f32::NAN)]));
    }

    #[test]
    fn an_alpha_leaves_out_the_pixels_it_makes_zero_in_its_own_type() {
        // Band 1 the data, band 2 the alpha. An alpha of 0.1 or -0.1, which a uint8 working
        // type would make 0, leaves its pixel in; NaN as well; 0 and -0.0 leave it out.
        let alpha = [0.0, 0.1, -0.0, f32::NAN, -0.1, 1e-45, 255.0];
        let mut v = vec![9.0f32; alpha.len()];
        v.extend(alpha);
        let valid = alpha_validity(&RasterData::F32(v.clone()), 1, alpha.len()).unwrap();
        assert_eq!(valid, [false, true, false, true, true, true, true]);
        // Converted first, the same alpha would leave out all but 255.
        let converted = convert(&RasterData::F32(v), DataType::U8).unwrap();
        assert_eq!(alpha_validity(&converted, 1, alpha.len()).unwrap(), [false, false, false, false, false, false, true]);
        assert_eq!(alpha_validity(&RasterData::I16(vec![5, 5, 5, 0, -1, 3]), 1, 3).unwrap(), [false, true, true]);
    }

    #[test]
    fn a_window_without_the_alpha_band_is_an_invalid_argument() {
        let data = RasterData::U8(vec![1; 8]);
        assert_eq!(alpha_validity(&data, 1, 4).unwrap(), [true; 4]);
        for (band, pixels) in [(2, 4), (1, 5), (usize::MAX, 4), (1, usize::MAX)] {
            let got = alpha_validity(&data, band, pixels);
            assert!(matches!(got, Err(Error::InvalidArgument(_))), "band {band} of {pixels}: {got:?}");
        }
    }
}
