//! Format drivers that terrio supplies itself, where the format libraries are wrong or missing.
//! Each reads and writes files the way GDAL does, so that both tools exchange them losslessly.

use crate::formats::RasterDriver;
use crate::{RasterDataset, Result};
use std::path::Path;
use wbraster::Raster;

pub(crate) mod aaigrid;
#[cfg(any(feature = "netcdf-hdf5", feature = "zarr"))]
mod cf;
pub(crate) mod common;
mod ehdr;
mod envi;
mod ers;
pub(crate) mod gdal_overview;
pub(crate) mod geokeys;
#[cfg(feature = "sqlite")]
pub(crate) mod gpkg;
#[cfg(feature = "grib")]
pub(crate) mod grib;
mod gsbg;
pub(crate) mod gtiff_update;
pub(crate) mod gtiff_write;
#[cfg(feature = "netcdf-hdf5")]
pub(crate) mod hdf5;
#[cfg(feature = "netcdf-hdf5")]
pub(crate) mod hdf5_write;
#[cfg(feature = "netcdf-hdf5")]
pub(crate) mod hdfeos;
#[cfg(feature = "hecras")]
pub mod hecras;
mod hfa;
pub(crate) mod hgt;
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
#[cfg(feature = "sqlite")]
pub(crate) mod sqlite_repack;
pub(crate) mod vector;
mod webp;
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
    if let Some(open) = raw_opener(path) {
        return Some(open(p).and_then(common::RawRaster::read));
    }
    let by_extension = RasterDriver::for_extension(path);
    // The formats found below by their content, not by the name's driver, are read only if their
    // driver is allowed, as `raw_opener`'s are.
    #[cfg(feature = "zarr")]
    if zarr::is_zarr(p) {
        return Some(check_recognised(RasterDriver::Zarr, path).and_then(|()| zarr::read(p)));
    }
    #[cfg(feature = "netcdf-hdf5")]
    if by_extension.is_none() && hdf5::is_hdf5(p) {
        return Some(check_recognised(RasterDriver::HDF5, path).and_then(|()| hdf5::read(p, path, None)));
    }
    #[cfg(feature = "netcdf-hdf5")]
    if by_extension.is_none() && netcdf::is_netcdf(p) {
        return Some(check_recognised(RasterDriver::NetCDF, path).and_then(|()| netcdf::read(p, None)));
    }
    #[cfg(feature = "grib")]
    if by_extension.is_none() && grib::is_grib(p) {
        return Some(check_recognised(RasterDriver::GRIB, path).and_then(|()| grib::read(p)));
    }
    let driver = by_extension?;
    if let Err(e) = driver.ensure_built() {
        return Some(Err(e));
    }
    Some(match driver {
        RasterDriver::AAIGrid => aaigrid::read(p),
        RasterDriver::XYZ => xyz::read(p),
        RasterDriver::PNG => png::read(p),
        RasterDriver::JPEG => png::read_jpeg(p),
        RasterDriver::WEBP => webp::read(p),
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
        #[cfg(feature = "grib")]
        RasterDriver::GRIB => grib::read(p),
        // Never `wbraster`'s Zarr reader (`ruzstd` 0.8, RUSTSEC-2026-0344): terrio's driver
        // reads the store or says why it is not one.
        #[cfg(feature = "zarr")]
        RasterDriver::Zarr => zarr::read(p),
        _ => return None,
    })
}

/// Whether [`read`] takes a lone file named `name` (in a directory of its own, as a remote file's
/// downloaded copy is) of `len` bytes whose first bytes are `head`, as far as they tell. For a name
/// of no raster driver, or of a labelled raw raster (ENVI, EHdr) whose `.hdr` is not beside it,
/// `read_own` finds the format by content alone: an HDF5 file (but under the name of another driver
/// that reads HDF5, [`hdf5::OTHER_DRIVERS`]), a netCDF file (classic, or HDF5 under one of netCDF's
/// extensions) and a GRIB file named for no raster driver, and a Surfer 6 binary grid under any
/// name. An SRTM tile is taken under any of its names by its name and size ([`hgt::is_tile`]). An
/// HDF5 signature after a user block is found only within `head`. The read refuses each of them
/// when the process-wide allow-list leaves out the driver [`lone_file_driver`] names.
#[cfg(remote)]
pub(crate) fn takes_lone_file(name: &str, head: &[u8], len: u64) -> bool {
    lone_file_driver(name, head, len).is_some()
}

/// The driver [`read`] takes a lone file with (see [`takes_lone_file`]), found by its content or,
/// for an SRTM tile, by its name and size. `None` when it does not take the file.
#[cfg(remote)]
pub(crate) fn lone_file_driver(name: &str, head: &[u8], len: u64) -> Option<RasterDriver> {
    if head.starts_with(b"DSBB") {
        return Some(RasterDriver::GSBG);
    }
    if hgt::is_tile(name, len, head) {
        return Some(RasterDriver::SRTMHGT);
    }
    if RasterDriver::for_extension(name).is_some() {
        return None;
    }
    #[cfg(feature = "netcdf-hdf5")]
    {
        let ext = crate::formats::extension(name).unwrap_or_default();
        let hdf5 = crate::formats::shows_hdf5(head);
        if hdf5 && !hdf5::OTHER_DRIVERS.contains(&ext.as_str()) {
            return Some(RasterDriver::HDF5);
        }
        if (head.len() >= 8 && head.starts_with(b"CDF")) || (hdf5 && netcdf::EXTENSIONS.contains(&ext.as_str())) {
            return Some(RasterDriver::NetCDF);
        }
    }
    #[cfg(feature = "grib")]
    if grib::is_grib_bytes(head) {
        return Some(RasterDriver::GRIB);
    }
    None
}

/// Refuses `path`, which one of terrio's drivers recognised by its content or by a file beside it
/// rather than by its name, when the process-wide raster allow-list leaves `driver` out: the list
/// holds however a format is recognised, as a de-registered GDAL driver identifies no file.
pub(crate) fn check_recognised(driver: RasterDriver, path: &str) -> Result<()> {
    crate::formats::check_driver_allowed(driver.name(), path, crate::formats::Kind::Raster)
}

/// Opens a raster of a raw layout (the samples at fixed offsets in a data file) for its header.
type RawOpener = fn(&Path) -> Result<common::RawRaster>;

/// The raw-layout driver (EHdr, ENVI, ERS, GSBG, RST, SAGA, SRTMHGT) that reads a local file,
/// found as `read_own` finds a file's driver: `None` when another driver reads it, so that a
/// raster read whole and read by window are read by the same driver. The drivers found by a file's
/// content or by the header beside it, whatever driver its name points to (ENVI, EHdr, GSBG,
/// SRTMHGT), open it only if the process-wide allow-list has them.
fn raw_opener(path: &str) -> Option<RawOpener> {
    let p = Path::new(path);
    let by_extension = RasterDriver::for_extension(path);
    // Labelled raw rasters (a data file with a `.hdr`) use many extensions; the formats with
    // extensions of their own are never sniffed as such.
    let raw_candidate = matches!(by_extension, None | Some(RasterDriver::ENVI | RasterDriver::EHdr | RasterDriver::HFA));
    if raw_candidate && envi::is_envi(p) {
        return Some(envi::open_recognised);
    }
    if raw_candidate && ehdr::is_ehdr(p) {
        return Some(ehdr::open_recognised);
    }
    #[cfg(feature = "zarr")]
    if zarr::is_zarr(p) {
        return None;
    }
    #[cfg(feature = "netcdf-hdf5")]
    if by_extension.is_none() && (hdf5::is_hdf5(p) || netcdf::is_netcdf(p)) {
        return None;
    }
    if gsbg::is_gsbg(p) {
        return Some(gsbg::open_recognised);
    }
    // SRTM tiles are known by their name and size, whatever their extension (`.raw`, `.img`...).
    // The process-wide allow-list is checked here too: a tile named for another driver (`.img`,
    // HFA's) reaches this point from an archive or a download as that driver's file.
    if hgt::is_hgt(p) {
        return Some(hgt::open_recognised);
    }
    // GRIB files, which `read_own` sniffs next, have none of these extensions.
    match by_extension? {
        RasterDriver::ERS => Some(ers::open),
        RasterDriver::SAGA => Some(saga::open),
        RasterDriver::RST => Some(rst::open),
        _ => None,
    }
}

/// Opens a raw-layout raster for its header, to be read by window: a local file one of those
/// drivers reads (see [`raw_opener`]), or a remote SRTM tile, read with range requests. `None`
/// for other files.
pub(crate) fn open_raw(path: &str) -> Option<Result<common::RawRaster>> {
    use crate::vfs::Location;
    match Location::parse(path) {
        Location::Local(p) => {
            let local = p.to_str()?;
            let open = raw_opener(local)?;
            Some(open(&p).map(|mut raw| {
                nodata::apply_pam(local, &mut raw.header);
                raw
            }))
        }
        #[cfg(remote)]
        loc @ (Location::Url(_) | Location::S3 { .. }) => hgt::open_remote(&loc),
        _ => None,
    }
}

/// Opens a raster read by window with one of terrio's own drivers (raw layouts such as SRTM tiles
/// and EHdr, ENVI or SAGA grids, read row by row; Zarr stores, local or remote, read chunk by
/// chunk; JPEG 2000 by tile and resolution level; GRIB by message). `None` when none of them
/// reads the file so.
pub(crate) fn open_lazy(path: &str) -> Option<Result<crate::LazyRaster>> {
    if let Some(result) = open_raw(path) {
        return Some(result.map(common::RawRaster::into_lazy));
    }
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
        RasterDriver::JPEG => png::write_jpeg(r, p, co("QUALITY")),
        RasterDriver::WEBP => webp::write(r, nodata, p, &options.creation_options),
        #[cfg(feature = "grib")]
        RasterDriver::GRIB => grib::write::write(r, nodata, p, options),
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
        _ => return None,
    })
}

#[cfg(all(test, remote))]
mod tests {
    use super::*;

    /// [`takes_lone_file`] says of a lone file, from its first bytes, what `read_own` does with the
    /// whole of it: it claims exactly the files the predicate takes (reading them, or failing to
    /// read them as that format), under the names a remote file's whole read judges by content.
    /// Not `.hdr`: `read_own` claims an ENVI header named so, and fails without its data file.
    #[test]
    fn a_lone_file_is_taken_as_read_own_claims_it() {
        let at = |offset: usize, magic: &[u8]| {
            let mut v = vec![0u8; offset];
            v.extend_from_slice(magic);
            v.resize(offset + 4096, 0);
            v
        };
        let contents = [
            ("HDF5", at(0, b"\x89HDF\r\n\x1a\n")),
            ("HDF5 after a user block", at(1024, b"\x89HDF\r\n\x1a\n")),
            ("netCDF", at(0, b"CDF\x01")),
            ("GRIB", at(0, b"GRIB\0\0\0\x02")),
            ("GRIB after a header", at(100, b"GRIB\0\0\0\x02")),
            ("Surfer 6 grid", at(0, b"DSBB")),
            ("TIFF", at(0, b"II*\0")),
            ("PNG", at(0, b"\x89PNG\r\n\x1a\n")),
            ("JPEG", at(0, &[0xFF, 0xD8, 0xFF, 0xE0])),
            ("JPEG 2000", at(0, b"\0\0\0\x0cjP  \r\n\x87\n")),
            ("SQLite", at(0, b"SQLite format 3\0")),
            ("STAC", br#"{"type":"Feature","stac_version":"1.0.0","id":"a"}"#.to_vec()),
            ("CSV", b"id,name\n1,a\n".to_vec()),
            ("SRTM tile", vec![0u8; 1201 * 1201 * 2]),
        ];
        let dir = tempfile::tempdir().unwrap();
        let names = ["x", "x.bin", "x.csv", "x.kea", "x.nc2", "x.gmac", "x.dat", "x.bsq", "x.bil", "x.flt", "N37W120.hgts", "N37W120.raw"];
        for (i, name) in names.iter().enumerate() {
            for (j, (what, bytes)) in contents.iter().enumerate() {
                let alone = dir.path().join(format!("{i}-{j}"));
                std::fs::create_dir(&alone).unwrap();
                let path = alone.join(name);
                std::fs::write(&path, bytes).unwrap();
                let claimed = read_own(path.to_str().unwrap()).is_some();
                assert_eq!(takes_lone_file(name, bytes, bytes.len() as u64), claimed, "{what} named {name}");
            }
        }
    }
}

/// The process-wide allow-list and the formats terrio's drivers recognise by their content or by
/// a file beside them, whatever driver the name points to: each is opened only if its driver is
/// allowed, by the open that judges the name and first bytes first (`crate::open_raster`,
/// `crate::LazyRaster::open`) and by the opener itself ([`read`], [`open_lazy`]), which other paths
/// reach. The list is set with `TERRIO_ALLOWED_DRIVERS` on the test's thread, which the
/// process-wide list joins; `tests/content_allow_list.rs` sets the process-wide list itself.
#[cfg(test)]
mod content_allow_list_tests {
    use super::*;

    fn allowing<T>(drivers: &str, f: impl FnOnce() -> T) -> T {
        let option = std::collections::HashMap::from([("TERRIO_ALLOWED_DRIVERS".to_string(), Some(drivers.to_string()))]);
        crate::config::push_scope(option);
        let out = f();
        crate::config::pop_scope();
        out
    }

    /// The driver an open used, or its error.
    fn outcome<T>(result: Option<Result<T>>, driver: impl Fn(&T) -> RasterDriver) -> std::result::Result<&'static str, String> {
        match result {
            None => Err("not claimed".into()),
            Some(r) => r.map(|d| driver(&d).name()).map_err(|e| e.to_string()),
        }
    }

    /// How `path` opens: [`read`], [`open_lazy`], `crate::open_raster` and `crate::LazyRaster::open`.
    fn opens(path: &str) -> [std::result::Result<&'static str, String>; 4] {
        [
            outcome(read(path), |d| d.driver),
            outcome(open_lazy(path), crate::LazyRaster::driver),
            outcome(Some(crate::open_raster(path)), |d| d.driver),
            outcome(Some(crate::LazyRaster::open(path)), crate::LazyRaster::driver),
        ]
    }

    /// Every open of [`opens`] with `driver`.
    fn opened(driver: &'static str) -> [std::result::Result<&'static str, String>; 4] {
        std::array::from_fn(|_| Ok(driver))
    }

    fn assert_refused(outcomes: &[std::result::Result<&'static str, String>], driver: &str, what: &str) {
        for (i, o) in outcomes.iter().enumerate() {
            // By its content ("holds GSBG data, and the GSBG driver..."), or by its name (`x.grd`).
            let refused = o.as_ref().is_err_and(|e| e.contains(&format!("the {driver} driver is not allowed (allowed: ")));
            assert!(refused, "{what}, open #{i}: {o:?}");
        }
    }

    /// A 10 x 10 Surfer 6 binary grid (`DSBB`) of the values 0 to 99.
    fn surfer_grid() -> Vec<u8> {
        let mut b = b"DSBB".to_vec();
        b.extend_from_slice(&10i16.to_le_bytes());
        b.extend_from_slice(&10i16.to_le_bytes());
        for v in [0.0f64, 9.0, 0.0, 9.0, 0.0, 99.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in 0..100u8 {
            b.extend_from_slice(&f32::from(v).to_le_bytes());
        }
        b
    }

    /// `magic` at 0, then `inner` at `offset`, then zeros to 4,096 bytes: a file whose first bytes
    /// show one format and whose reader finds another.
    fn polyglot(magic: &[u8], offset: usize, inner: &[u8]) -> Vec<u8> {
        let mut v = magic.to_vec();
        v.resize(offset, 0);
        v.extend_from_slice(inner);
        v.resize(4096, 0);
        v
    }

    /// A Surfer 6 grid under the name of another driver, or of none, was opened as GSBG under an
    /// allow-list without GSBG (`x.tif` and `x.png` under a tile server's list of GTiff, COG, JPEG,
    /// JP2OpenJPEG and PNG), as GDAL never opens it. It is refused under every name, and opened as
    /// before, with its values, once GSBG is allowed.
    #[test]
    fn a_surfer_6_grid_opens_only_with_gsbg_allowed_whatever_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let named = "GTiff,COG,JPEG,JP2OpenJPEG,PNG,SRTMHGT,HFA,XYZ";
        for name in ["x.tif", "x.png", "x.jpg", "x.hgt", "N37W120.hgt", "x.img", "x.xyz", "noext", "x.grd"] {
            let path = dir.path().join(name);
            std::fs::write(&path, surfer_grid()).unwrap();
            let path = path.to_str().unwrap();
            assert_refused(&allowing(named, || opens(path)), "GSBG", name);
            assert_eq!(allowing(&format!("{named},GSBG"), || opens(path)), opened("GSBG"), "{name}");
            let values = allowing(&format!("{named},GSBG"), || crate::open_raster(path)).unwrap().raster.data.to_f64_vec();
            assert_eq!(values.iter().sum::<f64>(), 4950.0, "{name}");
        }
    }

    /// ENVI and EHdr data files are found by the header beside them (`name.hdr`): under Imagine's
    /// `.img` (`e.img` with an ENVI header was opened as ENVI under an allow-list of HFA alone),
    /// and under each other's names, they open only with their own driver allowed.
    #[test]
    fn labelled_raw_rasters_open_only_with_their_own_driver_allowed() {
        let envi = "ENVI\nsamples = 10\nlines = 10\nbands = 1\nheader offset = 0\ndata type = 1\ninterleave = bsq\nbyte order = 0\n";
        let ehdr = "ncols 10\nnrows 10\nnbands 1\nnbits 8\nbyteorder I\nlayout bil\n";
        let dir = tempfile::tempdir().unwrap();
        for (data, header, driver, named) in [
            ("e.img", envi, "ENVI", "HFA,EHdr"),
            ("h.img", ehdr, "EHdr", "HFA,ENVI"),
            ("b.bil", envi, "ENVI", "EHdr"),
            ("d.dat", ehdr, "EHdr", "ENVI"),
        ] {
            let path = dir.path().join(data);
            std::fs::write(&path, (0..100u8).collect::<Vec<u8>>()).unwrap();
            std::fs::write(path.with_extension("hdr"), header).unwrap();
            let path = path.to_str().unwrap();
            assert_refused(&allowing(named, || opens(path)), driver, data);
            assert_eq!(allowing(&format!("{named},{driver}"), || opens(path)), opened(driver), "{data}");
            let values = allowing(&format!("{named},{driver}"), || crate::open_raster(path)).unwrap().raster.data.to_f64_vec();
            assert_eq!(values.iter().sum::<f64>(), 4950.0, "{data}");
        }
    }

    /// The formats terrio reads by their content under a name of no raster driver (HDF5, netCDF,
    /// GRIB) or under any name (a Zarr store's directory) open only with their driver allowed,
    /// where the first bytes the open judges show another format (a TIFF's signature before an
    /// HDF5 user block, or before a GRIB message within the first kilobyte), or none (a directory).
    #[cfg(all(feature = "netcdf-hdf5", feature = "grib", feature = "zarr"))]
    #[test]
    fn formats_found_by_content_open_only_with_their_driver_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path.to_str().unwrap().to_string()
        };
        // A Zarr store in a directory named for GTiff.
        let cfg = wbraster::RasterConfig {
            cols: 4,
            rows: 3,
            bands: 1,
            x_min: 10.0,
            y_min: 47.0,
            cell_size: 0.5,
            data_type: wbraster::DataType::U8,
            ..Default::default()
        };
        let raster = wbraster::Raster::from_data_native(cfg, wbraster::raster::RasterData::U8((0..12).collect())).unwrap();
        let store = dir.path().join("s.zarr");
        crate::write_raster(&raster, store.to_str().unwrap(), &Default::default()).unwrap();
        let zarr = dir.path().join("z.tif");
        std::fs::rename(&store, &zarr).unwrap();
        let zarr = zarr.to_str().unwrap();
        assert_refused(&allowing("GTiff", || opens(zarr)), "Zarr", "a Zarr store named z.tif");
        assert_eq!(allowing("GTiff,Zarr", || opens(zarr)), opened("Zarr"));
        // A GRIB message after a TIFF's signature, named for no driver; a GRIB file so named.
        let grib = at("g", &polyglot(b"II*\0", 100, b"GRIB\0\0\0\x02"));
        let real = at("r", &std::fs::read("tests/data/grib/simple.grib2").unwrap());
        assert_refused(&allowing("GTiff", || opens(&grib)), "GRIB", "GRIB after a TIFF signature");
        assert_eq!(allowing("GRIB", || opens(&real)), opened("GRIB"));
        let bytes = |b: Vec<u8>| crate::memory::open_raster_lazy_bytes(b, "g").map(|r| r.driver().name()).map_err(|e| e.to_string());
        assert_refused(&[allowing("GTiff", || bytes(std::fs::read(&grib).unwrap()))], "GRIB", "GRIB bytes");
        assert_eq!(allowing("GRIB", || bytes(std::fs::read(&real).unwrap())), Ok("GRIB"));
        // HDF5 after a 512-byte user block starting as a TIFF, named for no driver.
        let hdf5 = at("h", &polyglot(b"II*\0", 512, b"\x89HDF\r\n\x1a\n"));
        let [whole, _, public, _] = allowing("GTiff", || opens(&hdf5));
        assert_refused(&[whole, public], "HDF5", "HDF5 after a user block");
        let netcdf4 = std::fs::read("tests/data/netcdf_bands/axes.nc").unwrap();
        let n = at("n", &netcdf4);
        assert_eq!(allowing("HDF5", || crate::open_raster(&n).map(|d| d.driver.name()).map_err(|e| e.to_string())), Ok("HDF5"));
        // A netCDF-4 file (HDF5) under one of netCDF's extensions that no raster driver names.
        let nc3 = at("x.nc3", &netcdf4);
        let [whole, _, public, _] = allowing("HDF5", || opens(&nc3));
        assert_refused(&[whole, public], "netCDF", "netCDF-4 named x.nc3");
        let opened = allowing("HDF5,netCDF", || crate::open_raster(&nc3).map(|d| d.driver.name()).map_err(|e| e.to_string()));
        assert_eq!(opened, Ok("netCDF"));
    }

    /// With `TERRIO_WBRASTER_FALLBACK=YES`, `wbraster` detects a file named for no raster driver by
    /// its content (a CSF raster's, written as `.map`) where `sniff` sees nothing: the format it
    /// finds is read only if its driver is allowed.
    #[test]
    fn the_wbraster_fallback_reads_only_allowed_drivers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wbraster::RasterConfig {
            cols: 4,
            rows: 3,
            bands: 1,
            x_min: 10.0,
            y_min: 47.0,
            cell_size: 0.5,
            data_type: wbraster::DataType::F32,
            ..Default::default()
        };
        let data = wbraster::raster::RasterData::F32((0..12u8).map(f32::from).collect());
        let raster = wbraster::Raster::from_data_native(cfg, data).unwrap();
        let map = dir.path().join("p.map");
        crate::write_raster(&raster, map.to_str().unwrap(), &Default::default()).unwrap();
        let path = dir.path().join("p.geojson");
        std::fs::rename(&map, &path).unwrap();
        let path = path.to_str().unwrap();
        let open = |drivers: &str| {
            let options = std::collections::HashMap::from([
                ("TERRIO_ALLOWED_DRIVERS".to_string(), Some(drivers.to_string())),
                ("TERRIO_WBRASTER_FALLBACK".to_string(), Some("YES".to_string())),
            ]);
            crate::config::push_scope(options);
            let out = crate::open_raster(path).map(|d| d.driver.name()).map_err(|e| e.to_string());
            crate::config::pop_scope();
            out
        };
        let csf = RasterDriver::for_extension("p.map").unwrap().name();
        assert_refused(&[open("GeoJSON")], csf, "a CSF raster named p.geojson");
        assert_eq!(open(&format!("GeoJSON,{csf}")), Ok(csf));
    }
}
