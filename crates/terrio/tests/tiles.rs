//! PMTiles, MBTiles and MVT: archives written and read back, read in place over HTTP, and
//! real-world archives (written by GDAL and Tippecanoe) checked against values from GDAL 3.12.
//!
//! The real-world files live outside the repository: set `TERRIO_TEST_DATA` to their directory
//! (see `scripts/fetch_test_data.sh`). The tests reading public archives are ignored by default:
//! run them with `cargo test -- --ignored`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use terrio::{Dataset, Window, WindowOptions};

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

/// Half the width of the Web Mercator world.
const MAX_GM: f64 = 20037508.342789244;

/// A 3-band Byte raster in EPSG:3857 at zoom 11's resolution, covering 3 x 2 tiles exactly.
fn rgb_raster() -> terrio::RasterDataset {
    let res = 2.0 * MAX_GM / 256.0 / 2048.0;
    let (w, h) = (768, 512);
    let mut v = Vec::with_capacity(3 * w * h);
    for b in 0..3 {
        for y in 0..h {
            for x in 0..w {
                v.push(((x * 3 + y * 5 + b * 70) % 251) as f64);
            }
        }
    }
    let cfg = terrio::RasterConfig {
        cols: w,
        rows: h,
        bands: 3,
        x_min: -MAX_GM + 1000.0 * 256.0 * res,
        y_min: MAX_GM - 702.0 * 256.0 * res,
        cell_size: res,
        data_type: terrio::DataType::U8,
        crs: terrio::crs::SpatialRef::from_epsg(3857).unwrap().to_crs_info(),
        ..Default::default()
    };
    let data = wbraster::raster::RasterData::from_f64_vec(terrio::DataType::U8, v);
    terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, data).unwrap(), false)
}

fn points_layer() -> terrio::Layer {
    let mut l = terrio::Layer::new("places").with_epsg(4326);
    l.add_field(terrio::FieldDef::new("name", terrio::FieldType::Text));
    l.add_field(terrio::FieldDef::new("rank", terrio::FieldType::Integer));
    for (i, (x, y)) in [(-122.4, 37.8), (2.35, 48.85), (139.7, 35.7), (-0.1, 51.5)].iter().enumerate() {
        l.add_feature(Some(terrio::Geometry::point(*x, *y)), &[("name", format!("p{i}").into()), ("rank", (i as i64).into())]).unwrap();
    }
    l
}

#[test]
fn raster_archives_round_trip() {
    let src = rgb_raster();
    let dir = tempfile::tempdir().unwrap();
    for ext in ["pmtiles", "mbtiles"] {
        let path = dir.path().join(format!("rgb.{ext}"));
        let path = path.to_str().unwrap();
        let o = terrio::RasterWriteOptions { creation_options: vec![("RESAMPLING".into(), "near".into())], ..Default::default() };
        terrio::write_raster_dataset(&src, path, &o).unwrap();
        let mut lazy = terrio::open_raster_lazy(path).unwrap();
        assert!(lazy.is_lazy());
        assert_eq!((lazy.width(), lazy.height()), (768, 512));
        // MBTiles open as RGBA, as in GDAL; PMTiles with the bands of their tiles (with alpha).
        assert_eq!(lazy.band_count(), 4);
        assert_eq!(lazy.overviews(), vec![(384, 256), (192, 128)]);
        let gt = lazy.geo_transform();
        assert!((gt[0] - src.geo_transform()[0]).abs() < 1e-6 && (gt[3] - src.geo_transform()[3]).abs() < 1e-6);
        let win = lazy.read_window(Window::new(100, 50, 300, 200), &WindowOptions { bands: vec![1, 2, 3], overview: None }).unwrap();
        for b in 0..3 {
            let got = win.raster.band_view(b).as_slice().to_vec();
            let want: Vec<f64> = (50..250).flat_map(|y| (100..400).map(move |x| ((x * 3 + y * 5 + b * 70) % 251) as f64)).collect();
            assert_eq!(got, want, "{ext} band {}", b + 1);
        }
        let ovr = lazy.read_window(Window::new(0, 0, 192, 128), &WindowOptions { bands: vec![], overview: Some(1) }).unwrap();
        assert_eq!((ovr.raster.cols, ovr.raster.rows), (192, 128));
        let Dataset::Raster(whole) = terrio::open(path).unwrap() else { panic!("not a raster") };
        assert_eq!(whole.raster.metadata.iter().find(|(k, _)| k == "ZOOM_LEVEL").map(|(_, v)| v.as_str()), Some("11"));
        let terrio::info::Info::Raster(info) = terrio::info::info_path(path, &Default::default()).unwrap() else { panic!() };
        assert_eq!(info.crs.epsg, Some(3857));
    }
}

#[test]
fn rasters_are_reprojected_with_an_alpha_band() {
    // A geographic gray raster: reprojected to EPSG:3857, with an alpha band.
    let cfg = terrio::RasterConfig {
        cols: 200,
        rows: 100,
        bands: 1,
        x_min: 10.0,
        y_min: 40.0,
        cell_size: 0.01,
        data_type: terrio::DataType::U8,
        crs: terrio::crs::SpatialRef::from_epsg(4326).unwrap().to_crs_info(),
        ..Default::default()
    };
    let data = wbraster::raster::RasterData::from_f64_vec(terrio::DataType::U8, (0..20000).map(|i| (i % 200) as f64).collect());
    let ds = terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, data).unwrap(), false);
    let bytes = terrio::memory::write_raster_bytes(&ds, "gray.pmtiles", &Default::default()).unwrap();
    let mut lazy = terrio::memory::open_raster_lazy_bytes(bytes, "gray.pmtiles").unwrap();
    assert_eq!(lazy.band_count(), 2);
    let r = lazy.read().unwrap();
    // The alpha band is opaque inside the raster.
    let alpha = r.raster.band_view(1).as_slice().to_vec();
    let opaque = alpha.iter().filter(|v| **v == 255.0).count() as f64 / alpha.len() as f64;
    assert!(opaque > 0.95, "{opaque}");
}

#[test]
fn vector_archives_round_trip() {
    let layer = points_layer();
    let dir = tempfile::tempdir().unwrap();
    for (ext, driver) in [("pmtiles", None), ("mbtiles", None), ("mvt", Some(terrio::formats::VectorDriver::MVT))] {
        let path = dir.path().join(format!("places.{ext}"));
        let path = if ext == "mvt" { dir.path().join("places_dir") } else { path };
        let path = path.to_str().unwrap();
        let mut o = terrio::VectorWriteOptions { driver, ..Default::default() };
        o.tiles.max_zoom = Some(4);
        terrio::write_vector(std::slice::from_ref(&layer), path, &o).unwrap();
        let v = terrio::open_vector(path, &Default::default()).unwrap();
        assert_eq!(v.layers.len(), 1, "{ext}");
        let l = &v.layers[0];
        assert_eq!(l.name, "places");
        assert_eq!(l.len(), 4, "{ext}");
        assert_eq!(l.crs_epsg(), Some(3857));
        let names: Vec<&str> = l.schema.fields().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["mvt_id", "name", "rank"]);
        assert_eq!(l.schema.fields()[2].field_type, terrio::FieldType::Integer);
        // Zoom 0 has all four points too.
        let mut oo = terrio::VectorOpenOptions::default();
        oo.tiles.zoom_level = Some(0);
        assert_eq!(terrio::open_vector(path, &oo).unwrap().layers[0].len(), 4);
        if ext != "mvt" {
            assert_eq!(terrio::list_layers(path).unwrap(), ["places"]);
        }
    }
}

/// Serves `data` at `/x.pmtiles` on a local port, with range requests. Returns the URL and the
/// counter of requests.
fn serve(data: Vec<u8>) -> (String, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/x.pmtiles", listener.local_addr().unwrap());
    let count = Arc::new(AtomicU64::new(0));
    let counter = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut range = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                    let (a, b) = v.trim().split_once('-').unwrap();
                    range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()));
                }
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let (a, b) = range.unwrap_or((0, data.len() - 1));
            let b = b.min(data.len() - 1);
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{}\r\nConnection: close\r\n\r\n",
                b - a + 1,
                data.len()
            );
            let _ = stream.write_all(&[head.into_bytes(), data[a..=b].to_vec()].concat());
        }
    });
    (url, count)
}

#[test]
fn remote_pmtiles_are_read_in_place() {
    // A larger raster, so that the archive is much larger than what a window needs.
    let cfg = terrio::RasterConfig {
        cols: 2048,
        rows: 2048,
        bands: 3,
        x_min: -MAX_GM + 64.0 * 256.0 * (2.0 * MAX_GM / 256.0 / 1024.0),
        y_min: MAX_GM - 72.0 * 256.0 * (2.0 * MAX_GM / 256.0 / 1024.0),
        cell_size: 2.0 * MAX_GM / 256.0 / 1024.0,
        data_type: terrio::DataType::U8,
        crs: terrio::crs::SpatialRef::from_epsg(3857).unwrap().to_crs_info(),
        ..Default::default()
    };
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let v: Vec<f64> = (0..3 * 2048 * 2048)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 256) as f64
        })
        .collect();
    let data = wbraster::raster::RasterData::from_f64_vec(terrio::DataType::U8, v);
    let ds = terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, data).unwrap(), false);
    let bytes = terrio::memory::write_raster_bytes(&ds, "big.pmtiles", &Default::default()).unwrap();
    let len = bytes.len() as u64;
    let mut local = terrio::memory::open_raster_lazy_bytes(bytes.clone(), "big.pmtiles").unwrap();
    let (url, count) = serve(bytes);
    let before = terrio::http::fetch_stats();
    let mut remote = terrio::open_raster_lazy(&url).unwrap();
    assert_eq!((remote.width(), remote.height(), remote.overviews().len()), (2048, 2048, 3));
    let win = Window::new(700, 900, 200, 100);
    let got = remote.read_window(win, &Default::default()).unwrap();
    let fetched = terrio::http::fetch_stats().bytes - before.bytes;
    assert!(fetched < len / 4, "{fetched} of {len} bytes fetched");
    assert!(count.load(Ordering::SeqCst) <= 4);
    let want = local.read_window(win, &Default::default()).unwrap();
    assert_eq!(got.raster.data.to_f64_vec(), want.raster.data.to_f64_vec());
}

#[test]
fn single_tiles_and_layers_of_gdal_archives() {
    let Some(mb) = data("countries_mvt.mbtiles") else { return };
    let pm = data("countries_mvt.pmtiles").unwrap();
    for path in [&mb, &pm] {
        let v = terrio::open_vector(path, &Default::default()).unwrap();
        let l = &v.layers[0];
        assert_eq!(l.name, "ne_110m_admin_0_countries");
        // GDAL reads 279 features (clipped to their tiles) whose areas sum to this.
        assert_eq!(l.len(), 279, "{path}");
        let area: f64 = l.features.iter().filter_map(|f| f.geometry.as_ref()).map(area_of).sum();
        assert!((area - 327882424938912.0).abs() < 1e-9 * area, "{path}: {area}");
        let pop = l.schema.field("POP_EST").unwrap();
        assert_eq!(pop.field_type, terrio::FieldType::Integer);
    }
}

/// The area of a polygonal geometry (planar).
fn area_of(g: &terrio::Geometry) -> f64 {
    let ring = |r: &wbvector::Ring| r.signed_area().abs();
    match g {
        terrio::Geometry::Polygon { exterior, interiors } => ring(exterior) - interiors.iter().map(ring).sum::<f64>(),
        terrio::Geometry::MultiPolygon(ps) => ps.iter().map(|(e, h)| ring(e) - h.iter().map(ring).sum::<f64>()).sum(),
        _ => 0.0,
    }
}

#[test]
fn gdal_raster_mbtiles() {
    let Some(path) = data("landsat.mbtiles") else { return };
    let mut lazy = terrio::open_raster_lazy(&path).unwrap();
    assert_eq!((lazy.width(), lazy.height(), lazy.band_count()), (2504, 1842, 4));
    assert_eq!(lazy.geo_transform(), [-13442580.0, 152.8740565703525, 0.0, 4670100.000000002, 0.0, -152.8740565703525]);
    assert_eq!(lazy.overviews(), vec![(1252, 921), (626, 461), (313, 230), (156, 115)]);
    let sums = |ds: &terrio::RasterDataset| -> Vec<i64> {
        (0..ds.raster.bands).map(|b| ds.raster.band_view(b).as_slice().iter().map(|v| *v as i64).sum()).collect()
    };
    // Sums from GDAL reading the same windows.
    let full = lazy.read_window(Window::new(1000, 700, 300, 200), &Default::default()).unwrap();
    assert_eq!(sums(&full), [2244937, 2307655, 2308564, 3874095]);
    let ovr = lazy.read_window(Window::new(300, 200, 64, 32), &WindowOptions { bands: vec![], overview: Some(1) }).unwrap();
    assert_eq!(sums(&ovr), [57954, 56674, 59687, 107307]);
    let all = lazy.read().unwrap();
    assert_eq!(sums(&all), [152989355, 154998730, 165818075, 271293070]);
}

#[test]
fn tippecanoe_pmtiles() {
    let Some(path) = data("cb_2018_us_zcta510_500k.pmtiles") else { return };
    let v = terrio::open_vector(&path, &Default::default()).unwrap();
    let l = &v.layers[0];
    assert_eq!(l.name, "zcta");
    // GDAL reads 37887 features: two more, of zero area, from geometries that clipping reduces
    // to lines.
    assert_eq!(l.len(), 37885);
    let fields: Vec<(&str, terrio::FieldType)> = l.schema.fields().iter().map(|f| (f.name.as_str(), f.field_type)).collect();
    use terrio::FieldType::{Integer, Text};
    assert_eq!(
        fields,
        [("mvt_id", Integer), ("AFFGEOID10", Text), ("ALAND10", Integer), ("AWATER10", Integer), ("GEOID10", Text), ("ZCTA5CE10", Text)]
    );
    // A lower zoom level, and a spatial filter (in EPSG:3857) that reads only a few tiles.
    let o = terrio::VectorOpenOptions {
        spatial_filter: Some((-8_300_000.0, 4_900_000.0, -8_200_000.0, 5_000_000.0)),
        tiles: terrio::tiles::TileOpenOptions { zoom_level: Some(5), ..Default::default() },
        ..Default::default()
    };
    let near_nyc = terrio::open_vector(&path, &o).unwrap();
    assert!(near_nyc.layers[0].len() > 100 && near_nyc.layers[0].len() < 5000, "{}", near_nyc.layers[0].len());
}

/// A 30 GB raster archive (terrain RGB) read in place: its header, then one window.
#[test]
#[ignore = "network"]
fn remote_terrarium_window() {
    let url = "https://r2-public.protomaps.com/protomaps-sample-datasets/terrarium_z9.pmtiles";
    let before = terrio::http::fetch_stats();
    let mut r = terrio::open_raster_lazy(url).unwrap();
    assert_eq!((r.width(), r.height(), r.band_count()), (131072, 130644, 3));
    let w = r.read_window(Window::new(30000, 45000, 512, 512), &Default::default()).unwrap();
    assert_eq!((w.raster.cols, w.raster.rows), (512, 512));
    assert!(terrio::http::fetch_stats().bytes - before.bytes < 4_000_000);
}

/// A small WebP archive with 512-pixel tiles.
#[test]
#[ignore = "network"]
fn remote_webp_tiles() {
    let url = "https://pmtiles.io/usgs-mt-whitney-8-15-webp-512.pmtiles";
    let terrio::info::Info::Raster(info) = terrio::info::info_path(url, &Default::default()).unwrap() else { panic!() };
    assert_eq!(info.size, [2757, 1853]);
    assert_eq!(info.bands.len(), 3);
    assert_eq!(info.overviews.len(), 7);
}
