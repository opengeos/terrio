//! What a raster declares, and what a whole read of it allocates (#724).
//!
//! A GeoTIFF of a few hundred kilobytes can declare gigabytes of pixels: every strip or tile
//! points to the same small compressed block. These tests check that:
//!
//! - `TERRIO_MAX_DECLARED_BYTES` and `TERRIO_MAX_BANDS`, set for a thread (as Python's
//!   `terrio.rio.Env` sets them) or in the process environment, refuse such a raster when it is
//!   opened, whatever its driver and whatever public entry point opens it, before any of its pixels
//!   is decoded where its reader reads a header first;
//! - a whole read of a GeoTIFF that the process cannot hold is refused before its buffer is
//!   allocated, while a window of it that fits is read;
//! - the GeoTIFF and COG writers that operations stream to refuse a raster whose rows they would
//!   hold cannot fit.
//!
//! "Before" is measured: the binary's global allocator records the largest allocation asked for,
//! on any thread, so a refusal that came after the buffer was asked for is seen even when the
//! allocation would have failed. The tests run one at a time for it.

#[cfg(unix)]
use std::alloc::{GlobalAlloc, Layout};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use terrio::formats::RasterDriver;
use terrio::raster::RasterData;
use terrio::{DataType, RasterConfig, RasterDataset, RasterWriteOptions};

#[path = "stacta_common/mod.rs"]
mod stacta_common;

/// The global allocator, recording the largest allocation asked for since [`largest_during`] last
/// reset it. On Unix it is the C allocator, as `std::alloc::System` is there, called directly:
/// Rust 1.99's debug check of `System`'s `GlobalAlloc` methods (a zero-size layout) aborts on a
/// block `openjp2` frees with one when its codec is dropped, which the default allocator path does
/// not check. Elsewhere the default allocator is kept, and allocations are not counted.
#[cfg(unix)]
struct Counting;

/// The largest allocation asked for since the last reset, on any thread.
static LARGEST: AtomicUsize = AtomicUsize::new(0);

/// `std::alloc::System`'s smallest alignment on Unix: the C allocator's own aligns to it.
#[cfg(unix)]
const MIN_ALIGN: usize = 16;

// SAFETY: as `std::alloc::System` on Unix: `malloc`, `calloc` and `realloc` for alignments the C
// allocator gives, `posix_memalign` for larger ones, `free` for every block.
#[cfg(unix)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        if layout.align() <= MIN_ALIGN && layout.align() <= layout.size() {
            // SAFETY: a plain C allocation.
            return unsafe { libc::malloc(layout.size()) }.cast();
        }
        aligned(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        if layout.align() <= MIN_ALIGN && layout.align() <= layout.size() {
            // SAFETY: a plain C allocation.
            return unsafe { libc::calloc(layout.size(), 1) }.cast();
        }
        let ptr = aligned(layout);
        if !ptr.is_null() {
            // SAFETY: `ptr` holds `layout.size()` bytes.
            unsafe { std::ptr::write_bytes(ptr, 0, layout.size()) };
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::Relaxed);
        if layout.align() <= MIN_ALIGN && layout.align() <= new_size {
            // SAFETY: `ptr` came from this allocator, whose blocks are the C allocator's.
            return unsafe { libc::realloc(ptr.cast(), new_size) }.cast();
        }
        // SAFETY: the caller's contract: `new_size` rounded up to the alignment fits `isize`.
        let new = aligned(unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) });
        if !new.is_null() {
            // SAFETY: both blocks hold at least the bytes copied, and are distinct.
            unsafe {
                std::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                libc::free(ptr.cast());
            }
        }
        new
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: `ptr` came from this allocator, whose blocks are the C allocator's.
        unsafe { libc::free(ptr.cast()) }
    }
}

/// A block of `layout` at an alignment the C allocator's `malloc` does not give.
#[cfg(unix)]
fn aligned(layout: Layout) -> *mut u8 {
    let mut out = std::ptr::null_mut();
    let align = layout.align().max(std::mem::size_of::<usize>());
    // SAFETY: `out` is written only on success; the alignment is a power of two and a multiple
    // of the pointer size.
    let r = unsafe { libc::posix_memalign(&mut out, align, layout.size()) };
    if r == 0 { out.cast() } else { std::ptr::null_mut() }
}

#[cfg(unix)]
#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Whether allocations are counted: see [`Counting`].
const COUNTED: bool = cfg!(unix);

/// Held by each test: the allocations of one would be counted in another's.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What `f` returns, and the largest allocation asked for while it ran.
fn largest_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    LARGEST.store(0, Ordering::Relaxed);
    let out = f();
    (out, LARGEST.load(Ordering::Relaxed))
}

/// `f` with `options` set on the calling thread, as Python's `terrio.rio.Env` sets them.
fn scoped<T>(options: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            terrio::config::pop_scope();
        }
    }
    terrio::config::push_scope(options.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect());
    let _pop = Pop;
    f()
}

/// `Ok(())`, or the error's message.
fn outcome<T>(result: terrio::Result<T>) -> Result<(), String> {
    result.map(|_| ()).map_err(|e| e.to_string())
}

/// Whether `result` is a resource refusal (Python's `ResourceLimitError`) whose message is
/// `message`.
fn refused<T>(result: terrio::Result<T>, message: &str) -> bool {
    matches!(&result, Err(e) if e.is_resource_refusal() && e.to_string() == message)
}

/// A copy of an error, of its kind and message, for [`refused`].
fn clone_error(e: &terrio::Error) -> terrio::Error {
    match e {
        terrio::Error::ResourceLimit(m) => terrio::Error::ResourceLimit(m.clone()),
        e => terrio::Error::InvalidData(e.to_string()),
    }
}

/// The refusal of a raster of `cols` x `rows` pixels in `bands` bands of `bytes` bytes of pixels by
/// `TERRIO_MAX_DECLARED_BYTES=limit`.
fn too_many_bytes(cols: usize, rows: usize, bands: usize, bytes: u128, limit: u128) -> String {
    format!("the raster declares {cols} x {rows} pixels in {bands} band(s), {bytes} bytes, more than TERRIO_MAX_DECLARED_BYTES={limit}")
}

/// The refusal of a raster of `bands` bands by `TERRIO_MAX_BANDS=limit`.
fn too_many_bands(bands: usize, limit: usize) -> String {
    format!("the raster declares {bands} bands, more than TERRIO_MAX_BANDS={limit}")
}

// ─── The issue's files ──────────────────────────────────────────────────────────────────────────

/// A little-endian baseline TIFF of `bands` bands of `bits`-bit samples (`sample_format` 1 for
/// unsigned integers, 3 for floats) declaring `width` x `height` pixels, written as the issue
/// writes its files: band-separate planes of one strip each (with `ExtraSamples` for every band but
/// the first) or, with `block`, pixel-interleaved tiles of that size; every strip or tile points
/// to the same deflated block of zeros.
fn declaring_tiff(width: u32, height: u32, bands: u16, bits: u16, sample_format: u16, block: Option<(u32, u32)>) -> Vec<u8> {
    let (bw, bh) = block.unwrap_or((width, height));
    let per_band = match block {
        None => 1,
        Some(_) => width.div_ceil(bw) as usize * height.div_ceil(bh) as usize,
    };
    let count = per_band * usize::from(if block.is_some() { 1 } else { bands });
    let chunk = bw as usize * bh as usize * usize::from(bits) / 8 * if block.is_some() { usize::from(bands) } else { 1 };
    let data = deflate_zeros(chunk);
    // (tag, type, values): 3 SHORT, 4 LONG, 12 DOUBLE.
    let mut tags: Vec<(u16, u16, Vec<f64>)> = vec![
        (256, 4, vec![f64::from(width)]),
        (257, 4, vec![f64::from(height)]),
        (258, 3, vec![f64::from(bits); usize::from(bands)]),
        (259, 3, vec![8.0]),
        (262, 3, vec![1.0]),
        (277, 3, vec![f64::from(bands)]),
        (284, 3, vec![if bands > 1 && block.is_none() { 2.0 } else { 1.0 }]),
        (339, 3, vec![f64::from(sample_format); usize::from(bands)]),
        (33550, 12, vec![360.0 / f64::from(width), 180.0 / f64::from(height), 0.0]),
        (33922, 12, vec![0.0, 0.0, 0.0, -180.0, 90.0, 0.0]),
        (34735, 3, [1, 1, 0, 3, 1024, 0, 1, 2, 1025, 0, 1, 1, 2048, 0, 1, 4326].map(f64::from).to_vec()),
    ];
    if bands > 1 {
        tags.push((338, 3, vec![0.0; usize::from(bands) - 1]));
    }
    let (offsets, counts) = match block {
        None => {
            tags.push((278, 4, vec![f64::from(height)]));
            (273, 279)
        }
        Some((bw, bh)) => {
            tags.push((322, 3, vec![f64::from(bw)]));
            tags.push((323, 3, vec![f64::from(bh)]));
            (324, 325)
        }
    };
    tags.push((offsets, 4, vec![8.0; count]));
    tags.push((counts, 4, vec![data.len() as f64; count]));
    tags.sort_by_key(|t| t.0);
    let ifd_at = 8 + data.len() + (data.len() & 1);
    let extra_at = ifd_at + 2 + 12 * tags.len() + 4;
    let mut ifd = (tags.len() as u16).to_le_bytes().to_vec();
    let mut extra = Vec::new();
    for (tag, kind, values) in &tags {
        let packed: Vec<u8> = values
            .iter()
            .flat_map(|v| match kind {
                3 => (*v as u16).to_le_bytes().to_vec(),
                4 => (*v as u32).to_le_bytes().to_vec(),
                _ => v.to_le_bytes().to_vec(),
            })
            .collect();
        ifd.extend(tag.to_le_bytes());
        ifd.extend(kind.to_le_bytes());
        ifd.extend((values.len() as u32).to_le_bytes());
        if packed.len() <= 4 {
            let mut inline = packed.clone();
            inline.resize(4, 0);
            ifd.extend(inline);
        } else {
            ifd.extend(((extra_at + extra.len()) as u32).to_le_bytes());
            extra.extend(&packed);
            if packed.len() & 1 == 1 {
                extra.push(0);
            }
        }
    }
    ifd.extend(0u32.to_le_bytes());
    let mut file = b"II*\0".to_vec();
    file.extend((ifd_at as u32).to_le_bytes());
    file.extend(&data);
    if data.len() & 1 == 1 {
        file.push(0);
    }
    file.extend(ifd);
    file.extend(extra);
    file
}

/// `len` zero bytes, deflated (zlib).
fn deflate_zeros(len: usize) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    let zeros = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        let n = left.min(zeros.len());
        e.write_all(&zeros[..n]).unwrap();
        left -= n;
    }
    e.finish().unwrap()
}

/// One of the issue's files: its name, its bytes, its width, height and bands, and the bytes of a
/// sample.
type IssueFile = (&'static str, Vec<u8>, usize, usize, usize, usize);

/// An open under test, named: `Ok(())`, or the error's message.
type Open<'a> = (&'a str, &'a dyn Fn() -> Result<(), String>);

/// The issue's three files: 16,384 UInt8 bands of 256 x 256 pixels (1 GiB), the same in
/// Float32 (4 GiB), and one UInt8 band of 65,536 x 65,536 pixels in 512 x 512 tiles (4 GiB):
/// `(name, bytes, width, height, bands, bytes per sample)`.
fn issue_files() -> Vec<IssueFile> {
    vec![
        ("bands-u8.tif", declaring_tiff(256, 256, 16384, 8, 1, None), 256, 256, 16384, 1),
        ("bands-f32.tif", declaring_tiff(256, 256, 16384, 32, 3, None), 256, 256, 16384, 4),
        ("giant-u8.tif", declaring_tiff(65536, 65536, 1, 8, 1, Some((512, 512))), 65536, 65536, 1, 1),
    ]
}

/// The issue's files, opened eagerly and lazily under `TERRIO_MAX_DECLARED_BYTES` and
/// `TERRIO_MAX_BANDS` set for the thread: each is refused by what it declares, before anything
/// near its pixels' size is allocated; without the options, each opens lazily (its headers only).
#[test]
fn the_issues_geotiffs_are_refused_by_what_they_declare_before_their_pixels() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    for (name, bytes, w, h, bands, size) in issue_files() {
        let path = dir.path().join(name);
        std::fs::write(&path, &bytes).unwrap();
        let path = path.to_str().unwrap();
        let declared = (w * h * bands * size) as u128;
        let r = terrio::open_raster_lazy(path).unwrap();
        assert_eq!((r.width(), r.height(), r.band_count()), (w, h, bands), "{name}");
        let limit = 1u128 << 24;
        let message = too_many_bytes(w, h, bands, declared, limit);
        let opens: [Open; 2] = [("eager", &|| outcome(terrio::open_raster(path))), ("lazy", &|| outcome(terrio::open_raster_lazy(path)))];
        for (how, open) in opens {
            let (result, largest) = largest_during(|| scoped(&[("TERRIO_MAX_DECLARED_BYTES", &limit.to_string())], open));
            assert_eq!(result, Err(message.clone()), "{name}, {how}");
            assert!(largest < 1 << 24, "{name}, {how}: {largest} bytes asked for at once");
            if bands > 1 {
                let (result, largest) = largest_during(|| scoped(&[("TERRIO_MAX_BANDS", "1024")], open));
                assert_eq!(result, Err(too_many_bands(bands, 1024)), "{name}, {how}");
                assert!(largest < 1 << 24, "{name}, {how}: {largest} bytes asked for at once");
            }
        }
        // A bound the raster is within opens it.
        let within = scoped(&[("TERRIO_MAX_DECLARED_BYTES", &declared.to_string()), ("TERRIO_MAX_BANDS", &bands.to_string())], || {
            outcome(terrio::open_raster_lazy(path))
        });
        assert_eq!(within, Ok(()), "{name}");
        // The refusal is the error Python raises as `ResourceLimitError`.
        let error = scoped(&[("TERRIO_MAX_DECLARED_BYTES", "1")], || terrio::open_raster_lazy(path).map(|_| ()).unwrap_err());
        assert!(matches!(error, terrio::Error::ResourceLimit(_)), "{name}: {error:?}");
    }
}

/// A GeoTIFF declaring about a terabyte (65,535 UInt8 bands of 4,096 x 4,096 pixels, each band a
/// strip of 16 MiB pointing to the same deflated block), more than any test machine's memory: its
/// whole read, eager, lazy or into another buffer, is refused before its buffer is asked for, naming
/// the sizes; a window of it that fits (a band) is read. Where terrio cannot read the usable memory (no `/proc`:
/// Windows, macOS), nothing is refused and the test is skipped.
#[test]
#[cfg(target_pointer_width = "64")]
fn whole_reads_the_process_cannot_hold_are_refused_before_their_buffer() {
    let _serial = serial();
    let (w, h, bands) = (4096usize, 4096usize, 65535usize);
    let declared = w as u64 * h as u64 * bands as u64;
    let Some(usable) = terrio::usable_memory().filter(|m| *m < declared) else {
        eprintln!("skipped: the usable memory is not known here, or is more than {declared} bytes");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("terabyte.tif");
    std::fs::write(&path, declaring_tiff(w as u32, h as u32, bands as u16, 8, 1, None)).unwrap();
    let path = path.to_str().unwrap();
    let held = |what: &str| {
        format!(
            "{what} of {w} x {h} pixels in {bands} band(s) of 1-byte samples is too large to allocate: {declared} bytes, more than the \
             {usable} bytes of memory the process can use"
        )
    };
    let whole = held("the raster");
    let reads: [(&str, &dyn Fn() -> terrio::Result<()>); 3] = [
        ("open_raster", &|| terrio::open_raster(path).map(|_| ())),
        ("open_raster_with_tiff_limits", &|| terrio::open_raster_with_tiff_limits(path, Default::default()).map(|_| ())),
        ("memory::open_raster_bytes", &|| terrio::memory::open_raster_bytes(std::fs::read(path).unwrap(), "terabyte.tif").map(|_| ())),
    ];
    for (how, read) in reads {
        let (result, largest) = largest_during(read);
        assert!(refused(result.as_ref().map(|_| ()).map_err(clone_error), &whole), "{how}: {result:?}");
        assert!(largest < 1 << 26, "{how}: {largest} bytes asked for at once");
    }
    let window = held("a window");
    let mut r = terrio::open_raster_lazy(path).unwrap();
    let (result, largest) = largest_during(|| r.read().map(|_| ()));
    assert!(refused(result.as_ref().map(|_| ()).map_err(clone_error), &window), "lazy read: {result:?}");
    assert!(largest < 1 << 26, "lazy read: {largest} bytes asked for at once");
    // Its whole read into another buffer (a translation that does not stream reads so).
    let resampled = terrio::ResampledReadOptions {
        bands: Vec::new(),
        window: None,
        size: (w, h),
        resampling: terrio::ops::Resampling::Nearest,
        overview: terrio::ops::overview::OverviewSelection::Auto,
    };
    let (result, largest) = largest_during(|| r.read_resampled(&resampled).map(|_| ()));
    let message = format!(
        "a resampled read of {w} x {h} pixels in {bands} band(s) is too large to allocate: {declared} bytes, more than the {usable} \
         bytes of memory the process can use"
    );
    assert!(refused(result.as_ref().map(|_| ()).map_err(clone_error), &message), "resampled read: {result:?}");
    assert!(largest < 1 << 26, "resampled read: {largest} bytes asked for at once");
    // A window that fits is read: band 65,535 whole, 16 MiB.
    let band = r.read_window(terrio::Window::full(w, h), &terrio::WindowOptions { bands: vec![bands], overview: None }).unwrap();
    assert_eq!(band.raster.data.len(), w * h);
    assert!(band.raster.data.to_f64_vec().iter().all(|v| *v == 0.0));
}

// ─── Every driver ───────────────────────────────────────────────────────────────────────────────

/// A raster of `bands` bands of `cols` x `rows` pixels of `data_type`, zeros but for a ramp in its
/// first row, georeferenced in `epsg` with `cell`-unit pixels from `(x0, y1)`.
fn raster(cols: usize, rows: usize, bands: usize, data_type: DataType, epsg: u32, (x0, y1, cell): (f64, f64, f64)) -> RasterDataset {
    let cfg = RasterConfig {
        cols,
        rows,
        bands,
        x_min: x0,
        y_min: y1 - rows as f64 * cell,
        cell_size: cell,
        nodata: -9999.0,
        data_type,
        crs: terrio::crs::SpatialRef::from_epsg(epsg).unwrap().to_crs_info(),
        ..Default::default()
    };
    let values = (0..cols * rows * bands).map(|i| if i < cols { (i % 100) as f64 } else { 0.0 }).collect();
    RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::from_f64_vec(data_type, values)).unwrap(), false)
}

/// Writes `ds` with `driver` (by the name's extension when `None`) at `dir/name`.
fn write(dir: &Path, name: &str, ds: &RasterDataset, driver: Option<RasterDriver>) -> String {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let path = path.to_str().unwrap().to_string();
    terrio::write_raster_dataset(ds, &path, &RasterWriteOptions { driver, ..Default::default() }).unwrap_or_else(|e| panic!("{name}: {e}"));
    path
}

/// The drivers no file is written for here, and why. The test fails for a readable driver that is
/// neither sampled nor listed.
const NOT_SAMPLED: &[(&str, &str)] = &[
    #[cfg(not(feature = "flatgeobuf"))]
    ("GTI", "the native index fixture requires FlatGeobuf"),
    #[cfg(not(feature = "gdal"))]
    ("HFA", "terrio writes Imagine files only through GDAL (the `gdal` feature), and the repository has none"),
];

/// A file of `driver` and the options its open needs, written in `dir`; `None` for the drivers of
/// [`NOT_SAMPLED`]. Rasters of one band where the format holds one, else three; 1,024 x 1,024
/// pixels where the format allows it, so that the pixels outweigh what reading a header takes.
fn sample(driver: RasterDriver, dir: &Path) -> Option<(String, Vec<(&'static str, &'static str)>)> {
    let wgs84 = (10.0, 48.0, 1.0 / 1024.0);
    let one = || raster(1024, 1024, 1, DataType::U8, 4326, wgs84);
    let three = || raster(1024, 1024, 3, DataType::U8, 4326, wgs84);
    let none = Vec::new();
    Some(match driver {
        RasterDriver::GTiff => (write(dir, "gtiff/x.tif", &three(), None), none),
        RasterDriver::Cog => (write(dir, "cog/x.tif", &three(), Some(RasterDriver::Cog)), none),
        RasterDriver::AAIGrid => (write(dir, "aaigrid/x.asc", &one(), None), none),
        // An Esri binary grid is a directory, named by one of its files.
        RasterDriver::AIG => (write(dir, "aig/x/hdr.adf", &one(), Some(driver)), none),
        RasterDriver::EHdr => (write(dir, "ehdr/x.bil", &three(), None), none),
        RasterDriver::GRASSASCIIGrid => (write(dir, "grass/x.txt", &one(), Some(driver)), vec![("TERRIO_WBRASTER_FALLBACK", "YES")]),
        RasterDriver::GSBG => (write(dir, "gsbg/x.grd", &raster(1024, 1024, 1, DataType::F32, 4326, wgs84), None), none),
        RasterDriver::PCRaster => (write(dir, "pcraster/x.map", &one(), None), none),
        RasterDriver::SAGA => (write(dir, "saga/x.sdat", &one(), None), none),
        RasterDriver::RST => (write(dir, "rst/x.rst", &one(), None), none),
        RasterDriver::ERS => (write(dir, "ers/x.ers", &three(), None), none),
        RasterDriver::ENVI => (write(dir, "envi/x.dat", &three(), Some(driver)), none),
        RasterDriver::GPKG => (write(dir, "gpkg/x.gpkg", &three(), None), none),
        RasterDriver::JP2 => (write(dir, "jp2/x.jp2", &three(), None), none),
        RasterDriver::PNG => (write(dir, "png/x.png", &three(), None), none),
        RasterDriver::JPEG => (write(dir, "jpeg/x.jpg", &three(), None), none),
        RasterDriver::WEBP => (write(dir, "webp/x.webp", &three(), None), none),
        RasterDriver::XYZ => (write(dir, "xyz/x.xyz", &raster(512, 512, 1, DataType::U8, 4326, (10.0, 48.0, 1.0 / 512.0)), None), none),
        RasterDriver::Zarr => (write(dir, "zarr/x.zarr", &three(), None), none),
        RasterDriver::DTED => {
            let cell = (10.0 - 0.5 / 1200.0, 48.0 + 0.5 / 1200.0, 1.0 / 1200.0);
            (write(dir, "dted/x.dt1", &raster(1201, 1201, 1, DataType::I16, 4326, cell), None), none)
        }
        RasterDriver::SRTMHGT => {
            let tile = raster(1201, 1201, 1, DataType::I16, 4326, (10.0 - 0.5 / 1200.0, 48.0 + 0.5 / 1200.0, 1.0 / 1200.0));
            (write(dir, "hgt/N47E010.hgt", &tile, None), none)
        }
        RasterDriver::NetCDF => (write(dir, "netcdf/x.nc", &three(), None), none),
        // A netCDF-4 file, which is HDF5, under HDF5's name.
        RasterDriver::HDF5 => {
            let path = dir.join("hdf5/x.h5");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let nc4 = RasterWriteOptions {
                driver: Some(RasterDriver::NetCDF),
                creation_options: vec![("FORMAT".into(), "NC4".into())],
                ..Default::default()
            };
            terrio::write_raster_dataset(&three(), path.to_str().unwrap(), &nc4).unwrap();
            (path.to_str().unwrap().to_string(), none)
        }
        // Imagine files are written through GDAL, and read by terrio's own reader.
        #[cfg(feature = "gdal")]
        RasterDriver::HFA => (write(dir, "hfa/x.img", &three(), None), none),
        // The GDAL backend, by its prefix: a GeoTIFF read by GDAL.
        #[cfg(feature = "gdal")]
        RasterDriver::Gdal => (format!("GDAL:{}", write(dir, "gdal/x.tif", &three(), None)), none),
        // UInt8 bands, written with simple packing: the file is smaller than its Float64 bands.
        RasterDriver::GRIB => (write(dir, "grib/x.grb2", &three(), None), none),
        RasterDriver::VRT => {
            let tif = write(dir, "vrt/x.tif", &three(), None);
            let vrt = dir.join("vrt/x.vrt");
            std::fs::write(&vrt, vrt_of(&tif, 1024, 1024, 3, (1024, 1024))).unwrap();
            (vrt.to_str().unwrap().to_string(), none)
        }
        #[cfg(feature = "flatgeobuf")]
        RasterDriver::GTI => {
            write(dir, "gti/x.tif", &three(), None);
            let index = dir.join("gti/x.gti.fgb");
            terrio::write_vector(&[footprint("x.tif", (10.0, 47.0, 11.0, 48.0))], index.to_str().unwrap(), &Default::default()).unwrap();
            (index.to_str().unwrap().to_string(), none)
        }
        // Two items side by side: the mosaic declares more than either of its assets.
        RasterDriver::STACIT => {
            write(dir, "stacit/x.tif", &three(), None);
            write(dir, "stacit/y.tif", &raster(1024, 1024, 3, DataType::U8, 4326, (11.0, 48.0, 1.0 / 1024.0)), None);
            let items = dir.join("stacit/items.json");
            std::fs::write(&items, stac_items(&[("x.tif", 10.0), ("y.tif", 11.0)], 1024, 1024)).unwrap();
            (format!("STACIT:\"{}\":asset=data", items.to_str().unwrap()), none)
        }
        RasterDriver::STACTA => {
            let tiles = dir.join("stacta");
            stacta_common::write_tiles(&tiles, "Quad/", &[(0, 0, 0), (1, 0, 0), (1, 0, 1), (1, 1, 0), (1, 1, 1)]);
            let templates = serde_json::json!({"data": {"href": "./{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.tif"}});
            (stacta_common::write_item(&tiles, "item.json", &stacta_common::item(None, templates)), none)
        }
        RasterDriver::MBTiles | RasterDriver::PMTiles => {
            let res = 2.0 * 6378137.0 * std::f64::consts::PI / 256.0 / 1024.0;
            let mercator = raster(1024, 1024, 3, DataType::U8, 3857, (1000.0 * res, 3000.0 * res, res));
            let name = if driver == RasterDriver::MBTiles { "tiles/x.mbtiles" } else { "tiles/x.pmtiles" };
            (write(dir, name, &mercator, None), none)
        }
        _ => return None,
    })
}

/// A VRT of `width` x `height` pixels holding the `cols` x `rows` raster of `bands` bands at
/// `source` in its top left corner, band for band.
fn vrt_of(source: &str, cols: usize, rows: usize, bands: usize, (width, height): (usize, usize)) -> String {
    let band = |b: usize| {
        format!(
            "<VRTRasterBand dataType=\"Byte\" band=\"{b}\"><SimpleSource><SourceFilename relativeToVRT=\"0\">{source}</SourceFilename>\
             <SourceBand>{b}</SourceBand><SrcRect xOff=\"0\" yOff=\"0\" xSize=\"{cols}\" ySize=\"{rows}\"/>\
             <DstRect xOff=\"0\" yOff=\"0\" xSize=\"{cols}\" ySize=\"{rows}\"/></SimpleSource></VRTRasterBand>"
        )
    };
    format!("<VRTDataset rasterXSize=\"{width}\" rasterYSize=\"{height}\">{}</VRTDataset>", (1..=bands).map(band).collect::<String>())
}

/// A raster tile index layer of one tile: its footprint and its `location`.
fn footprint(location: &str, (x0, y0, x1, y1): (f64, f64, f64, f64)) -> terrio::Layer {
    let mut l = terrio::Layer::new("index").with_epsg(4326);
    l.add_field(terrio::FieldDef::new("location", terrio::FieldType::Text));
    let ring = vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)];
    l.push(terrio::Feature {
        fid: 1,
        geometry: Some(terrio::Geometry::polygon(ring.into_iter().map(|(x, y)| wbvector::Coord::xy(x, y)).collect(), vec![])),
        attributes: vec![terrio::FieldValue::Text(location.into())],
    });
    l
}

/// A STAC item collection of an item per `(href, west)`, whose `data` asset is the `cols` x `rows`
/// raster at `href`, of 1/1024-degree pixels from `(west, 48)`.
fn stac_items(items: &[(&str, f64)], cols: usize, rows: usize) -> String {
    let cell = 1.0 / 1024.0;
    let features: Vec<serde_json::Value> = items
        .iter()
        .enumerate()
        .map(|(i, &(href, west))| {
            let (east, south) = (west + cols as f64 * cell, 48.0 - rows as f64 * cell);
            serde_json::json!({
                "type": "Feature", "stac_version": "1.0.0", "id": format!("x{i}"), "collection": "c",
                "stac_extensions": ["https://stac-extensions.github.io/projection/v1.1.0/schema.json"],
                "properties": {"datetime": "2024-01-01T00:00:00Z", "proj:epsg": 4326},
                "geometry": {"type": "Polygon", "coordinates": [[[west, south], [east, south], [east, 48.0], [west, 48.0], [west, south]]]},
                "bbox": [west, south, east, 48.0], "links": [],
                "assets": {"data": {"href": href, "type": "image/tiff; application=geotiff", "roles": ["data"],
                                    "proj:shape": [rows, cols], "proj:transform": [cell, 0.0, west, 0.0, -cell, 48.0, 0.0, 0.0, 1.0]}}
            })
        })
        .collect();
    serde_json::json!({"type": "FeatureCollection", "features": features, "links": []}).to_string()
}

/// What each readable driver's sample declares, opened eagerly (`open_raster`), lazily
/// (`open_raster_lazy`), as a dataset of either kind (`open`, where it reads a raster) and to be
/// described (`info::info_path`, where it describes a raster) under
/// bounds set for the thread: opened when it declares exactly what
/// `TERRIO_MAX_DECLARED_BYTES` and `TERRIO_MAX_BANDS` allow, refused with a resource refusal naming
/// the option when it declares one byte or one band more. The drivers are those of
/// `RasterDriver::ALL` that this build reads: each needs a sample in [`sample`] or a reason in
/// [`NOT_SAMPLED`]. The largest allocation of each refused open of a sample of at least a MiB of
/// pixels tells whether it was refused before asking for what the raster declares: the opens
/// refused only after are [`held_first_opens`].
#[test]
fn every_driver_is_refused_by_what_its_rasters_declare() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut held_first = Vec::new();
    let mut measured = Vec::new();
    for &driver in RasterDriver::ALL {
        if !driver.info().read {
            continue;
        }
        let Some((path, options)) = sample(driver, dir.path()) else {
            assert!(
                NOT_SAMPLED.iter().any(|(d, _)| *d == driver.name()),
                "no sample of {}: write one, or say why in NOT_SAMPLED",
                driver.name()
            );
            continue;
        };
        let name = driver.name();
        eprintln!("driver {name}");
        let with = |bound: (&'static str, String), f: &dyn Fn() -> Result<(), String>| {
            let mut all: Vec<(&str, &str)> = options.clone();
            all.push((bound.0, &bound.1));
            scoped(&all, f)
        };
        let (w, h, bands, size, complex) = scoped(&options, || {
            let r = terrio::open_raster_lazy(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
            (r.width(), r.height(), r.band_count(), r.data_type().size_bytes(), r.is_complex())
        });
        let shown = if complex { bands / 2 } else { bands };
        let declared = (w * h * bands * size) as u128;
        // `open` reads a dataset of either kind: a GeoPackage of tiles is read as vector layers.
        let generic = scoped(&options, || matches!(terrio::open(&path), Ok(terrio::Dataset::Raster(_))));
        // `info` describes a dataset of either kind too, opening it as `open` does.
        let described =
            scoped(&options, || matches!(terrio::info::info_path(&path, &Default::default()), Ok(terrio::info::Info::Raster(_))));
        let (eager, lazy, any) =
            (|| outcome(terrio::open_raster(&path)), || outcome(terrio::open_raster_lazy(&path)), || outcome(terrio::open(&path)));
        let describe = || outcome(terrio::info::info_path(&path, &Default::default()));
        let mut opens: Vec<Open> = vec![("eager", &eager), ("lazy", &lazy)];
        if generic {
            opens.push(("generic", &any));
        }
        if described {
            opens.push(("info", &describe));
        }
        for (how, open) in opens {
            let bytes = |n: u128| ("TERRIO_MAX_DECLARED_BYTES", n.to_string());
            assert_eq!(with(bytes(declared), open), Ok(()), "{name}, {how}, within the bound");
            let (result, largest) = largest_during(|| with(bytes(declared - 1), open));
            assert_eq!(result, Err(too_many_bytes(w, h, shown, declared, declared - 1)), "{name}, {how}");
            if COUNTED && declared >= 1 << 20 {
                measured.push(format!("{name} {how}: {largest} of {declared}"));
                if largest as u128 >= declared {
                    held_first.push(format!("{name} {how}"));
                }
            }
            if shown > 1 {
                let most = |n: usize| ("TERRIO_MAX_BANDS", n.to_string());
                assert_eq!(with(most(shown), open), Ok(()), "{name}, {how}, within the band bound");
                assert_eq!(with(most(shown - 1), open), Err(too_many_bands(shown, shown - 1)), "{name}, {how}");
            }
        }
    }
    eprintln!("largest allocations of the refused opens:\n{}", measured.join("\n"));
    if COUNTED {
        assert_eq!(held_first, held_first_opens(), "the opens refused only after asking for what the raster declares");
    }
}

/// Only Imagine reads its entire uncompressed file before its declaration is available.
/// ASCII and fallback binary readers now preflight their headers before reading pixel buffers.
/// XYZ infers its grid from the coordinate records and checks before allocating the dense grid.
fn held_first_opens() -> Vec<String> {
    let drivers: &[RasterDriver] = &[
        #[cfg(feature = "gdal")]
        RasterDriver::HFA,
    ];
    drivers.iter().flat_map(|d| ["eager", "lazy", "generic", "info"].map(|how| format!("{} {how}", d.name()))).collect()
}

// ─── Every public entry point ───────────────────────────────────────────────────────────────────

/// What an entry point opened: its width, height, bands (a complex band counted once) and bytes
/// per sample.
type Dims = (usize, usize, usize, usize);

fn lazy_dims(r: &terrio::LazyRaster) -> Dims {
    let bands = if r.is_complex() { r.band_count() / 2 } else { r.band_count() };
    (r.width(), r.height(), bands, r.data_type().size_bytes() * if r.is_complex() { 2 } else { 1 })
}

fn dataset_dims(ds: &RasterDataset) -> Dims {
    let r = &ds.raster;
    let bands = if ds.complex { r.bands / 2 } else { r.bands };
    (r.cols, r.rows, bands, r.data_type.size_bytes() * if ds.complex { 2 } else { 1 })
}

fn any_dims(ds: terrio::Dataset) -> Dims {
    match ds {
        terrio::Dataset::Raster(r) => dataset_dims(&r),
        terrio::Dataset::Vector(_) => panic!("a vector dataset"),
    }
}

/// A file of `bytes` caller-fetched in full (see [`terrio::sparse`]).
fn sparse(bytes: &[u8]) -> terrio::sparse::SparseFile {
    let file = terrio::sparse::SparseFile::new(bytes.len() as u64);
    file.insert(0, bytes);
    file
}

/// An entry point under test, opening its dataset.
type Opener<'a> = Box<dyn Fn() -> terrio::Result<Dims> + 'a>;

/// A row of [`every_public_entry_point_is_refused_by_what_a_raster_declares`].
fn row<'a>(name: &'a str, open: impl Fn() -> terrio::Result<Dims> + 'a) -> (&'a str, Opener<'a>) {
    (name, Box::new(open))
}

/// Every public entry point that opens a raster from its location, its bytes, a caller-fetched
/// source or a description, a row each: the public functions and constructors that return a
/// `LazyRaster`, a `RasterDataset`, a raster `Dataset` or a `RasterCatalog` from one, listed from
/// their signatures (a new one needs a row here). Not listed: constructors from pixels already in
/// memory (`LazyRaster::from`), and the multidimensional API (`terrio::multidim`), which reads
/// arrays by selection rather than rasters. Each opens a 64 x 48 raster of three UInt8 bands, a
/// VRT holding it in a 128 x 96 raster, a pansharpened VRT of three one-band 64 x 48 rasters onto
/// a 128 x 96 panchromatic band (the VRTs declare more than any of their sources, so that what they
/// declare is what is refused), or another format's sample. Without bounds each opens its dataset;
/// under `TERRIO_MAX_DECLARED_BYTES` set to what it declares, and `TERRIO_MAX_BANDS` to its bands,
/// each still opens it; one byte or one band less and each refuses it, naming the option.
#[test]
fn every_public_entry_point_is_refused_by_what_a_raster_declares() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let wgs84 = (10.0, 48.0, 1.0 / 64.0);
    let ds = raster(64, 48, 3, DataType::U8, 4326, wgs84);
    let tif = write(dir.path(), "x.tif", &ds, None);
    let tif = tif.as_str();
    let bytes = std::fs::read(tif).unwrap();
    let vrt = dir.path().join("x.vrt");
    std::fs::write(&vrt, vrt_of(tif, 64, 48, 3, (128, 96))).unwrap();
    let vrt = vrt.to_str().unwrap();
    let vrt_text = std::fs::read_to_string(vrt).unwrap();
    let pan = write(dir.path(), "pan.tif", &raster(128, 96, 1, DataType::U8, 4326, (10.0, 48.0, 1.0 / 128.0)), None);
    let spectral: Vec<(String, usize)> =
        (1..=3).map(|i| (write(dir.path(), &format!("s{i}.tif"), &raster(64, 48, 1, DataType::U8, 4326, wgs84), None), 1)).collect();
    let pansharpened = terrio::ops::pansharpen::vrt::PansharpenVrt::from_paths(&pan, &spectral, &Default::default()).unwrap().to_xml();
    let translate = terrio::ops::translate::TranslateOptions::default();
    let auto = terrio::ops::overview::OverviewSelection::Auto;
    let index = footprint("x.tif", (10.0, 47.25, 11.0, 48.0));
    let mut openers: Vec<(&str, Opener)> = vec![
        row("open", || terrio::open(tif).map(any_dims)),
        row("open_raster", || terrio::open_raster(tif).map(|d| dataset_dims(&d))),
        row("open_raster_lazy", || terrio::open_raster_lazy(tif).map(|r| lazy_dims(&r))),
        row("LazyRaster::open", || terrio::LazyRaster::open(tif).map(|r| lazy_dims(&r))),
        row("open_raster_with_tiff_limits", || terrio::open_raster_with_tiff_limits(tif, Default::default()).map(|d| dataset_dims(&d))),
        row("LazyRaster::open_tiff", || terrio::LazyRaster::open_tiff(tif).map(|r| lazy_dims(&r.unwrap()))),
        row("LazyRaster::open_tiff_with_limits", || {
            terrio::LazyRaster::open_tiff_with_limits(tif, Default::default()).map(|r| lazy_dims(&r.unwrap()))
        }),
        row("LazyRaster::from_tiff_bytes", || terrio::LazyRaster::from_tiff_bytes(bytes.clone()).map(|r| lazy_dims(&r.unwrap()))),
        row("LazyRaster::from_tiff_bytes_with_limits", || {
            terrio::LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), Default::default()).map(|r| lazy_dims(&r.unwrap()))
        }),
        row("LazyRaster::from_tiff_shared_bytes", || {
            terrio::LazyRaster::from_tiff_shared_bytes(std::sync::Arc::new(bytes.clone())).map(|r| lazy_dims(&r.unwrap()))
        }),
        row("LazyRaster::from_sparse", || terrio::LazyRaster::from_sparse(sparse(&bytes)).map(|r| lazy_dims(&r.unwrap()))),
        row("LazyRaster::from_sparse_with_limits", || {
            terrio::LazyRaster::from_sparse_with_limits(sparse(&bytes), Default::default()).map(|r| lazy_dims(&r.unwrap()))
        }),
        row("memory::open_bytes", || terrio::memory::open_bytes(bytes.clone(), "x.tif").map(any_dims)),
        row("memory::open_raster_bytes", || terrio::memory::open_raster_bytes(bytes.clone(), "x.tif").map(|d| dataset_dims(&d))),
        row("memory::open_raster_lazy_bytes", || terrio::memory::open_raster_lazy_bytes(bytes.clone(), "x.tif").map(|r| lazy_dims(&r))),
        row("memory::open_raster_lazy_bytes_or_whole", || {
            terrio::memory::open_raster_lazy_bytes_or_whole(bytes.clone(), "x.tif").map(|opened| match opened {
                terrio::memory::OpenedRasterBytes::Lazy(r) => lazy_dims(&r),
                terrio::memory::OpenedRasterBytes::Whole(d) => dataset_dims(&d),
            })
        }),
        row("ops::translate::translate_path", || terrio::ops::translate::translate_path(tif, &translate, auto).map(|d| dataset_dims(&d))),
        row("ops::translate::translate_bytes", || {
            terrio::ops::translate::translate_bytes(bytes.clone(), "x.tif", &translate, auto).map(|d| dataset_dims(&d))
        }),
        row("ops::vrt::read_vrt", || terrio::ops::vrt::read_vrt(vrt).map(|d| dataset_dims(&d))),
        row("ops::vrt::open_virtual_raster", || terrio::ops::vrt::open_virtual_raster(vrt).map(|r| lazy_dims(&r))),
        row("VirtualRaster::into_lazy", || {
            terrio::ops::vrt::VirtualRaster::from_xml(&vrt_text)?.into_lazy(dir.path(), HashMap::new()).map(|r| lazy_dims(&r))
        }),
        row("VrtDataset::materialize", || terrio::ops::vrt::VrtDataset::open(vrt)?.materialize(dir.path()).map(|d| dataset_dims(&d))),
        row("PansharpenVrt::into_lazy", || {
            let description = terrio::ops::pansharpen::vrt::PansharpenVrt::from_xml(&pansharpened)?;
            description.into_lazy(dir.path(), HashMap::new()).map(|r| lazy_dims(&r))
        }),
        row("RasterCatalog::from_layer", || {
            let sources = HashMap::from([("x.tif".to_string(), ds.clone())]);
            terrio::catalog::RasterCatalog::from_layer(index.clone(), sources, Default::default()).map(|c| dataset_dims(c.header()))
        }),
        row("RasterCatalog::from_lazy_layer", || {
            let sources = HashMap::from([("x.tif".to_string(), terrio::LazyRaster::from(ds.clone()))]);
            terrio::catalog::RasterCatalog::from_lazy_layer(index.clone(), sources, Default::default()).map(|c| dataset_dims(c.header()))
        }),
        row("RasterCatalog::from_sources", || {
            let source = terrio::catalog::CatalogInput::raster(terrio::LazyRaster::from(ds.clone()));
            let sources = HashMap::from([("x.tif".to_string(), source)]);
            terrio::catalog::RasterCatalog::from_sources(index.clone(), sources, Default::default()).map(|c| dataset_dims(c.header()))
        }),
    ];
    #[cfg(feature = "flatgeobuf")]
    let gti = {
        let path = dir.path().join("x.gti.fgb");
        terrio::write_vector(std::slice::from_ref(&index), path.to_str().unwrap(), &Default::default()).unwrap();
        path.to_str().unwrap().to_string()
    };
    #[cfg(feature = "flatgeobuf")]
    openers.push(row("RasterCatalog::open", move || {
        terrio::catalog::RasterCatalog::open(&gti, Default::default()).map(|c| dataset_dims(c.header()))
    }));
    #[cfg(feature = "tiles")]
    let archive = {
        let res = 2.0 * 6378137.0 * std::f64::consts::PI / 256.0 / 1024.0;
        write(dir.path(), "x.pmtiles", &raster(300, 200, 3, DataType::U8, 3857, (1000.0 * res, 3000.0 * res, res)), None)
    };
    #[cfg(feature = "tiles")]
    {
        let (a, b) = (archive.clone(), archive.clone());
        let archive_bytes = std::fs::read(&archive).unwrap();
        openers.push(row("LazyRaster::from_tile_archive", move || {
            terrio::LazyRaster::from_tile_archive(terrio::tiles::TileArchive::open(&a)?).map(|r| lazy_dims(&r))
        }));
        openers.push(row("TileArchive::into_dataset", move || {
            terrio::tiles::TileArchive::open(&b)?.into_dataset("x.pmtiles", &Default::default()).map(any_dims)
        }));
        openers.push(row("tiles::open_dataset", move || terrio::tiles::open_dataset(&archive, &Default::default()).map(any_dims)));
        // Bytes read by window by their own reader, not through the opens of files (as are JPEG
        // 2000 and GRIB bytes, below).
        openers.push(row("memory::open_raster_lazy_bytes, PMTiles", move || {
            terrio::memory::open_raster_lazy_bytes(archive_bytes.clone(), "x.pmtiles").map(|r| lazy_dims(&r))
        }));
    }
    #[cfg(feature = "jp2")]
    {
        let jp2 = std::fs::read(write(dir.path(), "x.jp2", &ds, None)).unwrap();
        openers.push(row("memory::open_raster_lazy_bytes, JPEG 2000", move || {
            terrio::memory::open_raster_lazy_bytes(jp2.clone(), "x.jp2").map(|r| lazy_dims(&r))
        }));
    }
    #[cfg(feature = "grib")]
    {
        let grib = std::fs::read(write(dir.path(), "x.grb2", &ds, None)).unwrap();
        openers.push(row("memory::open_raster_lazy_bytes, GRIB", move || {
            terrio::memory::open_raster_lazy_bytes(grib.clone(), "x.grb2").map(|r| lazy_dims(&r))
        }));
    }
    #[cfg(feature = "zarr")]
    let (objects, store_files) = {
        let store = write(dir.path(), "z.zarr", &ds, None);
        let objects = std::sync::Arc::new(terrio::sparse::SparseObjects::new());
        // Fetched until the array opens, as a caller fetches them.
        for _ in 0..32 {
            if terrio::LazyRaster::from_zarr_objects(objects.clone(), "z.zarr", "").is_ok() && !objects.has_missing() {
                break;
            }
            for (key, range) in objects.missing() {
                let data = std::fs::read(Path::new(&store).join(&key)).ok().map(|b| match range {
                    terrio::sparse::ObjectRange::Whole => b,
                    terrio::sparse::ObjectRange::At(o, l) => b[o as usize..(o + l) as usize].to_vec(),
                    terrio::sparse::ObjectRange::Suffix(n) => b[b.len() - n as usize..].to_vec(),
                });
                objects.insert(&key, range, data);
            }
        }
        (objects, terrio::multidim::open(&store).unwrap().to_zarr_store().unwrap())
    };
    #[cfg(feature = "zarr")]
    {
        openers.push(row("LazyRaster::from_zarr_objects", move || {
            terrio::LazyRaster::from_zarr_objects(objects.clone(), "z.zarr", "").map(|r| lazy_dims(&r))
        }));
        openers
            .push(row("memory::open_zarr_store", move || terrio::memory::open_zarr_store(store_files.clone()).map(|d| dataset_dims(&d))));
    }
    // A HEC-RAS file's bytes, which a raster open reads with the HDF5 reader rather than through
    // `memory::open_bytes`.
    #[cfg(feature = "hecras")]
    {
        let hecras = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hecras/results_mesh_xs.h5")).unwrap();
        openers.push(row("memory::open_raster_bytes, HEC-RAS", move || {
            terrio::memory::open_raster_bytes(hecras.clone(), "plan.p01.hdf").map(|d| dataset_dims(&d))
        }));
    }
    #[cfg(feature = "gdal")]
    {
        openers.push(row("gdal_backend::open", || terrio::gdal_backend::open(tif).map(any_dims)));
        openers.push(row("gdal_backend::open_raster", || terrio::gdal_backend::open_raster(tif).map(|d| dataset_dims(&d))));
        openers.push(row("gdal_backend::open_bytes", || terrio::gdal_backend::open_bytes(bytes.clone(), "x.tif").map(any_dims)));
        openers.push(row("open_raster GDAL:", || terrio::open_raster(&format!("GDAL:{tif}")).map(|d| dataset_dims(&d))));
    }
    for (name, open) in openers {
        let (w, h, bands, size) = open().unwrap_or_else(|e| panic!("{name}: {e}"));
        let declared = (w * h * bands * size) as u128;
        let bytes = |n: u128| n.to_string();
        let within =
            scoped(&[("TERRIO_MAX_DECLARED_BYTES", &bytes(declared)), ("TERRIO_MAX_BANDS", &bands.to_string())], || outcome(open()));
        assert_eq!(within, Ok(()), "{name}, within the bounds");
        let over = scoped(&[("TERRIO_MAX_DECLARED_BYTES", &bytes(declared - 1))], || open().map(|_| ()));
        assert!(over.as_ref().is_err_and(terrio::Error::is_resource_refusal), "{name}: {over:?}");
        assert_eq!(outcome(over), Err(too_many_bytes(w, h, bands, declared, declared - 1)), "{name}");
        if bands > 1 {
            let over = scoped(&[("TERRIO_MAX_BANDS", &(bands - 1).to_string())], || outcome(open()));
            assert_eq!(over, Err(too_many_bands(bands, bands - 1)), "{name}");
        }
    }
}

// ─── What a VRT or a catalog opens ──────────────────────────────────────────────────────────────

/// [`outcome`], with an error other than a resource refusal shown as such.
fn refusal<T>(result: terrio::Result<T>) -> Result<(), String> {
    result.map(|_| ()).map_err(|e| if e.is_resource_refusal() { e.to_string() } else { format!("not a resource refusal: {e}") })
}

/// The rasters a VRT or a catalog opens are bounded as they are opened. A VRT, or a catalog, of the
/// 16 x 16 top-left corner of a 64 x 48 raster of three UInt8 bands declares 768 bytes, within a
/// bound of 768 that its source's 9,216 exceed:
/// - the VRT opened lazily opens, and its read is refused when it opens the source;
/// - the VRT read whole, or materialized, is refused;
/// - the catalog, which opens its first source to learn its grid, is refused when it is opened;
///   given its grid, it opens, and its read is refused when it opens the source.
///
/// Each refusal is a resource refusal naming what the source declares; under a bound of 9,216
/// bytes each is read. `VrtDataset::materialize` checks what its VRT declares before it opens any
/// source: over a missing source, a VRT declaring more than the bound is refused for what it
/// declares, and one within it fails for its missing source.
#[test]
fn the_rasters_a_vrt_or_a_catalog_opens_are_bounded_as_they_are_opened() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let tif = write(dir.path(), "x.tif", &raster(64, 48, 3, DataType::U8, 4326, (10.0, 48.0, 1.0 / 64.0)), None);
    let vrt = dir.path().join("corner.vrt");
    std::fs::write(&vrt, vrt_of(&tif, 16, 16, 3, (16, 16))).unwrap();
    let vrt = vrt.to_str().unwrap();
    let source = Err(too_many_bytes(64, 48, 3, 9216, 768));
    let bound = |most: &str, f: &dyn Fn() -> Result<(), String>| scoped(&[("TERRIO_MAX_DECLARED_BYTES", most)], f);
    let lazy = || terrio::open_raster_lazy(vrt);
    assert_eq!(bound("768", &|| refusal(lazy())), Ok(()), "the VRT, opened lazily");
    let read = || refusal(lazy().and_then(|mut r| r.read()));
    let whole = || refusal(terrio::open_raster(vrt));
    let materialized = || refusal(terrio::ops::vrt::VrtDataset::open(vrt).and_then(|v| v.materialize(dir.path())));
    let mut opens: Vec<Open> = vec![("the VRT, read", &read), ("the VRT, read whole", &whole), ("the VRT, materialized", &materialized)];
    #[cfg(feature = "flatgeobuf")]
    let gti = {
        let path = dir.path().join("corner.gti.fgb");
        let index = footprint("x.tif", (10.0, 47.75, 10.25, 48.0));
        terrio::write_vector(std::slice::from_ref(&index), path.to_str().unwrap(), &Default::default()).unwrap();
        path.to_str().unwrap().to_string()
    };
    #[cfg(feature = "flatgeobuf")]
    let grid = terrio::catalog::CatalogOptions { resolution: Some((1.0 / 64.0, 1.0 / 64.0)), bands: Some(3), ..Default::default() };
    #[cfg(feature = "flatgeobuf")]
    let catalog = || refusal(terrio::catalog::RasterCatalog::open(&gti, Default::default()));
    #[cfg(feature = "flatgeobuf")]
    let given = || terrio::catalog::RasterCatalog::open(&gti, grid.clone());
    #[cfg(feature = "flatgeobuf")]
    let given_read = || refusal(given().and_then(|mut c| c.read_window(terrio::Window::new(0, 0, 16, 16), &[])));
    #[cfg(feature = "flatgeobuf")]
    {
        assert_eq!(bound("768", &|| refusal(given())), Ok(()), "the catalog given its grid, opened");
        opens.push(("the catalog, opened", &catalog));
        opens.push(("the catalog given its grid, read", &given_read));
    }
    for (name, open) in opens {
        assert_eq!(bound("768", open), source, "{name}");
        assert_eq!(bound("9216", open), Ok(()), "{name}, within the bound");
    }
    // What a VRT declares is checked before any of its sources is opened.
    let missing = dir.path().join("missing.vrt");
    std::fs::write(&missing, vrt_of(dir.path().join("absent.tif").to_str().unwrap(), 64, 48, 3, (128, 96))).unwrap();
    let missing = missing.to_str().unwrap();
    let materialize = || refusal(terrio::ops::vrt::VrtDataset::open(missing).and_then(|v| v.materialize(dir.path())));
    assert_eq!(bound("36863", &materialize), Err(too_many_bytes(128, 96, 3, 36864, 36863)), "a VRT over a missing source");
    let within = bound("36864", &materialize);
    assert!(within.as_ref().is_err_and(|e| e.starts_with("not a resource refusal")), "a VRT over a missing source: {within:?}");
}

// ─── Operations that open a raster or a vector dataset ──────────────────────────────────────────

/// An open of [`render_check_and_clip_templates_keep_a_rasters_refusal`].
type PathOpen<'a> = (&'a str, &'a dyn Fn(&str) -> terrio::Result<()>);

/// `render`, `check` and clip templates open a dataset as a raster or as a vector dataset, and
/// keep a raster's refusal: of a 64 x 48 GeoTIFF of three UInt8 bands and of a GeoPackage of its
/// tiles, each is refused one byte below what it declares with the resource refusal naming the
/// option, and opened at what it declares. (`render` tries a raster, then a vector dataset;
/// `check` and a clip template open an identified raster as one.)
#[test]
fn render_check_and_clip_templates_keep_a_rasters_refusal() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let ds = raster(64, 48, 3, DataType::U8, 4326, (10.0, 48.0, 1.0 / 64.0));
    let tif = write(dir.path(), "x.tif", &ds, None);
    #[cfg(feature = "sqlite")]
    let gpkg = write(dir.path(), "x.gpkg", &ds, Some(RasterDriver::GPKG));
    let opens: [PathOpen; 3] = [
        ("render::open_source", &|p| terrio::ops::render::open_source(p, &[], false).map(|_| ())),
        ("check::check_path", &|p| terrio::check::check_path(p, &Default::default()).map(|_| ())),
        ("ClipRegion::from_path", &|p| terrio::ops::vector_clip::ClipRegion::from_path(p, &Default::default()).map(|_| ())),
    ];
    let paths = [
        tif.as_str(),
        #[cfg(feature = "sqlite")]
        gpkg.as_str(),
    ];
    for path in paths {
        let r = terrio::open_raster_lazy(path).unwrap();
        let (w, h, bands) = (r.width(), r.height(), r.band_count());
        let declared = (w * h * bands * r.data_type().size_bytes()) as u128;
        let bound = |most: u128, open: &dyn Fn(&str) -> terrio::Result<()>| {
            scoped(&[("TERRIO_MAX_DECLARED_BYTES", &most.to_string())], || refusal(open(path)))
        };
        for (name, open) in opens {
            assert_eq!(bound(declared, open), Ok(()), "{name}, {path}, within the bound");
            assert_eq!(bound(declared - 1, open), Err(too_many_bytes(w, h, bands, declared, declared - 1)), "{name}, {path}");
        }
    }
}

// ─── The process environment ────────────────────────────────────────────────────────────────────

/// Names the child process's scenario: the directory of its samples.
const CHILD: &str = "TERRIO_TEST_DECLARED_SIZE_IN_ENVIRONMENT";

/// `TERRIO_MAX_DECLARED_BYTES` and `TERRIO_MAX_BANDS` set in the process environment apply to every
/// open: the test runs itself again in a child process with each set (to 1,000 bytes, to one band),
/// where every driver's sample is refused, eagerly and lazily, but a single-band one under the band
/// bound; a value a thread's scope sets overrides the environment's there, as for the other
/// options.
#[test]
fn the_bounds_apply_from_the_process_environment() {
    if let Ok(dir) = std::env::var(CHILD) {
        return in_child(Path::new(&dir));
    }
    let _serial = serial();
    for (option, value) in [("TERRIO_MAX_DECLARED_BYTES", "1000"), ("TERRIO_MAX_BANDS", "1")] {
        let dir = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "the_bounds_apply_from_the_process_environment", "--test-threads=1", "--nocapture"])
            .env(CHILD, dir.path())
            .env_remove("TERRIO_MAX_DECLARED_BYTES")
            .env_remove("TERRIO_MAX_BANDS")
            .env(option, value)
            .output()
            .unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && text.contains("1 passed"), "{option}={value}:\n{text}");
    }
}

/// The child of [`the_bounds_apply_from_the_process_environment`].
fn in_child(dir: &Path) {
    let bytes_bound = std::env::var("TERRIO_MAX_DECLARED_BYTES").ok();
    let option = if bytes_bound.is_some() { "TERRIO_MAX_DECLARED_BYTES" } else { "TERRIO_MAX_BANDS" };
    let mut checked = 0;
    for &driver in RasterDriver::ALL {
        if !driver.info().read {
            continue;
        }
        let Some((path, options)) = sample(driver, dir) else { continue };
        let name = driver.name();
        // What it declares, the environment's bound unset in a scope.
        terrio::config::push_scope(
            options.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).chain([(option.to_string(), None)]).collect(),
        );
        let opened = terrio::open_raster_lazy(&path).map(|r| lazy_dims(&r));
        terrio::config::pop_scope();
        let (w, h, bands, size) = opened.unwrap_or_else(|e| panic!("{name}: {e}"));
        let declared = (w * h * bands * size) as u128;
        // Refused by the environment's bound: the raster itself, or a source its open reads (a
        // STACIT mosaic's first asset, a STACTA item's first tile).
        let refused = |result: Result<(), String>| match (&result, bytes_bound.is_some()) {
            (Err(m), true) => m.starts_with("the raster declares ") && m.ends_with(", more than TERRIO_MAX_DECLARED_BYTES=1000"),
            (Err(m), false) => bands > 1 && m == &too_many_bands(bands, 1),
            (Ok(()), bytes) => !bytes && bands == 1,
        };
        let own = if bytes_bound.is_some() { declared.to_string() } else { bands.to_string() };
        let opens: [Open; 2] = [("eager", &|| outcome(terrio::open_raster(&path))), ("lazy", &|| outcome(terrio::open_raster_lazy(&path)))];
        for (how, open) in opens {
            let result = scoped(&options, open);
            assert!(refused(result.clone()), "{name}, {how}: {result:?}");
            if bytes_bound.is_some() && !matches!(driver, RasterDriver::STACIT | RasterDriver::STACTA) {
                assert_eq!(result, Err(too_many_bytes(w, h, bands, declared, 1000)), "{name}, {how}");
            }
            let mut lifted = options.clone();
            lifted.push((option, &own));
            assert_eq!(scoped(&lifted, open), Ok(()), "{name}, {how}: the scope's value");
        }
        checked += 1;
    }
    assert!(checked > 20, "{checked} drivers checked");
}

// ─── The writers operations stream to ───────────────────────────────────────────────────────────

/// A raster of `bands` UInt8 bands of `cols` x `rows` pixels, without its pixels: what a writer is
/// created from.
fn header(cols: usize, rows: usize, bands: usize) -> RasterDataset {
    let raster = terrio::Raster {
        cols,
        rows,
        bands,
        x_min: 0.0,
        y_min: 0.0,
        cell_size_x: 1.0,
        cell_size_y: 1.0,
        nodata: -9999.0,
        data_type: DataType::U8,
        crs: Default::default(),
        metadata: Vec::new(),
        data: RasterData::new_filled(DataType::U8, 0, 0.0),
    };
    RasterDataset::new(raster, false)
}

/// GDAL 3.13.3 exposes a sidecar mask as part of the raster, not as all-valid fallback.
#[test]
fn sidecar_mask_declaration_refusals_are_not_discarded() {
    let _serial = serial();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("masked.tif");
    let mut image = header(16, 16, 1);
    image.raster.data = RasterData::U8(vec![1; 256]);
    let options = RasterWriteOptions { driver: Some(RasterDriver::GTiff), ..Default::default() };
    terrio::write_raster_dataset(&image, path.to_str().unwrap(), &options).unwrap();
    let mut mask = image.clone();
    mask.raster.data_type = DataType::U16;
    mask.raster.data = RasterData::U16(vec![1; 256]);
    let sidecar = directory.path().join("masked.tif.msk");
    terrio::write_raster_dataset(&mask, sidecar.to_str().unwrap(), &options).unwrap();
    let result = scoped(&[("TERRIO_MAX_DECLARED_BYTES", "256")], || terrio::open_raster_lazy(path.to_str().unwrap()));
    assert!(matches!(result, Err(terrio::Error::ResourceLimit(_))));
}

/// GDAL 3.13.3's VRT header declares its promoted band type without reading sources.
#[test]
fn opening_a_vrt_description_checks_its_declaration() {
    let _serial = serial();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("header.vrt");
    std::fs::write(&path, "<VRTDataset rasterXSize=\"32\" rasterYSize=\"16\"><VRTRasterBand dataType=\"UInt16\" band=\"1\"/></VRTDataset>")
        .unwrap();
    let result = scoped(&[("TERRIO_MAX_DECLARED_BYTES", "512")], || terrio::ops::vrt::VrtDataset::open(path.to_str().unwrap()));
    assert!(matches!(result, Err(terrio::Error::ResourceLimit(_))));
}

/// The GeoTIFF and COG writers that operations stream their output to (`terrio translate` and
/// `convert`, `GeoTiffWriter`) keep a row of their blocks of every band until it is complete, held
/// twice while a strip is appended: a writer whose rows cannot fit in the usable memory is refused
/// before its file is created, here a row of 512-row tiles of 2,048 bands of 2^20 pixels (2 TiB).
/// The same raster written in one-row strips holds a row (twice), and its writer is created. Where
/// terrio cannot read the usable memory (Windows, macOS), the test is skipped.
#[test]
#[cfg(target_pointer_width = "64")]
fn the_streaming_writers_refuse_rows_they_cannot_hold() {
    let _serial = serial();
    let (cols, rows, bands) = (1usize << 20, 600usize, 2048usize);
    let held = 2 * 512 * cols * bands;
    let Some(usable) = terrio::usable_memory().filter(|m| (*m as u128) < held as u128) else {
        eprintln!("skipped: the usable memory is not known here, or is more than {held} bytes");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ds = header(cols, rows, bands);
    let message = format!(
        "the rows a GeoTIFF writer holds (512 rows of {cols} pixels in {bands} band(s) of 1-byte samples, a row of its blocks, twice \
         while a strip is appended) are too large to allocate: {held} bytes, more than the {usable} bytes of memory the process can use"
    );
    let tiled = [
        ("cog.tif", RasterWriteOptions { driver: Some(RasterDriver::Cog), ..Default::default() }),
        ("tiled.tif", RasterWriteOptions { driver: Some(RasterDriver::GTiff), tile_size: Some(512), ..Default::default() }),
    ];
    for (name, options) in tiled {
        let path = dir.path().join(name);
        let (result, largest) = largest_during(|| terrio::GeoTiffWriter::create(path.to_str().unwrap(), &ds, &options).map(|_| ()));
        assert!(refused(result.as_ref().map(|_| ()).map_err(clone_error), &message), "{name}: {result:?}");
        assert!(largest < 1 << 24, "{name}: {largest} bytes asked for at once");
        assert!(!path.exists(), "{name}: the refused writer left its file");
    }
    if usable as u128 > (2 * cols * bands) as u128 {
        let path = dir.path().join("striped.tif");
        let striped = RasterWriteOptions { driver: Some(RasterDriver::GTiff), ..Default::default() };
        assert!(terrio::GeoTiffWriter::create(path.to_str().unwrap(), &ds, &striped).is_ok(), "a writer in one-row strips");
    }
}

/// The row a writer holds is counted twice, as it is held while a strip is appended: a row of
/// 512-row tiles of 2^20 pixels in as many 1-byte bands as fit once in the usable memory (at least
/// one, 512 MiB) does not fit twice, and its writer is refused when it is created. Where terrio
/// cannot read the usable memory (Windows, macOS), or it is less than one such band, the test is
/// skipped.
#[test]
#[cfg(target_pointer_width = "64")]
fn a_writer_whose_rows_fit_once_but_not_twice_is_refused() {
    let _serial = serial();
    let (cols, rows) = (1usize << 20, 600usize);
    let band_row = 512 * cols;
    let Some(usable) = terrio::usable_memory().and_then(|m| usize::try_from(m).ok()).filter(|m| *m >= band_row) else {
        eprintln!("skipped: the usable memory is not known here, or is less than {band_row} bytes");
        return;
    };
    let bands = usable / band_row;
    let once = band_row * bands;
    assert!(once <= usable && 2 * once > usable, "{bands} band(s): {once} bytes once, {usable} usable");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiled.tif");
    let options = RasterWriteOptions { driver: Some(RasterDriver::GTiff), tile_size: Some(512), ..Default::default() };
    let message = format!(
        "the rows a GeoTIFF writer holds (512 rows of {cols} pixels in {bands} band(s) of 1-byte samples, a row of its blocks, twice \
         while a strip is appended) are too large to allocate: {} bytes, more than the {usable} bytes of memory the process can use",
        2 * once
    );
    let result = terrio::GeoTiffWriter::create(path.to_str().unwrap(), &header(cols, rows, bands), &options).map(|_| ());
    assert!(refused(result.as_ref().map(|_| ()).map_err(clone_error), &message), "{result:?}");
    assert!(!path.exists(), "the refused writer left its file");
}

/// Header limits must win over corrupt pixel tokens, rather than decode the grid first.
/// Reference: independently specified Esri ASCII grid header; GDAL 3.13.3 reads the valid control.
#[test]
fn ascii_declarations_are_checked_before_pixel_parsing() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.asc");
    let text = "ncols 200\nnrows 200\ncellsize 1\ninvalid pixels\n";
    std::fs::write(&path, text).unwrap();
    scoped(&[("TERRIO_MAX_DECLARED_BYTES", "16")], || {
        assert!(matches!(terrio::open_raster(path.to_str().unwrap()), Err(terrio::Error::ResourceLimit(_))));
        assert!(matches!(terrio::memory::open_raster_bytes(text.as_bytes().to_vec(), "bad.asc"), Err(terrio::Error::ResourceLimit(_))));
    });
}

/// Header scanning must reach the final GRASS type even beyond an arbitrary prefix boundary.
#[test]
fn long_ascii_headers_are_checked_before_the_pixel_tokens() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grid.txt");
    let header = format!(
        "north: 1\nsouth: 0\neast: 1\nwest: 0\nrows: 1\ncols: 1\ntype: int\n{}type: double\n",
        "ignored: header extension\n".repeat(4096)
    );
    std::fs::write(&path, format!("{header}1.5\n")).unwrap();
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "8")], || {
        let r = terrio::open_raster(path.to_str().unwrap()).unwrap();
        assert_eq!(r.raster.data.to_f64_vec(), vec![1.5]);
    });
    std::fs::write(&path, format!("{header}invalid pixels\n")).unwrap();
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "4")], || {
        assert!(matches!(terrio::open_raster(path.to_str().unwrap()), Err(terrio::Error::ResourceLimit(_))));
    });
}

/// Reading an ASCII header must not collect an entire unwrapped pixel row before refusing it.
#[test]
fn ascii_declaration_refusal_does_not_allocate_a_long_pixel_line() {
    use std::io::Write;
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    for (name, header) in [
        ("long-row.asc", "ncols 1024\nnrows 16384\nxllcorner 0\nyllcorner 0\ncellsize 1\n"),
        ("long-row.txt", "north: 1\nsouth: 0\neast: 1\nwest: 0\nrows: 16384\ncols: 1024\ntype: int\n"),
    ] {
        let path = dir.path().join(name);
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        file.write_all(header.as_bytes()).unwrap();
        let cells = "7 ".repeat(4096);
        for _ in 0..4096 {
            file.write_all(cells.as_bytes()).unwrap();
        }
        drop(file);
        let (result, largest) = largest_during(|| {
            scoped(&[("TERRIO_MAX_DECLARED_BYTES", "16777216"), ("TERRIO_WBRASTER_FALLBACK", "YES")], || {
                terrio::open_raster(path.to_str().unwrap())
            })
        });
        assert!(matches!(result, Err(terrio::Error::ResourceLimit(_))), "{name}");
        if COUNTED {
            assert!(largest < 1 << 20, "{name}: the header check requested a {largest}-byte buffer for the pixel line");
        }
    }
}

/// Bounding line buffers must still read the complete meaning of long header values.
#[test]
fn long_ascii_header_values_keep_their_dimensions_and_sample_type() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long-values.txt");
    let header = format!(
        "north: 1\nsouth: 0\neast: 1\nwest: 0\n{}rows{}: {}1\ncols: 2\ntype{}: {}double\n",
        " ".repeat(128 << 10),
        " ".repeat(128 << 10),
        "0".repeat(128 << 10),
        " ".repeat(128 << 10),
        " ".repeat(128 << 10)
    );
    std::fs::write(&path, format!("{header}1.5 2.5\n")).unwrap();
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "16")], || {
        let out = terrio::open_raster(path.to_str().unwrap()).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows), (2, 1));
        assert_eq!(out.raster.data.to_f64_vec(), vec![1.5, 2.5]);
    });
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "15")], || {
        assert!(matches!(terrio::open_raster(path.to_str().unwrap()), Err(terrio::Error::ResourceLimit(_))));
    });
}

/// The bounded detector retains the fallback reader's sample type and binary signature priority.
#[test]
fn fallback_ascii_detection_keeps_types_and_binary_signatures() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("esri.txt");
    let header = "unknown extension\nncols=2\nnrows=2\nxllcorner 0\nyllcorner 0\ncellsize 1\n";
    std::fs::write(&path, format!("{header}1 2 3 4\n")).unwrap();
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "32")], || {
        let out = terrio::open_raster(path.to_str().unwrap()).unwrap();
        assert_eq!(out.raster.data_type, DataType::F64);
        assert_eq!(out.raster.data.to_f64_vec(), vec![1., 2., 3., 4.]);
    });
    std::fs::write(&path, format!("{header}invalid pixels\n")).unwrap();
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "31")], || {
        assert!(matches!(terrio::open_raster(path.to_str().unwrap()), Err(terrio::Error::ResourceLimit(_))));
    });
    // The fallback grammar accepts '=' and numeric fields longer than the retained prefix.
    let header = format!("ncols=={}2\nnrows=2\nxllcorner 0\nyllcorner 0\ncellsize 1\n", "0".repeat(128 << 10));
    std::fs::write(&path, format!("{header}invalid pixels\n")).unwrap();
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "31")], || {
        assert!(matches!(terrio::open_raster(path.to_str().unwrap()), Err(terrio::Error::ResourceLimit(_))));
    });
    let (source, _) = sample(RasterDriver::PCRaster, dir.path()).unwrap();
    let renamed = dir.path().join("binary.txt");
    std::fs::copy(source, &renamed).unwrap();
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "1048576")], || {
        let out = terrio::open_raster(renamed.to_str().unwrap()).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows, out.raster.data_type), (1024, 1024, DataType::U8));
    });
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "1")], || {
        assert!(matches!(terrio::open_raster(renamed.to_str().unwrap()), Err(terrio::Error::ResourceLimit(_))));
    });
}

/// Reference: GDAL 3.13.3. Unknown suffixes must not force a whole CSF read just to identify it.
#[test]
fn fallback_pcraster_is_checked_before_whole_file_detection() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let (source, _) = sample(RasterDriver::PCRaster, dir.path()).unwrap();
    let reference = terrio::open_raster(&source).unwrap();
    let path = dir.path().join("binary.unknown");
    std::fs::copy(source, &path).unwrap();
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "1048576")], || {
        let actual = terrio::open_raster(path.to_str().unwrap()).unwrap();
        assert_eq!((actual.raster.cols, actual.raster.rows, actual.raster.data_type), (1024, 1024, DataType::U8));
        assert_eq!(actual.raster.data.to_f64_vec(), reference.raster.data.to_f64_vec());
        assert_eq!(actual.geo_transform(), reference.geo_transform());
    });
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "1")], || {
        let (result, largest) = largest_during(|| terrio::open_raster(path.to_str().unwrap()).map(|_| ()));
        assert!(matches!(result, Err(terrio::Error::ResourceLimit(_))) && largest < 1 << 20, "{result:?}, largest={largest}");
    });
}

/// Reference: GDAL 3.13.3. Native validation stays strict; optional GDAL keeps its own syntax rules.
#[test]
fn malformed_ascii_preflight_preserves_codec_and_optional_backend_validation() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let esri = "ncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n";
    let grass = "north: 2\nsouth: 0\neast: 2\nwest: 0\nrows: 2\ncols: 2\ntype: double\n";
    for (name, header) in [("native.asc", esri), ("fallback.txt", esri), ("grass.txt", grass)] {
        let path = dir.path().join(name);
        let valid = format!("{header}1 2\n3 4\n");
        std::fs::write(&path, &valid).unwrap();
        scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "32")], || {
            let ds = terrio::open_raster(path.to_str().unwrap()).unwrap();
            assert_eq!(ds.raster.data.to_f64_vec(), vec![1., 2., 3., 4.]);
        });
        for in_header in [true, false] {
            let mut corrupt = valid.as_bytes().to_vec();
            corrupt[if in_header { 3 } else { header.len() }] = 0xff;
            std::fs::write(&path, corrupt).unwrap();
            for limit in ["", "1", "32"] {
                scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", limit)], || {
                    let (result, largest) = largest_during(|| terrio::open_raster(path.to_str().unwrap()));
                    assert!(largest < 1 << 20, "{name} header={in_header} limit={limit}: largest={largest}");
                    match result {
                        Ok(actual) => {
                            #[cfg(not(feature = "gdal"))]
                            panic!("native codec accepted {name} header={in_header} limit={limit}: {actual:?}");
                            #[cfg(feature = "gdal")]
                            {
                                // GDAL accepts some invalid pixel tokens as zero. This is the
                                // existing optional backend's grammar, not a native preflight bypass.
                                assert_ne!(limit, "1", "GDAL fallback bypassed the declaration budget");
                                let reference = terrio::gdal_backend::open_raster(path.to_str().unwrap()).unwrap();
                                assert_eq!(actual.raster.data_type, reference.raster.data_type);
                                assert_eq!((actual.raster.cols, actual.raster.rows), (reference.raster.cols, reference.raster.rows));
                                assert_eq!(actual.raster.data.to_f64_vec(), reference.raster.data.to_f64_vec());
                                assert_eq!(actual.geo_transform(), reference.geo_transform());
                                assert_eq!(actual.nodata(), reference.nodata());
                            }
                        }
                        Err(error) => {
                            if limit != "1" {
                                assert!(!matches!(error, terrio::Error::ResourceLimit(_)), "{error:?}");
                            } else if !in_header && name != "grass.txt" {
                                // A known Esri header is checked before inspecting invalid pixels.
                                assert!(matches!(error, terrio::Error::ResourceLimit(_)), "{error:?}");
                            }
                        }
                    }
                });
            }
        }
    }
}

/// Reference: GDAL 3.13.3. Native Esri keeps the first dimension; the pinned fallback keeps the last.
#[test]
fn duplicate_ascii_dimensions_follow_each_readers_rule() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    for (first, last) in [(3, 1000), (1000, 3)] {
        for (name, cols, data_type) in [("native.asc", first, DataType::I32), ("fallback.txt", last, DataType::F64)] {
            let path = dir.path().join(name);
            let pixels = (0..cols).map(|i| i.to_string()).collect::<Vec<_>>().join(" ");
            std::fs::write(&path, format!("ncols {first}\nncols {last}\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n{pixels}\n"))
                .unwrap();
            let bytes = cols * data_type.size_bytes();
            scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", &bytes.to_string())], || {
                let ds = terrio::open_raster(path.to_str().unwrap()).unwrap();
                assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.data_type), (cols, 1, data_type));
                assert_eq!(ds.raster.data.to_f64_vec(), (0..cols).map(|i| i as f64).collect::<Vec<_>>());
            });
            scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", &(bytes - 1).to_string())], || {
                assert!(matches!(terrio::open_raster(path.to_str().unwrap()), Err(terrio::Error::ResourceLimit(_))));
            });
        }
    }
}

/// Reference: GDAL 3.13.3. A tiny raster can request an enormous padded encoding tile.
#[test]
#[cfg(target_pointer_width = "64")]
fn padded_tiles_are_refused_before_the_writer_creates_its_file() {
    let _serial = serial();
    let tile = 1u32 << 30;
    let bytes = u64::from(tile) * u64::from(tile);
    if !terrio::usable_memory().is_some_and(|m| m < bytes) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ds = header(16, 1, 1);
    for driver in [RasterDriver::GTiff, RasterDriver::Cog] {
        let path = dir.path().join(format!("{driver:?}.tif"));
        let options = RasterWriteOptions { driver: Some(driver), tile_size: Some(tile), ..Default::default() };
        let (result, largest) = largest_during(|| terrio::GeoTiffWriter::create(path.to_str().unwrap(), &ds, &options).map(|_| ()));
        assert!(matches!(result, Err(terrio::Error::ResourceLimit(ref e)) if e.contains("a GeoTIFF tile")), "{result:?}");
        assert!(largest < 1 << 20, "{largest} bytes requested");
        assert!(!path.exists());
    }
}

/// Reference: GDAL 3.13.3. Lazy source and eager VRT outputs are bounded with no options set.
#[test]
#[cfg(target_pointer_width = "64")]
fn source_backed_windows_and_vrt_materialization_refuse_unusable_buffers() {
    let _serial = serial();
    let side = 1usize << 20;
    let bytes = side as u64 * side as u64 * 16;
    if !terrio::usable_memory().is_some_and(|m| m < bytes) {
        return;
    }
    let xml = format!(
        "<VRTDataset rasterXSize=\"{side}\" rasterYSize=\"{side}\"><VRTRasterBand dataType=\"Float64\" band=\"1\"/><VRTRasterBand dataType=\"Byte\" band=\"2\"/></VRTDataset>"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.vrt");
    std::fs::write(&path, &xml).unwrap();
    let mut source = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
    let (result, largest) = largest_during(|| source.read().map(|_| ()));
    assert!(matches!(result, Err(terrio::Error::ResourceLimit(_))), "{result:?}");
    assert!(largest < 1 << 20);
    let description = terrio::ops::vrt::VrtDataset::from_xml(&xml).unwrap();
    let (result, largest) = largest_during(|| description.materialize(dir.path()).map(|_| ()));
    assert!(matches!(result, Err(terrio::Error::ResourceLimit(_))), "{result:?}");
    assert!(largest < 1 << 20);
    // Mixed-band reads promote every returned sample to Float64 (16 bytes per pixel, not 9).
    let options = terrio::ResampledReadOptions { size: (side, side), ..Default::default() };
    let result = source.read_resampled(&options);
    assert!(matches!(result, Err(terrio::Error::ResourceLimit(ref e)) if e.contains(&bytes.to_string())), "{result:?}");
    let small = source.read_window(terrio::Window::full(2, 2), &Default::default()).unwrap();
    assert_eq!(small.raster.data.len(), 8);
}

/// Reference: GDAL 3.13.3. Coordinate metadata is not read for a refused Zarr declaration.
#[test]
#[cfg(feature = "zarr")]
fn zarr_declarations_precede_coordinate_array_reads() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    for (name, shape, chunks, dtype, dims) in [
        ("image", "[2,1048576]", "[2,1024]", "|u1", "[\"y\",\"x\"]"),
        ("x", "[1048576]", "[1048576]", "<f8", "[\"x\"]"),
        ("y", "[2]", "[2]", "<f8", "[\"y\"]"),
    ] {
        let node = dir.path().join(name);
        std::fs::create_dir(&node).unwrap();
        std::fs::write(node.join(".zarray"), format!(r#"{{"zarr_format":2,"shape":{shape},"chunks":{chunks},"dtype":"{dtype}","compressor":null,"fill_value":0,"order":"C","filters":null}}"#)).unwrap();
        std::fs::write(node.join(".zattrs"), format!(r#"{{"_ARRAY_DIMENSIONS":{dims}}}"#)).unwrap();
    }
    let path = dir.path().join("image");
    let (result, largest) = largest_during(|| {
        scoped(&[("TERRIO_MAX_DECLARED_BYTES", "1024")], || terrio::open_raster_lazy(path.to_str().unwrap()).map(|_| ()))
    });
    assert!(matches!(result, Err(terrio::Error::ResourceLimit(_))), "{result:?}");
    assert!(largest < 1 << 20, "{largest} bytes requested before refusing the raster");
}

/// Reference: GDAL 3.13.3. CFloat16 is materialized as Float32 real and imaginary parts.
#[test]
#[cfg(feature = "gdal")]
fn gdal_half_complex_declarations_count_promoted_components() {
    let _serial = serial();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/translate_complex_half.tif");
    let open = || terrio::gdal_backend::open_raster(path).map(|_| ());
    assert!(scoped(&[("TERRIO_MAX_DECLARED_BYTES", "1024")], open).is_ok());
    let result = scoped(&[("TERRIO_MAX_DECLARED_BYTES", "1023")], open);
    assert!(matches!(result, Err(terrio::Error::ResourceLimit(ref e)) if e.contains("1024 bytes")), "{result:?}");
}

/// Reference: GDAL 3.13.3. Repeated output bands are bounded even for an in-memory source.
#[test]
#[cfg(target_pointer_width = "64")]
fn memory_windows_refuse_repeated_bands_before_allocating_output() {
    let _serial = serial();
    let side = 1024;
    let pixels = side * side;
    let mut ds = header(side, side, 1);
    ds.raster.data = RasterData::new_filled(DataType::U8, pixels, 7.0);
    let mut source = terrio::LazyRaster::from(ds);
    let Some(usable) = terrio::usable_memory().and_then(|m| usize::try_from(m).ok()) else {
        return;
    };
    // A source of one MiB needs only one requested band per MiB of available memory to
    // exceed the budget. Keep this regression's band-index vector itself below eight MiB.
    let count = usable / pixels + 2;
    if count > 1 << 20 {
        return;
    }
    let options = terrio::WindowOptions { bands: vec![1; count], ..Default::default() };
    let (result, largest) = largest_during(|| source.read_window(terrio::Window::full(side, side), &options).map(|_| ()));
    assert!(matches!(result, Err(terrio::Error::ResourceLimit(ref e)) if e.contains("a window")), "{result:?}");
    assert!(largest < pixels * count, "the output buffer was requested before the refusal: {largest}");
    let small = source.read_window(terrio::Window::full(2, 2), &terrio::WindowOptions { bands: vec![1, 1], ..Default::default() }).unwrap();
    assert_eq!(small.raster.data.to_f64_vec(), vec![7.0; 8]);
}

/// Reference: GDAL 3.13.3. A tiny window still needs its tile's decoded buffer.
#[test]
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn tiny_windows_refuse_unusable_decoded_chunks_with_unlimited_limits() {
    const CHILD: &str = "TERRIO_TEST_TIFF_CHUNK_MEMORY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tiny_windows_refuse_unusable_decoded_chunks_with_unlimited_limits"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(child.status.success(), "{}\n{}", String::from_utf8_lossy(&child.stdout), String::from_utf8_lossy(&child.stderr));
        return;
    }
    let _serial = serial();
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: both calls use a valid rlimit pointer and change only this child process's policy.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_RSS, &mut limit), 0);
        limit.rlim_cur = limit.rlim_max.min(128 << 20);
        assert_eq!(libc::setrlimit(libc::RLIMIT_RSS, &limit), 0);
    }
    assert!(terrio::usable_memory().is_some_and(|m| m <= 128 << 20));
    let original = declaring_tiff(16, 16, 1, 8, 1, Some((16, 16)));
    let mut small =
        terrio::LazyRaster::from_tiff_bytes_with_limits(original.clone(), terrio::TiffReadLimits::unlimited()).unwrap().unwrap();
    assert_eq!(small.read_window(terrio::Window::full(1, 1), &Default::default()).unwrap().raster.data.to_f64_vec(), [0.0]);
    // A valid padded edge tile must keep the existing Deflate fallback. LERC and JPEG XL
    // may use Float64 working samples without changing the configured decoded-byte limit.
    let mut dataset = small.read().unwrap();
    dataset.raster.crs = Default::default();
    dataset.raster.metadata.clear();
    dataset.band_metadata.clear();
    dataset.metadata_domains.clear();
    let compressions = ["DEFLATE", "JXL"].into_iter().chain(cfg!(feature = "lerc").then_some("LERC"));
    for compression in compressions {
        let options = terrio::RasterWriteOptions {
            compression: Some(compression.into()),
            tile_size: Some(64),
            creation_options: vec![("GEOREFERENCED".into(), "NO".into())],
            ..Default::default()
        };
        let bytes = terrio::memory::write_raster_bytes(&dataset, "small.tif", &options).unwrap();
        let chunk_bytes = if compression == "DEFLATE" { 256 } else { 4096 };
        let mut source = terrio::LazyRaster::from_tiff_bytes_with_limits(bytes, terrio::TiffReadLimits { chunk_bytes }).unwrap().unwrap();
        let got = source.read_window(terrio::Window::full(1, 1), &Default::default()).unwrap();
        assert_eq!(got.raster.data.to_f64_vec(), [0.0], "{compression}");
    }
    // Keep the tiny payload: the oversized declaration must be refused before decoding it.
    // Compression NONE exercises the standard decoder; Deflate exercises Terrio's codec.
    for compression in [1u32, 8] {
        let mut bytes = original.clone();
        let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
        for i in 0..entries {
            let at = ifd + 2 + i * 12;
            let tag = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
            let value = match tag {
                256 | 322 => Some(32768u32),
                257 | 323 => Some(8192u32),
                259 => Some(compression),
                _ => None,
            };
            if let Some(value) = value {
                bytes[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
            }
        }
        let mut source =
            terrio::LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), terrio::TiffReadLimits::unlimited()).unwrap().unwrap();
        assert_eq!((source.width(), source.height()), (32768, 8192));
        let (result, largest) = largest_during(|| source.read_window(terrio::Window::full(1, 1), &Default::default()).map(|_| ()));
        assert!(
            matches!(result, Err(terrio::Error::ResourceLimit(ref e)) if e.contains("TIFF chunk")),
            "compression {compression}: {result:?}"
        );
        assert!(largest < 1 << 20, "compression {compression}: {largest} bytes requested before refusal");
        // An explicit configured limit keeps its own diagnostic, even below usable memory.
        let limits = terrio::TiffReadLimits { chunk_bytes: 64 << 20 };
        let mut source = terrio::LazyRaster::from_tiff_bytes_with_limits(bytes, limits).unwrap().unwrap();
        let result = source.read_window(terrio::Window::full(1, 1), &Default::default());
        assert!(matches!(result, Err(terrio::Error::DecodedLimitExceeded { limit, .. }) if limit == 64 << 20), "{result:?}");
    }
}

/// Reference: GDAL 3.13.3; fallback-specific unknown-line rules are checked against wbraster.
#[test]
fn fallback_ascii_unknown_lines_cannot_hide_later_dimensions() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unknown.txt");
    for token in ["nan", "inf", "*"] {
        let header = format!("ncols 2\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n{token} {}\nncols 3\n", "ignored".repeat(12000));
        std::fs::write(&path, format!("{header}1 2 3\n")).unwrap();
        scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "24")], || {
            let out = terrio::open_raster(path.to_str().unwrap()).unwrap();
            assert_eq!(out.raster.data.to_f64_vec(), vec![1., 2., 3.]);
        });
        std::fs::write(&path, format!("{header}invalid pixels\n")).unwrap();
        scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "23")], || {
            assert!(matches!(terrio::open_raster(path.to_str().unwrap()), Err(terrio::Error::ResourceLimit(_))), "{token}");
        });
    }
    // GRASS accepts unknown colon-bearing lines, even when the colon is far into the line.
    let header =
        format!("north: 1\nsouth: 0\neast: 3\nwest: 0\nrows: 1\ncols: 2\n{}: ignored\ncols: 3\ntype: double\n", "0".repeat(128 << 10));
    std::fs::write(&path, format!("{header}invalid pixels\n")).unwrap();
    scoped(&[("TERRIO_WBRASTER_FALLBACK", "YES"), ("TERRIO_MAX_DECLARED_BYTES", "23")], || {
        assert!(matches!(terrio::open_raster(path.to_str().unwrap()), Err(terrio::Error::ResourceLimit(_))));
    });
}

/// Reference: GDAL 3.13.3 fixed-level rounding. Terrio fills empty windows with band nodata.
/// Filling narrow output must not first allocate eight bytes for every output sample.
#[test]
fn empty_resampled_windows_allocate_only_their_output_type() {
    let _serial = serial();
    use terrio::ops::overview::OverviewSelection;
    let options = terrio::ResampledReadOptions {
        size: (512, 512),
        bands: vec![2, 1, 2],
        window: Some((99.9999, 0.0, 0.0001, 100.0)),
        overview: OverviewSelection::None,
        ..Default::default()
    };
    for data_type in [
        DataType::U8,
        DataType::I8,
        DataType::U16,
        DataType::I16,
        DataType::U32,
        DataType::I32,
        DataType::U64,
        DataType::I64,
        DataType::F32,
        DataType::F64,
    ] {
        let mut ds = header(100, 100, 2);
        ds.raster.data_type = data_type;
        ds.raster.data = RasterData::new_filled(data_type, 20000, 1.0);
        ds.band_nodata = vec![Some(7.0), Some(9.0)];
        let mut source = terrio::LazyRaster::from(ds);
        let (result, largest) = largest_during(|| source.read_resampled(&options));
        let out = result.unwrap();
        let n = 512 * 512;
        let bytes = 3 * n * data_type.size_bytes();
        assert_eq!(out.raster.data_type, data_type);
        assert_eq!(out.band_nodata, vec![Some(9.0), Some(7.0), Some(9.0)]);
        if COUNTED {
            assert!(largest <= bytes, "{data_type:?}: requested {largest} bytes for {bytes} output bytes");
        }
        let values = out.raster.data.to_f64_vec();
        for (band, expected) in [9.0, 7.0, 9.0].into_iter().enumerate() {
            assert!(values[band * n..(band + 1) * n].iter().all(|&value| value == expected));
        }
    }
}

/// Reference: GDAL 3.13.3. Narrow VRT/translation output does not bound Float64 working arrays.
#[test]
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn vrt_and_source_resampling_check_their_working_arrays() {
    const CHILD: &str = "TERRIO_TEST_VRT_WORKING_MEMORY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "vrt_and_source_resampling_check_their_working_arrays"])
            .env(CHILD, "1")
            .env_remove("TERRIO_MAX_DECLARED_BYTES")
            .env_remove("TERRIO_MAX_BANDS")
            .output()
            .unwrap();
        assert!(child.status.success(), "{}\n{}", String::from_utf8_lossy(&child.stdout), String::from_utf8_lossy(&child.stderr));
        return;
    }
    let _serial = serial();
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: valid pointers; only this child process's usable-memory policy is constrained.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_RSS, &mut limit), 0);
        limit.rlim_cur = limit.rlim_max.min(128 << 20);
        assert_eq!(libc::setrlimit(libc::RLIMIT_RSS, &limit), 0);
    }
    let dir = tempfile::tempdir().unwrap();
    let vrt = dir.path().join("empty.vrt");
    std::fs::write(&vrt, "<VRTDataset rasterXSize=\"3000\" rasterYSize=\"3000\"><VRTRasterBand dataType=\"Byte\" band=\"1\"><NoDataValue>7</NoDataValue></VRTRasterBand></VRTDataset>").unwrap();
    let mut source = terrio::open_raster_lazy(vrt.to_str().unwrap()).unwrap();
    let small = source.read_window(terrio::Window::full(16, 16), &Default::default()).unwrap();
    assert_eq!(small.raster.data.to_f64_vec(), vec![7.; 256]);
    let (result, largest) = largest_during(|| source.read().map(|_| ()));
    let vrt_refused = matches!(result, Err(terrio::Error::ResourceLimit(_))) && largest < 1 << 20;
    let vrt_result = format!("{result:?}, largest={largest}");

    use terrio::ops::translate::{Size, TranslateOptions};
    let mut data = header(1, 1, 1);
    data.raster.data = RasterData::U8(vec![11]);
    let options = TranslateOptions {
        out_size: Some((Size::Pixels(16), Size::Pixels(16))),
        resampling: Some(terrio::ops::Resampling::Average),
        ..Default::default()
    };
    let small = terrio::ops::translate::translate(&data, &options).unwrap();
    assert_eq!(small.raster.data.to_f64_vec(), vec![11.; 256]);
    let options = TranslateOptions { out_size: Some((Size::Pixels(3000), Size::Pixels(3000))), ..options };
    let (result, largest) = largest_during(|| terrio::ops::translate::translate(&data, &options).map(|_| ()));
    let source_refused = matches!(result, Err(terrio::Error::ResourceLimit(_))) && largest < 16 << 20;
    assert!(vrt_refused && source_refused, "VRT {vrt_result}; source {result:?}, largest={largest}");
}

/// Reference: GDAL 3.13.3. UInt16 alpha is converted to bytes before resizing the source window.
#[test]
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn resized_alpha_checks_the_source_and_conversion_together() {
    const CHILD: &str = "TERRIO_TEST_ALPHA_WORKING_MEMORY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "resized_alpha_checks_the_source_and_conversion_together"])
            .env(CHILD, "1")
            .env_remove("TERRIO_MAX_DECLARED_BYTES")
            .env_remove("TERRIO_MAX_BANDS")
            .output()
            .unwrap();
        assert!(child.status.success(), "{}\n{}", String::from_utf8_lossy(&child.stdout), String::from_utf8_lossy(&child.stderr));
        return;
    }
    let _serial = serial();
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: valid pointers; only this child's usable-memory policy is constrained.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_RSS, &mut limit), 0);
        limit.rlim_cur = limit.rlim_max.min(32 << 20);
        assert_eq!(libc::setrlimit(libc::RLIMIT_RSS, &limit), 0);
    }
    let dir = tempfile::tempdir().unwrap();
    let alpha_tiff = |bits, block| {
        let mut bytes = declaring_tiff(4096, 4096, 2, bits, 1, block);
        let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
        let extra = (0..count).map(|i| ifd + 2 + 12 * i).find(|&i| u16::from_le_bytes(bytes[i..i + 2].try_into().unwrap()) == 338).unwrap();
        bytes[extra + 8..extra + 10].copy_from_slice(&2u16.to_le_bytes());
        bytes
    };
    for bits in [8, 16] {
        // Separate planes keep each decoded strip within the policy. It is the subsequent
        // UInt16-to-Byte conversion, not a single decoder buffer, that crosses the limit.
        let path = dir.path().join(format!("alpha-{bits}.tif"));
        std::fs::write(&path, alpha_tiff(bits, None)).unwrap();
        for resampling in [terrio::ops::Resampling::Nearest, terrio::ops::Resampling::Bilinear, terrio::ops::Resampling::Average] {
            let mut source = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
            let small = terrio::ResampledReadOptions {
                bands: vec![1],
                window: Some((0., 0., 16., 16.)),
                size: (8, 8),
                resampling,
                ..Default::default()
            };
            assert_eq!(source.read_resampled_masks(&small).unwrap(), vec![0; 64]);
            let large = terrio::ResampledReadOptions { window: None, size: (512, 512), ..small };
            let (result, largest) = largest_during(|| source.read_resampled_masks(&large));
            if bits == 8 {
                assert_eq!(result.unwrap(), vec![0; 512 * 512]);
            } else {
                let result = result.map(|_| ());
                assert!(
                    matches!(result, Err(terrio::Error::ResourceLimit(_))) && largest < 1 << 20,
                    "{resampling:?}: {result:?}, largest={largest}"
                );
            }
        }
    }
    // With two source windows, the first conversion fits; the second also retains
    // the Byte output allocated while scattering the first. Refuse that later peak.
    let path = dir.path().join("alpha-two-windows.tif");
    std::fs::write(&path, alpha_tiff(16, Some((4096, 2048)))).unwrap();
    let mut source = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
    let options = terrio::ResampledReadOptions { bands: vec![1], size: (4096, 2048), ..Default::default() };
    let (result, largest) = largest_during(|| source.read_resampled_masks(&options).map(|_| ()));
    assert!(matches!(result, Err(terrio::Error::ResourceLimit(_))) && largest >= 16 << 20, "{result:?}, largest={largest}");
}

/// Reference grid: GDAL 3.13.3. Preserve the native Esri grammar for valueless known keys.
#[test]
fn bare_ascii_header_keys_keep_the_preflight_before_encoded_text_reads() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.asc");
    for key in ["ncols", "nrows", "nodata_value"] {
        let header = format!("{key}\nncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n");
        std::fs::write(&path, format!("{header}1 2\n3 4\n")).unwrap();
        scoped(&[("TERRIO_MAX_DECLARED_BYTES", "16")], || {
            let actual = terrio::open_raster(path.to_str().unwrap()).unwrap();
            assert_eq!(actual.raster.data_type, DataType::I32);
            assert_eq!(actual.raster.data.to_f64_vec(), vec![1., 2., 3., 4.]);
        });
        let mut encoded = header.into_bytes();
        encoded.extend(std::iter::repeat_n(b'0', 16 << 20));
        std::fs::write(&path, encoded).unwrap();
        scoped(&[("TERRIO_MAX_DECLARED_BYTES", "1")], || {
            let (result, largest) = largest_during(|| terrio::open_raster(path.to_str().unwrap()));
            assert!(matches!(result, Err(terrio::Error::ResourceLimit(_))) && largest < 1 << 20, "{key}: {result:?}, largest={largest}");
        });
    }
}
