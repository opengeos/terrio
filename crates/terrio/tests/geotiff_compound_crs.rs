//! Compound CRSs in GeoTIFF GeoKeys, as GDAL reads and writes them: a horizontal key and
//! `VerticalCSTypeGeoKey` (GeoTIFF 1.1), which name no code for the compound CRS. GDAL 3.12.1 reads
//! them as the registry's compound CRS of the two codes when there is one (EPSG:9707 for 4326 and
//! 5773), and writes a compound CRS's two codes, whatever code the compound has.

use terrio::crs::SpatialRef;
use terrio::raster::RasterData;
use terrio::{DataType, LazyRaster, RasterConfig, RasterDataset, RasterWriteOptions};

/// A GeoTIFF terrio writes in the CRS `crs`, and its GeoKey directory.
fn written(dir: &std::path::Path, crs: &str) -> (String, Vec<u16>) {
    let cfg = RasterConfig {
        cols: 2,
        rows: 2,
        bands: 1,
        data_type: DataType::F32,
        crs: SpatialRef::parse(crs).unwrap().to_crs_info(),
        x_min: 5.0,
        y_min: 44.0,
        cell_size: 0.5,
        ..Default::default()
    };
    let r = terrio::Raster::from_data_native(cfg, RasterData::F32(vec![0.0; 4])).unwrap();
    let path = dir.join(format!("{}.tif", crs.replace([':', '+'], "_")));
    terrio::write_raster_dataset(&RasterDataset::new(r, false), path.to_str().unwrap(), &RasterWriteOptions::default()).unwrap();
    let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
    let keys = dec.get_tag_u16_vec(tiff::tags::Tag::Unknown(34735)).unwrap();
    (path.to_string_lossy().into_owned(), keys)
}

/// The value of a SHORT GeoKey.
fn key(keys: &[u16], id: u16) -> Option<u16> {
    keys[4..].chunks(4).find(|k| k[0] == id && k[1] == 0).map(|k| k[3])
}

#[test]
fn compound_crss_are_written_and_read_as_gdal_does() {
    let dir = tempfile::tempdir().unwrap();
    // (CRS written, horizontal key, horizontal code, vertical code, the code read back)
    let cases = [
        ("EPSG:9707", 2048, 4326, 5773, Some(9707)),
        ("EPSG:4326+5773", 2048, 4326, 5773, Some(9707)),
        // Two registered compound CRSs of these codes, one (7412) deprecated: the one named so.
        ("EPSG:27572+5719", 3072, 27572, 5719, Some(7411)),
        // No registered compound CRS of these codes.
        ("EPSG:32631+5773", 3072, 32631, 5773, None),
    ];
    for (crs, horizontal_key, horizontal, vertical, code) in cases {
        let (path, keys) = written(dir.path(), crs);
        // GeoTIFF 1.1 (KeyRevision 1.1), which GDAL keeps the vertical CRS of.
        assert_eq!(keys[..3], [1, 1, 1], "{crs}");
        assert_eq!((key(&keys, horizontal_key), key(&keys, 4096)), (Some(horizontal), Some(vertical)), "{crs}");
        let read = LazyRaster::open(&path).unwrap().crs();
        assert_eq!(read.authority(), code.map(|c| ("EPSG", c)), "{crs}");
        let wkt = read.to_wkt().unwrap();
        assert!(wkt.starts_with("COMPD_CS["), "{crs}: {wkt}");
        assert!(wkt.contains(&format!("AUTHORITY[\"EPSG\",\"{vertical}\"]")), "{crs}: {wkt}");
    }
    // Other CRSs are written as they were: GeoTIFF 1.0, no vertical key.
    let (_, keys) = written(dir.path(), "EPSG:4326");
    assert_eq!((keys[..3].to_vec(), key(&keys, 2048), key(&keys, 4096)), (vec![1, 1, 0], Some(4326), None));
}
