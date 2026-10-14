//! STAC items, collections and mosaics (`STAC:` and `STACIT:` names), and GeoZarr stores.
//!
//! The synthetic tests always run. The tests on real files need `TERRIO_TEST_DATA` (see
//! `scripts/fetch_test_data.sh`); the one that queries Earth Search is ignored by default: run it
//! with `cargo test -- --ignored`. Its expected values come from GDAL 3.12's STACIT driver.

use serde_json::json;
use std::path::{Path, PathBuf};
use terrio::info::{Info, InfoOptions};
use terrio::{DataType, Dataset, Raster, RasterConfig, RasterDataset, RasterWriteOptions, Window, WindowOptions};

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

/// A 1-band UInt16 tile of `w` x `h` 10 m pixels with its top left corner at `(x0, y1)` in UTM
/// zone 13N, its values `base + column` (0 is nodata).
fn tile(x0: f64, y1: f64, w: usize, h: usize, base: u16, zero_col: Option<usize>) -> RasterDataset {
    let cfg = RasterConfig {
        cols: w,
        rows: h,
        bands: 1,
        x_min: x0,
        y_min: y1 - h as f64 * 10.0,
        cell_size: 10.0,
        nodata: 0.0,
        data_type: DataType::U16,
        crs: wbraster::CrsInfo::from_epsg(32613),
        ..Default::default()
    };
    let values: Vec<f64> = (0..w * h).map(|i| if Some(i % w) == zero_col { 0.0 } else { (base as usize + i % w) as f64 }).collect();
    let data = wbraster::raster::RasterData::from_f64_vec(DataType::U16, values);
    RasterDataset::new(Raster::from_data_native(cfg, data).unwrap(), true)
}

fn item(id: &str, datetime: &str, x0: f64, y1: f64, w: usize, h: usize, href: &str) -> serde_json::Value {
    json!({
        "type": "Feature",
        "stac_version": "1.0.0",
        "stac_extensions": ["https://stac-extensions.github.io/projection/v1.1.0/schema.json"],
        "id": id,
        "collection": "tiles",
        "properties": {"datetime": datetime, "proj:epsg": 32613, "eo:cloud_cover": 1.5},
        "geometry": {"type": "Polygon", "coordinates": [[[-105.0, 40.0], [-104.9, 40.0], [-104.9, 40.1], [-105.0, 40.0]]]},
        "bbox": [-105.0, 40.0, -104.9, 40.1],
        "links": [],
        "assets": {
            "data": {"href": href, "type": "image/tiff; application=geotiff", "roles": ["data"],
                     "proj:shape": [h, w], "proj:transform": [10.0, 0.0, x0, 0.0, -10.0, y1, 0.0, 0.0, 1.0]},
            "thumbnail": {"href": "thumb.png", "type": "image/png", "roles": ["thumbnail"]},
            "metadata": {"href": "meta.xml", "type": "application/xml", "roles": ["metadata"]}
        }
    })
}

/// Two overlapping tiles and their items: `a` (older) at x 500000, `b` (newer) 40 m east, with a
/// column of nodata.
fn stac_fixture(dir: &Path) -> (String, String) {
    std::fs::create_dir_all(dir.join("tiles")).unwrap();
    let opts = RasterWriteOptions::default();
    terrio::write_raster_dataset(&tile(500000.0, 4400000.0, 8, 6, 100, None), dir.join("tiles/a.tif").to_str().unwrap(), &opts).unwrap();
    terrio::write_raster_dataset(&tile(500040.0, 4400000.0, 8, 6, 200, Some(1)), dir.join("tiles/b.tif").to_str().unwrap(), &opts).unwrap();
    let a = item("a", "2023-07-01T00:00:00Z", 500000.0, 4400000.0, 8, 6, "./tiles/a.tif");
    let b = item("b", "2023-07-05T00:00:00Z", 500040.0, 4400000.0, 8, 6, "tiles/b.tif");
    let item_path = dir.join("a.json");
    std::fs::write(&item_path, serde_json::to_vec(&a).unwrap()).unwrap();
    // Newest first, as STAC APIs sort them: the mosaic draws the oldest first.
    let fc = json!({"type": "FeatureCollection", "features": [b, a], "links": []});
    let fc_path = dir.join("items.json");
    std::fs::write(&fc_path, serde_json::to_vec(&fc).unwrap()).unwrap();
    (item_path.to_string_lossy().into_owned(), fc_path.to_string_lossy().into_owned())
}

#[test]
fn stac_items_and_assets() {
    let dir = tempfile::tempdir().unwrap();
    let (item_path, _) = stac_fixture(dir.path());
    // The item: its footprint, with its STAC members as fields, and its data assets as subdatasets.
    let Info::Vector(v) = terrio::info::info_path(&item_path, &InfoOptions::default()).unwrap() else { panic!() };
    assert_eq!(v.layers[0].feature_count, 1);
    let fields: Vec<&str> = v.layers[0].fields.iter().map(|f| f.name.as_str()).collect();
    for f in ["id", "datetime", "eo:cloud_cover", "stac_version", "assets.data.href", "assets.data.proj:shape", "collection"] {
        assert!(fields.contains(&f), "{f} not in {fields:?}");
    }
    let names: Vec<&str> = v.subdatasets.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, [format!("STAC:\"{item_path}\":data")]);
    // The asset, by both names, read in place.
    for name in [format!("STAC:\"{item_path}\":data"), format!("{item_path}!data")] {
        let mut r = terrio::open_raster_lazy(&name).unwrap();
        assert!(r.is_lazy());
        assert_eq!((r.width(), r.height()), (8, 6));
        let ds = r.read().unwrap();
        assert_eq!(ds.raster.get(0, 0, 3), 103.0);
        let Dataset::Raster(ds) = terrio::open(&name).unwrap() else { panic!() };
        assert_eq!(ds.geo_transform(), [500000.0, 10.0, 0.0, 4400000.0, 0.0, -10.0]);
    }
    assert!(terrio::open(&format!("STAC:\"{item_path}\":nope")).is_err());
}

#[test]
fn stacit_mosaics_items_by_datetime() {
    let dir = tempfile::tempdir().unwrap();
    let (_, fc_path) = stac_fixture(dir.path());
    // The item collection: two footprints, one mosaic per asset (thumbnails and metadata left out).
    let Info::Vector(v) = terrio::info::info_path(&fc_path, &InfoOptions::default()).unwrap() else { panic!() };
    assert_eq!(v.layers[0].feature_count, 2);
    let names: Vec<&str> = v.subdatasets.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, [format!("STACIT:\"{fc_path}\":asset=data")]);
    let name = format!("STACIT:\"{fc_path}\":asset=data");
    let mut r = terrio::open_raster_lazy(&name).unwrap();
    assert_eq!(r.driver(), terrio::formats::RasterDriver::STACIT);
    assert_eq!((r.width(), r.height(), r.nodata()), (12, 6, Some(0.0)));
    assert_eq!(r.geo_transform(), [500000.0, 10.0, 0.0, 4400000.0, 0.0, -10.0]);
    let ds = r.read().unwrap();
    // Columns 0-3: a; 4 onwards: b, except b's nodata column (5), where a shows through.
    let row: Vec<f64> = (0..12).map(|c| ds.raster.get(0, 2, c)).collect();
    assert_eq!(row, [100.0, 101.0, 102.0, 103.0, 200.0, 105.0, 202.0, 203.0, 204.0, 205.0, 206.0, 207.0]);
    // A window reads the same values.
    let w = r.read_window(Window::new(3, 1, 4, 2), &WindowOptions::default()).unwrap();
    assert_eq!((0..4).map(|c| w.raster.get(0, 1, c)).collect::<Vec<_>>(), [103.0, 200.0, 105.0, 202.0]);
    // The whole mosaic in memory, through the CRS filter.
    let Dataset::Raster(m) = terrio::open(&format!("STACIT:\"{fc_path}\":asset=data,crs=EPSG_32613")).unwrap() else { panic!() };
    assert_eq!(m.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
    assert!(terrio::open_raster(&format!("STACIT:\"{fc_path}\":asset=data,crs=EPSG_4326")).is_err());
    // As vectors: the footprints.
    let v = terrio::open_vector(&fc_path, &Default::default()).unwrap();
    assert_eq!(v.layers[0].len(), 2);
}

#[test]
#[ignore = "queries Earth Search and reads Sentinel-2 COGs on AWS"]
fn earth_search_mosaic_matches_gdal_stacit() {
    let q = "https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a&bbox=-105.05,40.5,-104.95,40.6&datetime=2023-07-05T00:00:00Z/2023-07-05T23:59:59Z&limit=10";
    // gdalinfo 'STACIT:"<q>"': 32 subdatasets, one per COG and JPEG 2000 asset.
    let Info::Vector(v) = terrio::info::info_path(&format!("STACIT:\"{q}\""), &InfoOptions::default()).unwrap() else { panic!() };
    assert_eq!(v.subdatasets.len(), 32);
    assert_eq!(v.subdatasets[0].name, format!("STACIT:\"{q}\":asset=aot"));
    let mut r = terrio::open_raster_lazy(&format!("STACIT:\"{q}\":asset=red")).unwrap();
    assert_eq!((r.width(), r.height()), (20982, 20982));
    assert_eq!(r.geo_transform(), [399960.0, 10.0, 0.0, 4600020.0, 0.0, -10.0]);
    assert_eq!(r.crs().epsg, Some(32613));
    // gdal_translate -srcwin 9980 9980 48 48: where four tiles meet.
    let w = r.read_window(Window::new(9980, 9980, 48, 48), &WindowOptions::default()).unwrap();
    let s = terrio::info::band_statistics(&w, 0).unwrap();
    assert_eq!((s.min, s.max, s.valid_count), (10504.0, 12392.0, 2304));
    assert!((s.mean - 11480.295).abs() < 1e-3, "{}", s.mean);
}

/// A GeoZarr v3 store as the conventions' examples lay it out: a multiscales group whose levels
/// are groups holding an array, `proj:code` and `spatial:dimensions` on the group, and each
/// level's `spatial:transform` in the layout.
fn write_spec_store(dir: &Path, node: bool) {
    let arr = |p: &Path, w: usize, h: usize, values: &[f32]| {
        std::fs::create_dir_all(p.join("c/0")).unwrap();
        let meta = json!({
            "zarr_format": 3, "node_type": "array", "shape": [h, w], "data_type": "float32",
            "chunk_grid": {"name": "regular", "configuration": {"chunk_shape": [h, w]}},
            "chunk_key_encoding": {"name": "default", "configuration": {"separator": "/"}},
            "fill_value": "NaN", "codecs": [{"name": "bytes", "configuration": {"endian": "little"}}],
            "attributes": {}, "dimension_names": ["y", "x"]
        });
        std::fs::write(p.join("zarr.json"), meta.to_string()).unwrap();
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(p.join("c/0/0"), bytes).unwrap();
    };
    let group = |p: &Path, attrs: serde_json::Value| {
        std::fs::create_dir_all(p).unwrap();
        std::fs::write(p.join("zarr.json"), json!({"zarr_format": 3, "node_type": "group", "attributes": attrs}).to_string()).unwrap();
    };
    let full: Vec<f32> = (0..6 * 4).map(|i| i as f32).collect();
    let half: Vec<f32> = vec![0.0, 2.0, 4.0, 12.0, 14.0, 16.0];
    let reg = if node { "node" } else { "pixel" };
    group(
        dir,
        json!({
            "zarr_conventions": [
                {"uuid": "d35379db-88df-4056-af3a-620245f8e347", "name": "multiscales"},
                {"uuid": "f17cb550-5864-4468-aeb7-f3180cfb622f", "name": "proj"},
                {"uuid": "689b58e2-cf7b-45e0-9fff-9cfc0883d6b4", "name": "spatial"}
            ],
            "multiscales": {"layout": [
                {"asset": "r10m", "transform": {"scale": [1.0, 1.0], "translation": [0.0, 0.0]},
                 "spatial:shape": [4, 6], "spatial:transform": [10.0, 0.0, 500000.0, 0.0, -10.0, 5000000.0]},
                {"asset": "r20m", "derived_from": "r10m", "transform": {"scale": [2.0, 2.0], "translation": [0.0, 0.0]},
                 "spatial:shape": [2, 3], "spatial:transform": [20.0, 0.0, 500000.0, 0.0, -20.0, 5000000.0]}
            ]},
            "proj:code": "EPSG:32633",
            "spatial:dimensions": ["y", "x"],
            "spatial:registration": reg
        }),
    );
    for (level, w, h, values) in [("r10m", 6, 4, &full), ("r20m", 3, 2, &half)] {
        group(&dir.join(level), json!({}));
        arr(&dir.join(level).join("b04"), w, h, values);
    }
}

#[test]
fn geozarr_conventions_are_read() {
    let dir = tempfile::tempdir().unwrap();
    for node in [false, true] {
        let store = dir.path().join(format!("s2_{node}.zarr"));
        write_spec_store(&store, node);
        let p = store.to_str().unwrap();
        let ds = terrio::open_raster(p).unwrap();
        assert_eq!(ds.crs().epsg, Some(32633));
        assert_eq!((ds.raster.cols, ds.raster.rows), (6, 4));
        // Node registration: the transform locates pixel centres.
        let x0 = if node { 499995.0 } else { 500000.0 };
        let y1 = if node { 5000005.0 } else { 5000000.0 };
        assert_eq!(ds.geo_transform(), [x0, 10.0, 0.0, y1, 0.0, -10.0]);
        // The 20 m level is an overview.
        let mut r = terrio::open_raster_lazy(p).unwrap();
        assert_eq!(r.overviews(), [(3, 2)]);
        let o = r.read_window(Window::new(0, 0, 3, 2), &WindowOptions { bands: vec![], overview: Some(0) }).unwrap();
        assert_eq!(o.raster.data.to_f64_vec(), [0.0, 2.0, 4.0, 12.0, 14.0, 16.0]);
        assert_eq!(o.geo_transform()[1], 20.0);
        let Info::Raster(i) = terrio::info::info_path(p, &InfoOptions::default()).unwrap() else { panic!() };
        assert_eq!(i.overviews, [[3, 2]]);
    }
}

fn sample_raster() -> RasterDataset {
    let cfg = RasterConfig {
        cols: 600,
        rows: 300,
        bands: 1,
        x_min: -110.0,
        y_min: 37.0,
        cell_size: 0.01,
        nodata: -1.0,
        data_type: DataType::F32,
        crs: wbraster::CrsInfo::from_epsg(4326),
        ..Default::default()
    };
    let data = wbraster::raster::RasterData::from_f64_vec(DataType::F32, (0..600 * 300).map(|i| (i % 977) as f64).collect());
    RasterDataset::new(Raster::from_data_native(cfg, data).unwrap(), true)
}

#[test]
fn geozarr_round_trips_with_overviews() {
    let dir = tempfile::tempdir().unwrap();
    let src = sample_raster();
    let full = src.raster.data.to_f64_vec();
    for convention in ["SPATIAL_PROJ", "CF", "GDAL"] {
        for format in ["ZARR_V2", "ZARR_V3"] {
            let p = dir.path().join(format!("{convention}_{format}.zarr"));
            let p = p.to_str().unwrap();
            let co = vec![
                ("FORMAT".to_string(), format.to_string()),
                ("GEOREFERENCING_CONVENTION".to_string(), convention.to_string()),
                ("OVERVIEW_LEVELS".to_string(), "AUTO".to_string()),
            ];
            terrio::write_raster_dataset(&src, p, &RasterWriteOptions { creation_options: co, ..Default::default() }).unwrap();
            let ds = terrio::open_raster(p).unwrap();
            assert_eq!(ds.raster.data.to_f64_vec(), full, "{p}");
            assert_eq!(ds.crs().epsg, Some(4326), "{p}");
            let gt = ds.geo_transform();
            assert!((gt[0] + 110.0).abs() < 1e-9 && (gt[3] - 40.0).abs() < 1e-9 && (gt[1] - 0.01).abs() < 1e-12, "{p}: {gt:?}");
            // AUTO: halving until 256 pixels, as gdaladdo; nearest-neighbour levels sample the
            // top left pixel of each block, as GDAL's overview builder does.
            let mut r = terrio::open_raster_lazy(p).unwrap();
            assert_eq!(r.overviews(), [(300, 150), (150, 75)], "{p}");
            let o = r.read_window(Window::new(0, 0, 150, 75), &WindowOptions { bands: vec![], overview: Some(1) }).unwrap();
            let expected: Vec<f64> = (0..75).flat_map(|y| (0..150).map(move |x| (((y * 4) * 600 + x * 4) % 977) as f64)).collect();
            assert_eq!(o.raster.data.to_f64_vec(), expected, "{p}");
        }
    }
    let attrs: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("SPATIAL_PROJ_ZARR_V3.zarr/SPATIAL_PROJ_ZARR_V3/zarr.json")).unwrap())
            .unwrap();
    let a = &attrs["attributes"];
    assert_eq!(a["proj:code"], "EPSG:4326");
    assert_eq!(a["spatial:dimensions"], json!(["Y", "X"]));
    assert_eq!(a["spatial:transform"], json!([0.01, 0.0, -110.0, 0.0, -0.01, 40.0]));
    let cf: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("CF_ZARR_V2.zarr/spatial_ref/.zattrs")).unwrap()).unwrap();
    assert!(cf["crs_wkt"].as_str().unwrap().starts_with("GEOGCS[\"WGS 84\""));
    assert_eq!(cf["GeoTransform"], "-110 0.01 0 40 0 -0.01");
}

#[test]
fn geozarr_from_real_rasters() {
    let Some(src) = data("srtm_sub.tif") else { return };
    let dir = tempfile::tempdir().unwrap();
    let ds = terrio::open_raster(&src).unwrap();
    let p = dir.path().join("srtm.zarr");
    let p = p.to_str().unwrap();
    let co = vec![
        ("FORMAT".to_string(), "ZARR_V3".to_string()),
        ("GEOREFERENCING_CONVENTION".to_string(), "SPATIAL_PROJ".to_string()),
        ("OVERVIEW_LEVELS".to_string(), "2".to_string()),
    ];
    terrio::write_raster_dataset(&ds, p, &RasterWriteOptions { creation_options: co, ..Default::default() }).unwrap();
    let back = terrio::open_raster(p).unwrap();
    assert_eq!(back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
    assert_eq!(back.geo_transform(), ds.geo_transform());
    // gdaladdo -r nearest srtm_sub.tif 2: overview checksum 63285, as `gdalinfo -checksum`
    // computes it.
    let mut r = terrio::open_raster_lazy(p).unwrap();
    let o = r.read_window(Window::new(0, 0, 250, 200), &WindowOptions { bands: vec![], overview: Some(0) }).unwrap();
    assert_eq!(gdal_checksum(&o.raster.data.to_f64_vec(), 250, 200), 63285);
}

/// GDAL's band checksum (`GDALChecksumImage()`) of integer values.
fn gdal_checksum(values: &[f64], w: usize, h: usize) -> i32 {
    const PRIMES: [i32; 11] = [7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43];
    let (mut sum, mut p) = (0i32, 0usize);
    for y in 0..h {
        for x in 0..w {
            sum = sum.wrapping_add(values[y * w + x] as i32 % PRIMES[p]);
            p = (p + 1) % 11;
        }
        sum &= 0xffff;
    }
    sum
}
