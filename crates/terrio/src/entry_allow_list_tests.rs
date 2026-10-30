//! The public entry points that open data, each under the process-wide allow-list: with no list
//! and with one that has the driver the entry point opens a dataset with, the dataset opens; with
//! one that leaves the driver out, it is refused before that driver reads it, whatever the entry
//! point (multidimensional, TIFF readers, tile archives, sparse readers, streams, `info_path`, VRT
//! descriptions, GeoPackage readers and editors, point clouds). The list is set with
//! `TERRIO_ALLOWED_DRIVERS` on the test's thread, which the process-wide list joins;
//! `tests/entry_allow_list.rs` sets process-wide lists of one kind.

#[cfg(feature = "jp2")]
use crate::formats::RasterDriver;
#[cfg(feature = "pointcloud")]
use crate::pointcloud::{Dimension, PointCloud, PointWriteOptions, Values};
use crate::{DataType, RasterDataset, RasterWriteOptions, Result, VectorOpenOptions, VectorWriteOptions};
use std::collections::HashMap;
use std::path::Path;
use wbraster::raster::RasterData;
use wbvector::{Feature, FieldDef, FieldType, FieldValue, Geometry, Layer};

/// `Ok(())`, or the error's message.
type Outcome = std::result::Result<(), String>;

/// An entry point under test, opening its dataset.
type Opener<'a> = &'a dyn Fn() -> Outcome;

/// An entry point under test, reading the text it is given.
type Parser<'a> = &'a dyn Fn(&str) -> Outcome;

/// `f` under `TERRIO_ALLOWED_DRIVERS=drivers`, or under no list.
fn under<T>(drivers: Option<&str>, f: impl FnOnce() -> T) -> T {
    if let Some(list) = drivers {
        crate::config::push_scope(HashMap::from([("TERRIO_ALLOWED_DRIVERS".to_string(), Some(list.to_string()))]));
    }
    let out = f();
    if drivers.is_some() {
        crate::config::pop_scope();
    }
    out
}

/// `Ok(())`, or the result's error message.
fn outcome<T>(result: Result<T>) -> Outcome {
    result.map(|_| ()).map_err(|e| e.to_string())
}

/// The refusal of a dataset `driver` is about to open: "'shown' holds driver data, ...".
fn holds(shown: &str, driver: &str, allowed: &str) -> Outcome {
    Err(format!("'{shown}' holds {driver} data, and the {driver} driver is not allowed (allowed: {allowed})"))
}

/// The refusal of a dataset by the drivers its name points to.
fn named(shown: &str, drivers: &str, allowed: &str) -> Outcome {
    Err(format!("'{shown}': the {drivers} driver is not allowed (allowed: {allowed})"))
}

/// A 5 x 3 one-band raster in UTM zone 10N.
fn raster() -> RasterDataset {
    let (cols, rows) = (5, 3);
    let data = RasterData::from_f64_vec(DataType::U8, (0..cols * rows).map(|v| v as f64).collect());
    let cfg = wbraster::RasterConfig {
        cols,
        rows,
        bands: 1,
        nodata: -1.0,
        data_type: DataType::U8,
        crs: crate::crs::SpatialRef::from_epsg(32610).unwrap().to_crs_info(),
        ..Default::default()
    };
    let mut r = crate::Raster::from_data_native(cfg, data).unwrap();
    r.x_min = 500000.0;
    r.y_min = 4000000.0;
    r.cell_size_x = 30.0;
    r.cell_size_y = 30.0;
    RasterDataset::new(r, true)
}

/// A layer of two points in WGS 84.
fn points() -> Layer {
    let mut l = Layer::new("places").with_epsg(4326);
    l.add_field(FieldDef::new("n", FieldType::Integer));
    for (i, (x, y)) in [(10.5, 47.5), (11.5, 46.5)].into_iter().enumerate() {
        l.push(Feature { fid: i as u64 + 1, geometry: Some(Geometry::point(x, y)), attributes: vec![FieldValue::Integer(i as i64)] });
    }
    l
}

/// Writes `ds` at `dir/name` and returns the path.
fn write_raster(dir: &Path, name: &str, ds: &RasterDataset) -> String {
    let path = dir.join(name).to_str().unwrap().to_string();
    crate::write_raster_dataset(ds, &path, &RasterWriteOptions::default()).unwrap();
    path
}

/// A file of `bytes` caller-fetched in full (see [`crate::sparse`]).
fn sparse(bytes: &[u8]) -> crate::sparse::SparseFile {
    let file = crate::sparse::SparseFile::new(bytes.len() as u64);
    file.insert(0, bytes);
    file
}

/// The multidimensional API opens netCDF, HDF5 and Zarr only with the driver it opens them with
/// allowed: netCDF for a classic file or an HDF5 one under netCDF's names, HDF5 for other HDF5
/// files and HDF5 bytes, Zarr for a store by its name or layout (a directory not named `.zarr`
/// too, with Zarr allowed) and for a store in memory.
#[cfg(all(feature = "netcdf-hdf5", feature = "zarr"))]
#[test]
fn the_multidimensional_api_opens_only_with_its_driver_allowed() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let classic = data.join("rotated_pole/eur11.nc");
    let classic = classic.to_str().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let hdf5 = dir.path().join("axes.h5");
    std::fs::copy(data.join("netcdf_bands/axes.nc"), &hdf5).unwrap();
    let hdf5 = hdf5.to_str().unwrap();
    let netcdf4 = data.join("netcdf_bands/axes.nc");
    let netcdf4 = netcdf4.to_str().unwrap();
    let open = |list: Option<&str>, name: &str| under(list, || crate::multidim::open(name).map(|d| d.driver));
    assert_eq!(open(None, classic).as_deref().map_err(ToString::to_string), Ok("netCDF"));
    assert_eq!(outcome(open(Some("GTiff"), classic)), holds(classic, "netCDF", "GTIFF"));
    assert_eq!(outcome(open(Some("HDF5"), classic)), holds(classic, "netCDF", "HDF5"));
    assert_eq!(outcome(open(Some("GTiff,netCDF"), classic)), Ok(()));
    assert_eq!(outcome(open(Some("HDF5"), netcdf4)), holds(netcdf4, "netCDF", "HDF5"));
    assert_eq!(outcome(open(Some("netCDF"), netcdf4)), Ok(()));
    assert_eq!(open(None, hdf5).as_deref().map_err(ToString::to_string), Ok("HDF5"));
    assert_eq!(outcome(open(Some("netCDF"), hdf5)), holds(hdf5, "HDF5", "NETCDF"));
    assert_eq!(outcome(open(Some("HDF5"), hdf5)), Ok(()));

    let bytes = |list: Option<&str>, path: &str| {
        let b = std::fs::read(path).unwrap();
        under(list, || crate::multidim::open_bytes(&b).map(|d| d.driver))
    };
    assert_eq!(outcome(bytes(None, classic)), Ok(()));
    assert_eq!(outcome(bytes(Some("HDF5"), classic)), holds("a file in memory", "netCDF", "HDF5"));
    assert_eq!(outcome(bytes(Some("netCDF"), classic)), Ok(()));
    assert_eq!(outcome(bytes(Some("netCDF"), netcdf4)), holds("a file in memory", "HDF5", "NETCDF"));
    assert_eq!(outcome(bytes(Some("HDF5"), netcdf4)), Ok(()));

    let store = write_raster(dir.path(), "s.zarr", &raster());
    let unnamed = dir.path().join("store");
    std::fs::create_dir(&unnamed).unwrap();
    let files = crate::multidim::open(&store).unwrap().to_zarr_store().unwrap();
    for (name, bytes) in &files {
        let path = unnamed.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let unnamed = unnamed.to_str().unwrap();
    for path in [store.as_str(), unnamed] {
        assert_eq!(open(None, path).as_deref().map_err(ToString::to_string), Ok("Zarr"), "{path}");
        assert_eq!(outcome(open(Some("GTiff"), path)), holds(path, "Zarr", "GTIFF"));
        assert_eq!(outcome(open(Some("Zarr"), path)), Ok(()), "{path}");
    }
    let memory = |list: Option<&str>| under(list, || crate::multidim::open_zarr_store(files.clone()).map(|d| d.driver));
    assert_eq!(memory(None).as_deref().map_err(ToString::to_string), Ok("Zarr"));
    assert_eq!(outcome(memory(Some("GTiff"))), holds("the Zarr store", "Zarr", "GTIFF"));
    assert_eq!(outcome(memory(Some("Zarr"))), Ok(()));
}

/// The readers of a TIFF that are not `LazyRaster::open` (headers by path, bytes or caller-fetched
/// ranges; whole reads with decoder limits; GCPs, RPCs, COG validation, overview deletion) read a
/// TIFF only with GTiff allowed, whatever its name: a GeoTIFF named `x.tif` opened under a list
/// of PNG alone through `LazyRaster::open_tiff`.
#[test]
fn the_readers_of_a_tiff_read_it_only_with_gtiff_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let tif = write_raster(dir.path(), "x.tif", &raster());
    let tif = tif.as_str();
    let bytes = std::fs::read(tif).unwrap();
    let gdal = crate::CogValidationOptions { profile: crate::CogValidationProfile::Gdal, full_check: Some(true), ..Default::default() };
    let readers: [(&str, Opener); 12] = [
        ("open_tiff", &|| outcome(crate::LazyRaster::open_tiff(tif).map(|r| r.expect("a TIFF")))),
        ("open_tiff_with_limits", &|| outcome(crate::LazyRaster::open_tiff_with_limits(tif, Default::default()).map(|r| r.unwrap()))),
        ("open_raster_with_tiff_limits", &|| outcome(crate::open_raster_with_tiff_limits(tif, Default::default()))),
        ("read_gcps", &|| outcome(crate::read_gcps(tif))),
        ("read_rpc", &|| outcome(crate::ops::rpc::read_rpc(tif))),
        ("validate_cog", &|| outcome(crate::validate_cog(tif, false))),
        ("validate_cog_with_options", &|| outcome(crate::validate_cog_with_options(tif, &gdal))),
        ("validate_cog_bytes", &|| outcome(crate::validate_cog_bytes(&bytes, &gdal))),
        ("delete_overviews", &|| outcome(crate::ops::overview::delete_overviews(tif, &Default::default()))),
        ("from_tiff_bytes", &|| outcome(crate::LazyRaster::from_tiff_bytes(bytes.clone()).map(|r| r.unwrap()))),
        ("from_sparse", &|| outcome(crate::LazyRaster::from_sparse(sparse(&bytes)).map(|r| r.unwrap()))),
        ("delete_overviews_from_bytes", &|| outcome(crate::ops::overview::delete_overviews_from_bytes(&bytes, &Default::default()))),
    ];
    for (name, read) in readers {
        let shown = match name {
            "from_tiff_bytes" | "delete_overviews_from_bytes" | "validate_cog_bytes" => "a GeoTIFF in memory",
            "from_sparse" => "a remote GeoTIFF",
            _ => tif,
        };
        assert_eq!(under(None, read), Ok(()), "{name}, no list");
        assert_eq!(under(Some("PNG"), read), holds(shown, "GTiff", "PNG"), "{name}");
        assert_eq!(under(Some("PNG,GTiff"), read), Ok(()), "{name}");
    }
    // An `.ovr` sidecar, a TIFF too, is read to be deleted only with GTiff allowed; this one holds
    // no overviews, which the reader says once it has read it.
    let ovr = format!("{tif}.ovr");
    std::fs::copy(tif, &ovr).unwrap();
    let external = crate::ops::overview::DeleteOptions { external: true, ..Default::default() };
    let delete = |list| under(list, || outcome(crate::ops::overview::delete_overviews(tif, &external)));
    assert_eq!(delete(Some("PNG")), holds(&ovr, "GTiff", "PNG"));
    assert_eq!(delete(Some("GTiff")), Err(format!("refusing to remove '{ovr}': not an overview sidecar")));
}

/// Tile archives opened by the tiles API (by path, bytes or caller-fetched ranges, as datasets or
/// to be updated) open only with their driver allowed: MBTiles by its name or its SQLite header,
/// PMTiles otherwise. Terrio's own opens of an archive judged it by its name before.
#[cfg(feature = "tiles")]
#[test]
fn tile_archives_open_only_with_their_driver_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let layer = points();
    let mut o = VectorWriteOptions::default();
    o.tiles.max_zoom = Some(2);
    let (mb, pm) = (dir.path().join("v.mbtiles"), dir.path().join("v.pmtiles"));
    let (mb, pm) = (mb.to_str().unwrap(), pm.to_str().unwrap());
    for path in [mb, pm] {
        crate::write_vector(std::slice::from_ref(&layer), path, &o).unwrap();
    }
    for (path, driver) in [(mb, "MBTiles"), (pm, "PMTiles")] {
        let bytes = std::fs::read(path).unwrap();
        let opens: [(&str, Opener); 4] = [
            ("open", &|| outcome(crate::tiles::TileArchive::open(path))),
            ("open_dataset", &|| outcome(crate::tiles::open_dataset(path, &VectorOpenOptions::default()))),
            ("from_bytes", &|| outcome(crate::tiles::TileArchive::from_bytes(bytes.clone()))),
            ("update_vector_bytes", &|| {
                outcome(crate::tiles::update_vector_bytes(bytes.clone(), std::slice::from_ref(&layer), &o.tiles, false))
            }),
        ];
        for (name, open) in opens {
            let shown = if name.starts_with("open") { path } else { "an archive in memory" };
            assert_eq!(under(None, open), Ok(()), "{name} {driver}, no list");
            assert_eq!(under(Some("GeoJSON"), open), holds(shown, driver, "GEOJSON"), "{name} {driver}");
            assert_eq!(under(Some(&format!("GeoJSON,{driver}")), open), Ok(()), "{name} {driver}");
        }
        // Terrio's own open judges the name and first bytes, which `sniff` names GPKG for MBTiles.
        let own = under(Some(&format!("{driver},GPKG")), || outcome(crate::open_vector(path, &VectorOpenOptions::default())));
        assert_eq!(own, Ok(()), "{driver}");
    }
    let remote = |list| under(list, || outcome(crate::tiles::TileArchive::from_sparse(sparse(&std::fs::read(pm).unwrap()))));
    assert_eq!(remote(None), Ok(()));
    assert_eq!(remote(Some("GeoJSON")), holds("a remote archive", "PMTiles", "GEOJSON"));
    assert_eq!(remote(Some("PMTiles")), Ok(()));
}

/// A 300 x 200 one-band Byte raster on zoom level 10's Web Mercator pixel grid: a tile archive is
/// written from it a tile row at a time, read by window.
#[cfg(feature = "tiles")]
fn on_the_tile_grid() -> RasterDataset {
    let res = 2.0 * 6378137.0 * std::f64::consts::PI / 256.0 / 1024.0;
    let cfg = wbraster::RasterConfig {
        cols: 300,
        rows: 200,
        bands: 1,
        x_min: 1000.0 * res,
        y_min: 2000.0 * res,
        cell_size: res,
        data_type: DataType::U8,
        crs: crate::crs::SpatialRef::from_epsg(3857).unwrap().to_crs_info(),
        ..Default::default()
    };
    let data = RasterData::from_f64_vec(DataType::U8, (0..300 * 200).map(|i| (i % 251) as f64).collect());
    RasterDataset::new(crate::Raster::from_data_native(cfg, data).unwrap(), false)
}

/// Tile archives built a tile row at a time from a raster read by window (`LazyRaster::write` to
/// PMTiles or MBTiles, which `Raster.write` of a lazy raster and the Python and CLI `convert`
/// reach) read the raster `open_raster_lazy` opened, which judged its driver: under a list
/// without GTiff nothing is written. The archive written is not judged, as no writer is: GTiff
/// alone writes both; the archive then opens only with its driver allowed.
#[cfg(feature = "tiles")]
#[test]
fn tile_archives_are_written_from_rasters_opened_only_with_their_driver_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let tif = write_raster(dir.path(), "x.tif", &on_the_tile_grid());
    let options = RasterWriteOptions { creation_options: vec![("MAXZOOM".into(), "10".into())], ..Default::default() };
    for (name, driver) in [("x.pmtiles", "PMTiles"), ("x.mbtiles", "MBTiles")] {
        let out = dir.path().join(name);
        let out = out.to_str().unwrap();
        let write = |list| under(list, || outcome(crate::open_raster_lazy(&tif).and_then(|mut r| r.write(out, &options))));
        assert_eq!(write(Some("PNG")), named(&tif, "GTIFF/COG", "PNG"), "{driver}");
        assert!(!Path::new(out).exists(), "{driver}");
        assert_eq!(write(Some("GTiff")), Ok(()), "{driver}");
        let archive = |list| under(list, || outcome(crate::tiles::TileArchive::open(out)));
        assert_eq!(archive(Some("GTiff")), holds(out, driver, "GTIFF"), "{driver}");
        assert_eq!(archive(Some(driver)), Ok(()), "{driver}");
    }
}

/// The readers of FlatGeobuf and GeoParquet files whose bytes the caller fetches read them only
/// with their driver allowed.
#[cfg(all(feature = "flatgeobuf", feature = "parquet"))]
#[test]
fn caller_fetched_vector_files_are_read_only_with_their_driver_allowed() {
    let layer = points();
    let fgb = sparse(&crate::memory::write_vector_bytes(std::slice::from_ref(&layer), "x.fgb", &Default::default()).unwrap());
    let parquet = sparse(&crate::memory::write_vector_bytes(std::slice::from_ref(&layer), "x.parquet", &Default::default()).unwrap());
    let reads: [(&str, &str, &str, Opener); 4] = [
        ("read_flatgeobuf_sparse", "x.fgb", "FlatGeobuf", &|| outcome(crate::vector::read_flatgeobuf_sparse(&fgb, "x.fgb", None))),
        ("read_flatgeobuf_header_sparse", "x.fgb", "FlatGeobuf", &|| outcome(crate::vector::read_flatgeobuf_header_sparse(&fgb, "x.fgb"))),
        ("geoparquet_row_groups_sparse", "a remote GeoParquet file", "Parquet", &|| {
            outcome(crate::vector::geoparquet_row_groups_sparse(&parquet))
        }),
        ("read_geoparquet_sparse", "x.parquet", "Parquet", &|| {
            outcome(crate::vector::read_geoparquet_sparse(&parquet, "x.parquet", None, None))
        }),
    ];
    for (name, shown, driver, read) in reads {
        assert_eq!(under(None, read), Ok(()), "{name}, no list");
        assert_eq!(under(Some("GeoJSON"), read), holds(shown, driver, "GEOJSON"), "{name}");
        assert_eq!(under(Some(&format!("GeoJSON,{driver}")), read), Ok(()), "{name}");
    }
}

/// A minimal OpenStreetMap PBF file: its header alone, which says the file is sorted by type then
/// id, so that a conversion reads it a block at a time.
fn sorted_pbf() -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push(v as u8 | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    fn field(out: &mut Vec<u8>, number: u64, bytes: &[u8]) {
        varint(out, (number << 3) | 2);
        varint(out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }
    let mut header = Vec::new();
    field(&mut header, 4, b"OsmSchema-V0.6");
    field(&mut header, 4, b"DenseNodes");
    field(&mut header, 5, b"Sort.Type_then_ID");
    let mut blob = Vec::new();
    field(&mut blob, 1, &header);
    varint(&mut blob, 2 << 3);
    varint(&mut blob, header.len() as u64);
    let mut blob_header = Vec::new();
    field(&mut blob_header, 1, b"OSMHeader");
    varint(&mut blob_header, 3 << 3);
    varint(&mut blob_header, blob.len() as u64);
    let mut out = (blob_header.len() as u32).to_be_bytes().to_vec();
    out.extend(blob_header);
    out.extend(blob);
    out
}

/// Vector datasets read as streams of features, or converted by `stream::convert` (sorted
/// OpenStreetMap files by a reader of their own), are judged as `open_vector` judges them; so are
/// those whose schemas `export_schema_path` reads, those `vector_index_sources` indexes, and the
/// GeoPackages read or edited in place by the GeoPackage driver.
#[cfg(all(feature = "sqlite", feature = "osm"))]
#[test]
fn vector_streams_indexes_and_geopackages_are_read_only_with_their_driver_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let layer = points();
    let (geojson, gpkg) = (dir.path().join("x.geojson"), dir.path().join("x.gpkg"));
    let (geojson, gpkg) = (geojson.to_str().unwrap(), gpkg.to_str().unwrap());
    for path in [geojson, gpkg] {
        crate::write_vector(std::slice::from_ref(&layer), path, &VectorWriteOptions::default()).unwrap();
    }
    let stream = |list, path: &str| under(list, || outcome(crate::stream::open_layers(path, &VectorOpenOptions::default(), 10)));
    assert_eq!(stream(None, geojson), Ok(()));
    assert_eq!(stream(Some("GPKG"), geojson), named(geojson, "GEOJSON", "GPKG"));
    assert_eq!(stream(Some("GeoJSON"), geojson), Ok(()));
    assert_eq!(stream(Some("GeoJSON"), gpkg), named(gpkg, "GPKG", "GEOJSON"));

    let pbf = dir.path().join("x.osm.pbf");
    std::fs::write(&pbf, sorted_pbf()).unwrap();
    let pbf = pbf.to_str().unwrap();
    let convert = |list, n: usize| {
        let out = dir.path().join(format!("osm{n}.gpkg"));
        under(list, || outcome(crate::stream::convert(pbf, out.to_str().unwrap(), &Default::default())))
    };
    assert_eq!(convert(None, 0), Ok(()));
    assert_eq!(convert(Some("GeoJSON"), 1), named(pbf, "OSM", "GEOJSON"));
    assert_eq!(convert(Some("OSM"), 2), Ok(()));

    let gpkg_readers: [(&str, bool, Opener); 5] = [
        ("read_gpkg_metadata", false, &|| outcome(crate::ops::vector_edit::read_gpkg_metadata(gpkg))),
        ("rename_layers_in_place", false, &|| outcome(crate::ops::vector_edit::rename_layers_in_place(gpkg, &Default::default()))),
        ("edit_in_place", false, &|| outcome(crate::ops::vector_edit::edit_in_place(gpkg, &Default::default()))),
        ("export_schema_path", true, &|| outcome(crate::ops::vector_schema::export_schema_path(gpkg, &[]))),
        ("vector_index_sources", true, &|| outcome(crate::ops::tile_index::vector_index_sources(gpkg))),
    ];
    for (name, by_name, read) in gpkg_readers {
        assert_eq!(under(None, read), Ok(()), "{name}, no list");
        let refusal = if by_name { named(gpkg, "GPKG", "GEOJSON") } else { holds(gpkg, "GPKG", "GEOJSON") };
        assert_eq!(under(Some("GeoJSON"), read), refusal, "{name}");
        assert_eq!(under(Some("GPKG"), read), Ok(()), "{name}");
    }
}

/// `info_path` judges a dataset as `crate::open` does, by its name and first bytes: an ER Mapper
/// raster, found by its name by a reader that judges nothing itself, is refused under a list
/// without ERS. A JPEG 2000 file named for no raster driver is found by its content by the lazy
/// open, which judges it: under a point cloud's name, `info_path` leaves it to that open.
#[cfg(feature = "jp2")]
#[test]
fn info_and_lazy_opens_judge_what_they_open() {
    let dir = tempfile::tempdir().unwrap();
    let ers = write_raster(dir.path(), "x.ers", &raster());
    let ers = ers.as_str();
    let info = |list| under(list, || outcome(crate::info::info_path(ers, &Default::default())));
    assert_eq!(info(None), Ok(()));
    assert_eq!(info(Some("GTiff")), named(ers, "ERS", "GTIFF"));
    assert_eq!(info(Some("ERS")), Ok(()));

    let jp2 = write_raster(dir.path(), "x.jp2", &raster());
    let las = dir.path().join("x.las");
    std::fs::copy(&jp2, &las).unwrap();
    let las = las.to_str().unwrap();
    let lazy = |list| {
        under(list, || match crate::drivers::open_lazy(las) {
            Some(r) => r.map(|r| r.driver()).map_err(|e| e.to_string()),
            None => Err("not opened".to_string()),
        })
    };
    assert_eq!(lazy(None), Ok(RasterDriver::JP2));
    assert_eq!(lazy(Some("GTiff")), holds(las, "JP2", "GTIFF").map(|()| RasterDriver::JP2));
    assert_eq!(lazy(Some("JP2OpenJPEG")), Ok(RasterDriver::JP2));
}

/// VRT descriptions, from a file or from memory (GDAL's pansharpened ones too), are read only with
/// the VRT driver allowed, before any of their XML is parsed.
#[test]
fn vrt_descriptions_are_read_only_with_vrt_allowed() {
    let dir = tempfile::tempdir().unwrap();
    write_raster(dir.path(), "x.tif", &raster());
    let xml = "<VRTDataset rasterXSize=\"5\" rasterYSize=\"3\"><VRTRasterBand dataType=\"Byte\" band=\"1\"><SimpleSource>\
               <SourceFilename relativeToVRT=\"1\">x.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>";
    let vrt = dir.path().join("x.vrt");
    std::fs::write(&vrt, xml).unwrap();
    let vrt = vrt.to_str().unwrap();
    let files: [(&str, Opener); 3] = [
        ("open_virtual_raster", &|| outcome(crate::ops::vrt::open_virtual_raster(vrt))),
        ("read_vrt", &|| outcome(crate::ops::vrt::read_vrt(vrt))),
        ("VrtDataset::open", &|| outcome(crate::ops::vrt::VrtDataset::open(vrt))),
    ];
    for (name, open) in files {
        assert_eq!(under(None, open), Ok(()), "{name}, no list");
        assert_eq!(under(Some("GTiff"), open), holds(vrt, "VRT", "GTIFF"), "{name}");
        assert_eq!(under(Some("GTiff,VRT"), open), Ok(()), "{name}");
    }
    let pansharpen = "<VRTDataset subClass=\"VRTPansharpenedDataset\"><PansharpeningOptions><PanchroBand>\
                      <SourceFilename>pan.tif</SourceFilename><SourceBand>1</SourceBand></PanchroBand><SpectralBand dstBand=\"1\">\
                      <SourceFilename>ms.tif</SourceFilename><SourceBand>1</SourceBand></SpectralBand></PansharpeningOptions></VRTDataset>";
    let unclosed = "<VRTDataset rasterXSize=\"5\"";
    let texts: [(&str, Parser, &str); 3] = [
        ("VirtualRaster::from_xml", &|t| outcome(crate::ops::vrt::VirtualRaster::from_xml(t)), xml),
        ("VrtDataset::from_xml", &|t| outcome(crate::ops::vrt::VrtDataset::from_xml(t)), xml),
        ("PansharpenVrt::from_xml", &|t| outcome(crate::ops::pansharpen::vrt::PansharpenVrt::from_xml(t)), pansharpen),
    ];
    for (name, parse, text) in texts {
        assert_eq!(under(None, || parse(text)), Ok(()), "{name}, no list");
        assert_eq!(under(Some("VRT"), || parse(text)), Ok(()), "{name}");
        // Refused before the XML is parsed: a description that does not parse is refused alike.
        for text in [text, unclosed] {
            assert_eq!(under(Some("GTiff"), || parse(text)), holds("a VRT description", "VRT", "GTIFF"), "{name}");
        }
    }
}

/// A point cloud of three points.
#[cfg(feature = "pointcloud")]
fn cloud() -> PointCloud {
    let dims = vec![
        Dimension::new("x", Values::F64(vec![0.0, 1.0, 2.0])),
        Dimension::new("y", Values::F64(vec![0.0, 1.0, 0.0])),
        Dimension::new("z", Values::F64(vec![5.0, 6.0, 7.0])),
    ];
    PointCloud::new(dims, Default::default())
}

/// Point clouds have no allow-list of their own kind but `TERRIO_ALLOWED_DRIVERS`, which applies to
/// every kind: their readers open a file only with the driver it is read with allowed: EPT, PLY and
/// E57 by their names (or, from memory, their signatures), LAS, LAZ and COPC by their header, which
/// is read only if the list has one of the three.
#[cfg(feature = "pointcloud")]
#[test]
fn point_clouds_are_read_only_with_their_driver_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    for name in ["x.las", "x.laz", "x.copc.laz", "x.ply", "x.e57", "ept/ept.json"] {
        crate::pointcloud::write_points(&cloud(), &path(name), &PointWriteOptions::default()).unwrap();
    }
    let open = |list, name: &str| under(list, || outcome(crate::pointcloud::open_points(&path(name))));
    for (name, driver, other) in [
        ("x.las", "LAS", "LAZ"),
        ("x.laz", "LAZ", "LAS"),
        ("x.copc.laz", "COPC", "LAZ"),
        ("x.ply", "PLY", "LAS"),
        ("x.e57", "E57", "LAS"),
        ("ept/ept.json", "EPT", "LAS"),
    ] {
        assert_eq!(open(None, name), Ok(()), "{name}, no list");
        assert_eq!(open(Some(other), name), holds(&path(name), driver, other), "{name}");
        assert_eq!(open(Some(driver), name), Ok(()), "{name}");
    }
    // Under a list without the LAS family, a LAS header is not read: a damaged one is refused alike.
    let damaged = path("damaged.las");
    std::fs::write(&damaged, b"LASF and nothing a LAS header holds").unwrap();
    assert_eq!(open(Some("GTiff"), "damaged.las"), holds(&damaged, "LAS", "GTIFF"));
    assert!(open(Some("LAS"), "damaged.las").is_err_and(|e| !e.contains("not allowed")));

    let bytes = |list, name: &str| {
        let b = std::fs::read(path(name)).unwrap();
        under(list, || outcome(crate::pointcloud::open_points_bytes(b, name)))
    };
    for (name, driver, other) in [("x.las", "LAS", "PLY"), ("x.laz", "LAZ", "LAS"), ("x.ply", "PLY", "LAS"), ("x.e57", "E57", "LAS")] {
        assert_eq!(bytes(None, name), Ok(()), "{name} bytes, no list");
        assert_eq!(bytes(Some(other), name), holds(name, driver, other), "{name} bytes");
        assert_eq!(bytes(Some(driver), name), Ok(()), "{name} bytes");
    }
    let remote = |list| {
        let file = sparse(&std::fs::read(path("x.copc.laz")).unwrap());
        under(list, || outcome(crate::pointcloud::open_points_sparse(file, "x.copc.laz")))
    };
    assert_eq!(remote(None), Ok(()));
    assert_eq!(remote(Some("LAZ")), holds("x.copc.laz", "COPC", "LAZ"));
    assert_eq!(remote(Some("COPC")), Ok(()));
    let ept = |list| under(list, || outcome(crate::pointcloud::ept::EptReader::open(&path("ept/ept.json"))));
    assert_eq!(ept(Some("LAS")), holds(&path("ept/ept.json"), "EPT", "LAS"));
    assert_eq!(ept(Some("EPT")), Ok(()));
}

/// The GDAL backend opens a dataset with whatever driver of GDAL's recognises it: only with the
/// GDAL driver allowed, called directly, from bytes, or as the fallback for a dataset terrio's own
/// drivers cannot read (a PGM image named `x.tif`, which the name rule lets through under GTiff).
#[cfg(feature = "gdal")]
#[test]
fn the_gdal_backend_opens_only_with_gdal_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let mut pgm = b"P5\n5 3\n255\n".to_vec();
    pgm.extend(0..15u8);
    let (image, disguised) = (dir.path().join("x.pgm"), dir.path().join("x.tif"));
    for path in [&image, &disguised] {
        std::fs::write(path, &pgm).unwrap();
    }
    let (image, disguised) = (image.to_str().unwrap(), disguised.to_str().unwrap());
    let direct = |list| under(list, || outcome(crate::gdal_backend::open_raster(image)));
    assert_eq!(direct(None), Ok(()));
    assert_eq!(direct(Some("PNG")), holds(image, "GDAL", "PNG"));
    assert_eq!(direct(Some("PNG,GDAL")), Ok(()));
    let bytes = |list| under(list, || outcome(crate::gdal_backend::open_bytes(pgm.clone(), "x.pgm")));
    assert_eq!(bytes(None), Ok(()));
    assert!(bytes(Some("PNG")).is_err_and(|e| e.ends_with("holds GDAL data, and the GDAL driver is not allowed (allowed: PNG)")));
    let fallback = |list| under(list, || outcome(crate::open_raster(disguised)));
    assert_eq!(fallback(None), Ok(()));
    assert!(fallback(Some("GTiff")).is_err());
    assert_eq!(fallback(Some("GTiff,GDAL")), Ok(()));
}

/// Format identification (`inspection`) under the allow-list: GDAL
/// identifies a dataset only with its registered drivers (`GDALIdentifyDriver` skips the others),
/// so a driver the list leaves out identifies nothing, from a file's first bytes, from bytes, in a
/// directory scan or by the deeper fallback (whose opens are judged). The content check (`check`)
/// dispatches on that identification, then on the openers, so it reads a dataset only with its
/// driver allowed: a GeoPackage, whose tables it reads with the GeoPackage readers directly, is
/// refused unread, from a file and from bytes.
#[cfg(all(feature = "sqlite", feature = "pointcloud"))]
#[test]
fn identification_and_content_checks_see_only_allowed_drivers() {
    use crate::check::{check_bytes, check_path};
    use crate::inspection::{IdentifyOptions, identify, identify_bytes, identify_many};
    type Counts = std::result::Result<(u64, u64, u64), String>;
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    let tif = write_raster(dir.path(), "x.tif", &raster());
    let (geojson, las, tiles, mixed) = (path("v.geojson"), path("x.las"), path("tiles.gpkg"), path("mixed.gpkg"));
    crate::write_vector(&[points()], &geojson, &VectorWriteOptions::default()).unwrap();
    crate::pointcloud::write_points(&cloud(), &las, &PointWriteOptions::default()).unwrap();
    // A GeoPackage of one raster table, and one of a layer and a raster table.
    let gpkg = |with_layer: bool| {
        let mut builder = crate::drivers::vector::geopackage::Builder::new();
        if with_layer {
            builder.add_layer(&points()).unwrap();
        }
        crate::drivers::gpkg::add_raster(&mut builder, &raster().raster, None, "grid", None).unwrap();
        builder.finish().unwrap()
    };
    std::fs::write(&tiles, gpkg(false)).unwrap();
    std::fs::write(&mixed, gpkg(true)).unwrap();
    let name = |p: &str| Path::new(p).file_name().unwrap().to_str().unwrap().to_string();

    let found = |list, p: &str| under(list, || identify(p, false).unwrap().map(|i| i.driver));
    for (p, driver) in [(&tif, "GTiff"), (&geojson, "GeoJSON"), (&las, "LAS"), (&tiles, "GPKG")] {
        assert_eq!(found(None, p), Some(driver.to_string()), "{p}, no list");
        assert_eq!(found(Some(driver), p), Some(driver.to_string()), "{p}");
        assert_eq!(found(Some("PNG"), p), None, "{p}");
        let bytes = std::fs::read(p).unwrap();
        assert_eq!(under(Some("PNG"), || identify_bytes(&bytes, &name(p)).map(|i| i.driver)), None, "{p} as bytes");
    }
    let scan = |list| {
        let options = IdentifyOptions { recursive: true, report_failures: true, ..Default::default() };
        let records = under(list, || identify_many(&[dir.path().to_str().unwrap().to_string()], &options));
        records.into_iter().filter_map(|r| r.identification.map(|i| i.driver)).collect::<Vec<_>>()
    };
    assert_eq!(scan(None), ["GPKG", "GPKG", "GeoJSON", "LAS", "GTiff"]);
    assert_eq!(scan(Some("GTiff")), ["GTiff"]);
    // The deeper fallback opens with the openers, which judge the list.
    let raw = path("raw.dat");
    let options = RasterWriteOptions { driver: Some(crate::formats::RasterDriver::ENVI), ..Default::default() };
    crate::write_raster_dataset(&raster(), &raw, &options).unwrap();
    let deep = |list| under(list, || identify(&raw, true).unwrap().map(|i| (i.driver, i.deep)));
    assert_eq!(deep(None), Some(("ENVI".to_string(), true)));
    assert_eq!(deep(Some("GTiff")), None);

    let check = |list, p: &str| -> Counts {
        under(list, || check_path(p, &Default::default()).map(|s| (s.raster_samples, s.layers, s.features)).map_err(|e| e.to_string()))
    };
    assert_eq!(check(None, &tif), Ok((15, 0, 0)));
    assert_eq!(check(None, &tiles), Ok((15, 0, 0)));
    assert_eq!(check(None, &mixed), Ok((15, 1, 2)));
    // Refused by the opener the identified format dispatches to (by the name rule for the TIFF,
    // before GPKG's own readers for the GeoPackages), not by the check's unrecognized-input path.
    assert_eq!(check(Some("PNG"), &tif).map(|_| ()), named(&tif, "GTIFF/COG", "PNG"));
    for p in [&tiles, &mixed] {
        assert_eq!(check(Some("PNG"), p).map(|_| ()), holds(p, "GPKG", "PNG"), "{p}");
    }
    for p in [&geojson, &las] {
        let refused = check(Some("PNG"), p).unwrap_err();
        assert!(refused.contains("not allowed") && !refused.contains("unrecognized"), "{p}: {refused}");
    }
    assert_eq!(check(Some("GTiff"), &tif), Ok((15, 0, 0)));
    assert_eq!(check(Some("GPKG"), &tiles), Ok((15, 0, 0)));
    let check_upload = |list, p: &str| -> Counts {
        let bytes = std::fs::read(p).unwrap();
        under(list, || {
            check_bytes(bytes, &name(p), &Default::default()).map(|s| (s.raster_samples, s.layers, s.features)).map_err(|e| e.to_string())
        })
    };
    assert_eq!(check_upload(None, &mixed), Ok((15, 1, 2)));
    for p in [&mixed, &tiles] {
        assert_eq!(check_upload(Some("PNG"), p).map(|_| ()), holds(&name(p), "GPKG", "PNG"), "{p} as bytes");
    }
    let refused = check_upload(Some("PNG"), &tif).unwrap_err();
    assert!(refused.contains("not allowed") && !refused.contains("unrecognized"), "{refused}");
    assert_eq!(check_upload(Some("GPKG"), &mixed), Ok((15, 1, 2)));
}

/// HEC-RAS files, which the HECRAS driver recognises by their content, are read as vector layers
/// only with HECRAS allowed, and their HDF5 arrays as a raster only with HDF5 allowed.
#[cfg(feature = "hecras")]
#[test]
fn hec_ras_files_are_read_only_with_their_driver_allowed() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/hecras/results_mesh_xs.h5");
    let bytes = std::fs::read(fixture).unwrap();
    let vector = VectorOpenOptions::default();
    let readers: [(&str, Opener); 4] = [
        ("open_vector", &|| outcome(crate::open_vector(fixture, &vector))),
        ("codec::open", &|| outcome(crate::memory::codec::open(bytes.clone(), "x.h5", &vector))),
        ("HecRasHdf::open", &|| outcome(crate::hecras::HecRasHdf::open(fixture))),
        ("HecRasHdf::from_bytes", &|| outcome(crate::hecras::HecRasHdf::from_bytes(bytes.clone(), "x.h5"))),
    ];
    for (name, read) in readers {
        let shown = if name.starts_with("codec") || name.ends_with("bytes") { "x.h5" } else { fixture };
        assert_eq!(under(Some("HDF5"), read), holds(shown, "HECRAS", "HDF5"), "{name}");
        assert_eq!(under(Some("HDF5,HECRAS"), read), Ok(()), "{name}");
    }
    let raster = || outcome(crate::memory::open_raster_bytes(bytes.clone(), "x.h5"));
    assert_eq!(under(Some("PNG"), raster), named("x.h5", "HDF5", "PNG"));
}

/// The dataset file functions (inventory, and copy, rename and deletion through it) manage a
/// dataset only if its name and its content pass the lists, as `open` judges one.
#[test]
fn dataset_files_are_managed_only_with_their_driver_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let tif = write_raster(dir.path(), "x.tif", &raster());
    let list = || outcome(crate::dataset_files::file_list(&tif, None));
    assert_eq!(under(Some("PNG"), list), named(&tif, "GTIFF/COG", "PNG"));
    assert_eq!(under(Some("GTiff,COG"), list), Ok(()));
}

/// Parquet metadata is made from footers read only with the Parquet driver allowed.
#[cfg(feature = "parquet")]
#[test]
fn parquet_metadata_reads_footers_only_with_parquet_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("a.parquet").to_str().unwrap().to_string();
    crate::write_vector(&[points()], &input, &VectorWriteOptions::default()).unwrap();
    let output = dir.path().join("_metadata").to_str().unwrap().to_string();
    let create = || outcome(crate::parquet_metadata::create(std::slice::from_ref(&input), &output, &Default::default()));
    assert_eq!(under(Some("GeoJSON"), create), holds(&input, "Parquet", "GEOJSON"));
    assert_eq!(under(Some("GeoJSON,Parquet"), create), Ok(()));
}

/// GeoPackages are repacked, on disk and as bytes, only with GPKG allowed.
#[cfg(feature = "sqlite")]
#[test]
fn geopackages_are_repacked_only_with_gpkg_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = dir.path().join("x.gpkg").to_str().unwrap().to_string();
    crate::write_vector(&[points()], &gpkg, &VectorWriteOptions::default()).unwrap();
    let bytes = std::fs::read(&gpkg).unwrap();
    let repack = || outcome(crate::gpkg_maintenance::repack(&gpkg));
    let repack_bytes = || outcome(crate::gpkg_maintenance::repack_bytes(&bytes));
    assert_eq!(under(Some("GeoJSON"), repack), holds(&gpkg, "GPKG", "GEOJSON"));
    assert_eq!(under(Some("GeoJSON"), repack_bytes), holds("a GeoPackage in memory", "GPKG", "GEOJSON"));
    assert_eq!(under(Some("GPKG"), repack), Ok(()));
    assert_eq!(under(Some("GPKG"), repack_bytes), Ok(()));
}
