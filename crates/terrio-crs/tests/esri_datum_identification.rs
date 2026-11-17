//! The EPSG geographic CRS of an Esri WKT that names its datum only (no `AUTHORITY`), as PROJ
//! 9.8.1 identifies it.

use terrio_crs::{SpatialRef, Transformer};

/// Esri's GCS of Greenland 1996: `D_Greenland_1996` is EPSG's Greenland 1996 datum (6747), which
/// no CRS is on; GR96 (EPSG:4747) is on the GR96 ensemble (1421), which has the name `GR96` in
/// common with it.
const GR96: &str = r#"GEOGCS["GCS_Greenland_1996",DATUM["D_Greenland_1996",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;

/// PROJ's `WKT1_ESRI` of GR96 / UTM zone 22N (EPSG:3182) after its GEOGCS, on the GCS `{}`.
fn utm_22n(name: &str) -> String {
    format!(
        r#"PROJCS["{name}",{GR96},PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-51.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#
    )
}

/// A WKT 2 `GEOGCRS` named GCS_Greenland_1996 on `D_Greenland_1996`, of `dimension` 2 or 3.
fn gcrs(dimension: u8) -> String {
    let height = if dimension == 3 { r#",AXIS["ellipsoidal height",up,ORDER[3],LENGTHUNIT["metre",1]]"# } else { "" };
    format!(
        r#"GEOGCRS["GCS_Greenland_1996",DATUM["D_Greenland_1996",ELLIPSOID["GRS_1980",6378137,298.257222101,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,{dimension}],AXIS["longitude",east,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["latitude",north,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]]{height}]"#
    )
}

/// PROJ 9.8.1 (pyproj 3.8.0, network off) identifies ESRI's GCS_Greenland_1996 and its UTM zone
/// 22N as GR96 by their names, and no CRS for another name, for another axis or for a geographic
/// 3D CRS: `CRS.from_wkt(wkt).to_epsg(min_confidence=100)` gives 4747, 4747, 4747 (spaced), 3182,
/// None, None, None and None for the WKT 2 `GEOGCRS` of that name in 2D and 3D, and
/// None (for `Foo` it lists none at all), then EPSG:4909 for ESRI's GCS_Greenland_1996 with a
/// `LINUNIT`, a geographic 3D CRS terrio does not identify by name, and EPSG:4909 at 70 % for
/// Greenland_1996_3D.
#[test]
fn greenland_1996_is_gr96_by_its_name_as_proj_identifies_it() {
    let cases: [(String, Option<u32>, Option<u32>); 11] = [
        (GR96.to_string(), Some(4747), Some(4747)),
        (GR96.replace("GCS_Greenland_1996", "GR96"), Some(4747), Some(4747)),
        // Spaced, as some writers put it.
        (GR96.replace("GEOGCS[", "GEOGCS ["), Some(4747), Some(4747)),
        (utm_22n("Greenland_1996_UTM_Zone_22N"), Some(3182), Some(4747)),
        (GR96.replace("GCS_Greenland_1996", "Foo"), None, None),
        (GR96.replace("6378137.0", "6378142.0"), None, None),
        // The projected CRS of another name is not EPSG:3182, but its GEOGCS is GR96.
        (utm_22n("Foo"), None, Some(4747)),
        // WKT 2 of that name, 2D and 3D: PROJ maps ESRI's names for WKT 1 only.
        (gcrs(2), None, None),
        (gcrs(3), None, None),
        // ESRI's geographic 3D CRS, which PROJ identifies as EPSG:4909 (not in the name table).
        (GR96.replace("0.0174532925199433]]", r#"0.0174532925199433],LINUNIT["Meter",1.0]]"#), None, None),
        (
            r#"GEOGCS["Greenland_1996_3D",DATUM["D_Greenland_1996",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433],LINUNIT["Meter",1.0]]"#.to_string(),
            None,
            None,
        ),
    ];
    for (wkt, code, geographic) in cases {
        assert_eq!(terrio_crs::identify_epsg(&wkt), code, "{wkt}");
        assert_eq!(SpatialRef::from_wkt(&wkt).geographic_code(), geographic, "{wkt}");
    }
}

/// Esri GEOGCSs that name their datum only, as Esri writes them, and the EPSG geographic CRS on
/// the datum PROJ resolves them to (the ID of `CRS.from_wkt(wkt).datum`): ESRI's
/// `D_Kerguelen_Island_1949` is IGN 1962 Kerguelen (6698, EPSG:4698), which PROJ also
/// identifies the GEOGCS as. On the datum's ellipsoid with its semi-major axis 5 m longer, PROJ
/// identifies none (`to_epsg(min_confidence=100)` is None), nor does terrio.
///
/// PROJ identifies Mhast's, Hong Kong's and Tapi Aike's GEOGCSs at 70 % or 90 % only (their CRS
/// names differ from EPSG's), where terrio gives the CRS of the datum PROJ resolves (as
/// [`terrio_crs::identify_epsg`] identifies a CRS on its datum with its conversion, as GDAL does).
#[test]
fn an_esri_datum_name_alone_gives_the_geographic_crs_of_that_datum() {
    let gcs = |name: &str, datum: &str, ellipsoid: &str, a: f64, rf: f64| {
        format!(
            r#"GEOGCS["{name}",DATUM["{datum}",SPHEROID["{ellipsoid}",{a:?},{rf:?}]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#
        )
    };
    for (name, datum, ellipsoid, a, rf, code) in [
        ("GCS_Abidjan_1987", "D_Abidjan_1987", "Clarke_1880_RGS", 6_378_249.145, 293.465, 4143),
        ("GCS_PRS_1992", "D_Philippine_Reference_System_1992", "Clarke_1866", 6_378_206.4, 294.978_698_213_898, 4683),
        ("GCS_Grenada_1953", "D_Grenada_1953", "Clarke_1880_RGS", 6_378_249.145, 293.465, 4603),
        ("GCS_Kerguelen_Island_1949", "D_Kerguelen_Island_1949", "International_1924", 6_378_388.0, 297.0, 4698),
        ("GCS_Mhast", "D_Mhast", "International_1924", 6_378_388.0, 297.0, 4264),
        ("Hong_Kong_Geodetic_CS", "Hong_Kong_Geodetic", "GRS_1980", 6_378_137.0, 298.257_222_101, 8427),
        ("Tapi_Aike", "Tapi_Aike", "International_1924", 6_378_388.0, 297.0, 9248),
        ("GCS_Greenland_1996", "D_Greenland_1996", "GRS_1980", 6_378_137.0, 298.257_222_101, 4747),
    ] {
        let wkt = gcs(name, datum, ellipsoid, a, rf);
        assert_eq!(SpatialRef::from_wkt(&wkt).geographic_code(), Some(code), "{wkt}");
        let altered = gcs(name, datum, ellipsoid, a + 5.0, rf);
        assert_eq!(SpatialRef::from_wkt(&altered).geographic_code(), None, "{altered}");
    }
}

/// Identified as GR96, ESRI's GCS_Greenland_1996 keeps the transformations of its own datum, which
/// no CRS is on: PROJ 9.8.1 places it on WGS 84 by "Ballpark geographic offset from GR96 to WGS 84",
/// not by "GR96 to WGS 84 (1)" as EPSG:4747 (`Transformer.from_crs(CRS(wkt), "EPSG:4326",
/// always_xy=True)`; the point (-51.7, 64.2) is kept in both).
#[test]
fn greenland_1996_identified_as_gr96_keeps_its_ballpark() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let t = Transformer::new(&SpatialRef::from_wkt(GR96), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let (x, y) = t.transform(-51.7, 64.2).unwrap();
    assert!((x + 51.7).abs() < 1e-9 && (y - 64.2).abs() < 1e-9, "({x}, {y})");
    assert!(t.operation_at(-51.7, 64.2, 0.0).unwrap().unwrap().ballpark);
    // Neither it nor its projected CRS takes the identified code as its own, and a GeoTIFF writes
    // its datum, not GR96's GeographicTypeGeoKey (2048).
    for wkt in [GR96.to_string(), utm_22n("Greenland_1996_UTM_Zone_22N")] {
        let crs = SpatialRef::from_wkt(&wkt);
        assert_eq!(crs.epsg, None, "{wkt}");
        assert_eq!(crs.geographic_code(), Some(4747), "{wkt}");
        let keys = terrio_crs::geokeys::user_defined_keys(&crs, "custom").unwrap();
        assert!(!keys.shorts.contains(&(2048, 4747)), "{wkt}: {:?}", keys.shorts);
    }
}
