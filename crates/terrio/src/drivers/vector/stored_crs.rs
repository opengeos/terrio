// Written to match GDAL v3.13.3 stored Shapefile CRS behavior
// (ogr/ogrsf_frmts/shape/ogrshapelayer.cpp), without translating its code.

//! Shared validation of stored vector CRS definitions, without requiring parser support.

use crate::crs::SpatialRef;

/// A stored definition with whitespace/BOM/legacy terminators removed. Retain balanced
/// CRS-shaped WKT even when this build cannot parse it (GDAL 3.13.3 reference).
/// A source label enables diagnostics; redundant definitions with a valid authority
/// fallback may be ignored silently.
pub(super) fn definition<'a>(text: &'a str, source: Option<&str>) -> Option<&'a str> {
    let text = text
        .trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '\0' | '\x1a'))
        .trim_start();
    if text.is_empty() {
        return None;
    }
    if is_crs_definition(text) || (crs_wkt_opener(text).is_none() && SpatialRef::parse_literal(text).is_ok()) {
        return Some(text);
    }
    if let Some(source) = source {
        let quoted = quote_text(&text.chars().take(80).collect::<String>());
        crate::warnings::warn(format!(
            "{} holds no CRS definition terrio can read ('{quoted}'): the layer is read without a CRS",
            quote_text(source)
        ));
    }
    None
}

/// Quotes diagnostic text using the whitespace/control policy of [`quoted_as_space`].
pub(super) fn quote_text(text: &str) -> String {
    text.chars().map(|c| if quoted_as_space(c) { ' ' } else { c }).collect()
}

/// Quote whitespace, control codes and Unicode 15.1 General_Category=Cf format controls
/// as spaces. The Cf ranges were checked against Python's unicodedata table; printable
/// non-ASCII text is preserved, while terminal escapes, bidi controls and invisible tags are not.
fn quoted_as_space(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}' |
            '\u{0890}'..='\u{0891}' | '\u{08E2}' | '\u{180E}' | '\u{200B}'..='\u{200F}' |
            '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' |
            '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}' |
            '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' |
            '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

/// The keywords a CRS's WKT starts with: WKT 1's (OGC 18-010r7 B.8, and ESRI's `VERTCS`) and
/// WKT 2's (OGC 18-010r7, and 12-063r5's `IMAGECRS`).
const CRS_WKT_KEYWORDS: [&str; 24] = [
    "GEOGCS",
    "PROJCS",
    "GEOCCS",
    "VERT_CS",
    "VERTCS",
    "LOCAL_CS",
    "COMPD_CS",
    "FITTED_CS",
    "GEODCRS",
    "GEODETICCRS",
    "GEOGCRS",
    "GEOGRAPHICCRS",
    "PROJCRS",
    "PROJECTEDCRS",
    "DERIVEDPROJCRS",
    "VERTCRS",
    "VERTICALCRS",
    "ENGCRS",
    "ENGINEERINGCRS",
    "PARAMETRICCRS",
    "TIMECRS",
    "IMAGECRS",
    "COMPOUNDCRS",
    "BOUNDCRS",
];

/// The position of the bracket that opens `text`'s first WKT node, when the text before it is a
/// CRS's keyword (`PROJCS[`, `engcrs (`).
fn crs_wkt_opener(text: &str) -> Option<usize> {
    let open = text.find(['[', '('])?;
    CRS_WKT_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(text[..open].trim())).then_some(open)
}

/// Whether `text` has the form of a CRS definition, whether terrio reads it or not: WKT whose root
/// is opened by `[` or `(` after a CRS's keyword and has its brackets and its quotes balanced, with
/// nothing after it but another definition, on the next line or after a comma (`,VERTCS[...]`); or
/// a reference (`is_crs_reference`). The roots are taken one after the other, and their brackets
/// counted, not parsed: WKT may nest deeper than a parser recurses.
fn is_crs_definition(text: &str) -> bool {
    let mut text = text.trim();
    loop {
        if !text.contains(['[', '(']) {
            return is_crs_reference(text);
        }
        let Some(open) = crs_wkt_opener(text) else {
            return false;
        };
        let (mut depth, mut quoted, mut end) = (0usize, false, None);
        // Every delimiter is ASCII, which no byte of a UTF-8 sequence of another character is.
        for (i, b) in text.bytes().enumerate().skip(open) {
            match b {
                b'"' => quoted = !quoted,
                b'[' | b'(' if !quoted => depth += 1,
                b']' | b')' if !quoted => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            return false;
        };
        let rest = &text[end..];
        let next = rest.trim_start();
        if next.is_empty() {
            return true;
        }
        let separator = &rest[..rest.len() - next.len()];
        text = match next.strip_prefix(',') {
            Some(after) => after.trim_start(),
            None if separator.contains(['\n', '\r']) => next,
            None => return false,
        };
    }
}

/// The authorities of the CRS database of PROJ 9.8.1 (`get_authorities()` of pyproj 3.8.0, which
/// bundles it).
const CRS_AUTHORITIES: [&str; 8] = ["EPSG", "ESRI", "IAU_2015", "IGNF", "NKG", "NRCAN", "OGC", "PROJ"];

/// Whether `text` refers to a CRS of a known authority (`CRS_AUTHORITIES`, in either case): an OGC
/// URN of a CRS (`urn:ogc:def:crs:IGNF::LAMB93`, or a combined one), or a single `AUTHORITY:CODE`
/// (`IGNF:LAMB93`, not `ERROR:404`).
fn is_crs_reference(text: &str) -> bool {
    let authority = |a: &str| CRS_AUTHORITIES.iter().any(|k| k.eq_ignore_ascii_case(a));
    let code = |c: &str| !c.is_empty() && c.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    // The `authority:version:code` of an OGC URN (OGC 07-092r3 7.2), its version possibly empty.
    // Every textual part of a URN is case-insensitive (7.2).
    let reference = |r: &str| {
        let mut f = r.split(':');
        matches!((f.next(), f.next(), f.next(), f.next()), (Some(a), Some(v), Some(c), None) if authority(a) && (v.is_empty() || code(v)) && code(c))
    };
    fn after<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
        text.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| &text[prefix.len()..])
    }
    if let Some(r) = after(text, "urn:ogc:def:crs:") {
        return reference(r);
    }
    // A CRS combined from CRSs, or from a CRS, a CS and a conversion, or from a datum and a CS
    // (07-092r3 7.5.2, 7.5.4, 7.5.5).
    if let Some(parts) = after(text, "urn:ogc:def:crs,") {
        let kinds = ["crs", "cs", "datum", "coordinateOperation"];
        return parts
            .split(',')
            .all(|p| p.split_once(':').is_some_and(|(kind, r)| kinds.iter().any(|k| k.eq_ignore_ascii_case(kind)) && reference(r)));
    }
    text.split_once(':').is_some_and(|(a, c)| authority(a) && code(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crs_definition_is_told_by_its_form() {
        // Every CRS keyword, in either case, opened by either bracket.
        for k in CRS_WKT_KEYWORDS {
            for text in [format!(r#"{k}["x",UNIT["metre",1]]"#), format!(r#"{}("x")"#, k.to_ascii_lowercase())] {
                assert!(is_crs_definition(&text), "{text}");
            }
        }
        let deep = format!("GEOGCS[\"x\",{}1{}]", "A[".repeat(30_000), "]".repeat(30_000));
        for text in [
            r#"GEOGCS["x",DATUM("y",SPHEROID["z",6378137,298.257223563)]]"#,
            r#"LOCAL_CS["a]b",UNIT["metre",1]]"#,
            r#"LOCAL_CS["say ""grid""",UNIT["metre",1]]"#,
            deep.as_str(),
            // Roots one after the other, on the next line or after a comma.
            "PROJCS[\"x\"]\nVERTCS[\"y\"]",
            "LOCAL_CS[\"x\"] , VERTCS[\"y\"]\nVERTCS[\"z\"]\n",
            "ENGCRS[\"x\"]\nIGNF:LAMB93",
            // A known authority's code, in either case.
            "IGNF:LAMB93",
            "ignf:LAMB93",
            "ESRI:102100",
            "IAU_2015:49900",
            "EPSG:999999",
            "nrcan:CGVD28",
            // OGC URNs (OGC 07-092r3 7.2), in either case, and combined ones (7.5.2, 7.5.4, 7.5.5).
            "urn:ogc:def:crs:IGNF::LAMB93",
            "urn:ogc:def:crs:ignf::LAMB93",
            "URN:OGC:DEF:CRS:EPSG:9.8.15:4326",
            "urn:ogc:def:crs,crs:EPSG::4269,crs:EPSG::5713",
            "urn:ogc:def:crs,crs:EPSG::4326,cs:EPSG::4400,coordinateOperation:EPSG::16031",
            "urn:ogc:def:crs,datum:EPSG::6326,cs:EPSG::6422",
        ] {
            assert!(is_crs_definition(text), "{text}");
        }
        for text in [
            r#"FOO["x"]"#,
            "PROJCS",
            r#"PROJCS["x""#,
            r#"PROJCS["x"]]"#,
            r#"PROJCS["x"]][]"#,
            r#"PROJCS["x]"#,
            // A root followed by what is no definition.
            "PROJCS[\"x\"]\nnot a coordinate reference system",
            r#"LOCAL_CS["x"] notes [1]"#,
            r#"PROJCS["x"],"#,
            r#"PROJCS["x"],,VERTCS["y"]"#,
            r#"PROJCS["x"]VERTCS["y"]"#,
            r#"PROJCS["x"] VERTCS["y"]"#,
            "PROJCS[\"x\"]\tVERTCS[\"y\"]",
            r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984"(SPHEROID["WGS_1984",6378137,298.257223563]]]"#,
            "not a coordinate reference system",
            "http://example.com/crs",
            "IGNF:",
            ":LAMB93",
            "9IGNF:LAMB93",
            "IGNF:LAMB 93",
            "IGNF:LAMB93:2",
            // An authority that is none: other text holding a colon.
            "ERROR:404",
            "NOTE:DRAFT",
            "TODO:FIX",
            "note:draft",
            "Units:feet",
            "file:proj",
            "urn:ogc",
            "urn:ogc:def:crs:NOTE::DRAFT",
            "IGNF:LAMB/93",
            "/tmp/other.prj",
            r"C:\data\other.prj",
            "C:/data/other.prj",
            "urn:ogc:def:crs:IGNF:LAMB93",
            "urn:ogc:def:crs:IGNF::",
            "urn:ogc:def:crs:::LAMB93",
            "urn:ogc:def:crs:IGNF::LAMB 93",
            "urn:ogc:def:datum:EPSG::6326",
            "urn:ogc:def:crs,",
            "urn:ogc:def:crs,crs:EPSG::4269,unit:EPSG::9001",
        ] {
            assert!(!is_crs_definition(text), "{text}");
        }
    }

    #[test]
    fn stored_definition_rejects_prose_but_keeps_unsupported_forms() {
        crate::warnings::take();
        assert_eq!(definition("not a CRS", Some("test layer")), None);
        let warning = crate::warnings::take();
        assert_eq!(warning.len(), 1);
        assert!(warning[0].contains("test layer holds no CRS definition"));
        assert_eq!(definition("\u{feff} PROJCS[\"x\"]\0", Some("test layer")), Some("PROJCS[\"x\"]"));
        assert_eq!(
            definition("LOCAL_CS[\"site grid\",UNIT[\"metre\",1]]", Some("test layer")),
            Some("LOCAL_CS[\"site grid\",UNIT[\"metre\",1]]")
        );
        assert_eq!(definition(" \t\u{feff} PROJCS[\"x\"]\0", Some("test layer")), Some("PROJCS[\"x\"]"));
        assert_eq!(definition(" ", Some("test layer")), None);
        assert!(crate::warnings::take().is_empty());
    }
}
