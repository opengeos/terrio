"""Declared duplicate column names retain every positional value across vector readers."""

import json
import struct

import pytest

import terrio
import terrio.pyogrio as tpg

CASES = [
    (["count", "count", "total"], ["count", "count_2", "total"]),
    (["count", "count", "count_2", "total"], ["count", "count_3", "count_2", "total"]),
    (["count", "count", "count", "total"], ["count", "count_2", "count_3", "total"]),
]


def duplicate_file(folder, extension, names):
    """Write a valid positional dataset with deliberately repeated field declarations.

    Args:
        folder: Temporary directory.
        extension: MIF, Shapefile, FlatGeobuf or GeoParquet format.
        names: Declared source field names, including duplicates.

    Returns:
        tuple: Dataset path and source row numbers in its stored feature order.
    """
    path = folder / f"duplicate.{extension}"
    rows = [[10 * (row + 1) + column + 1 for column in range(len(names))] for row in range(3)]
    stored_rows = list(range(3))
    if extension == "mif":
        fields = "\n".join(f"  {name} Integer" for name in names)
        geometry = "\n".join(f"Point {10 + row} {46 + row}" for row in range(3))
        path.write_text(
            'Version 300\nCharset "Neutral"\nDelimiter ","\n'
            f"Columns {len(names)}\n{fields}\nData\n{geometry}\n"
        )
        path.with_suffix(".mid").write_text(
            "\n".join(",".join(map(str, row)) for row in rows) + "\n"
        )
    elif extension == "parquet":
        pa = pytest.importorskip("pyarrow")
        pq = pytest.importorskip("pyarrow.parquet")
        geometry = [struct.pack("<BIdd", 1, 1, 10.0 + row, 46.0 + row) for row in range(3)]
        arrays = [pa.array([row[column] for row in rows]) for column in range(len(names))]
        table = pa.Table.from_arrays(
            [*arrays, pa.array(geometry, pa.binary())], names=[*names, "geometry"]
        )
        geo = {
            "version": "1.1.0",
            "primary_column": "geometry",
            "columns": {"geometry": {"encoding": "WKB", "geometry_types": ["Point"]}},
        }
        pq.write_table(table.replace_schema_metadata({b"geo": json.dumps(geo).encode()}), path)
    else:
        original = [
            name if index == 0 or name not in names[:index] else f"coun{chr(87 + index)}"
            for index, name in enumerate(names)
        ]
        layer = terrio.Layer.from_geojson(
            {
                "type": "FeatureCollection",
                "features": [
                    {
                        "type": "Feature",
                        "properties": dict(zip(original, values, strict=True)),
                        "geometry": {"type": "Point", "coordinates": [10 + row, 46 + row]},
                    }
                    for row, values in enumerate(rows)
                ],
            }
        )
        assert list(layer.fields) == original
        layer.write(str(path))
        if extension == "shp":
            dbf_path = path.with_suffix(".dbf")
            data = bytearray(dbf_path.read_bytes())
            for index, name in enumerate(names):
                start = 32 + 32 * index
                assert data[start : start + 11].rstrip(b"\0").decode() == original[index]
                data[start : start + 11] = name.encode().ljust(11, b"\0")
            dbf_path.write_bytes(data)
        else:
            # FlatGeobuf's index sorts features spatially; renaming fields must keep that order.
            before_features = terrio.open_vector(str(path))[0].to_geojson()["features"]
            stored_rows = [feature["properties"]["count"] // 10 - 1 for feature in before_features]
            data = path.read_bytes()
            for before, after in zip(original, names, strict=True):
                if before != after:
                    assert len(before) == len(after) and data.count(before.encode()) == 1
                    data = data.replace(before.encode(), after.encode())
            path.write_bytes(data)
    return path, stored_rows


@pytest.mark.parametrize("extension", ["mif", "shp", "fgb", "parquet"])
@pytest.mark.parametrize("names,expected", CASES)
@pytest.mark.parametrize("surface", ["whole", "stream", "frame", "paged", "info"])
def test_duplicate_declared_columns_keep_their_values(
    tmp_path, extension, names, expected, surface
):
    """Keep column positions, generated-name collisions and reader metadata consistent.

    Args:
        tmp_path: Temporary dataset directory.
        extension: Input vector format.
        names: Repeated source declarations.
        expected: Distinct output field names.
        surface: Whole, streaming, dataframe, paged dataframe or metadata read.
    """
    path, stored_rows = duplicate_file(tmp_path, extension, names)
    if surface == "info":
        info = tpg.read_info(str(path))
        assert list(info["fields"]) == expected
        assert len(info["dtypes"]) == len(names)
        return
    selected_rows = stored_rows[1:2] if surface == "paged" else stored_rows
    if surface in ("frame", "paged"):
        pytest.importorskip("geopandas")
        options = {"skip_features": 1, "max_features": 1} if surface == "paged" else {}
        frame = tpg.read_dataframe(str(path), **options)
        assert list(frame.columns[:-1]) == expected
        rows = frame.drop(columns="geometry").to_dict("records")
        geometries = [[geometry.x, geometry.y] for geometry in frame.geometry]
    else:
        layer = (
            terrio.open_stream(str(path)).read_all()
            if surface == "stream"
            else terrio.open_vector(str(path))[0]
        )
        assert list(layer.fields) == expected
        features = layer.to_geojson()["features"]
        rows = [feature["properties"] for feature in features]
        geometries = [feature["geometry"]["coordinates"] for feature in features]
    assert rows == [
        dict(
            zip(
                expected, [10 * (row + 1) + column + 1 for column in range(len(names))], strict=True
            )
        )
        for row in selected_rows
    ]
    assert geometries == [[10 + row, 46 + row] for row in selected_rows]


@pytest.mark.parametrize("names,expected", CASES)
def test_arrow_duplicate_columns_keep_values(names, expected):
    """Keep Arrow imports aligned with the shared Parquet field decoder.

    Args:
        names: Repeated field declarations.
        expected: Distinct output names.
    """
    pa = pytest.importorskip("pyarrow")
    table = pa.Table.from_arrays(
        [pa.array([index + 1]) for index in range(len(names))], names=names
    )
    layer = terrio.Layer.from_arrow(table)
    assert list(layer.fields) == expected
    assert layer.to_geojson()["features"][0]["properties"] == dict(
        zip(expected, range(1, len(names) + 1), strict=True)
    )


@pytest.mark.parametrize("parquet", [False, True])
def test_flattened_struct_names_do_not_take_declared_names(tmp_path, parquet):
    """Resolve duplicate flattened members without taking a literal suffixed field.

    Args:
        tmp_path: Temporary dataset directory.
        parquet: Whether to read a Parquet file instead of an Arrow table.
    """
    pa = pytest.importorskip("pyarrow")
    struct_array = pa.StructArray.from_arrays(
        [pa.array([1]), pa.array([2])], names=["count", "count"]
    )
    table = pa.Table.from_arrays(
        [struct_array, pa.array([3]), pa.array([4])], names=["stats", "stats.count_2", "total"]
    )
    if parquet:
        pq = pytest.importorskip("pyarrow.parquet")
        path = tmp_path / "nested.parquet"
        pq.write_table(table, path)
        layer = terrio.open_vector(str(path))[0]
    else:
        layer = terrio.Layer.from_arrow(table)
    expected = {"stats.count": 1, "stats.count_3": 2, "stats.count_2": 3, "total": 4}
    assert list(layer.fields) == list(expected)
    assert layer.to_geojson()["features"][0]["properties"] == expected


def test_arrow_json_metadata_survives_duplicate_names():
    """Keep a duplicate JSON field's logical type and the following integer value."""
    pa = pytest.importorskip("pyarrow")
    schema = pa.schema(
        [
            pa.field("payload", pa.string(), metadata={b"ARROW:extension:name": b"arrow.json"}),
            pa.field("payload", pa.int64()),
        ]
    )
    table = pa.Table.from_arrays([pa.array(['{"ok":true}']), pa.array([9])], schema=schema)
    layer = terrio.Layer.from_arrow(table)
    assert layer.fields == {"payload": "Json", "payload_2": "Integer"}
    assert layer.to_geojson()["features"][0]["properties"] == {
        "payload": '{"ok":true}',
        "payload_2": 9,
    }
