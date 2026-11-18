//! The EPSG geographic CRS of an Esri WKT that names its datum only (no `AUTHORITY`), as PROJ
//! 9.9.0 identifies it.

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

/// The geographic CRS of ESRI's GCS_Greenland_1996 and its variants, against PROJ 9.9.0
/// (pyproj 3.8.0, network off): `CRS.from_wkt(wkt).to_epsg(min_confidence=100)` of the WKT, or of
/// its GEOGCS for a projected CRS, given per case. None of them takes the code as its own
/// (`SpatialRef::epsg`): PROJ places them by a ballpark, not by GR96's transformations.
#[test]
fn greenland_1996_is_gr96_by_its_name_as_proj_identifies_it() {
    let cases: [(String, Option<u32>); 11] = [
        // PROJ: 4747.
        (GR96.to_string(), Some(4747)),
        // PROJ: 4747, by EPSG's name.
        (GR96.replace("GCS_Greenland_1996", "GR96"), Some(4747)),
        // Spaced, as some writers put it. PROJ: 4747.
        (GR96.replace("GEOGCS[", "GEOGCS ["), Some(4747)),
        // PROJ: 3182, and 4747 for its GEOGCS.
        (utm_22n("Greenland_1996_UTM_Zone_22N"), Some(4747)),
        // Another name for the projected CRS: PROJ identifies none, but 4747 for its GEOGCS.
        (utm_22n("Foo"), Some(4747)),
        // Another name: PROJ lists no CRS at all.
        (GR96.replace("GCS_Greenland_1996", "Foo"), None),
        // Another axis: PROJ identifies none.
        (GR96.replace("6378137.0", "6378142.0"), None),
        // WKT 2 of that name, 2D and 3D: PROJ reads ESRI's datum names in WKT 1 only, and lists
        // 4747 at 25 %.
        (gcrs(2), None),
        (gcrs(3), None),
        // ESRI's geographic 3D CRS with a `LINUNIT`: PROJ gives 4909 (at 100 %), a geographic 3D
        // CRS terrio's name table leaves out, so terrio gives none.
        (GR96.replace("0.0174532925199433]]", r#"0.0174532925199433],LINUNIT["Meter",1.0]]"#), None),
        // Named Greenland_1996_3D: PROJ gives 4909 at 70 % only; terrio none either.
        (
            r#"GEOGCS["Greenland_1996_3D",DATUM["D_Greenland_1996",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433],LINUNIT["Meter",1.0]]"#.to_string(),
            None,
        ),
    ];
    for (wkt, geographic) in cases {
        let crs = SpatialRef::from_wkt(&wkt);
        assert_eq!(crs.geographic_code(), geographic, "{wkt}");
        assert_eq!(crs.epsg, None, "{wkt}");
    }
}

/// Esri GEOGCSs that name their datum only, as Esri writes them, and the EPSG geographic CRS on
/// the datum PROJ resolves them to (the ID of `CRS.from_wkt(wkt).datum`): ESRI's
/// `D_Kerguelen_Island_1949` is IGN 1962 Kerguelen (6698, EPSG:4698), which PROJ also
/// identifies the GEOGCS as. On the datum's ellipsoid with its semi-major axis 5 m longer, PROJ
/// identifies none (`to_epsg(min_confidence=100)` is None), nor does terrio.
///
/// PROJ identifies Mhast's, Hong Kong's and Tapi Aike's GEOGCSs at 70 % or 90 % only (their CRS
/// names differ from EPSG's), where terrio gives the CRS of the datum PROJ resolves (as terrio
/// identifies a CRS on its datum with its conversion, as GDAL does).
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
/// no CRS is on: PROJ 9.9.0 places it on WGS 84 by "Ballpark geographic offset from GR96 to WGS 84",
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
    // A GeoTIFF writes its datum, not GR96's GeographicTypeGeoKey (2048), for it and its
    // projected CRS; nor does terrio's identification give a layer or raster GR96's code.
    for wkt in [GR96.to_string(), utm_22n("Greenland_1996_UTM_Zone_22N")] {
        let crs = SpatialRef::from_wkt(&wkt);
        assert_eq!(terrio_crs::identify_epsg(&wkt), None, "{wkt}");
        let keys = terrio_crs::geokeys::user_defined_keys(&crs, "custom").unwrap();
        assert!(!keys.shorts.contains(&(2048, 4747)), "{wkt}: {:?}", keys.shorts);
    }
}
