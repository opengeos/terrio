// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/gtiff/gt_wkt_srs.cpp
//   libgeotiff newest:
//     libgeotiff/geo_normalize.c
// No code is translated from these sources. Compared with GDAL 3.12.1's output (rasterio 1.5.0)
// in tests/geokeys_gdal.rs.

//! Coordinate reference systems in GeoTIFF GeoKeys (GeoTIFF 1.1, OGC 19-008r4), as GDAL reads and
//! writes them: EPSG codes, user-defined geographic and projected CRSs (the projection method
//! and its parameters, for CRSs without an EPSG code), vertical CRSs and ESRI PE citations.
//!
//! [`read`] (and [`SpatialRef::from_geokeys`]) reads the CRS of a GeoKey directory,
//! [`SpatialRef::to_geokeys`] gives the GeoKeys of a CRS and [`GeoKeyValues::encode`] the three
//! TIFF tags that hold them.
//!
//! ```
//! use terrio_crs::SpatialRef;
//!
//! let keys = SpatialRef::from_epsg(32633)?.to_geokeys()?;
//! let tags = keys.encode()?;
//! let read = SpatialRef::from_geokeys(&tags.directory, &tags.doubles, &tags.ascii)?;
//! assert_eq!(read.epsg, Some(32633));
//! # Ok::<(), terrio_crs::Error>(())
//! ```

use crate::{Error, Result, SpatialRef};

/// GeoKey directory access: `(location, count, value_or_offset)` by key id.
struct GeoKeys<'a> {
    /// The GeoKeyDirectoryTag values.
    pub keys: &'a [u16],
    /// The GeoDoubleParamsTag values.
    pub doubles: &'a [f64],
    /// The GeoAsciiParamsTag value.
    pub ascii: &'a str,
}

const GEO_DOUBLE_PARAMS: u16 = 34736;
const GEO_ASCII_PARAMS: u16 = 34737;
const USER_DEFINED: u16 = 32767;

impl GeoKeys<'_> {
    fn key(&self, id: u16) -> Option<(u16, u16, u16)> {
        self.keys.get(4..)?.chunks(4).find(|k| k.len() == 4 && k[0] == id).map(|k| (k[1], k[2], k[3]))
    }

    fn short(&self, id: u16) -> Option<u16> {
        self.key(id).filter(|(loc, _, _)| *loc == 0).map(|(_, _, v)| v)
    }

    fn double(&self, id: u16) -> Option<f64> {
        self.key(id).filter(|(loc, _, _)| *loc == GEO_DOUBLE_PARAMS).and_then(|(_, _, off)| self.doubles.get(off as usize).copied())
    }

    fn doubles(&self, id: u16) -> Option<&[f64]> {
        let (loc, n, off) = self.key(id)?;
        if loc != GEO_DOUBLE_PARAMS {
            return None;
        }
        self.doubles.get(off as usize..off as usize + n as usize)
    }

    fn text(&self, id: u16) -> Option<&str> {
        let (loc, n, off) = self.key(id)?;
        if loc != GEO_ASCII_PARAMS {
            return None;
        }
        let start = (off as usize).min(self.ascii.len());
        let end = (start + n as usize).min(self.ascii.len());
        self.ascii.get(start..end).map(|s| s.trim_end_matches(['|', '\0']))
    }

    /// An ASCII key's text, its bytes sliced: offsets count bytes, and the text may hold
    /// multi-byte characters (invalid bytes are decoded to U+FFFD).
    fn ascii_lossy(&self, id: u16) -> Option<String> {
        let (loc, n, off) = self.key(id)?;
        if loc != GEO_ASCII_PARAMS {
            return None;
        }
        let b = self.ascii.as_bytes();
        let start = (off as usize).min(b.len());
        let end = (start + n as usize).min(b.len());
        Some(String::from_utf8_lossy(&b[start..end]).trim_end_matches('|').to_string())
    }

    fn first(&self, ids: &[u16]) -> Option<f64> {
        ids.iter().find_map(|id| self.double(*id))
    }
}

/// The CRS of an ESRI PE string in a citation key (`ESRI PE String = <WKT>`), which describes a
/// user-defined CRS more completely than the GeoKeys. Its EPSG code is the root node's: codes on
/// inner nodes (a unit's, a datum's) are not the CRS's.
fn esri_pe_crs(k: &GeoKeys) -> Option<SpatialRef> {
    let wkt = [3073, 1026, 2049].iter().find_map(|id| k.text(*id).and_then(|c| c.strip_prefix("ESRI PE String = ")))?.trim();
    // Read as any WKT: the code identified, and its PROJ4 extension (GDAL writes the sweep axis
    // of a geostationary CRS there) taken as the CRS.
    let s = SpatialRef::from_wkt(wkt);
    Some(SpatialRef { epsg: s.epsg, wkt: Some(wkt.to_string()), proj4: s.proj4, esri: s.esri })
}

/// The CRS GeoKeys give, as GDAL reads them, and a projection method terrio cannot read.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct GeoKeysCrs {
    /// The CRS: its EPSG code (and the registry's WKT), the WKT of a user-defined CRS or of an
    /// ESRI PE citation, or the PROJ string of a user-defined projected CRS. `None` when the
    /// GeoKeys give no CRS terrio reads.
    pub crs: Option<SpatialRef>,
    /// The ProjCoordTransGeoKey of a user-defined projected CRS whose method terrio does not read
    /// (or whose parameters are incomplete), which left [`crs`](Self::crs) `None`.
    pub unsupported_method: Option<u16>,
}

/// Reads the CRS of a GeoKey directory (GeoKeyDirectoryTag, 34735) with its double
/// (GeoDoubleParamsTag, 34736) and ASCII (GeoAsciiParamsTag, 34737) parameters, as stored in a
/// GeoTIFF or a LAS `LASF_Projection` record, as GDAL reads them (`GTIFGetOGISDefnAsOSR`):
/// - EPSG codes of a projected or geographic CRS (ProjectedCSTypeGeoKey,
///   GeographicTypeGeoKey). A user-defined projected CRS that is a registered one in the GeoKeys'
///   linear unit gets its code: a UTM zone by its ProjectionGeoKey or its citation, on WGS 84,
///   NAD83 or NAD27, and (with the `wbprojection-fallback` feature) the projected CRS its
///   citation names, on the GeoKeys' datum when they give its code. GDAL names only an
///   equivalent CRS of the citation's exact name: on keys with a datum of their own or none, the
///   code's definition can differ from theirs (see the CRS documentation);
/// - user-defined geographic and projected CRSs, for the coordinate transformation codes 1, 3,
///   7-28 and Hotine oblique Mercator variant B (9815, Swiss oblique Mercator among them), in
///   any linear unit and on an EPSG datum, an EPSG ellipsoid or the GeoKeys' own;
/// - ESRI PE citations (`ESRI PE String = <WKT>`), as GDAL writes a CRS GeoKeys cannot hold;
/// - a vertical CRS (VerticalCSTypeGeoKey) on an EPSG horizontal CRS, as the compound CRS (the
///   registry's compound CRS of the two codes when there is one), and a geographic 3D CRS as its
///   geographic 2D CRS with its own code as the vertical CRS (EPSG:4979 as 4326 and 4979).
///
/// ```
/// // GeoKeys of a UTM zone, as a GeoTIFF's three tags hold them.
/// let directory = [1, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, 32633];
/// let read = terrio_crs::geokeys::read(&directory, &[], "");
/// assert_eq!(read.crs.and_then(|c| c.epsg), Some(32633));
/// assert_eq!(read.unsupported_method, None);
/// ```
pub fn read(directory: &[u16], doubles: &[f64], ascii: &str) -> GeoKeysCrs {
    let k = GeoKeys { keys: directory, doubles, ascii };
    let mut epsg = match (k.short(1024), k.short(3072), k.short(2048)) {
        (_, Some(code), _) if code != 0 && code != USER_DEFINED => Some(code as u32),
        (Some(2), _, Some(code)) if code != 0 && code != USER_DEFINED => Some(code as u32),
        (Some(1), _, _) => {
            // A registered CRS only in the GeoKeys' linear unit: GDAL builds the CRS from the
            // keys (a ProjectionGeoKey's conversion in metres, in ProjLinearUnitsGeoKey's unit),
            // and gives it a registered CRS's code only when the two are equivalent
            // (gt_wkt_srs.cpp `GTIFGetOGISDefnAsOSR`): UTM zone 33N in US survey feet is no
            // EPSG:32633.
            let size = linear_unit_size(&k);
            // A user-defined datum whose citation names WGS 84 is WGS 84's, as GDAL identifies it.
            let cites_wgs84 = k.ascii_lossy(2049).is_some_and(|c| {
                c.split('|')
                    .any(|p| p.strip_prefix("Datum = ").is_some_and(|d| matches!(d.trim(), "World Geodetic System 1984" | "WGS_1984")))
            });
            let datum = if cites_wgs84 && k.short(2050) == Some(USER_DEFINED) { Some(6326) } else { k.short(2050) };
            let citation = k.ascii_lossy(1026).or_else(|| k.ascii_lossy(3073));
            user_defined_projected(k.short(3074), k.short(2048), datum, citation)
                .filter(|code| SpatialRef::from_epsg(*code).is_ok_and(|s| (s.metres_per_unit() / size - 1.0).abs() < 1e-9))
        }
        _ => None,
    };
    let (mut crs, mut proj4, mut unsupported_method) = (None, None, None);
    if epsg.is_none() {
        // Geographic and projected user-defined CRSs, and ESRI PE citations.
        if let Some(c) = projected_on_epsg_geographic(&k) {
            return GeoKeysCrs { crs: Some(c), unsupported_method: None };
        }
        if k.short(1024) == Some(1) && esri_pe_crs(&k).is_none() {
            proj4 = user_defined_proj4(&k);
            unsupported_method = unmapped_method(&k);
            // Where GDAL reads it otherwise than the WKT of its PROJ string: an ESRI CRS named by
            // the citation, a linear unit PROJ strings do not name, an inverse flattening
            // computed from the axes.
            if let Some(c) = proj4.as_deref().and_then(|p| user_defined_projected_crs(&k, p)) {
                return GeoKeysCrs { crs: Some(c), unsupported_method };
            }
        }
        if proj4.is_none() {
            crs = user_defined_crs(&k);
            epsg = crs.as_ref().and_then(|c| c.epsg);
        }
    }
    // A vertical CRS (VerticalCSTypeGeoKey) on an EPSG horizontal CRS: the compound CRS, as GDAL
    // reads it (EPSG:4326 with EGM96 heights is `EPSG:4326+5773`). GeoTIFF stores no code for the
    // compound: GDAL 3.12.1 takes the registry's compound CRS of the two codes when there is one
    // (`GTIFGetOGISDefnAsOSR`), EPSG:9707 for those.
    // A geographic 3D CRS: its geographic 2D CRS's code with its own as the vertical CRS, as GDAL
    // 3.12.1 writes and reads EPSG:4979 (`2048=4326`, `4096=4979`).
    if let Some(h) = epsg
        && let Some(v) = k.short(4096).map(u32::from).filter(|v| geographic_2d_of(*v) == Some(h))
        && let Ok(c) = SpatialRef::from_epsg(v)
    {
        return GeoKeysCrs { crs: Some(c), unsupported_method };
    }
    if let Some(h) = epsg
        && let Some(v) = k.short(4096).filter(|c| *c != 0 && *c != USER_DEFINED)
        && let Ok(c) = SpatialRef::compound(h, u32::from(v))
    {
        let registered = crate::compound_code(h, u32::from(v), c.name().as_deref()).and_then(|code| SpatialRef::from_epsg(code).ok());
        let c = registered.unwrap_or(c);
        return GeoKeysCrs { crs: Some(SpatialRef { wkt: c.to_wkt(), esri: None, ..c }), unsupported_method };
    }
    // The CRS an identifier is a summary of: the retained definition, else the code's, else the
    // PROJ string's.
    let crs = crs.or_else(|| {
        epsg.map(|code| SpatialRef { epsg: Some(code), wkt: SpatialRef::from_epsg(code).ok().and_then(|s| s.wkt), ..Default::default() })
    });
    let crs = crs.or_else(|| proj4.as_deref().and_then(|p| SpatialRef::from_proj4(p).ok()));
    GeoKeysCrs { crs, unsupported_method }
}

/// The geographic 2D CRS of an EPSG geographic 3D CRS: the one on its datum of the same name, as
/// GDAL pairs them in GeoKeys (EPSG:4979 and 4326, 4937 and 4258). `None` when there is none.
fn geographic_2d_of(code: u32) -> Option<u32> {
    use crate::epsg::GeodeticKind::{Geographic2D, Geographic3D};
    let r = crate::epsg::registry();
    let g = r.geodetic.get(&code).filter(|g| g.kind == Geographic3D)?;
    // The datum's CRSs, by code (sorted when the registry is read).
    r.by_datum.get(&g.datum)?.iter().copied().find(|c| r.geodetic.get(c).is_some_and(|t| t.kind == Geographic2D && t.name == g.name))
}

/// The EPSG code of a user-defined projected CRS, from its projection code (UTM zones) or its
/// citation (such as `WGS 84 / UTM zone 10N`). The datum is the geographic CRS's, else the
/// GeogGeodeticDatumGeoKey's `datum`: a UTM zone on another datum than WGS 84, NAD83 or NAD27
/// (NAD83(HARN), ETRS89, a datum of the GeoKeys' own) is no WGS 84 UTM zone, and is left to the
/// citation's name (a projected CRS, on that datum when the GeoKeys give its code).
fn user_defined_projected(projection: Option<u16>, geographic: Option<u16>, datum: Option<u16>, citation: Option<String>) -> Option<u32> {
    let citation = citation.unwrap_or_default();
    let defined = |c: &u16| *c != 0 && *c != USER_DEFINED;
    // A datum of the GeoKeys' own (a user-defined GeogGeodeticDatumGeoKey on a user-defined
    // geographic CRS, as GDAL writes a datum it has no code for) is none of the registry's: GDAL
    // names it after its ellipsoid (UTM zone 33N on GRS 1980 is not WGS 84's).
    let own_datum = !geographic.is_some_and(|c| defined(&c)) && datum == Some(USER_DEFINED);
    // A geographic CRS terrio does not know leaves the datum to the datum key, else the citation.
    let datum = geographic.filter(defined).and_then(|c| crate::geographic_datum(c as u32)).or(datum.filter(defined).map(u32::from));
    // Datum: WGS 84 unless the datum is NAD83 or NAD27, or the citation says so when the GeoKeys
    // name no datum.
    let (nad83, nad27, other) = match datum {
        Some(6269) => (true, false, false),
        Some(6267) => (false, true, false),
        Some(6326) => (false, false, false),
        Some(_) => (false, false, true),
        None => (citation.contains("NAD83"), citation.contains("NAD27"), own_datum),
    };
    let utm = match projection {
        Some(p @ 16001..=16060) => Some(((p - 16000) as u32, true)),
        Some(p @ 16101..=16160) => Some(((p - 16100) as u32, false)),
        _ => parse_utm_citation(&citation),
    };
    if let Some((zone, north)) = utm.filter(|_| !other) {
        return match (nad83, nad27, north) {
            (true, _, true) if (1..=23).contains(&zone) => Some(26900 + zone),
            (_, true, true) if (1..=22).contains(&zone) => Some(26700 + zone),
            (false, false, true) if (1..=60).contains(&zone) => Some(32600 + zone),
            (false, false, false) if (1..=60).contains(&zone) => Some(32700 + zone),
            _ => None,
        };
    }
    let name = citation.split('|').next().unwrap_or_default().trim();
    // The projected CRS a citation names, on the GeoKeys' datum when they give one (a citation of
    // `EPSG:32610` on NAD83(HARN) is no WGS 84 UTM zone). A datum of the GeoKeys' own is left to
    // the name: GDAL reads `NAD83 / UTM zone 15N` on a datum cited as
    // `North_American_Datum_1983` as EPSG:26915.
    let on_datum = |code: &u32| {
        datum.is_none_or(|d| {
            SpatialRef::from_epsg(*code).ok().and_then(|s| s.geographic_code()).and_then(crate::geographic_datum) == Some(d)
        })
    };
    let projected = |code: &u32| horizontal_geokey(*code).is_some_and(|(_, geographic)| !geographic);
    if name.is_empty() { None } else { code_of_name(name).filter(projected).filter(on_datum) }
}

/// Parses `UTM zone 10N` (or `UTM Zone 10, Northern Hemisphere`) from a citation.
fn parse_utm_citation(c: &str) -> Option<(u32, bool)> {
    let lower = c.to_ascii_lowercase();
    let rest = &lower[lower.find("utm zone")? + "utm zone".len()..];
    let rest = rest.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let zone: u32 = digits.parse().ok()?;
    let after = &rest[digits.len()..];
    let north = !(after.starts_with('s') || after.contains("south"));
    Some((zone, north))
}

/// The EPSG code a citation names, when terrio is built with wbprojection: an EPSG reference in
/// it (`EPSG:32633`), else the code of wbprojection's CRS of that name, as terrio has read
/// citations.
#[cfg(feature = "wbprojection-fallback")]
fn code_of_name(name: &str) -> Option<u32> {
    wbprojection::epsg_from_srs_reference(name).or_else(|| {
        wbprojection::epsg::known_epsg_codes()
            .into_iter()
            .find(|code| wbprojection::epsg::epsg_info(*code).is_some_and(|i| i.name.eq_ignore_ascii_case(name)))
    })
}

/// None without wbprojection: GDAL takes a citation's CRS only when it is the registered CRS of
/// that exact name and equivalent to the GeoKeys' (never for an `EPSG:` reference), which terrio
/// does not check yet.
#[cfg(not(feature = "wbprojection-fallback"))]
fn code_of_name(_: &str) -> Option<u32> {
    None
}

/// A user-defined projected CRS of GeoKeys as GDAL reads it, where that is not the WKT of its
/// PROJ string (`proj4`, from [`user_defined_proj4`]): `None` when it is.
///
/// - An ESRI CRS named by the citation (GTCitationGeoKey, as GDAL writes one), whose definition
///   the GeoKeys give: GDAL takes the registry's CRS of that name when the two are equivalent
///   (`GTIFGetOfficialCRS` on any authority, gt_wkt_srs.cpp), ESRI:102003 for
///   `USA_Contiguous_Albers_Equal_Area_Conic`.
/// - A linear unit PROJ strings do not name (Clarke's foot, a user-defined unit), named as GDAL
///   names it.
/// - The inverse flattening GDAL computes from the axes (see [`computed_inverse_flattening`]),
///   on a geographic CRS without code.
fn user_defined_projected_crs(k: &GeoKeys, proj4: &str) -> Option<SpatialRef> {
    let wkt = SpatialRef::from_proj4(proj4).ok()?.to_wkt()?;
    let name = [3073, 1026].iter().find_map(|id| k.text(*id)).map(str::trim).filter(|n| !n.is_empty() && !n.contains(" = "));
    if let Some(name) = name
        && let Some(named) = renamed(&wkt, name)
        && let Some(code) = SpatialRef::from_wkt(&named).with_esri().esri
        && let Some(esri) = SpatialRef::from_esri(code).ok().and_then(|s| s.to_wkt())
    {
        // With its code, as GDAL's `SetFromUserInput("ESRI:<code>")` gives it.
        let declared = esri.ends_with(&format!("AUTHORITY[\"ESRI\",\"{code}\"]]"));
        let esri = if declared { esri } else { format!("{},AUTHORITY[\"ESRI\",\"{code}\"]]", &esri[..esri.len() - 1]) };
        return Some(SpatialRef { epsg: None, wkt: Some(esri), proj4: None, esri: None });
    }
    let mut out = wkt.clone();
    if let Some(unit) = linear_unit_wkt(k) {
        out = with_unit(&out, &unit)?;
    }
    if let Some(rf) = computed_inverse_flattening(k)
        && let Some(i) = out.find("SPHEROID[")
        && !geogcs_has_code(&out)
    {
        // SPHEROID["name",a,rf...]: the third field.
        let body = &out[i..];
        let first = body.find(',')? + 1;
        let second = first + body[first..].find(',')? + 1;
        let end = second + body[second..].find([',', ']'])?;
        out = format!("{}{}{}", &out[..i + second], wkt_number(rf), &out[i + end..]);
    }
    (out != wkt).then_some(SpatialRef { epsg: None, wkt: Some(out), proj4: None, esri: None })
}

/// Whether the `GEOGCS` of a projected CRS's WKT 1 has a code: an `AUTHORITY` as its own (last)
/// child.
fn geogcs_has_code(wkt: &str) -> bool {
    let Some(start) = wkt.find("GEOGCS[") else { return false };
    let (mut depth, mut last) = (0, 0);
    for (i, c) in wkt[start..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' if depth == 1 => return wkt[start + last..].trim_start_matches(',').starts_with("AUTHORITY["),
            ']' => depth -= 1,
            ',' if depth == 1 => last = i,
            _ => {}
        }
    }
    false
}

/// The inverse flattening GDAL gives the ellipsoid of a user-defined geographic CRS whose
/// citation does not name the ellipsoid (GeogCitationGeoKey without `Ellipsoid = `, as GDAL
/// writes it for an ellipsoid with a code): GDAL names the ellipsoid by its code, and computes
/// the inverse flattening from the semi-major axis and the semi-minor axis libgeotiff derives
/// from GeogInvFlatteningGeoKey (`OSRCalcInvFlattening`, gt_wkt_srs.cpp), taking WGS 84's as
/// defined: 298.257222101004 for GRS 1980's 298.257222101. `None` when GDAL takes the key's
/// value (a citation naming the ellipsoid) or the keys give no axes.
fn computed_inverse_flattening(k: &GeoKeys) -> Option<f64> {
    if k.text(2049).is_some_and(|c| c.contains("Ellipsoid = ")) {
        return None;
    }
    let a = k.double(2057).filter(|a| *a > 0.0)?;
    // libgeotiff's GTIFGetDefn: the inverse flattening overrides the semi-minor axis.
    let b = match (k.double(2059), k.double(2058)) {
        (Some(rf), _) if rf != 0.0 => a * (1.0 - 1.0 / rf),
        (Some(_), _) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    if (a - b).abs() < 0.1 || b <= 0.0 || b > a {
        return None;
    }
    let rf = a / (a - b);
    Some(if (a - 6_378_137.0).abs() < 1e-10 && (rf - 298.257_223_563).abs() < 1e-10 { 298.257_223_563 } else { rf })
}

/// The PROJ definition of a user-defined projected CRS: the projection method and its parameters,
/// the datum and the linear unit. `None` for a method terrio does not map.
fn user_defined_proj4(k: &GeoKeys) -> Option<String> {
    // GeoKeys that name no datum or ellipsoid are taken as WGS 84.
    let geog = geographic_proj(k).unwrap_or_else(|| "+datum=WGS84".to_string());
    let (units, to_meter) = units(k);
    Some(format!("+proj={} {geog} {units} +no_defs", projection(k, to_meter)?))
}

/// The projection method of projected GeoKeys that [`user_defined_proj4`] does not map, for a
/// warning: `None` when there is none or it is mapped.
fn unmapped_method(k: &GeoKeys) -> Option<u16> {
    let method = k.short(3075).filter(|m| *m != 0 && *m != USER_DEFINED)?;
    (k.short(1024) == Some(1) && projection(k, 1.0).is_none()).then_some(method)
}

/// The CRS of GeoKeys that use no EPSG code for the whole CRS: an ESRI PE citation's WKT, or a
/// PROJ definition built from the projection method, its parameters and the datum.
fn user_defined_crs(k: &GeoKeys) -> Option<SpatialRef> {
    if let Some(crs) = esri_pe_crs(k) {
        return Some(crs);
    }
    let proj = match k.short(1024)? {
        2 => {
            // In an angular unit other than the degree, which PROJ strings cannot hold: the WKT
            // GDAL reads, without code (GDAL identifies none: its unit is not the registry's).
            if let Some((_, unit)) = angular_unit(k) {
                let wkt = SpatialRef::from_proj4(&format!("+proj=longlat {}", geographic_proj(k)?)).ok()?.to_wkt()?;
                let cited = |key: &str| k.text(2049).and_then(|c| c.split('|').find_map(|p| p.trim().strip_prefix(key)));
                let mut wkt = renamed(&without_code(&with_unit(&wkt, &unit)?), cited("GCS Name = ").unwrap_or("unknown"))?;
                // The datum and ellipsoid named by the citation, as GDAL names them
                // (`GetGeogCSFromCitation`), and its axes in the registry's order, as GDAL builds
                // a geographic CRS from GeoKeys.
                for (node, key) in [("DATUM[", "Datum = "), ("SPHEROID[", "Ellipsoid = ")] {
                    if let (Some(name), Some(i)) = (cited(key), wkt.find(node)) {
                        wkt = format!("{}{}", &wkt[..i], renamed(&wkt[i..], name)?);
                    }
                }
                let wkt =
                    wkt.replace(",AXIS[\"Longitude\",EAST],AXIS[\"Latitude\",NORTH]", ",AXIS[\"Latitude\",NORTH],AXIS[\"Longitude\",EAST]");
                return Some(SpatialRef { epsg: None, wkt: Some(wkt), proj4: None, esri: None });
            }
            // On an EPSG datum PROJ strings do not name (NAD83(HARN)): its EPSG geographic CRS.
            if let Some((code, wkt)) = epsg_geographic(k).and_then(|c| Some((c, crate::epsg_wkt(c)?))) {
                return Some(SpatialRef { epsg: Some(code), wkt: Some(wkt.to_string()), proj4: None, esri: None });
            }
            format!("+proj=longlat {}", geographic_proj(k)?)
        }
        1 => user_defined_proj4(k)?,
        _ => return None,
    };
    if let Some(crs) = projected_on_epsg_geographic(k) {
        return Some(crs);
    }
    // wbprojection's identification of the PROJ string, with the WKT of its code (wbraster's
    // `CrsInfo::from_proj4`), when built with it.
    #[cfg(feature = "wbprojection-fallback")]
    let (guess, guess_wkt) = {
        let code = wbprojection::from_proj_string(proj.trim()).ok().and_then(|c| wbprojection::identify_epsg_from_crs(&c));
        (code, code.and_then(|c| wbprojection::to_ogc_wkt(c).ok()))
    };
    #[cfg(not(feature = "wbprojection-fallback"))]
    let (guess, guess_wkt): (Option<u32>, Option<String>) = (None, None);
    // The WKT GDAL writes for the PROJ string, which describes it whole.
    let own = SpatialRef::from_proj4(proj.trim()).ok().and_then(|s| s.wkt);
    // wbprojection's code, when terrio reads the PROJ string, only if terrio identifies the CRS
    // as it: wbprojection's identification ignores the prime meridian and datum shift
    // (`+proj=longlat +ellps=bessel +pm=paris` is not EPSG:4162). When terrio cannot read it,
    // wbprojection's code and WKT stay the fallback.
    let epsg = match own.as_deref() {
        Some(w) => guess.filter(|code| crate::identify_epsg(w) == Some(*code)),
        None => guess,
    };
    let wkt = own.or(guess_wkt);
    #[cfg(feature = "wbprojection-fallback")]
    let wkt = wkt.or_else(|| wbprojection::from_proj_string(&proj).ok().map(|c| wbprojection::crs_to_wkt(&c)));
    Some(SpatialRef { epsg, wkt, proj4: Some(proj.trim().to_string()), esri: None })
}

/// A user-defined projected CRS on an EPSG geographic CRS a PROJ string does not name exactly
/// (OSGB36, ED50, NAD83(HARN), ETRS89...): its WKT, whose `GEOGCS` identifies the datum and its
/// transformations, as GDAL reads the GeoKeys. `None` for other GeoKeys.
fn projected_on_epsg_geographic(k: &GeoKeys) -> Option<SpatialRef> {
    if k.short(1024) != Some(1) || esri_pe_crs(k).is_some() {
        return None;
    }
    let code = epsg_geographic(k)?;
    let wkt = crate::from_proj_on(&user_defined_proj4(k)?, code)?;
    // A linear unit PROJ strings do not name, named as GDAL names it.
    let wkt = linear_unit_wkt(k).and_then(|u| with_unit(&wkt, &u)).unwrap_or(wkt);
    Some(SpatialRef { epsg: None, wkt: Some(wkt), proj4: None, esri: None })
}

/// A WKT 1 CRS without the code of its root (its last node).
fn without_code(wkt: &str) -> String {
    match wkt.rfind(",AUTHORITY[") {
        Some(i) if wkt[i..].matches(']').count() == 2 && wkt.ends_with("]]") => format!("{}]", &wkt[..i]),
        _ => wkt.to_string(),
    }
}

/// A WKT 1 CRS with its root named `name`.
fn renamed(wkt: &str, name: &str) -> Option<String> {
    let open = wkt.find("[\"")? + 2;
    let close = open + wkt[open..].find('"')?;
    Some(format!("{}{}{}", &wkt[..open], name.replace('"', ""), &wkt[close..]))
}

/// The EPSG geographic CRS of GeoKeys whose datum a PROJ string does not name exactly (OSGB36,
/// ED50, NAD83(HARN), ETRS89...): their GeographicTypeGeoKey, else, for a user-defined geographic
/// CRS, the EPSG geographic CRS on their GeogGeodeticDatumGeoKey's datum with their ellipsoid and
/// the Greenwich meridian, as GDAL reads them (the GeoKeys of PDAL's `autzen_trim.las` give
/// NAD83(HARN), 6152, as the datum of a user-defined geographic CRS). `None` for WGS 84, NAD83
/// and NAD27, PROJ's `+datum=` names, and for GeoKeys without EPSG datum.
fn epsg_geographic(k: &GeoKeys) -> Option<u32> {
    let code = match k.short(2048) {
        Some(USER_DEFINED) | None => {
            let datum = k.short(2050).filter(|d| !matches!(*d, 0 | USER_DEFINED | 6326 | 6269 | 6267))?;
            let greenwich = k.short(2051).is_none_or(|pm| pm == 8901) && k.double(2061).is_none_or(|pm| pm == 0.0);
            if !greenwich || k.doubles(2062).is_some() {
                return None;
            }
            crate::geographic_on_datum(datum as u32, &ellipsoid_proj(k)?)?
        }
        Some(code) => code as u32,
    };
    (!matches!(code, 0 | 4326 | 4269 | 4267)).then_some(code)
}

/// The PROJ datum (or ellipsoid and shift) of the EPSG datums PROJ strings name.
fn named_datum(datum: Option<u32>) -> Option<&'static str> {
    match datum? {
        6326 => Some("+datum=WGS84"),
        // NAD83(HARN) as NAD83, within a metre, as GDAL's PROJ strings write it.
        6269 | 6152 => Some("+datum=NAD83"),
        6267 => Some("+datum=NAD27"),
        6258 | 6283 | 6167 | 6619 => Some("+ellps=GRS80 +towgs84=0,0,0"),
        6322 => Some("+ellps=WGS72"),
        _ => None,
    }
}

/// The datum or ellipsoid part of a PROJ definition.
fn geographic_proj(k: &GeoKeys) -> Option<String> {
    let datum = match k.short(2048) {
        Some(code) if code != USER_DEFINED => Some(code as u32 + 2000), // GCS 4326 -> datum 6326
        _ => k.short(2050).map(u32::from),
    };
    if let Some(n) = named_datum(datum) {
        return Some(n.to_string());
    }
    let mut out = ellipsoid_proj(k)?;
    // GeogPrimeMeridianLongGeoKey and GeogTOWGS84GeoKey.
    if let Some(pm) = k.double(2061).filter(|pm| *pm != 0.0) {
        out.push_str(&format!(" +pm={pm}"));
    }
    if let Some(t) = k.doubles(2062).filter(|t| t.len() == 3 || t.len() == 7) {
        out.push_str(&format!(" +towgs84={}", t.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")));
    }
    Some(out)
}

/// The ellipsoid of GeoKeys, as PROJ parameters: their EPSG geographic CRS's, else their
/// ellipsoid code's, else their semi-major axis and inverse flattening (or semi-minor axis).
fn ellipsoid_proj(k: &GeoKeys) -> Option<String> {
    // The ellipsoid of another EPSG geographic CRS, when its datum cannot be named.
    if let Some(e) = k.short(2048).filter(|c| *c != USER_DEFINED).and_then(|c| crate::epsg_ellipsoid(c as u32)) {
        return Some(e);
    }
    let ellipsoid = match k.short(2056) {
        Some(7030) => Some("+ellps=WGS84"),
        Some(7019) => Some("+ellps=GRS80"),
        Some(7008) => Some("+ellps=clrk66"),
        Some(7004) => Some("+ellps=bessel"),
        Some(7022) => Some("+ellps=intl"),
        Some(7035) | Some(7047) => Some("+R=6371007.181"),
        _ => None,
    };
    Some(match ellipsoid {
        Some(e) => e.to_string(),
        None => {
            let a = k.double(2057)?;
            match (k.double(2059), k.double(2058)) {
                (Some(rf), _) if rf > 0.0 => format!("+a={a} +rf={rf}"),
                (_, Some(b)) => format!("+a={a} +b={b}"),
                _ => format!("+R={a}"),
            }
        }
    })
}

/// The projection method and parameters of a PROJ definition. GeoTIFF gives false eastings and
/// northings in the CRS's linear unit (`to_meter` metres), PROJ in metres.
fn projection(k: &GeoKeys, to_meter: f64) -> Option<String> {
    // Parameter keys.
    const STD_PARALLEL_1: u16 = 3078;
    const STD_PARALLEL_2: u16 = 3079;
    const NAT_ORIGIN_LONG: u16 = 3080;
    const NAT_ORIGIN_LAT: u16 = 3081;
    const FALSE_EASTING: u16 = 3082;
    const FALSE_NORTHING: u16 = 3083;
    const FALSE_ORIGIN_LONG: u16 = 3084;
    const FALSE_ORIGIN_LAT: u16 = 3085;
    const FALSE_ORIGIN_EASTING: u16 = 3086;
    const FALSE_ORIGIN_NORTHING: u16 = 3087;
    const CENTER_LONG: u16 = 3088;
    const CENTER_LAT: u16 = 3089;
    const SCALE_AT_NAT_ORIGIN: u16 = 3092;
    const SCALE_AT_CENTER: u16 = 3093;
    const AZIMUTH: u16 = 3094;
    const STRAIGHT_VERT_POLE_LONG: u16 = 3095;
    const RECTIFIED_GRID_ANGLE: u16 = 3096;

    // Angles are in the geographic CRS's angular unit, which libgeotiff (1.7.4 and later)
    // converts to degrees, the azimuth excepted.
    let to_degrees = angular_unit(k).map_or(1.0, |(d, _)| d);
    let angle = |ids: &[u16]| k.first(ids).map(|v| v * to_degrees);
    let lon0 = angle(&[NAT_ORIGIN_LONG, FALSE_ORIGIN_LONG, CENTER_LONG, STRAIGHT_VERT_POLE_LONG]).unwrap_or(0.0);
    let lat0 = angle(&[NAT_ORIGIN_LAT, FALSE_ORIGIN_LAT, CENTER_LAT]).unwrap_or(0.0);
    let x0 = k.first(&[FALSE_EASTING, FALSE_ORIGIN_EASTING]).unwrap_or(0.0) * to_meter;
    let y0 = k.first(&[FALSE_NORTHING, FALSE_ORIGIN_NORTHING]).unwrap_or(0.0) * to_meter;
    let scale = k.first(&[SCALE_AT_NAT_ORIGIN, SCALE_AT_CENTER]).unwrap_or(1.0);
    let lat1 = angle(&[STD_PARALLEL_1]);
    let lat2 = angle(&[STD_PARALLEL_2]);
    let false_xy = format!("+x_0={x0} +y_0={y0}");
    let oblique_mercator = |variant: &str| -> Option<String> {
        let gamma = angle(&[RECTIFIED_GRID_ANGLE]).map(|g| format!(" +gamma={g}")).unwrap_or_default();
        Some(format!("omerc{variant} +lat_0={lat0} +lonc={lon0} +alpha={}{gamma} +k={scale} {false_xy}", k.double(AZIMUTH)?))
    };
    // A UTM zone by its ProjectionGeoKey (16001-16060 north, 16101-16160 south) alone, as GDAL
    // writes a UTM zone on a user-defined geographic CRS.
    if k.short(3075).is_none() {
        return match k.short(3074)? {
            p @ 16001..=16060 => Some(format!("utm +zone={}", p - 16000)),
            p @ 16101..=16160 => Some(format!("utm +zone={} +south", p - 16100)),
            _ => None,
        };
    }
    Some(match k.short(3075)? {
        1 => format!("tmerc +lat_0={lat0} +lon_0={lon0} +k={scale} {false_xy}"),
        // Hotine oblique Mercator: variant A (natural origin) and B (centre of projection).
        3 => oblique_mercator(" +no_uoff")?,
        // Swiss oblique Mercator, as GDAL writes it: variant B of azimuth 90°.
        9815 if k.double(AZIMUTH) == Some(90.0) && angle(&[RECTIFIED_GRID_ANGLE]).is_none_or(|g| g == 90.0) => {
            format!("somerc +lat_0={lat0} +lon_0={lon0} +k_0={scale} {false_xy}")
        }
        9815 => oblique_mercator("")?,
        // Mercator 2SP has its standard parallel in ProjStdParallel1GeoKey, 1SP a scale (with
        // which a zero standard parallel is taken as unused).
        7 => match lat1.filter(|ts| *ts != 0.0 || scale == 1.0) {
            Some(ts) => format!("merc +lon_0={lon0} +lat_ts={ts} {false_xy}"),
            None => format!("merc +lon_0={lon0} +k={scale} {false_xy}"),
        },
        8 => format!("lcc +lat_1={} +lat_2={} +lat_0={lat0} +lon_0={lon0} {false_xy}", lat1?, lat2.or(lat1)?),
        9 => format!("lcc +lat_1={lat0} +lat_0={lat0} +lon_0={lon0} +k_0={scale} {false_xy}"),
        10 => format!("laea +lat_0={lat0} +lon_0={lon0} {false_xy}"),
        11 => format!("aea +lat_1={} +lat_2={} +lat_0={lat0} +lon_0={lon0} {false_xy}", lat1?, lat2.or(lat1)?),
        12 => format!("aeqd +lat_0={lat0} +lon_0={lon0} {false_xy}"),
        13 => format!("eqdc +lat_1={} +lat_2={} +lat_0={lat0} +lon_0={lon0} {false_xy}", lat1?, lat2.or(lat1)?),
        14 => format!("stere +lat_0={lat0} +lon_0={lon0} +k={scale} {false_xy}"),
        15 => {
            // Variant B's standard parallel is in ProjStdParallel1GeoKey for some writers, and in
            // ProjNatOriginLatGeoKey for GDAL (which writes a scale of 1 too); a natural origin
            // latitude of ±90 is variant A, with its scale.
            let origin = angle(&[NAT_ORIGIN_LAT]).unwrap_or(90.0);
            let ts = lat1.or(((origin.abs() - 90.0).abs() > 1e-9).then_some(origin));
            let pole = if ts.unwrap_or(origin) < 0.0 { -90.0 } else { 90.0 };
            // The longitude is ProjStraightVertPoleLongGeoKey's, whatever other origin keys say.
            let lon0 = angle(&[STRAIGHT_VERT_POLE_LONG]).unwrap_or(lon0);
            match ts {
                Some(ts) => format!("stere +lat_0={pole} +lat_ts={ts} +lon_0={lon0} {false_xy}"),
                None => format!("stere +lat_0={pole} +lon_0={lon0} +k={scale} {false_xy}"),
            }
        }
        16 => format!("sterea +lat_0={lat0} +lon_0={lon0} +k={scale} {false_xy}"),
        17 => format!("eqc +lat_ts={} +lat_0={lat0} +lon_0={lon0} {false_xy}", lat1.unwrap_or(0.0)),
        18 => format!("cass +lat_0={lat0} +lon_0={lon0} {false_xy}"),
        19 => format!("gnom +lat_0={lat0} +lon_0={lon0} {false_xy}"),
        20 => format!("mill +lat_0={lat0} +lon_0={lon0} {false_xy}"),
        21 => format!("ortho +lat_0={lat0} +lon_0={lon0} {false_xy}"),
        22 => format!("poly +lat_0={lat0} +lon_0={lon0} {false_xy}"),
        23 => format!("robin +lon_0={lon0} {false_xy}"),
        24 => format!("sinu +lon_0={lon0} {false_xy}"),
        25 => format!("vandg +lon_0={lon0} {false_xy}"),
        26 => format!("nzmg +lat_0={lat0} +lon_0={lon0} {false_xy}"),
        // Transverse Mercator, south-orientated: x westwards and y southwards.
        27 => format!("tmerc +axis=wsu +lat_0={lat0} +lon_0={lon0} +k={scale} {false_xy}"),
        28 => format!("cea +lat_ts={} +lon_0={lon0} {false_xy}", lat1.unwrap_or(0.0)),
        _ => return None,
    })
}

/// The EPSG length units, `(code, name, metres)`, as PROJ's database (9.8.1) gives them to GDAL's
/// GeoTIFF reader for a ProjLinearUnitsGeoKey (libgeotiff's `GTIFGetUOMLengthInfoEx`), rates
/// excepted.
const LENGTH_UNITS: &[(u16, &str, f64)] = &[
    (1025, "millimetre", 0.001),
    (1033, "centimetre", 0.01),
    (9001, "metre", 1.0),
    (9002, "foot", 0.3048),
    // libgeotiff's, not the registry's 0.30480060960121924 (the same to 16 digits).
    (9003, "US survey foot", 12.0 / 39.37),
    (9005, "Clarke's foot", 0.3047972654),
    (9014, "fathom", 1.8288),
    (9030, "nautical mile", 1852.0),
    (9031, "German legal metre", 1.0000135965),
    (9033, "US survey chain", 20.11684023368047),
    (9034, "US survey link", 0.2011684023368047),
    (9035, "US survey mile", 1609.3472186944375),
    (9036, "kilometre", 1000.0),
    (9037, "Clarke's yard", 0.9143917962),
    (9038, "Clarke's chain", 20.1166195164),
    (9039, "Clarke's link", 0.201166195164),
    (9040, "British yard (Sears 1922)", 0.9143984146160287),
    (9041, "British foot (Sears 1922)", 0.3047994715386762),
    (9042, "British chain (Sears 1922)", 20.116765121552632),
    (9043, "British link (Sears 1922)", 0.2011676512155263),
    (9050, "British yard (Benoit 1895 A)", 0.9143992),
    (9051, "British foot (Benoit 1895 A)", 0.3047997333333333),
    (9052, "British chain (Benoit 1895 A)", 20.1167824),
    (9053, "British link (Benoit 1895 A)", 0.201167824),
    (9060, "British yard (Benoit 1895 B)", 0.9143992042898124),
    (9061, "British foot (Benoit 1895 B)", 0.30479973476327077),
    (9062, "British chain (Benoit 1895 B)", 20.116782494375872),
    (9063, "British link (Benoit 1895 B)", 0.2011678249437587),
    (9070, "British foot (1865)", 0.30480083333333335),
    (9080, "Indian foot", 0.30479951024814694),
    (9081, "Indian foot (1937)", 0.30479841),
    (9082, "Indian foot (1962)", 0.3047996),
    (9083, "Indian foot (1975)", 0.3047995),
    (9084, "Indian yard", 0.9143985307444408),
    (9085, "Indian yard (1937)", 0.91439523),
    (9086, "Indian yard (1962)", 0.9143988),
    (9087, "Indian yard (1975)", 0.9143985),
    (9093, "Statute mile", 1609.344),
    (9094, "Gold Coast foot", 0.3047997101815088),
    (9095, "British foot (1936)", 0.3048007491),
    (9096, "yard", 0.9144),
    (9097, "chain", 20.1168),
    (9098, "link", 0.201168),
    (9099, "British yard (Sears 1922 truncated)", 0.914398),
    (9300, "British foot (Sears 1922 truncated)", 0.30479933333333337),
    (9301, "British chain (Sears 1922 truncated)", 20.116756),
    (9302, "British link (Sears 1922 truncated)", 0.20116756),
];

/// The linear unit of the GeoKeys (ProjLinearUnitsGeoKey, or ProjLinearUnitSizeGeoKey for a
/// user-defined one) as a PROJ parameter, and its size in metres. A code GDAL finds in no
/// registry leaves its size at a metre, as in libgeotiff's `GTIFGetDefn`.
fn units(k: &GeoKeys) -> (String, f64) {
    match k.short(3076) {
        Some(9002) => ("+units=ft".into(), 0.3048),
        Some(9003) => ("+units=us-ft".into(), 1200.0 / 3937.0),
        Some(9036) => ("+units=km".into(), 1000.0),
        Some(USER_DEFINED) => match k.double(3077) {
            Some(m) if m > 0.0 => (format!("+to_meter={m}"), m),
            _ => ("+units=m".into(), 1.0),
        },
        Some(code) => match LENGTH_UNITS.iter().find(|u| u.0 == code && u.0 != 9001) {
            Some(&(_, _, m)) => (format!("+to_meter={m}"), m),
            None => ("+units=m".into(), 1.0),
        },
        None => ("+units=m".into(), 1.0),
    }
}

/// The size in metres of the GeoKeys' linear unit (a metre when they give none).
fn linear_unit_size(k: &GeoKeys) -> f64 {
    units(k).1
}

/// A name as WKT 1 quotes it: within double quotes, each of its own doubled (as terrio's WKT
/// writer writes a name and its reader reads one).
fn wkt_quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A number as GDAL writes it in WKT: to 15 significant digits.
fn wkt_number(v: f64) -> String {
    format!("{v:.14e}").parse::<f64>().map(|r| r.to_string()).unwrap_or_else(|_| v.to_string())
}

/// The WKT `UNIT` GDAL gives the linear unit of projected GeoKeys when PROJ strings have no name
/// for it: an EPSG unit by its registry name and code, a user-defined one by the `LUnits = `
/// name of its citation (which GDAL writes for it), else `unknown`. `None` for the units PROJ
/// strings name (metre, foot, US survey foot, kilometre), whose WKT is already GDAL's.
fn linear_unit_wkt(k: &GeoKeys) -> Option<String> {
    let size = linear_unit_size(k);
    match k.short(3076) {
        None | Some(9001 | 9002 | 9003 | 9036) => None,
        Some(USER_DEFINED) => {
            let name = [3073, 1026].iter().find_map(|id| k.text(*id)?.split('|').find_map(|p| p.trim().strip_prefix("LUnits = ")));
            Some(format!("UNIT[{},{}]", wkt_quoted(name.unwrap_or("unknown").trim()), wkt_number(size)))
        }
        Some(code) => {
            let (_, name, m) = LENGTH_UNITS.iter().find(|u| u.0 == code)?;
            Some(format!("UNIT[\"{name}\",{},AUTHORITY[\"EPSG\",\"{code}\"]]", wkt_number(*m)))
        }
    }
}

/// A WKT 1 CRS with the `UNIT` node of its root (its last, after the `GEOGCS`'s) replaced by
/// `unit`. `None` for a WKT with a PROJ4 extension, which describes the CRS instead.
fn with_unit(wkt: &str, unit: &str) -> Option<String> {
    if wkt.contains("EXTENSION[") {
        return None;
    }
    let start = wkt.rfind(",UNIT[")? + 1;
    let mut depth = 0;
    let len = wkt[start..]
        .char_indices()
        .find(|&(_, c)| {
            depth += match c {
                '[' => 1,
                ']' => -1,
                _ => 0,
            };
            c == ']' && depth == 0
        })?
        .0;
    Some(format!("{}{unit}{}", &wkt[..start], &wkt[start + len + 1..]))
}

/// The angular unit of the GeoKeys when it is not the degree, as GDAL reads it: its size in
/// degrees and its WKT `UNIT` (GeogAngularUnitsGeoKey by the codes libgeotiff knows, else
/// GeogAngularUnitSizeGeoKey in radians with the `AUnits = ` name of the citation).
fn angular_unit(k: &GeoKeys) -> Option<(f64, String)> {
    const RAD: f64 = 180.0 / std::f64::consts::PI;
    let (name, degrees): (String, f64) = match k.short(2054)? {
        9102 | 9107 | 9108 | 9110 | 9122 => return None,
        9101 => ("radian".into(), RAD),
        9103 => ("arc-minute".into(), 1.0 / 60.0),
        9104 => ("arc-second".into(), 1.0 / 3600.0),
        9105 => ("grad".into(), 0.9),
        9106 => ("gon".into(), 0.9),
        9109 => ("microradian".into(), RAD / 1e6),
        USER_DEFINED => {
            let radians = k.double(2055).filter(|r| *r > 0.0)?;
            let name = k.text(2049).and_then(|c| c.split('|').find_map(|p| p.trim().strip_prefix("AUnits = ")));
            (name.unwrap_or("unknown").trim().to_string(), radians * RAD)
        }
        _ => return None,
    };
    // GDAL's WKT: the size in radians, and the registry's grad (PROJ names only the degree and
    // the grad by their codes).
    let unit = match name.as_str() {
        "grad" => "UNIT[\"grad\",0.015707963267949,AUTHORITY[\"EPSG\",\"9105\"]]".to_string(),
        _ => format!("UNIT[{},{}]", wkt_quoted(&name), wkt_number(degrees * 0.0174532925199433)),
    };
    Some((degrees, unit))
}

/// GeoKeys for a CRS without an EPSG code, as GDAL writes them: from its PROJ string when it has
/// one, else from the projection engine's model of it. `None` when GeoKeys cannot describe it.
pub fn user_defined_keys(crs: &SpatialRef, name: &str) -> Option<GeoKeyValues> {
    if let Some(p) = &crs.proj4 {
        return keys_for_proj(p, name, None);
    }
    // A WKT: its PROJ string as terrio reads it, on the EPSG geographic CRS it is identified on.
    if let Some(p) = crs.wkt.as_deref().and_then(crate::wkt_proj) {
        let geog = crs.geographic_code();
        return keys_for_proj(&p, name, geog);
    }
    wbprojection_keys(crs, name)
}

/// GeoKeys from wbprojection's model of a CRS, when terrio is built with it.
#[cfg(feature = "wbprojection-fallback")]
fn wbprojection_keys(crs: &SpatialRef, name: &str) -> Option<GeoKeyValues> {
    crs.to_wbp().ok().and_then(|c| keys_for_crs(&c, name))
}

#[cfg(not(feature = "wbprojection-fallback"))]
fn wbprojection_keys(_: &SpatialRef, _: &str) -> Option<GeoKeyValues> {
    None
}

/// GeoKeys for a PROJ string, as GDAL writes them, from its WKT 1 description, on an EPSG
/// geographic CRS when given one. `None` when the method has no GeoTIFF equivalent or the WKT 1
/// needs the PROJ string to describe it.
fn keys_for_proj(proj4: &str, name: &str, geog: Option<u32>) -> Option<GeoKeyValues> {
    let d = crate::describe(proj4)?;
    let mut v = GeoKeyValues::default();
    // The geographic CRS it was identified on, unless the PROJ string shifts its datum itself.
    let geog = d.geog.or(geog.filter(|_| d.towgs84.is_none() && d.pm == 0.0));
    match geog {
        Some(code) => v.shorts.push((2048, u16::try_from(code).ok()?)),
        None => {
            v.shorts.push((2048, USER_DEFINED));
            v.shorts.push((2050, USER_DEFINED));
            v.shorts.push((2054, 9102));
            v.shorts.push((2056, d.ellipsoid.and_then(|c| u16::try_from(c).ok()).unwrap_or(USER_DEFINED)));
            v.doubles.push((2057, d.a));
            if d.rf > 0.0 {
                v.doubles.push((2059, d.rf));
            } else {
                v.doubles.push((2058, d.a));
            }
            if d.pm != 0.0 {
                v.shorts.push((2051, USER_DEFINED));
                v.doubles.push((2061, d.pm));
            }
            if let Some(t) = d.towgs84 {
                v.doubles.extend(t.into_iter().map(|x| (2062, x)));
            }
        }
    }
    let Some(method) = d.method else {
        v.shorts.push((1024, 2));
        v.ascii.push((1026, name.to_string()));
        return Some(v);
    };
    let get = |k: &str| d.params.iter().find(|(n, _)| *n == k).map(|(_, x)| *x).unwrap_or(0.0);
    let (lat, lon, k, fe, fn_) =
        (get("latitude_of_origin"), get("central_meridian"), get("scale_factor"), get("false_easting"), get("false_northing"));
    let (clat, clon) = (get("latitude_of_center"), get("longitude_of_center"));
    let sp = |i: u8| get(if i == 1 { "standard_parallel_1" } else { "standard_parallel_2" });
    let natural = vec![(3081, lat), (3080, lon), (3092, k), (3082, fe), (3083, fn_)];
    let oblique =
        vec![(3089, clat), (3088, clon), (3094, get("azimuth")), (3096, get("rectified_grid_angle")), (3093, k), (3082, fe), (3083, fn_)];
    let (code, params): (u16, Vec<(u16, f64)>) = match method {
        "Transverse_Mercator" => (1, natural),
        "Transverse_Mercator_South_Orientated" => (27, natural),
        "Mercator_1SP" => (7, vec![(3081, 0.0), (3080, lon), (3092, k), (3082, fe), (3083, fn_)]),
        "Mercator_2SP" => (7, vec![(3078, sp(1)), (3080, lon), (3082, fe), (3083, fn_)]),
        "Lambert_Conformal_Conic_1SP" => (9, natural),
        "Lambert_Conformal_Conic_2SP" => (8, vec![(3078, sp(1)), (3079, sp(2)), (3085, lat), (3084, lon), (3086, fe), (3087, fn_)]),
        "Albers_Conic_Equal_Area" => (11, vec![(3078, sp(1)), (3079, sp(2)), (3081, clat), (3080, clon), (3082, fe), (3083, fn_)]),
        "Equidistant_Conic" => (13, vec![(3078, sp(1)), (3079, sp(2)), (3081, clat), (3080, clon), (3082, fe), (3083, fn_)]),
        "Lambert_Azimuthal_Equal_Area" => (10, vec![(3089, clat), (3088, clon), (3082, fe), (3083, fn_)]),
        "Azimuthal_Equidistant" => (12, vec![(3089, clat), (3088, clon), (3082, fe), (3083, fn_)]),
        "Stereographic" => (14, vec![(3089, lat), (3088, lon), (3092, k), (3082, fe), (3083, fn_)]),
        "Polar_Stereographic" if d.params.iter().any(|(n, _)| *n == "scale_factor") => {
            (15, vec![(3081, lat), (3095, lon), (3092, k), (3082, fe), (3083, fn_)])
        }
        "Polar_Stereographic" => (15, vec![(3081, lat), (3095, lon), (3082, fe), (3083, fn_)]),
        "Oblique_Stereographic" => (16, natural),
        "Equirectangular" => (17, vec![(3078, sp(1)), (3089, lat), (3088, lon), (3082, fe), (3083, fn_)]),
        "Cassini_Soldner" => (18, vec![(3081, lat), (3080, lon), (3082, fe), (3083, fn_)]),
        // At their centre, as GDAL writes them.
        "Gnomonic" => (19, vec![(3089, lat), (3088, lon), (3082, fe), (3083, fn_)]),
        "Miller_Cylindrical" => (20, vec![(3088, get("longitude_of_center")), (3082, fe), (3083, fn_)]),
        "Orthographic" => (21, vec![(3089, lat), (3088, lon), (3082, fe), (3083, fn_)]),
        "Polyconic" => (22, vec![(3081, lat), (3080, lon), (3082, fe), (3083, fn_)]),
        "Robinson" => (23, vec![(3088, clon), (3082, fe), (3083, fn_)]),
        "Sinusoidal" => (24, vec![(3088, clon), (3082, fe), (3083, fn_)]),
        "VanDerGrinten" => (25, vec![(3088, lon), (3082, fe), (3083, fn_)]),
        "New_Zealand_Map_Grid" => (26, vec![(3081, lat), (3080, lon), (3082, fe), (3083, fn_)]),
        "Cylindrical_Equal_Area" => (28, vec![(3078, sp(1)), (3088, lon), (3082, fe), (3083, fn_)]),
        // Hotine oblique Mercator, variant A and B (with Swiss oblique Mercator, as azimuths of
        // 90°).
        "Hotine_Oblique_Mercator" => (3, oblique),
        "Hotine_Oblique_Mercator_Azimuth_Center" => (9815, oblique),
        _ => return None,
    };
    v.shorts.push((1024, 1));
    v.ascii.push((1026, name.to_string()));
    v.shorts.push((3072, USER_DEFINED));
    v.shorts.push((3074, USER_DEFINED));
    v.shorts.push((3075, code));
    // A unit of no EPSG code PROJ strings name: user-defined, by its size
    // (ProjLinearUnitSizeGeoKey), as GDAL writes it.
    match d.unit_code {
        Some(code) => v.shorts.push((3076, u16::try_from(code).ok()?)),
        None => {
            v.shorts.push((3076, USER_DEFINED));
            v.doubles.push((3077, d.to_meter));
        }
    }
    // ProjLinearUnitsInterpCorrectGeoKey: the false eastings and northings are in that unit.
    if d.to_meter != 1.0 {
        v.shorts.push((3059, 1));
    }
    v.doubles.extend(params);
    Some(v)
}

/// GeoKeys for a CRS of the projection engine's model, as GDAL writes them: `(short keys,
/// double keys, ascii keys)` by key id. `None` when the projection method has no GeoTIFF
/// equivalent.
#[cfg(feature = "wbprojection-fallback")]
fn keys_for_crs(crs: &wbprojection::Crs, name: &str) -> Option<GeoKeyValues> {
    use wbprojection::ProjectionKind as K;
    let p = crs.projection.params();
    let mut v = GeoKeyValues::default();
    // The geographic CRS: a registered one for the common datums, otherwise its ellipsoid.
    let gcs = match crs.datum.name {
        "WGS 84" | "WGS84" => Some(4326),
        "NAD 83" | "NAD83" => Some(4269),
        "NAD 27" | "NAD27" => Some(4267),
        "ETRS89" => Some(4258),
        _ => None,
    };
    match gcs {
        Some(code) => v.shorts.push((2048, code)),
        None => {
            let e = &crs.datum.ellipsoid;
            v.shorts.push((2048, USER_DEFINED));
            v.shorts.push((2050, USER_DEFINED));
            v.shorts.push((2054, 9102));
            v.doubles.push((2057, e.a));
            if e.f > 0.0 {
                v.doubles.push((2059, 1.0 / e.f));
            } else {
                v.doubles.push((2058, e.b));
            }
        }
    }
    if matches!(p.kind, K::Geographic) {
        v.shorts.push((1024, 2));
        v.ascii.push((1026, name.to_string()));
        return Some(v);
    }
    let (fe, fn_, lat0, lon0, k) = (p.false_easting, p.false_northing, p.lat0, p.lon0, p.scale);
    let (method, params): (u16, Vec<(u16, f64)>) = match &p.kind {
        K::TransverseMercator => (1, vec![(3081, lat0), (3080, lon0), (3092, k), (3082, fe), (3083, fn_)]),
        K::Utm { zone, south } => {
            let lon = -183.0 + 6.0 * *zone as f64;
            (1, vec![(3081, 0.0), (3080, lon), (3092, 0.9996), (3082, 500000.0), (3083, if *south { 10_000_000.0 } else { 0.0 })])
        }
        K::Mercator => (7, vec![(3081, lat0), (3080, lon0), (3092, k), (3082, fe), (3083, fn_)]),
        K::LambertConformalConic { lat1, lat2: Some(lat2) } => {
            (8, vec![(3078, *lat1), (3079, *lat2), (3085, lat0), (3084, lon0), (3086, fe), (3087, fn_)])
        }
        K::LambertConformalConic { lat2: None, .. } => (9, vec![(3081, lat0), (3080, lon0), (3092, k), (3082, fe), (3083, fn_)]),
        K::AlbersEqualAreaConic { lat1, lat2 } => {
            (11, vec![(3078, *lat1), (3079, *lat2), (3081, lat0), (3080, lon0), (3082, fe), (3083, fn_)])
        }
        K::LambertAzimuthalEqualArea => (10, vec![(3089, lat0), (3088, lon0), (3082, fe), (3083, fn_)]),
        K::AzimuthalEquidistant => (12, vec![(3089, lat0), (3088, lon0), (3082, fe), (3083, fn_)]),
        K::Stereographic => (14, vec![(3089, lat0), (3088, lon0), (3093, k), (3082, fe), (3083, fn_)]),
        K::ObliqueStereographic => (16, vec![(3081, lat0), (3080, lon0), (3092, k), (3082, fe), (3083, fn_)]),
        K::PolarStereographic { north, lat_ts } => {
            let pole = if *north { 90.0 } else { -90.0 };
            match lat_ts {
                Some(ts) => (15, vec![(3081, *ts), (3095, lon0), (3082, fe), (3083, fn_)]),
                None => (15, vec![(3081, pole), (3095, lon0), (3092, k), (3082, fe), (3083, fn_)]),
            }
        }
        K::Equirectangular { lat_ts } => (17, vec![(3078, *lat_ts), (3081, lat0), (3088, lon0), (3082, fe), (3083, fn_)]),
        K::Sinusoidal => (24, vec![(3088, lon0), (3082, fe), (3083, fn_)]),
        _ => return None,
    };
    v.shorts.push((1024, 1));
    v.ascii.push((1026, name.to_string()));
    v.shorts.push((3072, USER_DEFINED));
    v.shorts.push((3074, USER_DEFINED));
    v.shorts.push((3075, method));
    v.shorts.push((3076, 9001));
    v.doubles.extend(params);
    Some(v)
}

/// GeoKey values by key id, as [`SpatialRef::to_geokeys`] gives them: [`encode`](Self::encode)
/// writes them in the three TIFF tags.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct GeoKeyValues {
    /// SHORT keys.
    pub shorts: Vec<(u16, u16)>,
    /// DOUBLE keys, in the order of GeoDoubleParamsTag; consecutive values of one key (the seven
    /// of GeogTOWGS84GeoKey) are its values.
    pub doubles: Vec<(u16, f64)>,
    /// ASCII keys.
    pub ascii: Vec<(u16, String)>,
}

impl GeoKeyValues {
    /// The GeoKeyDirectoryTag, GeoDoubleParamsTag and GeoAsciiParamsTag holding the keys, as
    /// GDAL writes them: the keys sorted by id, each ASCII key ended by `|`, and GeoTIFF 1.1
    /// (minor revision 1) when they hold a vertical CRS (VerticalCSTypeGeoKey), which GDAL strips
    /// from 1.0 files. Fails when a count or offset does not fit the directory's 16 bits (an ASCII
    /// key past 65,535 bytes, such as a very long ESRI PE citation).
    ///
    /// ```
    /// let keys = terrio_crs::SpatialRef::from_epsg(4326)?.to_geokeys()?;
    /// let tags = keys.encode()?;
    /// // GeoTIFF 1.0 (1, 1, 0) and two keys: GTModelTypeGeoKey 2 and GeographicTypeGeoKey 4326.
    /// assert_eq!(&tags.directory[..4], &[1, 1, 0, 2]);
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn encode(&self) -> Result<GeoKeyTags> {
        self.encode_with_minor_revision(u16::from(self.shorts.iter().any(|k| k.0 == 4096)))
    }

    /// The tags of [`encode`](Self::encode) with the directory's minor revision given: 0 for
    /// GeoTIFF 1.0 (KeyRevision 1.0, as LAS records hold them), 1 for GeoTIFF 1.1.
    pub fn encode_with_minor_revision(&self, minor: u16) -> Result<GeoKeyTags> {
        let too_long = |what: &str| Error::Crs(format!("GeoKeys: {what} does not fit a GeoKey directory's 16 bits"));
        if self.doubles.len() > usize::from(u16::MAX) {
            return Err(too_long("GeoDoubleParamsTag"));
        }
        let mut shorts = self.shorts.clone();
        shorts.sort_by_key(|k| k.0);
        let mut keys: Vec<[u16; 4]> = shorts.iter().map(|(id, val)| [*id, 0, 1, *val]).collect();
        keys.extend(self.double_keys());
        let mut ascii = String::new();
        for (id, text) in &self.ascii {
            let text = format!("{text}|");
            let count = u16::try_from(text.len()).map_err(|_| too_long("an ASCII key"))?;
            let offset = u16::try_from(ascii.len()).map_err(|_| too_long("GeoAsciiParamsTag"))?;
            keys.push([*id, GEO_ASCII_PARAMS, count, offset]);
            ascii.push_str(&text);
        }
        keys.sort_by_key(|k| k[0]);
        let count = u16::try_from(keys.len()).map_err(|_| too_long("the number of keys"))?;
        let mut directory = vec![1u16, 1, minor, count];
        directory.extend(keys.iter().flatten());
        Ok(GeoKeyTags { directory, doubles: self.doubles.iter().map(|d| d.1).collect(), ascii })
    }

    /// The directory entries of the DOUBLE keys: `[id, GeoDoubleParamsTag, count, offset]`.
    fn double_keys(&self) -> Vec<[u16; 4]> {
        let mut out: Vec<[u16; 4]> = Vec::new();
        for (i, (id, _)) in self.doubles.iter().enumerate() {
            match out.last_mut() {
                Some(last) if last[0] == *id => last[2] += 1,
                _ => out.push([*id, GEO_DOUBLE_PARAMS, 1, i as u16]),
            }
        }
        out
    }
}

/// The three TIFF tags of GeoKeys (see [`GeoKeyValues::encode`]).
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct GeoKeyTags {
    /// GeoKeyDirectoryTag (34735): the header, then `[id, location, count, value or offset]`.
    pub directory: Vec<u16>,
    /// GeoDoubleParamsTag (34736), empty when no key is a double.
    pub doubles: Vec<f64>,
    /// GeoAsciiParamsTag (34737), empty when no key is text.
    pub ascii: String,
}

impl SpatialRef {
    /// The CRS of a GeoKey directory and its double and ASCII parameters, as GDAL reads them:
    /// see [`read`], whose CRS this is. Fails when the GeoKeys give no CRS terrio reads, naming an
    /// unsupported projection method.
    ///
    /// ```
    /// use terrio_crs::SpatialRef;
    ///
    /// // A geographic CRS by its code (GTModelTypeGeoKey 2, GeographicTypeGeoKey 4326).
    /// let crs = SpatialRef::from_geokeys(&[1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326], &[], "")?;
    /// assert_eq!(crs.epsg, Some(4326));
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn from_geokeys(directory: &[u16], doubles: &[f64], ascii: &str) -> Result<SpatialRef> {
        let read = read(directory, doubles, ascii);
        read.crs.ok_or_else(|| match read.unsupported_method {
            Some(m) => Error::Crs(format!(
                "GeoTIFF projection method {m} (ProjCoordTransGeoKey) is not supported, or its parameters are incomplete"
            )),
            None => Error::Crs("the GeoKeys give no CRS".into()),
        })
    }

    /// The GeoKeys of the CRS, as GDAL 3.12.1 writes them (`GTIFSetFromOGISDefnEx`), without the
    /// raster type (GTRasterTypeGeoKey), which is the raster's:
    /// - a registered geographic or projected CRS by its EPSG code, a geographic 3D CRS by its
    ///   geographic 2D CRS's and its own as the vertical CRS;
    /// - a compound CRS of EPSG CRSs by its horizontal and vertical codes, and its name;
    /// - a CRS without an EPSG code by its projection method and parameters (see
    ///   [`user_defined_keys`]);
    /// - else (an engineering CRS, a method GeoTIFF has no code for, a CRS whose code is not a
    ///   horizontal one), GDAL's fallback: its WKT in an ESRI PE citation (`ESRI PE String =`),
    ///   with a user-defined model type.
    ///
    /// A geographic 3D CRS is written as its geographic 2D CRS with its own code as the vertical
    /// CRS, as GDAL writes it (EPSG:4979 as `2048=4326`, `4096=4979`). Fails for an unknown CRS,
    /// and for a CRS that needs the fallback and has no WKT.
    ///
    /// ```
    /// use terrio_crs::SpatialRef;
    ///
    /// let keys = SpatialRef::from_proj4("+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +datum=WGS84")?.to_geokeys()?;
    /// // A user-defined projected CRS (ProjectedCSTypeGeoKey 32767), Lambert conformal conic 2SP.
    /// assert!(keys.shorts.contains(&(3072, 32767)) && keys.shorts.contains(&(3075, 8)));
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn to_geokeys(&self) -> Result<GeoKeyValues> {
        if self.is_unknown() {
            return Err(Error::Crs("an unknown CRS has no GeoKeys".into()));
        }
        let mut v = GeoKeyValues::default();
        let engineering = self.wkt.as_deref().is_some_and(engineering_cs);
        let registered = self.epsg.filter(|_| !engineering).and_then(horizontal_geokey);
        // A compound CRS of EPSG CRSs: its horizontal CRS's keys, its vertical CRS's and its name,
        // in GeoTIFF 1.1 as GDAL 3.12.1 writes them (`GTIFSetFromOGISDefnEx`); a compound code in
        // a horizontal key reads as another CRS.
        let compound = (!engineering).then(|| self.compound_parts()).flatten().and_then(|(h, v)| {
            let (h, geographic) = horizontal_geokey(h)?;
            Some((h, u16::try_from(v).ok()?, geographic))
        });
        match (compound, registered) {
            (Some((h, vertical, geographic)), _) => {
                v.shorts.push((1024, if geographic { 2 } else { 1 }));
                v.shorts.push((if geographic { 2048 } else { 3072 }, h));
                v.shorts.push((4096, vertical));
                v.ascii.push((1026, self.name().unwrap_or_else(|| "unnamed".into())));
            }
            (None, Some((code, geographic))) => {
                v.shorts.push((1024, if geographic { 2 } else { 1 }));
                match geographic.then(|| geographic_2d_of(u32::from(code))).flatten().and_then(|h| u16::try_from(h).ok()) {
                    Some(h) => {
                        v.shorts.push((2048, h));
                        v.shorts.push((4096, code));
                    }
                    None => v.shorts.push((if geographic { 2048 } else { 3072 }, code)),
                }
            }
            (None, None) => {
                let name = self.name().unwrap_or_else(|| "unnamed".into());
                // Keep engineering and unrepresentable coded definitions as WKT. In particular,
                // an engineering identifier must never be written as a projected EPSG code.
                let user = if engineering || self.epsg.is_some() { None } else { user_defined_keys(self, &name) };
                match user {
                    Some(keys) => v = keys,
                    None => {
                        // GDAL's fallback for methods GeoTIFF cannot describe: the WKT in an ESRI PE
                        // citation, with a user-defined model type.
                        let wkt = self.to_wkt().ok_or_else(|| Error::Crs("the CRS has no WKT definition".into()))?;
                        v.shorts.push((1024, USER_DEFINED));
                        // GeoTIFF 1.1 requirement 8.10 requires a general citation for this model.
                        v.ascii.push((1026, name));
                        v.ascii.push((3073, format!("ESRI PE String = {wkt}")));
                    }
                }
            }
        }
        Ok(v)
    }
}

/// Whether the retained WKT describes an engineering grid, independently of its root ID.
fn engineering_cs(wkt: &str) -> bool {
    let root = wkt.split(['[', '(']).next().unwrap_or_default().trim().to_ascii_uppercase();
    matches!(root.as_str(), "LOCAL_CS" | "ENGCRS" | "ENGINEERINGCRS")
}

/// A registered horizontal code usable by GeoTIFF 1.1 (requirements 12.4 and 13.4), and
/// whether it is geographic. Engineering, vertical, geocentric and unknown codes need WKT.
fn horizontal_geokey(code: u32) -> Option<(u16, bool)> {
    let short = u16::try_from(code).ok().filter(|code| (1024..USER_DEFINED).contains(code))?;
    let wkt = crate::epsg_wkt(code)?;
    let geographic = SpatialRef::from_epsg(code).is_ok_and(|s| s.is_geographic());
    // The table holds GDAL's canonical WKT; projected roots use PROJCS or PROJCRS.
    (geographic || wkt.starts_with("PROJ")).then_some((short, geographic))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_epsg_geographic_crs_of_a_user_defined_projection() {
        // A transverse Mercator on NAD83(HARN) (GeographicTypeGeoKey 4152): its WKT keeps the
        // realization, not PROJ's +datum=NAD83.
        let keys = [
            1, 1, 0, 11, 1024, 0, 1, 1, 2048, 0, 1, 4152, 3072, 0, 1, 32767, 3074, 0, 1, 32767, 3075, 0, 1, 1, 3076, 0, 1, 9001, 3080,
            34736, 1, 0, 3081, 34736, 1, 1, 3082, 34736, 1, 2, 3083, 34736, 1, 3, 3092, 34736, 1, 4,
        ];
        let doubles = [-100.0, 0.0, 500000.0, 0.0, 0.9996];
        let crs = user_defined_crs(&GeoKeys { keys: &keys, doubles: &doubles, ascii: "" }).unwrap();
        let wkt = crs.wkt.unwrap();
        assert!(crs.proj4.is_none() && wkt.contains("AUTHORITY[\"EPSG\",\"4152\"]"), "{wkt}");
        assert!(wkt.contains("PARAMETER[\"central_meridian\",-100]"), "{wkt}");
    }

    #[test]
    fn reads_gdal_user_defined_lcc() {
        // As GDAL writes a WGS 84 Lambert Conformal Conic with no EPSG code.
        let keys = [
            1, 1, 0, 14, 1024, 0, 1, 1, 1025, 0, 1, 1, 2048, 0, 1, 32767, 2050, 0, 1, 6326, 2054, 0, 1, 9102, 3072, 0, 1, 32767, 3074, 0,
            1, 32767, 3075, 0, 1, 8, 3076, 0, 1, 9001, 3078, 34736, 1, 0, 3079, 34736, 1, 1, 3084, 34736, 1, 2, 3085, 34736, 1, 3, 3086,
            34736, 1, 4,
        ];
        let doubles = [33.0, 45.0, -96.0, 39.0, 0.0];
        let crs = user_defined_crs(&GeoKeys { keys: &keys, doubles: &doubles, ascii: "" }).unwrap();
        let p = crs.proj4.unwrap();
        assert!(p.starts_with("+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +x_0=0 +y_0=0 +datum=WGS84"), "{p}");
        assert!(crs.wkt.is_some());
    }

    #[test]
    fn identifies_user_defined_utm() {
        assert_eq!(user_defined_projected(Some(16010), None, None, Some("WGS 84 / UTM zone 10N".into())), Some(32610));
        assert_eq!(user_defined_projected(None, Some(4269), None, Some("NAD83 / UTM zone 15N".into())), Some(26915));
        assert_eq!(user_defined_projected(Some(16133), None, None, None), Some(32733));
        // A citation names only a projected CRS: a vertical one is no projected raster's (with
        // wbprojection; without it, citations name no code but a UTM zone).
        assert_eq!(user_defined_projected(None, None, None, Some("EPSG:5773".into())), None);
        // A zone out of range, from a citation, names no code (and does not overflow).
        assert_eq!(user_defined_projected(None, None, None, Some("WGS 84 / UTM zone 4294961611N".into())), None);
        assert_eq!(user_defined_projected(None, None, None, Some("WGS 84 / UTM zone 61N".into())), None);
        assert_eq!(user_defined_projected(Some(16015), Some(32767), Some(6269), None), Some(26915));
        // The geographic CRS's datum wins over the citation, as its code does for GDAL.
        assert_eq!(user_defined_projected(Some(16015), Some(4326), None, Some("NAD83 / UTM zone 15N".into())), Some(32615));
        assert_eq!(user_defined_projected(Some(16015), Some(4269), Some(6326), None), Some(26915));
        // A UTM zone on NAD83(HARN), by its geographic CRS or its datum, is no WGS 84 (or NAD83)
        // UTM zone (#209).
        assert_eq!(user_defined_projected(Some(16033), Some(32767), Some(6152), None), None);
        // ECOSTRESS: user-defined keys whose geographic citation names WGS 84's datum; GDAL reads EPSG:32610.
        let keys = [
            1, 1, 0, 8, 1024, 0, 1, 1, 1026, 34737, 22, 0, 2048, 0, 1, 32767, 2049, 34737, 92, 22, 2050, 0, 1, 32767, 3072, 0, 1, 32767,
            3074, 0, 1, 16010, 3076, 0, 1, 9001,
        ];
        let ascii = "WGS 84 / UTM zone 10N|GCS Name = WGS 84|Datum = World Geodetic System 1984|Ellipsoid = WGS 84|Primem = Greenwich||";
        assert_eq!(read(&keys, &[], ascii).crs.and_then(|c| c.epsg), Some(32610));
        // The same keys with another datum named keep it: not WGS 84's zone.
        let ascii = ascii.replace("World Geodetic System 1984", "Some Local Datum        ");
        assert_eq!(read(&keys, &[], &ascii).crs.and_then(|c| c.epsg), None);
        assert_eq!(user_defined_projected(Some(16010), Some(4152), None, Some("NAD83 / UTM zone 10N".into())), None);
    }

    #[test]
    fn compact_geo_keys_only_name_registered_horizontal_crss() {
        assert_eq!(horizontal_geokey(32615), Some((32615, false)));
        assert_eq!(horizontal_geokey(4326), Some((4326, true)));
        assert_eq!(horizontal_geokey(4979), Some((4979, true)));
        for code in [5800, 4978, 5773, 9707, 32767, 999999] {
            assert_eq!(horizontal_geokey(code), None, "{code}");
        }
    }
}
