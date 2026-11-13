"""CSV column collisions preserve values, types and geometry across reads and writes."""

import csv
import subprocess
import sys

import pytest

import terrio
import terrio.pyogrio as tpg

HEADER_CASES = [
    (["a", "a", "b"], ["a", "a_2", "b"], [1, 2, 3]),
    (["a", "a", "a", "b"], ["a", "a_2", "a_3", "b"], [1, 2, 3, 4]),
    (["x", "b", "b"], ["x", "b", "b_2"], [1, 2, 3]),
    (["a", "A", "b"], ["a", "A", "b"], [1, 2, 3]),
    (["a", "a", "a_2", "b"], ["a", "a_3", "a_2", "b"], [1, 2, 3, 4]),
    (["", "field_1", "b"], ["field_1_2", "field_1", "b"], [1, 2, 3]),
    (["label", "label", "b"], ["label", "label_2", "b"], ["text", "other", 3]),
]


@pytest.mark.parametrize("extension,separator", [("csv", ","), ("tsv", "\t")])
@pytest.mark.parametrize("header,names,values", HEADER_CASES)
@pytest.mark.parametrize("reader", ["whole", "stream", "range", "convert"])
def test_duplicate_input_columns_retain_their_values(
    tmp_path, extension, separator, header, names, values, reader
):
    """Repeated headers and generated-name collisions keep every source column.

    Args:
        tmp_path: Temporary directory.
        extension: CSV or TSV filename suffix.
        separator: Delimiter.
        header: Original column names.
        names: Expected distinct names.
        values: One record's values.
        reader: Public read surface.
    """
    source = tmp_path / ("input." + extension)
    with source.open("w", newline="", encoding="utf-8") as stream:
        writer = csv.writer(stream, delimiter=separator)
        writer.writerow(header)
        writer.writerow(values)
    expected = dict(zip(names, values, strict=True))
    if reader == "range":
        frame = tpg.read_dataframe(str(source), max_features=1)
        if "geometry" in frame.columns:
            assert frame.geometry.isna().all()
            frame = frame.drop(columns="geometry")
        assert list(frame.columns) == names
        assert frame.to_dict("records") == [expected]
        return
    if reader == "stream":
        layer = terrio.open_stream(str(source), batch_size=1).read_all()
    elif reader == "convert":
        # SQLite identifiers are case-insensitive, unlike CSV headers.
        suffix = "geojson" if len({n.lower() for n in names}) < len(names) else "gpkg"
        destination = tmp_path / ("converted." + suffix)
        terrio.convert(str(source), str(destination))
        layer = terrio.open_vector(str(destination))[0]
    else:
        layer = terrio.open_vector(str(source))[0]
    if reader != "convert":
        assert list(layer.fields) == names
    assert layer.to_geojson()["features"][0]["properties"] == expected
    assert layer.fields == {
        name: "Text" if isinstance(value, str) else "Integer"
        for name, value in zip(names, values, strict=True)
    }


def point_layer(identifier, mode=None):
    """Create one point, optionally with attributes colliding with geometry columns.

    Args:
        identifier: Feature identifier.
        mode: Geometry output mode whose canonical names should be occupied.

    Returns:
        terrio.Layer: Layer with known attributes and geometry.
    """
    properties = {"id": identifier, "name": f"p{identifier}"}
    if mode == "AS_WKT":
        properties.update(WKT=f"attribute-{identifier}", WKT_2=f"existing-{identifier}")
    elif mode == "AS_XY":
        properties.update(X=900 + identifier, Y=901 + identifier, Z=902 + identifier)
    return terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": properties,
                    "geometry": {
                        "type": "Point",
                        "coordinates": [10 + identifier, 46, 5 + identifier],
                    },
                }
            ],
        },
        name="points",
        crs="EPSG:4326",
    )


def assert_distinct_header_and_features(path, identifiers, mode=None):
    """Check the physical header, original attributes and point coordinates.

    Args:
        path: Output dataset.
        identifiers: Expected feature identifiers.
        mode: Whether original attribute names collide with geometry columns.
    """
    with path.open(newline="", encoding="utf-8") as stream:
        header = next(csv.reader(stream, delimiter="\t" if path.suffix == ".tsv" else ","))
    assert len(header) == len(set(header)), header
    actual = terrio.open_vector(str(path))[0].to_geojson()["features"]
    assert len(actual) == len(identifiers)
    for feature, identifier in zip(actual, identifiers, strict=True):
        expected = point_layer(identifier, mode).to_geojson()["features"][0]
        assert feature["geometry"] == expected["geometry"]
        for key, value in expected["properties"].items():
            assert feature["properties"][key] == value


@pytest.mark.parametrize("extension", ["csv", "tsv"])
@pytest.mark.parametrize("surface", ["convert", "cli", "append"])
def test_self_written_csv_converts_and_appends_without_column_shifts(tmp_path, extension, surface):
    """A CSV's retained WKT field does not shift ordinary attributes on its next write.

    Args:
        tmp_path: Temporary directory.
        extension: Output format.
        surface: Public conversion surface.
    """
    first, output = tmp_path / ("first." + extension), tmp_path / ("output." + extension)
    point_layer(0).write(str(first))
    if surface == "append":
        output = first
        source = tmp_path / "more.geojson"
        point_layer(1).write(str(source))
        terrio.convert(str(source), str(output), append=True)
    elif surface == "convert":
        terrio.convert(str(first), str(output))
    else:
        subprocess.run(
            [sys.executable, "-m", "terrio", "convert", str(first), str(output)],
            check=True,
            capture_output=True,
            text=True,
            timeout=60,
        )
    assert_distinct_header_and_features(output, [0, 1] if surface == "append" else [0])


@pytest.mark.parametrize("extension", ["csv", "tsv"])
@pytest.mark.parametrize(
    "mode,surface",
    [
        ("AS_WKT", "layer"),
        ("AS_WKT", "bytes"),
        ("AS_WKT", "convert"),
        ("AS_WKT", "cli"),
        ("AS_WKT", "append"),
        ("AS_XY", "cli"),
        ("AS_XY", "append"),
    ],
)
def test_geometry_columns_do_not_replace_colliding_attributes(tmp_path, extension, mode, surface):
    """Generated geometry aliases leave original attribute names and values intact.

    Args:
        tmp_path: Temporary directory.
        extension: Output format.
        mode: WKT or coordinate geometry output.
        surface: Public writer.
    """
    layer = point_layer(0, mode)
    source, output = tmp_path / "input.geojson", tmp_path / ("output." + extension)
    layer.write(str(source))
    if surface == "layer":
        layer.write(str(output))
    elif surface == "bytes":
        output.write_bytes(layer.to_bytes(output.name))
    elif surface == "cli" or mode == "AS_XY":
        subprocess.run(
            [
                sys.executable,
                "-m",
                "terrio",
                "convert",
                str(source),
                str(output),
                "--lco",
                "GEOMETRY=" + mode,
            ],
            check=True,
            capture_output=True,
            text=True,
            timeout=60,
        )
        if surface == "append":
            point_layer(1, mode).write(str(source))
            subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "terrio",
                    "convert",
                    str(source),
                    str(output),
                    "--append",
                    "--lco",
                    "GEOMETRY=" + mode,
                ],
                check=True,
                capture_output=True,
                text=True,
                timeout=60,
            )
    else:
        terrio.convert(str(source), str(output))
        if surface == "append":
            point_layer(1, mode).write(str(source))
            terrio.convert(str(source), str(output), append=True)
    assert_distinct_header_and_features(output, [0, 1] if surface == "append" else [0], mode)


@pytest.mark.parametrize("coordinate_columns", [False, True])
@pytest.mark.parametrize("explicit", [False, True])
def test_explicit_geometry_names_override_generated_alias_inference(
    tmp_path, coordinate_columns, explicit
):
    """Explicit possible-name options retain control over aliased geometry columns.

    Args:
        tmp_path: Temporary directory.
        coordinate_columns: Use coordinate aliases instead of a WKT alias.
        explicit: Restrict inference to the unsuffixed canonical names.
    """
    path = tmp_path / "aliases.csv"
    path.write_text("X_2,Y_2\n10,46\n" if coordinate_columns else "WKT_2\nPOINT (10 46)\n")
    options = (
        {"X_POSSIBLE_NAMES": "X", "Y_POSSIBLE_NAMES": "Y", "GEOM_POSSIBLE_NAMES": "WKT"}
        if explicit
        else {}
    )
    feature = terrio.open_vector(str(path), options=options)[0].to_geojson()["features"][0]
    expected = None if explicit else {"type": "Point", "coordinates": [10, 46]}
    assert feature["geometry"] == expected
