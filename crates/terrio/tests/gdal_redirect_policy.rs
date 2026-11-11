#![cfg(feature = "gdal")]

#[test]
fn restrictive_redirect_policy_blocks_explicit_and_fallback_gdal_opens() {
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            terrio::config::pop_scope();
        }
    }
    terrio::config::push_scope([("TERRIO_HTTP_FOLLOW_CROSS_ORIGIN".into(), Some("NO".into()))].into());
    let _scope = Scope;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unsupported.bin");
    std::fs::write(&path, b"not a supported raster").unwrap();
    for path in ["GDAL:missing.tif", path.to_str().unwrap()] {
        let result = terrio::open_raster(path);
        assert!(matches!(result, Err(terrio::Error::InvalidArgument(ref message)) if message.contains("TERRIO_HTTP_FOLLOW_CROSS_ORIGIN")));
    }
}
