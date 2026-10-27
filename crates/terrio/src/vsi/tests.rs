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
    assert_eq!(x.long_line().split_whitespace().nth(4), Some("6"));
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
    let undated = Entry { modified: None, ..e };
    assert_eq!(undated.long_line(), format!("d--------- 1 unknown unknown           80 {:<16} a", "-"));
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
    // APFS/HFS+ may reject these names; keep other Unix checks unconditional.
    if fs::write(&odd, "keep").is_ok() {
        assert!(list(&s(&source), &ListOptions { recursive: true, ..Default::default() }).is_err());
        assert!(rename(&s(&source), "/vsimem/refused-name-move").is_err());
        assert_eq!(fs::read(odd).unwrap(), b"keep");
    }
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

#[test]
fn memory_root_cannot_be_replaced_by_a_file() {
    for path in ["/vsimem", "/vsimem/", "/vsimem//./"] {
        assert!(write(path, b"not a root file").is_err());
        assert!(stat(path).unwrap().unwrap().is_dir());
    }
}

#[test]
fn archive_urls_keep_queries_out_of_member_paths() {
    for prefix in ["/vsizip/", "/vsitar/"] {
        let ext = if prefix == "/vsizip/" { "zip" } else { "tar.gz" };
        let url = format!("https://example.com/data.{ext}?sig=example/test&token=value");
        for (path, expected_inner) in
            [(format!("{prefix}{{/vsicurl/{url}}}/member.tif"), "member.tif"), (format!("{prefix}/vsicurl/{url}"), "")]
        {
            let Node::Archive { archive, inner, .. } = Node::parse(&path).unwrap() else { panic!("not an archive") };
            assert_eq!(
                *archive,
                Node::Url { base: format!("https://example.com/data.{ext}"), query: "sig=example/test&token=value".into() }
            );
            assert_eq!(inner, expected_inner);
            assert!(!display_node(&archive).contains("example/test"));
        }
    }
}

#[cfg(unix)]
#[test]
fn local_copy_and_sync_preserve_unix_colons_and_backslashes() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    fs::create_dir(&source).unwrap();
    for name in ["a:b.txt", r"a\..\b.txt"] {
        fs::write(source.join(name), name).unwrap();
    }
    for sync_mode in [false, true] {
        let dst = tmp.path().join(if sync_mode { "sync" } else { "copy" });
        if sync_mode {
            sync(&format!("{}/", s(&source)), &s(&dst), &SyncOptions { recursive: true, ..Default::default() }).unwrap();
        } else {
            copy(&s(&source), &s(&dst), &CopyOptions { recursive: true, ..Default::default() }).unwrap();
        }
        for name in ["a:b.txt", r"a\..\b.txt"] {
            assert_eq!(fs::read(dst.join(name)).unwrap(), name.as_bytes());
        }
    }
}

#[cfg(remote)]
#[test]
fn native_cloud_depth_walk_keeps_space_prefixes_raw() {
    use std::io::{BufRead, BufReader};
    for azure in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (i, stream) in listener.incoming().take(2).enumerate() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                requests.push(line.clone());
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let body = match (azure, i) {
                    (false, 0) => "<ListBucketResult><CommonPrefixes><Prefix>root/a b/</Prefix></CommonPrefixes></ListBucketResult>",
                    (false, _) => "<ListBucketResult><Contents><Key>root/a b/x.tif</Key><Size>7</Size></Contents></ListBucketResult>",
                    (true, 0) => "<EnumerationResults><Blobs><BlobPrefix><Name>root/a b/</Name></BlobPrefix></Blobs></EnumerationResults>",
                    (true, _) => {
                        "<EnumerationResults><Blobs><Blob><Name>root/a b/x.tif</Name><Properties><Content-Length>7</Content-Length></Properties></Blob></Blobs></EnumerationResults>"
                    }
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        let api = if azure {
            listing::Api::Azure { base, prefix: "root/".into(), sas: "sig=example".into() }
        } else {
            listing::Api::Url { base, prefix: "root/".into() }
        };
        let entries = remote_read_dir_api(api, 2).unwrap();
        assert_eq!(names(&entries), ["a b", "a b/x.tif"]);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("prefix=root%2Fa%20b%2F"), "{}", requests[1]);
        assert!(!requests[1].contains("%2520"));
        assert!(requests.iter().all(|request| request.contains("delimiter=")));
        if azure {
            assert!(requests.iter().all(|request| request.contains("sig=example")));
        }
    }
}

#[cfg(unix)]
#[test]
fn recursive_delete_keeps_outside_files_during_symlink_replacement() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("keep");
    fs::write(&sentinel, "outside contents").unwrap();
    for index in 0..64 {
        let root = tmp.path().join(format!("root{index}"));
        let child = root.join("child");
        let parked = tmp.path().join(format!("parked{index}"));
        fs::create_dir_all(&child).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let target = outside.clone();
        let writer = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                if fs::rename(&child, &parked).is_ok() {
                    if std::os::unix::fs::symlink(&target, &child).is_ok() {
                        let _ = fs::remove_file(&child);
                    }
                    let _ = fs::rename(&parked, &child);
                }
            }
        });
        // Concurrent mutation can make deletion fail. Neither outcome may delete outside.
        let _ = delete(&s(&root), true);
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "outside contents");
    }
}

#[cfg(remote)]
#[test]
fn nested_http_listing_failure_redacts_query_credentials() {
    use std::io::{BufRead, BufReader};
    let _cache = HtmlCacheScope::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/root", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for (index, stream) in listener.incoming().take(2).enumerate() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            let body = if index == 0 { "<title>Index of /root/</title><a href='sub/'>sub</a>" } else { "<html>ordinary page</html>" };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    let error = remote_read_dir(&Node::Url { base, query: "sig=PRIVATE_TEST_SIGNATURE".into() }, 2).unwrap_err().to_string();
    server.join().unwrap();
    assert!(error.contains("not a listable directory"));
    assert!(error.contains("<redacted>"));
    assert!(!error.contains("PRIVATE_TEST_SIGNATURE"));
}

#[test]
fn html_listing_keeps_query_out_of_subdirectory_urls() {
    use std::io::{BufRead, BufReader};
    let _cache = HtmlCacheScope::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/root", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut targets = Vec::new();
        for stream in listener.incoming().take(2) {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            let target = request.split_whitespace().nth(1).unwrap_or("").to_string();
            let body = if target.starts_with("/root/sub/?") {
                "<title>Index of /root/sub/</title><a href='child.tif'>child.tif</a>"
            } else {
                "<title>Index of /root/</title><a href='sub/'>sub</a>"
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            targets.push(target);
        }
        targets
    });
    let entries = remote_read_dir(&Node::Url { base, query: "sig=TEST_QUERY".into() }, 2).unwrap();
    let targets = server.join().unwrap();
    assert_eq!(targets, ["/root/?sig=TEST_QUERY", "/root/sub/?sig=TEST_QUERY"]);
    assert!(names(&entries).contains(&"sub/child.tif".to_string()), "{:?}", names(&entries));
}

#[test]
fn recursive_copy_preserves_filesystem_error_kind() {
    let (tmp, source) = tree();
    let destination = tmp.path().join("out");
    fs::create_dir_all(destination.join("t/x.txt")).unwrap();
    let error = copy(&s(&source), &s(&destination), &CopyOptions { recursive: true, ..Default::default() }).unwrap_err();
    assert!(matches!(error, Error::Io(_)), "{error:?}");
    assert!(destination.join("t/x.txt").is_dir());
    assert_eq!(fs::read_to_string(source.join("x.txt")).unwrap(), "hello\n");
}

#[cfg(unix)]
#[test]
fn uploads_preserve_colons_and_remote_backslashes() {
    let (tmp, source) = tree();
    fs::write(source.join("a:b.txt"), "colon").unwrap();
    let remote = Node::S3 { bucket: "example".into(), key: "prefix".into() };
    assert!(checked_child_target(&remote, r"literal\..\name.txt", false).is_ok());
    let target = "/vsimem/terrio-vsi-upload-names";
    copy(&s(&source), target, &CopyOptions { recursive: true, ..Default::default() }).unwrap();
    assert_eq!(
        names(&list(target, &ListOptions { recursive: true, ..Default::default() }).unwrap()),
        vec!["a", "a/b", "a/b/z.txt", "a/y.txt", "a:b.txt", "x.txt"]
    );
    delete(target, true).unwrap();
    let missing_parent = source.join("missing/../nested");
    assert!(copy(&s(&source), &s(&missing_parent), &CopyOptions { recursive: true, ..Default::default() }).is_err());
    assert!(rename(&s(&source), &s(&missing_parent)).is_err());
    assert!(source.join("x.txt").exists());
    assert!(!source.join("nested").exists());
    drop(tmp);
}

#[test]
fn memory_paths_refuse_parent_segments() {
    for p in ["/vsimem/a/../b", "/vsimem/../b", "/vsimem/a/..", "/vsimem/a\\..\\b", "/vsimem/a/..\\b"] {
        assert!(Node::parse(p).is_err(), "{p}");
    }
    assert!(matches!(Node::parse("/vsimem/a/./b"), Ok(Node::Mem(k)) if k == "/vsimem/a/b"));
}

#[test]
fn mounts_are_out_of_reach_of_vsi() {
    let mount = crate::memfs::Mount::new([("a/file".to_string(), b"mounted".to_vec())]);
    let inside = format!("{}/a/file", mount.path().display());
    // Neither written, copied over, moved, deleted nor listed through `vsi`.
    assert!(write(&inside, b"user").unwrap_err().to_string().contains("reserved"));
    assert!(delete(&mount.path().display().to_string(), true).unwrap_err().to_string().contains("reserved"));
    assert!(read(&inside).is_err());
    assert_eq!(crate::memfs::read(Path::new(&inside)).unwrap(), b"mounted");
    write("/vsimem/vsi-mounts-test/keep", b"user").unwrap();
    let names: Vec<String> =
        list("/vsimem", &ListOptions { recursive: true, ..Default::default() }).unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.iter().any(|n| n == "vsi-mounts-test/keep"), "{names:?}");
    assert!(names.iter().all(|n| !n.starts_with(".terrio-mounts")), "{names:?}");
    // A user file under a name like the mounts' survives a mount being dropped.
    write("/vsimem/terrio/1/user", b"user").unwrap();
    drop(mount);
    assert_eq!(read("/vsimem/terrio/1/user").unwrap(), b"user");
    delete("/vsimem/vsi-mounts-test", true).unwrap();
    delete("/vsimem/terrio", true).unwrap();
}

#[test]
fn recursive_memory_delete_refuses_dot_segments_before_mutation() {
    let root = "/vsimem/terrio-vsi-delete-dot";
    crate::memfs::write(Path::new("/vsimem/terrio-vsi-delete-dot/safe.txt"), b"keep").unwrap();
    crate::memfs::write(Path::new("/vsimem/terrio-vsi-delete-dot/nested/../outside.txt"), b"keep").unwrap();
    assert!(delete(root, true).unwrap_err().to_string().contains("dot segments"));
    assert!(stat(&format!("{root}/safe.txt")).unwrap().is_some());
    crate::memfs::remove_file(Path::new("/vsimem/terrio-vsi-delete-dot/safe.txt"));
    crate::memfs::remove_file(Path::new("/vsimem/terrio-vsi-delete-dot/nested/../outside.txt"));
}

#[cfg(unix)]
#[test]
fn recursive_copy_reports_special_files_without_opening_them() {
    let (tmp, source) = tree();
    let pipe = source.join("pipe");
    assert!(std::process::Command::new("mkfifo").arg(&pipe).status().unwrap().success());
    let target = tmp.path().join("target");
    let report = copy(&s(&source), &s(&target), &CopyOptions { recursive: true, skip_errors: true }).unwrap();
    assert_eq!(report.copied, 3);
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].1.contains("regular files"));
    assert!(!target.join("pipe").exists());
    assert!(check_copy_move_source(&Node::Local(source.clone())).unwrap_err().to_string().contains("regular files"));
    assert!(source.join("x.txt").is_file());
}

#[test]
#[cfg(unix)]
fn writes_and_moves_preserve_special_destinations() {
    use std::os::unix::fs::{FileTypeExt, symlink};
    let tmp = tempfile::tempdir().unwrap();
    let pipe = tmp.path().join("pipe");
    assert!(std::process::Command::new("mkfifo").arg(&pipe).status().unwrap().success());
    let source = tmp.path().join("source");
    fs::write(&source, b"payload").unwrap();
    assert!(copy(&s(&source), &s(&pipe), &CopyOptions::default()).is_err());
    assert!(rename(&s(&source), &s(&pipe)).is_err());
    assert!(fs::metadata(&pipe).unwrap().file_type().is_fifo());
    assert_eq!(fs::read(&source).unwrap(), b"payload");
    let link = tmp.path().join("pipe-link");
    symlink(&pipe, &link).unwrap();
    assert!(copy(&s(&source), &s(&link), &CopyOptions::default()).is_err());
    assert!(fs::metadata(&pipe).unwrap().file_type().is_fifo());
    let socket = tmp.path().join("socket");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    assert!(copy(&s(&source), &s(&socket), &CopyOptions::default()).is_err());
    assert!(rename(&s(&source), &s(&socket)).is_err());
    assert!(fs::metadata(&socket).unwrap().file_type().is_socket());
}
