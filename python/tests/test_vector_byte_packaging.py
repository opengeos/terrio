"""Vector byte filenames describe their document or archive, on the public Python API."""

import io
import zipfile

import pytest

import terrio


@pytest.fixture
def points():
    """Create points with scalar attributes and a geographic CRS.

    Returns:
        Layer: A three-feature WGS 84 point layer.
    """
    return terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {"name": f"place_{i}", "n": i},
                    "geometry": {"type": "Point", "coordinates": [10.123456789 + i, 46.987654321]},
                }
                for i in range(3)
            ],
        },
        name="points",
        crs="EPSG:4326",
    )


@pytest.mark.parametrize(
    "suffix",
    [
        "geojson",
        "geojsonl",
        "topojson",
        "gpkg",
        "parquet",
        "fgb",
        "kml",
        "kmz",
        "gml",
        "gpx",
        "dxf",
        "csv",
        "tsv",
        "xlsx",
        "shp.zip",
        "mif.zip",
        "tab.zip",
    ],
)
def test_named_vector_byte_outputs_round_trip(points, suffix):
    """Every default output in the issue reads back under the same filename.

    Args:
        points: A point layer.
        suffix: Supported output extension or explicit archive suffix.
    """
    filename = f"points.{suffix}"
    data = points.to_bytes(filename)
    assert sum(len(layer) for layer in terrio.open_bytes(data, filename)) == 3
    if suffix == "gml":
        assert data.startswith(b"<?xml")
        assert not zipfile.is_zipfile(io.BytesIO(data))


@pytest.mark.parametrize("suffix", ["shp", "mif", "tab"])
def test_multifile_byte_outputs_refuse_bare_names(points, suffix):
    """A bare multi-file extension must not silently receive a ZIP archive.

    Args:
        points: A point layer.
        suffix: Format requiring multiple files.
    """
    with pytest.raises(terrio.InvalidArgumentError, match=rf"\.{suffix}\.zip"):
        points.to_bytes(f"points.{suffix}")


@pytest.mark.parametrize("suffix", ["gml", "mif", "tab"])
def test_mislabeled_vector_zip_input_is_refused(points, suffix):
    """A ZIP cannot be interpreted as an empty or malformed standalone document.

    Args:
        points: A point layer.
        suffix: Incorrect standalone document extension.
    """
    data = points.to_bytes("points.mif.zip")
    with pytest.raises(terrio.InvalidArgumentError, match="ZIP archive.*\\.zip"):
        terrio.open_bytes(data, f"points.{suffix}")


@pytest.mark.parametrize("suffix", ["gml", "mif.zip", "tab.zip"])
def test_packaged_vectors_retain_attributes_geometry_and_crs(points, suffix):
    """New packaging preserves values and CRS within the MapInfo coordinate grid.

    Args:
        points: A point layer.
        suffix: Correct single-document or archive extension.
    """
    filename = f"points.{suffix}"
    restored = terrio.open_bytes(points.to_bytes(filename), filename)[0]
    assert restored.crs == "EPSG:4326"
    expected = points.to_geojson()["features"]
    actual = restored.to_geojson()["features"]
    for i, (before, after) in enumerate(zip(expected, actual, strict=True)):
        properties = dict(after["properties"])
        if suffix == "gml":
            assert properties.pop("gml_id") == f"points.{i}"
        assert properties == before["properties"]
        assert after["geometry"]["coordinates"] == pytest.approx(
            before["geometry"]["coordinates"], abs=1e-6, rel=0
        )


@pytest.mark.parametrize("filename", ["points.zip", "points.v2.zip"])
def test_explicit_mapinfo_driver_uses_a_mif_member_in_an_unnamed_zip(points, filename):
    """A driver-selected .zip uses the default MIF format for its members.

    Args:
        points: A point layer.
        filename: An archive filename, including a dotted stem.
    """
    stem = filename.removesuffix(".zip")
    data = points.to_bytes(filename, driver="MapInfo File")
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        assert set(archive.namelist()) == {f"{stem}.mif", f"{stem}.mid"}
    restored = terrio.open_bytes(data, filename)[0]
    assert restored.name == stem
    assert len(restored) == 3


def test_gml_document_preserves_fractional_values_and_date_text(points):
    """Pin current inference of fractional values and lexical date text without a schema.

    Args:
        points: A point layer.
    """
    document = points.to_geojson()
    for i, feature in enumerate(document["features"]):
        feature["properties"].update({"value": i + 0.25, "survey_date": "2024-01-02"})
    layer = terrio.Layer.from_geojson(document, name="typed", crs="EPSG:4326")
    restored = terrio.open_bytes(layer.to_bytes("typed.gml"), "typed.gml")[0].to_geojson()
    for before, after in zip(document["features"], restored["features"], strict=True):
        assert after["properties"]["survey_date"] == before["properties"]["survey_date"]
        assert after["properties"]["value"] == before["properties"]["value"]
        assert isinstance(after["properties"]["value"], float)


def test_gml_zip_output_is_refused_with_a_document_filename_hint(points):
    """GML bytes are standalone XML; schema sidecars belong to filesystem output.

    Args:
        points: A point layer.
    """
    with pytest.raises(terrio.InvalidArgumentError, match="use a .gml filename"):
        points.to_bytes("points.gml.zip")
