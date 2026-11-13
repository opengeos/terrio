//! `SpatialRef::from_geokeys` and `SpatialRef::to_geokeys` against GDAL 3.12.1 (rasterio 1.5.0),
//! frozen in `data/geokeys_gdal.tsv` by `scripts/gen_geokeys_gdal.py`: the GeoKeys GDAL writes
//! for a CRS (`GTIFSetFromOGISDefnEx`) and the CRS GDAL reads from them (`GTIFGetOGISDefnAsOSR`),
//! for every 20th EPSG projected and geographic 2D code, the codes behind each rule, PROJ strings
//! of every projection method GeoTIFF names in other units and on other datums, ESRI codes,
//! compound and geographic 3D CRSs.
//!
//! - Reading: terrio reads GDAL's GeoKeys as the CRS GDAL reads, by its EPSG code when GDAL's
//!   has one, else as the same CRS or one whose coordinates are its own (two points near the
//!   origin, within 1e-9° or a millionth of their distance from it).
//! - Writing: terrio's GeoKeys of the CRS hold GDAL's model type, EPSG codes (projected,
//!   geographic, vertical), projection method and linear unit, and GDAL's projection parameters
//!   (GeoDoubleParams 3078-3096) and a user-defined unit's size within 1e-9. GDAL also writes keys
//!   the codes imply (citations, the angular unit, the ellipsoid's axes), which terrio leaves out,
//!   as GeoTIFF 1.1 allows. And terrio reads its own GeoKeys back as the CRS.
//!
//! Known differences, listed in [`READ_OTHERWISE`] and [`WRITTEN_OTHERWISE`].

use terrio_crs::{SpatialRef, Transformer};

struct Row {
    definition: String,
    directory: Vec<u16>,
    doubles: Vec<f64>,
    ascii: String,
    gdal: String,
}

fn rows() -> Vec<Row> {
    include_str!("data/geokeys_gdal.tsv")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let list = |s: &str| s.split(',').filter(|v| !v.is_empty()).map(|v| v.parse().unwrap()).collect();
            Row {
                definition: f[1].to_string(),
                directory: f[2].split(',').filter(|v| !v.is_empty()).map(|v| v.parse().unwrap()).collect(),
                doubles: list(f[3]),
                ascii: serde_json::from_str(f[4]).unwrap(),
                gdal: f[5].to_string(),
            }
        })
        .collect()
}

/// Definitions terrio reads otherwise than GDAL from GDAL's GeoKeys.
const READ_OTHERWISE: &[&str] = &[];

/// Definitions whose GeoKeys terrio writes otherwise than GDAL:
/// - ESRI:54030 (World Robinson) on a user-defined geographic CRS of WGS 84's ellipsoid, where
///   GDAL names EPSG:4326: the ESRI WKT's `GCS_WGS_1984` has no EPSG code terrio identifies.
/// - Miller on the authalic sphere (`+R_A`) as an ESRI PE citation, where GDAL writes the method:
///   terrio's WKT 1 of the PROJ string needs its PROJ4 extension.
const WRITTEN_OTHERWISE: &[&str] = &["ESRI:54030", "+proj=mill +lat_0=0 +lon_0=0 +x_0=0 +y_0=0 +R_A +datum=WGS84"];

/// The EPSG code at the root of GDAL's WKT: a WKT 1 `AUTHORITY`, or the `ID` of a WKT 2 (which
/// GDAL writes for a CRS WKT 1 cannot hold, a geographic 3D one).
fn root_code(wkt: &str) -> Option<u32> {
    if let Some(i) = wkt.rfind(",ID[\"EPSG\",") {
        let tail = &wkt[i + ",ID[\"EPSG\",".len()..];
        return tail.strip_suffix("]]").and_then(|c| c.parse().ok());
    }
    let tail = &wkt[wkt.rfind(",AUTHORITY[\"EPSG\",\"")?..];
    if tail.matches(']').count() != 2 {
        return None;
    }
    tail.trim_start_matches(",AUTHORITY[\"EPSG\",\"").split('"').next()?.parse().ok()
}

/// The false easting and northing of a projected CRS, in its unit.
fn false_origin(s: &SpatialRef) -> (f64, f64) {
    let p = s.to_proj4().unwrap_or_default();
    let get = |k: &str| p.split_whitespace().find_map(|t| t.strip_prefix(k)?.parse::<f64>().ok()).unwrap_or(0.0);
    (get("+x_0=") / s.metres_per_unit(), get("+y_0=") / s.metres_per_unit())
}

/// Whether `b` is `a`, or a CRS whose coordinates are `a`'s at two points near the origin (off
/// its false origin by a thousandth of the body's radius), within 1e-9° or a millionth of that.
fn equivalent(a: &SpatialRef, b: &SpatialRef) -> bool {
    // The same PROJ string (as PROJ writes it): a method terrio does not transform with (Van
    // der Grinten without the wbprojection fallback) is compared by it.
    if a.same_as(b) {
        return true;
    }
    // The points compare horizontal coordinates: a compound CRS must keep its vertical CRS, and a
    // geographic 3D CRS its height.
    if a.compound_parts() != b.compound_parts() || a.has_height() != b.has_height() {
        return false;
    }
    if a.to_proj4().is_some_and(|p| Some(p) == b.to_proj4()) {
        return true;
    }
    let Ok(t) = Transformer::new(a, b) else { return false };
    let r = a.to_wkt().as_deref().and_then(terrio_crs::semi_major_axis).unwrap_or(6_378_137.0) * 1e-3 / a.metres_per_unit();
    let (x0, y0) = false_origin(a);
    let (points, tolerance) = if a.is_geographic() {
        ([(10.0, 20.0), (-30.0, -40.0)], 1e-9)
    } else {
        ([(x0 + r, y0 - 2.0 * r), (x0 - 3.0 * r, y0 + 4.0 * r)], 1e-6 * r)
    };
    points.iter().all(|&(x, y)| t.transform(x, y).is_ok_and(|(u, v)| (u - x).abs() <= tolerance && (v - y).abs() <= tolerance))
}

/// The keys of a directory: `(id, location, count, value or offset)`.
fn entries(directory: &[u16]) -> Vec<[u16; 4]> {
    directory.get(4..).unwrap_or_default().chunks(4).filter(|k| k.len() == 4).map(|k| [k[0], k[1], k[2], k[3]]).collect()
}

/// The SHORT keys that define a CRS: model type, geographic and projected CRS codes, projection
/// method, linear unit and vertical CRS. A user-defined geographic CRS on an EPSG datum, the
/// Greenwich meridian and degrees (as GDAL writes a datum PROJ strings name) is given the code of
/// that datum's geographic CRS, which terrio writes; the linear unit of a projected CRS's code is
/// the code's, and a metre is no unit to say; a model type of the user's has no geographic CRS.
fn defining_shorts(directory: &[u16]) -> Vec<(u16, u16)> {
    let short = |id: u16| entries(directory).iter().find(|k| k[0] == id && k[1] == 0).map(|k| k[3]);
    let mut out: Vec<(u16, u16)> = entries(directory)
        .iter()
        .filter(|k| k[1] == 0 && matches!(k[0], 1024 | 2048 | 3072 | 3075 | 3076 | 4096))
        .map(|k| (k[0], k[3]))
        .collect();
    let datum = short(2050)
        .filter(|d| (6001..=6999).contains(d) && short(2051).is_none_or(|pm| pm == 8901) && short(2054).is_none_or(|u| u == 9102));
    for k in out.iter_mut() {
        if let ((2048, 32767), Some(d)) = (*k, datum) {
            k.1 = d - 2000;
        }
    }
    let coded = short(3072).is_some_and(|c| c != 32767);
    let users = short(1024) == Some(32767);
    out.retain(|&(id, v)| !(id == 3076 && (v == 9001 || coded)) && !(id == 2048 && users));
    out.sort();
    out
}

/// The linear unit's size (ProjLinearUnitSizeGeoKey, 3077) and projection parameters
/// (GeoDoubleParams 3078-3096) of GeoKeys, by key id: an angle or
/// offset not given is 0 (and a scale 1), as GeoTIFF readers take it.
fn parameters(directory: &[u16], doubles: &[f64]) -> Vec<(u16, f64)> {
    let mut out: Vec<(u16, f64)> = entries(directory)
        .iter()
        .filter(|k| k[1] == 34736 && (3077..=3096).contains(&k[0]))
        .filter_map(|k| Some((k[0], *doubles.get(k[3] as usize)?)))
        .filter(|&(id, v)| v != if matches!(id, 3092 | 3093) { 1.0 } else { 0.0 })
        .collect();
    out.sort_by_key(|p| p.0);
    out
}

/// Defining SHORT keys, and the unit size and projection parameters, by key id.
type Defining = (Vec<(u16, u16)>, Vec<(u16, f64)>);

/// The defining SHORT keys and projection parameters of GeoKeys, a UTM zone by its
/// ProjectionGeoKey (16001-16060 north, 16101-16160 south, as GDAL writes one) as the Transverse
/// Mercator it is (as terrio writes it).
fn defining(directory: &[u16], doubles: &[f64]) -> Defining {
    let (mut shorts, mut params) = (defining_shorts(directory), parameters(directory, doubles));
    let short = |id: u16| entries(directory).iter().find(|k| k[0] == id && k[1] == 0).map(|k| k[3]);
    let zone = match short(3074) {
        Some(p @ 16001..=16060) => Some((p - 16000, false)),
        Some(p @ 16101..=16160) => Some((p - 16100, true)),
        _ => None,
    };
    if let (Some((zone, south)), None) = (zone, short(3075)) {
        let unit = match short(3076) {
            Some(9002) => 0.3048,
            Some(9003) => 1200.0 / 3937.0,
            Some(9036) => 1000.0,
            _ => 1.0,
        };
        shorts.push((3075, 1));
        shorts.sort();
        params.extend([(3080, f64::from(zone) * 6.0 - 183.0), (3082, 500_000.0 / unit), (3092, 0.9996)]);
        if south {
            params.push((3083, 10_000_000.0 / unit));
        }
        params.sort_by_key(|p| p.0);
    }
    (shorts, params)
}

/// The CRS of a definition: `Ok(None)` for an EPSG code only the wbprojection fallback reads
/// (whose GeoKeys are an ESRI PE citation), an error for a definition terrio does not read.
fn crs(definition: &str) -> Result<Option<SpatialRef>, String> {
    if definition.starts_with('+') {
        return SpatialRef::from_proj4(definition).map(Some).map_err(|e| e.to_string());
    }
    let fallback = definition.strip_prefix("EPSG:").and_then(|c| c.parse().ok()).is_some_and(|c| terrio_crs::epsg_wkt(c).is_none());
    if fallback { Ok(None) } else { SpatialRef::parse(definition).map(Some).map_err(|e| e.to_string()) }
}

fn read_check(row: &Row) -> Option<String> {
    let gdal = SpatialRef::from_wkt(&row.gdal);
    let ours = match SpatialRef::from_geokeys(&row.directory, &row.doubles, &row.ascii) {
        Ok(s) => s,
        Err(e) => return Some(format!("unread: {e}")),
    };
    if let Some(code) = root_code(&row.gdal) {
        return (ours.epsg != Some(code) && !equivalent(&gdal, &ours)).then(|| format!("read as {:?}, GDAL EPSG:{code}", ours.label()));
    }
    (!equivalent(&gdal, &ours)).then(|| format!("read as {}\n    GDAL {}", ours.to_wkt().unwrap_or_default(), row.gdal))
}

fn write_check(row: &Row) -> Option<String> {
    let s = match crs(&row.definition) {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(e) => return Some(format!("unread definition: {e}")),
    };
    let keys = match s.to_geokeys() {
        Ok(k) => k,
        Err(e) => return Some(format!("no GeoKeys: {e}")),
    };
    let tags = match keys.encode() {
        Ok(t) => t,
        Err(e) => return Some(format!("GeoKeys not encoded: {e}")),
    };
    let ours = defining(&tags.directory, &tags.doubles);
    let gdal = defining(&row.directory, &row.doubles);
    let same_parameters = ours.1.len() == gdal.1.len()
        && ours.1.iter().zip(&gdal.1).all(|(a, b)| a.0 == b.0 && (a.1 - b.1).abs() <= 1e-9 * a.1.abs().max(1.0));
    if ours.0 != gdal.0 || !same_parameters {
        return Some(format!("terrio {ours:?}\n    GDAL   {gdal:?}"));
    }
    let back = match SpatialRef::from_geokeys(&tags.directory, &tags.doubles, &tags.ascii) {
        Ok(b) => b,
        Err(e) => return Some(format!("terrio does not read its GeoKeys back: {e}")),
    };
    (!equivalent(&s, &back)).then(|| format!("terrio reads its GeoKeys back as {}", back.to_wkt().unwrap_or_default()))
}

#[test]
fn geokeys_are_read_as_gdal_reads_them() {
    let failures: Vec<String> = rows()
        .iter()
        .filter(|r| !READ_OTHERWISE.contains(&r.definition.as_str()))
        .filter_map(|r| read_check(r).map(|e| format!("{}: {e}", r.definition)))
        .collect();
    assert!(failures.is_empty(), "{} of {}:\n{}", failures.len(), rows().len(), failures.join("\n"));
}

#[test]
fn only_codes_outside_the_tables_are_not_written() {
    let skipped: Vec<String> = rows().into_iter().filter(|r| matches!(crs(&r.definition), Ok(None))).map(|r| r.definition).collect();
    assert_eq!(skipped, ["EPSG:3053", "EPSG:32700"]);
}

#[test]
fn geokeys_are_written_as_gdal_writes_them() {
    let failures: Vec<String> = rows()
        .iter()
        .filter(|r| !WRITTEN_OTHERWISE.contains(&r.definition.as_str()))
        .filter_map(|r| write_check(r).map(|e| format!("{}: {e}", r.definition)))
        .collect();
    assert!(failures.is_empty(), "{} of {}:\n{}", failures.len(), rows().len(), failures.join("\n"));
}

#[test]
fn listed_differences_are_still_differences() {
    for r in rows() {
        if READ_OTHERWISE.contains(&r.definition.as_str()) {
            assert!(read_check(&r).is_some(), "{} is now read as GDAL reads it", r.definition);
        }
        if WRITTEN_OTHERWISE.contains(&r.definition.as_str()) {
            assert!(write_check(&r).is_some(), "{} is now written as GDAL writes it", r.definition);
        }
    }
}
