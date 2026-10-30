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
    let r = optimize(&s(&plain), &s(&out), &yes(8192)).unwrap();
    assert_eq!((r.added, r.seek_optimized), (2, 1));
    let v = validate(&s(&out)).unwrap();
    assert!(v.valid && v.sozip_files == ["dir/big.csv"], "{v:?}");
    assert_eq!(crate::vsi::read(&format!("/vsizip/{}/dir/big.csv", s(&out))).unwrap(), data(300_000));
    assert!(optimize(&s(&plain), &s(&out), &yes(8192)).is_err(), "an existing output needs overwrite");
    assert!(optimize(&s(&out), &s(&out), &SozipOptions { overwrite: true, ..yes(8192) }).is_err());

    // A corrupted offset in the index is reported.
    let mut bytes = fs::read(&out).unwrap();
    let idx =
        bytes.windows(23).position(|w| w == b".big.csv.sozip.idx\x01\x00\x00\x00" || w.starts_with(b"dir/.big.csv.sozip.idx")).unwrap();
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
    let mut header = Index { chunk_size: 1, uncompressed_size: u64::MAX, compressed_size: 10, offsets: Vec::new() }.to_bytes();
    assert!(Index::parse(&header, &member).is_err());
    header[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Index::parse(&header, &member).is_err());
    let member = CdEntry { uncompressed_size: 2, ..member };
    let index = Index { chunk_size: 1, uncompressed_size: 2, compressed_size: 10, offsets: vec![5] };
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
    let index = Index { chunk_size: 4096, uncompressed_size: bytes.len() as u64, compressed_size, offsets };
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
    let index = Index { chunk_size: u32::MAX, uncompressed_size: u32::MAX as u64, compressed_size, offsets: vec![] };
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
    let index = Index { chunk_size: 262_144, uncompressed_size: bytes.len() as u64, compressed_size, offsets };
    for k in 0..index.chunks() {
        let expected = &bytes[k * 262_144..((k + 1) * 262_144).min(bytes.len())];
        assert_eq!(inflate_chunk(&mut std::io::Cursor::new(&compressed), 0, &index, k, expected.len()).unwrap(), expected);
    }
}
