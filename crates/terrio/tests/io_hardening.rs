// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Regression tests for I/O, network URL validation, archive limits, and scientific-driver bounds.

use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write;

#[test]
fn s3_bucket_validation_rejects_url_delimiters() {
    for bad in [
        "s3://localhost:8080?/key.tif",
        "s3://attacker.example?/key.tif",
        "s3://bucket#frag/key.tif",
        "s3://user@host/key.tif",
        "s3://../escape/key.tif",
        "s3:///empty-bucket.tif",
        "/vsis3/localhost:8080?/key.tif",
    ] {
        assert!(terrio::vsi::stat(bad).is_err(), "{bad} must be rejected by VSI");
        assert!(matches!(terrio::vfs::Location::parse(bad), terrio::vfs::Location::Local(_)), "{bad} must not parse as Location::S3");
    }
    assert!(matches!(
        terrio::vfs::Location::parse("s3://my-valid.bucket_1/dir/file.tif"),
        terrio::vfs::Location::S3 { ref bucket, ref key } if bucket == "my-valid.bucket_1" && key == "dir/file.tif"
    ));
}

#[test]
fn zarr_v2_multibyte_dtype_and_invalid_separator_return_errors_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let bad_dtype = dir.path().join("bad_dtype.zarr");
    std::fs::create_dir_all(&bad_dtype).unwrap();
    std::fs::write(
        bad_dtype.join(".zarray"),
        r#"{"zarr_format":2,"shape":[2,2],"chunks":[2,2],"dtype":"é","compressor":null,"fill_value":0,"order":"C","filters":null}"#,
    )
    .unwrap();
    assert!(terrio::open_raster(bad_dtype.to_str().unwrap()).is_err());

    let bad_sep = dir.path().join("bad_sep.zarr");
    std::fs::create_dir_all(&bad_sep).unwrap();
    std::fs::write(
        bad_sep.join(".zarray"),
        r#"{"zarr_format":2,"shape":[2,2],"chunks":[2,2],"dtype":"<u1","dimension_separator":"..\\..\\","compressor":null,"fill_value":0,"order":"C","filters":null}"#,
    )
    .unwrap();
    // terrio's reader refuses the separator; with the GDAL backend, the read falls back to GDAL's
    // Zarr driver, which may open the array itself: only the absence of a panic is checked then.
    let opened = terrio::open_raster(bad_sep.to_str().unwrap());
    #[cfg(not(feature = "gdal"))]
    assert!(opened.is_err());
    #[cfg(feature = "gdal")]
    drop(opened);
}

#[test]
fn tar_gz_decompression_enforces_zip_max_size() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("large.tar.gz");
    // Build a small compressed .tar.gz whose uncompressed tar stream is ~2 MiB of zeros.
    let mut gz = GzEncoder::new(std::fs::File::create(&archive).unwrap(), Compression::fast());
    {
        let mut tar = tar::Builder::new(&mut gz);
        let payload = vec![0u8; 2 * 1024 * 1024];
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "skip.bin", payload.as_slice()).unwrap();

        let small = b"hello";
        let mut h2 = tar::Header::new_gnu();
        h2.set_size(small.len() as u64);
        h2.set_mode(0o644);
        h2.set_cksum();
        tar.append_data(&mut h2, "target.txt", small.as_slice()).unwrap();
        tar.finish().unwrap();
    }
    gz.finish().unwrap().flush().unwrap();

    let vpath_root = format!("/vsitar/{}", archive.display());
    let vpath_member = format!("/vsitar/{}/target.txt", archive.display());

    // Enforce a 64 KiB cap in this thread's scoped config: list, stat, and read must all fail with ResourceLimit.
    struct ScopeGuard;
    impl Drop for ScopeGuard {
        fn drop(&mut self) {
            terrio::config::pop_scope();
        }
    }
    terrio::config::push_scope(std::collections::HashMap::from([("TERRIO_ZIP_MAX_SIZE".to_string(), Some("65536".to_string()))]));
    let _guard = ScopeGuard;
    assert!(matches!(terrio::vsi::list(&vpath_root, &Default::default()), Err(terrio::Error::ResourceLimit(_))));
    assert!(matches!(terrio::vsi::stat(&vpath_member), Err(terrio::Error::ResourceLimit(_))));
    assert!(matches!(terrio::vsi::read(&vpath_member), Err(terrio::Error::ResourceLimit(_))));
}

/// Tar framing (a 512-byte header per member, payload padding) is allowed beyond
/// `TERRIO_ZIP_MAX_SIZE` for the entries read: an archive of many small members within the entry
/// and payload limits reads as `.tar.gz` as it does as `.tar`.
#[test]
fn tar_gz_framing_of_many_small_members_is_within_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("many.tar.gz");
    let mut gz = GzEncoder::new(std::fs::File::create(&archive).unwrap(), Compression::fast());
    {
        let mut tar = tar::Builder::new(&mut gz);
        for i in 0..1000 {
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("m{i:04}.txt"), &b"x"[..]).unwrap();
        }
        tar.finish().unwrap();
    }
    gz.finish().unwrap().flush().unwrap();

    struct ScopeGuard;
    impl Drop for ScopeGuard {
        fn drop(&mut self) {
            terrio::config::pop_scope();
        }
    }
    // 1000 bytes of members, about 1 MiB of tar stream.
    terrio::config::push_scope(std::collections::HashMap::from([("TERRIO_ZIP_MAX_SIZE".to_string(), Some("65536".to_string()))]));
    let _guard = ScopeGuard;
    let root = format!("/vsitar/{}", archive.display());
    let listed = terrio::vsi::list(&root, &Default::default()).unwrap();
    assert_eq!(listed.len(), 1000);
    assert_eq!(terrio::vsi::read(&format!("{root}/m0999.txt")).unwrap(), b"x");
}
