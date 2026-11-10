"""A layer's CRS, from a Shapefile `.prj` or another source, is a CRS definition or nothing.

A `.prj` that holds no CRS definition gives its layer no CRS, with a warning, through
`terrio.open_vector`, `terrio.pyogrio` and the command line alike, as an empty `.prj` or none does
(silently). A definition is kept as it is, whether terrio reads it or it only has the form of one
(an engineering CRS's WKT, `IGNF:LAMB93`). A definition that pyproj cannot parse reaches
`terrio.pyogrio` callers as `None`, with a warning, since a GeoDataFrame refuses it.
"""

import json
import sqlite3
import subprocess
import sys
import types
import warnings

import pytest

import terrio
import terrio.pyogrio as tpg

POINTS = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"id": i},
            "geometry": {"type": "Point", "coordinates": [10 + i, 46]},
        }
        for i in range(3)
    ],
}
ESRI_WGS84 = (
    'GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],'
    'PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]'
)
CUSTOM_TM = (
    'PROJCS["custom",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],'
    'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],'
    'PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",15.5],'
    'PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],'
    'PARAMETER["false_northing",0],UNIT["metre",1]]'
)
# Text that is no CRS definition, a colon between two words included.
NOT_A_CRS = ["not a coordinate reference system", "http://example.com/crs", "note:draft"]
# An engineering CRS: a definition terrio's parser does not read.
LOCAL_CS = 'LOCAL_CS["site grid",UNIT["metre",1]]'
# An OGC URN of a CRS of an authority terrio does not read.
IGNF_URN = "urn:ogc:def:crs:IGNF::LAMB93"
# Control characters: ESC, which opens a terminal's escape sequences, BEL and the 8-bit CSI.
CONTROL = "\x1b[31mred\x1b[0m\x07\x9b2J"
# CONTROL as a warning quotes it, each control character as a space.
CONTROL_QUOTED = " [31mred [0m  2J"


def _shapefile(tmp_path, prj):
    """`pts.shp`, three points, with `prj` as its `.prj` (none when `prj` is None)."""
    path = tmp_path / "pts.shp"
    terrio.Layer.from_geojson(POINTS, name="pts").write(str(path))
    if prj is None:
        (tmp_path / "pts.prj").unlink()
    else:
        (tmp_path / "pts.prj").write_text(prj, encoding="utf-8")
    return str(path)


def _geopackage(tmp_path, definition):
    """`pts.gpkg`, three points, whose CRS is stored as `definition` without an authority."""
    path = tmp_path / "pts.gpkg"
    terrio.Layer.from_geojson(POINTS, name="pts").write(str(path))
    with sqlite3.connect(path) as db:
        db.execute(
            "UPDATE gpkg_spatial_ref_sys SET organization = 'NONE', organization_coordsys_id = 0,"
            " definition = ? WHERE srs_id = 4326",
            (definition,),
        )
    db.close()
    return str(path)


def _prj_warning(prj):
    """The warning for a `pts.prj` holding `prj`, which is not a CRS definition."""
    return (
        f"pts.prj holds no CRS definition terrio can read ('{prj}'): "
        "the layer is read without a CRS"
    )


def _warned(call):
    """`call()`'s result and the messages of the UserWarnings it issues."""
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        result = call()
    return result, [str(w.message) for w in caught if issubclass(w.category, UserWarning)]


@pytest.mark.parametrize("prj", NOT_A_CRS)
def test_a_prj_without_a_crs_definition_gives_no_crs_and_a_warning(tmp_path, prj):
    path = _shapefile(tmp_path, prj)
    warning = _prj_warning(prj)
    info, messages = _warned(lambda: tpg.read_info(path))
    assert (info["crs"], info["features"], messages) == (None, 3, [warning])
    layer, messages = _warned(lambda: terrio.open_vector(path)[0])
    assert (layer.crs, layer.crs_wkt, len(layer), messages) == (None, None, 3, [warning])


def test_the_command_line_reads_the_layer_without_the_prj(tmp_path):
    path = _shapefile(tmp_path, NOT_A_CRS[0])
    warning = f"terrio: warning: {_prj_warning(NOT_A_CRS[0])}\n"

    def run(*args):
        cmd = [sys.executable, "-m", "terrio", *args]
        done = subprocess.run(cmd, capture_output=True, text=True, cwd=tmp_path)
        assert done.returncode == 0, done.stderr
        return done.stdout, done.stderr

    out, err = run("info", path)
    assert ("CRS: unknown" in out.splitlines(), err) == (True, warning)
    out, err = run("info", "--json", path)
    assert (json.loads(out)["layers"][0]["crs"], err) == (
        {"epsg": None, "name": None, "wkt": None},
        warning,
    )
    # The text is not written out as a CRS either.
    assert run("convert", path, "out.shp") == ("", warning)
    assert not (tmp_path / "out.prj").exists()
    assert run("convert", path, "out.gpkg") == ("", warning)
    assert terrio.open_vector(str(tmp_path / "out.gpkg"))[0].crs is None


@pytest.mark.parametrize("prj", [None, "", " \n", "\ufeff"])
def test_an_empty_prj_gives_no_crs_silently(tmp_path, prj):
    path = _shapefile(tmp_path, prj)
    info, messages = _warned(lambda: tpg.read_info(path))
    assert (info["crs"], messages) == (None, [])
    layer, messages = _warned(lambda: terrio.open_vector(path)[0])
    assert (layer.crs, layer.crs_wkt, messages) == (None, None, [])


@pytest.mark.parametrize("prj", [ESRI_WGS84, "\ufeff" + ESRI_WGS84, ESRI_WGS84 + "\r\n"])
def test_a_prj_identified_as_an_epsg_crs_gives_that_crs(tmp_path, prj):
    path = _shapefile(tmp_path, prj)
    info, messages = _warned(lambda: tpg.read_info(path))
    assert (info["crs"], messages) == ("EPSG:4326", [])
    layer, messages = _warned(lambda: terrio.open_vector(path)[0])
    assert (layer.crs, messages) == ("EPSG:4326", [])


def test_a_prj_terrio_reads_without_an_epsg_code_is_kept(tmp_path):
    path = _shapefile(tmp_path, CUSTOM_TM + "\n")
    layer, messages = _warned(lambda: terrio.open_vector(path)[0])
    assert (layer.crs, layer.crs_wkt, messages) == (CUSTOM_TM, CUSTOM_TM, [])
    info, messages = _warned(lambda: tpg.read_info(path))
    assert info["crs"].startswith('PROJCS["custom",GEOGCS["WGS 84",DATUM["WGS_1984",')
    assert 'PARAMETER["central_meridian",15.5]' in info["crs"]
    assert messages == []


@pytest.mark.parametrize("prj", [LOCAL_CS, "IGNF:LAMB93", IGNF_URN])
def test_a_crs_definition_terrio_does_not_read_is_kept(tmp_path, prj):
    path = _shapefile(tmp_path, prj)
    layer, messages = _warned(lambda: terrio.open_vector(path)[0])
    assert (layer.crs, layer.crs_wkt, messages) == (prj, prj, [])
    pytest.importorskip("pyproj")
    info, messages = _warned(lambda: tpg.read_info(path))
    assert messages == []
    assert info["crs"].startswith(
        'LOCAL_CS["site grid",UNIT["metre",1' if prj == LOCAL_CS else 'PROJCS["RGF93 Lambert 93",'
    )


@pytest.mark.parametrize(
    ("prj", "name", "epsg"),
    [
        (LOCAL_CS, "site grid", None),
        ("IGNF:LAMB93", "RGF93 Lambert 93", 2154),
        (IGNF_URN, "RGF93 Lambert 93", 2154),
    ],
)
def test_read_dataframe_keeps_a_crs_definition_terrio_does_not_read(tmp_path, prj, name, epsg):
    pytest.importorskip("geopandas")
    df, messages = _warned(lambda: tpg.read_dataframe(_shapefile(tmp_path, prj)))
    assert (df.crs.name, df.crs.to_epsg(), messages) == (name, epsg, [])


@pytest.mark.parametrize("prj", NOT_A_CRS)
def test_read_dataframe_reads_the_layer_without_the_prj(tmp_path, prj):
    gpd = pytest.importorskip("geopandas")
    path = _shapefile(tmp_path, prj)
    warning = _prj_warning(prj)
    df, messages = _warned(lambda: tpg.read_dataframe(path))
    assert (df.crs, df["id"].tolist(), messages) == (None, [0, 1, 2], [warning])
    assert [(p.x, p.y) for p in df.geometry] == [(10, 46), (11, 46), (12, 46)]
    layer, messages = _warned(lambda: terrio.open_vector(path)[0])
    assert messages == [warning]
    assert layer.to_geopandas().crs is None
    assert gpd.GeoDataFrame.from_arrow(layer.to_arrow()).crs is None


@pytest.mark.parametrize("prj", ['PROJCS["x"]', 'ENGCRS["x"]', "EPSG:999999", None])
def test_a_definition_pyproj_cannot_parse_gives_no_crs_and_a_warning(tmp_path, prj):
    pytest.importorskip("pyproj")
    # terrio keeps a `.prj` whose WKT it reads but that is no CRS (`PROJCS["x"]`), ones that have
    # the form of a definition (`ENGCRS["x"]`, a code no authority defines), and a GeoPackage's
    # definition (None here).
    if prj is None:
        definition = "not a coordinate reference system"
        path = _geopackage(tmp_path, definition)
    else:
        definition = prj
        path = _shapefile(tmp_path, prj)
    layer, messages = _warned(lambda: terrio.open_vector(path)[0])
    assert (layer.crs_wkt, messages) == (definition, [])
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        info = tpg.read_info(path)
    warned = [w for w in caught if issubclass(w.category, UserWarning)]
    assert info["crs"] is None
    assert [str(w.message) for w in warned] == [
        f"the layer's CRS definition cannot be parsed ('{definition}'): it is read without a CRS"
    ]
    # Issued at the caller's line.
    assert warned[0].filename == __file__


@pytest.mark.parametrize("prj", ["EPSG:999999", None])
def test_read_dataframe_reads_a_layer_whose_crs_pyproj_cannot_parse(tmp_path, prj):
    pytest.importorskip("geopandas")
    # A code no authority defines, in a `.prj`, and a GeoPackage's definition (None here).
    definition = prj or "not a coordinate reference system"
    path = _geopackage(tmp_path, definition) if prj is None else _shapefile(tmp_path, prj)
    df, messages = _warned(lambda: tpg.read_dataframe(path))
    assert (df.crs, df["id"].tolist()) == (None, [0, 1, 2])
    assert messages == [
        f"the layer's CRS definition cannot be parsed ('{definition}'): it is read without a CRS"
    ]


def test_without_pyproj_the_definition_is_given_as_it_is(tmp_path, monkeypatch):
    path = _geopackage(tmp_path, "not a coordinate reference system")
    monkeypatch.setitem(sys.modules, "pyproj", None)
    info, messages = _warned(lambda: tpg.read_info(path))
    assert (info["crs"], messages) == ("not a coordinate reference system", [])


def test_a_pyproj_that_fails_to_import_counts_as_no_pyproj(tmp_path, monkeypatch):
    # A broken installation can raise another error than ImportError.
    def fail(name):
        raise OSError("the library cannot be loaded")

    broken = types.ModuleType("pyproj")
    vars(broken)["__getattr__"] = fail
    path = _geopackage(tmp_path, "not a coordinate reference system")
    monkeypatch.setitem(sys.modules, "pyproj", broken)
    info, messages = _warned(lambda: tpg.read_info(path))
    assert (info["crs"], messages) == ("not a coordinate reference system", [])


def test_a_warning_quotes_control_characters_as_spaces(tmp_path):
    # In a `.prj` that holds no CRS definition: terrio's warning, in Python and on the command
    # line's standard error.
    path = _shapefile(tmp_path, CONTROL)
    warning = _prj_warning(CONTROL_QUOTED)
    info, messages = _warned(lambda: tpg.read_info(path))
    assert (info["crs"], messages) == (None, [warning])
    cmd = [sys.executable, "-m", "terrio", "info", path]
    done = subprocess.run(cmd, capture_output=True, text=True, cwd=tmp_path)
    assert (done.returncode, done.stderr) == (0, f"terrio: warning: {warning}\n")
    # In a definition pyproj cannot parse.
    pytest.importorskip("pyproj")
    path = _geopackage(tmp_path, CONTROL)
    warning = (
        f"the layer's CRS definition cannot be parsed ('{CONTROL_QUOTED}'): "
        "it is read without a CRS"
    )
    info, messages = _warned(lambda: tpg.read_info(path))
    assert (info["crs"], messages) == (None, [warning])
