//! Retile layout and pixel regressions checked against GDAL 3.13.3.
use std::collections::BTreeMap;
use terrio::ops::raster_retile::{self as re, RetileOptions, RetileSource};
use terrio::ops::raster_tile::FolderSink;
use terrio::raster::RasterData;
use terrio::{DataType, LazyRaster, RasterConfig, RasterDataset, Window, WindowOptions};

#[derive(Default)]
struct Collect(BTreeMap<String, Vec<u8>>);
impl FolderSink for Collect {
    fn put(&mut self, name: &str, data: &[u8]) -> terrio::Result<()> {
        self.0.insert(name.into(), data.into());
        Ok(())
    }
}
fn source(width: usize, height: usize, x: f64, values: Vec<u16>) -> LazyRaster {
    let r = terrio::Raster::from_data_native(
        RasterConfig {
            cols: width,
            rows: height,
            bands: 1,
            x_min: x,
            y_min: 0.,
            cell_size: 1.,
            cell_size_y: Some(1.),
            data_type: DataType::U16,
            nodata: 0.,
            crs: terrio::crs::SpatialRef::from_epsg(4326).unwrap().to_crs_info(),
            ..Default::default()
        },
        RasterData::U16(values),
    )
    .unwrap();
    LazyRaster::from(RasterDataset::new(r, true))
}

#[test]
fn factor_two_pyramids_overlap_and_per_level_csv_match_the_reference_layout() {
    for overlap in [0, 1] {
        let mut input = source(7, 5, 10., (0..35).collect());
        let mut sources = [RetileSource { name: "sample.tif".into(), source: &mut input }];
        let options = RetileOptions { tile_size: [3, 3], levels: 1, overlap, csv: Some("tiles.csv".into()), ..Default::default() };
        let mut out = Collect::default();
        let report = re::generate(&mut sources, &options, &mut out).unwrap();
        assert_eq!((report.planned, report.written), (7, 7));
        let mut top = LazyRaster::from_tiff_bytes(out.0["1/sample_1_1.tif"].clone()).unwrap().unwrap();
        assert_eq!((top.width(), top.height()), (3, 2));
        assert_eq!(top.read().unwrap().raster.data.to_f64_vec(), [8., 10., 12., 22., 24., 26.]);
        let mut edge = LazyRaster::from_tiff_bytes(out.0["sample_2_3.tif"].clone()).unwrap().unwrap();
        assert_eq!((edge.width(), edge.height()), if overlap == 0 { (1, 2) } else { (3, 3) });
        assert_eq!(
            edge.read().unwrap().raster.data.to_f64_vec(),
            if overlap == 0 { vec![27., 34.] } else { vec![18., 19., 20., 25., 26., 27., 32., 33., 34.] }
        );
        let csv = String::from_utf8(out.0["tiles.csv"].clone()).unwrap();
        assert_eq!(csv.lines().count(), 6);
        assert!(csv.starts_with("sample_1_1.tif;10.000000;13.000000;2.000000;5.000000\n"));
    }
}

#[test]
fn later_sources_overwrite_nodata_and_physical_masks_do_not_change_raw_copies() {
    let mut left = source(4, 4, 0., vec![7; 16]);
    let mut right = source(4, 4, 2., vec![0; 16]);
    let options = RetileOptions { tile_size: [6, 4], ..Default::default() };
    let mut inputs =
        [RetileSource { name: "left.tif".into(), source: &mut left }, RetileSource { name: "right.tif".into(), source: &mut right }];
    let mut out = Collect::default();
    re::generate(&mut inputs, &options, &mut out).unwrap();
    let mut tile = LazyRaster::from_tiff_bytes(out.0["left_1_1.tif"].clone()).unwrap().unwrap();
    assert_eq!(tile.read().unwrap().raster.data.to_f64_vec(), vec![vec![7., 7., 0., 0., 0., 0.]; 4].concat());
    let mut pixels = source(4, 4, 2., vec![22; 16]);
    let ds = pixels.read().unwrap();
    let bytes = terrio::memory::write_raster_bytes(
        &ds,
        "masked.tif",
        &terrio::RasterWriteOptions { mask: Some(vec![0; 16]), ..Default::default() },
    )
    .unwrap();
    let mut masked = LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
    let mut inputs =
        [RetileSource { name: "left.tif".into(), source: &mut left }, RetileSource { name: "mask.tif".into(), source: &mut masked }];
    let mut out = Collect::default();
    re::generate(&mut inputs, &options, &mut out).unwrap();
    let mut tile = LazyRaster::from_tiff_bytes(out.0["left_1_1.tif"].clone()).unwrap().unwrap();
    assert_eq!(tile.read().unwrap().raster.data.to_f64_vec(), vec![vec![7., 7., 22., 22., 22., 22.]; 4].concat());
}

#[test]
fn aligned_base_tiles_keep_uint64_samples_exact() {
    let mut input = source(2, 1, 0., vec![1, 2]).read().unwrap();
    input.raster.data_type = DataType::U64;
    input.raster.data = RasterData::U64(vec![(1 << 53) + 1, u64::MAX - 1]);
    input.has_nodata = false;
    input.band_nodata.clear();
    let mut input = LazyRaster::from(input);
    let mut inputs = [RetileSource { name: "wide.tif".into(), source: &mut input }];
    let mut out = Collect::default();
    re::generate(&mut inputs, &RetileOptions::default(), &mut out).unwrap();
    let mut output = LazyRaster::from_tiff_bytes(out.0["wide_1_1.tif"].clone()).unwrap().unwrap();
    assert!(matches!(output.read().unwrap().raster.data,RasterData::U64(v) if v==[(1<<53)+1,u64::MAX-1]));
}

#[test]
fn row_directories_and_shapefile_indexes_are_portable_file_callbacks() {
    let mut input = source(7, 5, 10., (0..35).collect());
    let mut inputs = [RetileSource { name: "sample.tif".into(), source: &mut input }];
    let options = RetileOptions {
        tile_size: [3, 3],
        row_directories: true,
        tile_index: Some("tiles.shp".into()),
        csv: Some("tiles.csv".into()),
        ..Default::default()
    };
    let mut out = Collect::default();
    re::generate(&mut inputs, &options, &mut out).unwrap();
    for path in ["0/1/sample_1_1.tif", "0/2/sample_2_3.tif", "0/tiles.csv", "0/tiles.shp", "0/tiles.dbf", "0/tiles.shx", "0/tiles.prj"] {
        assert!(out.0.contains_key(path), "{path}");
    }
    assert!(String::from_utf8(out.0["0/tiles.csv"].clone()).unwrap().starts_with("1/sample_1_1.tif;"));
}

#[test]
fn invalid_plans_and_resume_preserve_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("tiles");
    let mut input = source(7, 5, 0., (0..35).collect());
    let mut inputs = [RetileSource { name: "https://host/sample.tif?secret=hidden".into(), source: &mut input }];
    let options = RetileOptions { tile_size: [3, 3], levels: 3, ..Default::default() };
    assert!(re::to_directory(&mut inputs, &options, &output, false).is_err());
    assert!(!output.exists());
    let options = RetileOptions { tile_size: [3, 3], ..Default::default() };
    assert_eq!(re::plan(&mut inputs, &options).unwrap().prefix, "sample");
    re::to_directory(&mut inputs, &options, &output, false).unwrap();
    let protected = output.join("sample_1_1.tif");
    std::fs::write(&protected, b"keep bytes").unwrap();
    std::fs::remove_file(output.join("sample_1_2.tif")).unwrap();
    let report = re::to_directory(&mut inputs, &options, &output, true).unwrap();
    assert_eq!((report.resumed, report.written), (5, 1));
    assert_eq!(std::fs::read(protected).unwrap(), b"keep bytes");
}

#[test]
fn planning_reads_only_metadata_and_zero_pyramid_dimensions_fail_early() {
    struct Metadata(RasterDataset);
    impl terrio::ops::warp::WarpSource for Metadata {
        fn info(&mut self) -> terrio::Result<terrio::ops::warp::WarpSourceInfo> {
            Ok(terrio::ops::warp::WarpSourceInfo { raster: self.0.clone(), overviews: Vec::new() })
        }
        fn read_window(&mut self, _: Window, _: &WindowOptions) -> terrio::Result<RasterDataset> {
            panic!("plan read pixels")
        }
    }
    let mut source = Metadata(source(7, 5, 0., (0..35).collect()).header());
    let mut inputs = [RetileSource { name: "input.tif".into(), source: &mut source }];
    assert_eq!(re::plan(&mut inputs, &RetileOptions { tile_size: [3, 3], levels: 1, ..Default::default() }).unwrap().planned, 7);
    assert!(re::plan(&mut inputs, &RetileOptions { levels: 3, ..Default::default() }).is_err());
    assert!(re::plan(&mut inputs, &RetileOptions { prefix: Some("../escape".into()), ..Default::default() }).is_err());
}

#[test]
fn retile_keeps_color_roles_but_drops_source_statistics_and_packing_metadata() {
    let mut dataset = source(4, 4, 0., vec![7; 16]).read().unwrap();
    dataset.band_metadata = vec![terrio::BandMetadata {
        scale: Some(2.),
        offset: Some(10.),
        unit: Some("m".into()),
        description: Some("measurement".into()),
        color_interp: Some("Gray".into()),
        items: vec![("SOURCE".into(), "sensor".into()), ("STATISTICS_MINIMUM".into(), "7".into())],
    }];
    let mut input = LazyRaster::from(dataset);
    let mut sources = [RetileSource { name: "source.tif".into(), source: &mut input }];
    let mut sink = Collect::default();
    re::generate(&mut sources, &RetileOptions { tile_size: [2, 2], ..Default::default() }, &mut sink).unwrap();
    let tile = terrio::memory::open_raster_bytes(sink.0["source_1_1.tif"].clone(), "tile.tif").unwrap();
    let band = &tile.band_metadata[0];
    assert_eq!(band.color_interp.as_deref(), Some("Gray"));
    assert!(band.scale.is_none() && band.offset.is_none() && band.unit.is_none() && band.description.is_none() && band.items.is_empty());
    assert_eq!(tile.raster.data.to_f64_vec(), [7.; 4]);
}

#[test]
fn recursive_pyramids_use_the_emitted_parent_samples() {
    // Nearest reduction of an 8-pixel row chooses 1,3,5,7, then 3,7.
    // A direct 8->2 reduction would instead choose 2,6.
    let mut input = source(8, 8, 0., (0..64).map(|v| v + 1).collect());
    let mut inputs = [RetileSource { name: "sample.tif".into(), source: &mut input }];
    let mut sink = Collect::default();
    re::generate(&mut inputs, &RetileOptions { tile_size: [4, 4], levels: 2, ..Default::default() }, &mut sink).unwrap();
    let tile = terrio::memory::open_raster_bytes(sink.0["2/sample_1_1.tif"].clone(), "tile.tif").unwrap();
    assert_eq!(tile.raster.data.to_f64_vec(), [28., 32., 60., 64.]);
}

#[test]
fn resume_reads_changed_parent_files_without_reading_source_pixels() {
    struct NoPixels(RasterDataset);
    impl terrio::ops::warp::WarpSource for NoPixels {
        fn info(&mut self) -> terrio::Result<terrio::ops::warp::WarpSourceInfo> {
            Ok(terrio::ops::warp::WarpSourceInfo { raster: self.0.clone(), overviews: vec![] })
        }
        fn read_window(&mut self, _: Window, _: &WindowOptions) -> terrio::Result<RasterDataset> {
            panic!("resume should read existing parents, not original pixels")
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("tiles");
    let mut input = source(8, 8, 0., vec![7; 64]);
    let header = input.header();
    let options = RetileOptions { tile_size: [4, 4], levels: 2, ..Default::default() };
    re::to_directory(&mut [RetileSource { name: "sample.tif".into(), source: &mut input }], &options, &dir, false).unwrap();
    let parent = dir.join("1/sample_1_1.tif");
    let mut dataset = terrio::memory::open_raster_bytes(std::fs::read(&parent).unwrap(), "parent.tif").unwrap();
    dataset.raster.data = RasterData::U16(vec![99; 16]);
    let bytes = terrio::memory::write_raster_bytes(&dataset, "parent.tif", &Default::default()).unwrap();
    std::fs::write(&parent, bytes).unwrap();
    std::fs::remove_file(dir.join("2/sample_1_1.tif")).unwrap();
    let mut input = NoPixels(header);
    let report = re::to_directory(&mut [RetileSource { name: "sample.tif".into(), source: &mut input }], &options, &dir, true).unwrap();
    assert_eq!((report.resumed, report.written), (5, 1));
    let output = terrio::memory::open_raster_bytes(std::fs::read(dir.join("2/sample_1_1.tif")).unwrap(), "tile.tif").unwrap();
    assert_eq!(output.raster.data.to_f64_vec(), [99.; 4]);
}

#[test]
fn source_overviews_are_not_used_for_retile_pyramids() {
    struct FullResolutionOnly(LazyRaster);
    impl terrio::ops::warp::WarpSource for FullResolutionOnly {
        fn info(&mut self) -> terrio::Result<terrio::ops::warp::WarpSourceInfo> {
            Ok(terrio::ops::warp::WarpSourceInfo { raster: self.0.header(), overviews: vec![(4, 4), (2, 2)] })
        }
        fn read_window(&mut self, window: Window, options: &WindowOptions) -> terrio::Result<RasterDataset> {
            assert!(options.overview.is_none(), "stored source overviews must be ignored");
            self.0.read_window(window, options)
        }
    }
    let mut input = FullResolutionOnly(source(8, 8, 0., vec![7; 64]));
    let mut sink = Collect::default();
    re::generate(
        &mut [RetileSource { name: "sample.tif".into(), source: &mut input }],
        &RetileOptions { tile_size: [4, 4], levels: 2, pyramid_only: true, ..Default::default() },
        &mut sink,
    )
    .unwrap();
    let tile = terrio::memory::open_raster_bytes(sink.0["2/sample_1_1.tif"].clone(), "tile.tif").unwrap();
    assert_eq!(tile.raster.data.to_f64_vec(), [7.; 4]);
}
