"""GeoParquet Arrow consumers share optional validation of stored CRS metadata."""

import json
import sys
import warnings

import pytest

import terrio
import terrio.pyogrio as tpg

pa = pytest.importorskip("pyarrow")
pq = pytest.importorskip("pyarrow.parquet")
gpd = pytest.importorskip("geopandas")
shapely = pytest.importorskip("shapely")
pyproj = pytest.importorskip("pyproj")

INVALID = ['PROJCS["x"]', {"type": "ProjectedCRS", "name": "x"}, "not a CRS"]


def write_points(path, definition):
    """Write nested attributes and four WKB points with a chosen stored CRS.

    Args:
        path: Output GeoParquet path.
        definition: JSON-compatible CRS declaration, including null.
    """
    geo = {
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {
            "geometry": {"encoding": "WKB", "geometry_types": ["Point"], "crs": definition}
        },
    }
    table = pa.table(
        {
            "id": [0, 1, 2, 3],
            "details": [{"label": "point", "values": [i, i + 1]} for i in range(4)],
            "geometry": pa.array([shapely.to_wkb(shapely.Point(10 + i, 46)) for i in range(4)]),
        }
    )
    pq.write_table(
        table.replace_schema_metadata({b"geo": json.dumps(geo).encode()}), path, row_group_size=2
    )


@pytest.mark.parametrize("definition", INVALID + [None, pyproj.CRS(4326).to_json_dict()])
@pytest.mark.parametrize(
    "surface", ["layer", "layer_arrow", "dataframe", "dataframe_arrow", "stream"]
)
def test_crs_validation_on_all_dataframe_paths(tmp_path, definition, surface):
    """Keep points and attributes readable when consumer CRS parsing rejects metadata.

    Args:
        tmp_path: Temporary file directory.
        definition: Invalid, absent or valid GeoParquet CRS.
        surface: Public Python export entry point.
    """
    path = tmp_path / "points.parquet"
    write_points(path, definition)
    layer = terrio.open_vector(str(path))[0]
    original = layer.crs
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        if surface == "layer":
            result = layer.to_geopandas()
        elif surface == "layer_arrow":
            result = gpd.GeoDataFrame.from_arrow(layer)
        elif surface == "dataframe":
            result = tpg.read_dataframe(path)
        elif surface == "dataframe_arrow":
            result = tpg.read_dataframe(path, use_arrow=True)
        else:
            result = gpd.GeoDataFrame.from_arrow(terrio.open_stream(str(path), batch_size=2))
    assert list(result["id"]) == [0, 1, 2, 3]
    assert [point.x for point in result.geometry] == [10, 11, 12, 13]
    assert layer.crs == original
    if definition in INVALID or definition is None:
        assert result.crs is None
    else:
        assert result.crs == pyproj.CRS(4326)
    if definition in INVALID and (original or surface in ("stream", "dataframe_arrow")):
        assert sum("CRS definition cannot be parsed" in str(w.message) for w in caught) == 1


def test_stream_keeps_nested_columns_and_batching(tmp_path):
    """Replace rejected CRS metadata without flattening or rebuilding attribute arrays.

    Args:
        tmp_path: Temporary file directory.
    """
    path = tmp_path / "points.parquet"
    write_points(path, INVALID[0])
    with pytest.warns(UserWarning, match="CRS definition cannot be parsed"):
        reader = pa.RecordBatchReader.from_stream(terrio.open_stream(str(path), batch_size=2))
    metadata = json.loads(reader.schema.field("geometry").metadata[b"ARROW:extension:metadata"])
    assert "crs" not in metadata
    assert pa.types.is_struct(reader.schema.field("details").type)
    batches = list(reader)
    assert [len(batch) for batch in batches] == [2, 2]
    table = pa.Table.from_batches(batches)
    assert table["details"].to_pylist() == [
        {"label": "point", "values": [i, i + 1]} for i in range(4)
    ]
    assert (
        json.loads(pq.read_schema(path).metadata[b"geo"])["columns"]["geometry"]["crs"]
        == INVALID[0]
    )


def test_optional_parser_unavailable_retains_metadata(tmp_path, monkeypatch):
    """Keep stored CRS metadata when the optional consumer parser cannot be imported.

    Args:
        tmp_path: Temporary file directory.
        monkeypatch: Temporarily disable the pyproj import.
    """
    path = tmp_path / "points.parquet"
    write_points(path, INVALID[0])
    monkeypatch.setitem(sys.modules, "pyproj", None)
    table = pa.table(terrio.open_stream(str(path)))
    metadata = json.loads(table.schema.field("geometry").metadata[b"ARROW:extension:metadata"])
    assert metadata["crs"] == INVALID[0]
