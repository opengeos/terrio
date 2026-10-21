//! User-defined coordinate reference systems in GeoTIFF GeoKeys: the projection method and its
//! parameters, as GDAL writes them for CRSs without an EPSG code, and ESRI PE citations.

/// GeoKey directory access: `(location, count, value_or_offset)` by key id.
pub(crate) struct GeoKeys<'a> {
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

    fn first(&self, ids: &[u16]) -> Option<f64> {
        ids.iter().find_map(|id| self.double(*id))
    }
}

/// The CRS of an ESRI PE string in a citation key (`ESRI PE String = <WKT>`), which describes a
/// user-defined CRS more completely than the GeoKeys. Its EPSG code is the root node's: codes on
/// inner nodes (a unit's, a datum's) are not the CRS's.
pub(crate) fn esri_pe_crs(k: &GeoKeys) -> Option<wbraster::CrsInfo> {
    let wkt = [3073, 1026, 2049].iter().find_map(|id| k.text(*id).and_then(|c| c.strip_prefix("ESRI PE String = ")))?.trim();
    // Read as any WKT: the code identified, and its PROJ4 extension (GDAL writes the sweep axis
    // of a geostationary CRS there) taken as the CRS.
    let s = crate::crs::SpatialRef::from_wkt(wkt);
    Some(wbraster::CrsInfo { epsg: s.epsg, wkt: Some(wkt.to_string()), proj4: s.proj4 })
}

/// The PROJ definition of a user-defined projected CRS: the projection method and its parameters,
/// the datum and the linear unit. `None` for a method terrio does not map.
pub(crate) fn user_defined_proj4(k: &GeoKeys) -> Option<String> {
    // GeoKeys that name no datum or ellipsoid are taken as WGS 84.
    let geog = geographic_proj(k).unwrap_or_else(|| "+datum=WGS84".to_string());
    let (units, to_meter) = units(k);
    Some(format!("+proj={} {geog} {units} +no_defs", projection(k, to_meter)?))
}

/// The projection method of projected GeoKeys that [`user_defined_proj4`] does not map, for a
/// warning: `None` when there is none or it is mapped.
pub(crate) fn unmapped_method(k: &GeoKeys) -> Option<u16> {
    let method = k.short(3075).filter(|m| *m != 0 && *m != USER_DEFINED)?;
    (k.short(1024) == Some(1) && projection(k, 1.0).is_none()).then_some(method)
}

/// The CRS of GeoKeys that use no EPSG code for the whole CRS: an ESRI PE citation's WKT, or a
/// PROJ definition built from the projection method, its parameters and the datum.
pub(crate) fn user_defined_crs(k: &GeoKeys) -> Option<wbraster::CrsInfo> {
    if let Some(crs) = esri_pe_crs(k) {
        return Some(crs);
    }
    let proj = match k.short(1024)? {
        2 => format!("+proj=longlat {}", geographic_proj(k)?),
        1 => user_defined_proj4(k)?,
        _ => return None,
    };
    if let Some(crs) = projected_on_epsg_geographic(k) {
        return Some(crs);
    }
    let crs = wbraster::CrsInfo::from_proj4(proj.trim());
    // The WKT GDAL writes for the PROJ string, which describes it whole.
    let wkt = crate::crs::SpatialRef::from_proj4(proj.trim()).ok().and_then(|s| s.wkt).or(crs.wkt);
    let wkt = wkt.or_else(|| wbprojection::from_proj_string(&proj).ok().map(|c| wbprojection::crs_to_wkt(&c)));
    Some(wbraster::CrsInfo { wkt, ..crs })
}

/// A user-defined projected CRS on an EPSG geographic CRS a PROJ string does not name exactly
/// (OSGB36, ED50, NAD83(HARN), ETRS89...): its WKT, whose `GEOGCS` identifies the datum and its
/// transformations, as GDAL reads the GeoKeys. `None` for other GeoKeys.
pub(crate) fn projected_on_epsg_geographic(k: &GeoKeys) -> Option<wbraster::CrsInfo> {
    if k.short(1024) != Some(1) || esri_pe_crs(k).is_some() {
        return None;
    }
    // WGS 84, NAD83 and NAD27 are PROJ's `+datum=` names.
    let code = k.short(2048).filter(|c| !matches!(*c, USER_DEFINED | 4326 | 4269 | 4267))?;
    let wkt = crate::crs::from_proj_on(&user_defined_proj4(k)?, code as u32)?;
    Some(wbraster::CrsInfo { epsg: None, wkt: Some(wkt), proj4: None })
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
    // The ellipsoid of another EPSG geographic CRS, when its datum cannot be named.
    if let Some(e) = k.short(2048).filter(|c| *c != USER_DEFINED).and_then(|c| crate::crs::epsg_ellipsoid(c as u32)) {
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
    let mut out = match ellipsoid {
        Some(e) => e.to_string(),
        None => {
            let a = k.double(2057)?;
            match (k.double(2059), k.double(2058)) {
                (Some(rf), _) if rf > 0.0 => format!("+a={a} +rf={rf}"),
                (_, Some(b)) => format!("+a={a} +b={b}"),
                _ => format!("+R={a}"),
            }
        }
    };
    // GeogPrimeMeridianLongGeoKey and GeogTOWGS84GeoKey.
    if let Some(pm) = k.double(2061).filter(|pm| *pm != 0.0) {
        out.push_str(&format!(" +pm={pm}"));
    }
    if let Some(t) = k.doubles(2062).filter(|t| t.len() == 3 || t.len() == 7) {
        out.push_str(&format!(" +towgs84={}", t.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")));
    }
    Some(out)
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

    let lon0 = k.first(&[NAT_ORIGIN_LONG, FALSE_ORIGIN_LONG, CENTER_LONG, STRAIGHT_VERT_POLE_LONG]).unwrap_or(0.0);
    let lat0 = k.first(&[NAT_ORIGIN_LAT, FALSE_ORIGIN_LAT, CENTER_LAT]).unwrap_or(0.0);
    let x0 = k.first(&[FALSE_EASTING, FALSE_ORIGIN_EASTING]).unwrap_or(0.0) * to_meter;
    let y0 = k.first(&[FALSE_NORTHING, FALSE_ORIGIN_NORTHING]).unwrap_or(0.0) * to_meter;
    let scale = k.first(&[SCALE_AT_NAT_ORIGIN, SCALE_AT_CENTER]).unwrap_or(1.0);
    let lat1 = k.double(STD_PARALLEL_1);
    let lat2 = k.double(STD_PARALLEL_2);
    let false_xy = format!("+x_0={x0} +y_0={y0}");
    let oblique_mercator = |variant: &str| -> Option<String> {
        let gamma = k.double(RECTIFIED_GRID_ANGLE).map(|g| format!(" +gamma={g}")).unwrap_or_default();
        Some(format!("omerc{variant} +lat_0={lat0} +lonc={lon0} +alpha={}{gamma} +k={scale} {false_xy}", k.double(AZIMUTH)?))
    };
    Some(match k.short(3075)? {
        1 => format!("tmerc +lat_0={lat0} +lon_0={lon0} +k={scale} {false_xy}"),
        // Hotine oblique Mercator: variant A (natural origin) and B (centre of projection).
        3 => oblique_mercator(" +no_uoff")?,
        // Swiss oblique Mercator, as GDAL writes it: variant B of azimuth 90°.
        9815 if k.double(AZIMUTH) == Some(90.0) && k.double(RECTIFIED_GRID_ANGLE).is_none_or(|g| g == 90.0) => {
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
            let origin = k.double(NAT_ORIGIN_LAT).unwrap_or(90.0);
            let ts = lat1.or(((origin.abs() - 90.0).abs() > 1e-9).then_some(origin));
            let pole = if ts.unwrap_or(origin) < 0.0 { -90.0 } else { 90.0 };
            // The longitude is ProjStraightVertPoleLongGeoKey's, whatever other origin keys say.
            let lon0 = k.double(STRAIGHT_VERT_POLE_LONG).unwrap_or(lon0);
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

/// The linear unit of the GeoKeys (ProjLinearUnitsGeoKey, or ProjLinearUnitSizeGeoKey for a
/// user-defined one) as a PROJ parameter, and its size in metres.
fn units(k: &GeoKeys) -> (String, f64) {
    match k.short(3076) {
        Some(9002) => ("+units=ft".into(), 0.3048),
        Some(9003) => ("+units=us-ft".into(), 1200.0 / 3937.0),
        Some(9036) => ("+units=km".into(), 1000.0),
        Some(USER_DEFINED) => match k.double(3077) {
            Some(m) if m > 0.0 => (format!("+to_meter={m}"), m),
            _ => ("+units=m".into(), 1.0),
        },
        _ => ("+units=m".into(), 1.0),
    }
}

/// GeoKeys for a CRS without an EPSG code, as GDAL writes them: from its PROJ string when it has
/// one, else from the projection engine's model of it. `None` when GeoKeys cannot describe it.
pub(crate) fn user_keys(crs: &crate::crs::SpatialRef, name: &str) -> Option<GeoKeyValues> {
    if let Some(p) = &crs.proj4 {
        return keys_for_proj(p, name, None);
    }
    // A WKT: its PROJ string as terrio reads it, on the EPSG geographic CRS it is identified on.
    if let Some(p) = crs.wkt.as_deref().and_then(crate::crs::wkt_proj) {
        let geog = crate::crs::Def::new(crs).ok().and_then(|d| d.geog());
        return keys_for_proj(&p, name, geog);
    }
    crs.to_wbp().ok().and_then(|c| keys_for_crs(&c, name))
}

/// GeoKeys for a PROJ string, as GDAL writes them, from its WKT 1 description, on an EPSG
/// geographic CRS when given one. `None` when the method or the linear unit has no GeoTIFF
/// equivalent or the WKT 1 needs the PROJ string to describe it.
pub(crate) fn keys_for_proj(proj4: &str, name: &str, geog: Option<u32>) -> Option<GeoKeyValues> {
    let d = crate::crs::describe(proj4)?;
    d.unit_code?;
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
        "Gnomonic" => (19, vec![(3081, lat), (3080, lon), (3082, fe), (3083, fn_)]),
        "Miller_Cylindrical" => (20, vec![(3088, get("longitude_of_center")), (3082, fe), (3083, fn_)]),
        "Orthographic" => (21, vec![(3081, lat), (3080, lon), (3082, fe), (3083, fn_)]),
        "Polyconic" => (22, vec![(3081, lat), (3080, lon), (3082, fe), (3083, fn_)]),
        "Robinson" => (23, vec![(3088, clon), (3082, fe), (3083, fn_)]),
        "Sinusoidal" => (24, vec![(3088, clon), (3082, fe), (3083, fn_)]),
        "VanDerGrinten" => (25, vec![(3088, lon), (3082, fe), (3083, fn_)]),
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
    v.shorts.push((3076, u16::try_from(d.unit_code?).ok()?));
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
pub(crate) fn keys_for_crs(crs: &wbprojection::Crs, name: &str) -> Option<GeoKeyValues> {
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

/// GeoKey values by key id.
#[derive(Debug, Default)]
pub(crate) struct GeoKeyValues {
    /// SHORT keys.
    pub shorts: Vec<(u16, u16)>,
    /// DOUBLE keys, in the order of GeoDoubleParamsTag; consecutive values of one key (the seven
    /// of GeogTOWGS84GeoKey) are its values.
    pub doubles: Vec<(u16, f64)>,
    /// ASCII keys.
    pub ascii: Vec<(u16, String)>,
}

impl GeoKeyValues {
    /// The directory entries of the DOUBLE keys: `[id, GeoDoubleParamsTag, count, offset]`.
    pub fn double_keys(&self) -> Vec<[u16; 4]> {
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
}
