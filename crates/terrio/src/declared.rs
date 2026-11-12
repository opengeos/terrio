//! Opt-in bounds on what a raster may declare, checked when it is opened, before its pixels are
//! read where its reader reads a header first.
//!
//! A file of a few hundred kilobytes can declare gigabytes of pixels (every strip or tile of a
//! GeoTIFF pointing to one small compressed block, a Zarr array of missing chunks, a VRT): reading
//! it whole takes the memory, the time and the temporary disk of what it declares. A service that
//! opens files it does not trust sets these options to what it is prepared to decode:
//!
//! | Option | Refuses a raster that declares |
//! |---|---|
//! | `TERRIO_MAX_DECLARED_BYTES` | more than this many bytes of pixels: width x height x bands x bytes per sample, the sample type being the one terrio reads (8 bytes for GRIB's `Float64` bands, 4 for half floats, read as `Float32`) |
//! | `TERRIO_MAX_BANDS` | more than this many bands (a complex band counts once) |
//!
//! Both are unset by default, and read as the other options are ([`crate::config::get`]): from
//! the calling thread's scope (Python's `terrio.rio.Env`, [`crate::config::push_scope`]), else
//! the process environment. Each must be a positive integer. A raster they refuse is an
//! [`Error::ResourceLimit`] (Python's `ResourceLimitError`) naming the option, which the optional
//! GDAL backend does not retry.

use crate::{Error, LazyRaster, RasterDataset, Result};
use wbraster::DataType;

/// The option bounding the bytes of pixels a raster may declare.
pub(crate) const MAX_DECLARED_BYTES_OPTION: &str = "TERRIO_MAX_DECLARED_BYTES";

/// The option bounding the bands a raster may declare.
pub(crate) const MAX_BANDS_OPTION: &str = "TERRIO_MAX_BANDS";

/// The value of the option `name`, a positive integer of `unit`, or `None` when it is unset.
fn bound(name: &str, unit: &str) -> Result<Option<u64>> {
    match crate::config::get(name) {
        None => Ok(None),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .map(Some)
            .ok_or_else(|| Error::InvalidArgument(format!("{name} must be a positive integer number of {unit}, not '{v}'"))),
    }
}

/// GDAL 3.13.3 complex bands use paired real/imaginary component bands. Refuse an
/// unrepresentable component count before an unchecked product can wrap or panic.
pub(crate) fn component_band_count(bands: usize, complex: bool) -> Result<usize> {
    bands
        .checked_mul(if complex { 2 } else { 1 })
        .ok_or_else(|| Error::ResourceLimit(format!("the raster's {bands} complex bands exceed the component address space")))
}

/// Refuses a raster of `cols` x `rows` pixels in `bands` bands of `data_type` samples (each part
/// of a complex sample a band of its own, as [`RasterDataset::complex`] holds them) that declares
/// more than [`MAX_BANDS_OPTION`] or [`MAX_DECLARED_BYTES_OPTION`] allow on the calling thread.
pub(crate) fn check(cols: usize, rows: usize, bands: usize, data_type: DataType, complex: bool) -> Result<()> {
    check_bytes(cols, rows, bands, data_type.size_bytes(), complex)
}

/// [`check`] for samples of `sample_bytes` bytes each.
pub(crate) fn check_bytes(cols: usize, rows: usize, bands: usize, sample_bytes: usize, complex: bool) -> Result<()> {
    let shown = if complex { bands / 2 } else { bands };
    if let Some(most) = bound(MAX_BANDS_OPTION, "bands")?
        && shown as u64 > most
    {
        return Err(Error::ResourceLimit(format!("the raster declares {shown} bands, more than {MAX_BANDS_OPTION}={most}")));
    }
    if let Some(most) = bound(MAX_DECLARED_BYTES_OPTION, "bytes")? {
        let bytes = [rows, bands, sample_bytes].iter().fold(cols as u128, |n, &d| n.saturating_mul(d as u128));
        if bytes > u128::from(most) {
            return Err(Error::ResourceLimit(format!(
                "the raster declares {cols} x {rows} pixels in {shown} band(s), {bytes} bytes, more than {MAX_DECLARED_BYTES_OPTION}={most}"
            )));
        }
    }
    Ok(())
}

/// [`check`] for a raster read whole, or a header (a dataset without its pixels).
pub(crate) fn check_dataset(ds: &RasterDataset) -> Result<()> {
    let r = &ds.raster;
    check(r.cols, r.rows, r.bands, r.data_type, ds.complex)
}

/// [`check`] for a raster opened to be read by window.
pub(crate) fn check_lazy(r: &LazyRaster) -> Result<()> {
    check(r.width(), r.height(), r.band_count(), r.data_type(), r.is_complex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::with_options;

    #[test]
    fn complex_component_counts_refuse_address_space_overflow() {
        assert_eq!(component_band_count(usize::MAX, false).unwrap(), usize::MAX);
        assert_eq!(component_band_count(usize::MAX / 2, true).unwrap(), usize::MAX - 1);
        assert!(matches!(component_band_count(usize::MAX / 2 + 1, true), Err(Error::ResourceLimit(_))));
    }

    /// Unset, nothing is refused; each option refuses what exceeds it and allows what equals it.
    #[test]
    fn each_bound_refuses_more_than_it_allows() {
        assert!(check(usize::MAX, usize::MAX, usize::MAX, DataType::F64, false).is_ok());
        // 10 x 20 pixels of 3 bands of 4 bytes: 2,400 bytes.
        let declared = || check(10, 20, 3, DataType::F32, false);
        with_options(&[(MAX_DECLARED_BYTES_OPTION, "2400")], || assert!(declared().is_ok()));
        with_options(&[(MAX_DECLARED_BYTES_OPTION, "2399")], || {
            let e = declared().unwrap_err();
            assert!(matches!(e, Error::ResourceLimit(_)), "{e:?}");
            assert_eq!(
                e.to_string(),
                "the raster declares 10 x 20 pixels in 3 band(s), 2400 bytes, more than TERRIO_MAX_DECLARED_BYTES=2399"
            );
        });
        with_options(&[(MAX_BANDS_OPTION, "3")], || assert!(declared().is_ok()));
        with_options(&[(MAX_BANDS_OPTION, "2")], || {
            let e = declared().unwrap_err();
            assert!(matches!(e, Error::ResourceLimit(_)), "{e:?}");
            assert_eq!(e.to_string(), "the raster declares 3 bands, more than TERRIO_MAX_BANDS=2");
        });
    }

    /// A complex band counts once, its parts both counted in its bytes; sizes that overflow are
    /// refused, not wrapped.
    #[test]
    fn complex_bands_and_huge_sizes_are_counted_whole() {
        // Two CFloat32 bands, held as four Float32 parts: 2 x 2 x 4 x 4 = 64 bytes.
        let complex = || check(2, 2, 4, DataType::F32, true);
        with_options(&[(MAX_BANDS_OPTION, "2")], || assert!(complex().is_ok()));
        with_options(&[(MAX_BANDS_OPTION, "1")], || assert!(complex().is_err()));
        with_options(&[(MAX_DECLARED_BYTES_OPTION, "64")], || assert!(complex().is_ok()));
        with_options(&[(MAX_DECLARED_BYTES_OPTION, "63")], || assert!(complex().is_err()));
        with_options(&[(MAX_DECLARED_BYTES_OPTION, &u64::MAX.to_string())], || {
            assert!(check(usize::MAX, usize::MAX, 2, DataType::U8, false).is_err());
        });
    }

    /// Anything but a positive integer is an invalid argument naming the option: zero, a negative
    /// number, a number with a unit, a fraction, or one more than the largest `u64`. The largest is
    /// taken, and an empty or blank value is unset, as for the other options.
    #[test]
    fn invalid_values_name_the_option() {
        for (option, unit) in [(MAX_DECLARED_BYTES_OPTION, "bytes"), (MAX_BANDS_OPTION, "bands")] {
            for value in ["0", "-1", "4 GiB", "1.5", "18446744073709551616"] {
                with_options(&[(option, value)], || {
                    let e = check(1, 1, 1, DataType::U8, false).unwrap_err();
                    assert!(matches!(e, Error::InvalidArgument(_)), "{option}={value}: {e:?}");
                    assert_eq!(e.to_string(), format!("{option} must be a positive integer number of {unit}, not '{value}'"));
                });
            }
            for value in ["18446744073709551615", "", " "] {
                with_options(&[(option, value)], || assert!(check(1, 1, 1, DataType::U8, false).is_ok(), "{option}={value:?}"));
            }
        }
    }
}
