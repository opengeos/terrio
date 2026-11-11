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


def test_explicit_mapinfo_driver_uses_a_mif_member_in_an_unnamed_zip(points):
    """A driver-selected .zip uses the default MIF format for its members.

    Args:
        points: A point layer.
    """
    data = points.to_bytes("points.zip", driver="MapInfo File")
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        assert set(archive.namelist()) == {"points.mif", "points.mid"}
    assert len(terrio.open_bytes(data, "points.zip")[0]) == 3


def test_gml_zip_output_is_refused_with_a_document_filename_hint(points):
    """GML bytes are standalone XML; schema sidecars belong to filesystem output.

    Args:
        points: A point layer.
    """
    with pytest.raises(terrio.InvalidArgumentError, match="use a .gml filename"):
        points.to_bytes("points.gml.zip")
