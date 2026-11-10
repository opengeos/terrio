"""Dataset-level vector operations: concat, partition, set_field_type, make_point,
export_schema and create_vector."""

import json
import os
import warnings
import zipfile

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
    assert [feature["properties"]["s"] for feature in out[0].to_geojson()["features"]] == [
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
    terrio.create_vector(copy, schema=path)
    # GDAL 3.13 creates schema/template fields in name order; explicit fields
    # above retain the caller's order.
    expected = json.loads(json.dumps(schema))
    expected["layers"][0]["fields"].sort(key=lambda f: f["name"])
    assert terrio.export_schema(copy) == expected
    like = str(tmp_path / "like.gpkg")
    terrio.create_vector(like, like=src, output_layer="renamed")
    assert terrio.list_layers(like) == ["renamed"]
    with pytest.raises(terrio.InvalidArgumentError, match="exists"):
        terrio.create_vector(like, like=src)
    terrio.create_vector(like, schema=schema, update=True, output_layer="second")
    assert terrio.list_layers(like) == ["renamed", "second"]


def test_rename_edit_update_bindings(tmp_path):
    """Copies preserve inputs; path edits and key updates persist into GeoPackages."""
    original = points("Café", [("x", 1), ("y", 2)])
    renamed = terrio.rename_layer(original, ascii=True, lower_case=True)
    assert renamed.layer_names == ["cafe"]
    assert original.name == "Café"
    assigned = terrio.edit_vector(renamed, crs="EPSG:3857")
    assert "3857" in assigned[0].crs
    assert "4326" in renamed[0].crs
    changes = points("changes", [("x", 9), ("z", 3)])
    result = terrio.update_vector(changes, renamed, key=["name"])
    assert len(result[0]) == 3
    assert len(renamed[0]) == 2
    assert sorted(f["properties"]["n"] for f in result[0].to_geojson()["features"]) == [
        2,
        3,
        9,
    ]
    path = tmp_path / "edit.gpkg"
    terrio.concat([renamed], str(path))
    terrio.rename_layer(path, in_place=True, output_layer="renamed")
    assert terrio.list_layers(str(path)) == ["renamed"]
    terrio.edit_vector(
        path,
        in_place=True,
        crs="EPSG:3857",
        metadata=["REVIEW=checked"],
        layer_metadata=["DESCRIPTION=review"],
    )
    persisted = terrio.update_vector(changes, path, key=["name"], mode="update-only")
    assert len(persisted[0]) == 2
    with pytest.raises(terrio.InvalidArgumentError, match="metadata edits require"):
        terrio.edit_vector(original, metadata=["A=B"])
    with pytest.raises(terrio.InvalidArgumentError, match="different"):
        terrio.update_vector(path, path)
    with pytest.raises(terrio.InvalidArgumentError, match="excludes"):
        terrio.rename_layer(path, str(path), in_place=True)


def test_partition_preserves_source_inside_overwrite_directory(tmp_path):
    """Overwriting parts must never delete the source file in their directory."""
    path = str(tmp_path / "source.gpkg")
    terrio.concat([points("points", [("x", 1)])], path)
    before = (tmp_path / "source.gpkg").read_bytes()
    with pytest.raises(terrio.InvalidArgumentError, match="contains the input"):
        terrio.partition(path, str(tmp_path), fields=["name"], scheme="flat", overwrite=True)
    assert (tmp_path / "source.gpkg").read_bytes() == before


def test_copy_operations_refuse_existing_output(tmp_path):
    src = points("data", [("x", 1)])
    dst = tmp_path / "existing.gpkg"
    dst.write_bytes(b"preserve")
    for operation in [
        lambda: terrio.concat([src], str(dst)),
        lambda: terrio.rename_layer(src, str(dst)),
        lambda: terrio.edit_vector(src, str(dst)),
    ]:
        with pytest.raises(terrio.InvalidArgumentError, match="already exists"):
            operation()
        assert dst.read_bytes() == b"preserve"


def test_package_import_without_native_partition(monkeypatch):
    import runpy
    import sys

    import terrio._terrio as extension

    monkeypatch.delattr(extension, "partition")
    monkeypatch.setattr(sys, "platform", "emscripten")
    loaded = runpy.run_path(terrio.__file__)
    assert "partition" not in loaded["__all__"]
    assert "partition" not in loaded


def test_collision_errors_are_clear():
    a = points("a", [("x", 1)])
    b = points("b", [("y", 2)])
    with pytest.raises(terrio.InvalidArgumentError, match="source-layer field.*already exists"):
        terrio.concat([a, b], mode="single", source_layer_field_name="name")
    with pytest.raises(terrio.InvalidArgumentError, match="duplicate layer name"):
        terrio.rename_layer([a, b], input_layer="a", output_layer="b")


STYLED_KML = b"""<kml><Document>
  <Style id="base"><LineStyle><color>ff0000ff</color><width>2</width></LineStyle></Style>
  <Placemark><name>shared</name><styleUrl>#base</styleUrl><Point><coordinates>30,-2</coordinates></Point></Placemark>
</Document></kml>"""
BASE_STYLE = "PEN(c:#FF0000FF,w:2.000000px)"


def ogr_style(dataset):
    return dataset[0].to_geojson()["features"][0]["properties"]["OGR_STYLE"]


def test_kml_style_table_is_read_and_written(tmp_path):
    source = tmp_path / "a.kml"
    source.write_bytes(STYLED_KML)
    dataset = terrio.open_vector(str(source))
    assert dataset.style_table == [("base", BASE_STYLE)]
    assert dataset[0].columns()["OGR_STYLE"] == ["@base"]
    assert ogr_style(dataset) == BASE_STYLE

    # The dataset writes its own table to KML and KMZ.
    for name in ("out.kml", "out.kmz"):
        dataset.write(str(tmp_path / name))
        back = terrio.open_vector(str(tmp_path / name))
        assert back.style_table == [("base", BASE_STYLE)]
        assert back[0].columns()["OGR_STYLE"] == ["@base"]

    # A layer writes the table of its dataset, or the one given.
    assert b'<Style id="base">' in dataset[0].to_bytes("out.kml")
    other = dataset[0].to_bytes("out.kml", style_table=[("base", "PEN(c:#00FF00FF)")])
    assert b"<color>ff00ff00</color>" in other
    assert b"PEN(c:#FF0000FF" in dataset[0].to_bytes("out.geojson")
    assert b"PEN(c:#FF0000FF" in dataset[0].to_bytes("out.geojson", style_table=[])
    with pytest.raises(terrio.InvalidArgumentError, match="style_table"):
        dataset[0].to_bytes("out.geojson", style_table=dataset.style_table)

    # Other formats have no table to refer to: each feature gets its style.
    dataset.write(str(tmp_path / "out.geojson"))
    assert ogr_style(terrio.open_vector(str(tmp_path / "out.geojson"))) == BASE_STYLE


def test_kml_styles_resolve_when_a_path_is_converted(tmp_path):
    source = tmp_path / "a.kml"
    source.write_bytes(STYLED_KML)
    terrio.convert(str(source), str(tmp_path / "c.kml"))
    converted = (tmp_path / "c.kml").read_bytes()
    assert b'<Style id="base">' in converted
    assert b"<styleUrl>#base</styleUrl>" in converted
    terrio.convert(str(source), str(tmp_path / "c.geojson"))
    assert ogr_style(terrio.open_vector(str(tmp_path / "c.geojson"))) == BASE_STYLE


def kml_document(name, points):
    placemarks = "<Placemark><Point><coordinates>1,2</coordinates></Point></Placemark>" * points
    body = f"<Document><name>{name}</name>{placemarks}</Document>"
    return f'<kml xmlns="http://www.opengis.net/kml/2.2">{body}</kml>'


def kmz(path, members):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")  # zipfile warns about a duplicate name it is asked to write
        with zipfile.ZipFile(path, "w") as archive:
            for name, text in members:
                archive.writestr(name, text)
    return str(path)


def test_a_kmz_is_read_from_the_first_kml_entry_at_its_root(tmp_path):
    """OGC KML 2.3, Annex C.1: the first KML file at the root is the main one, whatever its name."""
    first, second = kml_document("first", 1), kml_document("second", 3)
    path = kmz(tmp_path / "places.kmz", [("a.kml", first), ("doc.kml", second)])
    dataset = terrio.open_vector(path)
    assert [(layer.name, len(layer)) for layer in dataset] == [("first", 1)]
    assert terrio.list_layers(path) == ["first"]
    with open(path, "rb") as f:
        in_memory = terrio.open_bytes(f.read(), "places.kmz")
    assert [(layer.name, len(layer)) for layer in in_memory] == [("first", 1)]


def test_a_kmz_with_two_entries_of_one_name_is_refused(tmp_path):
    first, second = kml_document("first", 1), kml_document("second", 3)
    path = kmz(tmp_path / "places.kmz", [("doc.kml", first), ("doc.kml", second)])
    ambiguous = "several entries with the same name"
    with pytest.raises(terrio.InvalidDataError, match=ambiguous):
        terrio.open_vector(path)
    with pytest.raises(terrio.InvalidDataError, match=ambiguous):
        terrio.list_layers(path)
    with open(path, "rb") as f, pytest.raises(terrio.InvalidDataError, match=ambiguous):
        terrio.open_bytes(f.read(), "places.kmz")
