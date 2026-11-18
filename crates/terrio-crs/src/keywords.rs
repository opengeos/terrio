//! The WKT keywords terrio reads.
//!
//! First come the keywords of each BNF production of WKT 2 (OGC 18-010r11, "Well-known text
//! representation of coordinate reference systems", 2023), with the clause that defines it. Then
//! come the WKT 1 keywords (OGC 01-009) that its Annex C, Table C.2 maps to them, and last those
//! neither standard has: ESRI's, and terrio's own, which terrio keeps reading for backward
//! compatibility. A WKT 2 reader ignores keywords it does not know (Annex B.2.3).
//!
//! The lists the readers use are built from these parts. Where a reader reads only part of a
//! list, a constant names that part, so every reader keeps the keywords it reads. Each constant
//! lists its keywords in the order the readers look for them (see [`crate::wkt1::Node::child_in`]),
//! not always in the BNF's.
//!
//! Keywords are case-insensitive, but an underscore counts (Annex B.2.1): see [`is`].

// WKT 2 (OGC 18-010r11), one constant per production.

/// `<geodetic reference frame keyword> ::= DATUM | TRF | GEODETICDATUM` (8.2.3). WKT 1's datum
/// is `DATUM` too.
const GEODETIC_REFERENCE_FRAME: &[&str] = &["DATUM", "GEODETICDATUM", "TRF"];

/// `<datum ensemble keyword> ::= ENSEMBLE` (7.6).
const DATUM_ENSEMBLE: &[&str] = &["ENSEMBLE"];

/// `<base geographic crs keyword> ::= BASEGEOGCRS` (9.2.1).
const BASE_GEOGRAPHIC: &[&str] = &["BASEGEOGCRS"];

/// `<base geodetic crs keyword> ::= BASEGEODCRS` (9.2.1).
const BASE_GEODETIC: &[&str] = &["BASEGEODCRS"];

/// `<geographic crs keyword> ::= GEOGCRS | GEOGRAPHICCRS` (8.1): a geodetic CRS with an
/// ellipsoidal CS.
const GEOGRAPHIC: &[&str] = &["GEOGCRS", "GEOGRAPHICCRS"];

/// `<geodetic crs keyword> ::= GEODCRS | GEODETICCRS` (8.1): a geodetic CRS with a Cartesian or
/// spherical CS. One with an ellipsoidal CS, as the previous version of the standard allowed, is
/// to be read but not written (8.3): it is a geographic CRS, written `GEOGCRS`, so the keyword alone
/// does not tell the kind of CS.
pub(crate) const GEODETIC_CRS: &[&str] = &["GEODCRS", "GEODETICCRS"];

/// `<projected crs keyword> ::= PROJCRS | PROJECTEDCRS` (9.1).
pub(crate) const PROJECTED_CRS: &[&str] = &["PROJCRS", "PROJECTEDCRS"];

/// `<vertical crs keyword> ::= VERTCRS | VERTICALCRS` (10.1).
const VERTICAL: &[&str] = &["VERTCRS", "VERTICALCRS"];

/// `<vertical reference frame keyword> ::= VDATUM | VRF | VERTICALDATUM` (10.2).
const VERTICAL_REFERENCE_FRAME: &[&str] = &["VDATUM", "VERTICALDATUM", "VRF"];

/// `<compound crs keyword> ::= COMPOUNDCRS` (15.1).
const COMPOUND: &[&str] = &["COMPOUNDCRS"];

/// `<engineering crs keyword> ::= ENGCRS | ENGINEERINGCRS` (11.1).
const ENGINEERING: &[&str] = &["ENGCRS", "ENGINEERINGCRS"];

/// `<bound crs keyword> ::= BOUNDCRS` (20.1).
const BOUND: &[&str] = &["BOUNDCRS"];

/// `<identifier keyword> ::= ID` (7.3.3).
const IDENTIFIER: &[&str] = &["ID"];

/// `<ellipsoid keyword> ::= ELLIPSOID | SPHEROID` (8.2.1; "SPHEROID is permitted for backward
/// compatibility"). WKT 1's is `SPHEROID`.
pub(crate) const ELLIPSOID: &[&str] = &["SPHEROID", "ELLIPSOID"];

// WKT 1 (OGC 01-009), as Annex C, Table C.2 maps it to WKT 2.

/// `GEOGCS`, a geographic CRS (WKT 2's `GEOGCRS`), and in a `PROJCS` its base CRS (WKT 2's
/// `BASEGEODCRS`).
const WKT1_GEOGRAPHIC: &[&str] = &["GEOGCS"];

/// `GEOCCS`, a geocentric CRS (`GEODCRS`).
const WKT1_GEOCENTRIC: &[&str] = &["GEOCCS"];

/// `PROJCS`, a projected CRS (`PROJCRS`).
const WKT1_PROJECTED: &[&str] = &["PROJCS"];

/// `VERT_CS`, a vertical CRS (`VERTCRS`).
const WKT1_VERTICAL: &[&str] = &["VERT_CS"];

/// `VERT_DATUM`, a vertical datum (C.3.3).
const WKT1_VERTICAL_DATUM: &[&str] = &["VERT_DATUM"];

/// `COMPD_CS`, a compound CRS (`COMPOUNDCRS`).
const WKT1_COMPOUND: &[&str] = &["COMPD_CS"];

/// `LOCAL_CS`, an engineering CRS (`ENGCRS`).
const WKT1_ENGINEERING: &[&str] = &["LOCAL_CS"];

/// `AUTHORITY`, an identifier (`ID`, C.2.2).
const WKT1_AUTHORITY: &[&str] = &["AUTHORITY"];

// Neither standard has these.

/// ESRI's WKT 1 vertical CRS, `VERTCS`, which PROJ reads too.
const ESRI_VERTICAL: &[&str] = &["VERTCS"];

/// `DATUMENSEMBLE`, which terrio reads as a datum ensemble.
const TERRIO_DATUM_ENSEMBLE: &[&str] = &["DATUMENSEMBLE"];

/// `BASEGEOGRAPHICCRS` and `BASEGEODETICCRS`, which terrio reads as a base CRS.
const TERRIO_BASE_CRS: &[&str] = &["BASEGEOGRAPHICCRS", "BASEGEODETICCRS"];

// The lists the readers use.

/// A geodetic datum or datum ensemble, terrio's `DATUMENSEMBLE` included.
pub(crate) const GEODETIC_DATUM: &[&str] = &concat::<5>(&[GEODETIC_REFERENCE_FRAME, DATUM_ENSEMBLE, TERRIO_DATUM_ENSEMBLE]);

/// A geodetic datum or datum ensemble of the standard ([`GEODETIC_DATUM`] without
/// `DATUMENSEMBLE`), which the WKT 2 reader reads.
pub(crate) const STANDARD_GEODETIC_DATUM: &[&str] = &concat::<4>(&[GEODETIC_REFERENCE_FRAME, DATUM_ENSEMBLE]);

/// The base CRS of a WKT 2 projected or derived geographic CRS (9.2.1, 14.3.1), terrio's two
/// included.
pub(crate) const BASE_GEODETIC_CRS: &[&str] = &concat::<4>(&[BASE_GEOGRAPHIC, BASE_GEODETIC, TERRIO_BASE_CRS]);

/// The base CRS keywords of the standard ([`BASE_GEODETIC_CRS`] without terrio's two), which the
/// name resolution of [`crate::def`] reads.
pub(crate) const STANDARD_BASE_GEODETIC_CRS: &[&str] = &concat::<2>(&[BASE_GEOGRAPHIC, BASE_GEODETIC]);

/// A WKT 2 geographic or geodetic CRS.
pub(crate) const GEOGRAPHIC_OR_GEODETIC_CRS: &[&str] = &concat::<4>(&[GEOGRAPHIC, GEODETIC_CRS]);

/// The root of a horizontal CRS: a geographic, geocentric or projected CRS of WKT 1, a
/// geographic, geodetic or projected CRS of WKT 2, or a bound CRS (of one).
pub(crate) const HORIZONTAL_CRS: &[&str] =
    &concat::<10>(&[WKT1_GEOGRAPHIC, WKT1_PROJECTED, WKT1_GEOCENTRIC, GEOGRAPHIC, PROJECTED_CRS, GEODETIC_CRS, BOUND]);

/// A vertical CRS of WKT 1, WKT 2 or ESRI.
pub(crate) const VERTICAL_CRS: &[&str] = &concat::<4>(&[WKT1_VERTICAL, VERTICAL, ESRI_VERTICAL]);

/// [`VERTICAL_CRS`] without ESRI's `VERTCS`, which PROJ reads but the compound CRS splitter, the
/// check of a declared code and the WKT sniffer do not (#1012 item 59).
pub(crate) const VERTICAL_CRS_BUT_ESRI: &[&str] = &concat::<3>(&[WKT1_VERTICAL, VERTICAL]);

/// A vertical datum of WKT 1 or WKT 2.
pub(crate) const VERTICAL_DATUM: &[&str] = &concat::<4>(&[WKT1_VERTICAL_DATUM, VERTICAL_REFERENCE_FRAME]);

/// A compound CRS of WKT 1 or WKT 2.
pub(crate) const COMPOUND_CRS: &[&str] = &concat::<2>(&[WKT1_COMPOUND, COMPOUND]);

/// An engineering (local) CRS of WKT 1 or WKT 2.
pub(crate) const ENGINEERING_CRS: &[&str] = &concat::<3>(&[WKT1_ENGINEERING, ENGINEERING]);

/// An identifier of WKT 1 or WKT 2.
pub(crate) const ID: &[&str] = &concat::<2>(&[WKT1_AUTHORITY, IDENTIFIER]);

/// The keywords of `parts`, in order, as one list of `N`.
const fn concat<const N: usize>(parts: &[&[&'static str]]) -> [&'static str; N] {
    let mut out = [""; N];
    let (mut i, mut p) = (0, 0);
    while p < parts.len() {
        let mut j = 0;
        while j < parts[p].len() {
            out[i] = parts[p][j];
            i += 1;
            j += 1;
        }
        p += 1;
    }
    assert!(i == N, "the list's length is not that of its parts");
    out
}

/// Whether a keyword is one of `keys`, whatever its case (Annex B.2.1: "KEYWORD is equivalent to
/// keyword", but not to `KEY_WORD`).
pub(crate) fn is(key: &str, keys: &[&str]) -> bool {
    keys.iter().any(|k| key.eq_ignore_ascii_case(k))
}

/// The keyword of a WKT text's root node: what comes before its first `[` or `(`, trimmed.
pub(crate) fn root_key(wkt: &str) -> &str {
    wkt.split(['[', '(']).next().unwrap_or_default().trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lists_keep_the_keywords_and_order_the_readers_had() {
        assert_eq!(GEODETIC_DATUM, ["DATUM", "GEODETICDATUM", "TRF", "ENSEMBLE", "DATUMENSEMBLE"]);
        assert_eq!(STANDARD_GEODETIC_DATUM, ["DATUM", "GEODETICDATUM", "TRF", "ENSEMBLE"]);
        assert_eq!(BASE_GEODETIC_CRS, ["BASEGEOGCRS", "BASEGEODCRS", "BASEGEOGRAPHICCRS", "BASEGEODETICCRS"]);
        assert_eq!(STANDARD_BASE_GEODETIC_CRS, ["BASEGEOGCRS", "BASEGEODCRS"]);
        assert_eq!(GEOGRAPHIC_OR_GEODETIC_CRS, ["GEOGCRS", "GEOGRAPHICCRS", "GEODCRS", "GEODETICCRS"]);
        assert_eq!(VERTICAL_CRS, ["VERT_CS", "VERTCRS", "VERTICALCRS", "VERTCS"]);
        assert_eq!(VERTICAL_CRS_BUT_ESRI, ["VERT_CS", "VERTCRS", "VERTICALCRS"]);
        assert_eq!(VERTICAL_DATUM, ["VERT_DATUM", "VDATUM", "VERTICALDATUM", "VRF"]);
        assert_eq!(ID, ["AUTHORITY", "ID"]);
        assert_eq!(ELLIPSOID, ["SPHEROID", "ELLIPSOID"]);
        assert_eq!(GEODETIC_CRS, ["GEODCRS", "GEODETICCRS"]);
        assert_eq!(PROJECTED_CRS, ["PROJCRS", "PROJECTEDCRS"]);
        assert_eq!(COMPOUND_CRS, ["COMPD_CS", "COMPOUNDCRS"]);
        assert_eq!(ENGINEERING_CRS, ["LOCAL_CS", "ENGCRS", "ENGINEERINGCRS"]);
        let mut horizontal = HORIZONTAL_CRS.to_vec();
        horizontal.sort_unstable();
        assert_eq!(
            horizontal,
            ["BOUNDCRS", "GEOCCS", "GEODCRS", "GEODETICCRS", "GEOGCRS", "GEOGCS", "GEOGRAPHICCRS", "PROJCRS", "PROJCS", "PROJECTEDCRS"]
        );
    }

    #[test]
    fn a_node_finds_its_children_by_keyword() {
        let root = crate::wkt1::parse(
            r#"COMPD_CS["c",GEOGCS["g",DATUM[D_X,SPHEROID["s",1,0]],AUTHORITY["EPSG","4326"]],VERT_CS["v",VERT_DATUM["d",2005]],ID["X","1"]]"#,
        )
        .unwrap();
        assert!(root.is(COMPOUND_CRS));
        // Depth first, the first datum whatever its name.
        let datum = root.find(GEODETIC_DATUM).unwrap();
        assert_eq!((datum.key.as_str(), datum.name()), ("DATUM", None));
        assert_eq!(root.find(COMPOUND_CRS), Some(&root));
        assert!(root.find(ENGINEERING_CRS).is_none());
        // The first of the keywords the node has a child with, not the first child.
        let vertical = root.child_in(&["VERT_CS", "GEOGCS"]).unwrap();
        assert_eq!(vertical.key, "VERT_CS");
        assert_eq!(root.child_in(VERTICAL_DATUM), None);
        // Identifiers in the node's order, of either keyword.
        let geogcs = root.child("GEOGCS").unwrap();
        assert_eq!(geogcs.ids().map(|n| n.key.as_str()).collect::<Vec<_>>(), ["AUTHORITY"]);
        assert_eq!(root.ids().map(|n| n.key.as_str()).collect::<Vec<_>>(), ["ID"]);
    }

    #[test]
    fn keywords_are_read_whatever_their_case_but_not_their_underscores() {
        assert!(is("vertcs", VERTICAL_CRS));
        assert!(!is("VERTCS", VERTICAL_CRS_BUT_ESRI));
        assert!(!is("VERTCS", &["VERT_CS"]));
        assert_eq!(root_key(" GEOGCS [\"x\"]"), "GEOGCS");
        assert_eq!(root_key("VERT_CS(\"x\")"), "VERT_CS");
    }
}
