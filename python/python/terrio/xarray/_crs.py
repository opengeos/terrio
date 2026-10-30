# SPDX-FileCopyrightText: Copyright (c) 2006-2018, Jeffrey Whitaker.
# SPDX-FileCopyrightText: Copyright (c) 2019-2024, Open source contributors.
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
#
# Derived from (adapted and modified):
#   pyproj 3.8.0:
#     pyproj/crs/_cf1x8.py

"""CRS helpers: parsing user input, and CF grid mapping attributes as rioxarray writes them.

rioxarray writes the attributes `pyproj.CRS.to_cf()` gives for a rasterio CRS (built from its
WKT1). terrio has no PROJ, so they are derived here from the CRS's WKT1, following pyproj's
mapping of projection methods to CF grid mappings (pyproj: MIT license).
"""

from __future__ import annotations

from typing import Any

from terrio.rio.crs import CRS, _child, _parse_wkt

# Datum names as PROJ reads them from WKT1 (GDAL's names, with underscores).
_DATUM_NAMES = {
    "WGS_1984": "World Geodetic System 1984",
    "WGS_1972": "World Geodetic System 1972",
    "WGS 84": "World Geodetic System 1984",
    "European_Terrestrial_Reference_System_1989": "European Terrestrial Reference System 1989",
}


def crs_from_user_input(crs_input: Any) -> CRS:
    """A `terrio.rio.crs.CRS` from anything describing a CRS.

    Args:
        crs_input: A CRS, an EPSG code, `EPSG:n`, WKT, a PROJ string, a dict of PROJ
            parameters, or an object with a `to_wkt()` method (pyproj and rasterio CRSs).

    Returns:
        CRS: The CRS.
    """
    if isinstance(crs_input, CRS):
        return crs_input
    if not isinstance(crs_input, (str, int, dict)) and hasattr(crs_input, "to_wkt"):
        crs_input = crs_input.to_wkt()
    return CRS.from_user_input(crs_input)


def _params(node: list[Any]) -> dict[str, float]:
    """The PARAMETER values of a PROJCS node, by lower-case name.

    Args:
        node: The PROJCS node.

    Returns:
        dict: Parameter values.
    """
    out: dict[str, float] = {}
    for c in node[1:]:
        if isinstance(c, list) and c and str(c[0]).upper() == "PARAMETER" and len(c) > 2:
            out[str(c[1]).lower()] = float(c[2])
    return out


def _get(p: dict[str, float], *names: str, default: float | None = None) -> float:
    """The first parameter found among names.

    Args:
        p: Parameters.
        *names: Candidate names.
        default: The value when none is found.

    Returns:
        float: The value.

    Raises:
        KeyError: If no name is found and there is no default.
    """
    for n in names:
        if n in p:
            return p[n]
    if default is None:
        raise KeyError(names[0])
    return default


def _projection_to_cf(method: str, p: dict[str, float]) -> dict[str, Any] | None:
    """The CF grid mapping of a WKT1 projection method, as pyproj maps it.

    Args:
        method: The PROJECTION name.
        p: Its parameters.

    Returns:
        dict | None: The grid mapping attributes, or None when CF has no mapping for it.
    """
    m = method.lower()
    fe = _get(p, "false_easting", default=0.0)
    fn = _get(p, "false_northing", default=0.0)
    if m == "transverse_mercator":
        return {
            "grid_mapping_name": "transverse_mercator",
            "latitude_of_projection_origin": _get(p, "latitude_of_origin", default=0.0),
            "longitude_of_central_meridian": _get(p, "central_meridian", default=0.0),
            "false_easting": fe,
            "false_northing": fn,
            "scale_factor_at_central_meridian": _get(p, "scale_factor", default=1.0),
        }
    if m in ("albers_conic_equal_area", "albers"):
        return {
            "grid_mapping_name": "albers_conical_equal_area",
            "standard_parallel": (_get(p, "standard_parallel_1"), _get(p, "standard_parallel_2")),
            "latitude_of_projection_origin": _get(
                p, "latitude_of_center", "latitude_of_origin", default=0.0
            ),
            "longitude_of_central_meridian": _get(
                p, "longitude_of_center", "central_meridian", default=0.0
            ),
            "false_easting": fe,
            "false_northing": fn,
        }
    if m == "lambert_conformal_conic_2sp" or (
        m == "lambert_conformal_conic" and "standard_parallel_2" in p
    ):
        return {
            "grid_mapping_name": "lambert_conformal_conic",
            "standard_parallel": (_get(p, "standard_parallel_1"), _get(p, "standard_parallel_2")),
            "latitude_of_projection_origin": _get(p, "latitude_of_origin", default=0.0),
            "longitude_of_central_meridian": _get(p, "central_meridian", default=0.0),
            "false_easting": fe,
            "false_northing": fn,
        }
    if m == "lambert_conformal_conic_1sp":
        return {
            "grid_mapping_name": "lambert_conformal_conic",
            "standard_parallel": _get(p, "latitude_of_origin", default=0.0),
            "longitude_of_central_meridian": _get(p, "central_meridian", default=0.0),
            "false_easting": fe,
            "false_northing": fn,
        }
    if m == "lambert_azimuthal_equal_area":
        return {
            "grid_mapping_name": "lambert_azimuthal_equal_area",
            "latitude_of_projection_origin": _get(
                p, "latitude_of_center", "latitude_of_origin", default=0.0
            ),
            "longitude_of_projection_origin": _get(
                p, "longitude_of_center", "central_meridian", default=0.0
            ),
            "false_easting": fe,
            "false_northing": fn,
        }
    if m == "azimuthal_equidistant":
        return {
            "grid_mapping_name": "azimuthal_equidistant",
            "latitude_of_projection_origin": _get(
                p, "latitude_of_center", "latitude_of_origin", default=0.0
            ),
            "longitude_of_projection_origin": _get(
                p, "longitude_of_center", "central_meridian", default=0.0
            ),
            "false_easting": fe,
            "false_northing": fn,
        }
    if m == "cylindrical_equal_area":
        return {
            "grid_mapping_name": "lambert_cylindrical_equal_area",
            "standard_parallel": _get(p, "standard_parallel_1", default=0.0),
            "longitude_of_central_meridian": _get(p, "central_meridian", default=0.0),
            "false_easting": fe,
            "false_northing": fn,
        }
    if m == "mercator_1sp":
        return {
            "grid_mapping_name": "mercator",
            "standard_parallel": _get(p, "latitude_of_origin", default=0.0),
            "longitude_of_projection_origin": _get(p, "central_meridian", default=0.0),
            "false_easting": fe,
            "false_northing": fn,
            "scale_factor_at_projection_origin": _get(p, "scale_factor", default=1.0),
        }
    if m == "mercator_2sp":
        return {
            "grid_mapping_name": "mercator",
            "standard_parallel": _get(p, "standard_parallel_1", default=0.0),
            "longitude_of_projection_origin": _get(p, "central_meridian", default=0.0),
            "false_easting": fe,
            "false_northing": fn,
        }
    if m == "hotine_oblique_mercator_azimuth_center":
        return {
            "grid_mapping_name": "oblique_mercator",
            "latitude_of_projection_origin": _get(p, "latitude_of_center", default=0.0),
            "longitude_of_projection_origin": _get(p, "longitude_of_center", default=0.0),
            "azimuth_of_central_line": _get(p, "azimuth", default=0.0),
            "scale_factor_at_projection_origin": _get(p, "scale_factor", default=1.0),
            "false_easting": fe,
            "false_northing": fn,
        }
    if m == "orthographic":
        return {
            "grid_mapping_name": "orthographic",
            "latitude_of_projection_origin": _get(p, "latitude_of_origin", default=0.0),
            "longitude_of_projection_origin": _get(p, "central_meridian", default=0.0),
            "false_easting": fe,
            "false_northing": fn,
        }
    if m == "polar_stereographic":
        lat0 = _get(p, "latitude_of_origin", default=90.0)
        lon0 = _get(p, "central_meridian", default=0.0)
        if abs(lat0) != 90.0:
            # Variant B: the latitude is the standard parallel.
            return {
                "grid_mapping_name": "polar_stereographic",
                "standard_parallel": lat0,
                "straight_vertical_longitude_from_pole": lon0,
                "false_easting": fe,
                "false_northing": fn,
            }
        return {
            "grid_mapping_name": "polar_stereographic",
            "latitude_of_projection_origin": lat0,
            "straight_vertical_longitude_from_pole": lon0,
            "false_easting": fe,
            "false_northing": fn,
            "scale_factor_at_projection_origin": _get(p, "scale_factor", default=1.0),
        }
    if m == "sinusoidal":
        return {
            "grid_mapping_name": "sinusoidal",
            "longitude_of_projection_origin": _get(
                p, "longitude_of_center", "central_meridian", default=0.0
            ),
            "false_easting": fe,
            "false_northing": fn,
        }
    if m == "stereographic":
        return {
            "grid_mapping_name": "stereographic",
            "latitude_of_projection_origin": _get(p, "latitude_of_origin", default=0.0),
            "longitude_of_projection_origin": _get(p, "central_meridian", default=0.0),
            "false_easting": fe,
            "false_northing": fn,
            "scale_factor_at_projection_origin": _get(p, "scale_factor", default=1.0),
        }
    return None


def _datum_name(name: str) -> str:
    """A WKT1 datum name as PROJ names it.

    Args:
        name: The DATUM name.

    Returns:
        str: The datum's name.
    """
    if name in _DATUM_NAMES:
        return _DATUM_NAMES[name]
    if name.startswith("D_"):
        name = name[2:]
    return name.replace("_", " ")


def _geographic_to_cf(geog: list[Any]) -> dict[str, Any] | None:
    """The ellipsoid, prime meridian and datum attributes of a GEOGCS node.

    Args:
        geog: The GEOGCS node.

    Returns:
        dict | None: The attributes, or None when the node lacks them.
    """
    datum = _child(geog, "DATUM")
    if datum is None:
        return None
    sph = _child(datum, "SPHEROID")
    if sph is None or len(sph) < 4:
        return None
    a = float(sph[2])
    rf = float(sph[3])
    b = a if rf == 0 else a * (1.0 - 1.0 / rf)
    out: dict[str, Any] = {
        "semi_major_axis": a,
        "semi_minor_axis": b,
        "inverse_flattening": rf,
        "reference_ellipsoid_name": str(sph[1]),
    }
    pm = _child(geog, "PRIMEM")
    if pm is not None and len(pm) > 2:
        out["longitude_of_prime_meridian"] = float(pm[2])
        out["prime_meridian_name"] = str(pm[1])
    out["geographic_crs_name"] = str(geog[1])
    out["horizontal_datum_name"] = _datum_name(str(datum[1]))
    towgs84 = _child(datum, "TOWGS84")
    if towgs84 is not None:
        out["towgs84"] = [float(v) for v in towgs84[1:] if isinstance(v, float)]
    return out


def _horizontal_to_cf(node: list[Any]) -> dict[str, Any] | None:
    """The CF attributes of a GEOGCS or PROJCS node.

    Args:
        node: The node.

    Returns:
        dict | None: The attributes, or None when CF cannot describe the CRS.
    """
    kind = str(node[0]).upper()
    if _child(node, "EXTENSION") is not None:
        # A CRS defined by a PROJ string (such as Web Mercator's): CF has no grid mapping for it.
        return None
    if kind == "GEOGCS":
        out = _geographic_to_cf(node)
        if out is None:
            return None
        towgs84 = out.pop("towgs84", None)
        out["grid_mapping_name"] = "latitude_longitude"
        if towgs84 is not None:
            out["towgs84"] = towgs84
        return out
    if kind != "PROJCS":
        return None
    geog = _child(node, "GEOGCS")
    proj = _child(node, "PROJECTION")
    if geog is None or proj is None:
        return None
    out = _geographic_to_cf(geog)
    if out is None:
        return None
    towgs84 = out.pop("towgs84", None)
    out["projected_crs_name"] = str(node[1])
    try:
        mapping = _projection_to_cf(str(proj[1]), _params(node))
    except KeyError:
        return None
    if mapping is None:
        return None
    out.update(mapping)
    if towgs84 is not None:
        out["towgs84"] = towgs84
    return out


def crs_to_cf(crs: CRS) -> dict[str, Any]:
    """The CF grid mapping attributes of a CRS, as `pyproj.CRS.to_cf()` gives them.

    `crs_wkt` is not included: rioxarray writes it (and `spatial_ref`) from the CRS's WKT.

    Args:
        crs: The CRS.

    Returns:
        dict: The attributes; empty when CF has no grid mapping for the CRS.
    """
    root = _parse_wkt(crs.to_wkt()) if crs else None
    if root is None:
        return {}
    kind = str(root[0]).upper()
    if kind == "COMPD_CS":
        out: dict[str, Any] = {}
        for sub in root[1:]:
            if not isinstance(sub, list):
                continue
            sk = str(sub[0]).upper()
            if sk in ("GEOGCS", "PROJCS"):
                h = _horizontal_to_cf(sub)
                if h is None:
                    return {}
                out.update(h)
            elif sk == "VERT_CS":
                vd = _child(sub, "VERT_DATUM")
                if vd is not None:
                    out["geopotential_datum_name"] = str(vd[1])
        return out
    return _horizontal_to_cf(root) or {}
