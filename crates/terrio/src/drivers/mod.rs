//! Format drivers that terrio supplies itself, where the format libraries are wrong or missing.
//! Each reads and writes files the way GDAL does, so that both tools exchange them losslessly.

use crate::formats::RasterDriver;
use crate::{RasterDataset, Result};
use std::path::Path;
use wbraster::Raster;

mod aaigrid;
#[cfg(any(feature = "netcdf-hdf5", feature = "zarr"))]
mod cf;
pub(crate) mod common;
mod ehdr;
mod envi;
mod ers;
pub(crate) mod geokeys;
#[cfg(feature = "sqlite")]
pub(crate) mod gpkg;
#[cfg(feature = "grib")]
pub(crate) mod grib;
mod gsbg;
pub(crate) mod gtiff_write;
#[cfg(feature = "netcdf-hdf5")]
pub(crate) mod hdf5;
#[cfg(feature = "netcdf-hdf5")]
pub(crate) mod hdf5_write;
#[cfg(feature = "netcdf-hdf5")]
pub(crate) mod hdfeos;
mod hfa;
mod hgt;
#[cfg(feature = "jp2")]
pub(crate) mod jp2;
#[cfg(feature = "netcdf-hdf5")]
pub(crate) mod netcdf;
#[cfg(feature = "netcdf-hdf5")]
pub(crate) mod netcdf_write;
pub(crate) mod nodata;
mod png;
mod rst;
mod saga;
#[cfg(feature = "sqlite")]
pub(crate) mod sqlite;
#[cfg(feature = "sqlite")]
pub(crate) mod sqlite_edit;
pub(crate) mod vector;
pub(crate) mod wkt;
mod xyz;
#[cfg(feature = "zarr")]
pub(crate) mod zarr;
#[cfg(feature = "zarr")]
mod zarr_store;

/// Reads a raster with one of terrio's own drivers. `None` when none of them handles the file.
pub(crate) fn read(path: &str) -> Option<Result<RasterDataset>> {
    Some(read_own(path)?.map(|mut ds| {
        nodata::apply_pam(path, &mut ds);
        ds
    }))
}

fn read_own(path: &str) -> Option<Result<RasterDataset>> {
    let p = Path::new(path);
    let by_extension = RasterDriver::for_extension(path);
    // Labelled raw rasters (a data file with a `.hdr`) use many extensions; the formats with
    // extensions of their own are never sniffed as such.
    let raw_candidate = matches!(by_extension, None | Some(RasterDriver::ENVI | RasterDriver::EHdr | RasterDriver::HFA));
    if raw_candidate && envi::is_envi(p) {
        return Some(envi::read(p));
    }
    if raw_candidate && ehdr::is_ehdr(p) {
        return Some(ehdr::read(p));
    }
    #[cfg(feature = "zarr")]
    if zarr::is_zarr(p) {
        return Some(zarr::read(p));
    }
    #[cfg(feature = "netcdf-hdf5")]
    if by_extension.is_none() && hdf5::is_hdf5(p) {
        return Some(hdf5::read(p, path, None));
    }
    #[cfg(feature = "netcdf-hdf5")]
    if by_extension.is_none() && netcdf::is_netcdf(p) {
        return Some(netcdf::read(p, None));
    }
    if gsbg::is_gsbg(p) {
        return Some(gsbg::read(p));
    }
    // SRTM tiles are known by their name and size, whatever their extension (`.raw`, `.img`...).
    if hgt::is_hgt(p) {
        return Some(hgt::read(p));
    }
    #[cfg(feature = "grib")]
    if matches!(by_extension, None | Some(RasterDriver::GRIB)) && grib::is_grib_file(p) {
        return Some(grib::read(p));
    }
    let driver = by_extension?;
    if let Err(e) = driver.ensure_built() {
        return Some(Err(e));
    }
    Some(match driver {
        RasterDriver::AAIGrid => aaigrid::read(p),
        RasterDriver::ERS => ers::read(p),
        RasterDriver::SAGA => saga::read(p),
        RasterDriver::XYZ => xyz::read(p),
        RasterDriver::RST => rst::read(p),
        RasterDriver::PNG => png::read(p),
        RasterDriver::JPEG => png::read_jpeg(p),
        #[cfg(feature = "jp2")]
        RasterDriver::JP2 => jp2::read(p),
        #[cfg(feature = "sqlite")]
        RasterDriver::GPKG => gpkg::read(p),
        RasterDriver::HFA => hfa::read(p),
        // Not a tile, as `is_hgt` found: the reader says why.
        RasterDriver::SRTMHGT => hgt::read(p),
        #[cfg(feature = "netcdf-hdf5")]
        RasterDriver::NetCDF => netcdf::read(p, None),
        #[cfg(feature = "netcdf-hdf5")]
        RasterDriver::HDF5 => hdf5::read(p, path, None),
        _ => return None,
    })
}

/// Opens a raster read by window with one of terrio's own drivers (Zarr stores, local or remote,
/// read chunk by chunk; JPEG 2000 by tile and resolution level; GRIB by message). `None` when none of them reads
/// the file so.
#[cfg_attr(not(any(feature = "zarr", feature = "jp2", feature = "grib")), allow(unused_variables))]
pub(crate) fn open_lazy(path: &str) -> Option<Result<crate::LazyRaster>> {
    #[cfg(feature = "zarr")]
    if let Some(result) = zarr::open_lazy(path) {
        return Some(result);
    }
    #[cfg(feature = "jp2")]
    if let Some(result) = jp2::open_lazy(path) {
        return Some(result);
    }
    #[cfg(feature = "grib")]
    if let Some(result) = grib::open_lazy(path) {
        return Some(result);
    }
    None
}

/// Whether a name starts with a GDAL driver prefix (`NETCDF:`), case-insensitively.
#[allow(dead_code)]
fn has_prefix(name: &str, prefix: &str) -> bool {
    name.get(..prefix.len()).is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

/// Reads a dataset that terrio's own drivers read in place rather than downloading it (remote
/// Zarr stores, GDAL's `ZARR:` names). `None` when `path` is no such dataset.
pub(crate) fn open_in_place(path: &str) -> Option<Result<RasterDataset>> {
    #[cfg(feature = "zarr")]
    return zarr::open_named(path);
    #[cfg(not(feature = "zarr"))]
    has_prefix(path, "ZARR:").then(|| Err(crate::Error::disabled("zarr")))
}

/// Opens a subdataset named as GDAL names them (`NETCDF:"file.nc":variable`,
/// `NETCDF:"file.nc":/group/variable`, `HDF5:"file.h5"://group/dataset`, `GPKG:file.gpkg:table`).
/// `None` when the path is not a subdataset name.
pub(crate) fn open_subdataset(path: &str) -> Option<Result<RasterDataset>> {
    #[cfg(feature = "netcdf-hdf5")]
    if let Some((file, dataset)) = hdf5::parse_subdataset(path) {
        return Some(crate::vfs::resolve(&file).and_then(|local| {
            let mut ds = hdf5::read(Path::new(local.as_str()), &file, Some(&dataset))?;
            nodata::apply_pam(local.as_str(), &mut ds);
            Ok(ds)
        }));
    }
    #[cfg(feature = "sqlite")]
    if let Some((file, table)) = gpkg::parse_subdataset(path) {
        return Some(crate::vfs::resolve(&file).and_then(|local| {
            let mut ds = gpkg::read_bytes(std::fs::read(local.as_str())?, Some(&table))?;
            nodata::apply_pam(local.as_str(), &mut ds);
            Ok(ds)
        }));
    }
    #[cfg(not(feature = "sqlite"))]
    if has_prefix(path, "GPKG:") {
        return Some(Err(crate::Error::disabled("sqlite")));
    }
    #[cfg(feature = "netcdf-hdf5")]
    if let Some((file, variable)) = netcdf::parse_subdataset(path) {
        return Some(crate::vfs::resolve(&file).and_then(|local| {
            let mut ds = netcdf::read(Path::new(local.as_str()), Some(&variable))?;
            nodata::apply_pam(local.as_str(), &mut ds);
            Ok(ds)
        }));
    }
    #[cfg(not(feature = "netcdf-hdf5"))]
    if has_prefix(path, "NETCDF:") || has_prefix(path, "HDF5:") {
        return Some(Err(crate::Error::disabled("netcdf-hdf5")));
    }
    None
}

/// Writes rasters into one netCDF-4 file, each in its group (see `netcdf_write::groups_to_bytes`).
#[cfg(feature = "netcdf-hdf5")]
pub(crate) fn write_netcdf_groups(rasters: &[(&str, &RasterDataset)], path: &str, options: &crate::RasterWriteOptions) -> Result<()> {
    std::fs::write(path, netcdf_groups_to_bytes(rasters, options)?)?;
    Ok(())
}

/// Encodes rasters as one netCDF-4 file, each in its group, with the netCDF creation options of
/// `options` (`FORMAT`, `COMPRESS`, `ZLEVEL`, `CHUNKING`).
#[cfg(feature = "netcdf-hdf5")]
pub(crate) fn netcdf_groups_to_bytes(rasters: &[(&str, &RasterDataset)], options: &crate::RasterWriteOptions) -> Result<Vec<u8>> {
    // The first of an option given twice, as in `write`.
    let co = |key: &str| options.creation_options.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());
    let o = netcdf_write::Options {
        format: co("FORMAT"),
        compress: co("COMPRESS").or(options.compression.as_deref()),
        zlevel: co("ZLEVEL"),
        chunking: co("CHUNKING"),
        group: None,
    };
    let entries: Vec<(&str, &Raster, Option<f64>)> = rasters.iter().map(|(g, ds)| (*g, &ds.raster, ds.nodata())).collect();
    netcdf_write::groups_to_bytes(&entries, &o)
}

/// The rasters of an existing netCDF file with the group each is in, to append another one to
/// it (`APPEND_SUBDATASET=YES`): each of its raster variables, read as rasters and written again
/// with the new one (so what terrio's raster model does not hold, such as other variables and
/// attributes or the original chunking, is not carried over). Files with several variables in
/// one group cannot be carried over, since terrio writes one raster per group.
#[cfg(feature = "netcdf-hdf5")]
fn netcdf_rasters_of(path: &str) -> Result<Vec<(String, RasterDataset)>> {
    let mut out: Vec<(String, RasterDataset)> = Vec::new();
    for var in netcdf::data_variable_paths(Path::new(path))? {
        let group = var.trim_start_matches('/').rsplit_once('/').map_or("", |(g, _)| g).to_string();
        if out.iter().any(|(g, _)| *g == group) {
            return Err(crate::Error::Unsupported(format!(
                "{path} has several variables in the group '/{group}': terrio appends only to files with one raster per group"
            )));
        }
        out.push((group, crate::open_raster(&format!("NETCDF:\"{path}\":{var}"))?));
    }
    Ok(out)
}

/// Writes a raster with one of terrio's own drivers, declaring `nodata`, with the format's
/// compression and creation options when it has a choice. `None` when none of them handles the format.
pub(crate) fn write(
    r: &Raster,
    nodata: Option<f64>,
    path: &str,
    driver: RasterDriver,
    options: &crate::RasterWriteOptions,
) -> Option<Result<()>> {
    if let Err(e) = driver.ensure_built() {
        return Some(Err(e));
    }
    let p = Path::new(path);
    #[cfg_attr(not(any(feature = "netcdf-hdf5", feature = "sqlite", feature = "zarr")), allow(unused_variables))]
    let compression = options.compression.as_deref();
    #[cfg_attr(not(any(feature = "netcdf-hdf5", feature = "sqlite")), allow(unused_variables))]
    let co = |key: &str| options.creation_options.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());
    Some(match driver {
        RasterDriver::ENVI => envi::write(r, nodata, p),
        RasterDriver::EHdr => ehdr::write(r, nodata, p),
        RasterDriver::AAIGrid => aaigrid::write(r, nodata, p),
        RasterDriver::ERS => ers::write(r, nodata, p),
        RasterDriver::GSBG => gsbg::write(r, nodata, p),
        RasterDriver::SRTMHGT => hgt::write(r, nodata, p),
        RasterDriver::SAGA => saga::write(r, nodata, p),
        RasterDriver::XYZ => xyz::write(r, p),
        RasterDriver::RST => rst::write(r, nodata, p),
        RasterDriver::PNG => png::write(r, nodata, p),
        #[cfg(feature = "jp2")]
        RasterDriver::JP2 => jp2::write(r, nodata, p),
        #[cfg(feature = "zarr")]
        RasterDriver::Zarr => zarr::write_with(r, nodata, p, compression, &options.creation_options, &options.band_metadata),
        #[cfg(feature = "netcdf-hdf5")]
        RasterDriver::NetCDF => {
            let o = netcdf_write::Options {
                format: co("FORMAT"),
                compress: co("COMPRESS").or(compression),
                zlevel: co("ZLEVEL"),
                chunking: co("CHUNKING"),
                group: co("GROUP"),
            };
            let append = co("APPEND_SUBDATASET").is_some_and(|v| matches!(v.to_ascii_uppercase().as_str(), "YES" | "TRUE" | "ON" | "1"));
            if append && p.exists() {
                // The file's rasters, then this one in its group: the file written again.
                (|| {
                    let existing = netcdf_rasters_of(path)?;
                    let new = RasterDataset::with_driver(r.clone(), RasterDriver::NetCDF, nodata.is_some());
                    let mut all: Vec<(&str, &RasterDataset)> = existing.iter().map(|(g, ds)| (g.as_str(), ds)).collect();
                    all.push((o.group.unwrap_or(""), &new));
                    let bytes = netcdf_groups_to_bytes(&all, options)?;
                    // Written beside the file, then renamed over it: an interrupted write leaves the
                    // original as it was.
                    let tmp = p.with_extension(format!("nc.tmp-{}", std::process::id()));
                    std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, p)).inspect_err(|_| {
                        let _ = std::fs::remove_file(&tmp);
                    })?;
                    Ok(())
                })()
            } else {
                netcdf_write::write(r, nodata, p, &o)
            }
        }
        #[cfg(feature = "sqlite")]
        RasterDriver::GPKG => gpkg::write(r, nodata, p, compression.or(co("TILE_FORMAT")), co("RASTER_TABLE")),
        #[cfg(feature = "tiles")]
        RasterDriver::MBTiles | RasterDriver::PMTiles => {
            let ds = RasterDataset::with_driver(r.clone(), driver, nodata.is_some());
            crate::tiles::write_raster(&ds, path, driver, options)
        }
        // Formats without a CRS of their own: GDAL's sidecar holds it.
        RasterDriver::PCRaster | RasterDriver::GRASSASCIIGrid => {
            r.write(path, driver.to_wb()).map_err(Into::into).and_then(|()| common::write_pam(p, &r.crs))
        }
        RasterDriver::JPEG => r.write(path, wbraster::RasterFormat::Jpeg).map_err(Into::into).and_then(|()| {
            png::write_world_file(r, p)?;
            common::write_pam(p, &r.crs)
        }),
        _ => return None,
    })
}
