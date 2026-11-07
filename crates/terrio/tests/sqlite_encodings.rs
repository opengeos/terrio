//! GeoPackages in each SQLite text encoding, written by Python's sqlite3 module (SQLite is an
//! independent oracle for the file format), read whole, through the R-tree and streamed. Text
//! that is not valid Unicode reads as U+FFFD under the display reader policy; malformed
//! records are errors.
#![cfg(feature = "sqlite")]
use terrio::{FieldValue, VectorOpenOptions};

const LAYER: &str = "couches_é水";
const TEXT: &str = "é水😀";

#[path = "data/sqlite_encodings/fixtures.rs"]
mod fixtures;

fn features(path: &str, bbox: Option<(f64, f64, f64, f64)>) -> terrio::Result<(terrio::Layer, terrio::Layer)> {
    let options = VectorOpenOptions { spatial_filter: bbox, ..Default::default() };
    let eager = terrio::open_vector(path, &options)?.layers.remove(0);
    let streamed = terrio::stream::open_layers(path, &options, 1)?.remove(0).read_all()?;
    Ok((eager, streamed))
}

#[test]
fn reads_text_in_each_database_encoding() {
    let dir = tempfile::tempdir().unwrap();
    for (name, bytes) in fixtures::DATABASES {
        std::fs::write(dir.path().join(format!("{name}.gpkg")), bytes).unwrap();
    }
    let path = |name: &str| dir.path().join(format!("{name}.gpkg")).to_str().unwrap().to_string();
    let check = |name: &str, second: &str| {
        for (bbox, fids) in [(None, vec![1, 2, 3]), (Some((5.0, 5.0, 15.0, 15.0)), vec![2])] {
            let (eager, streamed) = features(&path(name), bbox).unwrap();
            assert_eq!(eager.name, LAYER, "{name}");
            assert_eq!(eager.crs_epsg(), Some(4326), "{name}");
            assert_eq!(streamed.crs_epsg(), Some(4326), "{name}");
            assert_eq!(eager.features.iter().map(|f| f.fid).collect::<Vec<_>>(), fids, "{name}");
            for f in &eager.features {
                assert!(f.geometry.is_some(), "{name}");
                let text = if f.fid == 2 { second } else { TEXT };
                assert_eq!(f.attributes, [FieldValue::Text(text.into())], "{name}");
            }
            assert_eq!(streamed.len(), eager.len(), "{name}");
            for (a, b) in streamed.features.iter().zip(&eager.features) {
                assert_eq!((a.fid, &a.geometry, &a.attributes), (b.fid, &b.geometry, &b.attributes), "{name}");
            }
        }
    };
    for encoding in ["UTF-8", "UTF-16le", "UTF-16be"] {
        check(encoding, "TRUNCATE_ME");
        check(&format!("{encoding}-invalid"), "\u{fffd}");
        let truncated = path(&format!("{encoding}-truncated"));
        for bbox in [None, Some((5.0, 5.0, 15.0, 15.0))] {
            let options = VectorOpenOptions { spatial_filter: bbox, ..Default::default() };
            assert!(terrio::open_vector(&truncated, &options).is_err(), "{encoding}");
            assert!(terrio::stream::open_layers(&truncated, &options, 1).and_then(|mut r| r.remove(0).read_all()).is_err(), "{encoding}");
        }
    }
    check("UTF-8-encoding0", "TRUNCATE_ME");
}
