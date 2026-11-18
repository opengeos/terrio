//! The horizontal axes of a WKT whose root is a `BOUNDCRS` (alone or in a `COMPOUNDCRS`) are
//! those of its source CRS, as PROJ 9.9.0 reads them, for `axis_order` and
//! `has_north_east_axis_order` alike (opengeos/terrio#1012, item 60); the geographic CRS of a WKT
//! 1 is its `GEOGCS` node in `(`/`)` brackets too, and not a name's text (`geographic_wkt`).
//!
//! `data/horizontal_root.json` (`scripts/gen_horizontal_root.py`, pyproj 3.8.0,
//! `PROJ_NETWORK=OFF`) holds PROJ's first two axis directions, and the code of PROJ's
//! `geodetic_crs`.

use serde_json::Value;
use terrio_crs::{AxisOrder, SpatialRef};

#[test]
fn a_bound_crs_has_its_source_axes_and_a_geogcs_is_read_from_the_tree() {
    let data: Value = serde_json::from_str(include_str!("data/horizontal_root.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 5);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        if let Some(axes) = case["axes"].as_array() {
            let north_east = axes.iter().map(|a| a.as_str().unwrap()).eq(["north", "east"]);
            assert_eq!(crs.has_north_east_axis_order(), north_east, "{name}");
            assert_eq!(crs.axis_order(), if north_east { AxisOrder::NorthEast } else { AxisOrder::EastNorth }, "{name}");
        }
        if let Some(code) = case["geographic"].as_u64() {
            // Of no code: read from the tree, not the registry.
            assert_eq!((crs.epsg, crs.esri), (None, None), "{name}");
            let geographic = crs.geographic_wkt().unwrap_or_else(|| panic!("{name}"));
            assert!(geographic.starts_with(r#"GEOGCS["OSGB36","#), "{name}: {geographic}");
            assert_eq!(SpatialRef::from_wkt(&geographic).epsg.map(u64::from), Some(code), "{name}: {geographic}");
        }
    }
}
