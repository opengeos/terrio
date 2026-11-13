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

impl LasCrs {
    /// The CRS that `crs` (the horizontal CRS of the points, or the one assigned to them) stands
    /// for when reprojecting: compound, with the vertical CRS the file declares, when `crs` is the
    /// file's horizontal CRS (`EPSG:6588+6360` for `EPSG:6588` in a file declaring NAVD88 height
    /// (ftUS)). `crs` itself otherwise, and when the vertical CRS is not an EPSG one terrio knows
    /// or the horizontal CRS has only a PROJ string.
    pub(crate) fn with_vertical(&self, crs: &SpatialRef) -> SpatialRef {
        let Some(v) = self.vertical_epsg else { return crs.clone() };
        if crs.is_unknown() || crs.has_height() || !crs.same_as(&self.horizontal) {
            return crs.clone();
        }
        if let Some(code) = crs.epsg
            && let Ok(c) = SpatialRef::compound(code, v)
        {
            return c;
        }
        let vertical = SpatialRef::from_epsg(v).ok().and_then(|v| v.to_wkt()).filter(|w| w.starts_with("VERT_CS["));
        match (crs.proj4.is_none().then(|| crs.to_wkt()).flatten(), vertical) {
            (Some(h), Some(v)) => {
                let name = format!("{} + {}", crs.name().unwrap_or_default(), quoted_name(&v).unwrap_or_default());
                SpatialRef::from_wkt(&format!("COMPD_CS[\"{name}\",{h},{v}]"))
            }
            _ => crs.clone(),
        }
    }
}

/// The CRS of a WKT definition, compound ones (and the projected CRSs with a vertical CRS inside
/// that some writers make) split into their horizontal and vertical parts.
fn wkt_crs(wkt: &str) -> LasCrs {
    let trimmed = wkt.trim();
    let vertical = ["VERT_CS[", "VERTCS[", "VERTCRS["].iter().find_map(|k| bracketed(trimmed, k));
    let (vertical_epsg, vertical_name) = match vertical {
        Some(v) => (own_authority_code(v), quoted_name(v)),
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

/// The bracketed element starting with `keyword` (such as `PROJCS[`), brackets included. Brackets
/// inside quoted names do not count.
fn bracketed<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let start = s.find(keyword)?;
    let mut depth = 0;
    let mut quoted = false;
    for (i, c) in s[start..].char_indices() {
        match c {
            '"' => quoted = !quoted,
            _ if quoted => {}
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

/// The code of the element's own `AUTHORITY["EPSG","code"]` (or `ID["EPSG",code]`): one directly
/// inside it, not that of a datum or unit nested in it. Brackets and commas inside quoted names
/// do not count.
fn own_authority_code(s: &str) -> Option<u32> {
    let mut depth = 0;
    let mut quoted = false;
    let mut own = None;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            _ if quoted => {}
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' if depth == 1 => {
                let rest = s[i + 1..].trim_start();
                if rest.starts_with("AUTHORITY[\"EPSG\"") || rest.starts_with("ID[\"EPSG\"") {
                    own = Some(rest);
                }
            }
            _ => {}
        }
    }
    let rest = own?;
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
    // A compound CRS: its horizontal CRS, and its vertical CRS's EPSG code in
    // VerticalCSTypeGeoKey, as GDAL writes them in GeoTIFF (GDAL 3.13 writes 3072 = 6588 and
    // 4096 = 6360 for EPSG:6588+6360). A vertical CRS without one is left out (with a warning), so
    // that the GeoKeys still describe the horizontal CRS rather than falling back to WKT.
    let (horizontal, vertical) = match crs.to_wkt().filter(|w| w.trim_start().starts_with("COMPD_CS[")) {
        Some(w) => {
            let c = wkt_crs(&w);
            let vertical = c.vertical_epsg.and_then(|v| u16::try_from(v).ok());
            if vertical.is_none() {
                crate::warnings::warn(format!(
                    "the vertical CRS of {} has no EPSG code for GeoKeys: only the horizontal CRS is written",
                    crs.label()
                ));
            }
            (c.horizontal, vertical)
        }
        None => (crs.clone(), None),
    };
    let mut v = crate::crs::geokeys::GeoKeyValues::default();
    let geographic = horizontal.is_geographic();
    v.shorts.push((1025, 1));
    match horizontal.epsg.and_then(|c| u16::try_from(c).ok()) {
        Some(code) => {
            v.shorts.push((1024, if geographic { 2 } else { 1 }));
            v.shorts.push((if geographic { 2048 } else { 3072 }, code));
        }
        None => {
            let name = horizontal.name().unwrap_or_else(|| "unnamed".into());
            let user = horizontal.user_defined_geokeys(&name)?;
            v.shorts.extend(user.shorts);
            v.doubles.extend(user.doubles);
            v.ascii.extend(user.ascii);
        }
    }
    if let Some(code) = vertical {
        v.shorts.push((4096, code));
    }
    // KeyRevision 1.0 whatever the keys, as terrio has always written LAS GeoKeys: GeoTIFF 1.1's
    // minor revision for a vertical CRS is GDAL's choice for GeoTIFFs.
    let crate::crs::geokeys::GeoKeyTags { directory: dir, ascii, .. } = v.encode_with_minor_revision(0).ok()?;
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
    fn brackets_in_a_quoted_vertical_name_do_not_shift_the_nesting() {
        let w = r#"COMPD_CS["NAD83 / Oregon + depth",PROJCS["NAD83 / Oregon LCC (m)",GEOGCS["NAD83",UNIT["degree",0.0174532925199433]],UNIT["metre",1]],VERT_CS["Mean Sea Level [ft",VERT_DATUM["Mean Sea Level",2005,AUTHORITY["EPSG","5100"]],UNIT["US survey foot",0.304800609601219],AXIS["Gravity-related height",UP],AUTHORITY["EPSG","5714"]]]"#;
        let c = wkt_crs(w);
        assert_eq!(c.vertical_epsg, Some(5714));
        assert_eq!(c.vertical_name.as_deref(), Some("Mean Sea Level [ft"));
    }

    #[test]
    fn geokeys_round_trip() {
        let crs = SpatialRef::from_epsg(26915).unwrap();
        let (vlrs, wkt) = crs_vlrs(&crs, false);
        assert!(!wkt);
        let h = LasHeader { vlrs, version: (1, 2), ..Default::default() };
        assert_eq!(header_crs(&h).horizontal.epsg, Some(26915));
    }

    #[test]
    fn vertical_crs_code_is_its_own_not_its_datums() {
        let vertical = r#"VERT_CS["NAVD88 height",VERT_DATUM["North American Vertical Datum 1988",2005,AUTHORITY["EPSG","5103"]],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Up",UP]]"#;
        let horizontal = SpatialRef::from_epsg(26915).unwrap().to_wkt().unwrap();
        let c = wkt_crs(&format!("COMPD_CS[\"x\",{horizontal},{vertical}]"));
        assert_eq!(c.vertical_epsg, None);
        let named = vertical.replace(",AXIS[\"Up\",UP]]", r#",AXIS["Up",UP],AUTHORITY["EPSG","5703"]]"#);
        assert_eq!(wkt_crs(&format!("COMPD_CS[\"x\",{horizontal},{named}]")).vertical_epsg, Some(5703));
        // Written without key 4096, with a warning.
        let crs = SpatialRef::from_wkt(&format!("COMPD_CS[\"x\",{horizontal},{vertical}]"));
        let _ = crate::warnings::take();
        let (vlrs, _) = crs_vlrs(&crs, false);
        assert!(!shorts(&vlrs[0].data)[4..].as_chunks::<4>().0.iter().any(|k| k[0] == 4096));
        assert!(!crate::warnings::take().is_empty());
    }

    #[test]
    fn compound_crs_geokeys_match_gdal() {
        // The keys GDAL 3.13 writes in a GeoTIFF in EPSG:6588+6360 (besides the citation).
        let (vlrs, wkt) = crs_vlrs(&SpatialRef::parse("EPSG:6588+6360").unwrap(), false);
        assert!(!wkt);
        let keys = shorts(&vlrs[0].data);
        let keys: Vec<[u16; 4]> = keys[4..].as_chunks::<4>().0.to_vec();
        for k in [[1024, 0, 1, 1], [1025, 0, 1, 1], [3072, 0, 1, 6588], [4096, 0, 1, 6360]] {
            assert!(keys.contains(&k), "{k:?} in {keys:?}");
        }
    }

    #[test]
    fn compound_crs_with_a_vertical_crs_without_a_code_keeps_geokeys() {
        let horizontal = SpatialRef::from_epsg(26915).unwrap().to_wkt().unwrap();
        let vertical = r#"VERT_CS["local height",VERT_DATUM["unknown",2000],UNIT["metre",1],AXIS["Up",UP]]"#;
        let crs = SpatialRef::from_wkt(&format!("COMPD_CS[\"local\",{horizontal},{vertical}]"));
        // A LAS 1.2 file cannot carry a WKT record, so the GeoKeys stay, and the vertical CRS
        // left out is reported.
        let _ = crate::warnings::take();
        let (vlrs, wkt) = crs_vlrs(&crs, false);
        assert!(!wkt);
        assert!(crate::warnings::take().iter().any(|w| w.contains("only the horizontal CRS")));
        let h = LasHeader { vlrs, version: (1, 2), ..Default::default() };
        assert_eq!(header_crs(&h).horizontal.epsg, Some(26915));
    }
}
