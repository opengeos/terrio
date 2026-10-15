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
    assert pa.Table.from_batches(batches).equals(whole)


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
