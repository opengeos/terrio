use super::*;
use std::fs;

/// Compressible but not trivial bytes: lines of numbers.
fn data(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    let mut i = 0u64;
    while out.len() < n {
        out.extend_from_slice(format!("{i},{},{}\n", i * 7919 % 10007, i * i % 9973).as_bytes());
        i += 1;
    }
    out.truncate(n);
    out
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

fn yes(chunk: u32) -> SozipOptions {
    SozipOptions { enable: EnableSozip::Yes, chunk_size: chunk, junk_paths: true, ..Default::default() }
}

#[test]
fn creates_lists_and_validates() {
    let tmp = tempfile::tempdir().unwrap();
    let big = tmp.path().join("big.csv");
    let small = tmp.path().join("small.txt");
    // A size that is an exact multiple of the chunk size, and one that is not.
    let exact = tmp.path().join("exact.bin");
    fs::write(&big, data(100_003)).unwrap();
    fs::write(&small, b"tiny").unwrap();
    fs::write(&exact, data(4096 * 5)).unwrap();
    let zip = tmp.path().join("out.zip");
    let r = create(&[&s(&big), &s(&small), &s(&exact)], &s(&zip), &yes(4096)).unwrap();
    assert_eq!((r.added, r.seek_optimized), (3, 2));
    let members = list(&s(&zip)).unwrap();
    let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["big.csv", "small.txt", "exact.bin"]);
    assert_eq!(members[0].sozip_chunk_size, Some(4096));
    assert_eq!(members[1].sozip_chunk_size, None);
    assert_eq!(members[2].sozip_chunk_size, Some(4096));
    assert!(members[0].list_line().contains("   yes (     4096 bytes)   "), "{}", members[0].list_line());
    let v = validate(&s(&zip)).unwrap();
    assert!(v.valid, "{v:?}");
    assert_eq!(v.sozip_files, ["big.csv", "exact.bin"]);

    // Ordinary readers read it too, and the hidden index is not a member.
    let mut z = zip::ZipArchive::new(fs::File::open(&zip).unwrap()).unwrap();
    assert_eq!(z.len(), 3);
    let mut back = Vec::new();
    z.by_name("big.csv").unwrap().read_to_end(&mut back).unwrap();
    assert_eq!(back, data(100_003));
    assert_eq!(crate::vsi::read(&format!("/vsizip/{}/exact.bin", s(&zip))).unwrap(), data(4096 * 5));

    // Random access through the index.
    let mut m = MemberReader::open(Box::new(fs::File::open(&zip).unwrap()), "big.csv").unwrap().expect("seekable");
    let all = data(100_003);
    for at in [0u64, 4095, 4096, 50_000, 99_990] {
        m.seek(SeekFrom::Start(at)).unwrap();
        let mut buf = vec![0u8; 20.min(100_003 - at as usize)];
        m.read_exact(&mut buf).unwrap();
        assert_eq!(buf, all[at as usize..at as usize + buf.len()]);
    }
    let mut rest = Vec::new();
    m.seek(SeekFrom::Start(8000)).unwrap();
    m.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, all[8000..]);
    // A Deflate member without index is not seekable in place.
    assert!(MemberReader::open(Box::new(fs::File::open(&zip).unwrap()), "small.txt").unwrap().is_none());
}

#[test]
fn auto_mode_appending_and_properties() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.csv");
    fs::write(&a, data(70_000)).unwrap();
    let zip = tmp.path().join("auto.zip");
    // Below the minimum file size: Deflate without an index.
    let o = SozipOptions { junk_paths: true, content_type: Some("text/csv".into()), ..Default::default() };
    create(&[&s(&a)], &s(&zip), &o).unwrap();
    let m = list(&s(&zip)).unwrap();
    assert_eq!(m[0].sozip_chunk_size, None);
    assert_eq!(m[0].properties, ["Content-Type=text/csv"]);
    // Added to: the first member is kept; a second copy of a name is refused.
    let b = tmp.path().join("b.csv");
    fs::write(&b, data(2_000_000)).unwrap();
    create(&[&s(&b)], &s(&zip), &SozipOptions { junk_paths: true, ..Default::default() }).unwrap();
    let m = list(&s(&zip)).unwrap();
    assert_eq!(m.iter().map(|m| (m.name.as_str(), m.sozip_chunk_size)).collect::<Vec<_>>(), [("a.csv", None), ("b.csv", Some(32768))]);
    assert_eq!(m[0].properties, ["Content-Type=text/csv"]);
    assert!(validate(&s(&zip)).unwrap().valid);
    let before = fs::read(&zip).unwrap();
    assert!(create(&[&s(&a)], &s(&zip), &SozipOptions { junk_paths: true, ..Default::default() }).is_err());
    assert_eq!(fs::read(&zip).unwrap(), before, "a failed addition leaves the archive as it was");
    // Overwrite replaces it; paths are kept without -j, recursively.
    let dir = tmp.path().join("d");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("sub/x.txt"), "x").unwrap();
    let o = SozipOptions { overwrite: true, recursive: true, ..Default::default() };
    create(&[&s(&dir)], &s(&zip), &o).unwrap();
    let m = list(&s(&zip)).unwrap();
    assert_eq!(m.len(), 1);
    assert!(m[0].name.ends_with("d/sub/x.txt") && !m[0].name.starts_with('/'), "{}", m[0].name);
    assert!(create(&[&s(&dir)], &s(&zip), &SozipOptions { overwrite: true, ..Default::default() }).is_err());
}

#[test]
fn optimizes_and_detects_corruption() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain.zip");
    {
        let mut z = zip::ZipWriter::new(fs::File::create(&plain).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        z.add_directory("dir/", o).unwrap();
        z.start_file("dir/big.csv", o).unwrap();
        z.write_all(&data(300_000)).unwrap();
        z.start_file("small.txt", o).unwrap();
        z.write_all(b"small").unwrap();
        z.finish().unwrap();
    }
    assert!(validate(&s(&plain)).unwrap().sozip_files.is_empty());
    let out = tmp.path().join("opt.zip");
    let r = optimize(&s(&plain), &s(&out), &SozipOptions { junk_paths: false, ..yes(8192) }).unwrap();
    assert_eq!((r.added, r.seek_optimized), (2, 1));
    let v = validate(&s(&out)).unwrap();
    assert!(v.valid && v.sozip_files == ["dir/big.csv"], "{v:?}");
    assert_eq!(crate::vsi::read(&format!("/vsizip/{}/dir/big.csv", s(&out))).unwrap(), data(300_000));
    assert!(
        optimize(&s(&plain), &s(&out), &SozipOptions { junk_paths: false, ..yes(8192) }).is_err(),
        "an existing output needs overwrite"
    );
    assert!(optimize(&s(&out), &s(&out), &SozipOptions { overwrite: true, ..yes(8192) }).is_err());

    // A corrupted offset in the index is reported.
    let mut bytes = fs::read(&out).unwrap();
    let idx = bytes.windows("dir/.big.csv.sozip.idx".len()).position(|w| w == b"dir/.big.csv.sozip.idx").unwrap();
    let header = idx + "dir/.big.csv.sozip.idx".len();
    // The first offset, after the 32-byte header: point it past the data.
    bytes[header + 32..header + 40].copy_from_slice(&u64::MAX.to_le_bytes());
    let bad = tmp.path().join("bad.zip");
    fs::write(&bad, bytes).unwrap();
    let v = validate(&s(&bad)).unwrap();
    assert!(!v.valid && v.errors[0].contains("offset"), "{v:?}");
    assert!(list(&s(&bad)).unwrap()[1].sozip_chunk_size.is_none());
}

#[test]
fn reads_geotiffs_in_seek_optimized_archives_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let tif = tmp.path().join("dem.tif");
    let mut raster =
        crate::Raster::new(crate::RasterConfig { cols: 300, rows: 200, bands: 1, data_type: crate::DataType::F32, ..Default::default() });
    for r in 0..200 {
        for c in 0..300 {
            raster.set(0, r, c, (r * 1000 + c) as f64).unwrap();
        }
    }
    crate::write_raster(&raster, &s(&tif), &crate::RasterWriteOptions::default()).unwrap();
    let zip = tmp.path().join("dem.zip");
    create(&[&s(&tif)], &s(&zip), &yes(4096)).unwrap();
    assert_eq!(list(&s(&zip)).unwrap()[0].sozip_chunk_size, Some(4096));
    let mut lazy = crate::open_raster_lazy(&format!("/vsizip/{}/dem.tif", s(&zip))).unwrap();
    let w = lazy.read_window(crate::Window { x: 250, y: 150, width: 10, height: 5 }, &Default::default()).unwrap();
    assert_eq!(w.raster.get(0, 0, 0), 150_250.0);
    assert_eq!(w.raster.get(0, 4, 9), 154_259.0);
    assert!(lazy.input_paths().unwrap().contains(&s(&zip)), "archive alias was omitted from input paths");
    let output = tmp.path().join("streamed.tif");
    lazy.write(&s(&output), &Default::default()).unwrap();
    let mut written = crate::open_raster_lazy(&s(&output)).unwrap();
    let window = written.read_window(crate::Window { x: 250, y: 150, width: 10, height: 5 }, &Default::default()).unwrap();
    assert_eq!(window.raster.get(0, 4, 9), 154_259.0);
    let options = crate::RasterWriteOptions { driver: Some(crate::formats::RasterDriver::GTiff), ..Default::default() };
    let before = fs::read(&zip).unwrap();
    assert!(lazy.write(&s(&zip), &options).is_err(), "container output paths are refused");
    assert_eq!(fs::read(&zip).unwrap(), before);
    #[cfg(unix)]
    {
        let alias = tmp.path().join("archive-alias.tif");
        std::os::unix::fs::symlink(&zip, &alias).unwrap();
        lazy.write(&s(&alias), &options).unwrap();
        let mut replaced = crate::open_raster_lazy(&s(&alias)).unwrap();
        let window = replaced.read_window(crate::Window { x: 250, y: 150, width: 10, height: 5 }, &Default::default()).unwrap();
        assert_eq!(window.raster.get(0, 4, 9), 154_259.0);
    }
}

#[test]
fn rejects_zero_chunk_size_before_replacing_output() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("input.txt");
    let archive = tmp.path().join("in.zip");
    let output = tmp.path().join("out.zip");
    fs::write(&source, data(2000)).unwrap();
    create(&[&s(&source)], &s(&archive), &yes(100)).unwrap();
    fs::write(&output, b"preserve existing output").unwrap();
    let options = SozipOptions { overwrite: true, ..yes(0) };
    for result in [create(&[&s(&source)], &s(&output), &options), optimize(&s(&archive), &s(&output), &options)] {
        assert!(matches!(result, Err(Error::InvalidArgument(_))));
        assert_eq!(fs::read(&output).unwrap(), b"preserve existing output");
    }
}

#[test]
fn rejects_oversized_and_trailing_index_tables() {
    let member = CdEntry {
        name: "x".into(),
        flags: 0,
        method: 8,
        dos_time: 0,
        dos_date: 0,
        crc32: 0,
        compressed_size: 10,
        uncompressed_size: u64::MAX,
        local_offset: 0,
        extra: Vec::new(),
        raw: Vec::new(),
    };
    let mut header = Index { chunk_size: 1, uncompressed_size: u64::MAX, compressed_size: 10, offsets: Vec::new(), table: None }.to_bytes();
    assert!(Index::parse(&header, &member).is_err());
    header[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Index::parse(&header, &member).is_err());
    let member = CdEntry { uncompressed_size: 2, ..member };
    let index = Index { chunk_size: 1, uncompressed_size: 2, compressed_size: 10, offsets: vec![5], table: None };
    assert!(Index::parse(&index.to_bytes(), &member).is_ok());
    let mut trailing = index.to_bytes();
    trailing.push(0);
    assert!(Index::parse(&trailing, &member).is_err());
}

#[test]
fn rejects_incomplete_or_trailing_chunk_streams() {
    let mut deflater = Deflater::new(Vec::new(), Some(4096));
    let bytes = data(10_000);
    deflater.write_all(&bytes).unwrap();
    let (compressed, compressed_size, offsets, _) = deflater.finish().unwrap();
    let index = Index { chunk_size: 4096, uncompressed_size: bytes.len() as u64, compressed_size, offsets, table: None };
    let mut reader = std::io::Cursor::new(&compressed);
    assert_eq!(inflate_chunk(&mut reader, 0, &index, 0, 4096).unwrap(), bytes[..4096]);
    assert_eq!(inflate_chunk(&mut reader, 0, &index, 2, 1808).unwrap(), bytes[8192..]);
    let mut bad_marker = compressed.clone();
    bad_marker[index.offset(1) as usize - 1] = 0;
    assert!(inflate_chunk(&mut std::io::Cursor::new(bad_marker), 0, &index, 0, 4096).is_err());
    let mut short = index.clone();
    short.compressed_size -= 1;
    assert!(inflate_chunk(&mut std::io::Cursor::new(&compressed), 0, &short, 2, 1808).is_err());
    let mut trailing = compressed.clone();
    trailing.push(0);
    let mut extra = index.clone();
    extra.compressed_size += 1;
    assert!(inflate_chunk(&mut std::io::Cursor::new(trailing), 0, &extra, 2, 1808).is_err());
}

#[test]
fn rejects_member_range_overflow_before_reading_an_index() {
    let mut bytes = vec![0u8; 30];
    bytes[..4].copy_from_slice(&zipdir::LOCAL.to_le_bytes());
    let member = CdEntry {
        name: "x".into(),
        flags: 0,
        method: 8,
        dos_time: 0,
        dos_date: 0,
        crc32: 0,
        compressed_size: u64::MAX,
        uncompressed_size: 10_000,
        local_offset: 0,
        extra: Vec::new(),
        raw: Vec::new(),
    };
    assert!(read_index(&mut std::io::Cursor::new(bytes), &member).unwrap().unwrap_err().contains("beyond the archive"));
}

#[test]
fn invalid_properties_and_names_preserve_existing_outputs() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("a.txt");
    let output = tmp.path().join("out.zip");
    fs::write(&input, b"hello").unwrap();
    fs::write(&output, b"preserve").unwrap();
    let options = SozipOptions { overwrite: true, content_type: Some("x".repeat(65536)), ..Default::default() };
    assert!(matches!(create(&[&s(&input)], &s(&output), &options), Err(Error::InvalidArgument(_))));
    assert_eq!(fs::read(&output).unwrap(), b"preserve");
    let root = "/vsimem/sozip-name-limits";
    let long = format!("{root}/{}", "x".repeat(65536));
    crate::vsi::write(&long, b"hello").unwrap();
    let options = SozipOptions { overwrite: true, ..Default::default() };
    assert!(matches!(create(&[&long], &s(&output), &options), Err(Error::InvalidArgument(_))));
    assert_eq!(fs::read(&output).unwrap(), b"preserve");
    crate::vsi::delete(root, true).unwrap();
}

#[test]
fn index_probe_propagates_io_failure() {
    struct FailingReader {
        bytes: std::io::Cursor<Vec<u8>>,
        fail_at: u64,
    }
    impl Read for FailingReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.bytes.position() == self.fail_at {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "injected transport failure"));
            }
            self.bytes.read(out)
        }
    }
    impl Seek for FailingReader {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.bytes.seek(pos)
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("data.bin");
    let archive = tmp.path().join("data.zip");
    fs::write(&source, data(10_000)).unwrap();
    create(&[&s(&source)], &s(&archive), &yes(4096)).unwrap();
    let mut bytes = std::io::Cursor::new(fs::read(archive).unwrap());
    let member = zipdir::central_directory(&mut bytes).unwrap().remove(0);
    let fail_at = zipdir::data_offset(&mut bytes, &member).unwrap() + member.compressed_size;
    let mut failing = FailingReader { bytes, fail_at };
    assert!(matches!(read_index(&mut failing, &member), Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut));
}

#[test]
fn small_chunk_does_not_allocate_its_forged_decoded_size() {
    let mut deflater = Deflater::new(Vec::new(), None);
    deflater.write_all(b"hello").unwrap();
    let (compressed, compressed_size, _, _) = deflater.finish().unwrap();
    let index = Index { chunk_size: u32::MAX, uncompressed_size: u32::MAX as u64, compressed_size, offsets: vec![], table: None };
    assert!(inflate_chunk(&mut std::io::Cursor::new(compressed), 0, &index, 0, u32::MAX as usize - 1).is_err());
}

#[test]
fn lazy_zip_errors_and_unindexed_fallback_are_explicit() {
    let tmp = tempfile::tempdir().unwrap();
    let raster =
        crate::Raster::new(crate::RasterConfig { cols: 10, rows: 10, bands: 1, data_type: crate::DataType::U8, ..Default::default() });
    let source = tmp.path().join("small.tif");
    crate::write_raster(&raster, &s(&source), &Default::default()).unwrap();
    let archive = tmp.path().join("plain.zip");
    create(&[&s(&source)], &s(&archive), &SozipOptions { enable: EnableSozip::No, junk_paths: true, ..Default::default() }).unwrap();
    let path = format!("/vsizip/{}/small.tif", s(&archive));
    let mut lazy = crate::open_raster_lazy(&path).unwrap();
    let window = lazy.read_window(crate::Window { x: 1, y: 2, width: 3, height: 4 }, &Default::default()).unwrap();
    assert_eq!(window.raster.get(0, 0, 0), 0.0);
    assert!(crate::open_raster_lazy(&format!("/vsizip/{}/missing.tif", s(&archive))).is_err());
    let malformed = tmp.path().join("bad.zip");
    fs::write(&malformed, b"not a ZIP archive").unwrap();
    assert!(crate::open_raster_lazy(&format!("/vsizip/{}/small.tif", s(&malformed))).is_err());
}

#[test]
fn chunks_larger_than_the_decode_scratch_buffer_remain_supported() {
    let bytes = data(600_000);
    let mut deflater = Deflater::new(Vec::new(), Some(262_144));
    deflater.write_all(&bytes).unwrap();
    let (compressed, compressed_size, offsets, _) = deflater.finish().unwrap();
    let index = Index { chunk_size: 262_144, uncompressed_size: bytes.len() as u64, compressed_size, offsets, table: None };
    for k in 0..index.chunks() {
        let expected = &bytes[k * 262_144..((k + 1) * 262_144).min(bytes.len())];
        assert_eq!(inflate_chunk(&mut std::io::Cursor::new(&compressed), 0, &index, k, expected.len()).unwrap(), expected);
    }
}

struct CountedReader {
    bytes: std::io::Cursor<Vec<u8>>,
    count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}
impl Read for CountedReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.bytes.read(bytes)?;
        self.count.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}
impl Seek for CountedReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.bytes.seek(pos)
    }
}

#[test]
fn index_header_is_checked_before_fetching_a_large_claimed_table() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("data");
    let archive = tmp.path().join("data.zip");
    fs::write(&source, data(10_000)).unwrap();
    create(&[&s(&source)], &s(&archive), &yes(4096)).unwrap();
    let mut bytes = std::io::Cursor::new(fs::read(archive).unwrap());
    let entry = zipdir::central_directory(&mut bytes).unwrap().remove(0);
    let header = (zipdir::data_offset(&mut bytes, &entry).unwrap() + entry.compressed_size) as usize;
    let mut bytes = bytes.into_inner();
    let claimed = 8 * 1024 * 1024u32;
    bytes[header + 18..header + 22].copy_from_slice(&claimed.to_le_bytes());
    bytes[header + 22..header + 26].copy_from_slice(&claimed.to_le_bytes());
    bytes.resize(header + 30 + index_name("data").len() + claimed as usize, 0);
    let count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut reader = CountedReader { bytes: std::io::Cursor::new(bytes), count: count.clone() };
    assert!(read_index(&mut reader, &entry).unwrap().is_err());
    assert!(count.load(std::sync::atomic::Ordering::Relaxed) < 512, "index body was downloaded before rejecting its header");
}

#[test]
fn a_small_read_from_a_large_chunk_has_bounded_io_and_reopens_independently() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("data");
    let archive = tmp.path().join("data.zip");
    let expected = data(3 * 1024 * 1024);
    fs::write(&source, &expected).unwrap();
    create(&[&s(&source)], &s(&archive), &yes(1024 * 1024)).unwrap();
    let bytes = fs::read(&archive).unwrap();
    let length = bytes.len() as u64;
    let count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut reader =
        MemberReader::open(Box::new(CountedReader { bytes: std::io::Cursor::new(bytes), count: count.clone() }), "data").unwrap().unwrap();
    count.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut tiny = [0; 16];
    reader.read_exact(&mut tiny).unwrap();
    assert_eq!(tiny, expected[..16]);
    assert!(count.load(std::sync::atomic::Ordering::Relaxed) < 128 * 1024, "a tiny read decoded/fetched a full MiB chunk");
    reader.seek(SeekFrom::Start(0)).unwrap();
    count.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut back = Vec::new();
    reader.read_to_end(&mut back).unwrap();
    assert_eq!(back, expected);
    assert!(count.load(std::sync::atomic::Ordering::Relaxed) < length + 8192, "sequential reads re-decompressed earlier windows");
    let location = crate::vfs::Location::Local(archive.clone());
    let original = open_member(&location, "data").unwrap().unwrap();
    let reopen = original.reopen().unwrap();
    let handles = [17u64, 1_500_000]
        .into_iter()
        .map(|at| {
            let reopen = reopen.clone();
            std::thread::spawn(move || {
                let mut reader = reopen.open().unwrap();
                reader.seek(SeekFrom::Start(at)).unwrap();
                let mut bytes = [0; 32];
                reader.read_exact(&mut bytes).unwrap();
                (at, bytes)
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        let (at, bytes) = handle.join().unwrap();
        assert_eq!(bytes, expected[at as usize..at as usize + 32]);
    }
}

#[test]
fn append_and_optimize_preserve_comments_and_cached_indexes_are_small() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join("comment.zip");
    {
        let mut zip = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
        zip.set_comment("keep this archive note").unwrap();
        zip.start_file("a", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"hello").unwrap();
        zip.finish().unwrap();
    }
    let source = tmp.path().join("b");
    fs::write(&source, b"world").unwrap();
    create(&[&s(&source)], &s(&archive), &yes(4096)).unwrap();
    let zip = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
    assert_eq!(zip.comment(), b"keep this archive note");
    let output = tmp.path().join("optimized.zip");
    optimize(&s(&archive), &s(&output), &Default::default()).unwrap();
    let zip = zip::ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
    assert_eq!(zip.comment(), b"keep this archive note");
    let entries = zipdir::central_directory(&mut fs::File::open(&archive).unwrap()).unwrap();
    assert!(entries.iter().all(|entry| entry.raw.is_empty() && entry.extra.is_empty()));
}

#[test]
fn forced_zip64_descriptors_on_small_members_keep_their_indexes() {
    let data = data(10_000);
    let mut deflater = Deflater::new(Vec::new(), Some(4096));
    deflater.write_all(&data).unwrap();
    let (compressed, size, offsets, crc) = deflater.finish().unwrap();
    let index = Index { chunk_size: 4096, uncompressed_size: data.len() as u64, compressed_size: size, offsets, table: None };
    let mut zip = ZipOut::new(Vec::new(), 0, vec![]);
    zip.add("data", 8, (0, 0), crc, data.len() as u64, &mut std::io::Cursor::new(&compressed), size, vec![], Some(&index)).unwrap();
    let mut bytes = zip.finish().unwrap();
    let old_end = bytes.len() - 22;
    let old_cd = u32::from_le_bytes(bytes[old_end + 16..old_end + 20].try_into().unwrap()) as usize;
    let mut extra = vec![1, 0, 16, 0];
    extra.extend_from_slice(&(data.len() as u64).to_le_bytes());
    extra.extend_from_slice(&size.to_le_bytes());
    bytes.splice(34..34, extra);
    let descriptor_at = 34 + 20 + size as usize;
    let mut descriptor = 0x08074b50u32.to_le_bytes().to_vec();
    descriptor.extend_from_slice(&crc.to_le_bytes());
    descriptor.extend_from_slice(&size.to_le_bytes());
    descriptor.extend_from_slice(&(data.len() as u64).to_le_bytes());
    bytes.splice(descriptor_at..descriptor_at, descriptor);
    bytes[4..6].copy_from_slice(&45u16.to_le_bytes());
    bytes[6..8].copy_from_slice(&8u16.to_le_bytes());
    bytes[18..26].fill(255);
    bytes[28..30].copy_from_slice(&20u16.to_le_bytes());
    let cd = old_cd + 44;
    bytes[cd + 6..cd + 8].copy_from_slice(&45u16.to_le_bytes());
    bytes[cd + 8..cd + 10].copy_from_slice(&8u16.to_le_bytes());
    let end = old_end + 44;
    bytes[end + 16..end + 20].copy_from_slice(&(cd as u32).to_le_bytes());
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("forced.zip");
    fs::write(&path, &bytes).unwrap();
    assert_eq!(list(&s(&path)).unwrap()[0].sozip_chunk_size, Some(4096));
    assert!(validate(&s(&path)).unwrap().valid);
}

#[test]
fn central_extra_lengths_dates_and_directory_headers_are_checked() {
    let mut zip = ZipOut::new(Vec::new(), u32::MAX as u64, vec![]);
    zip.add("x", 0, (0, 0), 0, 0, &mut std::io::empty(), 0, vec![0; 65520], None).unwrap();
    assert!(matches!(zip.finish(), Err(Error::InvalidArgument(_))));
    assert_eq!(dos_time(Some(i64::MAX)), ((23 << 11) | (59 << 5) | 29, (127 << 9) | (12 << 5) | 31));
    assert_eq!(dos_time(Some(i64::MIN)), (0, (1 << 5) | 1));
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("directory.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    zip.add_directory("dir/", zip::write::SimpleFileOptions::default()).unwrap();
    zip.finish().unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes[..4].fill(0);
    fs::write(&path, bytes).unwrap();
    assert!(!validate(&s(&path)).unwrap().valid);
}

fn stored_archive(payload: &[u8]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("data", zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
    zip.write_all(payload).unwrap();
    zip.finish().unwrap().into_inner()
}

#[test]
fn recursive_create_excludes_existing_and_temporary_outputs() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("a.csv");
    let output = root.path().join("out.zip");
    fs::write(&input, data(10_000)).unwrap();
    let options = SozipOptions { recursive: true, ..yes(4096) };
    assert_eq!(create(&[&s(root.path())], &s(&output), &options).unwrap().added, 1);
    assert_eq!(list(&s(&output)).unwrap().iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["a.csv"]);
    fs::remove_file(input).unwrap();
    fs::write(root.path().join("b.csv"), data(12_000)).unwrap();
    assert_eq!(create(&[&s(root.path())], &s(&output), &options).unwrap().added, 1);
    assert_eq!(list(&s(&output)).unwrap().iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["a.csv", "b.csv"]);
    assert!(validate(&s(&output)).unwrap().valid);
    assert_eq!(create(&[&s(root.path())], &s(&output), &SozipOptions { overwrite: true, ..options }).unwrap().added, 1);
    assert_eq!(list(&s(&output)).unwrap()[0].name, "b.csv");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
}

#[test]
fn remote_member_names_omit_userinfo_queries_and_fragments() {
    for prefix in ["https://", "/vsicurl/https://"] {
        let input = format!("{prefix}user:TEST_SECRET@example.com/data/file.csv?sig=TEST_TOKEN#fragment");
        assert_eq!(stored_input_name(&input, true, false), "example.com/data/file.csv");
        assert_eq!(stored_input_name(&input, true, true), "file.csv");
    }
    assert_eq!(stored_input_name("https://USER_SECRET@[::1]:8080/file@name", true, false), "[::1]:8080/file@name");
    assert_eq!(stored_input_name("local/name?literal", false, false), "local/name?literal");
    assert_eq!(stored_input_name("/home/u/a:/nested/file.txt", false, false), "home/u/a:/nested/file.txt");
    assert_eq!(stored_input_name("C:/nested/file.txt", false, false), "nested/file.txt");
    assert_eq!(stored_input_name(r"a\b.csv", false, true), "b.csv");
    assert_eq!(stored_input_name(r"https://example.com/a\b.csv", true, false), r"example.com/a\b.csv");
    if !cfg!(windows) {
        assert_eq!(stored_input_name(r"a\b.csv", false, false), r"a\b.csv");
    }
}

#[test]
fn auto_chunking_uses_the_streamed_length_with_an_unknown_size_hint() {
    let root = tempfile::tempdir().unwrap();
    for (length, indexed) in [(6000, false), (12000, true)] {
        let payload = data(length);
        let options = SozipOptions { enable: EnableSozip::Auto, chunk_size: 4096, min_file_size: 8000, ..Default::default() };
        let mut bytes = Vec::new();
        let mut zip = ZipOut::new(&mut bytes, 0, Vec::new());
        let result = add_member(&mut zip, "data", 0, dos_time(None), &options, Vec::new(), |out| {
            out.write_all(&payload)?;
            Ok(payload.len() as u64)
        })
        .unwrap();
        assert_eq!(result, (length as u64, indexed));
        zip.finish().unwrap();
        let path = root.path().join(format!("{length}.zip"));
        fs::write(&path, &bytes).unwrap();
        assert_eq!(list(&s(&path)).unwrap()[0].sozip_chunk_size.is_some(), indexed);
        assert!(validate(&s(&path)).unwrap().valid);
        assert_eq!(crate::vsi::read(&format!("/vsizip/{}/data", s(&path))).unwrap(), payload);
    }
}

#[test]
fn append_rejects_replaced_native_or_memory_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("archive.zip");
    let replacement = root.path().join("replacement.zip");
    let first = stored_archive(b"old!");
    let second = stored_archive(b"new!");
    assert_eq!(first.len(), second.len());
    fs::write(&output, &first).unwrap();
    fs::write(&replacement, &second).unwrap();
    for path in [&output, &replacement] {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        #[cfg(windows)]
        matching_windows_timestamps(path);
    }
    let (out, _) = Output::open(&s(&output), false).unwrap();
    fs::rename(replacement, &output).unwrap();
    assert!(out.build(|_| Ok(SozipReport::default())).unwrap_err().to_string().contains("changed"));
    assert_eq!(fs::read(&output).unwrap(), second);
    let path = "/vsimem/sozip-append-version-regression.zip";
    crate::vsi::write(path, &first).unwrap();
    let (out, _) = Output::open(path, false).unwrap();
    crate::vsi::write(path, &second).unwrap();
    assert!(out.build(|_| Ok(SozipReport::default())).unwrap_err().to_string().contains("changed"));
    assert_eq!(crate::vsi::read(path).unwrap(), second);
    crate::vsi::delete(path, false).unwrap();
}

#[cfg(windows)]
fn matching_windows_timestamps(path: &Path) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::SetFileTime;
    // 1970-01-01 in Windows' 100 ns intervals since 1601-01-01.
    let epoch = 116_444_736_000_000_000u64;
    let timestamp = FILETIME { dwLowDateTime: epoch as u32, dwHighDateTime: (epoch >> 32) as u32 };
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    // SAFETY: the handle is live and writable; the input FILETIME remains valid
    // throughout the call. A null access-time pointer leaves that field unchanged.
    assert_ne!(unsafe { SetFileTime(file.as_raw_handle(), &timestamp, std::ptr::null(), &timestamp) }, 0);
}

#[cfg(windows)]
#[test]
fn windows_archive_snapshots_reject_matching_metadata_replacements() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.zip");
    let replacement = root.path().join("replacement.zip");
    fs::write(&path, stored_archive(b"old!")).unwrap();
    fs::write(&replacement, stored_archive(b"new!")).unwrap();
    matching_windows_timestamps(&path);
    matching_windows_timestamps(&replacement);
    let original = fs::File::open(&path).unwrap();
    let other = fs::File::open(&replacement).unwrap();
    let before = original.metadata().unwrap();
    let after = other.metadata().unwrap();
    assert_eq!(
        (before.len(), before.modified().unwrap(), before.created().unwrap()),
        (after.len(), after.modified().unwrap(), after.created().unwrap())
    );
    let snapshot = NativeSnapshot::capture(&original).unwrap();
    assert!(snapshot.matches_file(&original).unwrap());
    assert!(!snapshot.matches_file(&other).unwrap());
    drop(other);
    let mut reader = open_member(&crate::vfs::Location::Local(path.clone()), "data").unwrap().unwrap();
    let reopen = reader.reopen().unwrap();
    assert!(reopen.open().is_ok());
    fs::rename(&replacement, &path).unwrap();
    assert!(!snapshot.matches_path(&path).unwrap());
    assert!(reopen.open().is_err());
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"old!");
    assert_eq!(crate::vsi::read(&format!("/vsizip/{}/data", s(&path))).unwrap(), b"new!");
}

#[test]
fn stored_members_reject_mismatched_sizes_and_out_of_file_extents() {
    let original = stored_archive(b"data");
    let at = original.windows(4).position(|part| part == b"PK\x01\x02").unwrap();
    for both in [false, true] {
        let mut bytes = original.clone();
        bytes[at + 24..at + 28].copy_from_slice(&1000u32.to_le_bytes());
        if both {
            bytes[at + 20..at + 24].copy_from_slice(&1000u32.to_le_bytes());
        }
        assert!(MemberReader::open(Box::new(std::io::Cursor::new(bytes)), "data").unwrap().is_none());
    }
}

#[test]
fn stored_reads_report_truncation_and_checked_offset_overflow() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("stored.zip");
    fs::write(&path, stored_archive(b"data")).unwrap();
    let mut reader = MemberReader::open(Box::new(fs::File::open(&path).unwrap()), "data").unwrap().unwrap();
    fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(reader.data_start + 2).unwrap();
    assert_eq!(reader.read_to_end(&mut Vec::new()).unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
    let mut reader = MemberReader {
        inner: Box::new(std::io::Cursor::new(Vec::<u8>::new())),
        data_start: u64::MAX,
        size: 2,
        index: None,
        pos: 1,
        cached: None,
        decoder: None,
        reopen: None,
    };
    assert_eq!(reader.read(&mut [0]).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn legacy_cp437_hidden_index_names_match_the_member() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("cafe.csv");
    let output = tmp.path().join("legacy.zip");
    let payload = data(20_000);
    fs::write(&input, &payload).unwrap();
    create(&[&s(&input)], &s(&output), &yes(4096)).unwrap();
    let mut bytes = fs::read(&output).unwrap();
    // Preserve every record length while converting the visible and hidden names
    // to unflagged CP437, as used by legacy ZIP producers.
    for at in 0..bytes.len().saturating_sub(46) {
        let signature = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let (flag, name) = if signature == zipdir::LOCAL {
            (at + 6, at + 30)
        } else if signature == 0x0201_4b50 {
            (at + 8, at + 46)
        } else {
            continue;
        };
        let flags = u16::from_le_bytes(bytes[flag..flag + 2].try_into().unwrap()) & !0x800;
        bytes[flag..flag + 2].copy_from_slice(&flags.to_le_bytes());
        let name = name + usize::from(bytes[name] == b'.');
        assert_eq!(&bytes[name..name + 4], b"cafe");
        bytes[name + 3] = 0x82;
    }
    fs::write(&output, &bytes).unwrap();
    assert_eq!(list(&s(&output)).unwrap()[0].sozip_chunk_size, Some(4096));
    assert!(validate(&s(&output)).unwrap().valid);
    let mut reader = MemberReader::open(Box::new(std::io::Cursor::new(bytes)), "café.csv").unwrap().unwrap();
    let mut back = Vec::new();
    reader.read_to_end(&mut back).unwrap();
    assert_eq!(back, payload);
}

#[test]
fn optimize_preserves_all_kv_fields_on_files_and_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("directory.zip");
    let output = tmp.path().join("optimized.zip");
    let mut extra = kv_extra(&[("Content-Type", "inode/directory")]).unwrap();
    extra.extend_from_slice(&kv_extra(&[("Other", "second field")]).unwrap());
    let mut zip = ZipOut::new(Vec::new(), 0, vec![]);
    zip.add("dir/", 0, (0, 0), 0, 0, &mut std::io::empty(), 0, extra.clone(), None).unwrap();
    zip.add("dir/empty", 0, (0, 0), 0, 0, &mut std::io::empty(), 0, extra.clone(), None).unwrap();
    fs::write(&input, zip.finish().unwrap()).unwrap();
    optimize(&s(&input), &s(&output), &SozipOptions { junk_paths: false, ..yes(4096) }).unwrap();
    let (entries, _, _) = zipdir::central_directory_at(&mut fs::File::open(&output).unwrap()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "dir/");
    for entry in entries {
        assert_eq!(keep_kv(&entry.extra), extra);
        assert_eq!(kv_properties(&entry.extra), ["Content-Type=inode/directory", "Other=second field"]);
    }
    assert_eq!(list(&s(&output)).unwrap()[0].properties, ["Content-Type=inode/directory", "Other=second field"]);
    assert!(validate(&s(&output)).unwrap().valid);
}

#[test]
fn create_accepts_existing_empty_native_and_memory_outputs() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("input.txt");
    let output = tmp.path().join("empty.zip");
    fs::write(&input, b"payload").unwrap();
    fs::write(&output, []).unwrap();
    assert_eq!(create(&[&s(&input)], &s(&output), &yes(4096)).unwrap().added, 1);
    assert!(validate(&s(&output)).unwrap().valid);
    assert_eq!(crate::vsi::read(&format!("/vsizip/{}/input.txt", s(&output))).unwrap(), b"payload");
    let memory = format!("/vsimem/sozip-empty-{}/out.zip", tmp.path().file_name().unwrap().to_string_lossy());
    crate::vsi::write(&memory, &[]).unwrap();
    assert_eq!(create(&[&s(&input)], &memory, &yes(4096)).unwrap().added, 1);
    assert!(validate(&memory).unwrap().valid);
    assert_eq!(crate::vsi::read(&format!("/vsizip/{memory}/input.txt")).unwrap(), b"payload");
    crate::vsi::delete(&memory, false).unwrap();
}

#[test]
fn hidden_zip64_size_can_follow_other_extra_fields() {
    let payload = data(10_000);
    let mut deflater = Deflater::new(Vec::new(), Some(4096));
    deflater.write_all(&payload).unwrap();
    let (compressed, size, offsets, crc) = deflater.finish().unwrap();
    let index = Index { chunk_size: 4096, uncompressed_size: payload.len() as u64, compressed_size: size, offsets, table: None };
    let mut zip = ZipOut::new(Vec::new(), 0, vec![]);
    zip.add("data", 8, (0, 0), crc, payload.len() as u64, &mut std::io::Cursor::new(compressed), size, vec![], Some(&index)).unwrap();
    let mut bytes = zip.finish().unwrap();
    let at = 34 + size as usize;
    let old_end = bytes.len() - 22;
    let old_cd = u32::from_le_bytes(bytes[old_end + 16..old_end + 20].try_into().unwrap());
    let mut extra = vec![0xfe, 0xca, 0, 0, 1, 0, 16, 0];
    let index_size = index.to_bytes().len() as u64;
    extra.extend_from_slice(&index_size.to_le_bytes());
    extra.extend_from_slice(&index_size.to_le_bytes());
    let extra_len = extra.len();
    let extra_at = at + 30 + index_name("data").len();
    bytes.splice(extra_at..extra_at, extra);
    bytes[at + 4..at + 6].copy_from_slice(&45u16.to_le_bytes());
    bytes[at + 18..at + 26].fill(255);
    bytes[at + 28..at + 30].copy_from_slice(&(extra_len as u16).to_le_bytes());
    let end = old_end + extra_len;
    bytes[end + 16..end + 20].copy_from_slice(&(old_cd + extra_len as u32).to_le_bytes());
    let entry = zipdir::central_directory(&mut std::io::Cursor::new(&bytes)).unwrap().remove(0);
    assert!(read_index(&mut std::io::Cursor::new(bytes), &entry).unwrap().unwrap().is_some());
}

#[test]
fn offset_budget_stops_tiny_chunk_growth_before_allocating_more() {
    let mut deflater = Deflater::new(Vec::new(), Some(1));
    deflater.offsets = vec![0; MAX_WRITER_OFFSETS];
    let error = deflater.write_all(b"ab").unwrap_err();
    assert!(error.to_string().contains("offset budget"));
    assert_eq!(deflater.offsets.len(), MAX_WRITER_OFFSETS);
}

#[cfg(unix)]
#[test]
fn native_append_holds_an_exclusive_lock_until_publication_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("archive.zip");
    fs::write(&path, stored_archive(b"old")).unwrap();
    let (first, _) = Output::open(&s(&path), false).unwrap();
    assert!(Output::open(&s(&path), false).is_err());
    first
        .build(|_| {
            assert!(Output::open(&s(&path), false).is_err());
            Ok(SozipReport::default())
        })
        .unwrap();
    assert!(Output::open(&s(&path), false).is_ok());
}

#[test]
fn absent_outputs_do_not_replace_intervening_native_or_memory_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let native = s(&tmp.path().join("out.zip"));
    let memory = format!("/vsimem/sozip-absent-{}/out.zip", tmp.path().file_name().unwrap().to_string_lossy());
    for path in [&native, &memory] {
        let (output, _) = Output::open(path, false).unwrap();
        let result = output.build(|_| {
            crate::vsi::write(path, b"another writer").unwrap();
            Ok(SozipReport::default())
        });
        assert!(result.is_err());
        assert_eq!(crate::vsi::read(path).unwrap(), b"another writer");
    }
    crate::vsi::delete(&memory, false).unwrap();
}

#[cfg(unix)]
#[test]
fn native_member_reopens_reject_replaced_or_modified_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("source.zip");
    let original = stored_archive(b"original");
    fs::write(&path, &original).unwrap();
    let location = crate::vfs::Location::Local(path.clone());
    let mut reader = open_member(&location, "data").unwrap().unwrap();
    let reopen = reader.reopen().unwrap();
    assert!(reopen.open().is_ok());
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let replacement = tmp.path().join("replacement.zip");
    fs::write(&replacement, stored_archive(b"replaced")).unwrap();
    fs::File::open(&replacement).unwrap().set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
    fs::rename(replacement, &path).unwrap();
    assert!(reopen.open().is_err());
    let mut back = Vec::new();
    reader.read_to_end(&mut back).unwrap();
    assert_eq!(back, b"original");
    let reader = open_member(&location, "data").unwrap().unwrap();
    let reopen = reader.reopen().unwrap();
    fs::write(&path, original).unwrap();
    fs::File::open(&path).unwrap().set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
    assert!(reopen.open().is_err());
}
