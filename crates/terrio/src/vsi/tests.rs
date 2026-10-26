use super::*;
use std::fs;

/// `t/x.txt`, `t/a/y.txt`, `t/a/b/z.txt` under a temporary directory.
fn tree() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().join("t");
    fs::create_dir_all(t.join("a/b")).unwrap();
    fs::write(t.join("x.txt"), "hello\n").unwrap();
    fs::write(t.join("a/y.txt"), "abc\n").unwrap();
    fs::write(t.join("a/b/z.txt"), "z\n").unwrap();
    (tmp, t)
}

fn names(entries: &[Entry]) -> Vec<String> {
    entries.iter().map(|e| e.name.clone()).collect()
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

fn files_below(dir: &Path) -> Vec<String> {
    let entries = list(&s(dir), &ListOptions { recursive: true, ..Default::default() }).unwrap();
    entries.into_iter().filter(|e| !e.is_dir()).map(|e| e.name).collect()
}

#[test]
fn lists_local_directories() {
    let (_tmp, t) = tree();
    assert_eq!(names(&list(&s(&t), &ListOptions::default()).unwrap()), ["a", "x.txt"]);
    let all = list(&s(&t), &ListOptions { recursive: true, ..Default::default() }).unwrap();
    assert_eq!(names(&all), ["a", "a/b", "a/b/z.txt", "a/y.txt", "x.txt"]);
    let x = all.iter().find(|e| e.name == "x.txt").unwrap();
    assert_eq!((x.kind, x.size), (EntryKind::File, Some(6)));
    assert!(x.modified.is_some());
    assert!(x.long_line().ends_with(" 6 ".to_string().as_str()) || x.long_line().contains("            6 "), "{}", x.long_line());
    let depth1 = list(&s(&t), &ListOptions { recursive: true, depth: Some(1), ..Default::default() }).unwrap();
    assert_eq!(names(&depth1), ["a", "x.txt"]);
    let depth2 = list(&s(&t), &ListOptions { recursive: true, depth: Some(2), ..Default::default() }).unwrap();
    assert_eq!(names(&depth2), ["a", "a/b", "a/y.txt", "x.txt"]);
    let abs = list(&s(&t), &ListOptions { absolute: true, ..Default::default() }).unwrap();
    assert_eq!(abs[1].name, format!("{}/x.txt", s(&std::path::absolute(&t).unwrap())));
    // A file lists as itself.
    let file = list(&s(&t.join("x.txt")), &ListOptions::default()).unwrap();
    assert_eq!(names(&file), [s(&t.join("x.txt"))]);
    assert!(list(&s(&t.join("missing")), &ListOptions::default()).unwrap_err().to_string().contains("does not exist"));
}

#[test]
fn formats_times_as_gdal() {
    assert_eq!(format_time(0), "1970-01-01 00:00:00Z");
    assert_eq!(format_time(1_704_073_347), "2024-01-01 01:42:27Z");
    assert_eq!(crate::civil_time::unix_seconds(2024, 1, 1, 1, 42, 27), 1_704_073_347);
    let e = Entry { name: "a".into(), kind: EntryKind::Directory, size: Some(80), modified: Some(0), permissions: None, etag: None };
    assert_eq!(e.long_line(), "d--------- 1 unknown unknown           80 1970-01-01 00:00 a");
}

#[test]
fn copies_files_and_trees_as_gdal() {
    let (tmp, t) = tree();
    // A directory needs the recursive option.
    let o1 = tmp.path().join("o1");
    assert!(copy(&s(&t), &s(&o1), &CopyOptions::default()).is_err());
    // To a new name: the tree becomes it.
    let r = copy(&s(&t), &s(&o1), &CopyOptions { recursive: true, ..Default::default() }).unwrap();
    assert_eq!((r.copied, r.bytes), (3, 6 + 4 + 2));
    assert_eq!(files_below(&o1), ["a/b/z.txt", "a/y.txt", "x.txt"]);
    // Into an existing directory: below it, by name.
    let o2 = tmp.path().join("o2");
    fs::create_dir(&o2).unwrap();
    copy(&s(&t), &s(&o2), &CopyOptions { recursive: true, ..Default::default() }).unwrap();
    assert_eq!(files_below(&o2), ["t/a/b/z.txt", "t/a/y.txt", "t/x.txt"]);
    copy(&s(&t.join("x.txt")), &s(&o2), &CopyOptions::default()).unwrap();
    assert_eq!(fs::read_to_string(o2.join("x.txt")).unwrap(), "hello\n");
    copy(&s(&t.join("x.txt")), &s(&tmp.path().join("o4.txt")), &CopyOptions::default()).unwrap();
    assert!(tmp.path().join("o4.txt").is_file());
    // Not into itself, not onto itself.
    assert!(copy(&s(&t), &s(&t.join("a")), &CopyOptions { recursive: true, ..Default::default() }).is_err());
    assert!(copy(&s(&t.join("x.txt")), &s(&t.join("x.txt")), &CopyOptions::default()).is_err());
    assert_eq!(fs::read_to_string(t.join("x.txt")).unwrap(), "hello\n");
}

#[cfg(unix)]
#[test]
fn copy_failures_leave_the_target_and_skip_errors() {
    use std::os::unix::fs::PermissionsExt;
    let (tmp, t) = tree();
    let unreadable = t.join("a/secret.txt");
    fs::write(&unreadable, "s").unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&unreadable).is_ok() {
        return; // Running as root: permissions are not enforced.
    }
    let out = tmp.path().join("out");
    assert!(copy(&s(&t), &s(&out), &CopyOptions { recursive: true, ..Default::default() }).is_err());
    let r = copy(&s(&t), &s(&out), &CopyOptions { recursive: true, skip_errors: true }).unwrap();
    assert_eq!(r.copied, 3);
    assert_eq!(r.errors.len(), 1, "{r:?}");
    // No partial file nor temporary file left for the failed one.
    assert!(!out.join("a/secret.txt").exists());
    assert!(fs::read_dir(out.join("a")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("terrio-partial")));
    // An existing target is kept when its replacement fails.
    fs::write(out.join("a/secret.txt"), "old").unwrap();
    assert!(copy(&s(&unreadable), &s(&out.join("a/secret.txt")), &CopyOptions::default()).is_err());
    assert_eq!(fs::read_to_string(out.join("a/secret.txt")).unwrap(), "old");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn deletes_only_below_the_path() {
    let (tmp, t) = tree();
    // Siblings sharing the name as a prefix.
    let sibling = tmp.path().join("t2");
    fs::create_dir(&sibling).unwrap();
    fs::write(sibling.join("keep.txt"), "k").unwrap();
    fs::write(tmp.path().join("t.txt"), "k").unwrap();
    // A directory needs the recursive option, and is left intact without it.
    assert!(delete(&s(&t), false).is_err());
    assert!(t.join("a/b/z.txt").exists());
    let r = delete(&s(&t), true).unwrap();
    assert_eq!(r.deleted, 6);
    assert!(!t.exists());
    assert!(sibling.join("keep.txt").exists() && tmp.path().join("t.txt").exists());
    assert!(delete(&s(&t), true).unwrap_err().to_string().contains("does not exist"));
}

#[cfg(unix)]
#[test]
fn recursive_delete_does_not_follow_links() {
    let (tmp, t) = tree();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precious.txt"), "p").unwrap();
    std::os::unix::fs::symlink(&outside, t.join("a/link")).unwrap();
    std::os::unix::fs::symlink(outside.join("precious.txt"), t.join("file_link")).unwrap();
    // Listing shows the links without descending into them.
    let all = names(&list(&s(&t), &ListOptions { recursive: true, ..Default::default() }).unwrap());
    assert!(all.contains(&"a/link".to_string()) && !all.iter().any(|n| n.contains("precious")), "{all:?}");
    delete(&s(&t), true).unwrap();
    assert!(!t.exists());
    assert_eq!(fs::read_to_string(outside.join("precious.txt")).unwrap(), "p");
    // A link given as the path: the link is deleted, not its target.
    std::os::unix::fs::symlink(&outside, tmp.path().join("l")).unwrap();
    delete(&s(&tmp.path().join("l")), true).unwrap();
    assert!(outside.join("precious.txt").exists());
}

#[test]
fn refuses_dangerous_targets() {
    for p in ["", "  ", "/", ".", "..", "/vsimem", "/vsimem/", "s3://bucket", "/vsis3/bucket/", "/vsizip/a.zip/x"] {
        let r = Node::parse(p).and_then(|n| check_deletable(p, &n, "delete"));
        assert!(r.is_err(), "'{p}' was accepted");
    }
    // The current directory and its parents, given absolutely.
    let cwd = std::env::current_dir().unwrap();
    for p in [cwd.clone(), cwd.parent().unwrap().to_path_buf()] {
        let n = Node::Local(p.clone());
        assert!(check_deletable(&s(&p), &n, "delete").is_err(), "{}", p.display());
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = s(Path::new(&home));
        assert!(check_deletable(&home, &Node::parse(&home).unwrap(), "delete").is_err());
    }
    #[cfg(remote)]
    {
        for p in ["https://example.com/a.tif", "/vsicurl/https://example.com/dir", "gs://bucket/key"] {
            assert!(check_deletable(p, &Node::parse(p).unwrap(), "delete").is_err(), "{p}");
        }
        assert!(check_deletable("az://account/container", &Node::parse("az://account/container").unwrap(), "delete").is_err());
        assert!(check_deletable("s3://bucket/key", &Node::parse("s3://bucket/key").unwrap(), "delete").is_ok());
    }
    assert!(delete("", true).is_err());
}

#[test]
fn moves_and_syncs_as_gdal() {
    let (tmp, t) = tree();
    let m1 = tmp.path().join("m1");
    copy(&s(&t), &s(&m1), &CopyOptions { recursive: true, ..Default::default() }).unwrap();
    let m2 = tmp.path().join("m2");
    fs::create_dir(&m2).unwrap();
    rename(&s(&m1), &s(&m2)).unwrap();
    assert!(!m1.exists());
    assert_eq!(files_below(&m2), ["m1/a/b/z.txt", "m1/a/y.txt", "m1/x.txt"]);
    rename(&s(&m2.join("m1/x.txt")), &s(&tmp.path().join("m3.txt"))).unwrap();
    assert!(tmp.path().join("m3.txt").is_file() && !m2.join("m1/x.txt").exists());
    assert!(rename(&s(&m2), &s(&m2.join("m1"))).is_err());
    // Not into itself through a target whose parents do not exist yet (nothing moved or lost).
    assert!(rename(&s(&m2), &s(&m2.join("new/deeper/still"))).is_err());
    assert!(copy(&s(&m2), &s(&m2.join("new/deeper")), &CopyOptions { recursive: true, ..Default::default() }).is_err());
    assert_eq!(files_below(&m2), ["m1/a/b/z.txt", "m1/a/y.txt"]);
    // A rename failing for another reason than crossing file systems is reported, not turned
    // into a copy and a deletion.
    let dir_target = tmp.path().join("occupied");
    fs::create_dir_all(dir_target.join("inner")).unwrap();
    fs::write(dir_target.join("inner/keep"), "k").unwrap();
    assert!(rename(&s(&m2.join("m1")), &s(&dir_target.join("inner/keep"))).is_err());
    assert!(m2.join("m1/a/y.txt").exists() && dir_target.join("inner/keep").exists());

    // Without a trailing slash, the directory itself; with one, its contents.
    let s1 = tmp.path().join("s1");
    let r = sync(&s(&t), &s(&s1), &SyncOptions::default()).unwrap();
    assert_eq!(r.copied, 1);
    assert_eq!(names(&list(&s(&s1), &ListOptions { recursive: true, ..Default::default() }).unwrap()), ["t", "t/a", "t/x.txt"]);
    let s3 = tmp.path().join("s3");
    let r = sync(&format!("{}/", s(&t)), &s(&s3), &SyncOptions { recursive: true, ..Default::default() }).unwrap();
    assert_eq!((r.copied, r.unchanged), (3, 0));
    assert_eq!(files_below(&s3), ["a/b/z.txt", "a/y.txt", "x.txt"]);
    // Again: nothing to copy; with overwrite, everything.
    let r = sync(&format!("{}/", s(&t)), &s(&s3), &SyncOptions { recursive: true, ..Default::default() }).unwrap();
    assert_eq!((r.copied, r.unchanged), (0, 3));
    let r = sync(&format!("{}/", s(&t)), &s(&s3), &SyncOptions { recursive: true, strategy: SyncStrategy::Overwrite }).unwrap();
    assert_eq!(r.copied, 3);
    // A changed size is copied; extra target files are kept.
    fs::write(t.join("a/y.txt"), "abcdef\n").unwrap();
    fs::write(s3.join("extra.txt"), "e").unwrap();
    let r = sync(&format!("{}/", s(&t)), &s(&s3), &SyncOptions { recursive: true, strategy: SyncStrategy::ETag }).unwrap();
    assert_eq!((r.copied, r.unchanged), (1, 2));
    assert_eq!(fs::read_to_string(s3.join("a/y.txt")).unwrap(), "abcdef\n");
    assert!(s3.join("extra.txt").exists());
    assert!("bogus".parse::<SyncStrategy>().is_err());
    assert_eq!("etag".parse::<SyncStrategy>().unwrap(), SyncStrategy::ETag);
}

#[test]
fn memory_files() {
    write("/vsimem/vsi_test/a/b.txt", b"bee").unwrap();
    write("/vsimem/vsi_test/c.txt", b"sea").unwrap();
    let all = list("/vsimem/vsi_test", &ListOptions { recursive: true, ..Default::default() }).unwrap();
    assert_eq!(names(&all), ["a", "a/b.txt", "c.txt"]);
    copy("/vsimem/vsi_test", "/vsimem/vsi_test2", &CopyOptions { recursive: true, ..Default::default() }).unwrap();
    assert_eq!(read("/vsimem/vsi_test2/a/b.txt").unwrap(), b"bee");
    rename("/vsimem/vsi_test2/c.txt", "/vsimem/vsi_test2/d.txt").unwrap();
    assert!(stat("/vsimem/vsi_test2/c.txt").unwrap().is_none());
    assert_eq!(delete("/vsimem/vsi_test2", true).unwrap().deleted, 2);
    assert!(stat("/vsimem/vsi_test2").unwrap().is_none());
    assert!(stat("/vsimem/vsi_test/a").unwrap().unwrap().is_dir());
    delete("/vsimem/vsi_test", true).unwrap();
}

#[test]
fn lists_and_extracts_archives() {
    let (tmp, t) = tree();
    let zip_path = tmp.path().join("t.zip");
    {
        let mut zip = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zip.add_directory("a/", opts).unwrap();
        zip.start_file("x.txt", opts).unwrap();
        zip.write_all(b"hello\n").unwrap();
        // No explicit entry for `a/b/`: implied.
        zip.start_file("a/b/z.txt", opts).unwrap();
        zip.write_all(b"z\n").unwrap();
        zip.finish().unwrap();
    }
    let root = format!("/vsizip/{}", s(&zip_path));
    let all = list(&root, &ListOptions { recursive: true, ..Default::default() }).unwrap();
    assert_eq!(names(&all), ["a", "a/b", "a/b/z.txt", "x.txt"]);
    assert_eq!(all[2].size, Some(2));
    assert_eq!(names(&list(&format!("{root}/a"), &ListOptions::default()).unwrap()), ["b"]);
    assert_eq!(read(&format!("{root}/a/b/z.txt")).unwrap(), b"z\n");
    let out = tmp.path().join("unzipped");
    copy(&root, &s(&out), &CopyOptions { recursive: true, ..Default::default() }).unwrap();
    assert_eq!(files_below(&out), ["a/b/z.txt", "x.txt"]);
    // Read only.
    assert!(delete(&format!("{root}/x.txt"), false).is_err());
    assert!(copy(&s(&t.join("x.txt")), &format!("{root}/new.txt"), &CopyOptions::default()).is_err());
    assert!(zip_path.is_file());

    #[cfg(not(target_family = "wasm"))]
    {
        let tar_path = tmp.path().join("t.tar");
        let mut builder = tar::Builder::new(fs::File::create(&tar_path).unwrap());
        builder.append_dir_all("./", &t).unwrap();
        builder.finish().unwrap();
        drop(builder);
        let root = format!("/vsitar/{}", s(&tar_path));
        let all = list(&root, &ListOptions { recursive: true, ..Default::default() }).unwrap();
        assert_eq!(names(&all), ["a", "a/b", "a/b/z.txt", "a/y.txt", "x.txt"]);
        assert_eq!(read(&format!("{root}/a/y.txt")).unwrap(), b"abc\n");
        let gz_path = tmp.path().join("t.tar.gz");
        let mut gz = flate2::write::GzEncoder::new(fs::File::create(&gz_path).unwrap(), flate2::Compression::fast());
        gz.write_all(&fs::read(&tar_path).unwrap()).unwrap();
        gz.finish().unwrap();
        let root = format!("/vsitar/{}", s(&gz_path));
        assert_eq!(names(&list(&root, &ListOptions { recursive: true, ..Default::default() }).unwrap()).len(), 5);
        assert_eq!(read(&format!("{root}/a/b/z.txt")).unwrap(), b"z\n");
        let extracted = tmp.path().join("extracted-gzip");
        assert_eq!(copy(&root, &s(&extracted), &CopyOptions { recursive: true, ..Default::default() }).unwrap().copied, 3);
        assert_eq!(fs::read(extracted.join("a/b/z.txt")).unwrap(), b"z\n");
    }
}

#[test]
fn parses_locations() {
    assert_eq!(Node::parse("/vsimem//a/./b/").unwrap(), Node::Mem("/vsimem/a/b".into()));
    assert_eq!(
        Node::parse("/vsizip/{d/a.zip}/x/y").unwrap(),
        Node::Archive { archive: Box::new(Node::Local("d/a.zip".into())), kind: ArchiveKind::Zip, inner: "x/y".into() }
    );
    assert!(matches!(Node::parse("/vsitar/a.tgz").unwrap(), Node::Archive { kind: ArchiveKind::TarGz, .. }));
    assert_eq!(Node::parse("/vsis3/b/k/").unwrap(), Node::S3 { bucket: "b".into(), key: "k".into() });
    assert_eq!(Node::parse("https://h.org/d/?sig=1").unwrap(), Node::Url { base: "https://h.org/d".into(), query: "sig=1".into() });
    // A plain zip path is the file itself.
    assert_eq!(Node::parse("a.zip").unwrap(), Node::Local("a.zip".into()));
    assert!(Node::parse("/vsigzip/a.gz").is_err());
    assert_eq!(Node::parse("s3://b").unwrap().join("x").join("y"), Node::S3 { bucket: "b".into(), key: "x/y".into() });
}

#[cfg(unix)]
#[test]
fn portable_moves_refuse_links_and_non_utf8_without_deleting_source() {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let (tmp, source) = tree();
    symlink(source.join("a"), source.join("link")).unwrap();
    assert!(rename(&s(&source), "/vsimem/refused-link-move").is_err());
    assert!(source.join("a/y.txt").exists());
    fs::remove_file(source.join("link")).unwrap();
    let odd = source.join(std::ffi::OsString::from_vec(vec![0xff]));
    fs::write(&odd, "keep").unwrap();
    assert!(list(&s(&source), &ListOptions { recursive: true, ..Default::default() }).is_err());
    assert!(rename(&s(&source), "/vsimem/refused-name-move").is_err());
    assert_eq!(fs::read(odd).unwrap(), b"keep");
    let alias = tmp.path().join("home-link");
    symlink(std::env::current_dir().unwrap(), &alias).unwrap();
    delete(&s(&alias), false).unwrap();
    assert!(std::env::current_dir().unwrap().exists());
}

#[cfg(remote)]
#[test]
fn failing_operation_drops_html_listing_cache() {
    HTML_PAGE.with(|page| *page.borrow_mut() = Some(("https://example.test/stale".into(), Some(Vec::new()))));
    assert!(copy("/vsimem/no-such-source-cache-test", "/vsimem/cache-output", &Default::default()).is_err());
    assert!(HTML_PAGE.with(|page| page.borrow().is_none()));
}

#[cfg(unix)]
#[test]
fn atomic_copy_follows_destination_links_keeps_modes_and_handles_long_names() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("input");
    fs::write(&input, "new").unwrap();
    let output = tmp.path().join("output");
    fs::write(&output, "old").unwrap();
    fs::set_permissions(&output, fs::Permissions::from_mode(0o640)).unwrap();
    let link = tmp.path().join("link");
    symlink(&output, &link).unwrap();
    copy(&s(&input), &s(&link), &Default::default()).unwrap();
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(&output).unwrap(), b"new");
    assert_eq!(fs::metadata(output).unwrap().permissions().mode() & 0o777, 0o640);
    let long = tmp.path().join("x".repeat(250));
    copy(&s(&input), &s(&long), &Default::default()).unwrap();
    assert_eq!(fs::read(long).unwrap(), b"new");
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn tar_tree_extraction_reads_the_archive_once() {
    use std::io::{Cursor, Read};
    struct Counted {
        inner: Cursor<Vec<u8>>,
        bytes: usize,
    }
    impl Read for Counted {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(out)?;
            self.bytes += n;
            Ok(n)
        }
    }
    let mut builder = tar::Builder::new(Vec::new());
    for i in 0..32 {
        let mut header = tar::Header::new_gnu();
        header.set_size(1024);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, format!("files/{i}.bin"), &[i as u8; 1024][..]).unwrap();
    }
    let bytes = builder.into_inner().unwrap();
    let len = bytes.len();
    let mut reader = Counted { inner: Cursor::new(bytes), bytes: 0 };
    let tmp = tempfile::tempdir().unwrap();
    let mut report = Report::default();
    copy_tar_reader(&mut reader, "files", &Node::Local(tmp.path().join("out")), false, None, &mut report).unwrap();
    assert_eq!(report.copied, 32);
    assert_eq!(report.bytes, 32 * 1024);
    assert!(reader.bytes <= len);
    assert_eq!(fs::read(tmp.path().join("out/31.bin")).unwrap(), vec![31; 1024]);
    reader.inner.set_position(0);
    reader.bytes = 0;
    let mut unchanged = Report::default();
    copy_tar_reader(
        &mut reader,
        "files",
        &Node::Local(tmp.path().join("out")),
        false,
        Some(&SyncOptions { recursive: true, ..Default::default() }),
        &mut unchanged,
    )
    .unwrap();
    assert_eq!(unchanged.copied, 0);
    assert_eq!(unchanged.unchanged, 32);
    assert!(reader.bytes <= reader.inner.get_ref().len());
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn tar_copy_reports_links_and_syncs_untimed_memory_by_content() {
    let mut builder = tar::Builder::new(Vec::new());
    let mut file = tar::Header::new_gnu();
    file.set_size(4);
    file.set_mode(0o644);
    file.set_mtime(1);
    file.set_cksum();
    builder.append_data(&mut file, "file.txt", &b"data"[..]).unwrap();
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    link.set_mode(0o777);
    link.set_mtime(1);
    builder.append_link(&mut link, "link.txt", "file.txt").unwrap();
    let bytes = builder.into_inner().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let target = Node::Local(tmp.path().join("out"));
    let mut report = Report::default();
    let error = copy_tar_reader(&mut &bytes[..], "", &target, false, None, &mut report).unwrap_err();
    assert!(error.to_string().contains("tar links"));
    let mut report = Report::default();
    copy_tar_reader(&mut &bytes[..], "", &target, true, None, &mut report).unwrap();
    assert_eq!(report.copied, 1);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].0, "link.txt");
    assert!(!tmp.path().join("out/link.txt").exists());

    let memory = Node::Mem(format!("/vsimem/tar-sync-{}", tmp.path().file_name().unwrap().to_string_lossy()));
    let options = SyncOptions { recursive: true, ..Default::default() };
    for (copied, unchanged) in [(1, 0), (0, 1)] {
        let mut report = Report::default();
        copy_tar_reader(&mut &bytes[..], "", &memory, true, Some(&options), &mut report).unwrap();
        assert_eq!((report.copied, report.unchanged), (copied, unchanged));
        assert_eq!(report.errors.len(), 1);
    }
    let Node::Mem(key) = memory.join("file.txt") else { unreachable!() };
    crate::memfs::write(Path::new(&key), b"edit").unwrap();
    let mut report = Report::default();
    copy_tar_reader(&mut &bytes[..], "", &memory, true, Some(&options), &mut report).unwrap();
    assert_eq!((report.copied, report.unchanged), (1, 0));
    assert_eq!(crate::memfs::read(Path::new(&key)).unwrap(), b"data");
    let Node::Mem(root) = memory else { unreachable!() };
    delete(&root, true).unwrap();
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn tar_member_metadata_stops_early_and_plain_reads_seek_over_payloads() {
    use std::io::{Cursor, Seek, SeekFrom};
    struct Counted {
        inner: Cursor<Vec<u8>>,
        bytes: usize,
    }
    impl Read for Counted {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(out)?;
            self.bytes += n;
            Ok(n)
        }
    }
    impl Seek for Counted {
        fn seek(&mut self, offset: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(offset)
        }
    }
    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes) in [("first.txt", vec![1; 4]), ("large.bin", vec![2; 1024 * 1024]), ("last.txt", vec![3; 4])] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, &bytes[..]).unwrap();
    }
    let mut reader = Counted { inner: Cursor::new(builder.into_inner().unwrap()), bytes: 0 };
    {
        let mut archive = tar::Archive::new(&mut reader);
        let stat = tar_stat_entries(archive.entries().unwrap(), "first.txt").unwrap().unwrap();
        assert_eq!(stat.size, Some(4));
    }
    assert_eq!(reader.bytes, 512, "metadata must stop before reading the first member's payload");
    reader.inner.set_position(0);
    reader.bytes = 0;
    let mut out = Vec::new();
    {
        let mut archive = tar::Archive::new(&mut reader);
        copy_tar_entries(archive.entries_with_seek().unwrap(), "last.txt", &mut out).unwrap();
    }
    assert_eq!(out, [3; 4]);
    assert!(reader.bytes < 4096, "plain tar member reads must seek over earlier payloads");
}

#[test]
fn missing_copy_destination_directory_is_named() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source.txt");
    fs::write(&source, b"source").unwrap();
    let target = format!("{}/missing-output-directory/", tmp.path().display());
    let error = copy(&s(&source), &target, &CopyOptions::default()).unwrap_err();
    assert!(error.to_string().contains("missing-output-directory"));
    assert_eq!(fs::read(&source).unwrap(), b"source");
}

#[test]
fn archive_copy_refuses_parent_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("unsafe.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    zip.start_file("../escape.txt", zip::write::SimpleFileOptions::default()).unwrap();
    zip.write_all(b"do not write outside").unwrap();
    zip.finish().unwrap();
    let source = format!("/vsizip/{}", s(&path));
    let result = copy(&source, &s(&tmp.path().join("out")), &CopyOptions { recursive: true, ..Default::default() });
    assert!(result.is_err());
    assert!(!tmp.path().join("escape.txt").exists());
}

#[cfg(unix)]
#[test]
fn archive_extraction_does_not_follow_links_outside_output() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("safe.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    zip.start_file("nested/file.txt", zip::write::SimpleFileOptions::default()).unwrap();
    zip.write_all(b"stay inside").unwrap();
    zip.finish().unwrap();
    let root = tmp.path().join("out");
    let outside = tmp.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(root.join("safe.zip")).unwrap();
    symlink(&outside, root.join("safe.zip/nested")).unwrap();
    let result = copy(&format!("/vsizip/{}", s(&path)), &s(&root), &CopyOptions { recursive: true, ..Default::default() });
    assert!(result.is_err());
    assert!(!outside.join("file.txt").exists());
}

#[test]
fn move_replaces_an_existing_file_as_gdal_does() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let target = tmp.path().join("target");
    fs::write(&source, "new").unwrap();
    fs::write(&target, "old").unwrap();
    rename(&s(&source), &s(&target)).unwrap();
    assert!(!source.exists());
    assert_eq!(fs::read(target).unwrap(), b"new");
}

#[test]
fn object_names_encode_url_delimiters_and_windows_traversal_is_refused() {
    let node = Node::Url { base: "https://storage.googleapis.com/bucket".into(), query: String::new() };
    assert_eq!(
        node.join("x#y?a% b.tif"),
        Node::Url { base: "https://storage.googleapis.com/bucket/x%23y%3Fa%25%20b.tif".into(), query: String::new() }
    );
    let html = Node::Url { base: "https://example.com/files".into(), query: String::new() };
    assert_eq!(html.join("a%20b.tif"), Node::Url { base: "https://example.com/files/a%20b.tif".into(), query: String::new() });
    for name in ["..\\escape", "nested/..\\escape", "C:escape", "\\absolute"] {
        assert!(unsafe_relative(name));
    }
}

#[cfg(unix)]
#[test]
fn copying_directory_links_reports_an_error() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let outside = tmp.path().join("outside");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("data.txt"), "preserve").unwrap();
    symlink(&outside, source.join("link")).unwrap();
    let out = tmp.path().join("out");
    assert!(copy(&s(&source), &s(&out), &CopyOptions { recursive: true, ..Default::default() }).is_err());
    let result = copy(&s(&source), &s(&tmp.path().join("skip")), &CopyOptions { recursive: true, skip_errors: true }).unwrap();
    assert_eq!(result.errors.len(), 1);
}

#[cfg(unix)]
#[test]
fn deleting_a_directory_symlink_with_a_trailing_slash_preserves_its_target() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep.txt"), "keep").unwrap();
    for suffix in ["/", "/."] {
        let link = tmp.path().join("link");
        symlink(&target, &link).unwrap();
        delete(&(s(&link) + suffix), true).unwrap();
        assert!(!link.exists());
        assert_eq!(fs::read(target.join("keep.txt")).unwrap(), b"keep");
    }
}

#[test]
fn credential_queries_are_redacted_in_refusals_and_public_names() {
    let path = "https://account.blob.core.windows.net/container?sig=test-signature";
    let error = delete(path, true).unwrap_err().to_string();
    assert!(!error.contains("test-signature"));
    assert!(error.contains("redacted"));
    let node = Node::parse(path).unwrap();
    assert_eq!(absolute_name(path, &node, "item.tif"), "https://account.blob.core.windows.net/container/item.tif?<redacted>");
    for url in
        ["https://storage.googleapis.com/bucket", "https://account.blob.core.windows.net/container", "https://bucket.s3.amazonaws.com/"]
    {
        assert!(listing::Api::for_url(url).is_root());
    }
}

#[test]
fn opaque_entity_tags_use_timestamp_without_hashing_local_data() {
    let mut source = super::Stat::file(4).entry("local".into());
    source.modified = Some(1);
    let mut target = super::Stat::file(4).entry("remote".into());
    target.modified = Some(2);
    target.etag = Some("0x8DAZURETAG".into());
    assert!(
        super::up_to_date(
            &super::Node::Mem("missing".into()),
            &source,
            &super::Node::Mem("also-missing".into()),
            &target,
            super::SyncStrategy::ETag
        )
        .unwrap()
    );
}

#[test]
fn local_path_edge_spaces_are_not_removed_before_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("report.txt");
    let spaced = tmp.path().join("report.txt ");
    fs::write(&plain, b"keep").unwrap();
    fs::write(&spaced, b"remove").unwrap();
    delete(&s(&spaced), false).unwrap();
    assert!(!spaced.exists());
    assert_eq!(fs::read(plain).unwrap(), b"keep");
    assert!(matches!(Node::parse(" leading.txt").unwrap(), Node::Local(p) if p == Path::new(" leading.txt")));
    assert!(Node::parse("  ").is_err());
}

#[test]
fn differing_multipart_tags_override_older_timestamps() {
    let mut source = Stat::file(4).entry("source".into());
    source.modified = Some(1);
    source.etag = Some("first-2".into());
    let mut target = Stat::file(4).entry("target".into());
    target.modified = Some(2);
    target.etag = Some("second-2".into());
    let node = Node::Mem("not-read".into());
    assert!(!up_to_date(&node, &source, &node, &target, SyncStrategy::ETag).unwrap());
    target.etag = source.etag.clone();
    assert!(up_to_date(&node, &source, &node, &target, SyncStrategy::ETag).unwrap());
}

#[test]
fn atomic_directory_move_does_not_report_inode_bytes_as_copied_data() {
    let (tmp, source) = tree();
    let target = tmp.path().join("moved");
    let report = rename(&s(&source), &s(&target)).unwrap();
    assert_eq!((report.copied, report.bytes), (0, 0));
    assert!(!source.exists());
    assert_eq!(files_below(&target), ["a/b/z.txt", "a/y.txt", "x.txt"]);
}

#[test]
fn repeated_memory_sync_skips_unchanged_content_and_detects_same_size_edits() {
    let source = "/vsimem/sync-clockless-source";
    let target = "/vsimem/sync-clockless-target";
    write(source, b"first").unwrap();
    assert_eq!(sync(source, target, &Default::default()).unwrap().copied, 1);
    assert_eq!(sync(source, target, &Default::default()).unwrap().unchanged, 1);
    write(source, b"other").unwrap();
    assert_eq!(sync(source, target, &Default::default()).unwrap().copied, 1);
    assert_eq!(read(target).unwrap(), b"other");
    delete(source, false).unwrap();
    delete(target, false).unwrap();
}
