//! The process-wide raster allow-list and SRTM tiles, in a binary of its own because a list once
//! set cannot be lifted: a tile named for another driver (`N37W120.img`, HFA's) is refused under a
//! list without SRTMHGT however it arrives: on disk, as bytes, or as a member of an archive, whose
//! extracted copy terrio's readers recognise as a tile by its name and size.

use std::io::Write;
use terrio::formats::{Kind, set_allowed_drivers};

/// A 3" tile (1201 x 1201 Int16 samples), all zero.
fn tile() -> Vec<u8> {
    vec![0u8; 1201 * 1201 * 2]
}

fn refused<T>(result: terrio::Result<T>) -> String {
    match result {
        Ok(_) => panic!("opened"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn a_tile_named_for_another_driver_is_refused_without_srtmhgt() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("N37W120.img");
    std::fs::write(&img, tile()).unwrap();
    let zip = dir.path().join("tiles.zip");
    let mut w = zip::ZipWriter::new(std::fs::File::create(&zip).unwrap());
    w.start_file("N37W120.img", zip::write::SimpleFileOptions::default()).unwrap();
    w.write_all(&tile()).unwrap();
    w.finish().unwrap();
    let member = format!("/vsizip/{}/N37W120.img", zip.display());
    // Without a list, the tile opens from either place.
    assert_eq!(terrio::open_raster(img.to_str().unwrap()).unwrap().driver.name(), "SRTMHGT");
    assert_eq!(terrio::open_raster(&member).unwrap().driver.name(), "SRTMHGT");

    set_allowed_drivers(Some(&["HFA", "GTiff"]), Some(Kind::Raster)).unwrap();
    for message in [
        refused(terrio::open_raster(img.to_str().unwrap())),
        refused(terrio::memory::open_raster_bytes(tile(), "N37W120.img")),
        refused(terrio::open_raster(&member)),
    ] {
        assert!(message.contains("SRTMHGT") && message.contains("not allowed"), "{message}");
    }
}
