"""Vector layers read as streams: `open_stream`, `iter_features`, Arrow streams and streamed
`convert`, compared with whole reads and with GDAL."""

import pytest

import terrio


def squares(n):
    """A layer of `n` squares with an integer, a real and a date field.

    Args:
        n: The number of features.

    Returns:
        terrio.Layer: The layer, in EPSG:4326.
    """
    features = []
    for i in range(n):
        x = -170.0 + (i % 300)
        y = -80.0 + (i // 300) * 0.5
        features.append(
            {
                "type": "Feature",
                "properties": {
                    "i": i,
                    "v": i / 7,
                    "name": f"f{i}",
                    "day": f"2024-02-{i % 28 + 1:02d}",
                },
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [
                        [[x, y], [x + 0.4, y], [x + 0.4, y + 0.4], [x, y + 0.4], [x, y]]
                    ],
                },
            }
        )
    return terrio.Layer.from_geojson(
        {"type": "FeatureCollection", "features": features}, name="squares", crs="EPSG:4326"
    )


@pytest.fixture(params=["gpkg", "fgb", "parquet", "shp", "geojson", "geojsonl", "csv"])
def path(request, tmp_path):
    p = str(tmp_path / f"squares.{request.param}")
    squares(1000).write(p)
    return p


def test_chunks_match_whole_reads(path):
    whole = terrio.open_vector(path)[0]
    stream = terrio.open_stream(path, batch_size=128)
    assert stream.name == whole.name
    assert stream.fields == whole.fields
    assert stream.crs == whole.crs
    sizes = []
    features = []
    for chunk in stream:
        sizes.append(len(chunk))
        features.extend(chunk.__geo_interface__["features"])
    assert sizes == [128] * 7 + [104]
    assert features == whole.__geo_interface__["features"]
    assert list(terrio.iter_features(path, batch_size=300)) == features
    assert len(terrio.open_stream(path).read_all()) == 1000


def test_arrow_stream_matches_table(path):
    pa = pytest.importorskip("pyarrow")
    whole = pa.table(terrio.open_vector(path)[0])
    reader = pa.RecordBatchReader.from_stream(terrio.open_stream(path, batch_size=250))
    batches = list(reader)
    assert [b.num_rows for b in batches] == [250] * 4
    table = pa.Table.from_batches(batches)
    if path.endswith(".parquet"):
        # a GeoParquet stream hands out the file's own types (`i` is written as int32)
        assert table.schema.field("i").type == pa.int32()
        # each exported field has the type the file stores, so the cast below hides no change
        pq = pytest.importorskip("pyarrow.parquet")
        file_schema = pq.read_schema(path)
        for field in table.schema:
            assert field.type == file_schema.field(field.name).type, field.name
        whole = whole.cast(table.schema)
    assert table.equals(whole)


def test_bbox_and_errors(tmp_path):
    p = str(tmp_path / "s.gpkg")
    squares(1000).write(p)
    inside = terrio.open_stream(p, bbox=(-100.0, -80.0, -90.0, -70.0)).read_all()
    assert 0 < len(inside) < 1000
    assert len(inside) == len(terrio.open_vector(p, bbox=(-100.0, -80.0, -90.0, -70.0))[0])
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.open_stream(p, "nope")
    stream = terrio.open_stream(p)
    next(stream)
    with pytest.raises(terrio.InvalidArgumentError):
        stream.__arrow_c_stream__()


def test_geoparquet_arrow_streams_hand_out_the_files_batches(tmp_path):
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    p = str(tmp_path / "s.parquet")
    squares(1000).write(p)
    batches = list(pa.RecordBatchReader.from_stream(terrio.open_stream(p, batch_size=300)))
    assert [b.num_rows for b in batches] == [300, 300, 300, 100]
    table = pa.Table.from_batches(batches)
    # the file's columns, without the bounding-box covering column
    assert table.column_names == ["i", "v", "name", "day", "geometry"]
    plain = pq.read_table(p)
    for name in table.column_names:
        assert table[name].to_pylist() == plain[name].to_pylist()
    geometry = table.schema.field("geometry")
    assert geometry.metadata[b"ARROW:extension:name"] == b"geoarrow.wkb"
    assert b"crs" in geometry.metadata[b"ARROW:extension:metadata"]


def test_streams_select_columns_and_rows(tmp_path):
    pa = pytest.importorskip("pyarrow")
    box = (-100.0, -80.0, -90.0, -70.0)
    for ext in ["parquet", "gpkg"]:
        p = str(tmp_path / f"s.{ext}")
        squares(1000).write(p)
        whole = terrio.open_vector(p, bbox=box)[0].__geo_interface__["features"]
        assert 0 < len(whole) < 1000, ext
        # chunks with the fields asked for, in the layer's order
        stream = terrio.open_stream(p, bbox=box, columns=["name", "i"])
        assert list(stream.fields) == ["i", "name"], ext
        features = stream.read_all().__geo_interface__["features"]
        assert [f["properties"] for f in features] == [
            {"i": f["properties"]["i"], "name": f["properties"]["name"]} for f in whole
        ], ext
        # and the Arrow export
        table = pa.table(terrio.open_stream(p, bbox=box, columns=["i"]))
        assert table.column_names[0] == "i" and "name" not in table.column_names, ext
        assert table["i"].to_pylist() == [f["properties"]["i"] for f in whole], ext
        with pytest.raises(terrio.InvalidArgumentError):
            terrio.open_stream(p, columns=["nope"])
        # names match without case, on both outputs, with the values of an unfiltered read
        whole_i = [
            f["properties"]["i"] for f in terrio.open_vector(p)[0].__geo_interface__["features"]
        ]
        table = pa.table(terrio.open_stream(p, columns=["I"]))
        assert table.column_names[0] == "i", ext
        assert table["i"].to_pylist() == whole_i, ext
        stream = terrio.open_stream(p, columns=["I"])
        assert list(stream.fields) == ["i"], ext
        features = stream.read_all().__geo_interface__["features"]
        assert [f["properties"]["i"] for f in features] == whole_i, ext


def test_geoparquet_streams_read_files_in_archives(tmp_path):
    pa = pytest.importorskip("pyarrow")
    import zipfile

    p = str(tmp_path / "s.parquet")
    squares(1000).write(p)
    z = str(tmp_path / "s.zip")
    with zipfile.ZipFile(z, "w") as f:
        f.write(p, "s.parquet")
    table = pa.table(terrio.open_stream(f"/vsizip/{z}/s.parquet"))
    assert table.num_rows == 1000
    assert table["i"].to_pylist() == list(range(1000))


def test_geoparquet_streams_give_a_geodataframe_in_crs84(tmp_path):
    gpd = pytest.importorskip("geopandas")
    p = str(tmp_path / "s.parquet")
    squares(1000).write(p)
    gdf = gpd.GeoDataFrame.from_arrow(terrio.open_stream(p))
    assert len(gdf) == 1000
    # GeoParquet without a `crs` means OGC:CRS84, as geopandas reads it (GDAL names it EPSG:4326)
    assert gdf.crs.equals("OGC:CRS84")
    assert gdf.geometry.iloc[0].equals(terrio.open_vector(p)[0].to_geopandas().geometry.iloc[0])


@pytest.mark.parametrize("ext", ["fgb", "parquet", "shp", "geojsonl", "csv", "geojson"])
def test_streamed_convert_matches_layers(tmp_path, ext):
    src = str(tmp_path / "src.gpkg")
    squares(1000).write(src)
    dst = str(tmp_path / f"out.{ext}")
    terrio.convert(src, dst, where="i >= 100", limit=500, t_srs="EPSG:3857")
    expected = terrio.open_vector(src)[0].to_crs("EPSG:3857")
    out = terrio.open_vector(dst)[0]
    assert len(out) == 500
    got = out.__geo_interface__["features"]
    want = [f for f in expected.__geo_interface__["features"] if f["properties"]["i"] >= 100][:500]
    # FlatGeobuf files hold their features in the order of their spatial index.
    assert sorted(f["properties"]["i"] for f in got) == [f["properties"]["i"] for f in want]


@pytest.mark.parametrize("ext", ["gpkg", "fgb", "parquet", "shp", "geojsonl"])
def test_streams_match_gdal(tmp_path, ogr, ext):
    src = str(tmp_path / f"s.{ext}")
    squares(700).write(src)
    ds = ogr.Open(src)
    lyr = ds.GetLayer(0)
    gdal_rows = [(f.GetField("i"), f.GetGeometryRef().GetEnvelope()) for f in lyr]
    ours = [
        (
            f["properties"]["i"],
            f["geometry"]["coordinates"][0][0][0],
            f["geometry"]["coordinates"][0][2][0],
        )
        for f in terrio.iter_features(src, batch_size=64)
    ]
    assert [r[0] for r in gdal_rows] == [r[0] for r in ours]
    for (_, env), (_, x0, x1) in zip(gdal_rows, ours, strict=True):
        assert env[0] == pytest.approx(x0) and env[1] == pytest.approx(x1)
