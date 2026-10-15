//! Format drivers that terrio supplies itself, where the format libraries are wrong or missing.
//! Each reads and writes files the way GDAL does, so that both tools exchange them losslessly.

use crate::formats::RasterDriver;
use crate::{RasterDataset, Result};
use std::path::Path;
use wbraster::Raster;

mod aaigrid;
pub(crate) mod common;
mod ehdr;
mod envi;
mod ers;
pub(crate) mod geokeys;
pub(crate) mod gpkg;
mod gsbg;
pub(crate) mod gtiff_write;
pub(crate) mod hdf5;
mod hdf5_write;
mod hfa;
pub(crate) mod jp2;
pub(crate) mod netcdf;
mod netcdf_write;
pub(crate) mod nodata;
mod png;
mod rst;
mod saga;
pub(crate) mod sqlite;
pub(crate) mod vector;
pub(crate) mod wkt;
mod xyz;
mod zarr;

/// Reads a raster with one of terrio's own drivers. `None` when none of them handles the file.
pub(crate) fn read(path: &str) -> Option<Result<RasterDataset>> {
    let p = Path::new(path);
    // Labelled raw rasters (a data file with a `.hdr`) use many extensions; the formats with
    // extensions of their own are never sniffed as such.
    let raw_candidate =
        matches!(RasterDriver::for_extension(path), None | Some(RasterDriver::ENVI | RasterDriver::EHdr | RasterDriver::HFA));
    let result = if raw_candidate && envi::is_envi(p) {
        envi::read(p)
    } else if raw_candidate && ehdr::is_ehdr(p) {
        ehdr::read(p)
    } else if zarr::is_zarr(p) {
        zarr::read(p)
    } else if RasterDriver::for_extension(path).is_none() && hdf5::is_hdf5(p) {
        hdf5::read(p, path, None)
    } else if RasterDriver::for_extension(path).is_none() && netcdf::is_netcdf(p) {
        netcdf::read(p, None)
    } else if gsbg::is_gsbg(p) {
        gsbg::read(p)
    } else {
        match RasterDriver::for_extension(path)? {
            RasterDriver::AAIGrid => aaigrid::read(p),
            RasterDriver::ERS => ers::read(p),
            RasterDriver::SAGA => saga::read(p),
            RasterDriver::XYZ => xyz::read(p),
            RasterDriver::RST => rst::read(p),
            RasterDriver::PNG => png::read(p),
            RasterDriver::JPEG => png::read_jpeg(p),
            RasterDriver::JP2 => jp2::read(p),
            RasterDriver::GPKG => gpkg::read(p),
            RasterDriver::HFA => hfa::read(p),
            RasterDriver::NetCDF => netcdf::read(p, None),
            RasterDriver::HDF5 => hdf5::read(p, path, None),
            _ => return None,
        }
    };
    Some(result.map(|mut ds| {
        nodata::apply_pam(path, &mut ds);
        ds
    }))
}

/// Opens a raster read by window with one of terrio's own drivers (Zarr multiscales pyramids).
/// `None` when none of them reads the file so.
pub(crate) fn open_lazy(path: &str) -> Option<Result<crate::LazyRaster>> {
    let p = Path::new(path);
    if zarr::is_zarr(p) { zarr::open_lazy(p) } else { None }
}

/// Opens a subdataset named as GDAL names them (`NETCDF:"file.nc":variable`,
/// `NETCDF:"file.nc":/group/variable`, `HDF5:"file.h5"://group/dataset`, `GPKG:file.gpkg:table`).
/// `None` when the path is not a subdataset name.
pub(crate) fn open_subdataset(path: &str) -> Option<Result<RasterDataset>> {
    if let Some((file, dataset)) = hdf5::parse_subdataset(path) {
        return Some(crate::vfs::resolve(&file).and_then(|local| {
            let mut ds = hdf5::read(Path::new(local.as_str()), &file, Some(&dataset))?;
            nodata::apply_pam(local.as_str(), &mut ds);
            Ok(ds)
        }));
    }
    if let Some((file, table)) = gpkg::parse_subdataset(path) {
        return Some(crate::vfs::resolve(&file).and_then(|local| {
            let mut ds = gpkg::read_bytes(std::fs::read(local.as_str())?, Some(&table))?;
            nodata::apply_pam(local.as_str(), &mut ds);
            Ok(ds)
        }));
    }
    let (file, variable) = netcdf::parse_subdataset(path)?;
    Some(crate::vfs::resolve(&file).and_then(|local| {
        let mut ds = netcdf::read(Path::new(local.as_str()), Some(&variable))?;
        nodata::apply_pam(local.as_str(), &mut ds);
        Ok(ds)
    }))
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
    let p = Path::new(path);
    let compression = options.compression.as_deref();
    let co = |key: &str| options.creation_options.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());
    Some(match driver {
        RasterDriver::ENVI => envi::write(r, nodata, p),
        RasterDriver::EHdr => ehdr::write(r, nodata, p),
        RasterDriver::AAIGrid => aaigrid::write(r, nodata, p),
        RasterDriver::ERS => ers::write(r, nodata, p),
        RasterDriver::GSBG => gsbg::write(r, nodata, p),
        RasterDriver::SAGA => saga::write(r, nodata, p),
        RasterDriver::XYZ => xyz::write(r, p),
        RasterDriver::RST => rst::write(r, nodata, p),
        RasterDriver::PNG => png::write(r, nodata, p),
        RasterDriver::JP2 => jp2::write(r, nodata, p),
        RasterDriver::Zarr => zarr::write_with(r, nodata, p, compression, &options.creation_options),
        RasterDriver::NetCDF => {
            let o = netcdf_write::Options {
                format: co("FORMAT"),
                compress: co("COMPRESS").or(compression),
                zlevel: co("ZLEVEL"),
                chunking: co("CHUNKING"),
            };
            netcdf_write::write(r, nodata, p, &o)
        }
        RasterDriver::GPKG => gpkg::write(r, nodata, p, compression.or(co("TILE_FORMAT")), co("RASTER_TABLE")),
        RasterDriver::MBTiles | RasterDriver::PMTiles => {
            let ds = RasterDataset { raster: r.clone(), driver, has_nodata: nodata.is_some() };
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
