//! Remote files read through `terrio::sparse::SparseFile`, as the WebAssembly build reads them
//! with `fetch`: GeoTIFFs by window and overview, PMTiles, COPC by bounds, FlatGeobuf by bounding
//! box (spatial index) and GeoParquet by row group. Each read is compared with the same read of
//! the whole file, and the bytes fetched are counted.
//!
//! Synthetic files always; real files when `TERRIO_TEST_DATA` names the directory filled by
//! `scripts/fetch_test_data.sh` (with GDAL's results for the spatial filters).

use terrio::sparse::{SparseFile, read_with};
use terrio::{VectorOpenOptions, Window, WindowOptions};

fn data_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    dir.is_dir().then_some(dir)
}

/// Reads with `read`, fetching what it misses from `bytes`. Returns the result and the bytes
/// fetched.
fn sparse<T>(bytes: &[u8], file: &SparseFile, read: impl FnMut() -> terrio::Result<T>) -> (T, u64) {
    let before = file.fetched().0;
    let out = read_with(file, 32, |o, n| Ok(bytes[o as usize..(o + n) as usize].to_vec()), read).unwrap();
    (out, file.fetched().0 - before)
}

fn raster(cols: usize, rows: usize) -> terrio::RasterDataset {
    let data = terrio::raster::RasterData::F32((0..cols * rows).map(|i| (i % 9973) as f32).collect());
    let cfg = terrio::RasterConfig {
        cols,
        rows,
        bands: 1,
        nodata: -1.0,
        data_type: terrio::DataType::F32,
        crs: terrio::crs::SpatialRef::from_epsg(32610).unwrap().to_crs_info(),
        ..Default::default()
    };
    let mut r = terrio::Raster::from_data_native(cfg, data).unwrap();
    r.x_min = 500000.0;
    r.y_min = 4000000.0;
    r.cell_size_x = 10.0;
    r.cell_size_y = 10.0;
    terrio::RasterDataset::new(r, true)
}

#[test]
fn cog_by_window_and_overview() {
    let ds = raster(2000, 1500);
    let options = terrio::RasterWriteOptions { driver: Some(terrio::formats::RasterDriver::Cog), ..Default::default() };
    let bytes = terrio::memory::write_raster_bytes(&ds, "c.tif", &options).unwrap();
    let file = SparseFile::new(bytes.len() as u64);
    let (mut r, opened) = sparse(&bytes, &file, || Ok(terrio::LazyRaster::from_sparse(file.clone())?.unwrap()));
    assert_eq!((r.width(), r.height()), (2000, 1500));
    assert!(!r.overviews().is_empty());
    let mut whole = terrio::memory::open_raster_lazy_bytes(bytes.clone(), "c.tif").unwrap();
    for (w, overview) in [(Window::new(700, 300, 300, 200), None), (Window::new(10, 10, 200, 100), Some(0))] {
        let o = WindowOptions { bands: vec![1], overview };
        let (got, fetched) = sparse(&bytes, &file, || r.read_window(w, &o));
        let want = whole.read_window(w, &o).unwrap();
        assert_eq!(got.raster.data.to_f64_vec(), want.raster.data.to_f64_vec());
        assert_eq!(got.geo_transform(), want.geo_transform());
        assert!(fetched < bytes.len() as u64 / 4, "{fetched} of {} bytes", bytes.len());
    }
    assert!(opened <= 2 * terrio::sparse::DEFAULT_BLOCK_SIZE);
}

fn countries() -> Vec<terrio::Layer> {
    let mut layer = terrio::Layer::new("squares");
    layer.add_field(terrio::FieldDef::new("id", terrio::FieldType::Integer));
    layer.set_crs_epsg(Some(4326));
    let mut fid = 0;
    for i in 0..60 {
        for j in 0..40 {
            let (x, y) = (-170.0 + i as f64 * 5.0, -80.0 + j as f64 * 4.0);
            let ring = terrio::vector::Ring::new(vec![
                terrio::vector::Coord::xy(x, y),
                terrio::vector::Coord::xy(x + 3.0, y),
                terrio::vector::Coord::xy(x + 3.0, y + 3.0),
                terrio::vector::Coord::xy(x, y + 3.0),
            ]);
            let g = terrio::Geometry::Polygon { exterior: ring, interiors: vec![] };
            layer.push(terrio::Feature { fid, geometry: Some(g), attributes: vec![terrio::FieldValue::Integer(fid as i64)] });
            fid += 1;
        }
    }
    vec![layer]
}

/// The ids of the features that intersect a rectangle, as `ogr2ogr -spat` keeps them.
fn spat(layers: &[terrio::Layer], rect: (f64, f64, f64, f64)) -> Vec<u64> {
    let o = terrio::ops::vector_filter::VectorTranslateOptions { spatial_filter: Some(rect), ..Default::default() };
    terrio::ops::vector_filter::translate(layers, &o).unwrap()[0].features.iter().map(|f| f.fid).collect()
}

#[test]
fn flatgeobuf_by_bbox() {
    let layers = countries();
    let bytes = terrio::memory::write_vector_bytes(&layers, "s.fgb", &Default::default()).unwrap();
    let whole = terrio::memory::open_vector_bytes(bytes.clone(), "s.fgb", &Default::default()).unwrap();
    let file = SparseFile::with_block_size(bytes.len() as u64, 4096);
    let rect = (10.0, 10.0, 22.0, 18.0);
    let ((info, layer), fetched) = sparse(&bytes, &file, || terrio::vector::read_flatgeobuf_sparse(&file, "s.fgb", Some(rect)));
    assert_eq!(info.feature_count, 2400);
    assert!(info.index_size > 0);
    assert_eq!(layer.name, "squares");
    let mut got = spat(std::slice::from_ref(&layer), rect);
    got.sort_unstable();
    let mut want = spat(&whole.layers, rect);
    want.sort_unstable();
    assert_eq!(got, want);
    assert!(!got.is_empty());
    assert!(fetched < bytes.len() as u64 / 4, "{fetched} of {} bytes", bytes.len());
    // Every feature without a filter.
    let ((_, all), _) = sparse(&bytes, &file, || terrio::vector::read_flatgeobuf_sparse(&file, "s.fgb", None));
    assert_eq!(all.features.len(), 2400);
    assert_eq!(all.features[7].geometry, whole.layers[0].features[7].geometry);
}

/// Offsets and lengths from a corrupt file are errors, not panics or huge allocations.
#[test]
fn corrupt_flatgeobuf_indexes_are_errors() {
    let bytes = terrio::memory::write_vector_bytes(&countries(), "s.fgb", &Default::default()).unwrap();
    let file = SparseFile::new(bytes.len() as u64);
    let ((info, _), _) = sparse(&bytes, &file, || terrio::vector::read_flatgeobuf_header_sparse(&file, "s.fgb"));
    let (start, nodes) = (info.header_size as usize, info.index_size as usize / 40);
    let read = |b: &[u8]| {
        let file = SparseFile::new(b.len() as u64);
        read_with(
            &file,
            32,
            |o, n| Ok(b[o as usize..(o + n) as usize].to_vec()),
            || terrio::vector::read_flatgeobuf_sparse(&file, "s.fgb", Some((-180.0, -90.0, 180.0, 90.0))),
        )
    };
    // The root points outside the next level.
    let mut b = bytes.clone();
    b[start + 32..start + 40].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(read(&b).unwrap_err().to_string().contains("outside its level"));
    // The leaves point past the end of the file.
    let mut b = bytes.clone();
    for k in nodes - 2400..nodes {
        let at = start + k * 40 + 32;
        b[at..at + 8].copy_from_slice(&(u64::MAX / 2).to_le_bytes());
    }
    assert!(read(&b).unwrap_err().to_string().contains("truncated"));
    // A feature's length past the end of the file.
    let mut b = bytes.clone();
    let data = start + nodes * 40;
    b[data..data + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(read(&b).unwrap_err().to_string().contains("truncated"));
}

#[test]
fn geoparquet_by_row_group() {
    let layers = countries();
    let bytes = terrio::memory::write_vector_bytes(&layers, "s.parquet", &Default::default()).unwrap();
    let file = SparseFile::with_block_size(bytes.len() as u64, 1024);
    let (groups, _) = sparse(&bytes, &file, || terrio::vector::geoparquet_row_groups_sparse(&file));
    let total: u64 = groups.iter().map(|g| g.rows).sum();
    assert_eq!(total, 2400);
    let last = groups.len() - 1;
    let (layer, _) = sparse(&bytes, &file, || terrio::vector::read_geoparquet_sparse(&file, "s.parquet", Some(&[last]), None));
    assert_eq!(layer.features.len() as u64, groups[last].rows);
    assert_eq!(layer.features[0].fid, groups[last].first_row);
    let rect = (10.0, 10.0, 22.0, 18.0);
    let (layer, _) = sparse(&bytes, &file, || terrio::vector::read_geoparquet_sparse(&file, "s.parquet", None, Some(rect)));
    let whole = terrio::memory::open_vector_bytes(bytes.clone(), "s.parquet", &Default::default()).unwrap();
    assert_eq!(spat(std::slice::from_ref(&layer), rect), spat(&whole.layers, rect));
}

#[test]
fn pmtiles_vector_tiles_by_bbox() {
    let layers = countries();
    let mut options = terrio::VectorWriteOptions::default();
    options.tiles.max_zoom = Some(4);
    let bytes = terrio::memory::write_vector_bytes(&layers, "s.pmtiles", &options).unwrap();
    let file = SparseFile::with_block_size(bytes.len() as u64, 4096);
    let rect = (1_000_000.0, 1_000_000.0, 3_000_000.0, 2_000_000.0);
    let o = VectorOpenOptions { spatial_filter: Some(rect), ..Default::default() };
    let (ds, _) = sparse(&bytes, &file, || terrio::tiles::TileArchive::from_sparse(file.clone())?.into_dataset("s.pmtiles", &o));
    let whole = terrio::tiles::TileArchive::from_bytes(bytes.clone()).unwrap().into_dataset("s.pmtiles", &o).unwrap();
    let (terrio::Dataset::Vector(a), terrio::Dataset::Vector(b)) = (ds, whole) else { panic!("not vector") };
    assert_eq!(a.layers.len(), b.layers.len());
    for (x, y) in a.layers.iter().zip(&b.layers) {
        assert_eq!(x.features.len(), y.features.len());
        assert!(x.features.iter().zip(&y.features).all(|(f, g)| f.geometry == g.geometry));
    }
}

#[test]
fn real_files() {
    let Some(dir) = data_dir() else { return };
    // A COG read by window and overview.
    // A USGS 3DEP tile (a COG) read by window and overview.
    let bytes = std::fs::read(dir.join("usgs_dem.tif")).unwrap();
    let file = SparseFile::new(bytes.len() as u64);
    let (mut r, _) = sparse(&bytes, &file, || Ok(terrio::LazyRaster::from_sparse(file.clone())?.unwrap()));
    let mut whole = terrio::memory::open_raster_lazy_bytes(bytes.clone(), "usgs_dem.tif").unwrap();
    for (w, overview) in [(Window::new(1500, 1500, 512, 512), None), (Window::new(0, 0, 400, 400), Some(1))] {
        let o = WindowOptions { bands: vec![], overview };
        let (got, fetched) = sparse(&bytes, &file, || r.read_window(w, &o));
        assert_eq!(got.raster.data.to_f64_vec(), whole.read_window(w, &o).unwrap().raster.data.to_f64_vec());
        assert!(fetched < bytes.len() as u64 / 4, "{fetched} of {} bytes", bytes.len());
        eprintln!("usgs_dem.tif: {w:?} of overview {overview:?} in {fetched} of {} bytes", bytes.len());
    }

    // FlatGeobuf by bounding box: the features GDAL keeps with -spat.
    let bytes = std::fs::read(dir.join("UScounties.fgb")).unwrap();
    let file = SparseFile::new(bytes.len() as u64);
    let rect = (-100.0, 35.0, -95.0, 40.0);
    let ((_, layer), fetched) = sparse(&bytes, &file, || terrio::vector::read_flatgeobuf_sparse(&file, "UScounties.fgb", Some(rect)));
    let mut fids = spat(std::slice::from_ref(&layer), rect);
    fids.sort_unstable();
    assert_eq!(fids.len(), 138);
    assert_eq!(fids[..5], [714, 715, 716, 717, 718]);
    assert_eq!(fids.iter().sum::<u64>(), 122191);
    assert!(fetched < bytes.len() as u64 / 4, "{fetched} of {} bytes", bytes.len());
    // The same natively, through the spatial filter of open_vector.
    let path = dir.join("UScounties.fgb");
    let o = VectorOpenOptions { spatial_filter: Some(rect), ..Default::default() };
    let native = terrio::open_vector(path.to_str().unwrap(), &o).unwrap();
    let mut n = spat(&native.layers, rect);
    n.sort_unstable();
    assert_eq!(n, fids);

    // GeoParquet in row groups (written by GDAL with ROW_GROUP_SIZE=1000).
    let path = dir.join("ne_places_rg.parquet");
    if path.exists() {
        let bytes = std::fs::read(&path).unwrap();
        let file = SparseFile::new(bytes.len() as u64);
        let (groups, _) = sparse(&bytes, &file, || terrio::vector::geoparquet_row_groups_sparse(&file));
        assert_eq!(groups.iter().map(|g| g.rows).collect::<Vec<_>>(), [1000, 1000, 1000, 1000, 1000, 1000, 1000, 342]);
        let rect = (0.0, 40.0, 20.0, 55.0);
        let (layer, fetched) =
            sparse(&bytes, &file, || terrio::vector::read_geoparquet_sparse(&file, "ne_places_rg.parquet", None, Some(rect)));
        let fids = spat(std::slice::from_ref(&layer), rect);
        assert_eq!((fids.len(), fids.iter().sum::<u64>()), (306, 921657));
        eprintln!("ne_places_rg.parquet: 306 places in {fetched} of {} bytes", bytes.len());
        let (layer, _) = sparse(&bytes, &file, || terrio::vector::read_geoparquet_sparse(&file, "ne_places_rg.parquet", Some(&[7]), None));
        assert_eq!((layer.features.len(), layer.features[0].fid), (342, 7000));
    }

    // COPC by bounds and level.
    let bytes = std::fs::read(dir.join("pointcloud/madison.copc.laz")).unwrap();
    let file = SparseFile::new(bytes.len() as u64);
    let (mut pc, _) = sparse(&bytes, &file, || terrio::pointcloud::open_points_sparse(file.clone(), "madison.copc.laz"));
    let h = pc.header().clone();
    let (cx, cy) = ((h.min[0] + h.max[0]) / 2.0, (h.min[1] + h.max[1]) / 2.0);
    let mut options = terrio::pointcloud::PointReadOptions { max_level: Some(3), ..Default::default() };
    options.filter.bounds = Some([cx - 100.0, cy - 100.0, cx + 100.0, cy + 100.0]);
    let (points, fetched) = sparse(&bytes, &file, || pc.read(&options));
    let want = terrio::pointcloud::open_points_bytes(bytes.clone(), "madison.copc.laz").unwrap().read(&options).unwrap();
    assert_eq!(points.len(), want.len());
    assert!(!points.is_empty());
    assert!(fetched < bytes.len() as u64 / 4, "{fetched} of {} bytes", bytes.len());
    eprintln!("madison.copc.laz: {} points in {fetched} of {} bytes", points.len(), bytes.len());
}
