"""Arrow export and import of vector layers (the Arrow PyCapsule interface), GeoPackage
spatial filters and updates."""

import json
import os
import sqlite3

import pytest

import terrio

POLYGONS = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"name": "west", "value": 3, "area": 1.5, "ok": True},
            "geometry": {
                "type": "Polygon",
                "coordinates": [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]]],
            },
        },
        {
            "type": "Feature",
            "properties": {"name": "east", "value": 7, "area": None, "ok": False},
            "geometry": {
                "type": "MultiPolygon",
                "coordinates": [[[[20, 0], [30, 0], [30, 10], [20, 10], [20, 0]]]],
            },
        },
        {
            "type": "Feature",
            "properties": {"name": "none", "value": None, "area": 2.0, "ok": None},
            "geometry": None,
        },
    ],
}


@pytest.fixture
def layer():
    return terrio.Layer.from_geojson(POLYGONS, name="squares")


def geoarrow(field):
    """The GeoArrow extension name and metadata of a pyarrow field."""
    meta = field.metadata or {}
    return meta[b"ARROW:extension:name"].decode(), json.loads(meta[b"ARROW:extension:metadata"])


@pytest.fixture
def pa():
    pa = pytest.importorskip("pyarrow")
    # Keep GeoArrow columns as their storage types with their metadata, even when an extension
    # type is registered (Arrow C++ registers geoarrow.wkb).
    for name in ["wkb", "point", "polygon", "multipolygon", "linestring"]:
        try:
            pa.unregister_extension_type(f"geoarrow.{name}")
        except (KeyError, pa.ArrowKeyError):
            pass
    return pa


def test_arrow_table_without_pyarrow(layer):
    t = layer.to_arrow()
    assert isinstance(t, terrio.ArrowTable)
    assert len(t) == t.num_rows == 3
    assert t.column_names == ["name", "value", "area", "ok", "geometry"]
    assert t.num_columns == 5 and t.num_batches == 1
    assert "geometry" in repr(t)
    assert type(t.__arrow_c_stream__()).__name__ == "PyCapsule"
    assert type(t.__arrow_c_schema__()).__name__ == "PyCapsule"


def test_to_pyarrow_wkb(layer, pa):
    shapely = pytest.importorskip("shapely")
    t = pa.table(layer.to_arrow())
    assert t.schema.field("value").type == pa.int64()
    assert t.schema.field("area").type == pa.float64()
    assert t.schema.field("ok").type == pa.bool_()
    assert t.column("name").to_pylist() == ["west", "east", "none"]
    assert t.column("value").to_pylist() == [3, 7, None]
    name, meta = geoarrow(t.schema.field("geometry"))
    assert name == "geoarrow.wkb"
    assert meta["crs_type"] == "projjson"
    assert meta["crs"]["id"] == {"authority": "EPSG", "code": 4326}
    geoms = t.column("geometry").to_pylist()
    assert geoms[2] is None
    assert shapely.from_wkb(geoms[0]).equals(shapely.box(0, 0, 10, 10))
    # The layer itself exports a stream.
    assert pa.table(layer).equals(t)


@pytest.mark.parametrize("coordinates", ["separated", "interleaved"])
def test_native_round_trip(layer, pa, coordinates):
    t = pa.table(layer.to_arrow(geometry_encoding="geoarrow", coordinates=coordinates))
    name, meta = geoarrow(t.schema.field("geometry"))
    # A polygon and a multipolygon make a multipolygon column.
    assert name == "geoarrow.multipolygon"
    assert meta["crs"]["id"]["code"] == 4326
    back = terrio.from_arrow(t, name="back")
    assert back.name == "back" and len(back) == 3
    assert back.crs == "EPSG:4326"
    assert back.geometry_type == "MultiPolygon"
    assert back.columns() == layer.columns()
    assert back.to_geojson()["features"][0]["geometry"]["type"] == "MultiPolygon"
    assert back.to_geojson()["features"][2]["geometry"] is None


def test_native_needs_one_type(pa):
    mixed = terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {},
                    "geometry": {"type": "Point", "coordinates": [0, 0]},
                },
                {
                    "type": "Feature",
                    "properties": {},
                    "geometry": {"type": "LineString", "coordinates": [[0, 0], [1, 1]]},
                },
            ],
        }
    )
    with pytest.raises(terrio.UnsupportedError, match="use WKB"):
        mixed.to_arrow(geometry_encoding="geoarrow")
    assert len(pa.table(mixed.to_arrow())) == 2
    with pytest.raises(terrio.InvalidArgumentError):
        mixed.to_arrow(geometry_encoding="nope")


def test_from_arrow_inputs(layer, pa):
    t = pa.table(layer.to_arrow(fid_column="fid"))
    assert t.column("fid").to_pylist() == [0, 1, 2]
    # A record batch, through __arrow_c_array__ alone.
    batch = t.to_batches()[0]

    class ArrayOnly:
        def __arrow_c_array__(self, requested_schema=None):
            return batch.__arrow_c_array__(requested_schema)

    for data in (t, batch, ArrayOnly(), t.to_reader(), layer.to_arrow()):
        back = terrio.from_arrow(data)
        assert len(back) == 3
    assert terrio.Layer.from_arrow(t, crs="EPSG:3857").crs == "EPSG:3857"
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.from_arrow(42)


def test_write_vector_takes_arrow(layer, pa, tmp_path):
    out = tmp_path / "a.gpkg"
    terrio.write_vector(pa.table(layer.to_arrow()), str(out), layer_name="squares")
    back = terrio.open_vector(str(out))["squares"]
    assert len(back) == 3 and back.crs == "EPSG:4326"


def test_geopandas(layer, pa):
    gpd = pytest.importorskip("geopandas")
    gdf = gpd.GeoDataFrame.from_arrow(layer.to_arrow())
    assert list(gdf["name"]) == ["west", "east", "none"]
    assert gdf.crs.to_epsg() == 4326
    assert gdf.geometry.iloc[2] is None
    gdf_native = gpd.GeoDataFrame.from_arrow(layer.to_arrow(geometry_encoding="geoarrow"))
    assert gdf_native.geometry.iloc[0].geom_type == "MultiPolygon"
    for encoding in ("WKB", "geoarrow"):
        back = terrio.from_arrow(gdf.to_arrow(geometry_encoding=encoding))
        assert back.crs == "EPSG:4326"
        assert back.columns()["name"] == ["west", "east", "none"]
    # A GeoDataFrame itself goes through its to_arrow().
    assert len(terrio.from_arrow(gdf)) == 3


def test_polars(layer):
    pl = pytest.importorskip("polars")
    pytest.importorskip("pyarrow")
    df = pl.DataFrame(layer.to_arrow())
    assert df["name"].to_list() == ["west", "east", "none"]
    back = terrio.from_arrow(df)
    assert back.columns()["value"] == [3, 7, None]
    assert back.to_wkb() == layer.to_wkb()


def test_open_vector_bbox_and_gpkg_append(layer, tmp_path):
    path = str(tmp_path / "s.gpkg")
    layer.write(path)
    # The R-tree finds the features whose box intersects the rectangle.
    hits = terrio.open_vector(path, bbox=(25, 5, 40, 20))["squares"]
    assert hits.columns()["name"] == ["east"]
    assert len(terrio.open_vector(path, bbox=(100, 100, 101, 101))["squares"]) == 0
    # Other formats are filtered after reading.
    geojson = str(tmp_path / "s.geojson")
    layer.write(geojson)
    assert terrio.open_vector(geojson, bbox=(25, 5, 40, 20))[0].columns()["name"] == ["east"]
    # Appending to the layer of a GeoPackage, and adding another, in place (the same file) where
    # SQLite's file locks are taken (Unix); elsewhere the file is rewritten.
    inode = os.stat(path).st_ino
    terrio.write_vector(layer, path, append=True)
    other = layer.to_crs("EPSG:3857")
    other.name = "other"
    terrio.write_vector(other, path, append=True)
    if os.name == "posix":
        assert os.stat(path).st_ino == inode
    assert not os.path.exists(path + "-journal")
    con = sqlite3.connect(path)
    assert con.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
    assert con.execute("SELECT rtreecheck('rtree_squares_geom')").fetchone()[0] == "ok"
    con.close()
    ds = terrio.open_vector(path)
    assert ds.layer_names == ["squares", "other"]
    assert len(ds["squares"]) == 6 and len(ds["other"]) == 3
    hits = terrio.open_vector(path, layers=["squares"], bbox=(25, 5, 40, 20))["squares"]
    assert hits.columns()["name"] == ["east", "east"]
    # Overwriting replaces it.
    terrio.write_vector(layer, path, overwrite=True)
    assert len(terrio.open_vector(path)["squares"]) == 3


def test_real_layers_against_gdal(data_dir, ogr, pa):
    shapely = pytest.importorskip("shapely")
    for name in ["countries.gpkg", "UScounties.fgb"]:
        layer = terrio.open_vector(str(data_dir / name))[0]
        ds = ogr.Open(str(data_dir / name))
        lyr = ds.GetLayer(0)
        stream = lyr.GetArrowStreamAsPyArrow(["INCLUDE_FID=NO"])
        ref = pa.Table.from_batches([pa.RecordBatch.from_struct_array(a) for a in stream])
        got = pa.table(layer.to_arrow())
        assert got.num_rows == ref.num_rows
        geom = [f.name for f in ref.schema if f.name in ("geom", "geometry", "wkb_geometry")][0]
        a = shapely.from_wkb(ref.column(geom).to_pylist())
        b = shapely.from_wkb(got.column("geometry").to_pylist())
        assert all(shapely.equals_exact(a, b, tolerance=0))

        def norm(values):
            return [
                float(v) if isinstance(v, (int, float)) and not isinstance(v, bool) else v
                for v in values
            ]

        for column in ref.column_names:
            if column != geom:
                assert norm(ref.column(column).to_pylist()) == norm(
                    got.column(column).to_pylist()
                ), column
