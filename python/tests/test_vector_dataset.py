"""Dataset-level vector operations: concat, partition, set_field_type, make_point,
export_schema and create_vector."""

import json
import os

import pytest

import terrio


def points(name, rows, fields=("name", "n")):
    """A GeoJSON layer of points with attributes."""
    return terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": dict(zip(fields, r, strict=True)),
                    "geometry": {"type": "Point", "coordinates": [i, i]},
                }
                for i, r in enumerate(rows)
            ],
        },
        name=name,
        crs="EPSG:4326",
    )


def test_concat_modes(tmp_path):
    a = points("a", [("x", 1), ("y", 2)])
    b = points("b", [("z", 3)])
    merged = terrio.concat([a, b], mode="single", source_layer_field_name="src")
    assert merged.layer_names == ["merged"]
    layer = merged[0]
    assert len(layer) == 3
    assert list(layer.fields)[0] == "src"
    stacked = terrio.concat([a, b], mode="stack", output_layer="{LAYER_INDEX}_{LAYER_NAME}")
    assert stacked.layer_names == ["0_a", "0_b"]
    out = str(tmp_path / "all.gpkg")
    terrio.concat([a, b], out, mode="single", output_layer="all")
    assert terrio.list_layers(out) == ["all"]
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.concat([a, b], output_layer="x")


def test_partition_hive_and_flat(tmp_path):
    layer = points("places", [("TN", 1), ("NY", 2), ("TN", 3), (None, 4)], fields=("state", "n"))
    hive = tmp_path / "hive"
    written = terrio.partition(layer, str(hive), ["state"], format="GeoJSON")
    rel = sorted(os.path.relpath(p, hive).replace(os.sep, "/") for p in written)
    assert rel == [
        "places/state=NY/part_0000000001.json",
        "places/state=TN/part_0000000001.json",
        "places/state=__HIVE_DEFAULT_PARTITION__/part_0000000001.json",
    ]
    tn = terrio.open_vector(str(hive / "places/state=TN/part_0000000001.json"))[0]
    assert len(tn) == 2
    with pytest.raises(terrio.InvalidArgumentError, match="exists"):
        terrio.partition(layer, str(hive), ["state"], format="GeoJSON")
    flat = tmp_path / "flat"
    written = terrio.partition(
        layer, str(flat), ["state"], scheme="flat", feature_limit=1, format="GPKG"
    )
    names = sorted(os.path.basename(p) for p in written)
    assert names[:2] == ["places_NY_0000000001.gpkg", "places_TN_0000000001.gpkg"]
    assert "places_TN_0000000002.gpkg" in names
    # Overwrite replaces a partitioned directory; append adds parts after the existing ones.
    terrio.partition(layer, str(flat), ["state"], scheme="flat", format="GPKG", overwrite=True)
    more = terrio.partition(layer, str(flat), ["state"], scheme="flat", format="GPKG", append=True)
    assert any(p.endswith("places_TN_0000000002.gpkg") for p in more)


def test_set_field_type_conversions(tmp_path):
    layer = points("t", [("12", 1.7), ("abc", -1.7), ("3.9", 3e10)], fields=("s", "r"))
    with pytest.warns(UserWarning, match="parsed incompletely"):
        out = terrio.set_field_type(layer, "Integer", field_name="s")
    assert [f["s"] for f in out[0].to_geojson()["features"] for f in [f["properties"]]] == [
        12,
        0,
        3,
    ]
    with pytest.warns(UserWarning, match="lossy conversion"):
        out = terrio.set_field_type(layer, "Integer", src_field_type="Real")
    values = [f["properties"]["r"] for f in out[0].to_geojson()["features"]]
    assert values == [1, -1, 2147483647]
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.set_field_type(layer, "Integer", field_name="missing")
    with pytest.raises(terrio.UnsupportedError):
        terrio.set_field_type(layer, "IntegerList", field_name="s")


def test_make_point_from_fields():
    layer = points("t", [("-105.5", 40.25), ("-104", 39.0)], fields=("lon", "lat"))
    out = terrio.make_point(layer, "lon", "lat", crs="EPSG:4326")
    assert out.crs == "EPSG:4326"
    coords = [f["geometry"]["coordinates"] for f in out.to_geojson()["features"]]
    assert coords == [[-105.5, 40.25], [-104.0, 39.0]]
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.make_point(layer, "nope", "lat")


def test_export_schema_and_create(tmp_path):
    src = str(tmp_path / "src.gpkg")
    terrio.create_vector(
        src,
        output_layer="sites",
        geometry_type="POINT",
        crs="EPSG:32633",
        fields=["name:STRING(40)", "count:INTEGER", "total:INTEGER64", "day:DATE"],
    )
    schema = terrio.export_schema(src)
    layer = schema["layers"][0]
    assert layer["name"] == "sites"
    assert layer["fidColumnName"] == "fid"
    assert layer["geometryFields"][0]["type"] == "Point"
    assert layer["geometryFields"][0]["coordinateSystem"] == {"authid": "EPSG:32633"}
    assert [(f["name"], f["type"]) for f in layer["fields"]] == [
        ("name", "String"),
        ("count", "Integer"),
        ("total", "Integer64"),
        ("day", "Date"),
    ]
    assert layer["fields"][0]["width"] == 40
    # Round trip through a schema file and a template.
    path = tmp_path / "schema.json"
    path.write_text(json.dumps(schema))
    copy = str(tmp_path / "copy.gpkg")
    terrio.create_vector(copy, schema=str(path))
    assert terrio.export_schema(copy) == schema
    like = str(tmp_path / "like.gpkg")
    terrio.create_vector(like, like=src, output_layer="renamed")
    assert terrio.list_layers(like) == ["renamed"]
    with pytest.raises(terrio.InvalidArgumentError, match="exists"):
        terrio.create_vector(like, like=src)
    terrio.create_vector(like, schema=schema, update=True, output_layer="second")
    assert terrio.list_layers(like) == ["renamed", "second"]
