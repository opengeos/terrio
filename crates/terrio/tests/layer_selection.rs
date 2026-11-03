//! Reader and streaming selection agree; file-stem aliases are a Terrio extension to
//! GDAL 3.13.3's layer-name lookup.

use terrio::{Layer, VectorOpenOptions};

fn options(names: &[&str]) -> VectorOpenOptions {
    VectorOpenOptions { layers: names.iter().map(|s| s.to_string()).collect(), ..Default::default() }
}

#[test]
fn file_and_stream_readers_validate_every_requested_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut extensions = vec!["geojson", "kml"];
    if cfg!(feature = "sqlite") {
        extensions.push("gpkg");
    }
    if cfg!(feature = "xlsx") {
        extensions.push("xlsx");
    }
    for ext in extensions {
        let path = dir.path().join(format!("map.{ext}"));
        let path = path.to_str().unwrap();
        terrio::write_vector(&[Layer::new("roads")], path, &Default::default()).unwrap();
        for names in [vec![], vec!["roads"], vec!["map"], vec!["map", "roads", "map"]] {
            let ds = terrio::open_vector(path, &options(&names)).unwrap();
            assert_eq!(ds.layers.len(), 1, "{ext}");
            assert_eq!(ds.layers[0].name, "roads", "{ext}");
            let streamed = terrio::stream::open_layers(path, &options(&names), 1).unwrap();
            assert_eq!(streamed.len(), 1, "{ext}");
            assert_eq!(streamed.into_iter().next().unwrap().read_all().unwrap().name, "roads", "{ext}");
        }
        let o = options(&["roads", "missing"]);
        assert_eq!(terrio::open_vector(path, &o).unwrap_err().to_string(), "no layer named missing (layers: roads)", "{ext}");
        let error = match terrio::stream::open_layers(path, &o, 1) {
            Err(error) => error,
            Ok(_) => panic!("{ext}: accepted a missing name"),
        };
        assert_eq!(error.to_string(), "no layer named missing (layers: roads)", "{ext}");
        if ext == "geojson" {
            continue;
        }
        terrio::write_vector(&[Layer::new("roads"), Layer::new("rivers")], path, &Default::default()).unwrap();
        // Selecting one real name must not make a multi-layer file eligible for the alias.
        let o = options(&["roads", "map"]);
        assert_eq!(terrio::open_vector(path, &o).unwrap_err().to_string(), "no layer named map (layers: roads, rivers)", "{ext}");
        let error = match terrio::stream::open_layers(path, &o, 1) {
            Err(error) => error,
            Ok(_) => panic!("{ext}: accepted an alias for multiple layers"),
        };
        assert_eq!(error.to_string(), "no layer named map (layers: roads, rivers)", "{ext}");
    }
}
