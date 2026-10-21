//! The WKT terrio writes for a CRS given as a PROJ string, and the GeoKeys of the GeoTIFFs it
//! writes, describe the same CRS (#124): terrio reading them gives terrio's coordinates for the
//! PROJ string, for every projection terrio computes, with parameters other than the defaults.
//! PROJ's and GDAL's reading of them is checked by `python/tests/test_crs_wkt.py`.

use terrio::crs::{SpatialRef, Transformer};
use terrio::raster::RasterData;
use terrio::{DataType, RasterConfig, RasterDataset};

const W: &str = "+ellps=WGS84 +units=m";
const B: &str = "+ellps=bessel +units=m";

/// PROJ strings (with `{W}` and `{B}` for their ellipsoids) and a point near which to check them.
const CASES: &[(&str, (f64, f64))] = &[
    ("+proj=merc +lat_ts=33 +lon_0=-100 {W}", (-95.0, 40.0)),
    ("+proj=merc +k=0.99 +lon_0=-100 {W}", (-95.0, 40.0)),
    ("+proj=lcc +lat_1=40 +lat_0=40 +lon_0=-96 +k_0=0.999 {W}", (-90.0, 45.0)),
    ("+proj=lcc +lat_1=40 +lon_0=-96 +k_0=0.999 {W}", (-90.0, 45.0)),
    ("+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +x_0=1000 +y_0=2000 {W}", (-90.0, 41.0)),
    ("+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +k_0=0.999 {W}", (-90.0, 41.0)),
    ("+proj=somerc +lat_0=46.95 +lon_0=7.44 {B}", (8.0, 47.0)),
    ("+proj=krovak {B}", (15.0, 50.0)),
    ("+proj=krovak +axis=swu {B}", (15.0, 50.0)),
    ("+proj=mod_krovak {B}", (15.0, 50.0)),
    ("+proj=hammer {W}", (30.0, 40.0)),
    ("+proj=collg {W}", (30.0, 40.0)),
    ("+proj=nzmg +ellps=intl +units=m", (174.0, -41.0)),
    ("+proj=eqc +lat_ts=30 {W}", (10.0, 40.0)),
    ("+proj=eqc +lat_ts=30 +lat_0=10 +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    ("+proj=omerc +lat_0=4 +lonc=115 +alpha=53.31 +gamma=53.13 +k=0.99984 +no_uoff {W}", (116.0, 5.0)),
    ("+proj=omerc +lat_0=4 +lonc=115 +alpha=53.31 +gamma=40 +k=0.99984 {W}", (116.0, 5.0)),
    ("+proj=omerc +lat_0=4 +lonc=115 +alpha=53.31 +k=0.99984 +x_0=1000 {W}", (116.0, 5.0)),
    ("+proj=loxim +lat_1=40 {W}", (30.0, 50.0)),
    ("+proj=tmerc +lon_0=-100 +k=0.9996 +x_0=500000 +ellps=WGS84 +units=us-ft", (-95.0, 40.0)),
    ("+proj=tmerc +lon_0=-100 +towgs84=100,50,-30 {W}", (-95.0, 40.0)),
    ("+proj=tmerc +lon_0=10 +pm=ferro {W}", (28.0, 48.0)),
    ("+proj=etmerc +lon_0=10 {W}", (12.0, 48.0)),
    ("+proj=tmerc +lat_0=5 +lon_0=10 +k=0.9995 +x_0=1000 +y_0=2000 {W}", (12.0, 8.0)),
    ("+proj=tmerc +axis=wsu +lon_0=29 {W}", (28.0, -26.0)),
    ("+proj=utm +zone=33 +south {W}", (14.0, -30.0)),
    ("+proj=utm +zone=33 +datum=WGS84 +units=m", (14.0, 30.0)),
    ("+proj=tmerc +lat_0=49 +lon_0=-1 +k=0.9996 +x_0=400000 +y_0=-100000 +datum=OSGB36 +units=m", (-2.0, 53.0)),
    ("+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=1000 +y_0=2000 {W}", (-90.0, 41.0)),
    ("+proj=eqdc +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=1000 {W}", (-90.0, 41.0)),
    ("+proj=laea +lat_0=52 +lon_0=10 +x_0=1000 +y_0=2000 {W}", (12.0, 50.0)),
    ("+proj=aeqd +lat_0=40 +lon_0=20 +x_0=1000 {W}", (25.0, 45.0)),
    ("+proj=stere +lat_0=40 +lon_0=10 +k=0.999 +x_0=1000 {W}", (12.0, 42.0)),
    ("+proj=stere +lat_0=90 +lon_0=-45 +k=0.994 +x_0=1000 {W}", (-40.0, 75.0)),
    ("+proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +x_0=1000 {W}", (10.0, -75.0)),
    ("+proj=ups {W}", (30.0, 85.0)),
    ("+proj=sterea +lat_0=52.15 +lon_0=5.38 +k=0.9999 +x_0=155000 +y_0=463000 {W}", (6.0, 52.0)),
    ("+proj=cass +lat_0=10 +lon_0=-61 +x_0=1000 {W}", (-60.0, 11.0)),
    ("+proj=poly +lat_0=10 +lon_0=-61 +x_0=1000 {W}", (-60.0, 11.0)),
    ("+proj=sinu +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    ("+proj=bonne +lat_1=40 +lon_0=20 +x_0=1000 {W}", (25.0, 45.0)),
    ("+proj=mill +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    ("+proj=vandg +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    ("+proj=gnom +lat_0=40 +lon_0=20 +x_0=1000 {W}", (25.0, 42.0)),
    ("+proj=cea +lat_ts=30 +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    ("+proj=eqearth +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    ("+proj=labrd +lat_0=-18.9 +lon_0=46.43 +azi=18.9 +k_0=0.9995 +x_0=400000 +y_0=800000 {W}", (47.0, -19.0)),
    ("+proj=col_urban +lat_0=4.68 +lon_0=-74.14 +x_0=92334.879 +y_0=109320.965 +h_0=2550 {W}", (-74.0, 4.7)),
    ("+proj=ortho +lat_0=40 +lon_0=20 +x_0=1000 {W}", (25.0, 42.0)),
    ("+proj=geos +h=35786023 +lon_0=-75 +sweep=x {W}", (-70.0, 10.0)),
    ("+proj=robin +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    ("+proj=wintri +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    ("+proj=wink1 +lat_ts=30 +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=wink2 +lat_1=30 +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=gall +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=gstmerc +lat_0=-21.11666667 +lon_0=55.53333333 +x_0=160000 +y_0=50000 {W}", (55.5, -21.0)),
    ("+proj=fouc_s +n=0.5 +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=mbtfps +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=putp4p +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=murd2 +lat_1=30 +lat_2=50 +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=tissot +lat_1=30 +lat_2=50 +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=moll +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    ("+proj=eck4 +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=eck6 +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=aitoff +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=natearth +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=wag3 +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=kav7 +lon_0=20 {W}", (25.0, 40.0)),
    ("+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80 +units=km", (12.0, 50.0)),
    ("+proj=tmerc +lon_0=10 +R=6371000 +to_meter=2.5", (12.0, 48.0)),
    ("+proj=tmerc +lon_0=10 +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7 +units=m", (12.0, 48.0)),
    ("+proj=tmerc +lon_0=10 +pm=3.5 +a=6378000 +rf=300 +units=m", (16.0, 48.0)),
    ("+proj=longlat +ellps=bessel +pm=paris +towgs84=1,2,3", (8.0, 48.0)),
];

fn proj(case: &str) -> String {
    case.replace("{W}", W).replace("{B}", B)
}

/// The coordinates of WGS 84 points around a point in a CRS.
fn coords(crs: &SpatialRef, (lon, lat): (f64, f64)) -> Vec<(f64, f64)> {
    let t = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), crs).unwrap();
    let mut out = Vec::new();
    for dx in [-1.0, 0.0, 1.5] {
        for dy in [-1.0, 0.0, 1.5] {
            out.push(t.transform(lon + dx, lat + dy).unwrap());
        }
    }
    out
}

fn distance(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    a.iter().zip(b).map(|(p, q)| (p.0 - q.0).hypot(p.1 - q.1)).fold(0.0, f64::max)
}

#[test]
fn wkt_of_a_proj_string_reads_as_the_proj_string() {
    for (case, point) in CASES {
        let p = proj(case);
        let crs = SpatialRef::parse(&p).unwrap();
        let wkt = crs.to_wkt().unwrap();
        let d = distance(&coords(&crs, *point), &coords(&SpatialRef::parse(&wkt).unwrap(), *point));
        assert!(d < 1e-3, "{p}: {d} m\n{wkt}");
        // Without its extension too, as a WKT stored without its PROJ string is read.
        let bare = SpatialRef { wkt: Some(wkt.clone()), ..Default::default() };
        let d = distance(&coords(&crs, *point), &coords(&bare, *point));
        assert!(d < 1e-3, "{p} (WKT only): {d} m\n{wkt}");
    }
}

#[test]
fn geotiff_of_a_proj_string_reads_as_the_proj_string() {
    let dir = tempfile::tempdir().unwrap();
    for (i, (case, point)) in CASES.iter().enumerate() {
        let p = proj(case);
        let crs = SpatialRef::parse(&p).unwrap();
        // As the CRS itself, and as its WKT alone (which terrio.rio passes).
        for (j, given) in [crs.clone(), SpatialRef { wkt: crs.to_wkt(), ..Default::default() }].into_iter().enumerate() {
            let cfg = RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::F32, crs: given.to_crs_info(), ..Default::default() };
            let mut ds = RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::F32(vec![0.0; 4])).unwrap(), false);
            ds.set_geo_transform([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]);
            let path = dir.path().join(format!("{i}_{j}.tif")).to_string_lossy().into_owned();
            terrio::write_raster_dataset(&ds, &path, &Default::default()).unwrap();
            let read = terrio::open_raster(&path).unwrap().crs();
            let d = distance(&coords(&crs, *point), &coords(&read, *point));
            assert!(d < 1e-3, "{p} ({j}): {d} m\n{:?}", read.to_wkt());
        }
    }
}

#[test]
fn geotiff_of_a_wkt_on_an_epsg_datum_keeps_its_datum() {
    // A transverse Mercator without EPSG code on ED50, whose datum terrio identifies: GeoKeys on
    // EPSG:4230, read back with ED50's transformations.
    let geogcs = SpatialRef::from_epsg(4230).unwrap().to_wkt().unwrap();
    let wkt = format!(
        "PROJCS[\"unknown\",{geogcs},PROJECTION[\"Transverse_Mercator\"],PARAMETER[\"latitude_of_origin\",0],PARAMETER[\"central_meridian\",4],PARAMETER[\"scale_factor\",0.9995],PARAMETER[\"false_easting\",300000],PARAMETER[\"false_northing\",0],UNIT[\"metre\",1]]"
    );
    let crs = SpatialRef { wkt: Some(wkt), ..Default::default() };
    let cfg = RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::F32, crs: crs.to_crs_info(), ..Default::default() };
    let mut ds = RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::F32(vec![0.0; 4])).unwrap(), false);
    ds.set_geo_transform([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ed50.tif").to_string_lossy().into_owned();
    terrio::write_raster_dataset(&ds, &path, &Default::default()).unwrap();
    let read = terrio::open_raster(&path).unwrap().crs();
    let d = distance(&coords(&crs, (5.0, 50.0)), &coords(&read, (5.0, 50.0)));
    assert!(d < 1e-3, "{d} m\n{:?}", read.to_wkt());
}

#[test]
fn wkt1_reader_accepts_custom_proj4_and_krovak_axes() {
    // GDAL's custom_proj4: the PROJ string of the extension.
    let hammer = r#"PROJCS["unknown",GEOGCS["unknown",DATUM["Unknown based on WGS 84 ellipsoid",SPHEROID["WGS 84",6378137,298.257223563,AUTHORITY["EPSG","7030"]]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]]],PROJECTION["custom_proj4"],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],EXTENSION["PROJ4","+proj=hammer +lon_0=20 +ellps=WGS84 +units=m"]]"#;
    let own = SpatialRef::parse("+proj=hammer +lon_0=20 +ellps=WGS84 +units=m").unwrap();
    assert!(distance(&coords(&own, (30.0, 40.0)), &coords(&SpatialRef::parse(hammer).unwrap(), (30.0, 40.0))) < 1e-3);
    // Krovak's axes: east-north, or south-west.
    let krovak = SpatialRef::parse("+proj=krovak +ellps=bessel +units=m").unwrap().to_wkt().unwrap();
    let swu = SpatialRef::parse("+proj=krovak +axis=swu +ellps=bessel +units=m").unwrap();
    let as_swu = krovak.replace("AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]", "AXIS[\"Southing\",SOUTH],AXIS[\"Westing\",WEST]");
    assert!(as_swu != krovak);
    assert!(distance(&coords(&swu, (15.0, 50.0)), &coords(&SpatialRef::parse(&as_swu).unwrap(), (15.0, 50.0))) < 1e-3);
}
