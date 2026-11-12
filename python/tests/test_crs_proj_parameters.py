"""PROJ parameter round trips against GDAL, locally validated with GDAL 3.13.3."""

import pickle
import subprocess
import sys
import warnings

import numpy as np
import pytest

import terrio
from terrio.rio.crs import CRS
from terrio.rio.errors import CRSError
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
    if code in (3031, 3976):
        assert crs.to_dict()["lat_ts"] < 0
        assert crs.to_dict()["lat_0"] == -90
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
    with warnings.catch_warnings():
        warnings.filterwarnings("ignore", message="best coordinate operation needs a missing grid")
        x, y = terrio.transform_points([point[0]], [point[1]], "EPSG:4326", original.to_proj4())
    np.testing.assert_allclose((x[0], y[0]), coordinates(original, point), rtol=0, atol=1e-6)


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


@pytest.mark.parametrize("whitespace", ["", " "])
def test_explicit_shift_survives_wkt_export(whitespace):
    """WKT TOWGS84 parameters survive even when the datum also has a PROJ name.

    Args:
        whitespace: Whitespace before the operation's opening bracket.
    """
    wkt = CRS.from_epsg(27700).to_wkt()
    # Insert an explicit operation at the datum's end, before the prime meridian.
    wkt = wkt.replace(
        "]],PRIMEM[", "],TOWGS84[446.448,-125.157,542.06,0.15,0.247,0.842,-20.489]],PRIMEM[", 1
    )
    wkt = wkt.replace("TOWGS84[", f"TOWGS84{whitespace}[")
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


@pytest.mark.parametrize(
    "code,point", [(27700, (-0.1276, 51.5072)), (31467, (9, 50)), (2100, (23, 38))]
)
def test_ordinary_wkt_and_dataset_crs_keep_authority_operations(tmp_path, code, point):
    """WKT, dataset, copied and unpickled CRSs keep authority-based operation selection.

    Args:
        tmp_path: Temporary directory.
        code: Projected CRS with a named datum.
        point: Longitude and latitude in the CRS area.
    """
    from affine import Affine

    import terrio.rio as rio
    from terrio.rio.warp import _crs_text

    authority = CRS.from_epsg(code)
    path = tmp_path / "datum.tif"
    with rio.open(
        path,
        "w",
        driver="GTiff",
        width=2,
        height=2,
        count=1,
        dtype="uint8",
        crs=authority,
        transform=Affine(1, 0, 0, 0, -1, 2),
    ) as dataset:
        dataset.write(np.ones((1, 2, 2), dtype="uint8"))
    with rio.open(path) as dataset:
        from_dataset = dataset.crs
    for crs in [
        CRS.from_wkt(authority.to_wkt()),
        from_dataset,
        CRS(from_dataset),
        pickle.loads(pickle.dumps(from_dataset)),
    ]:
        assert _crs_text(crs) == f"EPSG:{code}"
        np.testing.assert_allclose(
            coordinates(crs, point), coordinates(authority, point), rtol=0, atol=1e-6
        )


def test_equality_does_not_warn_about_implicit_parameter_exports():
    """Equality stays quiet while explicit export warns about an unrepresentable datum."""
    wkt = (
        'PROJCS["A",GEOGCS["ED50",DATUM["European_Datum_1950",'
        'SPHEROID["International 1924",6378388,297]],PRIMEM["Greenwich",0],'
        'UNIT["degree",0.0174532925199433]],PROJECTION["Equirectangular"],'
        'PARAMETER["standard_parallel_1",30],UNIT["metre",1]]'
    )
    first, second = CRS.from_wkt(wkt), CRS.from_wkt(wkt.replace('PROJCS["A"', 'PROJCS["B"'))
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        assert first == second
    assert not caught
    with pytest.warns(UserWarning, match="omit datum"):
        first.to_dict()


@pytest.mark.parametrize("later", ["+south=True", "+south"])
def test_duplicate_flag_values_keep_native_precedence(later):
    """Export and native coordinates both use the first occurrence of a boolean flag.

    Args:
        later: The second occurrence of the flag.
    """
    crs = CRS.from_proj4(f"+proj=utm +zone=33 +datum=WGS84 +south=False {later} +units=m")
    assert crs.to_dict()["south"] is False
    assert "+south" not in crs.to_proj4()
    np.testing.assert_allclose(
        coordinates(crs, (15, 30)), coordinates("EPSG:32633", (15, 30)), rtol=0, atol=1e-7
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
    definition = f"+proj=utm +zone=33 +south={value} +datum=WGS84 +units=m"
    result = subprocess.run(
        [sys.executable, "-m", "terrio", "crs", "EPSG:4326", "--to", definition, "15", "-30"],
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


@pytest.mark.parametrize("value", ["False", "True"])
def test_init_boolean_overrides_are_refused(value):
    """An unsupported init override must not silently use the authority hemisphere.

    Args:
        value: A valid explicit boolean value.
    """
    with pytest.raises(ValueError, match="overrides.*init"):
        CRS.from_proj4(f"+init=epsg:32633 +south={value}")


@pytest.mark.parametrize("authority", ["epsg:32633", "ESRI:102003", "OGC:CRS84"])
@pytest.mark.parametrize("enabled", [False, True])
@pytest.mark.parametrize("constructor", ["dict", "kwargs"])
def test_init_mapping_overrides_are_not_silently_dropped(authority, enabled, constructor):
    """Mapping constructors refuse unsupported init overrides like PROJ-string inputs.

    Args:
        authority: Registry initialization.
        enabled: An explicit hemisphere flag value.
        constructor: Mapping or keyword constructor.
    """
    params = {"init": authority, "south": enabled}
    with pytest.raises(ValueError, match="overrides.*init"):
        if constructor == "dict":
            CRS.from_dict(params)
        else:
            CRS(**params)


@pytest.mark.parametrize("authority", ["epsg:32633", "ESRI:102003", "OGC:CRS84"])
def test_init_mapping_metadata_flags_preserve_authority_support(authority):
    """Inert metadata switches retain support for non-EPSG initialization too.

    Args:
        authority: Registry initialization.
    """
    assert CRS.from_dict(
        {"init": authority, "no_defs": False, "wktext": True}
    ) == CRS.from_user_input(authority)


@pytest.mark.parametrize("authority", ["epsg:32633", "ESRI:102003"])
def test_init_mapping_metadata_flag_errors_use_the_public_exception(authority):
    """Invalid metadata flags retain the compatibility API's CRS error class.

    Args:
        authority: Registry initialization.
    """
    with pytest.raises(CRSError, match="boolean flag"):
        CRS.from_dict({"init": authority, "no_defs": "maybe"})


def test_named_datum_with_custom_spheroid_warns_and_preserves_axes():
    """A recognized datum name cannot replace a caller's custom ellipsoid."""
    wkt = CRS.from_epsg(4326).to_wkt().replace("6378137", "6379000")
    wkt = wkt.removesuffix(',AUTHORITY["EPSG","4326"]]') + "]"
    crs = CRS.from_wkt(wkt)
    with pytest.warns(UserWarning, match="omit datum"):
        params = crs.to_dict()
    assert params["a"] == 6379000
    assert "datum" not in params


@pytest.mark.parametrize("authority", ["EPSG:32633", "ESRI:102003", "OGC:CRS84"])
def test_init_mapping_non_switch_parameters_keep_authority(authority):
    """Legacy init mappings retain their authority despite ignored numeric parameters.

    Args:
        authority: Registry initialization.
    """
    assert CRS.from_dict({"init": authority, "units": "m"}) == CRS.from_user_input(authority)


def test_declared_projected_authority_exports_its_effective_spheroid():
    """A conflicting WKT spheroid cannot change coordinates after a PROJ round trip."""
    crs = CRS.from_wkt(CRS.from_epsg(32633).to_wkt().replace("6378137", "6379000"))
    assert crs.to_epsg() == 32633
    expected = coordinates(crs, (16, 30))
    for back in [CRS.from_dict(crs.to_dict()), CRS.from_proj4(crs.to_proj4())]:
        np.testing.assert_allclose(coordinates(back, (16, 30)), expected, rtol=0, atol=1e-6)


@pytest.mark.parametrize("method", ["to_dict", "to_proj4"])
def test_authority_export_warns_when_projection_nodes_conflict(method):
    """Report when an authority identity overrides edited projection parameters.

    Args:
        method: Public parameter export method.
    """
    wkt = CRS.from_epsg(32633).to_wkt().replace('"central_meridian",15', '"central_meridian",9')
    crs = CRS.from_wkt(wkt)
    with pytest.warns(UserWarning, match="conflicting WKT parameters") as caught:
        result = getattr(crs, method)()
    assert caught[0].filename == __file__
    assert result == getattr(CRS.from_epsg(32633), method)()


@pytest.mark.parametrize("access", [lambda crs: crs["units"], len, list])
def test_mapping_datum_warning_points_at_public_caller(access):
    """Attribute mapping-interface datum warnings to the public caller.

    Args:
        access: Mapping operation to exercise.
    """
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        access(CRS.from_epsg(23031))
    assert caught
    assert all(warning.filename == __file__ for warning in caught)


def test_registry_parameter_exports_reuse_native_lookup(monkeypatch):
    """Repeated parameter exports reuse their parsed registry definition.

    Args:
        monkeypatch: Scoped native metadata lookup replacement.
    """
    from terrio import _terrio

    crs = CRS.from_wkt(CRS.from_epsg(32633).to_wkt())
    original = _terrio.crs_info
    calls = []

    def observed(definition):
        """Record only the registry lookup used for parameter export.

        Args:
            definition: Native CRS input.

        Returns:
            dict: Native metadata.
        """
        if definition == "EPSG:32633":
            calls.append(definition)
        return original(definition)

    monkeypatch.setattr(_terrio, "crs_info", observed)
    root = crs._root
    for _ in range(100):
        assert crs.to_dict()["zone"] == 33
        assert "+zone=33" in crs.to_proj4()
        assert crs._root is root
    assert calls == ["EPSG:32633"]


def test_unknown_declared_authority_has_safe_parameter_exports():
    """An unregistered authority retains representable WKT parameter exports."""
    wkt = (
        CRS.from_epsg(32633)
        .to_wkt()
        .replace('AUTHORITY["EPSG","32633"]', 'AUTHORITY["EPSG","999999"]')
    )
    crs = CRS.from_wkt(wkt)
    assert crs.to_epsg() == 999999
    expected = CRS.from_epsg(32633)
    assert crs.to_dict() == expected.to_dict()
    assert crs.to_proj4() == expected.to_proj4()
    assert list(crs) == list(expected)


def test_equality_does_not_warn_about_registry_parameter_conflicts():
    """Equality suppresses export-policy warnings even with warnings as errors."""
    canonical = CRS.from_epsg(32633).to_wkt()
    tagged = CRS.from_wkt(canonical.replace('"central_meridian",15', '"central_meridian",9'))
    untagged = CRS.from_wkt(
        canonical.replace(',AUTHORITY["EPSG","32633"]', "")
        .replace('"central_meridian",15', '"central_meridian",11')
        .replace("WGS 84 / UTM zone 33N", "custom")
    )
    assert tagged.to_epsg() == 32633
    assert untagged.to_epsg() is None
    with warnings.catch_warnings():
        warnings.simplefilter("error", UserWarning)
        assert tagged != untagged


def test_wkt2_mercator_registry_export_retains_spherical_extension():
    """A WKT2 import uses Web Mercator's registry extension in its literal export."""
    pyproj = pytest.importorskip("pyproj")
    crs = CRS.from_wkt(pyproj.CRS.from_epsg(3857).to_wkt())
    exported = crs.to_proj4()
    np.testing.assert_allclose(
        coordinates(CRS.from_proj4(exported), (10, 50)),
        coordinates("EPSG:3857", (10, 50)),
        rtol=0,
        atol=1e-6,
    )


@pytest.mark.parametrize("authority,point", [("ESRI:102003", (-95, 40)), ("OGC:CRS84", (10, 50))])
def test_non_epsg_authority_transforms_match_their_wkt(authority, point):
    """Non-EPSG authority inputs retain coordinates when selecting their native registry.

    Args:
        authority: Native ESRI or OGC authority input.
        point: A geographic point within its area.
    """
    crs = CRS.from_user_input(authority)
    np.testing.assert_allclose(
        coordinates(crs, point),
        coordinates(CRS.from_wkt(crs.to_wkt()), point),
        rtol=0,
        atol=1e-6,
    )


def test_etrs89_export_reports_loss_of_datum_identity():
    """An ellipsoid-only ETRS89 export explicitly reports its omitted datum identity."""
    crs = CRS.from_epsg(25832)
    with pytest.warns(UserWarning, match="omit datum") as caught:
        parameters = crs.to_dict()
    assert len(caught) == 1
    assert "datum" not in parameters
    assert parameters["a"] == 6378137


def test_distinct_unrepresentable_datums_are_not_equal():
    """Equal ellipsoids do not erase distinct datum identities during equality."""
    wkt = (
        'GEOGCS["Custom",DATUM["First_unknown_datum",'
        'SPHEROID["Bessel",6377397.155,299.1528128]],'
        'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]'
    )
    first = CRS.from_wkt(wkt)
    second = CRS.from_wkt(wkt.replace("First_unknown_datum", "Second_unknown_datum"))
    assert first != second


def test_authority_transform_does_not_export_wkt(monkeypatch):
    """A copied authority CRS uses its code without exporting WKT.

    Args:
        monkeypatch: Pytest's attribute replacement fixture.
    """
    from terrio.rio.warp import _crs_text

    crs = CRS(CRS.from_epsg(32633))

    def refuse_export(self):
        """Fail if authority selection invokes a WKT export.

        Args:
            self: The CRS whose export was requested.
        """
        raise AssertionError("unnecessary WKT export")

    monkeypatch.setattr(CRS, "to_wkt", refuse_export)
    assert _crs_text(crs) == "EPSG:32633"


@pytest.mark.parametrize("method", ["to_dict", "to_proj4"])
def test_datum_warning_points_at_public_caller(method):
    """Both public parameter exporters attribute a lossy export warning to the caller.

    Args:
        method: The public export method.
    """
    crs = CRS.from_epsg(23031)
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        getattr(crs, method)()
    assert len(caught) == 1
    assert caught[0].filename == __file__


def test_parameter_getters_do_not_modify_crs_identity():
    """Exporting a validated extension does not populate shared identity metadata."""
    crs = CRS.from_epsg(3857)
    before = dict(crs._info)
    code = crs.to_epsg()
    assert crs.to_dict()
    assert crs.to_proj4()
    assert crs._info == before
    assert crs.to_epsg() == code


@pytest.mark.parametrize("code,point", [(27700, (-0.1276, 51.5072)), (23031, (2, 48))])
def test_gdal_serialized_synthetic_shift_keeps_the_explicit_operation(code, point):
    """A GDAL-written shift remains authoritative even on a dataset CRS with an EPSG ID.

    Args:
        code: Authority definition used as the GDAL WKT source.
        point: A longitude and latitude in the CRS's area.
    """
    osr = pytest.importorskip("osgeo.osr")
    pyproj = pytest.importorskip("pyproj")

    from terrio.rio.warp import _crs_text

    reference = osr.SpatialReference()
    reference.ImportFromEPSG(code)
    reference.SetTOWGS84(10, 20, 30)
    wkt = reference.ExportToWkt()
    assert "TOWGS84[" in wkt
    assert f'AUTHORITY["EPSG","{code}"]' in wkt
    crs = CRS._from_dataset(CRS.from_wkt(wkt))
    assert "TOWGS84[" in _crs_text(crs)
    assert pyproj.CRS.from_wkt(wkt).is_bound
    expected = pyproj.Transformer.from_crs(4326, wkt, always_xy=True).transform(*point)
    np.testing.assert_allclose(coordinates(crs, point), expected, rtol=0, atol=1e-3)


def test_validated_extension_is_cached_without_changing_identity(monkeypatch):
    """Repeated exports validate one captured WKT once and keep identity metadata intact.

    Args:
        monkeypatch: Scoped native call replacement.
    """
    from terrio import _terrio

    crs = CRS.from_epsg(3857)
    before = dict(crs._info)
    original = _terrio.crs_info
    calls = []

    def record(definition):
        """Record native validation calls.

        Args:
            definition: The CRS being validated.

        Returns:
            dict: Native CRS metadata.
        """
        calls.append(definition)
        return original(definition)

    monkeypatch.setattr(_terrio, "crs_info", record)
    assert crs.to_dict() == crs.to_dict()
    assert crs.to_proj4()
    assert calls == [crs.wkt]
    assert crs._info == before


@pytest.mark.parametrize("code", [27700, 23031])
def test_gdal_written_authority_dataset_keeps_unbound_selection(tmp_path, code):
    """Standard GDAL WKT1 exports have no shift and retain authority operation selection.

    Locally verified with GDAL 3.13.3.

    Args:
        tmp_path: Temporary GeoTIFF directory.
        code: Projected authority CRS.
    """
    gdal = pytest.importorskip("osgeo.gdal")
    osr = pytest.importorskip("osgeo.osr")
    import terrio.rio as rio
    from terrio.rio.warp import _crs_text

    crs = osr.SpatialReference()
    crs.ImportFromEPSG(code)
    wkt = crs.ExportToWkt()
    assert "TOWGS84" not in wkt
    path = tmp_path / "authority.tif"
    dataset = gdal.GetDriverByName("GTiff").Create(str(path), 2, 2, 1, gdal.GDT_Byte)
    dataset.SetProjection(wkt)
    dataset.SetGeoTransform((0, 1, 0, 2, 0, -1))
    dataset = None
    with rio.open(path) as opened:
        assert _crs_text(opened.crs) == f"EPSG:{code}"
