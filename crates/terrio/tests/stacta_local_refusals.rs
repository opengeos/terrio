//! Local STACTA containers retain their metadata fallback for unavailable tile drivers.
mod stacta_common;

use serde_json::json;
use stacta_common::{item, write_item, write_tiles};
use terrio::formats::{Kind, set_allowed_drivers};

#[test]
fn local_tile_driver_refusals_keep_container_metadata() {
    let dir = tempfile::tempdir().unwrap();
    write_tiles(dir.path(), "", &[(0, 0, 0), (1, 0, 0), (1, 0, 1), (1, 1, 0), (1, 1, 1)]);
    let path = write_item(
        dir.path(),
        "item.json",
        &item(
            None,
            json!({
                "data": {"href": "./{TileMatrix}/{TileRow}/{TileCol}.tif"},
                "other": {"href": "./{TileMatrix}/{TileRow}/{TileCol}.tif"}
            }),
        ),
    );
    set_allowed_drivers(Some(&["STACTA", "STAC", "GeoJSON"]), Some(Kind::Raster)).unwrap();
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            terrio::config::pop_scope();
        }
    }
    for skip in ["NO", "YES"] {
        terrio::config::push_scope(std::collections::HashMap::from([("GDAL_STACTA_SKIP_MISSING_METATILE".into(), Some(skip.into()))]));
        let _scope = Pop;
        let info = terrio::info::info_path(&path, &Default::default()).unwrap();
        let terrio::info::Info::Raster(info) = info else { panic!("STACTA container must report raster metadata") };
        assert_eq!(info.band_count, 0);
        assert_eq!(info.subdatasets.len(), 2);
    }
}
