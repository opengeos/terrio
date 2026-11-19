// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Whether a CRS fits a registered code, compared once (#1058 step 4), as PROJ 9.9.0 compares a
//! CRS with its database's (`isEquivalentTo`, `identify()`): its kind, its vertical CRS, its
//! datum, its dimension and its content (conversion and ellipsoid), each reported in a [`Fit`].
//! The functions that keep a declared code ([`declared_fits`], [`esri_fits`]) read that report,
//! each by its rule; those that identify one ([`identify_by_shared_name`], and `identify_epsg`'s
//! search) compare content by the same [`same_content`].
//!
//! The comparison is written to match PROJ's results, which are checked in tests; it is not a
//! translation of PROJ's code.

use crate::def::{self, Def};
use crate::math::Ell;
use crate::{SpatialRef, catalog, epsg, keywords, wkt1};

#[cfg(test)]
mod baseline_tests;

/// Whether two lengths are the same as PROJ 9.9.0 compares measures (`Measure::_isEquivalentTo`,
/// `DEFAULT_MAX_REL_ERROR`): within 1e-10 of the first.
fn same_measure(x: f64, y: f64) -> bool {
    (x - y).abs() <= 1e-10 * x.abs()
}

/// Whether two ellipsoids are the same, as PROJ 9.9.0 compares them (`Ellipsoid::_isEquivalentTo`):
/// the semi-major axis, the semi-minor axis and, of two ellipsoids that are not spheres, the
/// inverse flattening, each within 1e-10. GRS 1980 is not WGS 84 (EPSG:6986 is not EPSG:7138).
pub(crate) fn same_figure(a: f64, es: f64, other_a: f64, other_es: f64) -> bool {
    let b = |a: f64, es: f64| a * (1.0 - es).sqrt();
    let rf = |es: f64| 1.0 / (1.0 - (1.0 - es).sqrt());
    same_measure(a, other_a)
        && same_measure(b(a, es), b(other_a, other_es))
        && (es == 0.0 || other_es == 0.0 || same_measure(rf(es), rf(other_es)))
}

/// The semi-major axis in metres and the eccentricity squared of the first `SPHEROID` or
/// `ELLIPSOID` of a WKT definition (in its `LENGTHUNIT`).
pub(crate) fn wkt_figure(wkt: &str) -> Option<(f64, f64)> {
    let (a, rf) = wkt1::parse(wkt)?.find(keywords::ELLIPSOID)?.ellipsoid()?;
    let f = if rf == 0.0 { 0.0 } else { 1.0 / rf };
    Some((a, f * (2.0 - f)))
}

/// Whether two WKT definitions' first ellipsoids are the same (see [`same_figure`]); `None` when
/// either has none.
pub(crate) fn same_wkt_figure(wkt: &str, other: &str) -> Option<bool> {
    let ((a, es), (b, other_es)) = (wkt_figure(wkt)?, wkt_figure(other)?);
    Some(same_figure(a, es, b, other_es))
}

/// Whether two ellipsoids are the same (see [`same_figure`]).
pub(crate) fn same_ellipsoid(a: &Ell, b: &Ell) -> bool {
    same_figure(a.a, a.es, b.a, b.es)
}

/// Whether two definitions convert alike on the same ellipsoid: the content a code is compared by.
pub(crate) fn same_content(own: &Def, theirs: &Def) -> bool {
    own.same_conversion(theirs) && same_ellipsoid(&own.ell, &theirs.ell)
}

/// How a CRS's geodetic datum matches a code's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Match {
    /// The same EPSG datum, by the datum's `ID` or `AUTHORITY`.
    SameId,
    /// The same EPSG datum by name (ESRI's `D_North_American_1983` for NAD83), or the same name.
    SameName,
    /// Another datum, of a name the two share (`GR96` for Greenland 1996 and its ensemble), or a
    /// datum whose CRSs ESRI writes with the other's name (ETRS89-NOR for `D_ETRS_1989`).
    Alias,
    /// Not compared: a side without a datum terrio knows, or PROJ's placeholder `unknown`.
    Unknown,
    /// Another datum.
    Different,
}

/// The EPSG datums of a WKT definition's geodetic datum: those of its `ID` or `AUTHORITY`, else
/// those its name is (`D_European_1950` is ED50); its name; and the other datums whose CRSs ESRI
/// writes with its name (ETRS89-NOR for `D_ETRS_1989`). No datum for a datum of another name
/// (`Unknown based on Bessel 1841 ellipsoid`), as for GDAL.
#[derive(Debug, Default)]
pub(crate) struct Datums {
    pub codes: Vec<u32>,
    /// Whether the codes come from the datum's `ID` or `AUTHORITY`.
    pub by_id: bool,
    pub others: &'static [u32],
    pub name: Option<String>,
}

impl Datums {
    pub(crate) fn of(wkt: &str) -> Self {
        let Some(root) = wkt1::parse(wkt) else { return Self::default() };
        let name = first_datum_name(&root);
        let Some(datum) = root.find(keywords::GEODETIC_DATUM) else { return Self { name, ..Self::default() } };
        if let Some(code) = datum.epsg() {
            return Self { codes: vec![code], by_id: true, others: &[], name };
        }
        let written = datum.name().unwrap_or_default();
        Self { codes: epsg::datums_named(written), by_id: false, others: epsg::datums_written_as(written), name }
    }
}

/// The name of the first geodetic datum under a node (`DATUM`, `GEODETICDATUM`, `TRF` or a datum
/// ensemble, as [`Datums::of`] finds it: a compound or bound CRS's horizontal or source CRS's). A
/// datum node without a quoted name gives none, and the search goes on after it.
fn first_datum_name(n: &wkt1::Node) -> Option<String> {
    if n.is(keywords::GEODETIC_DATUM) {
        return n.name().map(str::to_string);
    }
    n.args.iter().find_map(|a| match a {
        wkt1::Arg::Node(c) => first_datum_name(c),
        _ => None,
    })
}

/// How a horizontal CRS's WKT is on the datum of the registry's CRS `code` (`theirs`), as
/// [`declared_fits`] compares it.
fn declared_datum(wkt: &str, code: u32, theirs: &Def) -> Match {
    // On another datum than the code's, when the WKT's names one terrio knows (ETRS89 declaring
    // NAD83, both on GRS 1980): not the code, whose transformations would move it wrongly.
    // Both as their names identify them (and the code's datum), others included.
    // Datums terrio has CRSs on (not a superseded code a name still gives: Greenland 1996's 6747).
    let own = Datums::of(wkt);
    let named_datums = &own.codes;
    let mut own_datums = named_datums.clone();
    own_datums.retain(|d| !epsg::geographic_on(*d).is_empty());
    let registered = SpatialRef { epsg: Some(code), ..Default::default() }.to_wkt();
    let registered = registered.as_deref().map(|w| def::split_compound(w).map(|(h, _)| h).unwrap_or_else(|| w.to_string()));
    let their = registered.as_deref().map(Datums::of).unwrap_or_default();
    let mut their_datums = their.codes;
    their_datums.extend(theirs.datum_code());
    // A datum terrio does not know is not the code's unless named as it (PROJ keeps no code
    // either): a local datum declaring ETRS89 would get ETRS89's transformations.
    // PROJ 9.8.1's placeholder `unknown` (`D_unknown`), spelt so, matches any datum.
    let placeholder = own.name.as_deref().is_some_and(|n| n.strip_prefix("D_").unwrap_or(n) == "unknown");
    // A named datum terrio has no CRS on (Greenland 1996's 6747 and 1421) is compared too.
    let unknown = own_datums.is_empty() && own.others.is_empty() && own.name.is_some() && !placeholder;
    if !(!own_datums.is_empty() || unknown) || their_datums.is_empty() {
        return Match::Unknown;
    }
    if own_datums.iter().chain(named_datums).any(|d| their_datums.contains(d) || their.others.contains(d)) {
        return if own.by_id { Match::SameId } else { Match::SameName };
    }
    if their_datums.iter().any(|d| own.others.contains(d)) {
        return Match::Alias;
    }
    // A named datum terrio has no CRSs on, and the code's datum, that PROJ's database gives a
    // name in common (`GR96` for Greenland 1996, 6747, and its ensemble, 1421). Not datums terrio
    // has CRSs on, which aliases also share (`NAD83` for NAD83 and NAD83(HARN)).
    if named_datums.iter().filter(|d| !own_datums.contains(d)).any(|d| their_datums.iter().any(|t| epsg::datums_share_a_name(*d, *t))) {
        return Match::Alias;
    }
    // A datum of the same name is the same: EPSG gives Greenland 1996 two codes (6747 and 1421),
    // and a deprecated CRS (EPSG:4640) is on a superseded datum of the current one's name, which
    // ESRI's `D_RRAF_1991` identifies.
    let own_names: Vec<String> =
        own.name.iter().cloned().chain(named_datums.iter().filter_map(|d| epsg::datum_name(*d))).map(|n| epsg::datum_key(&n)).collect();
    let same_name = their
        .name
        .into_iter()
        .chain(their_datums.iter().filter_map(|d| epsg::datum_name(*d)))
        .any(|n| own_names.contains(&epsg::datum_key(&n)));
    if same_name { Match::SameName } else { Match::Different }
}

/// How a WKT is on an ESRI CRS's datum (its registry WKT's). On the same datum, by name: PROJ
/// 9.8.1 identifies no code for ESRI's Monte_Mario_Italy_1 on a datum renamed `D_Local`, and places
/// it by a ballpark, not by Monte Mario's transformations. By the EPSG datums the names are (ESRI's
/// `D_North_American_1983` and the registry's `North_American_Datum_1983` are NAD83); by name when
/// one is not known; either way when neither is (a datum without a transformation).
fn esri_datum(wkt: &str, registered: &str) -> Match {
    let own = Datums::of(wkt);
    let their = Datums::of(registered);
    match (own.codes.is_empty(), their.codes.is_empty()) {
        (false, false) => {
            if own.codes.iter().any(|d| their.codes.contains(d)) {
                if own.by_id { Match::SameId } else { Match::SameName }
            } else if own.codes.iter().any(|d| their.others.contains(d)) || their.codes.iter().any(|d| own.others.contains(d)) {
                Match::Alias
            } else {
                Match::Different
            }
        }
        (true, true) => Match::Unknown,
        _ => {
            let key = own.name.map(|n| epsg::datum_key(&n));
            if key.is_none() {
                Match::Unknown
            } else if their.name.map(|n| epsg::datum_key(&n)) == key {
                Match::SameName
            } else {
                Match::Different
            }
        }
    }
}

/// A WKT's fit to a code, component by component, in the order each constructor compares them:
/// a component after one that does not fit is not compared (`None`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fit {
    /// Of the code's kind: a horizontal or compound CRS against a code terrio builds (or, content
    /// terrio cannot read, against one it cannot build either), a vertical CRS against its own
    /// vertical code.
    pub kind: Option<bool>,
    /// A compound CRS's vertical CRS is the code's (by code or datum name, unit and direction);
    /// a CRS without one is not a compound code.
    pub vertical: Option<bool>,
    pub datum: Option<Match>,
    /// Geographic 2D or 3D as the code.
    pub dimension: Option<bool>,
    /// The same conversion on the same ellipsoid; for content terrio cannot read, the code's
    /// method by name (or, without a conversion, its kind and semi-major axis).
    pub content: Option<bool>,
}

impl Fit {
    /// Whether every component compared fits.
    pub(crate) fn fits(&self) -> bool {
        self.kind != Some(false)
            && self.vertical != Some(false)
            && self.datum != Some(Match::Different)
            && self.dimension != Some(false)
            && self.content != Some(false)
    }

    /// A WKT's fit to the EPSG CRS `code` it declares: the same conversion (units and axes
    /// included) on the same ellipsoid and datum, and of the same kind (geocentric, geographic
    /// 2D or 3D). When terrio cannot read the content (a method only the registry has), the
    /// code's projection method by name.
    pub(crate) fn declared(wkt: &str, code: u32) -> Self {
        let mut fit = Self::default();
        // Only a horizontal CRS's content is compared (not a vertical or engineering CRS's).
        let horizontal = wkt1::parse(wkt).is_some_and(|r| r.is(keywords::HORIZONTAL_CRS) || r.is(keywords::COMPOUND_CRS));
        // A vertical CRS's code only of its unit and direction; another root's (engineering, local)
        // not the code of a CRS terrio places (a vertical CRS declaring EPSG:4326 is not WGS 84).
        if !horizontal {
            let vertical = wkt1::parse(wkt).is_some_and(|r| r.is(keywords::VERTICAL_CRS));
            fit.kind = Some(if vertical {
                def::vertical_code(wkt) == Some(code)
            } else {
                epsg::vertical(code).is_none() && Def::new(&SpatialRef { epsg: Some(code), ..Default::default() }).is_err()
            });
            return fit;
        }
        // A compound CRS by its horizontal CRS, and its vertical CRS below.
        let compound = def::split_compound(wkt);
        let horizontal = compound.as_ref().map(|(h, _)| h.clone());
        let own = Def::wkt_conversion(horizontal.as_deref().unwrap_or(wkt));
        // A code terrio cannot build (EPSG:1, or one only wbprojection's registry has, without
        // that feature): kept only when the content cannot be read either, as there is nothing to
        // compare.
        let Ok(theirs) = Def::new(&SpatialRef { epsg: Some(code), ..Default::default() }) else {
            fit.kind = Some(own.is_none());
            return fit;
        };
        fit.kind = Some(true);
        // The vertical CRS of a compound code is the code's: its vertical code when the WKT's has
        // one, its unit and direction (heights in feet or depths declaring a code in metres up are
        // not that code, whose heights terrio would give).
        // Nor is a CRS without a vertical CRS a compound code (EPSG:9707's heights it has none of).
        fit.vertical = Some(match &compound {
            None => theirs.vertical().is_none(),
            Some((_, v)) => compound_vertical_fits(v, &theirs),
        });
        if fit.vertical == Some(false) {
            return fit;
        }
        fit.datum = Some(declared_datum(horizontal.as_deref().unwrap_or(wkt), code, &theirs));
        if fit.datum == Some(Match::Different) {
            return fit;
        }
        // A geographic 2D CRS is not a 3D code, nor the reverse (PROJ identifies EPSG:4979's WKT
        // declaring 4326 as 4979).
        fit.dimension = Some(match crate::geographic_dimension_3d(horizontal.as_deref().unwrap_or(wkt)) {
            Some(three_d) if theirs.is_geographic() && theirs.vertical().is_none() => three_d == theirs.has_height(),
            _ => true,
        });
        if fit.dimension == Some(false) {
            return fit;
        }
        fit.content = Some(match own {
            Some(own) => same_content(&own, &theirs),
            None => unread_content_fits(horizontal.as_deref().unwrap_or(wkt), code, &theirs),
        });
        fit
    }

    /// A geographic CRS's WKT's fit to the EPSG CRS it declares but for its prime meridian: on
    /// its datum, so with its transformations after the meridian's shift, as PROJ 9.8.1 places
    /// NTF (Paris) on another meridian through NTF (Paris) and "NTF to WGS 84 (1)". `None` for a
    /// WKT this does not compare (on the code's meridian, or not a geographic code).
    pub(crate) fn declared_but_meridian(wkt: &str, code: u32) -> Option<Self> {
        let (Some(mut own), Ok(theirs)) = (Def::wkt_conversion(wkt), Def::new(&SpatialRef { epsg: Some(code), ..Default::default() }))
        else {
            return None;
        };
        // A geographic code, not a compound one (a vertical CRS the GEOGCS has none of).
        if !theirs.is_geographic() || theirs.vertical().is_some() || own.pm() == theirs.pm() {
            return None;
        }
        own.set_pm(theirs.pm());
        let mut fit = Self { kind: Some(true), content: Some(same_content(&own, &theirs)), ..Self::default() };
        if fit.content == Some(true) {
            fit.datum = Some(declared_datum(wkt, code, &theirs));
        }
        Some(fit)
    }

    /// A WKT's fit to the ESRI CRS `code` it declares, as [`Fit::declared`] for an EPSG code: the
    /// same conversion on the same ellipsoid and datum; content terrio cannot read only when it
    /// names the ESRI CRS's own method (an unknown method declaring ESRI:54009 is not Mollweide).
    /// The ellipsoid by its semi-major axis.
    pub(crate) fn esri(w: &str, code: u32) -> Self {
        let mut fit = Self::default();
        let Some(e) = epsg::esri(code) else {
            fit.kind = Some(false);
            return fit;
        };
        fit.kind = Some(true);
        // On its datum (a local datum declaring an ESRI CRS on WGS 84 is not that CRS).
        fit.datum = Some(esri_datum(w, &e.wkt));
        if fit.datum == Some(Match::Different) {
            return fit;
        }
        // The datum's ellipsoid, the WKT's by the registry WKT's (in metres in an Esri WKT 1), not
        // the definitions': a spherical method's `+R_A` sphere (ESRI:54003, Miller on WGS 84).
        // Whole, as PROJ 9.9.0 compares it: an altered flattening is not the code's.
        let same_ellipsoid = same_wkt_figure(w, &e.wkt).is_none_or(|same| same);
        let Some(own) = Def::wkt_conversion(w) else {
            let method = |w: &str| {
                let root = wkt1::parse(w)?;
                let node = root.child("PROJECTION").or_else(|| root.child("CONVERSION").and_then(|c| c.child("METHOD")))?.clone();
                Some((node.name().map(wkt1::norm)?, node.epsg()))
            };
            // The registry WKT's method, by its name or PROJ's spherical variant of it (PROJ writes
            // ESRI:102035's `Orthographic` as `Orthographic (Spherical)`), or by the EPSG method
            // PROJ names otherwise (ESRI:54002's `Equirectangular` as `Equidistant Cylindrical
            // (Spherical)`, EPSG 1029); not another method, nor a method of no known name.
            let (Some((m, id)), Some((r, _))) = (method(w), method(&e.wkt)) else {
                fit.content = Some(false);
                return fit;
            };
            // On the registry's ellipsoid too: the definition terrio builds is the registry's,
            // not the WKT's.
            fit.content = Some(same_ellipsoid && (m == r || m == format!("{r}spherical") || (r == "equirectangular" && id == Some(1029))));
            return fit;
        };
        fit.content =
            Some(Def::new(&SpatialRef { esri: Some(code), ..Default::default() }).is_ok_and(|r| own.same_conversion(&r) && same_ellipsoid));
        fit
    }
}

/// Whether a compound CRS's vertical CRS (`v`) is the compound code's (`theirs`): another vertical
/// code only when its vertical datum is the code's by name (PROJ keeps EPSG:7405 whose vertical
/// CRS, Ordnance Datum Newlyn, declares another code); the same unit and direction.
fn compound_vertical_fits(v: &str, theirs: &Def) -> bool {
    let Some(their_vertical) = theirs.vertical() else { return false };
    let vertical_datum = |w: &str| {
        let root = wkt1::parse(w)?;
        let datum = root.child_in(keywords::VERTICAL_DATUM)?;
        datum.name().map(epsg::datum_key)
    };
    let same_vertical_datum = || {
        let theirs = their_vertical.code.and_then(epsg::vertical).and_then(|t| vertical_datum(&t.wkt));
        vertical_datum(v).is_some_and(|own| Some(own) == theirs)
    };
    if crate::declared_epsg(v).is_some_and(|c| their_vertical.code.is_some_and(|t| t != c)) && !same_vertical_datum() {
        return false;
    }
    let Some((to_meter, down)) = crate::vertical_axis(v) else { return false };
    !((to_meter - their_vertical.to_meter).abs() > 1e-9 * their_vertical.to_meter || down != their_vertical.down)
}

/// Whether a horizontal CRS whose content terrio cannot read is the EPSG CRS `code`: by the
/// code's method, by EPSG's name (PROJ's own WKT 2 of Belge 72, `Lambert Conic Conformal (2SP
/// Belgium)`, which the registry's WKT 1 names otherwise) or by the registry WKT's name; not
/// another method (Belge 72 or an unknown method declaring EPSG:27700).
fn unread_content_fits(wkt: &str, code: u32, theirs: &Def) -> bool {
    let method = |w: &str| {
        let root = wkt1::parse(w)?;
        let node = root.child("PROJECTION").or_else(|| root.child("CONVERSION").and_then(|c| c.child("METHOD")))?.clone();
        Some(node)
    };
    // A CRS without a conversion (a geocentric CRS in PROJ's simplified WKT 2, whose bare `UNIT`
    // the reader refuses): the code's, on its ellipsoid. Of the code's kind: a geocentric WKT is
    // not a geographic code on its ellipsoid. A compound CRS by its horizontal CRS, the
    // registry's too.
    let Some(own) = method(wkt) else {
        let Some(root) = wkt1::parse(wkt) else { return false };
        let converted = root.child("PROJECTION").is_some() || root.child("CONVERSION").is_some();
        let cartesian =
            root.child("CS").is_some_and(|c| matches!(c.args.first(), Some(wkt1::Arg::Ident(t)) if t.eq_ignore_ascii_case("Cartesian")));
        let geocentric = root.key.eq_ignore_ascii_case("GEOCCS") || (root.is(keywords::GEODETIC_CRS) && cartesian);
        return !converted
            && geocentric == theirs.is_geocentric()
            && wkt_figure(wkt).is_some_and(|(a, es)| same_figure(a, es, theirs.ell.a, theirs.ell.es));
    };
    let registered = epsg::get(code)
        .map(|e| e.wkt.clone())
        .or_else(|| crate::registry_wkt(code))
        .map(|w| def::split_compound(&w).map(|(h, _)| h).unwrap_or(w));
    let Some(name) = own.name().map(wkt1::norm) else { return false };
    catalog::epsg_method(code).is_some_and(|m| wkt1::norm(m) == name)
        || registered.as_deref().and_then(method).is_some_and(|t| t.name().map(wkt1::norm).as_deref() == Some(name.as_str()))
}

/// Whether a WKT's own content is the EPSG CRS it declares (see [`Fit::declared`]).
pub(crate) fn declared_fits(wkt: &str, code: u32) -> bool {
    Fit::declared(wkt, code).fits()
}

/// Whether a geographic CRS's WKT is the EPSG CRS it declares but for its prime meridian (see
/// [`Fit::declared_but_meridian`]).
pub(crate) fn declared_fits_but_meridian(wkt: &str, code: u32) -> bool {
    Fit::declared_but_meridian(wkt, code).is_some_and(|f| f.fits())
}

/// Whether a WKT's content is the ESRI CRS it declares (see [`Fit::esri`]).
pub(crate) fn esri_fits(w: &str, code: u32) -> bool {
    Fit::esri(w, code).fits()
}

/// Whether a WKT is on an ESRI CRS's datum (its registry WKT's), as [`Fit::esri`] compares it.
pub(crate) fn esri_same_datum(wkt: &str, registered: &str) -> bool {
    esri_datum(wkt, registered) != Match::Different
}

/// The EPSG geographic CRS PROJ 9.8.1 identifies a WKT 1 `GEOGCS` as by its name alone, on a datum
/// no EPSG geographic CRS is on and with which the CRS's datum has a name in common: ESRI's
/// GCS_Greenland_1996 on `D_Greenland_1996` (Greenland 1996, 6747) as GR96, EPSG:4747, on the GR96
/// ensemble (1421). Not for another name on that datum, where PROJ identifies none, nor for a
/// geographic 3D CRS (ESRI's `LINUNIT`), which the name table leaves out, nor for WKT 2, where PROJ
/// does not read ESRI's datum names. The caller gives it as the geographic CRS only, not as the
/// CRS's own code: PROJ places such a CRS by a ballpark, not by the transformations of GR96.
pub(crate) fn identify_by_shared_name(geogcs: &str) -> Option<u32> {
    let datums = Datums::of(geogcs).codes;
    if datums.is_empty() || !datums.iter().all(|d| epsg::geographic_on(*d).is_empty()) {
        return None;
    }
    let root = wkt1::parse(geogcs)?;
    if !root.key.eq_ignore_ascii_case("GEOGCS") || root.child("LINUNIT").is_some() {
        return None;
    }
    // ESRI's names for a `GCS_` geographic CRS, as PROJ's WKT reader maps them.
    let esri = root.name().is_some_and(|n| n.starts_with("GCS_"));
    let code = epsg::crs_named_as_proj(root.name()?, false, false, esri)?;
    let datum = epsg::get(code)?.datum;
    if datums.contains(&datum) || !datums.iter().any(|d| epsg::datums_share_a_name(*d, datum)) {
        return None;
    }
    // The same conversion on the same ellipsoid.
    let own = Def::wkt_conversion(geogcs)?;
    let theirs = Def::new(&SpatialRef { epsg: Some(code), ..Default::default() }).ok()?;
    same_content(&own, &theirs).then_some(code)
}
