//! Lazy GeoTIFF reads keep the chunk tables of each image they read (the offsets and byte counts of
//! its tiles or strips) and how its chunks are decoded: after the first window read of a level, the
//! next ones read no table and copy none, and mask reads copy no mask's, whatever the number of
//! chunks. Shown with sparse files whose blocks of metadata are dropped after the first read, and
//! with the bytes each thread allocates. Opening, and the `tiff` crate's own reading of an image's
//! directory when reads move to another level, are not covered: they still read the tables.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use terrio::raster::RasterData;
use terrio::sparse::SparseFile;
use terrio::{DataType, LazyRaster, RasterConfig, RasterDataset, RasterWriteOptions, TiffReadLimits, Window, WindowOptions};

/// Counts the bytes each thread allocates, and those it holds (allocated less freed).
struct Counting;

thread_local! {
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
    static HELD: Cell<isize> = const { Cell::new(0) };
}

fn count(allocated: usize, freed: usize) {
    let _ = ALLOCATED.try_with(|a| a.set(a.get() + allocated));
    let _ = HELD.try_with(|h| h.set(h.get() + allocated as isize - freed as isize));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size(), 0);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size(), 0);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count(0, layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size, layout.size());
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// What `f` returns, the bytes it allocated on this thread, and those it still holds.
fn allocations<T>(f: impl FnOnce() -> T) -> (T, usize, isize) {
    let (allocated, held) = (ALLOCATED.with(Cell::get), HELD.with(Cell::get));
    let out = f();
    (out, ALLOCATED.with(Cell::get) - allocated, HELD.with(Cell::get) - held)
}

/// A Byte raster of `bands` bands, values below 251, nodata 255.
fn raster(cols: usize, rows: usize, bands: usize) -> RasterDataset {
    let cfg = RasterConfig { cols, rows, bands, data_type: DataType::U8, nodata: 255.0, ..Default::default() };
    let values = (0..bands * cols * rows).map(|i| (i * 7 % 251) as u8).collect();
    RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::U8(values)).unwrap(), true)
}

/// A validity mask with invalid squares.
fn mask(cols: usize, rows: usize) -> Vec<u8> {
    (0..cols * rows).map(|i| if ((i % cols) / 5 + (i / cols) / 3).is_multiple_of(4) { 0 } else { 255 }).collect()
}

fn options(compression: &str, creation: &[(&str, &str)]) -> RasterWriteOptions {
    RasterWriteOptions {
        compression: Some(compression.into()),
        creation_options: creation.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    }
}

fn read(r: &mut LazyRaster, w: Window, bands: &[usize], overview: Option<usize>) -> Vec<f64> {
    r.read_window(w, &WindowOptions { bands: bands.to_vec(), overview }).unwrap().raster.data.to_f64_vec()
}

/// The chunk tables of an image (IFD) of a little-endian TIFF or BigTIFF: where their values are
/// in the file, with the size of one, and the values.
struct Tables {
    offsets_at: (usize, usize),
    counts_at: (usize, usize),
    offsets: Vec<u64>,
    counts: Vec<u64>,
}

/// The chunk tables of every image of a TIFF, masks included, in file order.
fn chunk_tables(bytes: &[u8]) -> Vec<Tables> {
    let u16_at = |o: usize| u16::from_le_bytes(bytes[o..o + 2].try_into().unwrap());
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
    assert_eq!(&bytes[..2], b"II");
    let big = u16_at(2) == 43;
    let (mut ifd, count_len, entry_len, inline) = if big { (u64_at(8) as usize, 8, 20, 8) } else { (u32_at(4) as usize, 2, 12, 4) };
    let mut out = Vec::new();
    while ifd != 0 {
        let n = if big { u64_at(ifd) as usize } else { u16_at(ifd) as usize };
        // (tag, where its values are, value size, count)
        let mut entries = Vec::new();
        for e in 0..n {
            let at = ifd + count_len + e * entry_len;
            let (tag, typ) = (u16_at(at), u16_at(at + 2));
            let (count, value) = if big { (u64_at(at + 4) as usize, at + 12) } else { (u32_at(at + 4) as usize, at + 8) };
            let size = match typ {
                3 => 2,
                4 => 4,
                16 => 8,
                _ => continue,
            };
            let values_at = match (count * size <= inline, big) {
                (true, _) => value,
                (false, true) => u64_at(value) as usize,
                (false, false) => u32_at(value) as usize,
            };
            entries.push((tag, values_at, size, count));
        }
        let find = |tags: [u16; 2]| *entries.iter().find(|e| tags.contains(&e.0)).unwrap();
        let ((_, offsets_at, offset_size, n_chunks), (_, counts_at, count_size, _)) = (find([324, 273]), find([325, 279]));
        let values = |at: usize, size: usize| -> Vec<u64> {
            (0..n_chunks)
                .map(|i| match size {
                    2 => u16_at(at + i * 2) as u64,
                    4 => u32_at(at + i * 4) as u64,
                    _ => u64_at(at + i * 8),
                })
                .collect()
        };
        out.push(Tables {
            offsets_at: (offsets_at, offset_size),
            counts_at: (counts_at, count_size),
            offsets: values(offsets_at, offset_size),
            counts: values(counts_at, count_size),
        });
        let next = ifd + count_len + n * entry_len;
        ifd = if big { u64_at(next) as usize } else { u32_at(next) as usize };
    }
    out
}

/// Makes chunk `i` of an image sparse: offset and byte count 0.
fn clear_chunk(bytes: &mut [u8], t: &Tables, i: usize) {
    for (at, size) in [t.offsets_at, t.counts_at] {
        bytes[at + i * size..][..size].fill(0);
    }
}

/// The block size of the sparse files of these tests: small, so that few blocks hold both chunk
/// data and metadata.
const BLOCK: usize = 16;

/// A sparse file holding every block of `bytes`, those holding chunk data (of every image, masks
/// included) inserted last; and the number of these.
fn sparse_file(bytes: &[u8]) -> (SparseFile, usize) {
    let file = SparseFile::with_block_size(bytes.len() as u64, BLOCK as u64);
    let blocks = bytes.len().div_ceil(BLOCK);
    let mut data = vec![false; blocks];
    for t in chunk_tables(bytes) {
        for (&offset, &n) in t.offsets.iter().zip(&t.counts).filter(|(_, n)| **n > 0) {
            let (first, end) = (offset as usize / BLOCK, ((offset + n) as usize).div_ceil(BLOCK).min(blocks));
            data[first..end].iter_mut().for_each(|d| *d = true);
        }
    }
    let insert = |b: usize| file.insert((b * BLOCK) as u64, &bytes[b * BLOCK..((b + 1) * BLOCK).min(bytes.len())]);
    (0..blocks).filter(|b| !data[*b]).for_each(insert);
    (0..blocks).filter(|b| data[*b]).for_each(insert);
    (file, data.iter().filter(|d| **d).count())
}

/// Window reads of a level after the level's first read, with the file's blocks of metadata gone
/// (its directories, the chunk tables, other tags): they read chunk data only. Before, each read
/// read both tables again, and LZW's codec read them once more; Deflate's lack of a terrio codec
/// was found again from the tags, the three values of BitsPerSample among them.
#[test]
fn window_reads_after_the_first_of_a_level_read_only_chunk_data() {
    let (cols, rows) = (83, 61);
    let ds = raster(cols, rows, 3);
    let RasterData::U8(src) = &ds.raster.data else { unreachable!() };
    let validity = mask(cols, rows);
    let tiles = [("TILED", "YES"), ("BLOCKXSIZE", "16"), ("BLOCKYSIZE", "16")];
    // (compression, creation options, BigTIFF): edge tiles 3 x 13 pixels, edge strips 5 rows.
    let cases = [
        ("DEFLATE", [&tiles[..], &[("INTERLEAVE", "PIXEL")]].concat(), false),
        ("LZW", [&tiles[..], &[("INTERLEAVE", "PIXEL")]].concat(), false),
        ("LZW", vec![("BLOCKYSIZE", "7"), ("INTERLEAVE", "BAND")], false),
        ("DEFLATE", [&tiles[..], &[("INTERLEAVE", "BAND")]].concat(), true),
    ];
    for (compression, creation, bigtiff) in cases {
        let tiled = creation.contains(&("TILED", "YES"));
        let planar = creation.contains(&("INTERLEAVE", "BAND"));
        let o = RasterWriteOptions { bigtiff, mask: Some(validity.clone()), overviews: vec![2], ..options(compression, &creation) };
        let mut bytes = terrio::memory::write_raster_bytes(&ds, "t.tif", &o).unwrap();
        // Chunk 1 of the full-resolution image (of band 1 when planar) is sparse: read as nodata.
        let full = chunk_tables(&bytes).swap_remove(0);
        clear_chunk(&mut bytes, &full, 1);
        let sparse = |b: usize, r: usize, c: usize| {
            (!planar || b == 1) && if tiled { r < 16 && (16..32).contains(&c) } else { (7..14).contains(&r) }
        };
        let expected = |w: Window, bands: &[usize]| -> Vec<f64> {
            let all = [1, 2, 3];
            let mut out = Vec::new();
            for &b in if bands.is_empty() { &all[..] } else { bands } {
                for r in w.y..w.y + w.height {
                    for c in w.x..w.x + w.width {
                        out.push(if sparse(b, r, c) { 255.0 } else { f64::from(src[(b - 1) * cols * rows + r * cols + c]) });
                    }
                }
            }
            out
        };
        let label = format!("{compression} {creation:?} BigTIFF {bigtiff}");
        // Overview reads are compared with a reader whose first read they are.
        let fresh = || LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap();
        for overview in [None, Some(0)] {
            let (file, data_blocks) = sparse_file(&bytes);
            let mut r = LazyRaster::from_sparse(file.clone()).unwrap().unwrap();
            let first = Window::new(0, 0, 1, 1);
            assert_eq!(read(&mut r, first, &[], overview), read(&mut fresh(), first, &[], overview), "{label}");
            file.trim((data_blocks * BLOCK) as u64);
            let (lw, lh) = overview.map_or((cols, rows), |i| r.overviews()[i]);
            for (w, bands) in [
                // Several chunks, the sparse one among them; bands reordered and repeated.
                (Window::new(5, 3, 30, 20), vec![3, 1, 3]),
                // The edge chunks, then (at full resolution) the sparse chunk alone.
                (Window::new(lw - 8, lh - 11, 8, 11), vec![2]),
                (Window::new(17, 2, 3, 3), vec![1]),
                (Window::new(0, 0, lw, lh), vec![]),
            ] {
                let got = read(&mut r, w, &bands, overview);
                assert!(!file.has_missing(), "{label}, overview {overview:?}, {w:?}: read past the chunks");
                let want = match overview {
                    None => expected(w, &bands),
                    Some(_) => read(&mut fresh(), w, &bands, overview),
                };
                assert_eq!(got, want, "{label}, overview {overview:?}, {w:?}");
            }
            // Masks are read from their descriptors, borrowed: the full-resolution one, also for
            // the overview, which has none of its own.
            let w = Window::new(4, 6, 30, 20);
            let m = r.read_mask(w, overview).unwrap().unwrap();
            assert!(!file.has_missing(), "{label}: a mask read read past the chunks");
            let want = match overview {
                None => (6..26).flat_map(|r| (4..34).map(move |c| (r, c))).map(|(r, c)| validity[r * cols + c]).collect::<Vec<_>>(),
                Some(_) => fresh().read_mask(w, overview).unwrap().unwrap(),
            };
            assert_eq!(m, want, "{label}, overview {overview:?}");
        }
    }
}

/// Reads of one-row strips and of their mask, with tables of 65,536 values: once a level is read,
/// a window read allocates far less than one copy of the tables, and so does a mask read, at full
/// resolution or sampled for an overview. Before, a window read allocated the tables twice (four
/// times with LZW) as the `tiff` crate decodes them, and a mask read copied the mask's tables.
#[test]
fn window_and_mask_reads_copy_no_chunk_table() {
    let (cols, rows) = (8, 1 << 16);
    let ds = raster(cols, rows, 1);
    let RasterData::U8(src) = &ds.raster.data else { unreachable!() };
    let validity = mask(cols, rows);
    // The image's offsets and byte counts as 64-bit values (1 MiB), and as many for the mask's.
    let tables = 2 * rows * 8;
    for compression in ["DEFLATE", "LZW"] {
        let o = RasterWriteOptions { mask: Some(validity.clone()), overviews: vec![2], ..options(compression, &[("BLOCKYSIZE", "1")]) };
        let bytes = terrio::memory::write_raster_bytes(&ds, "t.tif", &o).unwrap();
        assert_eq!(chunk_tables(&bytes)[0].counts.len(), rows);
        let file = SparseFile::new(bytes.len() as u64);
        file.insert(0, &bytes);
        // Sparse files are decoded on the calling thread, whose allocations are counted.
        let mut r = LazyRaster::from_sparse(file).unwrap().unwrap();
        let strip = |y: usize| Window::new(2, y, 4, 1);
        let (_, _, held) = allocations(|| read(&mut r, strip(0), &[1], None));
        for y in [1, 40_000, rows - 1] {
            let (got, allocated, _) = allocations(|| read(&mut r, strip(y), &[1], None));
            assert!(allocated < tables / 4, "{compression}: a strip read allocated {allocated} bytes");
            let want: Vec<f64> = (2..6).map(|c| f64::from(src[y * cols + c])).collect();
            assert_eq!(got, want, "{compression}, row {y}");
        }
        // The overview (4 x 32768) has no mask of its own: its pixels sample the full-resolution
        // mask's at (2c + 1, 2r + 1).
        for (overview, w) in [(None, strip(50_000)), (Some(0), Window::new(0, 20_000, 4, 1))] {
            let (m, allocated, _) = allocations(|| r.read_mask(w, overview).unwrap().unwrap());
            assert!(allocated < tables / 4, "{compression}: a mask read (overview {overview:?}) allocated {allocated} bytes");
            let want: Vec<u8> = match overview {
                None => (2..6).map(|c| validity[w.y * cols + c]).collect(),
                Some(_) => (0..4).map(|c| validity[(2 * w.y + 1) * cols + 2 * c + 1]).collect(),
            };
            assert_eq!(m, want, "{compression}: mask, overview {overview:?}");
        }
        // The level's first read kept one copy of its tables, shared by its codec (LZW).
        assert!(held >= tables as isize && held < (tables + tables / 2) as isize, "{compression}: the first read kept {held} bytes");
    }
}

/// Reads moving between levels: each level keeps its own tables, so returning to a level reads
/// none of them again through terrio (the `tiff` crate's move to the level's directory still reads
/// them). Values are compared with readers whose first read each is.
#[test]
fn returning_to_a_level_reads_none_of_its_tables_again() {
    let (cols, rows) = (16, 1 << 13);
    let ds = raster(cols, rows, 1);
    let o = RasterWriteOptions { overviews: vec![2, 4], ..options("LZW", &[("BLOCKYSIZE", "1")]) };
    let bytes = terrio::memory::write_raster_bytes(&ds, "t.tif", &o).unwrap();
    let tables = chunk_tables(&bytes);
    // The overviews' chunk counts: the images after the full-resolution one.
    let overview_chunks = tables[1].counts.len();
    assert!(overview_chunks >= rows / 2, "{overview_chunks} chunks");
    let file = SparseFile::new(bytes.len() as u64);
    file.insert(0, &bytes);
    let mut r = LazyRaster::from_sparse(file).unwrap().unwrap();
    let check = |r: &mut LazyRaster, overview: Option<usize>, y: usize| {
        let w = Window::new(1, y, 3, 2);
        let fresh = read(&mut LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap(), w, &[1], overview);
        let (got, allocated, _) = allocations(|| read(r, w, &[1], overview));
        assert_eq!(got, fresh, "overview {overview:?}, row {y}");
        allocated
    };
    check(&mut r, None, 100);
    // Overview 0 read first, then again after reading another level: both move the decoder to
    // its image, the first also reads its tables.
    let first = check(&mut r, Some(0), 7);
    check(&mut r, None, 4000);
    check(&mut r, Some(1), 9);
    let again = check(&mut r, Some(0), 1000);
    assert!(first >= again + overview_chunks * 2 * 8, "{first} bytes allocated by the first read, {again} by the next");
    // Each level kept its own tables: every level reads the same as a fresh reader, in any order.
    for (overview, y) in [(Some(1), 2000), (None, 8000), (Some(0), 4090), (Some(1), 0), (None, 0)] {
        check(&mut r, overview, y);
    }
}

/// External overviews (a `.ovr` with overviews and a mask of its own) read in turn with the
/// full-resolution image, by one reader: the `.ovr`'s levels keep their own tables.
#[test]
fn external_overview_levels_keep_their_own_tables() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("img.tif");
    let (cols, rows) = (16, 4096);
    let o = options("LZW", &[("BLOCKYSIZE", "1")]);
    terrio::write_raster_dataset(&raster(cols, rows, 1), base.to_str().unwrap(), &o).unwrap();
    // The `.ovr`: other values, its own overview and mask.
    let mut ovr = raster(cols / 2, rows / 2, 1);
    let RasterData::U8(v) = &mut ovr.raster.data else { unreachable!() };
    v.iter_mut().for_each(|x| *x = 250 - *x);
    let ovr_mask = mask(cols / 2, rows / 2);
    let path = dir.path().join("ovr.tif");
    let ovr_options = RasterWriteOptions { overviews: vec![2], mask: Some(ovr_mask.clone()), ..o.clone() };
    terrio::write_raster_dataset(&ovr, path.to_str().unwrap(), &ovr_options).unwrap();
    std::fs::rename(&path, dir.path().join("img.tif.ovr")).unwrap();
    let ovr_chunks = chunk_tables(&std::fs::read(dir.path().join("img.tif.ovr")).unwrap())[0].counts.len();

    let base = base.to_str().unwrap();
    let mut r = terrio::open_raster_lazy(base).unwrap();
    assert_eq!(r.overviews(), [(cols / 2, rows / 2), (cols / 4, rows / 4)]);
    let check = |r: &mut LazyRaster, overview: Option<usize>, y: usize| {
        let w = Window::new(1, y, 3, 2);
        let fresh = read(&mut terrio::open_raster_lazy(base).unwrap(), w, &[1], overview);
        let (got, allocated, _) = allocations(|| read(r, w, &[1], overview));
        assert_eq!(got, fresh, "overview {overview:?}, row {y}");
        let mask = terrio::open_raster_lazy(base).unwrap().read_mask(w, overview).unwrap();
        assert_eq!(r.read_mask(w, overview).unwrap(), mask, "mask, overview {overview:?}, row {y}");
        allocated
    };
    check(&mut r, None, 5);
    check(&mut r, Some(1), 3);
    // The `.ovr`'s first image, read first and then again after its second one: both reads move
    // the `.ovr`'s decoder to the image, the first also reads its tables.
    let first = check(&mut r, Some(0), 11);
    check(&mut r, Some(1), 500);
    check(&mut r, None, 4000);
    let again = check(&mut r, Some(0), 2000);
    assert!(first >= again + ovr_chunks * 2 * 8, "{first} bytes allocated by the first read, {again} by the next");
    for (overview, y) in [(Some(1), 1000), (Some(0), 0), (None, 0), (Some(1), 0)] {
        check(&mut r, overview, y);
    }
    // The `.ovr`'s mask is its own, for both levels of the `.ovr`; the base has none.
    assert_eq!(r.read_mask(Window::new(0, 1, 8, 1), Some(0)).unwrap().unwrap(), ovr_mask[8..16]);
    assert!(r.read_mask(Window::new(0, 0, 2, 2), None).unwrap().is_none());
}

/// Tables over the decoding limits are an error on every read, as before: nothing is kept from a
/// read that failed, and a limit they fit in reads them.
#[test]
fn tables_over_the_limits_fail_every_read() {
    // 64 strips: the `tiff` crate decodes each table of 64 values into 64 enum values of at least
    // 16 bytes, over a limit of 512 bytes, while each strip decodes to 16 bytes.
    let (cols, rows) = (16, 64);
    let ds = raster(cols, rows, 1);
    let bytes = terrio::memory::write_raster_bytes(&ds, "t.tif", &options("DEFLATE", &[("BLOCKYSIZE", "1")])).unwrap();
    let w = Window::new(0, 10, 4, 3);
    let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), TiffReadLimits { chunk_bytes: 512 }).unwrap().unwrap();
    for _ in 0..2 {
        let e = r.read_window(w, &WindowOptions { bands: vec![1], overview: None }).unwrap_err();
        assert!(matches!(e, terrio::Error::DecodedLimitExceeded { limit: 512, .. }), "{e}");
    }
    let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes, TiffReadLimits { chunk_bytes: 64 * 64 }).unwrap().unwrap();
    for _ in 0..2 {
        assert_eq!(read(&mut r, w, &[1], None).len(), 12);
    }
}

/// A cached native codec and the cached absence of one belong to their IFD, even when the
/// decoder moves between images with different compression and strip geometry.
#[test]
fn levels_with_different_codecs_keep_their_own_metadata() {
    use tiff::encoder::{Compression, TiffEncoder, colortype::Gray8};
    let mut bytes = std::io::Cursor::new(Vec::new());
    let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
    for (level, compression) in
        [Compression::Deflate(Default::default()), Compression::Lzw, Compression::Uncompressed].into_iter().enumerate()
    {
        let (width, height) = (32 >> level, 24 >> level);
        encoder = encoder.with_compression(compression);
        let mut image = encoder.new_image::<Gray8>(width, height).unwrap();
        image.encoder().write_tag(tiff::tags::Tag::NewSubfileType, u32::from(level > 0)).unwrap();
        image.rows_per_strip(3 - level as u32).unwrap();
        let values: Vec<u8> = (0..width * height).map(|i| (i % 67 + 70 * level as u32) as u8).collect();
        image.write_data(&values).unwrap();
    }
    let mut r = LazyRaster::from_tiff_bytes(bytes.into_inner()).unwrap().unwrap();
    for level in [0, 1, 2, 1, 0, 2, 0, 1] {
        let overview = (level > 0).then(|| level - 1);
        let w = Window::new(1, 2, 5, 3);
        let got = read(&mut r, w, &[1], overview);
        let width = 32 >> level;
        let want: Vec<f64> =
            (w.y..w.y + w.height).flat_map(|y| (w.x..w.x + w.width).map(move |x| ((y * width + x) % 67 + 70 * level) as f64)).collect();
        assert_eq!(got, want, "level {level}");
    }
}

/// Shared lazy handles serialize decoder movement and cache initialization; chunk workers
/// borrow the immutable codec while reads on other handles wait for that decoder.
#[test]
fn concurrent_shared_readers_keep_cached_levels_consistent() {
    for compression in ["DEFLATE", "LZW"] {
        let (cols, rows) = (128, 96);
        let mut ds = raster(cols, rows, 3);
        ds.set_geo_transform([100.0, 2.0, 0.0, 400.0, 0.0, -2.0]);
        let o = RasterWriteOptions {
            overviews: vec![2, 4],
            mask: Some(mask(cols, rows)),
            ..options(compression, &[("TILED", "YES"), ("BLOCKXSIZE", "16"), ("BLOCKYSIZE", "16")])
        };
        let bytes = terrio::memory::write_raster_bytes(&ds, "t.tif", &o).unwrap();
        let mut r = LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap();
        // The full-resolution cache is already populated; the overview caches start empty.
        read(&mut r, Window::new(0, 0, 1, 1), &[1], None);
        let handles: Vec<_> = (0..4).map(|_| r.shared()).collect();
        std::thread::scope(|scope| {
            for (worker, mut handle) in handles.into_iter().enumerate() {
                let bytes = &bytes;
                scope.spawn(move || {
                    let mut reference = LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap();
                    for k in 0..12 {
                        let level = (worker + k) % 3;
                        let overview = (level > 0).then(|| level - 1);
                        let w = Window::new(3 + k % 4, 2 + worker, 23, 17);
                        let opts = WindowOptions { bands: vec![3, 1, 3], overview };
                        let got = handle.read_window(w, &opts).unwrap();
                        let want = reference.read_window(w, &opts).unwrap();
                        assert_eq!(got.raster.data.to_f64_vec(), want.raster.data.to_f64_vec());
                        assert_eq!(got.geo_transform(), want.geo_transform());
                        assert_eq!(got.nodata(), Some(255.0));
                        assert_eq!(got.raster.data_type, DataType::U8);
                        assert_eq!(handle.read_mask(w, overview).unwrap(), reference.read_mask(w, overview).unwrap());
                    }
                });
            }
        });
    }
}
