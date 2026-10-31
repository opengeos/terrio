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


def _g(v: Any) -> str:
    """A number as GDAL writes it in WKT1 (`%.15g`).

    Args:
        v: The number.

    Returns:
        str: The text.
    """
    return f"{float(v):.15g}"


# CF attributes that name parts of the CRS. pyproj builds named or registered datums,
# ellipsoids and prime meridians from them, which the templates below do not reproduce.
_CF_NAMED = (
    "horizontal_datum_name",
    "reference_ellipsoid_name",
    "prime_meridian_name",
    "geographic_crs_name",
    "projected_crs_name",
    "towgs84",
    "crs_wkt",
    "spatial_ref",
)

_WGS84_DATUM = (
    'DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563,AUTHORITY["EPSG","7030"]],'
    'AUTHORITY["EPSG","6326"]]'
)


def _cf_geogcs(attrs: Any, geographic: bool) -> str:
    """The GEOGCS of CF grid mapping attributes, as GDAL writes pyproj's `CRS.from_cf`.

    Args:
        attrs: The grid mapping attributes.
        geographic: True for a geographic CRS (unit authority and axes are written).

    Returns:
        str: The WKT1 GEOGCS.
    """
    if "earth_radius" in attrs:
        datum = f'DATUM["undefined",SPHEROID["undefined",{_g(attrs["earth_radius"])},0]]'
    elif "semi_major_axis" in attrs and "inverse_flattening" in attrs:
        a, rf = attrs["semi_major_axis"], attrs["inverse_flattening"]
        datum = f'DATUM["undefined",SPHEROID["undefined",{_g(a)},{_g(rf)}]]'
    elif "semi_major_axis" in attrs and "semi_minor_axis" in attrs:
        a, b = float(attrs["semi_major_axis"]), float(attrs["semi_minor_axis"])
        rf = 0.0 if a == b else a / (a - b)
        datum = f'DATUM["undefined",SPHEROID["undefined",{_g(a)},{_g(rf)}]]'
    else:
        datum = _WGS84_DATUM
    if "longitude_of_prime_meridian" in attrs:
        primem = f'PRIMEM["undefined",{_g(attrs["longitude_of_prime_meridian"])}]'
    else:
        primem = 'PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]]'
    if geographic:
        unit = 'UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],'
        unit += 'AXIS["Longitude",EAST],AXIS["Latitude",NORTH]'
    else:
        unit = 'UNIT["degree",0.0174532925199433]'
    return f'GEOGCS["undefined",{datum},{primem},{unit}]'


def _cf_parallels(attrs: Any) -> list[float]:
    """The `standard_parallel` attribute as a list (a scalar is one parallel).

    Args:
        attrs: The grid mapping attributes.

    Returns:
        list: The parallels.
    """
    sp = attrs["standard_parallel"]
    try:
        return [float(v) for v in sp]
    except TypeError:
        return [float(sp)]


def _cf_projection(attrs: Any) -> tuple[str, list[tuple[str, float]]] | None:
    """The WKT1 projection and parameters of a CF projected grid mapping.

    Args:
        attrs: The grid mapping attributes.

    Returns:
        tuple | None: `(PROJECTION name, [(PARAMETER name, value), ...])`, or None when the
        grid mapping is not one whose rendering is reproduced here.
    """
    name = attrs["grid_mapping_name"]
    fe = ("false_easting", attrs.get("false_easting", 0.0))
    fn = ("false_northing", attrs.get("false_northing", 0.0))
    lat0 = attrs.get("latitude_of_projection_origin", 0.0)
    if name == "lambert_conformal_conic":
        sp = _cf_parallels(attrs)
        lon0 = attrs.get("longitude_of_central_meridian", 0.0)
        if len(sp) == 2:
            return "Lambert_Conformal_Conic_2SP", [
                ("standard_parallel_1", sp[0]),
                ("standard_parallel_2", sp[1]),
                ("latitude_of_origin", lat0),
                ("central_meridian", lon0),
                fe,
                fn,
            ]
        if len(sp) == 1:
            return "Lambert_Conformal_Conic_1SP", [
                ("latitude_of_origin", sp[0]),
                ("central_meridian", lon0),
                ("scale_factor", attrs.get("scale_factor_at_projection_origin", 1.0)),
                fe,
                fn,
            ]
        return None
    if name == "transverse_mercator":
        return "Transverse_Mercator", [
            ("latitude_of_origin", lat0),
            ("central_meridian", attrs.get("longitude_of_central_meridian", 0.0)),
            ("scale_factor", attrs.get("scale_factor_at_central_meridian", 1.0)),
            fe,
            fn,
        ]
    if name == "albers_conical_equal_area":
        sp = _cf_parallels(attrs)
        if len(sp) != 2:
            return None
        return "Albers_Conic_Equal_Area", [
            ("latitude_of_center", lat0),
            ("longitude_of_center", attrs.get("longitude_of_central_meridian", 0.0)),
            ("standard_parallel_1", sp[0]),
            ("standard_parallel_2", sp[1]),
            fe,
            fn,
        ]
    if name == "lambert_azimuthal_equal_area":
        return "Lambert_Azimuthal_Equal_Area", [
            ("latitude_of_center", lat0),
            ("longitude_of_center", attrs.get("longitude_of_projection_origin", 0.0)),
            fe,
            fn,
        ]
    if name == "polar_stereographic":
        lon0 = attrs.get("straight_vertical_longitude_from_pole", 0.0)
        if "scale_factor_at_projection_origin" in attrs:
            return "Polar_Stereographic", [
                ("latitude_of_origin", lat0),
                ("central_meridian", lon0),
                ("scale_factor", attrs["scale_factor_at_projection_origin"]),
                fe,
                fn,
            ]
        if "standard_parallel" in attrs:
            sp = _cf_parallels(attrs)
            return "Polar_Stereographic", [
                ("latitude_of_origin", sp[0]),
                ("central_meridian", lon0),
                fe,
                fn,
            ]
        return None
    if name == "mercator" and "standard_parallel" in attrs:
        return "Mercator_2SP", [
            ("standard_parallel_1", _cf_parallels(attrs)[0]),
            ("central_meridian", attrs.get("longitude_of_projection_origin", 0.0)),
            fe,
            fn,
        ]
    return None


def crs_from_cf(attrs: Any) -> CRS | None:
    """The CRS of CF grid mapping attributes, as rioxarray reads it.

    rioxarray falls back to `pyproj.CRS.from_cf(attrs)` when the grid mapping has neither
    `spatial_ref` nor `crs_wkt`, and rasterio holds that CRS as GDAL's WKT1. For the grid
    mappings rendered here (latitude_longitude, Lambert conformal conic with one or two
    parallels, transverse Mercator, Albers, Lambert azimuthal equal area, polar
    stereographic A and B, Mercator 2SP) the CRS is that WKT1, text for text. Others are
    read through pyproj when it is installed, else not at all.

    Args:
        attrs: The grid mapping coordinate's attributes.

    Returns:
        CRS | None: The CRS, or None when the attributes do not describe one.
    """
    name = attrs.get("grid_mapping_name")
    if not name:
        return None
    wkt = None
    if not any(k in attrs for k in _CF_NAMED):
        try:
            if name == "latitude_longitude":
                wkt = _cf_geogcs(attrs, geographic=True)
            else:
                proj = _cf_projection(attrs)
                if proj is not None:
                    method, params = proj
                    body = ",".join(f'PARAMETER["{k}",{_g(v)}]' for k, v in params)
                    wkt = (
                        f'PROJCS["undefined",{_cf_geogcs(attrs, geographic=False)},'
                        f'PROJECTION["{method}"],{body},UNIT["metre",1,AUTHORITY["EPSG","9001"]],'
                        'AXIS["Easting",EAST],AXIS["Northing",NORTH]]'
                    )
        except (KeyError, TypeError, ValueError):
            wkt = None
    if wkt is not None:
        return CRS.from_wkt(wkt)
    try:
        import pyproj
    except ImportError:
        return None
    try:
        return CRS.from_wkt(pyproj.CRS.from_cf(dict(attrs)).to_wkt("WKT1_GDAL"))
    except Exception:  # noqa: BLE001 - not a CRS rioxarray could read either
        return None


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
