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


@pytest.mark.parametrize("names,expected", CASES)
@pytest.mark.parametrize("surface", ["stream", "arrow", "frame"])
@pytest.mark.parametrize("select_second", [False, True])
@pytest.mark.parametrize("filtered", [False, True])
@pytest.mark.parametrize("covering", [False, True])
def test_geoparquet_arrow_exports_keep_each_physical_column(
    tmp_path, names, expected, surface, select_second, filtered, covering
):
    """Preserve positional values through Arrow selection and both bbox filter paths.

    Args:
        tmp_path: Temporary dataset directory.
        names: Repeated physical column names.
        expected: Distinct public attribute names.
        surface: Arrow stream, pyogrio Arrow read or dataframe read.
        select_second: Whether to request only the renamed second column.
        filtered: Whether to keep only the middle feature by bounding box.
        covering: Whether a Zstandard file supplies a bounding-box covering struct.
    """
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    path, _ = duplicate_file(tmp_path, "parquet", names)
    table = pq.ParquetFile(path).read()
    if covering:
        bounds = pa.StructArray.from_arrays(
            [pa.array([10.0, 11.0, 12.0]), pa.array([46.0, 47.0, 48.0])] * 2,
            names=["xmin", "ymin", "xmax", "ymax"],
        )
        table = table.add_column(0, "bbox", bounds)
        geo = json.loads(table.schema.metadata[b"geo"])
        geo["columns"]["geometry"]["covering"] = {
            "bbox": {key: ["bbox", key] for key in ["xmin", "ymin", "xmax", "ymax"]}
        }
        table = table.replace_schema_metadata({b"geo": json.dumps(geo).encode()})
    pq.write_table(table, path, compression="zstd" if covering else "snappy", row_group_size=2)
    selected = [expected[1]] if select_second else expected
    columns = selected if select_second else None
    bbox = (10.5, 46.5, 11.5, 47.5) if filtered else None
    rows = [1] if filtered else [0, 1, 2]
    if surface == "frame":
        pytest.importorskip("geopandas")
        frame = tpg.read_dataframe(path, use_arrow=True, columns=columns, bbox=bbox)
        assert list(frame.columns) == [*selected, "geometry"]
        actual = frame.drop(columns="geometry").to_dict("records")
        assert [(g.x, g.y) for g in frame.geometry] == [(10 + r, 46 + r) for r in rows]
    else:
        if surface == "stream":
            stream = terrio.open_stream(str(path), columns=columns, bbox=bbox, batch_size=1)
            assert list(stream.fields) == selected
            table = pa.table(stream)
        else:
            meta, table = tpg.read_arrow(path, columns=columns, bbox=bbox)
            assert list(meta["fields"]) == selected
        assert table.column_names == [*selected, "geometry"]
        assert table["geometry"].to_pylist() == [
            struct.pack("<BIdd", 1, 1, 10.0 + r, 46.0 + r) for r in rows
        ]
        actual = table.drop(["geometry"]).to_pylist()
    assert actual == [
        {name: 10 * (row + 1) + expected.index(name) + 1 for name in selected} for row in rows
    ]


@pytest.mark.parametrize(
    "names,expected",
    [
        (["count", "count", "COUNT_2"], ["count", "count_2", "COUNT_2"]),
        (["", "field_1", ""], ["field_1_2", "field_1", "field_3"]),
    ],
)
def test_geoparquet_resolved_names_select_exact_physical_columns(tmp_path, names, expected):
    """Select case-distinct generated names and collision-safe empty-name fallbacks.

    Args:
        tmp_path: Directory for the positional Parquet fixture.
        names: Declared source names.
        expected: Stable resolved names.
    """
    pa = pytest.importorskip("pyarrow")
    path, _ = duplicate_file(tmp_path, "parquet", names)
    assert pa.table(terrio.open_stream(str(path))).column_names == [*expected, "geometry"]
    for column, name in enumerate(expected):
        _, table = tpg.read_arrow(path, columns=[name])
        assert table.column_names == [name, "geometry"]
        assert table[name].to_pylist() == [11 + column, 21 + column, 31 + column]


def test_geoparquet_covering_filters_nonzero_polygon_extents(tmp_path):
    """Check both sides of covering boxes using actual non-degenerate polygons.

    Args:
        tmp_path: Directory for the GeoParquet fixture.
    """
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    path, _ = duplicate_file(tmp_path, "parquet", ["count", "count"])
    table = pq.ParquetFile(path).read()
    polygons = []
    for row in range(3):
        x, y = 10.0 + row, 46.0 + row
        ring = [(x, y), (x + 0.25, y), (x + 0.25, y + 0.25), (x, y + 0.25), (x, y)]
        polygons.append(
            struct.pack("<BIII", 1, 3, 1, 5) + b"".join(struct.pack("<dd", *p) for p in ring)
        )
    table = table.set_column(2, "geometry", pa.array(polygons, pa.binary()))
    bounds = pa.StructArray.from_arrays(
        [pa.array([base + row for row in range(3)]) for base in [10.0, 46.0, 10.25, 46.25]],
        names=["xmin", "ymin", "xmax", "ymax"],
    )
    table = table.add_column(0, "bbox", bounds)
    geo = json.loads(table.schema.metadata[b"geo"])
    geo["columns"]["geometry"].update(
        geometry_types=["Polygon"],
        covering={"bbox": {key: ["bbox", key] for key in ["xmin", "ymin", "xmax", "ymax"]}},
    )
    table = table.replace_schema_metadata({b"geo": json.dumps(geo).encode()})
    pq.write_table(table, path, compression="zstd", row_group_size=2)
    for bbox, row in [((10.2, 46.2, 10.3, 46.3), 0), ((10.9, 46.9, 11.1, 47.1), 1)]:
        _, selected = tpg.read_arrow(path, columns=["count_2"], bbox=bbox)
        assert selected.column_names == ["count_2", "geometry"]
        assert selected["count_2"].to_pylist() == [12 + 10 * row]
        assert selected["geometry"].to_pylist() == [polygons[row]]


def test_geoparquet_arrow_duplicate_types_and_metadata(tmp_path):
    """Keep each repeated column's nested Arrow type, nulls and field metadata."""
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    schema = pa.schema(
        [
            pa.field("count", pa.int64()),
            pa.field("count", pa.list_(pa.int32()), metadata={b"source": b"second"}),
            pa.field("total", pa.string()),
        ]
    )
    source = pa.Table.from_arrays(
        [pa.array([1, None]), pa.array([[2, 3], None], pa.list_(pa.int32())), pa.array(["a", "b"])],
        schema=schema,
    )
    path = tmp_path / "typed.parquet"
    pq.write_table(source, path, compression="zstd")
    table = pa.table(terrio.open_stream(str(path)))
    assert table.column_names == ["count", "count_2", "total"]
    assert table["count"].to_pylist() == [1, None]
    assert table["count_2"].to_pylist() == [[2, 3], None]
    assert table.schema.field("count_2").type == pa.list_(pa.int32())
    assert table.schema.field("count_2").metadata == {b"source": b"second"}


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
