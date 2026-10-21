//! The CF conventions' grid mappings, shared by the netCDF, HDF5 and Zarr drivers.

use super::common;
use wbraster::CrsInfo;

/// The CRS of CF grid mapping attributes, given by their text (`text`) and numeric (`numbers`)
/// values: the WKT of `crs_wkt` or `spatial_ref` (as GDAL writes them), or the CF parameters.
pub(crate) fn cf_crs(text: impl Fn(&str) -> Option<String>, numbers: impl Fn(&str) -> Option<Vec<f64>>) -> Option<CrsInfo> {
    if let Some(wkt) = text("crs_wkt").or_else(|| text("spatial_ref")) {
        return common::crs_from_text(&wkt);
    }
    let p = |k: &str| numbers(k).and_then(|v| v.first().copied());
    let a = p("semi_major_axis").or(p("earth_radius")).unwrap_or(6378137.0);
    let ellps = match (p("inverse_flattening"), p("semi_minor_axis"), p("earth_radius")) {
        (Some(rf), _, _) if rf > 0.0 => format!("+a={a} +rf={rf}"),
        (_, Some(b), _) => format!("+a={a} +b={b}"),
        (_, _, Some(r)) => format!("+R={r}"),
        _ => "+datum=WGS84".to_string(),
    };
    let fe = p("false_easting").unwrap_or(0.0);
    let fnn = p("false_northing").unwrap_or(0.0);
    let lat0 = p("latitude_of_projection_origin").unwrap_or(0.0);
    let lon0 = p("longitude_of_central_meridian")
        .or(p("longitude_of_projection_origin"))
        .or(p("straight_vertical_longitude_from_pole"))
        .unwrap_or(0.0);
    let parallels = numbers("standard_parallel").unwrap_or_default();
    let xy = format!("+x_0={fe} +y_0={fnn}");
    let proj = match text("grid_mapping_name")?.as_str() {
        "latitude_longitude" => {
            let wgs84 = (a - 6378137.0).abs() < 1e-6 && p("inverse_flattening").is_none_or(|rf| (rf - 298.257223563).abs() < 1e-6);
            if wgs84 {
                return Some(CrsInfo::from_epsg(4326));
            }
            format!("+proj=longlat {ellps}")
        }
        "transverse_mercator" => {
            let k = p("scale_factor_at_central_meridian").unwrap_or(1.0);
            format!("+proj=tmerc +lat_0={lat0} +lon_0={lon0} +k={k} {xy} {ellps}")
        }
        "lambert_conformal_conic" => {
            let lat1 = parallels.first().copied().unwrap_or(lat0);
            let lat2 = parallels.get(1).copied().unwrap_or(lat1);
            format!("+proj=lcc +lat_1={lat1} +lat_2={lat2} +lat_0={lat0} +lon_0={lon0} {xy} {ellps}")
        }
        "albers_conical_equal_area" => {
            let lat1 = parallels.first().copied().unwrap_or(lat0);
            let lat2 = parallels.get(1).copied().unwrap_or(lat1);
            format!("+proj=aea +lat_1={lat1} +lat_2={lat2} +lat_0={lat0} +lon_0={lon0} {xy} {ellps}")
        }
        "lambert_azimuthal_equal_area" => format!("+proj=laea +lat_0={lat0} +lon_0={lon0} {xy} {ellps}"),
        "polar_stereographic" => match parallels.first() {
            Some(ts) => format!("+proj=stere +lat_0={lat0} +lat_ts={ts} +lon_0={lon0} {xy} {ellps}"),
            None => {
                format!("+proj=stere +lat_0={lat0} +lon_0={lon0} +k={} {xy} {ellps}", p("scale_factor_at_projection_origin").unwrap_or(1.0))
            }
        },
        "mercator" => match parallels.first() {
            Some(ts) => format!("+proj=merc +lat_ts={ts} +lon_0={lon0} {xy} {ellps}"),
            None => format!("+proj=merc +lon_0={lon0} +k={} {xy} {ellps}", p("scale_factor_at_projection_origin").unwrap_or(1.0)),
        },
        "sinusoidal" => format!("+proj=sinu +lon_0={lon0} {xy} {ellps}"),
        // The grid's north pole, as PROJ maps it.
        "rotated_latitude_longitude" => {
            let (lat, lon) = (p("grid_north_pole_latitude")?, p("grid_north_pole_longitude")?);
            let o_lon = p("north_pole_grid_longitude").unwrap_or(0.0);
            format!("+proj=ob_tran +o_proj=longlat +o_lat_p={lat} +o_lon_p={o_lon} +lon_0={} {ellps}", lon + 180.0)
        }
        _ => return None,
    };
    Some(CrsInfo::from_proj4(proj))
}
