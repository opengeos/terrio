//! Sparse resampled reads batch the chunks sampling visits before decoding, preserving lazy
//! I/O and decoded-cache hits while avoiding one retry per chunk.

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

fn open_sparse(bytes: &[u8], file: &SparseFile) -> terrio::LazyRaster {
    for _ in 0..64 {
        match terrio::LazyRaster::from_sparse(file.clone()) {
            Ok(Some(lazy)) => return lazy,
            Ok(None) => panic!("a TIFF was not recognized"),
            Err(error) if file.has_missing() => {
                for (offset, length) in file.missing() {
                    file.insert(offset, &bytes[offset as usize..(offset + length) as usize]);
                }
                assert!(!error.to_string().is_empty());
            }
            Err(error) => panic!("open failed: {error}"),
        }
    }
    panic!("TIFF metadata still missing after 64 fetch rounds");
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
    let mut r = open_sparse(&bytes, &file);
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

    let mut r = open_sparse(&bytes, &file);
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

/// Heavy nearest decimation must fetch the same data/mask blocks as its sampled pixel,
/// rather than the entire source window. Zero stored overviews makes the distinction explicit.
#[test]
fn heavily_decimated_reads_fetch_only_sampled_blocks() {
    for kind in ["pixels", "mask", "alpha"] {
        let mut ds = raster(1024, 1024);
        let mut options = terrio::RasterWriteOptions {
            driver: Some(terrio::formats::RasterDriver::Cog),
            compression: Some("NONE".into()),
            tile_size: Some(128),
            creation_options: vec![("OVERVIEW_COUNT".into(), "0".into())],
            ..Default::default()
        };
        if kind == "mask" {
            options.mask = Some((0..1024 * 1024).map(|i| if i % 3 == 0 { 0 } else { 255 }).collect());
        } else if kind == "alpha" {
            let cfg = terrio::RasterConfig { cols: 1024, rows: 1024, bands: 4, data_type: terrio::DataType::U16, ..Default::default() };
            let values = (0..4 * 1024 * 1024).map(|i| (i % 65536) as u16).collect();
            ds = terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, terrio::raster::RasterData::U16(values)).unwrap(), false);
            options.band_metadata = ["Red", "Green", "Blue", "Alpha"]
                .into_iter()
                .map(|name| terrio::BandMetadata { color_interp: Some(name.into()), ..Default::default() })
                .collect();
        }
        let bytes = terrio::memory::write_raster_bytes(&ds, "sampled.tif", &options).unwrap();
        let file = SparseFile::with_block_size(bytes.len() as u64, 4096);
        let mut lazy = open_sparse(&bytes, &file);
        assert!(lazy.overviews().is_empty());
        let before = file.fetched().0;
        let read = ResampledReadOptions { bands: vec![1], size: (1, 1), resampling: Resampling::Nearest, ..Default::default() };
        let (got, masks) = terrio::sparse::read_with(
            &file,
            3, // At most one fetch round for data and one more for a mask/UInt16 alpha.
            |offset, len| Ok(bytes[offset as usize..(offset + len) as usize].to_vec()),
            || lazy.read_resampled_with_masks(&read),
        )
        .unwrap();
        let fetched = file.fetched().0 - before;
        let point_file = SparseFile::with_block_size(bytes.len() as u64, 4096);
        let mut point = open_sparse(&bytes, &point_file);
        let before_point = point_file.fetched().0;
        terrio::sparse::read_with(
            &point_file,
            64,
            |offset, len| Ok(bytes[offset as usize..(offset + len) as usize].to_vec()),
            || {
                point.read_window(terrio::Window::new(512, 512, 1, 1), &terrio::WindowOptions { bands: vec![1], overview: None })?;
                point.read_mask(terrio::Window::new(512, 512, 1, 1), None)
            },
        )
        .unwrap();
        let sampled_bytes = point_file.fetched().0 - before_point;
        let mut memory = terrio::memory::open_raster_lazy_bytes(bytes, "sampled.tif").unwrap();
        let (want, want_masks) = memory.read_resampled_with_masks(&read).unwrap();
        assert_eq!(got.raster.data.to_f64_vec(), want.raster.data.to_f64_vec());
        assert_eq!(masks, want_masks);
        eprintln!("{kind}: resampled {fetched} bytes; direct sampled pixel {sampled_bytes} bytes");
        assert_eq!(fetched, sampled_bytes, "{kind}: unused blocks were fetched");
    }
}

#[test]
fn malformed_unsampled_chunk_does_not_break_a_nearest_read() {
    let options = terrio::RasterWriteOptions {
        driver: Some(terrio::formats::RasterDriver::Cog),
        tile_size: Some(128),
        creation_options: vec![("OVERVIEW_COUNT".into(), "0".into())],
        ..Default::default()
    };
    let mut bytes = terrio::memory::write_raster_bytes(&raster(1024, 1024), "unused.tif", &options).unwrap();
    assert_eq!(&bytes[..4], b"II\x2a\0");
    let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
    let tag = (0..entries).map(|i| ifd + 2 + i * 12).find(|&i| u16::from_le_bytes(bytes[i..i + 2].try_into().unwrap()) == 324).unwrap();
    assert_eq!(u16::from_le_bytes(bytes[tag + 2..tag + 4].try_into().unwrap()), 4);
    let offsets = u32::from_le_bytes(bytes[tag + 8..tag + 12].try_into().unwrap()) as usize;
    let invalid = bytes.len() as u32 + 4096;
    bytes[offsets..offsets + 4].copy_from_slice(&invalid.to_le_bytes());
    let file = SparseFile::with_block_size(bytes.len() as u64, 4096);
    let mut lazy = open_sparse(&bytes, &file);
    let read = ResampledReadOptions { bands: vec![1], size: (1, 1), resampling: Resampling::Nearest, ..Default::default() };
    terrio::sparse::read_with(
        &file,
        64,
        |offset, len| Ok(bytes[offset as usize..(offset + len) as usize].to_vec()),
        || lazy.read_resampled_with_masks(&read),
    )
    .unwrap();
}

#[test]
fn decoded_tiles_do_not_refetch_after_sparse_bytes_are_evicted() {
    let options = terrio::RasterWriteOptions {
        driver: Some(terrio::formats::RasterDriver::Cog),
        tile_size: Some(128),
        creation_options: vec![("OVERVIEW_COUNT".into(), "0".into())],
        ..Default::default()
    };
    let bytes = terrio::memory::write_raster_bytes(&raster(1024, 1024), "warm.tif", &options).unwrap();
    let file = SparseFile::with_block_size(bytes.len() as u64, 4096);
    let mut lazy = open_sparse(&bytes, &file);
    let read = ResampledReadOptions { bands: vec![1], size: (128, 128), resampling: Resampling::Nearest, ..Default::default() };
    let (first, _) = terrio::sparse::read_with(
        &file,
        64,
        |offset, len| Ok(bytes[offset as usize..(offset + len) as usize].to_vec()),
        || lazy.read_resampled_with_masks(&read),
    )
    .unwrap();
    file.trim(0);
    let before = file.fetched();
    let (again, _) = lazy.read_resampled_with_masks(&read).expect("decoded tiles must not request their compressed bytes again");
    assert_eq!(first.raster.data.to_f64_vec(), again.raster.data.to_f64_vec());
    assert!(!file.has_missing());
    assert_eq!(file.fetched(), before);
}
