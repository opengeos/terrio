//! OGC WKT1 as GDAL writes it, so that GDAL (and PROJ) recognize the CRSs terrio writes into
//! text headers (`.prj` files, ENVI headers, Zarr and netCDF attributes).
//!
//! The registry's WKT names datums in a way PROJ does not match to EPSG datums; this writer uses
//! the EPSG names and authority codes for the common datums and GDAL's projection names.

use crate::crs::SpatialRef;
use std::fmt::Write as _;
use wbprojection::ProjectionKind as K;

/// `(registry datum name, WKT datum name, datum code, geographic CRS name, geographic CRS code,
/// ellipsoid name, ellipsoid code)` for the datums PROJ needs authority codes to recognize.
const DATUMS: &[(&str, &str, u32, &str, u32, &str, u32)] = &[
    ("WGS 84", "WGS_1984", 6326, "WGS 84", 4326, "WGS 84", 7030),
    ("NAD 83", "North_American_Datum_1983", 6269, "NAD83", 4269, "GRS 1980", 7019),
    ("NAD 27", "North_American_Datum_1927", 6267, "NAD27", 4267, "Clarke 1866", 7008),
    ("ETRS 89", "European_Terrestrial_Reference_System_1989", 6258, "ETRS89", 4258, "GRS 1980", 7019),
    ("ED 50", "European_Datum_1950", 6230, "ED50", 4230, "International 1924", 7022),
    ("GDA94", "Geocentric_Datum_of_Australia_1994", 6283, "GDA94", 4283, "GRS 1980", 7019),
    ("OSGB36", "Ordnance_Survey_of_Great_Britain_1936", 6277, "OSGB36", 4277, "Airy 1830", 7001),
];

fn num(v: f64) -> String {
    format!("{v}")
}

/// The CRS as GDAL-style OGC WKT1. For a CRS without an EPSG code, the stored WKT is returned
/// unchanged (it usually came from GDAL); `None` when nothing is known.
pub(crate) fn gdal_wkt(crs: &SpatialRef) -> Option<String> {
    // EPSG codes: the WKT GDAL writes, from terrio's EPSG table.
    if let Some(w) = crs.epsg.and_then(crate::crs::epsg_wkt) {
        return Some(w.to_string());
    }
    // A PROJ string: GDAL's WKT 1 of it, which terrio stores.
    if crs.epsg.is_none() && crs.proj4.is_some() {
        return crs.to_wkt();
    }
    let built = match crs.epsg {
        Some(code) => wbprojection::Crs::from_epsg(code).ok().and_then(|c| build(&c, Some(code))),
        None => crs.to_wbp().ok().and_then(|c| build(&c, None)),
    };
    built.or_else(|| crs.to_wkt())
}

/// Builds the WKT of a CRS from its parameters, with its EPSG code when it has one.
fn build(c: &wbprojection::Crs, code: Option<u32>) -> Option<String> {
    let info = code.and_then(wbprojection::epsg::epsg_info);
    let name = code.and_then(|k| SpatialRef::from_epsg(k).ok()).and_then(|s| s.name()).unwrap_or_else(|| c.name.clone());
    let p = c.projection.params();
    let e = &c.datum.ellipsoid;
    let rf = if e.f > 0.0 { 1.0 / e.f } else { 0.0 };
    let known = DATUMS.iter().find(|d| d.0 == c.datum.name);
    let mut geog = String::new();
    match known {
        Some((_, datum, dcode, gname, gcode, ename, ecode)) => {
            let _ = write!(
                geog,
                "GEOGCS[\"{gname}\",DATUM[\"{datum}\",SPHEROID[\"{ename}\",{},{},AUTHORITY[\"EPSG\",\"{ecode}\"]],AUTHORITY[\"EPSG\",\"{dcode}\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]]",
                num(e.a),
                num(rf)
            );
            let _ = write!(geog, ",AUTHORITY[\"EPSG\",\"{gcode}\"]]");
        }
        None => {
            let datum = c.datum.name.replace([' ', '-'], "_");
            let _ = write!(
                geog,
                "GEOGCS[\"{}\",DATUM[\"{datum}\",SPHEROID[\"{}\",{},{}]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
                c.datum.name,
                e.name,
                num(e.a),
                num(rf)
            );
        }
    }
    if matches!(p.kind, K::Geographic) {
        return match code {
            Some(code) if known.is_none_or(|d| d.4 != code) => {
                // A registered geographic CRS on a datum without an authority code here.
                let body = geog.trim_end_matches(']');
                Some(format!("{body},AUTHORITY[\"EPSG\",\"{code}\"]]"))
            }
            _ => Some(geog),
        };
    }

    // Linear units: the registry's parameters are in metres.
    let unit = info.as_ref().map(|i| i.unit.to_ascii_lowercase()).unwrap_or_default();
    let (unit_name, unit_m, unit_code) = if unit.contains("us survey foot") || unit.contains("foot_us") {
        ("US survey foot", 0.304800609601219, 9003)
    } else if unit.contains("foot") {
        ("foot", 0.3048, 9002)
    } else {
        ("metre", 1.0, 9001)
    };
    let (fe, fn_) = (p.false_easting / unit_m, p.false_northing / unit_m);
    let (lat0, lon0, k) = (p.lat0, p.lon0, p.scale);
    let mut extension = None;
    let (method, params): (&str, Vec<(&str, f64)>) = match &p.kind {
        K::TransverseMercator => (
            "Transverse_Mercator",
            vec![
                ("latitude_of_origin", lat0),
                ("central_meridian", lon0),
                ("scale_factor", k),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        K::Utm { zone, south } => (
            "Transverse_Mercator",
            vec![
                ("latitude_of_origin", 0.0),
                ("central_meridian", -183.0 + 6.0 * *zone as f64),
                ("scale_factor", 0.9996),
                ("false_easting", 500000.0),
                ("false_northing", if *south { 10_000_000.0 } else { 0.0 }),
            ],
        ),
        K::Mercator => {
            ("Mercator_1SP", vec![("central_meridian", lon0), ("scale_factor", k), ("false_easting", fe), ("false_northing", fn_)])
        }
        K::WebMercator => {
            extension =
                Some("+proj=merc +a=6378137 +b=6378137 +lat_ts=0 +lon_0=0 +x_0=0 +y_0=0 +k=1 +units=m +nadgrids=@null +wktext +no_defs");
            ("Mercator_1SP", vec![("central_meridian", 0.0), ("scale_factor", 1.0), ("false_easting", 0.0), ("false_northing", 0.0)])
        }
        K::LambertConformalConic { lat1, lat2: Some(lat2) } => (
            "Lambert_Conformal_Conic_2SP",
            vec![
                ("latitude_of_origin", lat0),
                ("central_meridian", lon0),
                ("standard_parallel_1", *lat1),
                ("standard_parallel_2", *lat2),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        K::LambertConformalConic { lat2: None, .. } => (
            "Lambert_Conformal_Conic_1SP",
            vec![
                ("latitude_of_origin", lat0),
                ("central_meridian", lon0),
                ("scale_factor", k),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        K::AlbersEqualAreaConic { lat1, lat2 } => (
            "Albers_Conic_Equal_Area",
            vec![
                ("latitude_of_center", lat0),
                ("longitude_of_center", lon0),
                ("standard_parallel_1", *lat1),
                ("standard_parallel_2", *lat2),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        K::LambertAzimuthalEqualArea => (
            "Lambert_Azimuthal_Equal_Area",
            vec![("latitude_of_center", lat0), ("longitude_of_center", lon0), ("false_easting", fe), ("false_northing", fn_)],
        ),
        K::AzimuthalEquidistant => (
            "Azimuthal_Equidistant",
            vec![("latitude_of_center", lat0), ("longitude_of_center", lon0), ("false_easting", fe), ("false_northing", fn_)],
        ),
        K::PolarStereographic { north, lat_ts } => (
            "Polar_Stereographic",
            vec![
                ("latitude_of_origin", lat_ts.unwrap_or(if *north { 90.0 } else { -90.0 })),
                ("central_meridian", lon0),
                ("scale_factor", k),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        K::Stereographic => (
            "Stereographic",
            vec![
                ("latitude_of_origin", lat0),
                ("central_meridian", lon0),
                ("scale_factor", k),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        K::ObliqueStereographic => (
            "Oblique_Stereographic",
            vec![
                ("latitude_of_origin", lat0),
                ("central_meridian", lon0),
                ("scale_factor", k),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        K::Equirectangular { lat_ts } => (
            "Equirectangular",
            vec![
                ("latitude_of_origin", lat0),
                ("central_meridian", lon0),
                ("standard_parallel_1", *lat_ts),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        K::Sinusoidal => ("Sinusoidal", vec![("longitude_of_center", lon0), ("false_easting", fe), ("false_northing", fn_)]),
        K::CylindricalEqualArea { lat_ts } => (
            "Cylindrical_Equal_Area",
            vec![("standard_parallel_1", *lat_ts), ("central_meridian", lon0), ("false_easting", fe), ("false_northing", fn_)],
        ),
        _ => return None,
    };
    let mut w = format!("PROJCS[\"{name}\",{geog},PROJECTION[\"{method}\"]");
    for (k, v) in params {
        let _ = write!(w, ",PARAMETER[\"{k}\",{}]", num(v));
    }
    let _ =
        write!(w, ",UNIT[\"{unit_name}\",{unit_m},AUTHORITY[\"EPSG\",\"{unit_code}\"]],AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]");
    if let Some(ext) = extension {
        let _ = write!(w, ",EXTENSION[\"PROJ4\",\"{ext}\"]");
    }
    if let Some(code) = code {
        let _ = write!(w, ",AUTHORITY[\"EPSG\",\"{code}\"]");
    }
    w.push(']');
    Some(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_epsg_names_and_codes() {
        let w = gdal_wkt(&SpatialRef::from_epsg(32611).unwrap()).unwrap();
        assert!(w.starts_with("PROJCS[\"WGS 84 / UTM zone 11N\",GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\""), "{w}");
        assert!(w.contains("PARAMETER[\"central_meridian\",-117]") && w.ends_with("AUTHORITY[\"EPSG\",\"32611\"]]"), "{w}");
        let g = gdal_wkt(&SpatialRef::from_epsg(4326).unwrap()).unwrap();
        assert!(g.starts_with("GEOGCS[\"WGS 84\"") && g.ends_with("AUTHORITY[\"EPSG\",\"4326\"]]"), "{g}");
    }
}
