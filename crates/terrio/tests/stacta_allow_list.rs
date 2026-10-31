//! STACTA, a driver added by #218, behind the process-wide raster allow-list and the
//! `driver=` check: an item of tiled assets is a pointer format (its tiles are other files,
//! remote ones too), which opens only when its driver is named. A test binary of its own, as a
//! list once set cannot be lifted.

mod stacta_common;

use serde_json::json;
use stacta_common::{item, write_item, write_tiles};
use terrio::formats::{Kind, check_drivers, set_allowed_drivers};

fn refused<T>(result: &terrio::Result<T>) -> bool {
    result.as_ref().err().is_some_and(|e| e.to_string().contains("not allowed"))
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn stacta_opens_only_when_its_driver_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    write_tiles(dir.path(), "", &[(0, 0, 0), (1, 0, 0), (1, 0, 1), (1, 1, 0), (1, 1, 1)]);
    let json = write_item(dir.path(), "item.json", &item(None, json!({"data": {"href": "./{TileMatrix}/{TileRow}/{TileCol}.tif"}})));
    let named = format!("STACTA:\"{json}\":data");

    // `driver=` (rasterio's, through `check_drivers`): the item's content reads as STAC, and a
    // `STACTA:` name names its driver, so neither passes as GTiff, nor the name as STAC or
    // GeoJSON (the extension of the file it names).
    assert!(check_drivers(&json, &names(&["GTiff"])).is_err());
    let bare = format!("STACTA:{json}");
    for name in [&named, &bare] {
        assert!(check_drivers(name, &names(&["GTiff", "STAC", "GeoJSON"])).is_err(), "a STACTA: name is STACTA's: {name}");
        assert!(check_drivers(name, &names(&["STACTA"])).is_ok(), "{name}");
    }

    // No list: the item opens, by its path and by its name.
    assert!(terrio::open_raster_lazy(&json).is_ok());
    assert!(terrio::open_raster_lazy(&named).is_ok());

    // A list that allows what the item's file looks like (STAC content, a `.json` name) but not
    // STACTA refuses it, by its path and by its name, before any tile is opened.
    set_allowed_drivers(Some(&["GTiff", "COG", "STAC", "GeoJSON"]), Some(Kind::Raster)).unwrap();
    assert!(refused(&terrio::open_raster_lazy(&json)), "{:?}", terrio::open_raster_lazy(&json).err());
    assert!(refused(&terrio::open_raster(&json)));
    assert!(refused(&terrio::open_raster_lazy(&named)));
    assert!(refused(&terrio::open_raster(&named)));
}
