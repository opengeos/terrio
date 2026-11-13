"""Dotted Shapefile names keep their own components and preserve neighboring datasets."""

import subprocess
import sys

import pytest

import terrio
import terrio.pyogrio as tpg

COMPONENTS = ("shp", "shx", "dbf", "prj", "cpg")


@pytest.mark.parametrize("extension", ["geojson", "shx", "DBf", "CPG", "prj"])
@pytest.mark.parametrize("writer", ["convert", "cli"])
def test_normalized_output_alias_reads_source_before_writing(tmp_path, extension, writer):
    """Converting to an alias of the input retains its records and geometry.

    Args:
        tmp_path: Temporary directory.
        extension: Requested output extension, normalized by the explicit driver.
        writer: Public conversion surface.
    """
    source = tmp_path / "roads.v2.shp"
    install_components(source, 5)
    target = source.with_suffix("." + extension)
    if writer == "convert":
        terrio.convert(str(source), str(target), format="ESRI Shapefile")
    else:
        subprocess.run(
            [
                sys.executable,
                "-m",
                "terrio",
                "convert",
                str(source),
                str(target),
                "--format",
                "ESRI Shapefile",
            ],
            check=True,
            capture_output=True,
            text=True,
            timeout=60,
        )
    assert_point(terrio.open_vector(str(source))[0], 5, source.stem)
    assert_point(terrio.open_stream(str(source), batch_size=1).read_all(), 5, source.stem)


def points(value, crs="EPSG:4326"):
    """Create a point with distinct attributes and coordinates.

    Args:
        value: Integer identifying this dataset.
        crs: Coordinate reference system.

    Returns:
        terrio.Layer: One point with UTF-8 text.
    """
    return terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {"id": value, "label": "café"},
                    "geometry": {"type": "Point", "coordinates": [value, value]},
                }
            ],
        },
        name="roads",
        crs=crs,
    )


def install_components(path, value, crs="EPSG:4326", uppercase=False):
    """Install independently named components without relying on dotted-name writing.

    Args:
        path: Destination geometry filename.
        value: Dataset identifier.
        crs: Source CRS.
        uppercase: Use uppercase sidecar extensions.
    """
    staging = path.parent / "staging"
    staging.mkdir(exist_ok=True)
    seed = staging / "seed.shp"
    points(value, crs).write(str(seed))
    for extension in COMPONENTS:
        target = (
            path
            if extension == "shp"
            else path.with_suffix("." + (extension.upper() if uppercase else extension))
        )
        target.write_bytes(seed.with_suffix("." + extension).read_bytes())


def assert_point(layer, value, name=None):
    """Check attributes, geometry, CRS and optionally the dataset name.

    Args:
        layer: Loaded layer.
        value: Expected dataset identifier.
        name: Expected layer name, when applicable.
    """
    assert layer.crs == "EPSG:4326"
    if name is not None:
        assert layer.name == name
    feature = layer.to_geojson()["features"][0]
    assert feature["properties"] == {"id": value, "label": "café"}
    assert feature["geometry"]["coordinates"] == [value, value]


@pytest.mark.parametrize("writer", ["layer", "convert", "cli"])
@pytest.mark.parametrize(
    "filename,neighbor", [("roads.v2.shp", "roads.shp"), ("roads.part.v2.SHP", "roads.part.shp")]
)
def test_writes_keep_dotted_names_and_neighbor_files(tmp_path, writer, filename, neighbor):
    """Every writer creates the requested components without replacing a neighboring dataset.

    Args:
        tmp_path: Temporary directory.
        writer: Public write surface.
        filename: Requested dotted geometry filename.
        neighbor: Existing dataset that must remain unchanged.
    """
    directory = tmp_path / "folder.v1"
    directory.mkdir()
    target, other = directory / filename, directory / neighbor
    install_components(other, 1, crs="EPSG:3857")
    before = {
        extension: other.with_suffix("." + extension).read_bytes() for extension in COMPONENTS
    }
    layer = points(2)
    if writer == "layer":
        layer.write(str(target))
    else:
        source = directory / "input.geojson"
        layer.write(str(source))
        if writer == "convert":
            terrio.convert(str(source), str(target))
        else:
            subprocess.run(
                [sys.executable, "-m", "terrio", "convert", str(source), str(target)],
                check=True,
                capture_output=True,
                text=True,
                timeout=60,
            )
    assert target.exists()
    for extension in COMPONENTS:
        component = target if extension == "shp" else target.with_suffix("." + extension)
        assert component.exists(), component
        assert other.with_suffix("." + extension).read_bytes() == before[extension]
    assert_point(terrio.open_vector(str(target))[0], 2, target.stem)
    assert_point(terrio.open_stream(str(target), batch_size=1).read_all(), 2, target.stem)


@pytest.mark.parametrize("uppercase", [False, True])
@pytest.mark.parametrize("neighbor", [False, True])
@pytest.mark.parametrize("reader", ["whole", "stream", "range", "info"])
def test_reads_use_the_dotted_datasets_own_sidecars(tmp_path, uppercase, neighbor, reader):
    """Readers choose matching sidecars, including case variants and SHX range reads.

    Args:
        tmp_path: Temporary directory.
        uppercase: Sidecar extension case.
        neighbor: Add a competing undotted dataset.
        reader: Public read surface.
    """
    target = tmp_path / "roads.v5.shp"
    if neighbor:
        install_components(tmp_path / "roads.shp", 1, crs="EPSG:3857")
    install_components(target, 5, uppercase=uppercase)
    if reader == "whole":
        assert_point(terrio.open_vector(str(target))[0], 5, target.stem)
    elif reader == "stream":
        assert_point(terrio.open_stream(str(target), batch_size=1).read_all(), 5, target.stem)
    elif reader == "range":
        frame = tpg.read_dataframe(str(target), skip_features=0, max_features=1)
        assert frame["id"].tolist() == [5]
        assert frame["label"].tolist() == ["café"]
        assert frame.crs.to_epsg() == 4326
        assert frame.geometry.x.tolist() == [5]
        assert frame.geometry.y.tolist() == [5]
    else:
        info = tpg.read_info(str(target))
        assert list(info["fields"]) == ["id", "label"]
        assert info["crs"] == "EPSG:4326"


@pytest.mark.parametrize(
    "requested,expected",
    [
        ("output", "output.shp"),
        ("roads.v2.geojson", "roads.v2.shp"),
        ("roads.v2.DBf", "roads.v2.shp"),
        ("roads.v2.shx", "roads.v2.shp"),
        ("roads.v2.CPG", "roads.v2.shp"),
        ("roads.v2.prj", "roads.v2.shp"),
    ],
)
@pytest.mark.parametrize("writer", ["layer", "convert", "cli"])
def test_explicit_driver_derives_a_distinct_geometry_component(
    tmp_path, requested, expected, writer
):
    """Explicit Shapefile output keeps geometry separate from every companion component.

    Args:
        tmp_path: Temporary directory.
        requested: Output name with no or a different extension.
        expected: Normalized geometry component name.
        writer: Public writer.
    """
    neighbor = tmp_path / "roads.shp"
    install_components(neighbor, 1, crs="EPSG:3857")
    before = {
        extension: neighbor.with_suffix("." + extension).read_bytes() for extension in COMPONENTS
    }
    target, geometry = tmp_path / requested, tmp_path / expected
    layer = points(2)
    if writer == "layer":
        layer.write(str(target), driver="ESRI Shapefile")
    else:
        source = tmp_path / "input.geojson"
        layer.write(str(source))
        if writer == "convert":
            terrio.convert(str(source), str(target), format="ESRI Shapefile")
        else:
            subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "terrio",
                    "convert",
                    str(source),
                    str(target),
                    "--format",
                    "ESRI Shapefile",
                ],
                check=True,
                capture_output=True,
                text=True,
                timeout=60,
            )
    assert geometry.read_bytes()[:4] == (9994).to_bytes(4, "big")
    for extension in COMPONENTS:
        assert geometry.with_suffix("." + extension).exists()
        assert neighbor.with_suffix("." + extension).read_bytes() == before[extension]
    assert_point(terrio.open_vector(str(geometry))[0], 2, geometry.stem)
    assert_point(terrio.open_stream(str(geometry), batch_size=1).read_all(), 2, geometry.stem)
