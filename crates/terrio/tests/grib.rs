//! The GRIB driver against GDAL 3.12.2's GRIB driver, on small files of GDAL's test suite and
//! files GDAL wrote (`tests/data/grib`): sizes, geotransforms, nodata values, band names and
//! levels, and the pixels (the sum of every band's values, added in order, as GDAL gives them).
//! The real-world files (GFS, HRRR, ECMWF open data) are compared by
//! `scripts/compare_grib_with_gdal.py` and the real-data tests.
#![cfg(feature = "grib")]

use std::path::PathBuf;
use terrio::formats::RasterDriver;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/grib");

/// A band as GDAL reads it: nodata, the sum of its values, element, short name, description.
type Band = (Option<f64>, f64, &'static str, &'static str, &'static str);

/// A file as GDAL reads it: width, height, geotransform and bands.
struct Expected {
    file: &'static str,
    size: (usize, usize),
    gt: [f64; 6],
    bands: &'static [Band],
}

const SFC: &str = "0[-] SFC=\"Ground or water surface\"";
const RESERVED: &str = "0[-] RESERVED=\"Reserved\"";
const EATM: &str = "0[-] EATM=\"Entire Atmosphere\"";
const LCC_WINDOW: [f64; 6] = [-1229020.119442048, 3000.0, 0.0, 58193.80813694153, 0.0, -3000.0];
const POINT: [f64; 6] = [-117.64205450000004, 0.013158999999999999, 0.0, 33.902435, 0.0, -0.010766];
const QUIKSCAT_SFC: &str = "0[-] SFC (Ground or water surface)";

const FILES: &[Expected] = &[
    // PNG packing, MRMS's local table, a WGS 84-like ellipsoid of its own.
    Expected {
        file: "MRMS_EchoTop_18_00.50_20161015-133230.grib2",
        size: (3000, 1500),
        gt: [-89.99999999983328, 0.009999999666555522, 0.0, 24.999999999999996, 0.0, -0.009999999999999998],
        bands: &[(None, -4477087.906070948, "EchoTop_18", "500-GPML", "500[m] GPML=\"Specific altitude above mean sea level\"")],
    },
    // GRIB1 with bitmaps: 9999 for missing values, or 9.999e20 when values reach 9999.
    Expected {
        file: "Sample_QuikSCAT.grb",
        size: (66, 74),
        gt: [-20.2465, 0.3330000000000001, 0.0, 56.0505, 0.0, -0.333],
        bands: &[
            (Some(9999.0), 26897357.0, "CRAIN", "0-SFC", QUIKSCAT_SFC),
            (Some(9999.0), 26885134.318999946, "USCT", "0-SFC", QUIKSCAT_SFC),
            (Some(9999.0), 26902716.10099996, "VSCT", "0-SFC", QUIKSCAT_SFC),
            (Some(9.999e20), 2.689730999999959e24, "TSEC", "0-SFC", QUIKSCAT_SFC),
        ],
    },
    // Complex packing with missing values, without and with spatial differencing (GDAL wrote both).
    Expected { file: "complex_missing.grib2", size: (48, 40), gt: LCC_WINDOW, bands: &[(Some(-10.0), 34692.5, "REFC", "0-EATM", EATM)] },
    Expected {
        file: "complex_sd2_missing.grib2",
        size: (48, 40),
        gt: LCC_WINDOW,
        bands: &[(Some(-10.0), 34692.5, "REFC", "0-EATM", EATM)],
    },
    // A global grid from 0 to 360 degrees whose first column is at 355: rewrapped.
    Expected {
        file: "gfs.t06z.pgrb2.1p0.grib2",
        size: (36, 18),
        gt: [-185.0, 10.0, 0.0, 90.125, 0.0, -10.0],
        bands: &[(None, 65415633.90625, "PRMSL", "0-MSL", "0[-] MSL=\"Mean sea level\"")],
    },
    Expected {
        file: "ieee754_single.grb2",
        size: (22, 18),
        gt: [-117.64205407142856, 0.0005981428571431024, 0.0, 33.90243508823529, 0.0, -0.0005981764705880972],
        bands: &[(None, 50114.0, "unknown", "0-RESERVED", RESERVED)],
    },
    // JPEG 2000 packing (GDAL wrote it), on HRRR's Lambert conformal grid.
    Expected { file: "jpeg2000_lcc.grib2", size: (48, 40), gt: LCC_WINDOW, bands: &[(None, 34692.5, "REFC", "0-EATM", EATM)] },
    // Constant fields scaled by a decimal factor, in three packings.
    Expected {
        file: "jpeg2000_nbits_zero_decimal_scaled.grb2",
        size: (1, 1),
        gt: POINT,
        bands: &[(None, 250.0, "unknown", "0-RESERVED", RESERVED)],
    },
    Expected {
        file: "png_nbits_zero_decimal_scaled.grb2",
        size: (1, 1),
        gt: POINT,
        bands: &[(None, 250.0, "unknown", "0-RESERVED", RESERVED)],
    },
    Expected {
        file: "simple_packing_nbits_zero_decimal_scaled.grb2",
        size: (1, 1),
        gt: POINT,
        bands: &[(None, 250.0, "unknown", "0-RESERVED", RESERVED)],
    },
    Expected {
        file: "lambert_conformal_conic.grb2",
        size: (26, 26),
        gt: [8974734.73768588, 60.021, 0.0, 6235918.969800107, 0.0, -60.021],
        bands: &[(None, 50879.0, "unknown", "0-RESERVED", RESERVED)],
    },
    Expected { file: "png_vis.grib2", size: (48, 40), gt: LCC_WINDOW, bands: &[(None, 16424200.0, "VIS", "0-SFC", SFC)] },
    Expected {
        file: "polar_stereographic.grb2",
        size: (27, 27),
        gt: [-5621962.072511505, 71.86, 0.0, 2943991.8007649756, 0.0, -71.86],
        bands: &[(None, 51551.0, "unknown", "0-RESERVED", RESERVED)],
    },
    // Spatial differencing of order 1 with a bitmap, Kelvin converted to Celsius.
    Expected {
        file: "spatial_differencing_order_1.grb2",
        size: (177, 129),
        gt: [-7127137.299303299, 2500.0, 0.0, 2105561.461394906, 0.0, -2500.0],
        bands: &[(Some(9999.0), 38013550.490735725, "TMIN", "0-SFC", SFC)],
    },
    // Two fields in one message, the second reusing the first's bitmap.
    Expected {
        file: "subgrids_reuse_bitmap.grib2",
        size: (20, 20),
        gt: [440720.0, 60.0, 0.0, 3751320.0, 0.0, -60.0],
        bands: &[
            (Some(9999.0), 50706.0, "UGRD", "0-RESERVED(220)", "0[-] RESERVED(220) (Reserved for local use)"),
            (Some(9999.0), 51294.0, "VGRD", "0-RESERVED(220)", "0[-] RESERVED(220) (Reserved for local use)"),
        ],
    },
    Expected {
        file: "two_bands_with_bitmap.grib2",
        size: (20, 20),
        gt: [440720.0, 60.0, 0.0, 3751320.0, 0.0, -60.0],
        bands: &[(Some(9999.0), 50706.0, "unknown", "0-RESERVED", RESERVED), (Some(9999.0), 50706.0, "unknown", "0-RESERVED", RESERVED)],
    },
];

/// A band's item of the metadata (`BAND_<n>#<key>`, or terrio's own `TERRIO#BAND_<n>_<key>`).
fn item(meta: &[(String, String)], key: &str) -> Option<String> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

#[test]
fn reads_grib_files_as_gdal() {
    for e in FILES {
        let path = format!("{DIR}/{}", e.file);
        let ds = terrio::open_raster(&path).unwrap_or_else(|err| panic!("{}: {err}", e.file));
        assert_eq!(ds.driver, RasterDriver::GRIB, "{}", e.file);
        assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.bands), (e.size.0, e.size.1, e.bands.len()), "{}", e.file);
        // Within 1e-12 relative: the origins of projected grids, projected by terrio rather than
        // PROJ, differ in their last bits.
        let gt = ds.geo_transform();
        assert!(gt.iter().zip(e.gt).all(|(a, b)| (a - b).abs() <= 1e-12 * b.abs().max(1.0)), "{}: {gt:?} vs {:?}", e.file, e.gt);
        let n = e.size.0 * e.size.1;
        let meta = &ds.raster.metadata;
        for (b, (nodata, sum, element, short, description)) in e.bands.iter().enumerate() {
            let k = b + 1;
            let got: f64 = (0..n).map(|i| ds.raster.data.get_f64(b * n + i)).sum();
            assert_eq!(got, *sum, "{} band {k}", e.file);
            let band_nodata = ds.band_nodata(k);
            assert_eq!(band_nodata, *nodata, "{} band {k}", e.file);
            assert_eq!(item(meta, &format!("BAND_{k}#GRIB_ELEMENT")).as_deref(), Some(*element), "{} band {k}", e.file);
            assert_eq!(item(meta, &format!("BAND_{k}#GRIB_SHORT_NAME")).as_deref(), Some(*short), "{} band {k}", e.file);
            assert_eq!(ds.band_metadata[b].description.as_deref(), Some(*description), "{} band {k}", e.file);
        }
    }
}

/// A raster's values.
fn values(ds: &terrio::RasterDataset) -> Vec<f64> {
    (0..ds.raster.data.len()).map(|i| ds.raster.data.get_f64(i)).collect()
}

#[test]
fn reads_grib_by_band_and_from_memory() {
    let path = format!("{DIR}/subgrids_reuse_bitmap.grib2");
    let whole = terrio::open_raster(&path).unwrap();
    let mut lazy = terrio::open_raster_lazy(&path).unwrap();
    assert!(lazy.is_lazy());
    assert_eq!(lazy.driver(), RasterDriver::GRIB);
    // The second band's window: the second field of the message, decoded alone.
    let opts = terrio::WindowOptions { bands: vec![2], overview: None };
    let w = lazy.read_window(terrio::Window::new(3, 5, 7, 4), &opts).unwrap();
    for row in 0..4 {
        for col in 0..7 {
            assert_eq!(w.raster.data.get_f64(row * 7 + col), whole.raster.data.get_f64(400 + (5 + row) * 20 + 3 + col));
        }
    }
    // From memory, read whole and by band.
    let bytes = std::fs::read(&path).unwrap();
    let mem = terrio::memory::open_raster_bytes(bytes.clone(), "x.grib2").unwrap();
    assert_eq!(values(&mem), values(&whole));
    let mut lazy = terrio::memory::open_raster_lazy_bytes(bytes, "x.bin").unwrap();
    assert!(lazy.is_lazy());
    assert_eq!(values(&lazy.read().unwrap()), values(&whole));
}

#[test]
fn rotated_lat_lon_grids() {
    // GDAL's rotated pole test file (template 3.1, JPEG 2000), whose data GDAL's tests replace
    // with zeros (GDAL fails to read it, as terrio does): its georeferencing and metadata.
    let mut bytes = vec![0u8; 1446367];
    let begin = std::fs::read(format!("{DIR}/rotated_pole.grb2.begin")).unwrap();
    bytes[..begin.len()].copy_from_slice(&begin);
    let end = bytes.len();
    bytes[end - 4..].copy_from_slice(b"7777");
    let r = terrio::memory::open_raster_lazy_bytes(bytes, "rotated.grb2").unwrap();
    assert_eq!((r.width(), r.height(), r.band_count()), (1102, 1076, 1));
    assert_eq!(r.geo_transform(), [-62.6222310049955, 0.09000000999091741, 0.0, 48.28500200186046, 0.0, -0.09000000372093023]);
    assert!(r.crs().is_rotated_pole());
    let meta = r.metadata();
    assert_eq!(item(&meta, "BAND_1#GRIB_ELEMENT").as_deref(), Some("DEPR"));
    assert_eq!(r.band_metadata()[0].description.as_deref(), Some("100000[Pa] ISBL=\"Isobaric surface\""));
}

#[test]
fn grib_metadata_items_as_gdal() {
    let ds = terrio::open_raster(&format!("{DIR}/gfs.t06z.pgrb2.1p0.grib2")).unwrap();
    let meta = &ds.raster.metadata;
    let expected = [
        ("GRIB_UNIT", "[Pa]"),
        ("GRIB_COMMENT", "Pressure reduced to MSL [Pa]"),
        ("GRIB_DISCIPLINE", "0(Meteorological)"),
        ("GRIB_PDS_PDTN", "0"),
        ("GRIB_REF_TIME", "1632031200"),
        ("GRIB_VALID_TIME", "1632031200"),
        ("GRIB_FORECAST_SECONDS", "0"),
    ];
    for (k, v) in expected {
        assert_eq!(item(meta, &format!("BAND_1#{k}")).as_deref(), Some(v), "{k}");
    }
    let ids = item(meta, "BAND_1#GRIB_IDS").unwrap();
    assert!(
        ids.starts_with(
            "CENTER=7(US-NCEP) SUBCENTER=0 MASTER_TABLE=2 LOCAL_TABLE=0 SIGNF_REF_TIME=1(Start_of_Forecast) REF_TIME=2021-09-19T06:00:00Z"
        ),
        "{ids}"
    );
    // Kelvin as Celsius.
    let t = terrio::open_raster(&format!("{DIR}/spatial_differencing_order_1.grb2")).unwrap();
    assert_eq!(item(&t.raster.metadata, "BAND_1#GRIB_UNIT").as_deref(), Some("[C]"));
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/grib")
}

/// GRIB2 read as GDAL reads it: the files of `tests/data/grib` written by GDAL's GRIB driver, and
/// what GDAL (through rasterio) reads of them (`scripts/make_grib_fixtures.py`): size, band count,
/// geotransform, CRS, each band's nodata, description, metadata and pixels.
#[test]
fn grib_reads_as_gdal() {
    let text = std::fs::read_to_string(dir().join("expected.json")).unwrap();
    let expected: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut checked = 0;
    for (name, e) in expected.as_object().unwrap() {
        let path = dir().join(name).to_string_lossy().into_owned();
        let ds = terrio::open_raster(&path).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(ds.driver, terrio::formats::RasterDriver::GRIB, "{name}");
        let bands = e["bands"].as_array().unwrap();
        let size = |k: &str| e[k].as_u64().unwrap() as usize;
        assert_eq!((ds.band_count(), ds.raster.rows, ds.raster.cols), (bands.len(), size("height"), size("width")), "{name}");
        assert_eq!(ds.data_type_name(), "Float64", "{name}");
        let gt: Vec<f64> = e["geo_transform"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
        for (a, b) in ds.geo_transform().iter().zip(&gt) {
            assert!((a - b).abs() <= 1e-9 * b.abs().max(1e-3), "{name}: geotransform {:?} != {gt:?}", ds.geo_transform());
        }
        // GDAL's EPSG code (EPSG:4326 for WGS 84 latitude/longitude grids), else its WKT.
        let crs = ds.crs();
        match crs.epsg {
            Some(code) => assert_eq!(Some(code as u64), e["epsg"].as_u64(), "{name}"),
            None => assert_eq!(crs.wkt.as_deref(), e["crs_wkt"].as_str(), "{name}"),
        }
        let values = ds.raster.data.to_f64_vec();
        let n = ds.raster.rows * ds.raster.cols;
        for (b, band) in bands.iter().enumerate() {
            let pixels: Vec<f64> = band["pixels"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            let ours = &values[b * n..(b + 1) * n];
            let differ = ours.iter().zip(&pixels).filter(|(a, b)| a.to_bits() != b.to_bits()).count();
            assert_eq!(differ, 0, "{name} band {}: {differ} pixels differ from GDAL's", b + 1);
            let nodata = if ds.band_nodata.is_empty() { ds.nodata() } else { ds.band_nodata[b] };
            assert_eq!(nodata, band["nodata"].as_f64(), "{name} band {}: nodata", b + 1);
            let md = &ds.band_metadata[b];
            assert_eq!(md.description.as_deref(), band["description"].as_str(), "{name} band {}", b + 1);
            for (k, v) in band["tags"].as_object().unwrap() {
                let ours = md.items.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
                assert_eq!(ours, v.as_str(), "{name} band {}: {k}", b + 1);
            }
        }
        checked += 1;
    }
    assert!(checked >= 10, "{checked} files");
}

/// GRIB files are found by their content as well as by their extension, as GDAL finds them; a
/// file that is not GRIB2 is refused with the reason.
#[test]
fn grib_detection() {
    let src = dir().join("simple.grib2");
    let tmp = tempfile::tempdir().unwrap();
    let renamed = tmp.path().join("forecast.bin");
    std::fs::copy(&src, &renamed).unwrap();
    assert_eq!(terrio::open_raster(renamed.to_str().unwrap()).unwrap().driver, terrio::formats::RasterDriver::GRIB);
    let mut grib1 = std::fs::read(&src).unwrap();
    grib1[7] = 1;
    let p = tmp.path().join("old.grb");
    std::fs::write(&p, grib1).unwrap();
    let err = terrio::open_raster(p.to_str().unwrap()).unwrap_err().to_string();
    assert!(err.contains("GRIB1"), "{err}");
}
