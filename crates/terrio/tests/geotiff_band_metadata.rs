//! Band scale, offset, unit and description of GeoTIFFs: the `GDAL_METADATA` items with a role
//! GDAL writes for them, read back by the lazy and in-memory readers.

use terrio::raster::RasterData;
use terrio::{BandMetadata, DataType, RasterConfig, RasterDataset, RasterWriteOptions};

fn band() -> BandMetadata {
    BandMetadata {
        scale: Some(0.5),
        offset: Some(-10.0),
        unit: Some("feet".into()),
        description: Some("a<b & c".into()),
        ..Default::default()
    }
}

fn written() -> Vec<u8> {
    let cfg = RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::F32, ..Default::default() };
    let r = terrio::Raster::from_data_native(cfg, RasterData::F32(vec![1.0, 2.0, 3.0, 4.0])).unwrap();
    let options = RasterWriteOptions { band_metadata: vec![band()], ..Default::default() };
    terrio::memory::write_raster_bytes(&RasterDataset::new(r, false), "b.tif", &options).unwrap()
}

fn properties(b: &BandMetadata) -> (Option<f64>, Option<f64>, Option<&str>, Option<&str>) {
    (b.scale, b.offset, b.unit.as_deref(), b.description.as_deref())
}

#[test]
fn band_properties_round_trip() {
    let bytes = written();
    let text = String::from_utf8_lossy(&bytes);
    // As GDAL 3.13 writes them.
    assert!(text.contains(r#"<Item name="SCALE" sample="0" role="scale">0.5</Item>"#), "{text}");
    assert!(text.contains(r#"<Item name="UNITTYPE" sample="0" role="unittype">feet</Item>"#), "{text}");
    let expected = band();
    let want = properties(&expected);

    let mut lazy = terrio::LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap();
    assert_eq!(properties(&lazy.header().band_metadata[0]), want);
    let window = lazy.read_window(terrio::Window::new(1, 1, 1, 1), &Default::default()).unwrap();
    assert_eq!(properties(&window.band_metadata[0]), want);
    let ds = terrio::memory::open_raster_bytes(bytes, "b.tif").unwrap();
    assert_eq!(properties(&ds.band_metadata[0]), want);
    // Role items are band properties, not band metadata items.
    assert!(ds.band_metadata[0].items.is_empty());
}

#[test]
fn default_band_items_survive_lazy_eager_and_copy_reads_but_unscale_drops_statistics() {
    let config = RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::U8, ..Default::default() };
    let raster = terrio::Raster::from_data_native(config, RasterData::U8(vec![1, 2, 3, 4])).unwrap();
    let mut source = RasterDataset::new(raster, false);
    source.band_metadata = vec![BandMetadata {
        scale: Some(2.),
        offset: Some(10.),
        items: vec![("SOURCE".into(), "sensor & instrument".into()), ("STATISTICS_MINIMUM".into(), "1".into())],
        ..Default::default()
    }];
    let bytes = terrio::memory::write_raster_bytes(&source, "source.tif", &Default::default()).unwrap();
    let lazy = terrio::LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap();
    let eager = terrio::memory::open_raster_bytes(bytes, "source.tif").unwrap();
    assert_eq!(lazy.header().band_metadata[0].items, source.band_metadata[0].items);
    assert_eq!(eager.band_metadata[0].items, source.band_metadata[0].items);
    let copied = terrio::memory::write_raster_bytes(&eager, "copy.tif", &Default::default()).unwrap();
    let copied = terrio::memory::open_raster_bytes(copied, "copy.tif").unwrap();
    assert_eq!(copied.band_metadata[0].items, source.band_metadata[0].items);
    let unscaled = terrio::ops::pixel::unscale(&eager, &Default::default()).unwrap();
    let bytes = terrio::memory::write_raster_bytes(&unscaled, "unscaled.tif", &Default::default()).unwrap();
    let output = terrio::memory::open_raster_bytes(bytes, "unscaled.tif").unwrap();
    assert_eq!(output.band_metadata[0].items, [("SOURCE".into(), "sensor & instrument".into())]);
    assert_eq!(output.raster.data.to_f64_vec(), [12., 14., 16., 18.]);
}
