//! A read of a whole GeoTIFF image holds the output and bounded working memory: its decoded chunks
//! are placed in the output and not also kept in the block cache, as GDAL's multi-threaded reads
//! of a whole image skip its block cache (GDAL 3.13.3, `GDAL_NUM_THREADS` above 1). A window read
//! still fills the cache.
//!
//! The peak counts the bytes every thread holds at once, through a process-wide counting allocator:
//! chunks are decoded on worker threads and freed on the caller's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use terrio::raster::RasterData;
use terrio::{DataType, LazyRaster, RasterConfig, RasterDataset, RasterWriteOptions, Window, WindowOptions};

/// Counts the bytes held by every thread, and the most held at once.
struct Counting;

static HELD: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(n: usize) {
    let held = HELD.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(held, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grow(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grow(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        grow(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The most bytes held at once while `f` runs, above those held when it starts.
fn peak<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let start = HELD.load(Ordering::Relaxed);
    PEAK.store(start, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed) - start)
}

#[test]
fn whole_read_holds_the_output_and_bounded_working_memory() {
    // A 32 MiB UInt16 image of 4,096 DEFLATE tiles of 8 KiB, smaller than the default cache.
    let (cols, rows) = (4096, 4096);
    let cfg = RasterConfig { cols, rows, bands: 1, data_type: DataType::U16, nodata: 0.0, ..Default::default() };
    let values: Vec<u16> = (0..cols * rows).map(|i| if i % cols < cols / 2 { 0 } else { (i * 7 % 65_521) as u16 }).collect();
    let ds = RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::U16(values.clone())).unwrap(), true);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiled.tif");
    let tiles = [("TILED", "YES"), ("BLOCKXSIZE", "64"), ("BLOCKYSIZE", "64")];
    let options = RasterWriteOptions {
        compression: Some("DEFLATE".into()),
        creation_options: tiles.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    };
    terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &options).unwrap();
    drop(ds);
    // A cache larger than the image, whatever the environment sets.
    terrio::block_cache::set_max_bytes(64 << 20);

    let mut r = LazyRaster::open(path.to_str().unwrap()).unwrap();
    let all = WindowOptions { bands: vec![1], overview: None };
    // A whole read first, so that the decoders of the worker threads are made and kept for the
    // next; then an empty cache, so that the next decodes every chunk.
    drop(r.read_window(Window::full(cols, rows), &all).unwrap());
    terrio::block_cache::clear();
    let output = cols * rows * 2;
    let (whole, most) = peak(|| r.read_window(Window::full(cols, rows), &all).unwrap());
    let RasterData::U16(got) = whole.raster.data else { panic!("not UInt16") };
    assert!(got == values, "the whole read's values differ from the image's");
    // Before, the decoded chunks were also kept in the cache: twice the output.
    assert!(most < output + output / 4, "a whole read of {output} bytes held {most} bytes at once");
}
