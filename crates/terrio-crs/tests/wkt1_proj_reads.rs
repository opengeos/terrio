//! WKT that PROJ 9.9.0 reads is read, and places points where PROJ does: ESRI's
//! `Mercator_Variant_A` (EPSG Guidance Note 7-2's Makassar / NEIEZ example, EPSG:3002, as an ESRI
//! `.prj` names its method) and EPSG's `Mercator (variant A)` in WKT 1, as PROJ's Mercator (1SP),
//! GDAL's `Mercator_1SP` with a latitude of origin (and a scale of 1) as Mercator (variant B) with
//! that latitude as its standard parallel, GDAL's `Lambert_Conformal_Conic_2SP` with a
//! `scale_factor` (ignored, as PROJ ignores it), ESRI's `Lambert_Conformal_Conic` with one
//! (applied) and with one parallel as 1SP at its latitude of origin, Hotine variant A (in WKT 2
//! too) at an azimuth and grid angle of 90° as the Swiss oblique Mercator, and WKT 1 and WKT 2 with
//! a node of an unknown keyword (`rustgis:unapplied`, `a-b`, `a"b c"`, an empty one), which PROJ
//! skips whatever its keyword. A keyword with a space, a vertical tab, a NUL or an unclosed quote
//! in it is refused, as PROJ refuses it, and so is EPSG's `Mercator (variant A)` with a latitude of
//! origin other than 0, which PROJ reads but neither transforms nor writes as a PROJ string. A
//! definition ends at its first NUL, as PROJ takes it as a C string, so a NUL in a keyword is
//! refused; text after a complete WKT, NUL or not, was already ignored. A NUL a PROJJSON name gives
//! (`\u0000`) is part of the name, as in PROJ: EPSG:32631's PROJJSON with one is still read. A
//! quote in a value runs to the next quote, delimiters included, as in PROJ (`b"c,d"e` is one
//! value), and a quote left unclosed refuses the WKT. GeoTIFF's GeoAsciiParamsTag ends at its first
//! NUL, as GDAL 3.12.1 reads it, and a CRS with a NUL in a name is written to GeoKeys that read
//! back as it, or refused where GDAL refuses it.
//!
//! `data/wkt1_proj_reads.json` (`scripts/gen_wkt1_proj_reads.py`, pyproj 3.8.0, no grids,
//! `PROJ_NETWORK=OFF`) holds, for each WKT, a point in its geographic CRS, the point in the
//! WKT's CRS by `Transformer.from_crs(crs.geodetic_crs, crs, always_xy=True)`, and
//! `CRS.from_wkt(wkt).to_proj4()` and `CRS.to_epsg()`; Makassar / NEIEZ in WGS 84 by
//! `Transformer.from_crs(crs, "EPSG:4326", always_xy=True)` and its `CRS.to_epsg()`; the WKT
//! `CRS.from_wkt` refuses; and the WKT `Transformer.from_crs(crs.geodetic_crs, crs)` and
//! `CRS.to_proj4()` refuse; and EPSG:32631's PROJJSON with a `\u0000` in a name, with its
//! `CRS.to_epsg()` and a point by `Transformer.from_crs("EPSG:4326", crs, always_xy=True)`.
//! Tolerance: 1e-3 in the CRS's unit for projected coordinates (1 mm, or 0.3 mm in US survey
//! feet), 1e-9 degrees for geographic ones, 1e-8 degrees (about 1 mm) in WGS 84, and 1e-3 m for
//! the PROJJSON's UTM coordinates; PROJ strings have PROJ's parameters, their values within a
//! relative 1e-12.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

fn data() -> Value {
    serde_json::from_str(include_str!("data/wkt1_proj_reads.json")).unwrap()
}

#[test]
fn wkt_read_by_proj_places_points_as_proj_does() {
    offline();
    let data = data();
    let cases = data["read"].as_array().unwrap();
    assert_eq!(cases.len(), 36);
    let mut bad = vec![];
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let wkt = case["wkt"].as_str().unwrap();
        let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
        if terrio_crs::wkt_proj(wkt).is_none() {
            bad.push(format!("{name}: wkt_proj reads nothing"));
            continue;
        }
        let crs = SpatialRef::from_wkt(wkt);
        let Some(geog) = crs.geographic_wkt().map(|w| SpatialRef::from_wkt(&w)) else {
            bad.push(format!("{name}: no geographic CRS"));
            continue;
        };
        let got = Transformer::new(&geog, &crs).and_then(|t| t.transform(num(&case["point"], 0), num(&case["point"], 1)));
        let proj = (num(&case["proj"], 0), num(&case["proj"], 1));
        let tolerance = if crs.is_geographic() { 1e-9 } else { 1e-3 };
        match got {
            Ok(got) if (got.0 - proj.0).abs() < tolerance && (got.1 - proj.1).abs() < tolerance => {}
            got => bad.push(format!("{name}: {got:?}, PROJ {proj:?}")),
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Whether two PROJ strings have the same parameters, their values within a relative 1e-12:
/// PROJ writes the false easting of the US survey feet case as 1188722.37744476, terrio as
/// 1188722.37744475 (3,900,000 m is 1,188,722.377444755 US survey feet).
///
/// In any order: terrio writes LCC's parallels before its origin (`+lat_1 +lat_2 +lat_0`), PROJ
/// after it, which changes nothing.
fn same_proj(a: &str, b: &str) -> bool {
    fn key(t: &str) -> &str {
        t.split_once('=').map_or(t, |(k, _)| k)
    }
    let (mut a, mut b): (Vec<&str>, Vec<&str>) = (a.split_whitespace().collect(), b.split_whitespace().collect());
    a.sort_by(|x, y| key(x).cmp(key(y)));
    b.sort_by(|x, y| key(x).cmp(key(y)));
    a.len() == b.len()
        && a.iter().zip(&b).all(|(a, b)| {
            let (Some((ka, va)), Some((kb, vb))) = (a.split_once('='), b.split_once('=')) else { return a == b };
            ka == kb
                && match (va.parse::<f64>(), vb.parse::<f64>()) {
                    (Ok(x), Ok(y)) => (x - y).abs() <= 1e-12 * x.abs().max(y.abs()),
                    _ => va == vb,
                }
        })
}

/// The PROJ string is PROJ's.
#[test]
fn wkt_read_by_proj_exports_proj_strings_as_proj_does() {
    offline();
    let data = data();
    let mut bad = vec![];
    for case in data["read"].as_array().unwrap() {
        let got = SpatialRef::from_wkt(case["wkt"].as_str().unwrap()).to_proj4();
        if !got.as_deref().is_some_and(|got| same_proj(got, case["proj4"].as_str().unwrap())) {
            bad.push(format!("{}: {got:?}, PROJ {}", case["name"], case["proj4"]));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn wkt_refused_by_proj_is_refused() {
    offline();
    let data = data();
    let cases = data["refused"].as_array().unwrap();
    assert_eq!(cases.len(), 6);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for case in cases {
        let wkt = case["wkt"].as_str().unwrap();
        assert_eq!(terrio_crs::wkt_proj(wkt), None, "{}", case["name"]);
        assert!(SpatialRef::parse(wkt).is_err(), "{}", case["name"]);
        assert!(Transformer::new(&SpatialRef::from_wkt(wkt), &wgs84).is_err(), "{}", case["name"]);
    }
}

/// Makassar / NEIEZ given by ESRI's `.prj` is EPSG:3002, and goes to WGS 84 by Makassar's
/// transformation, as in PROJ.
#[test]
fn makassar_neiez_goes_to_wgs84_as_in_proj() {
    offline();
    let data = data();
    let case = &data["to_wgs84"];
    let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
    assert_eq!(crs.epsg.map(u64::from), case["epsg"].as_u64());
    let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
    let t = Transformer::new(&crs, &SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let got = t.transform(num(&case["point"], 0), num(&case["point"], 1)).unwrap();
    let proj = (num(&case["proj"], 0), num(&case["proj"], 1));
    // 1e-8 degrees is about a millimetre.
    assert!((got.0 - proj.0).abs() < 1e-8 && (got.1 - proj.1).abs() < 1e-8, "{got:?}, PROJ {proj:?}");
}

/// The EPSG code PROJ identifies is terrio's (NAD27 / UTM zone 17N with a node of a hyphenated
/// keyword is EPSG:26717). A CRS PROJ identifies as none may still be identified by terrio.
#[test]
fn wkt_read_by_proj_is_identified_as_proj_identifies_it() {
    offline();
    let data = data();
    let mut bad = vec![];
    for case in data["read"].as_array().unwrap().iter().filter(|c| !c["epsg"].is_null()) {
        let got = SpatialRef::from_wkt(case["wkt"].as_str().unwrap()).epsg;
        if got.map(u64::from) != case["epsg"].as_u64() {
            bad.push(format!("{}: {got:?}, PROJ {}", case["name"], case["epsg"]));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// EPSG's `Mercator (variant A)` with a latitude of origin other than 0 is refused: PROJ reads
/// it, but neither transforms it ("Input is not a transformation") nor writes it as a PROJ
/// string, as `+proj=merc` has no latitude of origin.
#[test]
fn mercator_variant_a_with_a_latitude_of_origin_is_refused_as_in_proj() {
    offline();
    let data = data();
    let cases = data["untransformed"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    for case in cases {
        let wkt = case["wkt"].as_str().unwrap();
        let crs = SpatialRef::from_wkt(wkt);
        assert_eq!(terrio_crs::wkt_proj(wkt), None, "{}", case["name"]);
        assert_eq!(crs.to_proj4(), None, "{}", case["name"]);
        let geog = SpatialRef::from_wkt(&crs.geographic_wkt().unwrap());
        assert!(Transformer::new(&geog, &crs).is_err(), "{}", case["name"]);
    }
}

/// A NUL (`\u0000`) in a PROJJSON name is part of the name: EPSG:32631's PROJJSON with one is
/// read as EPSG:32631 and places a point as PROJ does. A raw NUL ends the text, as PROJ 9.9.0's
/// `proj_create` reads it (checked through its C API: text after the NUL is ignored, and a NUL
/// inside the JSON leaves it unclosed and refused).
#[test]
fn a_nul_in_a_projjson_name_is_read_as_in_proj() {
    offline();
    let data = data();
    let case = &data["projjson"];
    let text = case["projjson"].as_str().unwrap();
    assert!(text.contains(r"Latitude of\u0000 natural origin"));
    let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
    let proj = (num(&case["proj"], 0), num(&case["proj"], 1));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for crs in [SpatialRef::from_projjson(text).unwrap(), SpatialRef::parse_literal(text).unwrap()] {
        assert_eq!(crs.epsg.map(u64::from), case["epsg"].as_u64());
        let got = Transformer::new(&wgs84, &crs).unwrap().transform(num(&case["point"], 0), num(&case["point"], 1)).unwrap();
        assert!((got.0 - proj.0).abs() < 1e-3 && (got.1 - proj.1).abs() < 1e-3, "{got:?}, PROJ {proj:?}");
    }
    let trailing = format!("{text}\0garbage");
    assert_eq!(SpatialRef::from_projjson(&trailing).unwrap().epsg, SpatialRef::from_projjson(text).unwrap().epsg);
    // A NUL after the 50th character, at a character boundary.
    let at = text.char_indices().nth(50).map_or(text.len(), |(i, _)| i);
    let inside = format!("{}\0{}", &text[..at], &text[at..]);
    assert!(SpatialRef::from_projjson(&inside).is_err());
}

/// GeoAsciiParamsTag ends at its first NUL, as GDAL 3.12.1 reads it (rasterio 1.5.0 on GeoTIFFs
/// tifffile wrote with these keys): an ESRI PE citation with a NUL in it, or after a citation
/// with a NUL, is lost and the CRS refused; a NUL after the citation leaves it whole. A key that
/// starts at or after the NUL leaves the GeoKeys without a CRS; one that spans it is cut there.
#[test]
fn geoascii_params_end_at_their_first_nul() {
    offline();
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let pe = r#"ESRI PE String = GEOGCS["NAD27",DATUM["North_American_Datum_1927",SPHEROID["Clarke 1866",6378206.4,294.978698213898]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
    // GTModelTypeGeoKey user-defined, and the citations in GeoAsciiParamsTag in this order.
    let read = |parts: &[(u16, String)]| {
        let (mut ascii, mut keys) = (String::new(), vec![[1024u16, 0, 1, 32767]]);
        for (id, text) in parts {
            let text = format!("{text}|");
            keys.push([*id, 34737, text.len() as u16, ascii.len() as u16]);
            ascii.push_str(&text);
        }
        keys.sort();
        let mut directory = vec![1u16, 1, 0, keys.len() as u16];
        directory.extend(keys.iter().flatten());
        let crs = terrio_crs::geokeys::read(&directory, &[], ascii.as_bytes()).crs;
        crs.is_some_and(|crs| Transformer::new(&crs, &wgs84).is_ok())
    };
    let name = |n: &str| (1026u16, n.to_string());
    let citation = |c: &str| (3073u16, c.to_string());
    assert!(read(&[name("NAD27"), citation(pe)]));
    assert!(!read(&[name("NAD27"), citation(&pe.replace(r#""NAD27""#, "\"NA\0D27\""))]));
    assert!(!read(&[name("NAD27"), citation(&pe.replace(",PRIMEM", ",a\0b[\"x\"],PRIMEM"))]));
    assert!(!read(&[name("NA\0D27"), citation(pe)]));
    assert!(read(&[citation(pe), name("NA\0D27")]));
    // A key that starts at or after the NUL leaves no CRS, where one that spans the NUL is cut
    // there: GDAL 3.12.1 on a UTM zone 13N ProjectionGeoKey (16013) with these citations.
    let utm = |ascii: &[u8], citations: &[[u16; 4]]| {
        let mut keys = vec![[1024u16, 0, 1, 1], [3072, 0, 1, 32767], [3074, 0, 1, 16013]];
        keys.extend_from_slice(citations);
        keys.sort();
        let mut directory = vec![1u16, 1, 0, keys.len() as u16];
        directory.extend(keys.iter().flatten());
        terrio_crs::geokeys::read(&directory, &[], ascii).crs
    };
    let twice = b"NAD27 / UTM zone 13N|\0NAD27 / UTM zone 13N|";
    assert_eq!(utm(twice, &[[3073, 34737, 21, 0], [1026, 34737, 21, 22]]), None);
    assert_eq!(utm(twice, &[[1026, 34737, 21, 22]]), None);
    let short = b"NAD27 / UTM zone 13N|\0abc|";
    assert_eq!(utm(short, &[[3073, 34737, 21, 0], [1026, 34737, 4, 21]]), None);
    assert!(utm(short, &[[3073, 34737, 26, 0]]).is_some_and(|c| c.name().as_deref() == Some("NAD27 / UTM zone 13N")));
    // libgeotiff's `ReadKey` boundary, with EPSG:32613 in ProjectedCSTypeGeoKey: the whole CRS,
    // code included, is dropped for a key past the text, as GDAL 3.12.1 drops it. The text is
    // `"NAD27|\0abc|"`, 6 bytes before the NUL, with a 3073 citation of `"NAD27|"` at 0.
    let code = |citations: &[[u16; 4]]| {
        let mut keys = vec![[1024u16, 0, 1, 1], [3072, 0, 1, 32613], [3073, 34737, 6, 0]];
        keys.extend_from_slice(citations);
        keys.sort();
        let mut directory = vec![1u16, 1, 0, keys.len() as u16];
        directory.extend(keys.iter().flatten());
        terrio_crs::geokeys::read(&directory, &[], b"NAD27|\0abc|").crs.and_then(|c| c.epsg)
    };
    // GDAL: 32613, as without the key.
    assert_eq!(code(&[]), Some(32613));
    // A key at the NUL of 1 byte (ending at the NUL), of none at it or inside: read, empty.
    for key in [[1026, 34737, 1, 6], [1026, 34737, 0, 6], [1026, 34737, 0, 2]] {
        assert_eq!(code(&[key]), Some(32613), "{key:?}");
    }
    // A key spanning the NUL (3073 of 10 bytes), or ending one past it (7): cut there.
    for count in [10, 7] {
        let mut keys = [[1024u16, 0, 1, 1], [3072, 0, 1, 32613], [3073, 34737, count, 0]];
        keys.sort();
        let mut directory = vec![1u16, 1, 0, keys.len() as u16];
        directory.extend(keys.iter().flatten());
        assert_eq!(terrio_crs::geokeys::read(&directory, &[], b"NAD27|\0abc|").crs.and_then(|c| c.epsg), Some(32613), "{count}");
    }
    // GDAL: no CRS. A key at the NUL of 2 bytes, after it, or of none past the text.
    for key in [[1026, 34737, 2, 6], [1026, 34737, 4, 7], [1026, 34737, 0, 9]] {
        assert_eq!(code(&[key]), None, "{key:?}");
    }
}

/// GeoAsciiParamsTag's keys are read at their byte offsets, each decoded on its own, a byte that
/// isn't UTF-8 (Windows-1252) becoming U+FFFD in it: GDAL 3.12.1 reads such an ESRI PE citation
/// and such a GeogCitationGeoKey of WGS 84 in grads (rasterio 1.5.0 opens both, with the raw byte
/// in the WKT; GDAL 3.10.3's `gdalsrsinfo` shows the PE string's Bogota and the citation's
/// grads). A missing tag
/// fails every ASCII key, as GDAL 3.12.1 gives no CRS then.
#[test]
fn geoascii_params_are_read_per_key_and_need_the_tag() {
    offline();
    let directory = |keys: &[[u16; 4]]| {
        let mut keys = keys.to_vec();
        keys.sort();
        let mut d = vec![1u16, 1, 0, keys.len() as u16];
        d.extend(keys.iter().flatten());
        d
    };
    let pe = b"ESRI PE String = PROJCS[\"Bogot\xe1 / x\",GEOGCS[\"GCS_Bogota\",DATUM[\"D_Bogota\",SPHEROID[\"International_1924\",6378388.0,297.0]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Transverse_Mercator\"],PARAMETER[\"False_Easting\",1000000.0],PARAMETER[\"False_Northing\",1000000.0],PARAMETER[\"Central_Meridian\",-74.08091666666667],PARAMETER[\"Scale_Factor\",1.0],PARAMETER[\"Latitude_Of_Origin\",4.599047222222222],UNIT[\"Meter\",1.0]]|";
    let crs = terrio_crs::geokeys::read(&directory(&[[1024, 0, 1, 32767], [3073, 34737, pe.len() as u16, 0]]), &[], pe).crs.unwrap();
    // The PE string is the CRS (terrio names it by the code it identifies, EPSG's Bogota zone).
    assert!(crs.wkt.as_deref().is_some_and(|w| w.starts_with("PROJCS[\"Bogot\u{fffd} / x\"")), "{:?}", crs.wkt);
    assert!(crs.to_proj4().is_some_and(|p| p.contains("+lon_0=-74.0809166666667") && p.contains("+x_0=1000000")), "{:?}", crs.to_proj4());
    // WGS 84 in grads, as GDAL writes it, its citation's GCS name in Windows-1252.
    let citation = b"GCS Name = R\xe9seau|Ellipsoid = WGS 84|Primem = Greenwich|AUnits = grad||";
    let keys = [
        [1024, 0, 1, 2],
        [2048, 0, 1, 32767],
        [2049, 34737, citation.len() as u16, 0],
        [2050, 0, 1, 6326],
        [2054, 0, 1, 9105],
        [2057, 34736, 1, 0],
        [2059, 34736, 1, 1],
    ];
    let crs = terrio_crs::geokeys::read(&directory(&keys), &[6378137.0, 298.257223563], citation).crs.unwrap();
    let wkt = crs.to_wkt().unwrap();
    assert!(wkt.contains("UNIT[\"grad\"") && wkt.starts_with("GEOGCS[\"R\u{fffd}seau\""), "{wkt}");
    // No GeoAsciiParamsTag, with EPSG:32613 in ProjectedCSTypeGeoKey: a key of one byte or of
    // none in it leaves no CRS; without it, the code.
    let utm = [[1024u16, 0, 1, 1], [3072, 0, 1, 32613]];
    assert_eq!(terrio_crs::geokeys::read(&directory(&utm), &[], b"").crs.and_then(|c| c.epsg), Some(32613));
    for count in [1, 0] {
        let keys = [utm[0], utm[1], [1026, 34737, count, 0]];
        assert_eq!(terrio_crs::geokeys::read(&directory(&keys), &[], b"").crs, None, "{count}");
    }
}

/// A citation's name ends at its first NUL too: ESRI:102003's GeoKeys with text after a NUL in
/// its citation read as without it.
#[test]
fn a_citation_name_ends_at_its_first_nul() {
    offline();
    let keys = SpatialRef::from_esri(102003).unwrap().to_geokeys().unwrap();
    let read = |junk: &str| {
        let mut keys = keys.clone();
        for (_, text) in &mut keys.ascii {
            text.push_str(junk);
        }
        let tags = keys.encode_with_minor_revision(1).unwrap();
        let crs = terrio_crs::geokeys::read(&tags.directory, &tags.doubles, tags.ascii.as_bytes()).crs.unwrap();
        (crs.name(), crs.wkt)
    };
    let plain = read("");
    assert_eq!(plain.0.as_deref(), Some("USA_Contiguous_Albers_Equal_Area_Conic"));
    assert_eq!(read("\0junk"), plain);
}

/// A CRS with a NUL in a name (from PROJJSON) is written to GeoKeys that read back as it, its
/// names cut at the NUL: by its code (EPSG:32631), or as a user-defined CRS (UTM 32N with
/// `+towgs84`). One GeoKeys can't describe (Mollweide) would need its WKT in an ESRI PE citation,
/// which would end at the NUL: it is refused, as GDAL 3.12.1 refuses to write such a CRS
/// (rasterio 1.5.0: "missing , or ]"), PROJ's WKT of it ending at the NUL.
#[test]
fn a_crs_with_a_nul_in_a_name_round_trips_through_geokeys() {
    offline();
    let data = data();
    let round_trip = |crs: &SpatialRef| {
        let keys = crs.to_geokeys().unwrap();
        assert!(keys.ascii.iter().all(|(_, text)| !text.contains('\0')), "{:?}", keys.ascii);
        let tags = keys.encode_with_minor_revision(1).unwrap();
        terrio_crs::geokeys::read(&tags.directory, &tags.doubles, tags.ascii.as_bytes()).crs.unwrap()
    };
    let utm31 = SpatialRef::from_projjson(data["projjson"]["projjson"].as_str().unwrap()).unwrap();
    assert_eq!(round_trip(&utm31).epsg, Some(32631));
    let bound = SpatialRef::from_projjson(data["bound_projjson"]["projjson"].as_str().unwrap()).unwrap();
    assert!(bound.wkt.as_deref().is_some_and(|w| w.contains('\0')));
    let back = round_trip(&bound).to_proj4().unwrap();
    let proj4 = data["bound_projjson"]["proj4"].as_str().unwrap();
    assert!(same_proj(&back, proj4), "{back}, PROJ {proj4}");
    let mollweide = SpatialRef::from_projjson(data["mollweide_projjson"]["projjson"].as_str().unwrap()).unwrap();
    assert!(same_proj(&mollweide.to_proj4().unwrap(), data["mollweide_projjson"]["proj4"].as_str().unwrap()));
    assert!(mollweide.to_geokeys().is_err());
}

/// A bound CRS read from PROJJSON with a NUL (`\u0000`) in its name keeps its shift wherever
/// terrio reads its stored WKT: its PROJ string is PROJ's (`+towgs84` included), it places a
/// point in WGS 84 as PROJ does, and its GeoTIFF keys are written.
#[test]
fn a_bound_projjson_with_a_nul_in_its_name_keeps_its_shift() {
    offline();
    let data = data();
    let case = &data["bound_projjson"];
    let crs = SpatialRef::from_projjson(case["projjson"].as_str().unwrap()).unwrap();
    assert!(crs.wkt.as_deref().is_some_and(|w| w.contains("un\0known")));
    let proj4 = case["proj4"].as_str().unwrap();
    let got = crs.to_proj4().unwrap();
    assert!(same_proj(&got, proj4), "{got}, PROJ {proj4}");
    let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
    let t = Transformer::new(&crs, &SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let at = t.transform(num(&case["point"], 0), num(&case["point"], 1)).unwrap();
    let proj = (num(&case["proj"], 0), num(&case["proj"], 1));
    // 1e-8 degrees is about a millimetre.
    assert!((at.0 - proj.0).abs() < 1e-8 && (at.1 - proj.1).abs() < 1e-8, "{at:?}, PROJ {proj:?}");
    assert!(terrio_crs::geokeys::user_defined_keys(&crs, "bound").is_some());
}

/// A stored WKT 1 with `TOWGS84` and a NUL in its quoted name (as a PROJJSON name gives one) is
/// written to PROJJSON as the `BoundCRS` PROJ 9.9.0 writes for it, with the same shift.
#[test]
fn a_stored_wkt_with_a_nul_in_its_name_is_written_with_its_shift() {
    offline();
    let data = data();
    let case = &data["towgs84_projjson"];
    let wkt = case["wkt"].as_str().unwrap().replace(r#"GEOGCS["unknown""#, "GEOGCS[\"un\0known\"");
    assert!(wkt.contains('\0'));
    let crs = SpatialRef { wkt: Some(wkt), ..Default::default() };
    let written: Value = serde_json::from_str(&crs.to_projjson().unwrap()).unwrap();
    assert_eq!(written["type"], case["type"], "{written}");
    let got: Vec<f64> = written["transformation"]["parameters"].as_array().unwrap().iter().map(|p| p["value"].as_f64().unwrap()).collect();
    let proj: Vec<f64> = case["parameters"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    assert_eq!(got, proj);
}

/// `wkt_proj` and `identify_epsg` read a CRS's stored WKT whole, as `to_wkt` gives it: a NUL in a
/// name (from PROJJSON) stays part of it. EPSG:32631's PROJJSON with one gives the PROJ string
/// of PROJ's UTM zone 31N and is identified as EPSG:32631, as PROJ identifies it.
#[test]
fn stored_wkt_with_a_nul_in_a_name_is_read_whole() {
    offline();
    let data = data();
    let case = &data["projjson"];
    let wkt = SpatialRef::from_projjson(case["projjson"].as_str().unwrap()).unwrap().to_wkt().unwrap();
    assert!(wkt.contains('\0'));
    let proj = terrio_crs::wkt_proj(&wkt).unwrap();
    let utm = SpatialRef::from_epsg(32631).unwrap();
    assert!(terrio_crs::wkt_proj(&utm.to_wkt().unwrap()).is_some_and(|p| same_proj(&p, &proj)), "{proj}");
    assert_eq!(terrio_crs::identify_epsg(&wkt).map(u64::from), case["epsg"].as_u64());
}

/// A stored WKT with a NUL in a name is written for files with the name cut at the NUL, as PROJ
/// 9.9.0 gives such a name (`CRS.name` of PROJJSON's `"WGS\u0000 84 / UTM zone 31N"` is `WGS`):
/// a whole WKT that reads as the CRS. A doubled quote and a NUL outside quotes are handled; WKT
/// without a NUL is returned as it is.
#[test]
fn a_stored_wkt_is_written_for_files_with_names_cut_at_a_nul() {
    offline();
    let data = data();
    let crs = SpatialRef::from_projjson(data["projjson"]["projjson"].as_str().unwrap()).unwrap();
    let stored = crs.to_wkt().unwrap();
    let written = terrio_crs::wkt_for_files(&stored);
    assert!(!written.contains('\0') && written.contains(r#""Latitude of""#), "{written}");
    let back = SpatialRef::from_wkt(&written);
    assert_eq!(back.epsg, Some(32631));
    assert!(same_proj(&back.to_proj4().unwrap(), &crs.to_proj4().unwrap()));
    assert_eq!(terrio_crs::wkt_for_files("A[\"a\"\"b\0c\"\"d\",x]"), r#"A["a""b",x]"#);
    assert_eq!(terrio_crs::wkt_for_files("A[\"a\",\0x]"), "A[\"a\",x]");
    assert!(matches!(terrio_crs::wkt_for_files("A[\"a\"]"), std::borrow::Cow::Borrowed(_)));
    // A doubled quote after the cut is part of what is cut.
    assert_eq!(terrio_crs::wkt_for_files("A[\"ab\0c\"\"d\",x]"), r#"A["ab",x]"#);
    // A NUL right after the opening quote leaves an empty name, which reads: Mollweide's PROJ
    // 9.9.0 PROJJSON named `"\u0000weide"` is written `PROJCRS["",...]` and reads as Mollweide.
    let case = &data["mollweide_projjson"];
    let empty = case["projjson"].as_str().unwrap().replace(r"Moll\u0000weide", r"\u0000weide");
    let crs = SpatialRef::from_projjson(&empty).unwrap();
    let written = terrio_crs::wkt_for_files(crs.wkt.as_deref().unwrap()).into_owned();
    assert!(written.starts_with(r#"PROJCRS["","#), "{written}");
    let back = SpatialRef::from_wkt(&written);
    assert!(same_proj(&back.to_proj4().unwrap(), case["proj4"].as_str().unwrap()));
    assert!(Transformer::new(&back, &SpatialRef::from_epsg(4326).unwrap()).is_ok());
}

/// ESRI's `Lambert_Conformal_Conic` with one `Standard_Parallel_1` and no `Latitude_Of_Origin`,
/// which PROJ 9.9.0 reads at the equator but cannot transform, keeps terrio's reading: the
/// parallel as `+lat_1`.
#[test]
fn esri_lcc_with_one_parallel_and_no_origin_keeps_its_parallel() {
    let wkt = r#"PROJCS["l",GEOGCS["GCS_North_American_1927",DATUM["D_North_American_1927",SPHEROID["Clarke_1866",6378206.4,294.9786982]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic"],PARAMETER["False_Easting",1000000],PARAMETER["False_Northing",500000],PARAMETER["Central_Meridian",-96],PARAMETER["Standard_Parallel_1",42],PARAMETER["Scale_Factor",1.0],UNIT["Meter",1.0]]"#;
    let proj = SpatialRef::from_wkt(wkt).to_proj4().unwrap();
    assert!(proj.contains("+lat_1=42"), "{proj}");
}

/// GDAL's `Mercator_1SP` with a latitude of origin and a scale other than 1, which PROJ 9.9.0
/// reads but cannot transform, keeps terrio's reading: the scale, without the latitude.
#[test]
fn mercator_1sp_with_a_latitude_of_origin_and_a_scale_keeps_its_scale() {
    let wkt = r#"PROJCS["m",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Mercator_1SP"],PARAMETER["latitude_of_origin",30],PARAMETER["central_meridian",10],PARAMETER["scale_factor",0.99],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1]]"#;
    let proj = SpatialRef::from_wkt(wkt).to_proj4().unwrap();
    assert!(proj.contains("+k=0.99") && !proj.contains("lat_ts"), "{proj}");
}

/// Hotine variant A in WKT 2 (EPSG method 9812) with an azimuth and a rectified grid angle of 90° is
/// the Swiss oblique Mercator, as in WKT 1: PROJ 9.9.0 reads it as the same PROJ string as the
/// WKT 1 case `Hotine_Oblique_Mercator at 90° and 90°`, with the false origin at variant A's
/// natural origin. It places that case's point where PROJ does.
#[test]
fn hotine_variant_a_at_90_degrees_in_wkt2_is_the_swiss_oblique_mercator() {
    offline();
    let wkt2 = concat!(
        r#"PROJCRS["unknown",BASEGEOGCRS["NAD27",DATUM["North American Datum 1927",ELLIPSOID["Clarke 1866",6378206.4,294.978698213898,LENGTHUNIT["metre",1]]],"#,
        r#"PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["unknown",METHOD["Hotine Oblique Mercator (variant A)",ID["EPSG",9812]],"#,
        r#"PARAMETER["Latitude of projection centre",43,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8811]],"#,
        r#"PARAMETER["Longitude of projection centre",-80,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8812]],"#,
        r#"PARAMETER["Azimuth at projection centre",90,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8813]],"#,
        r#"PARAMETER["Angle from Rectified to Skew Grid",90,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8814]],"#,
        r#"PARAMETER["Scale factor at projection centre",0.9999,SCALEUNIT["unity",1],ID["EPSG",8815]],"#,
        r#"PARAMETER["False easting",500000,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",200000,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
        r#"CS[Cartesian,2],AXIS["(E)",east,ORDER[1],LENGTHUNIT["metre",1]],AXIS["(N)",north,ORDER[2],LENGTHUNIT["metre",1]]]"#
    );
    let data = data();
    let case = data["read"].as_array().unwrap().iter().find(|c| c["name"] == "Hotine_Oblique_Mercator at 90° and 90°").unwrap();
    let crs = SpatialRef::from_wkt(wkt2);
    let proj = crs.to_proj4().unwrap();
    assert!(same_proj(&proj, case["proj4"].as_str().unwrap()), "{proj}");
    let geog = SpatialRef::from_wkt(case["wkt"].as_str().unwrap()).geographic_wkt().map(|w| SpatialRef::from_wkt(&w)).unwrap();
    let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
    let (x, y) = Transformer::new(&geog, &crs).unwrap().transform(num(&case["point"], 0), num(&case["point"], 1)).unwrap();
    assert!((x - num(&case["proj"], 0)).abs() < 1e-3 && (y - num(&case["proj"], 1)).abs() < 1e-3, "({x}, {y})");
}
