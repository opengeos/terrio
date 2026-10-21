//! The CRS of LAS files: OGC WKT (`LASF_Projection` record 2112) or GeoTIFF GeoKeys (records
//! 34735, 34736 and 34737), read and written.

use super::las::{LASF_PROJECTION, LasHeader, Vlr};
use crate::crs::SpatialRef;

/// The WKT bit of the global encoding.
pub(crate) const WKT_BIT: u16 = 1 << 4;

/// Where a LAS file's CRS comes from.
#[derive(Debug, Clone, Default)]
pub(crate) struct LasCrs {
    /// The horizontal CRS.
    pub horizontal: SpatialRef,
    /// The EPSG code of the vertical CRS, when the file declares one.
    pub vertical_epsg: Option<u32>,
    /// The name of the vertical CRS.
    pub vertical_name: Option<String>,
}

/// The CRS of a LAS file. The WKT record wins when the global encoding's WKT bit is set (as LAS
/// 1.4 requires for formats 6 to 10) or when there are no GeoKeys; otherwise the GeoKeys do.
pub(crate) fn header_crs(h: &LasHeader) -> LasCrs {
    let wkt = h.find_vlr(LASF_PROJECTION, 2112).map(|v| super::las::c_str(&v.data)).filter(|w| !w.trim().is_empty());
    let keys = h.find_vlr(LASF_PROJECTION, 34735).map(|v| shorts(&v.data)).filter(|k| k.len() >= 4);
    let use_wkt = wkt.is_some() && (h.global_encoding & WKT_BIT != 0 || keys.is_none());
    if use_wkt && let Some(w) = wkt {
        return wkt_crs(&w);
    }
    match keys {
        Some(keys) => {
            let doubles: Option<Vec<f64>> =
                h.find_vlr(LASF_PROJECTION, 34736).map(|v| v.data.as_chunks::<8>().0.iter().map(|c| f64::from_le_bytes(*c)).collect());
            let ascii = h.find_vlr(LASF_PROJECTION, 34737).map(|v| String::from_utf8_lossy(&v.data).replace('\0', ""));
            let info = crate::geotiff::crs_from_geokeys(&keys, doubles.as_deref(), ascii.as_deref());
            // A user-defined CRS is kept as its PROJ string: the WKT made from it loses a foot
            // unit, and the EPSG code identified from that WKT would be the metre one.
            let horizontal = match (&info.epsg, &info.proj4) {
                (None, Some(p)) => SpatialRef { proj4: Some(p.clone()), ..Default::default() },
                _ => SpatialRef::from_raster_crs(&info),
            };
            let vertical_epsg = keys
                .get(4..)
                .and_then(|k| k.as_chunks::<4>().0.iter().find(|e| e[0] == 4096 && e[1] == 0))
                .map(|e| e[3] as u32)
                .filter(|c| *c != 0 && *c != 32767);
            let vertical_name = vertical_epsg.and_then(wbprojection::epsg::epsg_info).map(|i| i.name.to_string());
            LasCrs { horizontal, vertical_epsg, vertical_name }
        }
        None => LasCrs::default(),
    }
}

/// The CRS of a WKT definition, compound ones (and the projected CRSs with a vertical CRS inside
/// that some writers make) split into their horizontal and vertical parts.
fn wkt_crs(wkt: &str) -> LasCrs {
    let trimmed = wkt.trim();
    let vertical = ["VERT_CS[", "VERTCS[", "VERTCRS["].iter().find_map(|k| bracketed(trimmed, k));
    let (vertical_epsg, vertical_name) = match vertical {
        Some(v) => (last_authority_code(v), quoted_name(v)),
        None => (None, None),
    };
    let horizontal = if trimmed.starts_with("COMPD_CS") || trimmed.starts_with("COMPOUNDCRS") {
        ["PROJCS[", "GEOGCS[", "PROJCRS[", "GEOGCRS[", "GEODCRS["].iter().find_map(|k| bracketed(trimmed, k)).map(str::to_string)
    } else {
        Some(match vertical {
            Some(v) => trimmed.replacen(&format!(",{v}"), "", 1).replacen(v, "", 1),
            None => trimmed.to_string(),
        })
    };
    LasCrs { horizontal: horizontal.map(|h| SpatialRef::from_wkt(&h)).unwrap_or_default(), vertical_epsg, vertical_name }
}

/// The bracketed element starting with `keyword` (such as `PROJCS[`), brackets included.
fn bracketed<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let start = s.find(keyword)?;
    let mut depth = 0;
    for (i, c) in s[start..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The code of the last `AUTHORITY["EPSG","code"]` (or `ID["EPSG",code]`) of an element: the one
/// of the element itself.
fn last_authority_code(s: &str) -> Option<u32> {
    let i = s.rfind("AUTHORITY[\"EPSG\"").or_else(|| s.rfind("ID[\"EPSG\""))?;
    let rest = &s[i..];
    let digits: String = rest.split(',').nth(1)?.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn quoted_name(s: &str) -> Option<String> {
    let start = s.find('"')? + 1;
    let end = start + s[start..].find('"')?;
    Some(s[start..end].to_string())
}

fn shorts(b: &[u8]) -> Vec<u16> {
    b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()
}

/// The records that describe a CRS: an OGC WKT record when `wkt` is true (LAS 1.4), GeoKeys
/// otherwise (falling back to WKT for a CRS GeoKeys cannot describe).
pub(crate) fn crs_vlrs(crs: &SpatialRef, wkt: bool) -> (Vec<Vlr>, bool) {
    if crs.is_unknown() {
        return (Vec::new(), false);
    }
    if !wkt && let Some(v) = geokey_vlrs(crs) {
        return (v, false);
    }
    match crs.to_wkt() {
        Some(w) => {
            let mut data = w.into_bytes();
            data.push(0);
            (vec![Vlr::new(LASF_PROJECTION, 2112, "OGC WKT", data)], true)
        }
        None => (Vec::new(), false),
    }
}

fn geokey_vlrs(crs: &SpatialRef) -> Option<Vec<Vlr>> {
    let mut v = crate::drivers::geokeys::GeoKeyValues::default();
    let geographic = crs.is_geographic();
    v.shorts.push((1025, 1));
    match crs.epsg.and_then(|c| u16::try_from(c).ok()) {
        Some(code) => {
            v.shorts.push((1024, if geographic { 2 } else { 1 }));
            v.shorts.push((if geographic { 2048 } else { 3072 }, code));
        }
        None => {
            let name = crs.name().unwrap_or_else(|| "unnamed".into());
            let user = crate::drivers::geokeys::user_keys(crs, &name)?;
            v.shorts.extend(user.shorts);
            v.doubles.extend(user.doubles);
            v.ascii.extend(user.ascii);
        }
    }
    let mut keys: Vec<[u16; 4]> = v.shorts.iter().map(|(id, val)| [*id, 0, 1, *val]).collect();
    keys.extend(v.double_keys());
    let mut ascii = String::new();
    for (id, text) in &v.ascii {
        let text = format!("{text}|");
        keys.push([*id, 34737, text.len() as u16, ascii.len() as u16]);
        ascii.push_str(&text);
    }
    keys.sort_by_key(|k| k[0]);
    let mut dir = vec![1u16, 1, 0, keys.len() as u16];
    dir.extend(keys.iter().flatten());
    let mut out = vec![Vlr::new(LASF_PROJECTION, 34735, "GeoTIFF GeoKeyDirectoryTag", dir.iter().flat_map(|k| k.to_le_bytes()).collect())];
    if !v.doubles.is_empty() {
        out.push(Vlr::new(
            LASF_PROJECTION,
            34736,
            "GeoTIFF GeoDoubleParamsTag",
            v.doubles.iter().flat_map(|d| d.1.to_le_bytes()).collect(),
        ));
    }
    if !ascii.is_empty() {
        let mut data = ascii.into_bytes();
        data.push(0);
        out.push(Vlr::new(LASF_PROJECTION, 34737, "GeoTIFF GeoAsciiParamsTag", data));
    }
    Some(out)
}

/// Whether a record describes the CRS.
pub(crate) fn is_crs_vlr(v: &Vlr) -> bool {
    v.user_id == LASF_PROJECTION && matches!(v.record_id, 2111 | 2112 | 34735 | 34736 | 34737)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_compound_wkt() {
        let w = r#"COMPD_CS["NAD83 / Oregon + NAVD88",PROJCS["NAD83 / Oregon LCC (m)",GEOGCS["NAD83",DATUM["North_American_Datum_1983",SPHEROID["GRS 1980",6378137,298.257222101]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic_2SP"],PARAMETER["latitude_of_origin",41.75],PARAMETER["central_meridian",-120.5],PARAMETER["standard_parallel_1",43],PARAMETER["standard_parallel_2",45.5],PARAMETER["false_easting",400000],PARAMETER["false_northing",0],UNIT["metre",1]],VERT_CS["NAVD88 height (ftUS)",VERT_DATUM["North American Vertical Datum 1988",2005,AUTHORITY["EPSG","5103"]],UNIT["US survey foot",0.304800609601219,AUTHORITY["EPSG","9003"]],AXIS["Gravity-related height",UP],AUTHORITY["EPSG","6360"]]]"#;
        let c = wkt_crs(w);
        assert!(c.horizontal.wkt.as_deref().unwrap().starts_with("PROJCS[\"NAD83 / Oregon LCC (m)\""));
        assert_eq!(c.vertical_epsg, Some(6360));
        assert_eq!(c.vertical_name.as_deref(), Some("NAVD88 height (ftUS)"));
    }

    #[test]
    fn splits_a_vertical_crs_inside_a_projected_one() {
        let w = r#"PROJCS["NAD83(HARN) / New Mexico Central (ftUS)",GEOGCS["NAD83(HARN)",DATUM["NAD83_High_Accuracy_Reference_Network",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6152"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4152"]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",31],PARAMETER["central_meridian",-106.25],PARAMETER["scale_factor",0.9999],PARAMETER["false_easting",1640416.667],PARAMETER["false_northing",0],UNIT["US survey foot",0.3048006096012192,AUTHORITY["EPSG","9003"]],AXIS["X",EAST],AXIS["Y",NORTH],AUTHORITY["EPSG","2903"],VERTCS["North American Vertical Datum of 1988",VERT_DATUM["North American Vertical Datum 1988",2005,AUTHORITY["EPSG","5103"]],UNIT["US survey foot",1.0],AXIS["Gravity-related height",UP],AUTHORITY["EPSG","5703"]]]"#;
        let c = wkt_crs(w);
        assert_eq!(c.horizontal.epsg, Some(2903));
        assert_eq!(c.vertical_epsg, Some(5703));
    }

    #[test]
    fn geokeys_round_trip() {
        let crs = SpatialRef::from_epsg(26915).unwrap();
        let (vlrs, wkt) = crs_vlrs(&crs, false);
        assert!(!wkt);
        let h = LasHeader { vlrs, version: (1, 2), ..Default::default() };
        assert_eq!(header_crs(&h).horizontal.epsg, Some(26915));
    }
}
