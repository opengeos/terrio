//! Public identification behavior in builds without HTTP/cloud support.
#[cfg(all(not(feature = "remote"), not(target_family = "wasm")))]
#[test]
fn disabled_remote_support_reports_the_feature_requirement() {
    for path in ["https://example.invalid/data.tif", "s3://bucket/data.tif"] {
        let error = terrio::inspection::identify(path, false).unwrap_err();
        assert!(error.to_string().contains("remote"));
    }
}
