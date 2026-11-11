"""PROJ parameter round trips against GDAL, locally validated with GDAL 3.13.3."""

import subprocess
import sys
import warnings
from pathlib import Path

import numpy as np
import pytest

from terrio.rio.crs import CRS
from terrio.rio.warp import transform


def coordinates(crs, point):
    """Transform a WGS 84 point while allowing a known missing-grid fallback.

    Args:
        crs: Destination CRS.
        point: Longitude and latitude in degrees.

    Returns:
        tuple: Projected x and y coordinates.
    """
    with warnings.catch_warnings():
        warnings.filterwarnings("ignore", message="best coordinate operation needs a missing grid")
        x, y = transform("EPSG:4326", crs, [point[0]], [point[1]])
    return x[0], y[0]


CASES = [
    (3031, (0, -75)),
    (3413, (-45, 75)),
    (3976, (0, -75)),
    (3994, (150, -41)),
    (3857, (10, 50)),
    (32661, (0, 85)),
    (32761, (0, -85)),
    (32633, (15, 30)),
    (32733, (15, -30)),
    (32198, (-73, 48)),
    (5070, (-95, 40)),
    (4087, (40, 45)),
]


@pytest.mark.parametrize("code,point", CASES)
@pytest.mark.parametrize("from_wkt", [False, True])
def test_projection_parameters_preserve_coordinates(code, point, from_wkt):
    """Authority and WKT exports preserve projections through both public encodings.

    Args:
        code: EPSG projection code.
        point: A point in the projection's area.
        from_wkt: Whether to reconstruct the input through WKT first.
    """
    crs = CRS.from_epsg(code)
    if from_wkt:
        crs = CRS.from_wkt(crs.to_wkt())
    expected = coordinates(crs, point)
    exported = crs.to_proj4()
    assert exported
    for back in [
        CRS.from_proj4(exported),
        CRS.from_user_input(exported),
        CRS.from_dict(crs.to_dict()),
    ]:
        np.testing.assert_allclose(coordinates(back, point), expected, rtol=0, atol=1e-6)
        # These UPS code pairs differ in formal axis order. PROJ parameter exports keep
        # coordinates in the public GIS x/y order, rather than those authority axes.
        if code not in (32661, 32761):
            assert back == crs


@pytest.mark.parametrize("code,point", CASES[:9])
def test_exported_projections_match_gdal(code, point):
    """Exported projections agree with an independent GDAL coordinate transform.

    Args:
        code: EPSG projection code.
        point: Longitude and latitude in degrees.
    """
    pytest.importorskip("rasterio")
    from rasterio.warp import transform as reference_transform

    expected = reference_transform("EPSG:4326", f"EPSG:{code}", [point[0]], [point[1]])
    result = coordinates(CRS.from_proj4(CRS.from_epsg(code).to_proj4()), point)
    np.testing.assert_allclose(result, [expected[0][0], expected[1][0]], rtol=0, atol=1e-5)


@pytest.mark.parametrize(
    "code,name,point",
    [
        (27700, "OSGB36", (-0.1276, 51.5072)),
        (31467, "potsdam", (9, 50)),
        (2100, "GGRS87", (23.73, 37.98)),
    ],
)
def test_named_datums_keep_coordinate_operations(code, name, point):
    """Named datums retain area-based operation selection, including grid availability.

    Args:
        code: EPSG CRS code.
        name: PROJ datum name.
        point: Longitude and latitude in the CRS area.
    """
    original = CRS.from_epsg(code)
    assert original.to_dict()["datum"] == name
    back = CRS.from_proj4(original.to_proj4())
    np.testing.assert_allclose(
        coordinates(back, point), coordinates(original, point), rtol=0, atol=1e-6
    )


@pytest.mark.parametrize("code", [23031, 28992, 4258])
def test_unrepresentable_datum_export_warns(code):
    """A datum without a PROJ name or explicit shift is not silently reduced to an ellipsoid.

    Args:
        code: CRS whose datum cannot be represented by a PROJ datum name.
    """
    crs = CRS.from_epsg(code)
    with pytest.warns(UserWarning, match="omit datum.*to_wkt"):
        params = crs.to_dict()
    assert "a" in params and "datum" not in params
    with pytest.warns(UserWarning, match="omit datum.*to_wkt"):
        assert crs.to_proj4()


def test_explicit_shift_survives_wkt_export():
    """WKT TOWGS84 parameters survive even when the datum also has a PROJ name."""
    wkt = CRS.from_epsg(27700).to_wkt()
    # Insert an explicit operation at the datum's end, before the prime meridian.
    wkt = wkt.replace(
        "]],PRIMEM[", "],TOWGS84[446.448,-125.157,542.06,0.15,0.247,0.842,-20.489]],PRIMEM[", 1
    )
    crs = CRS.from_wkt(wkt)
    assert "towgs84" in crs.to_dict()
    back = CRS.from_dict(crs.to_dict())
    # The registry's legacy Airy semiminor axis is rounded to millimeters; WKT uses
    # inverse flattening instead. Preserve the operation within that 1 mm precision.
    np.testing.assert_allclose(
        coordinates(back, (-0.1276, 51.5072)),
        coordinates(crs, (-0.1276, 51.5072)),
        rtol=0,
        atol=1e-3,
    )


def test_equirectangular_standard_parallel_is_not_an_origin():
    """A nonzero equirectangular standard parallel controls horizontal scale."""
    original = CRS.from_proj4("+proj=eqc +lat_ts=30 +datum=WGS84 +units=m")
    crs = CRS.from_wkt(original.to_wkt())
    params = crs.to_dict()
    assert params["lat_ts"] == 30 and "lat_1" not in params
    np.testing.assert_allclose(
        coordinates(CRS.from_dict(params), (40, 45)),
        coordinates(original, (40, 45)),
        rtol=0,
        atol=1e-6,
    )


def test_oblique_stereographic_is_not_equal_to_a_polar_standard_parallel():
    """An oblique origin is not the same CRS as a polar variant-B standard parallel."""
    polar = CRS.from_epsg(3031)
    oblique = CRS.from_proj4("+proj=stere +lat_0=-71 +lon_0=0 +datum=WGS84 +units=m")
    assert polar != oblique


@pytest.mark.parametrize(
    "definition,flag",
    [
        ("+proj=longlat +datum=WGS84 +over +no_defs", "over"),
        ("+proj=tmerc +lon_0=9 +approx +datum=WGS84 +units=m", "approx"),
    ],
)
def test_bare_flags_keep_their_type(definition, flag):
    """User-supplied bare flags remain booleans after repeated export.

    Args:
        definition: PROJ definition with a bare flag.
        flag: Parameter whose type is checked.
    """
    crs = CRS.from_proj4(definition)
    back = CRS.from_proj4(crs.to_proj4())
    assert back.to_dict()[flag] is True
    assert back.to_dict() == crs.to_dict()


@pytest.mark.parametrize(
    "value,south",
    [("False", False), ("F", False), ("f", False), ("0", False), ("True", True), ("1", True)],
)
def test_explicit_flag_values_preserve_types_and_hemisphere(value, south):
    """Boolean-valued flags normalize on export and set the correct UTM hemisphere.

    Args:
        value: Explicit flag value.
        south: Whether the flag enables the southern hemisphere.
    """
    crs = CRS.from_proj4(f"+proj=utm +zone=33 +south={value} +datum=WGS84 +units=m +no_defs=True")
    assert crs.to_dict()["south"] is south
    assert crs.to_dict()["no_defs"] is True
    exported = crs.to_proj4()
    assert "=True" not in exported and "=False" not in exported
    assert ("+south" in exported) is south
    expected = coordinates(f"EPSG:{32733 if south else 32633}", (15, -30))
    for back in [crs, CRS.from_proj4(exported), CRS.from_wkt(crs.to_wkt())]:
        np.testing.assert_allclose(coordinates(back, (15, -30)), expected, rtol=0, atol=1e-7)


@pytest.mark.parametrize("value", ["maybe", "2", "NaN", ""])
@pytest.mark.parametrize("prefix", ["+proj=utm +zone=33", "+init=epsg:32633"])
def test_invalid_boolean_flag_values_are_refused(value, prefix):
    """Unknown flag values fail rather than silently selecting the southern hemisphere.

    Args:
        value: Invalid boolean value.
        prefix: A projected definition or an authority initialization.
    """
    with pytest.raises(ValueError, match="boolean flag"):
        CRS.from_proj4(f"{prefix} +south={value} +datum=WGS84")


@pytest.mark.parametrize("value,code", [("False", 32633), ("True", 32733)])
def test_cli_reads_boolean_flag_values(value, code):
    """The CLI shares the native parser's boolean semantics.

    Args:
        value: Explicit south flag.
        code: Expected UTM authority code.
    """
    cli = Path(sys.executable).parent / "terrio"
    definition = f"+proj=utm +zone=33 +south={value} +datum=WGS84 +units=m"
    result = subprocess.run(
        [str(cli), "crs", "EPSG:4326", "--to", definition, "15", "-30"],
        check=True,
        text=True,
        capture_output=True,
    )
    np.testing.assert_allclose(
        [float(value) for value in result.stdout.split()],
        coordinates(f"EPSG:{code}", (15, -30)),
        rtol=0,
        atol=1e-7,
    )
