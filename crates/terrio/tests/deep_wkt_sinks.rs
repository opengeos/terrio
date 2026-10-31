//! A `.prj` nested deeper than PROJ reads reaches the PROJJSON converter of the Arrow and
//! GeoParquet writers, since the shapefile driver keeps a definition it does not identify as the
//! file has it: the converter refuses it as PROJ does, instead of parsing it until the stack
//! overflows (the Python bindings died of SIGSEGV on such a `.prj` of 1,000,000 levels). GDAL
//! refuses this CRS altogether (pyogrio 0.13.0: `CRSError: too many nesting levels`); terrio's
//! writers keep it as text, as any definition the converter does not model. A test binary of its
//! own: without the bound, the overflow aborts its process.
#![cfg(feature = "parquet")]

use parquet::file::reader::{FileReader, SerializedFileReader};
use serde_json::Value;

/// ESRI's NAD 1983 StatePlane New York Long Island (US feet), which GDAL reads.
const ESRI_LONG_ISLAND: &str = r#"PROJCS["NAD_1983_StatePlane_New_York_Long_Island_FIPS_3104_Feet",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic"],PARAMETER["False_Easting",984250.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-74.0],PARAMETER["Standard_Parallel_1",40.66666666666666],PARAMETER["Standard_Parallel_2",41.03333333333333],PARAMETER["Latitude_Of_Origin",40.16666666666666],UNIT["Foot_US",0.3048006096012192]]"#;

/// A one-point shapefile with this `.prj`, as terrio reads it.
fn shapefile_with_prj(dir: &std::path::Path, prj: &str) -> Vec<terrio::Layer> {
    let mut layer = terrio::Layer::new("p");
    layer.add_field(terrio::FieldDef::new("id", terrio::FieldType::Integer));
    let point = terrio::Geometry::Point(terrio::vector::Coord::xy(10.0, 20.0));
    layer.push(terrio::Feature { fid: 0, geometry: Some(point), attributes: vec![terrio::FieldValue::Integer(1)] });
    let path = dir.join("p.shp");
    terrio::write_vector(&[layer], path.to_str().unwrap(), &Default::default()).unwrap();
    std::fs::write(dir.join("p.prj"), prj).unwrap();
    terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers
}

/// The CRS written for the layer: in the GeoArrow metadata of the Arrow table's geometry column,
/// and in the `geo` metadata of the GeoParquet file.
fn written_crs(layers: &[terrio::Layer]) -> (Value, Value) {
    let table = terrio::arrow::layer_to_arrow(&layers[0], &Default::default()).unwrap();
    let field = table.schema.field_with_name("geometry").unwrap();
    let arrow: Value = serde_json::from_str(&field.metadata()["ARROW:extension:metadata"]).unwrap();
    let bytes = terrio::memory::write_vector_bytes(layers, "p.parquet", &Default::default()).unwrap();
    let reader = SerializedFileReader::new(bytes::Bytes::from(bytes)).unwrap();
    let kv = reader.metadata().file_metadata().key_value_metadata().unwrap();
    let geo = kv.iter().find(|k| k.key == "geo").and_then(|k| k.value.as_deref()).unwrap();
    let geo: Value = serde_json::from_str(geo).unwrap();
    (arrow["crs"].clone(), geo["columns"]["geometry"]["crs"].clone())
}

/// Runs `f` on a thread with 2 MiB of stack, Rust's default for spawned threads, given
/// explicitly so that `RUST_MIN_STACK` cannot hide an unbounded recursion.
fn on_2_mib<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new().stack_size(2 << 20).spawn(f).unwrap().join().unwrap()
}

#[test]
fn a_prj_nested_deeper_than_proj_reads_is_kept_as_text_without_overflowing_the_stack() {
    let dir = tempfile::tempdir().unwrap();
    // 90,013 bytes, within GDAL's 100,000-byte limit; the 1 sits at level 30,001.
    let deep = format!("GEOGCS[\"x\",{}1{}]", "A[".repeat(30_000), "]".repeat(30_000));
    let (d, prj) = (dir.path().to_path_buf(), deep.clone());
    let (wkt, (arrow, geo)) = on_2_mib(move || {
        let layers = shapefile_with_prj(&d, &prj);
        (layers[0].crs_wkt().map(str::to_string), written_crs(&layers))
    });
    assert_eq!(wkt.as_deref(), Some(deep.as_str()));
    assert_eq!(arrow, Value::String(deep.clone()));
    assert_eq!(geo, Value::String(deep));
}

#[test]
fn a_prj_gdal_reads_is_written_as_projjson() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_path_buf();
    let (arrow, geo) = on_2_mib(move || written_crs(&shapefile_with_prj(&d, ESRI_LONG_ISLAND)));
    for crs in [&arrow, &geo] {
        assert_eq!(crs["type"], "ProjectedCRS", "{crs}");
        // Lambert Conic Conformal (2SP), in US survey feet.
        assert_eq!(crs["conversion"]["method"]["id"]["code"], 9802, "{crs}");
        assert_eq!(crs["coordinate_system"]["axis"][0]["unit"]["name"], "US survey foot", "{crs}");
    }
}
