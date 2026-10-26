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
    assert_eq!(civil_seconds(2024, 1, 1, 1, 42, 27), 1_704_073_347);
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
        let r = Node::parse(p).and_then(|n| check_deletable(p, &n));
        assert!(r.is_err(), "'{p}' was accepted");
    }
    // The current directory and its parents, given absolutely.
    let cwd = std::env::current_dir().unwrap();
    for p in [cwd.clone(), cwd.parent().unwrap().to_path_buf()] {
        let n = Node::Local(p.clone());
        assert!(check_deletable(&s(&p), &n).is_err(), "{}", p.display());
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = s(Path::new(&home));
        assert!(check_deletable(&home, &Node::parse(&home).unwrap()).is_err());
    }
    #[cfg(remote)]
    {
        for p in ["https://example.com/a.tif", "/vsicurl/https://example.com/dir", "gs://bucket/key"] {
            assert!(check_deletable(p, &Node::parse(p).unwrap()).is_err(), "{p}");
        }
        assert!(check_deletable("az://account/container", &Node::parse("az://account/container").unwrap()).is_err());
        assert!(check_deletable("s3://bucket/key", &Node::parse("s3://bucket/key").unwrap()).is_ok());
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
