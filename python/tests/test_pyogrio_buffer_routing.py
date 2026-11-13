"""Ambiguous SQLite, geometry sequences and no-layer ZIPs retain caller intent."""

import io
import zipfile

import pytest

import terrio
import terrio.pyogrio as tpg
from terrio.pyogrio.errors import DataSourceError

pytest_plugins = ["test_pyogrio_buffer_sources"]


@pytest.mark.parametrize("extension", ["mbtiles", "gpkg", "geojsonl"])
def test_named_buffers_match_their_paths(tmp_path, sources, extension):
    """Keep a compatible filename when a signature alone cannot choose the reader.

    Args:
        tmp_path: Temporary file directory.
        sources: Encoded datasets produced by the format writers.
        extension: SQLite container or geometry sequence extension.
    """
    pytest.importorskip("geopandas")
    path = sources[extension]
    data = path.read_bytes()
    if extension == "geojsonl":
        data = b"".join(b'{"type":"Point","coordinates":[%d,46]}\n' % i for i in range(4))
        path = tmp_path / "points.geojsonl"
        path.write_bytes(data)
    stream = io.BytesIO(data)
    stream.name = path.name
    expected = tpg.read_dataframe(path)
    actual = tpg.read_dataframe(stream)
    assert len(actual) == len(expected) == 4
    assert actual.geometry.equals(expected.geometry)
    assert tpg.read_info(stream)["features"] == 4
    assert tpg.list_layers(stream).tolist() == tpg.list_layers(path).tolist()
    if extension != "mbtiles":
        assert len(tpg.read_dataframe(data)) == 4


@pytest.mark.parametrize("reader", [tpg.read_dataframe, tpg.read_info, tpg.list_layers])
def test_anonymous_mbtiles_requests_a_filename(sources, reader):
    """Ambiguous SQLite bytes ask for a filename rather than reporting a GeoPackage error.

    Args:
        sources: Encoded MBTiles dataset.
        reader: Public compatibility entry point.
    """
    if reader is tpg.read_dataframe:
        pytest.importorskip("geopandas")
    with pytest.raises(DataSourceError, match="SQLite buffer needs a filename"):
        reader(sources["mbtiles"].read_bytes())


def test_workbook_named_zip_is_identified_by_its_content(sources):
    """Use the workbook reader when an uploaded XLSX has a generic ZIP filename.

    Args:
        sources: Encoded XLSX dataset.
    """
    stream = io.BytesIO(sources["xlsx"].read_bytes())
    stream.name = "workbook.zip"
    assert tpg.read_info(stream)["features"] == 4


def test_named_sqlite_refusal_requests_an_identifying_suffix(sources):
    """Name the ambiguous upload and identify the suffixes that disambiguate it.

    Args:
        sources: Encoded MBTiles dataset.
    """
    stream = io.BytesIO(sources["mbtiles"].read_bytes())
    stream.name = "points.db"
    with pytest.raises(DataSourceError, match="SQLite buffer.*points.db.*mbtiles.*gpkg"):
        tpg.read_info(stream)


@pytest.mark.parametrize("nested", [False, True])
def test_archives_without_layers_name_the_input(tmp_path, sources, nested):
    """Refuse a nonempty archive whose selected KML contains no layers.

    Args:
        tmp_path: Temporary file directory.
        sources: KMZ containing a root KML and separate layer files.
        nested: Whether to place the KML entries below a folder.
    """
    encoded = io.BytesIO()
    with zipfile.ZipFile(sources["kmz"]) as original, zipfile.ZipFile(encoded, "w") as output:
        for name in original.namelist():
            output.writestr(("kmz/" if nested else "") + name, original.read(name))
    data = encoded.getvalue()
    path = tmp_path / "renamed.zip"
    path.write_bytes(data)
    for read in (terrio.open_vector, terrio.open):
        with pytest.raises(
            terrio.errors.UnsupportedError, match="no dataset found in 'renamed.zip'"
        ):
            read(str(path))
    with pytest.raises(terrio.errors.UnsupportedError, match="no dataset found in 'data.zip'"):
        terrio.open_bytes(data, "data.zip")
    stream = io.BytesIO(data)
    stream.name = "renamed.zip"
    with pytest.raises(DataSourceError, match="no dataset found in 'renamed.zip'"):
        tpg.list_layers(stream)
    if nested:
        with pytest.raises(DataSourceError, match="no dataset found in 'buffer'"):
            tpg.list_layers(data)
