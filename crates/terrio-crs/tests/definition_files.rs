// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

use terrio_crs::SpatialRef;

#[test]
fn regular_definition_files_remain_supported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("definition.prj");
    std::fs::write(&path, SpatialRef::from_epsg(4326).unwrap().to_wkt().unwrap()).unwrap();
    assert_eq!(SpatialRef::parse(path.to_str().unwrap()).unwrap().epsg, Some(4326));
    assert!(SpatialRef::parse_literal(path.to_str().unwrap()).is_err());
    assert!(SpatialRef::parse(dir.path().to_str().unwrap()).is_err());
    #[cfg(unix)]
    {
        let link = dir.path().join("linked.prj");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(SpatialRef::parse(link.to_str().unwrap()).unwrap().epsg, Some(4326));
    }
}

/// A subprocess makes the pre-fix blocking open a bounded, deterministic failure.
#[cfg(unix)]
#[test]
fn fifo_definitions_are_refused_without_waiting() {
    const CHILD: &str = "TERRIO_CRS_FIFO_TEST";
    if let Some(path) = std::env::var_os(CHILD) {
        assert!(SpatialRef::parse(path.to_str().unwrap()).is_err());
        return;
    }
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("definition.fifo");
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a valid NUL-terminated path and mkfifo retains no pointers.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fifo_definitions_are_refused_without_waiting", "--nocapture"])
        .env(CHILD, &path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("CRS parsing blocked while opening a FIFO with no writer");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
