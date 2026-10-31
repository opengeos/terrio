//! The process-wide driver allow-lists, per kind and monotonic for the life of the process, as
//! GDAL's driver de-registration is: one test in a binary of its own, because a list once set
//! cannot be lifted.

use terrio::formats::{Kind, allowed_drivers, set_allowed_drivers};

const GEOJSON: &str =
    r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"v":1},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
const VRT: &str = r#"<VRTDataset rasterXSize="1" rasterYSize="1"><VRTRasterBand dataType="Byte" band="1"></VRTRasterBand></VRTDataset>"#;

/// Whether an open failed because the allow-list refused it (the message `check_drivers` gives).
fn refused<T>(result: &terrio::Result<T>) -> bool {
    match result {
        Ok(_) => false,
        Err(e) => {
            let message = e.to_string();
            message.contains("not allowed") || message.contains("no allowed driver")
        }
    }
}

fn names(list: &[&str]) -> Option<Vec<String>> {
    Some(list.iter().map(|s| s.to_string()).collect())
}

#[test]
fn allow_lists_are_per_kind_and_only_narrow() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, bytes: &[u8]| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_string()
    };
    let geojson = write("a.geojson", GEOJSON.as_bytes());
    let csv = write("a.csv", b"x,y,v\n1,2,3\n");
    let vrt = write("pointer.vrt", VRT.as_bytes());
    let jpg = write("a.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10]);
    let tif = write("a.tif", b"II*\0\x08\0\0\0");
    let vector = terrio::VectorOpenOptions::default();

    assert_eq!((allowed_drivers(None), allowed_drivers(Some(Kind::Raster)), allowed_drivers(Some(Kind::Vector))), (None, None, None));
    set_allowed_drivers(None, None).expect("lifting no list is a no-op");

    // The first raster call sets the raster list only.
    set_allowed_drivers(Some(&["GTiff", "COG", "PNG", "VRT"]), Some(Kind::Raster)).unwrap();
    assert_eq!(allowed_drivers(Some(Kind::Raster)), names(&["GTIFF", "COG", "PNG", "VRT"]));
    assert_eq!(allowed_drivers(Some(Kind::Vector)), None);
    assert!(refused(&terrio::open_raster(&jpg)) && refused(&terrio::LazyRaster::open(&jpg)));
    assert!(!refused(&terrio::open_raster(&vrt)), "VRT is allowed so far");
    assert!(!refused(&terrio::open_raster(&tif)), "GTiff is allowed (the file itself is truncated)");
    assert_eq!(terrio::open_vector(&geojson, &vector).unwrap().layers[0].features.len(), 1, "vectors are not restricted yet");

    // A later, broader call narrows: COG goes, JPEG and NetCDF are not re-enabled.
    set_allowed_drivers(Some(&["GTiff", "PNG", "JPEG", "VRT", "NetCDF"]), Some(Kind::Raster)).unwrap();
    assert_eq!(allowed_drivers(Some(Kind::Raster)), names(&["GTIFF", "PNG", "VRT"]));
    assert!(refused(&terrio::open_raster(&jpg)) && refused(&terrio::LazyRaster::open(&jpg)));
    set_allowed_drivers(Some(&["GTiff", "PNG"]), Some(Kind::Raster)).unwrap();
    assert!(refused(&terrio::open_raster(&vrt)) && refused(&terrio::LazyRaster::open(&vrt)));

    // Lifting a set list is refused and changes nothing; lifting an unset one is a no-op.
    assert!(set_allowed_drivers(None, Some(Kind::Raster)).unwrap_err().to_string().contains("cannot be lifted"));
    assert!(set_allowed_drivers(None, None).is_err());
    set_allowed_drivers(None, Some(Kind::Vector)).unwrap();
    assert_eq!(allowed_drivers(Some(Kind::Raster)), names(&["GTIFF", "PNG"]));
    assert!(refused(&terrio::open_raster(&vrt)));

    // The vector list: the drivers named open, the others are refused and never re-enabled.
    set_allowed_drivers(Some(&["GeoJSON", "ESRI Shapefile"]), Some(Kind::Vector)).unwrap();
    assert_eq!(terrio::open_vector(&geojson, &vector).unwrap().layers[0].features.len(), 1);
    assert!(refused(&terrio::open_vector(&csv, &vector)));
    assert!(refused(&terrio::open_vector(&vrt, &vector)));
    set_allowed_drivers(Some(&["ESRI Shapefile", "GeoJSON", "CSV", "GPKG"]), Some(Kind::Vector)).unwrap();
    assert_eq!(allowed_drivers(Some(Kind::Vector)), names(&["GEOJSON", "ESRI SHAPEFILE"]));
    assert!(refused(&terrio::open_vector(&csv, &vector)), "CSV is not re-enabled");
    set_allowed_drivers(Some(&["ESRI Shapefile"]), Some(Kind::Vector)).unwrap();
    assert!(refused(&terrio::open_vector(&geojson, &vector)), "the vector allow-list refuses GeoJSON now");

    // A dataset of unknown kind (`terrio::open`) needs each kind its driver serves.
    assert!(refused(&terrio::open(&geojson)) && refused(&terrio::open(&jpg)) && refused(&terrio::open(&csv)));
    assert!(!refused(&terrio::open(&tif)));

    // `None` narrows both kinds at once.
    set_allowed_drivers(Some(&["PNG", "ESRI Shapefile"]), None).unwrap();
    assert_eq!((allowed_drivers(Some(Kind::Raster)), allowed_drivers(Some(Kind::Vector))), (names(&["PNG"]), names(&["ESRI SHAPEFILE"])));
    assert!(refused(&terrio::open_raster(&tif)) && refused(&terrio::LazyRaster::open(&tif)));

    // Datasets given as bytes are checked by name and content with their kind, as files are.
    let tif_bytes = b"II*\0\x08\0\0\0".to_vec();
    let png_bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    assert!(refused(&terrio::memory::open_vector_bytes(GEOJSON.as_bytes().to_vec(), "a.geojson", &vector)));
    assert!(refused(&terrio::memory::open_vector_bytes(GEOJSON.as_bytes().to_vec(), "a.shp", &vector)), "GeoJSON named .shp");
    assert!(refused(&terrio::memory::open_bytes(GEOJSON.as_bytes().to_vec(), "a.geojson")));
    assert!(refused(&terrio::memory::open_raster_bytes(tif_bytes.clone(), "a.tif")));
    assert!(refused(&terrio::memory::open_raster_lazy_bytes(tif_bytes.clone(), "a.tif")));
    assert!(refused(&terrio::memory::open_raster_lazy_bytes(tif_bytes, "a.png")), "a TIFF named .png");
    assert!(refused(&terrio::memory::open_raster_lazy_bytes(VRT.as_bytes().to_vec(), "a.png")), "a VRT named .png");
    assert!(!refused(&terrio::memory::open_raster_lazy_bytes(png_bytes, "a.png")), "PNG is allowed (the bytes are truncated)");
    assert!(refused(&terrio::list_layers(&geojson)) && refused(&terrio::list_layers(&csv)));
}
