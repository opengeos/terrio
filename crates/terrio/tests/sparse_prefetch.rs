//! Sparse resampled reads prefetch every chunk of the source window before decoding, so a COG
//! read costs one fetch round rather than one retry per chunk.

use terrio::ResampledReadOptions;
use terrio::ops::Resampling;
use terrio::ops::overview::OverviewSelection;
use terrio::sparse::SparseFile;

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
fn resampled_world_read_prefetches_all_chunks_in_one_round() {
    // The EUDEM pattern: 4x4 = 16 chunks in the overview chosen for a world read.
    let ds = raster(2048, 2048);
    let options = terrio::RasterWriteOptions {
        driver: Some(terrio::formats::RasterDriver::Cog),
        tile_size: Some(128),
        // Two overviews only: the coarsest is 512, so the 256 px world output reads that
        // overview as 4x4 = 16 chunks by nearest neighbour.
        creation_options: vec![("OVERVIEW_COUNT".into(), "2".into())],
        ..Default::default()
    };
    let bytes = terrio::memory::write_raster_bytes(&ds, "prefetch.tif", &options).unwrap();
    let file = SparseFile::new(bytes.len() as u64);

    // Open first, as a browser source does.
    let mut r = loop {
        match terrio::LazyRaster::from_sparse(file.clone()) {
            Ok(Some(lazy)) => break lazy,
            Ok(None) => panic!("a COG was not recognized as a TIFF"),
            Err(error) if file.has_missing() => {
                for (offset, length) in file.missing() {
                    file.insert(offset, &bytes[offset as usize..(offset + length) as usize]);
                }
                // Avoid looping forever on a malformed file.
                assert!(!error.to_string().is_empty());
            }
            Err(error) => panic!("open failed: {error}"),
        }
    };
    assert_eq!(r.overviews(), vec![(1024, 1024), (512, 512)]);
    assert_eq!(r.block_size(), Some((128, 128)));

    let read = ResampledReadOptions {
        bands: vec![1],
        window: Some((0.0, 0.0, 2048.0, 2048.0)),
        size: (256, 256),
        resampling: Resampling::Nearest,
        overview: OverviewSelection::Auto,
    };
    let mut rounds = 0;
    let (got, _) = loop {
        match r.read_resampled_with_masks(&read) {
            Ok(result) => break result,
            Err(error) if file.has_missing() => {
                rounds += 1;
                assert!(rounds <= 2, "the world read needed another fetch round: {error}");
                for (offset, length) in file.missing() {
                    file.insert(offset, &bytes[offset as usize..(offset + length) as usize]);
                }
            }
            Err(error) => panic!("world read failed: {error}"),
        }
    };
    assert_eq!(rounds, 1, "the whole window must be prefetched before decoding");

    let mut whole = terrio::memory::open_raster_lazy_bytes(bytes, "prefetch.tif").unwrap();
    let (want, _) = whole.read_resampled_with_masks(&read).unwrap();
    assert_eq!(got.raster.data.to_f64_vec(), want.raster.data.to_f64_vec());
}

#[test]
fn resampled_world_read_prefetches_mask_chunks_too() {
    let ds = raster(2048, 2048);
    let mask: Vec<u8> = (0..2048 * 2048).map(|i| if i % 3 == 0 { 0 } else { 255 }).collect();
    let options = terrio::RasterWriteOptions {
        driver: Some(terrio::formats::RasterDriver::Cog),
        tile_size: Some(128),
        creation_options: vec![("OVERVIEW_COUNT".into(), "2".into())],
        mask: Some(mask),
        ..Default::default()
    };
    let bytes = terrio::memory::write_raster_bytes(&ds, "prefetch_mask.tif", &options).unwrap();
    let file = SparseFile::new(bytes.len() as u64);

    let mut r = loop {
        match terrio::LazyRaster::from_sparse(file.clone()) {
            Ok(Some(lazy)) => break lazy,
            Ok(None) => panic!("a COG was not recognized as a TIFF"),
            Err(_) if file.has_missing() => {
                for (offset, length) in file.missing() {
                    file.insert(offset, &bytes[offset as usize..(offset + length) as usize]);
                }
            }
            Err(error) => panic!("open failed: {error}"),
        }
    };
    assert!(r.has_mask());

    let read = ResampledReadOptions {
        bands: vec![1],
        window: Some((0.0, 0.0, 2048.0, 2048.0)),
        size: (256, 256),
        resampling: Resampling::Nearest,
        overview: OverviewSelection::Auto,
    };
    let mut rounds = 0;
    let (got, got_mask) = loop {
        match r.read_resampled_with_masks(&read) {
            Ok(result) => break result,
            Err(error) if file.has_missing() => {
                rounds += 1;
                assert!(rounds <= 3, "the masked world read needed more fetch rounds: {error}");
                for (offset, length) in file.missing() {
                    file.insert(offset, &bytes[offset as usize..(offset + length) as usize]);
                }
            }
            Err(error) => panic!("masked world read failed: {error}"),
        }
    };
    // Pixels and masks share the prefetch path: one round for pixels, at most one more for masks.
    assert!(rounds <= 2, "the masked world read used {rounds} fetch rounds");

    let mut whole = terrio::memory::open_raster_lazy_bytes(bytes, "prefetch_mask.tif").unwrap();
    let (want, want_mask) = whole.read_resampled_with_masks(&read).unwrap();
    assert_eq!(got.raster.data.to_f64_vec(), want.raster.data.to_f64_vec());
    assert_eq!(got_mask, want_mask);
}
