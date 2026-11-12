"""`skip_features` and `max_features` through `terrio.pyogrio.read_dataframe`: the features from
`skip_features` on, `max_features` of them at most, in the layer's order, whatever the format of
the file, and counted after the `bbox` filter when there is one."""

import json

import pytest

pytest.importorskip("geopandas")

import terrio  # noqa: E402
import terrio.pyogrio as tpg  # noqa: E402

NAMES = [f"p{i}" for i in range(5)]


@pytest.fixture(scope="module")
def points(tmp_path_factory):
    """Five points `p0` to `p4`, point `i` at `(i, 0)`, as GeoJSON and as a GeoPackage."""
    directory = tmp_path_factory.mktemp("points")
    geojson = directory / "points.geojson"
    features = [
        {
            "type": "Feature",
            "properties": {"name": name},
            "geometry": {"type": "Point", "coordinates": [i, 0]},
        }
        for i, name in enumerate(NAMES)
    ]
    geojson.write_text(json.dumps({"type": "FeatureCollection", "features": features}))
    gpkg = directory / "points.gpkg"
    terrio.convert(str(geojson), str(gpkg))
    return {"GeoJSON": str(geojson), "GPKG": str(gpkg)}


@pytest.mark.parametrize("driver", ["GeoJSON", "GPKG"])
@pytest.mark.parametrize(
    ("skip", "limit", "names"),
    [(1, 2, ["p1", "p2"]), (4, None, ["p4"]), (0, 1, ["p0"]), (5, None, []), (0, None, NAMES)],
)
def test_a_feature_range_reads_those_features(points, driver, skip, limit, names):
    frame = tpg.read_dataframe(points[driver], skip_features=skip, max_features=limit)
    assert frame["name"].tolist() == names


@pytest.mark.parametrize("driver", ["GeoJSON", "GPKG"])
def test_a_feature_range_counts_the_features_a_bbox_keeps(points, driver):
    # The bbox keeps p1 to p3: the second of them alone.
    frame = tpg.read_dataframe(
        points[driver], bbox=(0.5, -1, 3.5, 1), skip_features=1, max_features=1
    )
    assert frame["name"].tolist() == ["p2"]


@pytest.mark.parametrize("damaged", [0, 1, 2])
@pytest.mark.parametrize("memory", [False, True])
def test_gpkg_page_decodes_only_selected_records(tmp_path, damaged, memory):
    """A page ignores malformed skipped records but still refuses a selected malformed record."""
    import sqlite3

    path = tmp_path / "ranged.gpkg"
    names = ["poison-left-012345", "selected", "poison-right-987654"]
    with sqlite3.connect(path) as db:
        db.executescript(
            "CREATE TABLE gpkg_contents(table_name TEXT,data_type TEXT,description TEXT);"
            "INSERT INTO gpkg_contents VALUES('features','attributes','');"
            "CREATE TABLE features(fid INTEGER PRIMARY KEY,name TEXT);"
        )
        db.executemany("INSERT INTO features VALUES(?,?)", zip([11, 23, 47], names, strict=True))
    raw = bytearray(path.read_bytes())
    text = names[damaged].encode()
    record = bytes([3, 0, 13 + 2 * len(text)]) + text
    offset = raw.find(record)
    assert offset >= 0
    raw[offset + 2] += 2  # The unselected text now declares one byte beyond its record.
    path.write_bytes(raw)
    source = path.read_bytes() if memory else path
    if damaged == 1:
        from terrio.pyogrio.errors import DataSourceError

        with pytest.raises(DataSourceError, match="truncated record"):
            tpg.read_dataframe(source, skip_features=1, max_features=1, read_geometry=False)
    else:
        page = tpg.read_dataframe(
            source, skip_features=1, max_features=1, read_geometry=False, fid_as_index=True
        )
        assert page["name"].tolist() == ["selected"]
        assert page.index.tolist() == [23]
