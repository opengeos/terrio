//! Tests on real-world files, with expected values taken from GDAL 3.12.
//!
//! The files live outside the repository: set `TERRIO_TEST_DATA` to their directory (see
//! `scripts/fetch_test_data.sh`). Without it, these tests pass without checking anything.

use std::path::PathBuf;
use terrio::Dataset;

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

fn assert_close(a: f64, b: f64, what: &str) {
    assert!((a - b).abs() <= 1e-9 * b.abs().max(1.0), "{what}: {a} != {b}");
}

#[test]
fn geotiff_model_transformation() {
    let Some(p) = data("cog.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let gt = ds.geo_transform();
    assert_close(gt[0], 6.860074498707138, "origin x");
    assert_close(gt[3], 53.416_342_076_542_28, "origin y");
    assert_close(gt[1], 0.000089831528412, "pixel width");
    assert_eq!(ds.crs().epsg, Some(4326));
    assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.bands), (343, 206, 4));
    // Old-style deflate (compression 32946): values must decode.
    let s = terrio::info::band_statistics(&ds, 0).unwrap();
    assert_eq!((s.min, s.max), (488.0, 2102.0));
}

#[test]
fn geotiff_user_defined_utm() {
    let Some(p) = data("ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    assert_eq!(ds.crs().epsg, Some(32610));
    assert!(ds.nodata().unwrap().is_nan());
}

#[test]
fn geotiff_palette_with_predictor() {
    let Some(p) = data("OPERA_L3_DSWx_WTR.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    assert_eq!(ds.nodata(), Some(255.0));
    let s = terrio::info::band_statistics(&ds, 0).unwrap();
    assert_eq!((s.min, s.max), (0.0, 253.0));
    assert!((s.mean - 1.7177237138564778).abs() < 1e-6, "{}", s.mean);
}

#[test]
fn geotiff_lzw_floating_point_predictor() {
    let Some(p) = data("usgs_dem.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    assert_eq!(ds.crs().epsg, Some(4269));
    assert_eq!((ds.raster.cols, ds.raster.rows), (3612, 3612));
}

#[test]
fn shapefile_in_zip() {
    let Some(p) = data("nybb.zip") else { return };
    let Dataset::Vector(v) = terrio::open(&format!("{p}!nybb_16a/nybb.shp")).unwrap() else { panic!("not a vector") };
    assert_eq!(v.layers[0].len(), 5);
    assert_eq!(terrio::crs::SpatialRef::from_epsg(6538).unwrap().name().unwrap(), "NAD83(2011) / New York Long Island");
}

#[test]
fn geopackage_layers() {
    let Some(p) = data("multi.gpkg") else { return };
    assert_eq!(terrio::list_layers(&p).unwrap(), vec!["buildings", "roads"]);
    let v = terrio::open_vector(&p, &Default::default()).unwrap();
    assert_eq!(v.layers.iter().map(|l| l.len()).collect::<Vec<_>>(), vec![540, 78]);
}

#[test]
fn geojson_defaults_to_wgs84() {
    let Some(p) = data("countries.geojson") else { return };
    let v = terrio::open_vector(&p, &Default::default()).unwrap();
    assert_eq!(v.layers[0].crs_epsg(), Some(4326));
    // GDAL names the layer after the collection's "name" member.
    assert_eq!(v.layers[0].name, "ne_110m_admin_0_countries");
}

#[test]
fn geotiff_user_defined_albers() {
    let Some(p) = data("nlcd_knoxville.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let crs = ds.crs();
    assert!(!crs.is_unknown(), "the custom Albers CRS must be read from the GeoKeys");
    // GDAL: (-84.38694942782567, 36.340361661300086).
    let t = terrio::crs::Transformer::new(&crs, &terrio::crs::SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let (lon, lat) = t.transform(1030275.0, 1539885.0).unwrap();
    assert!((lon - -84.38694942782567).abs() < 1e-9 && (lat - 36.340361661300086).abs() < 1e-9, "{lon} {lat}");
}

/// The GDAL statistics of `srtm_sub.tif`, which every `srtm_sub` copy must reproduce.
const SRTM_SUB_STATS: (f64, f64, f64) = (370.0, 2843.0, 1603.6905150000036);

/// Checks a copy of `srtm_sub.tif` written by GDAL in another format.
fn check_srtm_sub(name: &str, data_type: terrio::DataType, nodata: f64) {
    // A netCDF variable: `NETCDF:"file":variable`.
    let p = match name.split_once(':') {
        Some((file, var)) => match data(file) {
            Some(f) => format!("NETCDF:\"{f}\":{var}"),
            None => return,
        },
        None => match data(name) {
            Some(p) => p,
            None => return,
        },
    };
    let ds = terrio::open_raster(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.bands), (500, 400, 1), "{name}");
    assert_eq!(ds.raster.data_type, data_type, "{name}");
    assert_eq!(ds.nodata(), Some(nodata), "{name}");
    assert_eq!(ds.crs().epsg, Some(4326), "{name}");
    let gt = ds.geo_transform();
    assert!((gt[0] - -119.947_443_593_553).abs() < 1e-7 && (gt[3] - 37.818_439_678_407).abs() < 1e-7, "{name}: {gt:?}");
    assert!((gt[1] - 0.000_808_483_755_7).abs() < 1e-9, "{name}: {gt:?}");
    let s = terrio::info::band_statistics(&ds, 0).unwrap();
    assert_eq!((s.min, s.max), (SRTM_SUB_STATS.0, SRTM_SUB_STATS.1), "{name}");
    assert!((s.mean - SRTM_SUB_STATS.2).abs() < 1e-6, "{name}: {}", s.mean);
}

#[test]
fn srtm_sub_in_gdal_written_formats() {
    use terrio::DataType::{F32, I16};
    check_srtm_sub("srtm_sub.bil", I16, -32768.0);
    check_srtm_sub("srtm_sub.sdat", I16, -32768.0);
    check_srtm_sub("srtm_sub.ers", I16, -32768.0);
    check_srtm_sub("srtm_sub.rst", I16, -32768.0);
    check_srtm_sub("srtm_sub.jp2", I16, -32768.0);
    check_srtm_sub("srtm_sub_v3.zarr", I16, -32768.0);
    check_srtm_sub("srtm_sub_blosc.zarr", I16, -32768.0);
    check_srtm_sub("srtm_sub.grd", F32, 1.701_41e38_f32 as f64);
    check_srtm_sub("srtm_sub.nc:Band1", I16, -32768.0);
}

#[test]
fn netcdf4_variables_as_subdatasets() {
    let Some(p) = data("wind_global.nc") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    // The first variable, as GDAL's first subdataset; south-up latitudes are turned north-up.
    assert_eq!((ds.raster.cols, ds.raster.rows), (360, 181));
    assert_eq!(ds.geo_transform(), [-0.5, 1.0, 0.0, 90.5, 0.0, -1.0]);
    assert!(ds.nodata().unwrap().is_nan());
    let s = terrio::info::band_statistics(&ds, 0).unwrap();
    assert_eq!((s.min, s.max), (-21.61, 25.61));
    let v = terrio::open_raster(&format!("NETCDF:\"{p}\":v_wind")).unwrap();
    let s = terrio::info::band_statistics(&v, 0).unwrap();
    assert_eq!((s.min, s.max), (-22.77, 21.12));
}

#[test]
fn gdal_derived_las_vegas_dem() {
    for name in ["lv_dem.asc", "lv_dem.envi", "lv_dem.img", "lv_dem.jp2", "lv_dem.zarr"] {
        let Some(p) = data(name) else { return };
        let ds = terrio::open_raster(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((ds.raster.cols, ds.raster.rows), (970, 498), "{name}");
        assert_eq!(ds.crs().epsg, Some(3857), "{name}");
        assert_eq!(ds.geo_transform(), [-12824908.0, 1.0, 0.0, 4317505.0, 0.0, -1.0], "{name}");
    }
    // GDAL types an ASCII grid with decimals as Float32, and the JPEG 2000 copy is signed.
    let Some(p) = data("lv_dem.asc") else { return };
    assert_eq!(terrio::open_raster(&p).unwrap().raster.data_type, terrio::DataType::F32);
    let Some(p) = data("lv_dem.jp2") else { return };
    let jp2 = terrio::open_raster(&p).unwrap();
    assert_eq!(jp2.raster.data_type, terrio::DataType::I16);
    let s = terrio::info::band_statistics(&jp2, 0).unwrap();
    assert_eq!((s.min, s.max), (-32767.0, 679.0));
}

#[test]
fn geopackage_gridded_coverage_at_full_resolution() {
    let Some(p) = data("dem.gpkg") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    assert_eq!((ds.raster.cols, ds.raster.rows), (2880, 1773));
    assert_eq!(ds.raster.data_type, terrio::DataType::F32);
    assert_close(ds.geo_transform()[1], 33.239_125_326_623_25, "pixel width");
    assert_eq!(ds.crs().epsg, Some(3857));
}

#[test]
fn geotiff_and_cog_written_by_terrio_read_back() {
    let Some(p) = data("srtm_sub.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (driver, compression) in [(terrio::formats::RasterDriver::GTiff, "LZW"), (terrio::formats::RasterDriver::Cog, "DEFLATE")] {
        let out = dir.path().join(format!("{}.tif", driver.name()));
        let out = out.to_str().unwrap();
        let opts = terrio::RasterWriteOptions { driver: Some(driver), compression: Some(compression.into()), ..Default::default() };
        terrio::write_raster_dataset(&ds, out, &opts).unwrap();
        let back = terrio::open_raster(out).unwrap();
        assert_eq!(back.raster.data_i16(), ds.raster.data_i16());
        assert_eq!(back.nodata(), Some(-32768.0));
        assert_eq!(back.geo_transform(), ds.geo_transform());
    }
}

#[test]
fn netcdf_and_zarr_v3_written_by_terrio_read_back() {
    let Some(p) = data("srtm_sub.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let co = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];
    for (name, driver, compression, options) in [
        ("a.nc", terrio::formats::RasterDriver::NetCDF, None, vec![]),
        ("b.nc", terrio::formats::RasterDriver::NetCDF, None, co("FORMAT", "NC2")),
        ("c.zarr", terrio::formats::RasterDriver::Zarr, Some("ZSTD"), co("FORMAT", "ZARR_V3")),
    ] {
        let out = dir.path().join(name);
        let out = out.to_str().unwrap();
        let opts = terrio::RasterWriteOptions {
            driver: Some(driver),
            compression: compression.map(Into::into),
            creation_options: options,
            ..Default::default()
        };
        terrio::write_raster_dataset(&ds, out, &opts).unwrap();
        let back = terrio::open_raster(out).unwrap();
        assert_eq!(back.raster.data_i16(), ds.raster.data_i16(), "{name}");
        assert_eq!(back.nodata(), Some(-32768.0), "{name}");
        assert_eq!(back.crs().epsg, Some(4326), "{name}");
        for (a, b) in back.geo_transform().iter().zip(ds.geo_transform()) {
            assert_close(*a, b, name);
        }
    }
}

#[test]
fn zipped_shapefile_from_bytes() {
    let Some(p) = data("ne_countries.zip") else { return };
    // GDAL: 177 features in EPSG:4326, 168 fields.
    let ds = terrio::memory::open_vector_bytes(std::fs::read(&p).unwrap(), "ne_countries.zip", &Default::default()).unwrap();
    let l = &ds.layers[0];
    assert_eq!(l.name, "ne_110m_admin_0_countries");
    assert_eq!(l.features.len(), 177);
    assert_eq!(l.crs_epsg(), Some(4326));
    assert_eq!(l.schema.len(), 168);
}

// ── Vector drivers ────────────────────────────────────────────────────────────────────────────

fn vector(name: &str) -> Option<terrio::VectorDataset> {
    Some(terrio::open_vector(&data(name)?, &Default::default()).unwrap())
}

fn field_names(l: &terrio::Layer) -> Vec<&str> {
    l.schema.fields().iter().map(|f| f.name.as_str()).collect()
}

fn extent(l: &terrio::Layer) -> [f64; 4] {
    let e = terrio::vector::layer_extent(l).unwrap();
    [e.0, e.1, e.2, e.3]
}

fn assert_extent(l: &terrio::Layer, expected: [f64; 4]) {
    for (a, b) in extent(l).iter().zip(expected) {
        assert!((a - b).abs() < 1e-6, "{} extent {:?} != {expected:?}", l.name, extent(l));
    }
}

fn value<'a>(l: &'a terrio::Layer, feature: usize, field: &str) -> &'a terrio::FieldValue {
    &l.features[feature].attributes[l.schema.field_index(field).unwrap()]
}

/// Writes layers with terrio into a temporary file and reads them back.
fn round_trip(layers: &[terrio::Layer], name: &str) -> terrio::VectorDataset {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    let path = path.to_str().unwrap();
    terrio::write_vector(layers, path, &Default::default()).unwrap();
    terrio::open_vector(path, &Default::default()).unwrap()
}

#[test]
fn flatgeobuf_reads_and_writes() {
    let Some(v) = vector("UScounties.fgb") else { return };
    let l = &v.layers[0];
    assert_eq!((l.name.as_str(), l.len(), l.crs_epsg()), ("US__counties", 3221, Some(4269)));
    assert_eq!(field_names(l), ["STATE_FIPS", "COUNTY_FIP", "FIPS", "STATE", "NAME", "LSAD"]);
    assert_extent(l, [-179.14734, 17.884813, 179.77847, 71.3525606439998]);
    let back = round_trip(&v.layers, "us.fgb");
    assert_eq!((back.layers[0].len(), back.layers[0].crs_epsg()), (3221, Some(4269)));
    assert_extent(&back.layers[0], extent(l));
    assert_eq!(vector("countries.fgb").unwrap().layers[0].len(), 176);
}

#[test]
fn csv_points_from_coordinate_columns() {
    let Some(v) = vector("hike_gps_trace.csv") else { return };
    let l = &v.layers[0];
    assert_eq!((l.len(), l.crs_epsg(), l.geom_type), (3989, Some(4326), Some(terrio::GeometryType::Point)));
    assert_extent(l, [-122.020158, 45.632416, -122.005147, 45.660109]);
    assert_eq!(l.schema.fields()[0].field_type, terrio::FieldType::Float);
    let back = round_trip(&v.layers, "hike.csv");
    assert_eq!(back.layers[0].len(), 3989);
    assert_extent(&back.layers[0], extent(l));
}

#[test]
fn geoparquet_covering_crs_and_types() {
    let Some(v) = vector("countries.parquet") else { return };
    let l = &v.layers[0];
    assert!(!field_names(l).iter().any(|f| ["xmin", "geometry_bbox"].contains(f)), "{:?}", field_names(l));
    assert_eq!((l.len(), l.crs_epsg(), l.geom_type), (176, Some(4326), Some(terrio::GeometryType::MultiPolygon)));
    let places = vector("ne_places.parquet").unwrap();
    let p = &places.layers[0];
    assert_eq!(p.schema.field("latitude").unwrap().field_type, terrio::FieldType::Float);
    assert!(matches!(value(p, 0, "latitude"), terrio::FieldValue::Float(x) if (x + 34.479999).abs() < 1e-9));
    let back = round_trip(&v.layers, "countries.parquet");
    assert_eq!((back.layers[0].len(), back.layers[0].crs_epsg()), (176, Some(4326)));
}

#[test]
fn geojson_text_and_crs() {
    let Some(v) = vector("countries.geojson") else { return };
    let l = &v.layers[0];
    let ivory = l.features.iter().position(|f| f.attributes.contains(&terrio::FieldValue::Text("Ivory Coast".into()))).unwrap();
    assert_eq!(value(l, ivory, "NAME"), &terrio::FieldValue::Text("Côte d'Ivoire".into()));
    assert_eq!(l.schema.field("LABELRANK").unwrap().field_type, terrio::FieldType::Float);
    // A projected layer keeps its coordinates and names its CRS, as GDAL writes it.
    let ny = terrio::open_vector(&data("nybb.zip").unwrap(), &Default::default()).unwrap();
    let back = round_trip(&ny.layers, "nybb.geojson");
    assert_eq!(back.layers[0].crs_epsg(), ny.layers[0].crs_epsg());
    assert_extent(&back.layers[0], extent(&ny.layers[0]));
}

#[test]
fn shapefile_encodings_and_writing() {
    let Some(v) = vector("ne_countries/ne_110m_admin_0_countries.shp") else { return };
    let l = &v.layers[0];
    assert_eq!(value(l, 0, "NAME"), &terrio::FieldValue::Text("Fiji".into()));
    assert_eq!(value(l, 0, "NAME_ZH"), &terrio::FieldValue::Text("斐济".into()));
    let back = round_trip(&v.layers, "countries.shp");
    let b = &back.layers[0];
    assert_eq!((b.len(), b.crs_epsg()), (177, Some(4326)));
    assert_eq!(value(b, 0, "NAME_ZH"), &terrio::FieldValue::Text("斐济".into()));
    assert_extent(b, extent(l));
}

#[test]
fn geopackage_writing() {
    let Some(v) = vector("UScounties.fgb") else { return };
    let back = round_trip(&v.layers, "us.gpkg");
    let b = &back.layers[0];
    assert_eq!((b.name.as_str(), b.len(), b.crs_epsg()), ("US__counties", 3221, Some(4269)));
    assert_extent(b, extent(&v.layers[0]));
    // Layer names with spaces, several layers.
    let kml = vector("kml_samples.kml").unwrap();
    let back = round_trip(&kml.layers, "samples.gpkg");
    assert_eq!(back.layers.iter().map(|l| l.len()).sum::<usize>(), 21);
}

#[test]
fn gml_features_ids_and_crs() {
    let Some(v) = vector("countries.gml") else { return };
    let l = &v.layers[0];
    assert_eq!((l.name.as_str(), l.len(), l.crs_epsg()), ("ne_110m_admin_0_countries", 176, Some(4326)));
    assert_eq!(field_names(l)[..3], ["gml_id", "scalerank", "featurecla"]);
    assert_extent(l, [-180.0, -55.61183, 180.0, 83.64513]);
    let back = round_trip(&v.layers, "countries.gml");
    assert_extent(&back.layers[0], extent(l));
}

#[test]
fn gpx_layers_like_gdal() {
    let Some(v) = vector("hike.gpx") else { return };
    let names: Vec<&str> = v.layers.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["waypoints", "routes", "tracks", "route_points", "track_points"]);
    assert_eq!(v.layers[0].len(), 3989);
    assert_eq!(field_names(&v.layers[0])[..5], ["ele", "time", "magvar", "geoidheight", "name"]);
    let old = vector("gpx10_tracks.gpx").unwrap();
    assert_eq!((old.layers[2].len(), old.layers[4].len()), (4, 871));
    assert!(field_names(&old.layers[4]).contains(&"course"));
    let back = round_trip(&old.layers, "tracks.gpx");
    assert_eq!((back.layers[2].len(), back.layers[4].len()), (4, 871));
}

#[test]
fn kml_and_kmz_like_libkml() {
    let Some(v) = vector("countries.kml") else { return };
    let l = &v.layers[0];
    assert_eq!(
        field_names(l)[..12],
        [
            "id",
            "Name",
            "description",
            "timestamp",
            "begin",
            "end",
            "altitudeMode",
            "tessellate",
            "extrude",
            "visibility",
            "drawOrder",
            "icon"
        ]
    );
    assert_eq!(l.schema.field("LABELRANK").unwrap().field_type, terrio::FieldType::Float);
    let samples = vector("kml_samples.kml").unwrap();
    let counts: Vec<(&str, usize)> = samples.layers.iter().map(|l| (l.name.as_str(), l.len())).collect();
    assert_eq!(
        counts,
        [
            ("Placemarks", 3),
            ("Styles and Markup", 1),
            ("Highlighted Icon", 1),
            ("Ground Overlays", 1),
            ("Screen Overlays", 0),
            ("Paths", 6),
            ("Polygons", 0),
            ("Google Campus", 4),
            ("Extruded Polygon", 1),
            ("Absolute and Relative", 4)
        ]
    );
    // LIBKML renames a NAME field that clashes with Name.
    assert!(field_names(&vector("census_states.kml").unwrap().layers[0]).contains(&"NAME2"));
    assert_eq!(vector("ne_places.kmz").unwrap().layers[0].len(), 7342);
    let back = round_trip(&v.layers, "countries.kmz");
    assert_eq!(back.layers[0].len(), 176);
}

#[test]
fn topojson_objects_as_layers() {
    let Some(v) = vector("world-110m.topojson") else { return };
    let counts: Vec<(&str, usize)> = v.layers.iter().map(|l| (l.name.as_str(), l.len())).collect();
    assert_eq!(counts, [("countries", 177), ("land", 1)]);
    assert_extent(&v.layers[0], [-180.0, -85.60903777459771, 180.0, 83.64513]);
    let back = round_trip(&v.layers, "world.topojson");
    assert_eq!(back.layers.iter().map(|l| l.len()).collect::<Vec<_>>(), [177, 1]);
}

#[test]
fn mapinfo_mif() {
    let Some(v) = vector("countries.mif") else { return };
    let l = &v.layers[0];
    assert_eq!((l.len(), l.crs_epsg()), (176, Some(4326)));
    assert_extent(l, [-180.0, -55.61183, 180.0, 83.64513]);
    let back = round_trip(&v.layers, "countries.mif");
    assert_eq!((back.layers[0].len(), back.layers[0].crs_epsg()), (176, Some(4326)));
}

#[test]
fn osm_pbf_layers_like_gdal() {
    let Some(v) = vector("liechtenstein.osm.pbf") else { return };
    let names: Vec<&str> = v.layers.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["points", "lines", "multilinestrings", "multipolygons", "other_relations"]);
    assert!(v.layers.iter().all(|l| !l.is_empty()));
    assert_eq!(
        field_names(&v.layers[1]),
        ["osm_id", "name", "highway", "waterway", "aerialway", "barrier", "man_made", "railway", "z_order", "other_tags"]
    );
    assert!(v.layers[3].features.iter().all(|f| matches!(f.geometry, Some(terrio::Geometry::MultiPolygon(_)))));
}

#[test]
fn mapinfo_tab_reads_and_writes() {
    let Some(v) = vector("countries.tab") else { return };
    let l = &v.layers[0];
    let mif = vector("countries.mif").unwrap();
    assert_eq!((l.len(), l.crs_epsg()), (176, Some(4326)));
    assert_eq!(field_names(l), field_names(&mif.layers[0]));
    assert_eq!(l.features[0].fid, 1);
    assert_eq!(value(l, 0, "NAME"), value(&mif.layers[0], 0, "NAME"));
    assert_extent(l, [-180.0, -55.61183, 180.0, 83.64513]);
    let back = round_trip(&v.layers, "countries.tab");
    assert_eq!((back.layers[0].len(), back.layers[0].crs_epsg()), (176, Some(4326)));
    assert_extent(&back.layers[0], extent(l));
    // A projected table: the CRS comes from the .map header.
    let utm = vector("roads_utm.tab").unwrap();
    assert_eq!((utm.layers[0].len(), utm.layers[0].crs_epsg()), (78, Some(32611)));
}

#[test]
fn geoparquet_bbox_filter() {
    let Some(p) = data("ne_places.parquet") else { return };
    let rect = (-10.0, 35.0, 30.0, 60.0);
    let options = terrio::VectorOpenOptions { spatial_filter: Some(rect), ..Default::default() };
    let v = terrio::open_vector(&p, &options).unwrap();
    let l = &v.layers[0];
    // ogr2ogr -spat -10 35 30 60 keeps 752 places.
    assert_eq!(l.len(), 752);
    assert!(l.features.iter().all(|f| matches!(&f.geometry, Some(terrio::Geometry::Point(c)) if c.x >= -10.0 && c.x <= 30.0)));
    // Written back: the CRS is PROJJSON, the bounding boxes in a covering column.
    let reprojected = terrio::vector::reproject_layer(l, &terrio::crs::SpatialRef::from_epsg(3035).unwrap()).unwrap();
    let back = round_trip(&[reprojected], "places.parquet");
    assert_eq!((back.layers[0].len(), back.layers[0].crs_epsg()), (752, Some(3035)));
}

#[test]
fn osm_xml_like_pbf() {
    // Made from the PBF file when osmium is available (see fetch_test_data.sh).
    let Some(dir) = std::env::var_os("TERRIO_TEST_DATA") else { return };
    let xml = PathBuf::from(dir).join("liechtenstein.osm");
    if !xml.exists() {
        return;
    }
    let x = terrio::open_vector(xml.to_str().unwrap(), &Default::default()).unwrap();
    let p = vector("liechtenstein.osm.pbf").unwrap();
    for (a, b) in x.layers.iter().zip(&p.layers) {
        assert_eq!((a.name.as_str(), a.len()), (b.name.as_str(), b.len()));
        assert_eq!(field_names(a), field_names(b));
    }
}

#[test]
fn kml_styles() {
    let Some(v) = vector("countries.kml") else { return };
    let l = &v.layers[0];
    // GDAL's style string for <LineStyle><color>ff0000ff</color>...<fill>0</fill>.
    assert_eq!(value(l, 0, "OGR_STYLE"), &terrio::FieldValue::Text("PEN(c:#FF0000FF)".into()));
    let back = round_trip(&v.layers, "countries.kml");
    assert_eq!(value(&back.layers[0], 0, "OGR_STYLE"), value(l, 0, "OGR_STYLE"));
    let samples = vector("kml_samples.kml").unwrap();
    let paths = samples.layers.iter().find(|l| l.name == "Paths").unwrap();
    assert_eq!(value(paths, 5, "OGR_STYLE"), &terrio::FieldValue::Text("PEN(c:#FF0000FF);BRUSH(fc:#0000FFFF)".into()));
}
