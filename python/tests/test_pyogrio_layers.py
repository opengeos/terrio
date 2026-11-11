"""`terrio.pyogrio` and sources with several layers.

`list_layers` lists every layer, each with the geometry type `read_info` gives it. `read_dataframe`
and `read_info` read the layer `layer` names, or numbers from 0, and `skip_features`,
`max_features`, `bbox`, `columns` and `fid_as_index` apply to that layer alone. A name or number
that is no layer raises `DataLayerError`, and a `layer` that is neither a str nor an int
`ValueError`. Without `layer`, the first layer is read, with a warning that names the others.

The sources are those terrio reads with several layers: every driver terrio writes is given two
layers, and those `terrio.list_layers` reads back with more than one are kept, so the set comes
from terrio rather than from a list here. OSM (written here) and HEC-RAS (a fixture of the
repository) are read only. A directory and a zip archive of two shapefiles are terrio.pyogrio's
own sources of several layers, a layer per shapefile.

Only the test of `read_dataframe`'s arguments needs geopandas.
"""

import functools
import http.server
import io
import json
import os
import re
import shutil
import threading
import warnings
import zipfile
from pathlib import Path

import pytest

np = pytest.importorskip("numpy")
pytest.importorskip("shapely")

import terrio  # noqa: E402
import terrio.pyogrio as tpg  # noqa: E402
import terrio.rio as rio  # noqa: E402
from terrio import _terrio  # noqa: E402
from terrio.pyogrio.errors import DataLayerError, DataSourceError  # noqa: E402

HECRAS = Path(__file__).resolve().parents[2] / "crates/terrio/tests/data/hecras/pipes.h5"
DRIVERS = {d["name"]: d for d in terrio.drivers() if d["kind"] == "vector"}
WRITTEN = [name for name, d in DRIVERS.items() if d["write"]]
SHAPEFILES = ["directory of shapefiles", "zip of shapefiles"]
KINDS = [*WRITTEN, "GPX trip", "OSM", "HECRAS", *SHAPEFILES]

# Two waypoints, a route of three points and a track of two segments.
TRIP_GPX = """<?xml version="1.0"?>
<gpx version="1.1" creator="terrio-tests" xmlns="http://www.topografix.com/GPX/1/1">
<wpt lat="46.0" lon="7.0"><name>w0</name></wpt>
<wpt lat="46.1" lon="7.1"><name>w1</name></wpt>
<rte><name>r0</name><rtept lat="46.0" lon="7.0"/><rtept lat="46.2" lon="7.2"/>\
<rtept lat="46.4" lon="7.1"/></rte>
<trk><name>t0</name><trkseg><trkpt lat="46.0" lon="7.0"/><trkpt lat="46.1" lon="7.1"/></trkseg>
<trkseg><trkpt lat="46.3" lon="7.3"/><trkpt lat="46.4" lon="7.4"/></trkseg></trk>
</gpx>
"""
# A node with a tag (a point) and a way (a line).
OSM_XML = """<?xml version="1.0"?><osm version="0.6">
<node id="1" lat="0" lon="0"><tag k="amenity" v="cafe"/></node>
<node id="2" lat="0" lon="1"/><node id="3" lat="1" lon="1"/>
<way id="10"><nd ref="1"/><nd ref="2"/><nd ref="3"/><tag k="highway" v="residential"/></way>
</osm>
"""


def _points(directory, name, n, x0):
    """A GeoJSON file of `n` points `name0`, `name1`... from `(x0, 0)`."""
    path = directory / f"{name}.geojson"
    features = [
        {
            "type": "Feature",
            "properties": {"name": f"{name}{i}"},
            "geometry": {"type": "Point", "coordinates": [x0 + i, 0.5 * i]},
        }
        for i in range(n)
    ]
    path.write_text(json.dumps({"type": "FeatureCollection", "features": features}))
    return str(path)


@pytest.fixture(scope="module")
def sources(tmp_path_factory):
    """Each source, by kind: None for a driver terrio writes one layer with."""
    d = tmp_path_factory.mktemp("layers")
    gpkg = str(d / "two.gpkg")
    terrio.convert(_points(d, "five", 5, 0), gpkg, nln="five")
    terrio.convert(_points(d, "three", 3, 10), gpkg, nln="three", append=True)
    two = terrio.open_vector(gpkg)
    out: dict[str, str | None] = {}
    for name in WRITTEN:
        path = str(d / f"two-{name.replace(' ', '_')}.{DRIVERS[name]['extensions'][0]}")
        try:
            terrio.write_vector(two, path, driver=name)
        except terrio.errors.InvalidArgumentError:
            out[name] = None  # "the ... format holds one layer"
            continue
        out[name] = path if len(terrio.list_layers(path)) > 1 else None
    (d / "trip.gpx").write_text(TRIP_GPX)
    out["GPX trip"] = str(d / "trip.gpx")
    (d / "x.osm").write_text(OSM_XML)
    out["OSM"] = str(d / "x.osm")
    out["HECRAS"] = str(HECRAS) if "HECRAS" in DRIVERS and HECRAS.is_file() else None
    shapes = d / "shapes"
    shapes.mkdir()
    terrio.convert(_points(d, "aa", 2, 0), str(shapes / "aa.shp"))
    terrio.convert(_points(d, "bb", 3, 5), str(shapes / "bb.shp"))
    out["directory of shapefiles"] = str(shapes)
    archive = d / "shapes.zip"
    with zipfile.ZipFile(archive, "w") as z:
        for f in sorted(shapes.iterdir()):
            z.write(f, f.name)
    out["zip of shapefiles"] = str(archive)
    return out


def _source(sources, kind):
    path = sources[kind]
    if path is None:
        pytest.skip(f"{kind}: one layer, or not built")
    if os.path.isdir(path) and kind not in SHAPEFILES:
        # An MVT tile directory: terrio.pyogrio opens a directory as a directory of shapefiles.
        pytest.skip(f"{kind}: a directory")
    return path


def _names(kind, path):
    """The layer names, as terrio reads them (the shapefiles of a directory or an archive)."""
    return ["aa", "bb"] if kind in SHAPEFILES else terrio.list_layers(path)


def _bounds(bounds):
    return None if bounds is None else tuple(float(x) for x in bounds)


def _summary(info):
    """A layer's name, feature count, bounds and fields, from `read_info`."""
    fields = [str(f) for f in info["fields"]]
    return info["layer_name"], info["features"], _bounds(info["total_bounds"]), fields


def _terrios(kind, path, name):
    """The same summary of the layer `name` read by terrio itself."""
    if kind in SHAPEFILES:
        # The archive holds the shapefiles of the directory.
        directory = path if kind == "directory of shapefiles" else Path(path).with_suffix("")
        layer = terrio.open_vector(os.path.join(directory, f"{name}.shp")).layers[0]
    else:
        layer = terrio.open_vector(path, layers=[name]).layers[0]
    return name, len(layer.to_wkb()), _bounds(layer.bounds), list(layer.fields)


def _quiet(call, *args, **kwargs):
    """A read whose warnings (the default layer's, a CRS's) do not matter here."""
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        return call(*args, **kwargs)


def _first_layer_warning(path, names):
    others = ", ".join(f"'{n}'" for n in names[1:])
    return re.escape(
        f"More than one layer found in '{os.path.basename(path)}': '{names[0]}' (default), "
        f"{others}. Specify layer parameter to avoid this warning."
    )


def test_the_sources_come_from_terrio(sources):
    several = {kind for kind, path in sources.items() if path is not None}
    assert {"GPKG", "KML", "GPX"} <= several, several


@pytest.mark.parametrize("kind", KINDS)
def test_list_layers_lists_every_layer(sources, kind):
    path = _source(sources, kind)
    rows = tpg.list_layers(path)
    assert rows.shape == (len(_names(kind, path)), 2)
    assert rows[:, 0].tolist() == _names(kind, path)
    for name, geometry_type in rows:
        assert geometry_type == _quiet(tpg.read_info, path, layer=name)["geometry_type"], name


@pytest.mark.parametrize("kind", KINDS)
def test_each_layer_is_read_by_name_and_by_number(sources, kind):
    path = _source(sources, kind)
    for i, name in enumerate(_names(kind, path)):
        by_name = _summary(_quiet(tpg.read_info, path, layer=name))
        assert by_name == _summary(_quiet(tpg.read_info, path, layer=i)), name
        assert by_name == _terrios(kind, path, name)
        if kind not in SHAPEFILES:
            # terrio is asked for that layer alone, and gives no other.
            assert _terrio.pyogrio_read(path, layer=name)["layers"] == [name]


@pytest.mark.parametrize("kind", KINDS)
def test_a_layer_that_is_not_there_is_refused(sources, kind):
    path = _source(sources, kind)
    count = len(_names(kind, path))
    # A dataset of several layers does not answer to its file's stem, as one of one layer does.
    for layer in ("nope", "", count, -1, Path(path).stem):
        with pytest.raises(DataLayerError, match=re.escape(f"Layer '{layer}' could not be opened")):
            tpg.read_info(path, layer=layer)
    for layer in (1.0, b"aa", np.int64(0)):
        with pytest.raises(ValueError, match="'layer' parameter must be a str or int, got"):
            tpg.read_info(path, layer=layer)


@pytest.mark.parametrize("kind", KINDS)
def test_without_a_layer_the_first_is_read_with_a_warning(sources, kind):
    path = _source(sources, kind)
    names = _names(kind, path)
    with pytest.warns(UserWarning, match=_first_layer_warning(path, names)):
        first = tpg.read_info(path)
    assert _summary(first) == _summary(_quiet(tpg.read_info, path, layer=0))


@pytest.mark.parametrize("kind", ["GPKG", "KML", "GPX trip", *SHAPEFILES])
def test_read_dataframe_applies_its_arguments_to_the_layer(sources, kind):
    """By name and by number, a range, a bbox with a range, columns and FIDs; a layer that is not
    there; the first layer, with the warning, at the call."""
    pytest.importorskip("geopandas")
    pd = pytest.importorskip("pandas")
    shapely = pytest.importorskip("shapely")
    path = _source(sources, kind)
    names = _names(kind, path)
    frames = {}
    for i, name in enumerate(names):
        frames[name] = _quiet(tpg.read_dataframe, path, layer=name, fid_as_index=True)
        by_number = _quiet(tpg.read_dataframe, path, layer=i, fid_as_index=True)
        pd.testing.assert_frame_equal(frames[name], by_number)
        assert len(frames[name]) == _terrios(kind, path, name)[1], name
    # The layer with the most features.
    name = max(names, key=lambda n: len(frames[n]))
    full = frames[name]
    ranged = _quiet(
        tpg.read_dataframe, path, layer=name, skip_features=1, max_features=1, fid_as_index=True
    )
    pd.testing.assert_frame_equal(ranged, full.iloc[1:2])
    fields = [c for c in full.columns if c != "geometry"]
    some = _quiet(tpg.read_dataframe, path, layer=name, columns=fields[:1])
    assert [c for c in some.columns if c != "geometry"] == fields[:1]
    # A box around the second feature and those after it, then the first of them skipped.
    x0, y0 = full.geometry.iloc[1].bounds[:2]
    x1, y1 = full.total_bounds[2:]
    box = (x0 - 1e-9, y0 - 1e-9, x1 + 1e-9, y1 + 1e-9)
    inside = full.index[full.geometry.intersects(shapely.box(*box))]
    boxed = _quiet(
        tpg.read_dataframe, path, layer=name, bbox=box, skip_features=1, fid_as_index=True
    )
    assert boxed.index.tolist() == inside[1:].tolist()
    with pytest.raises(DataLayerError, match="Layer 'nope' could not be opened"):
        tpg.read_dataframe(path, layer="nope")
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        first = tpg.read_dataframe(path)
    pd.testing.assert_frame_equal(first, _quiet(tpg.read_dataframe, path, layer=0))
    layers = [w for w in caught if "More than one layer" in str(w.message)]
    assert [w.filename for w in layers] == [__file__]


def test_a_single_layer_is_read_without_a_warning(tmp_path):
    path = _points(tmp_path, "one", 2, 0)
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        assert tpg.read_info(path)["features"] == 2
        assert tpg.read_info(path, layer="one")["features"] == 2
        assert tpg.list_layers(path).tolist() == [["one", "Point"]]


def test_a_layer_of_its_own_name_also_answers_to_the_files_stem(tmp_path):
    # A GeoJSON document's `name` member names its layer; the file's stem still reads it.
    document = json.loads(Path(_points(tmp_path, "one", 2, 0)).read_text())
    path = tmp_path / "file.geojson"
    path.write_text(json.dumps({**document, "name": "own"}))
    assert tpg.list_layers(str(path)).tolist() == [["own", "Point"]]
    for layer in ("own", "OWN", "file", 0):
        assert tpg.read_info(str(path), layer=layer)["layer_name"] == "own", layer


def test_the_warnings_point_at_the_call(sources, tmp_path):
    path = _source(sources, "GPKG")
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        tpg.read_info(path)
    layers = [w for w in caught if "More than one layer" in str(w.message)]
    assert [w.filename for w in layers] == [__file__]
    # The warning of a CRS pyproj cannot parse, from list_layers as from read_info.
    pytest.importorskip("pyproj")
    shp = tmp_path / "x.shp"
    terrio.convert(_points(tmp_path, "x", 1, 0), str(shp))
    shp.with_suffix(".prj").write_text('PROJCS["x"]')
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        tpg.list_layers(str(shp))
    crs = [w for w in caught if "cannot be parsed" in str(w.message)]
    assert [w.filename for w in crs] == [__file__]


def test_a_name_matches_whatever_its_case(sources):
    # A layer is looked up by its name as written, then by its name in any case.
    for kind, layer, name in [
        ("GPKG", "THREE", "three"),
        ("KML", "Three", "three"),
        ("GPX trip", "ROUTES", "routes"),
    ]:
        path = _source(sources, kind)
        assert _quiet(tpg.read_info, path, layer=layer)["layer_name"] == name


def test_a_gpx_file_has_a_layer_of_each_kind(sources):
    # TRIP_GPX: 2 waypoints, a route of 3 points, and a track of 2 segments of 2 points each.
    path = _source(sources, "GPX trip")
    assert tpg.list_layers(path).tolist() == [
        ["waypoints", "Point"],
        ["routes", "LineString"],
        ["tracks", "MultiLineString"],
        ["route_points", "Point"],
        ["track_points", "Point"],
    ]
    infos = [_quiet(tpg.read_info, path, layer=i) for i in range(5)]
    assert [info["features"] for info in infos] == [2, 1, 1, 3, 4]
    assert _bounds(infos[4]["total_bounds"]) == (7.0, 46.0, 7.4, 46.4)


@pytest.mark.parametrize("kind", ["GPKG", "KML", "GPX trip", "zip of shapefiles"])
def test_a_layer_of_bytes(sources, kind):
    """Bytes of a zip archive: a GeoPackage, a KML or a GPX file alone in it, or shapefiles."""
    path = _source(sources, kind)
    if kind == "zip of shapefiles":
        data = Path(path).read_bytes()
    else:
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, "w") as z:
            z.write(path, os.path.basename(path))
        data = buffer.getvalue()
    names = _names(kind, path)
    assert tpg.list_layers(io.BytesIO(data))[:, 0].tolist() == names
    for i, name in enumerate(names):
        from_bytes = _summary(_quiet(tpg.read_info, io.BytesIO(data), layer=i))
        assert from_bytes == _summary(_quiet(tpg.read_info, path, layer=name)), name


def _item(href):
    """A STAC item whose asset `data` is the GeoPackage `href`."""
    return {
        "type": "Feature",
        "stac_version": "1.0.0",
        "id": "two",
        "properties": {"datetime": "2026-01-01T00:00:00Z"},
        "geometry": {"type": "Point", "coordinates": [0.0, 0.0]},
        "links": [],
        "assets": {"data": {"href": href, "type": "application/geopackage+sqlite3"}},
    }


def test_the_layers_are_listed_under_the_callers_options_and_asset_policy(sources, tmp_path):
    """A layer named or numbered has the dataset's layers listed first: under the options of
    `set_gdal_config_options` and of `terrio.rio.Env`, as the read is, and under the STAC asset
    read policy. A layer that is not there is refused by the listing alone, before any read: a
    listing outside the caller's options would raise `DataLayerError` for it."""
    gpkg = _source(sources, "GPKG")
    calls = (
        lambda: tpg.read_info(gpkg, layer=1),
        lambda: tpg.read_info(gpkg, layer="nope"),
        lambda: tpg.list_layers(gpkg),
    )
    tpg.set_gdal_config_options({"TERRIO_ALLOWED_DRIVERS": "GeoJSON"})
    try:
        for call in calls:
            with pytest.raises(DataSourceError, match="GPKG driver is not allowed"):
                call()
    finally:
        tpg.set_gdal_config_options({"TERRIO_ALLOWED_DRIVERS": None})
    with rio.Env(TERRIO_ALLOWED_DRIVERS="GeoJSON"):
        for call in calls:
            with pytest.raises(DataSourceError, match="GPKG driver is not allowed"):
                call()
    # The asset of a local item is read; a remote item naming that local file is refused. The
    # local item names its asset relative to itself, a form that reads the same on every platform.
    shutil.copyfile(gpkg, tmp_path / "two.gpkg")
    (tmp_path / "item.json").write_text(json.dumps(_item("./two.gpkg")))
    local = f'STAC:"{tmp_path / "item.json"}":data'
    (tmp_path / "remote.json").write_text(json.dumps(_item((tmp_path / "two.gpkg").as_uri())))
    assert tpg.list_layers(local)[:, 0].tolist() == ["five", "three"]
    assert _quiet(tpg.read_info, local, layer="three")["features"] == 3

    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *args):
            """Log nothing."""

    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), functools.partial(Quiet, directory=str(tmp_path))
    )
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        remote = f'STAC:"http://127.0.0.1:{server.server_address[1]}/remote.json":data'
        for call in (
            lambda: tpg.read_info(remote, layer="three"),
            lambda: tpg.read_info(remote, layer="nope"),
            lambda: tpg.list_layers(remote),
        ):
            with pytest.raises(DataSourceError, match="names local files"):
                call()
    finally:
        server.shutdown()
        server.server_close()
