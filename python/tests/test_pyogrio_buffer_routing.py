"""Ambiguous SQLite, geometry sequences and no-layer ZIPs retain caller intent."""

import io
import json
import sqlite3
import zipfile

import pytest

import terrio
import terrio.pyogrio as tpg
from terrio.pyogrio.errors import DataSourceError

pytest_plugins = ["test_pyogrio_buffer_sources"]


@pytest.mark.parametrize("count", [1, 2])
@pytest.mark.parametrize("mode", ["path", "bytes", "named", "file"])
@pytest.mark.parametrize("read", [tpg.read_dataframe, tpg.read_info, tpg.list_layers])
def test_non_dataset_zip_errors_name_the_input(tmp_path, monkeypatch, count, mode, read):
    """Name unsupported archives consistently without exposing extracted copies.

    Args:
        tmp_path: Directory for the archive.
        monkeypatch: Run relative-path reads from the fixture directory.
        count: Number of non-dataset members.
        mode: Path, anonymous bytes, named bytes or an open file.
        read: Pyogrio entry point under test.
    """
    monkeypatch.chdir(tmp_path)
    encoded = io.BytesIO()
    with zipfile.ZipFile(encoded, "w") as archive:
        for index in range(count):
            archive.writestr(f"README{index}.txt", "not a dataset")
    data = encoded.getvalue()
    path = tmp_path / "readme.zip"
    path.write_bytes(data)
    source = path.name if mode == "path" else data
    if mode == "named":
        source = io.BytesIO(data)
        source.name = path.name
    if mode == "file":
        source = path.open("rb")
    try:
        with pytest.raises(DataSourceError) as caught:
            read(source)
        shown = "buffer" if mode == "bytes" else path.name
        assert str(caught.value) == f"no dataset found in '{shown}'"
    finally:
        if hasattr(source, "close"):
            source.close()


@pytest.mark.parametrize("read", [terrio.open, terrio.info, terrio.open_vector, terrio.list_layers])
@pytest.mark.parametrize("member", ["README.txt", "broken.gpkg"])
def test_explicit_zip_member_errors_keep_the_virtual_path(tmp_path, monkeypatch, read, member):
    """Keep original member names through general, vector and metadata opens.

    Args:
        tmp_path: Directory for the archive.
        monkeypatch: Run relative-path reads from the fixture directory.
        read: Native open or inspection entry point.
        member: Unsupported text or a malformed GeoPackage to inspect.
    """
    monkeypatch.chdir(tmp_path)
    with zipfile.ZipFile("readme #2.zip", "w") as archive:
        archive.writestr(member, "not a dataset")
    path = f"/vsizip/readme #2.zip/{member}"
    with pytest.raises(terrio.TerrioError) as caught:
        read(path)
    assert path in str(caught.value)
    assert "terrio-" not in str(caught.value)


@pytest.mark.parametrize("read", [tpg.read_dataframe, tpg.read_info, tpg.list_layers])
@pytest.mark.parametrize("named", [False, True])
def test_malformed_zip_buffer_resolution_names_the_input(read, named):
    """Name constructor-time archive failures without disclosing the spill path.

    Args:
        read: Public pyogrio entry point.
        named: Whether the corrupt upload has a caller-provided filename.
    """
    source = b"PK\x03\x04broken"
    if named:
        source = io.BytesIO(source)
        source.name = "broken.zip"
    with pytest.raises(DataSourceError) as caught:
        read(source)
    message = str(caught.value)
    assert ("broken.zip" if named else "buffer") in message
    assert "not a zip file" in message.lower()
    assert "terrio-pyogrio-" not in message


@pytest.mark.parametrize("read", [tpg.read_dataframe, tpg.read_info, tpg.list_layers])
def test_malformed_zip_upload_redacts_credential_like_filename(read):
    """Redact the final constructor error after replacing raw temporary paths.

    Args:
        read: Public pyogrio entry point.
    """
    source = io.BytesIO(b"PK\x03\x04broken")
    source.name = "broken#sig=PRIVATE.zip"
    with pytest.raises(DataSourceError) as caught:
        read(source)
    message = str(caught.value)
    assert "broken#<redacted>" in message
    assert "PRIVATE" not in message
    assert "terrio-pyogrio-" not in message


@pytest.mark.parametrize("read", [tpg.read_dataframe, tpg.list_layers])
@pytest.mark.parametrize(
    "suffix,data",
    [
        ("geojson", b'{"type":"FeatureCollection","features": [invalid]}'),
        ("gpkg", b"SQLite format 3\0broken"),
    ],
)
def test_bad_dataset_buffer_keeps_its_reason_without_spill_paths(read, suffix, data):
    """Retain malformed dataset diagnostics while hiding the buffer's storage path.

    Args:
        read: Pyogrio entry point under test.
        suffix: The malformed dataset's declared format.
        data: Invalid document or database bytes.
    """
    source = io.BytesIO(data)
    source.name = f"bad.{suffix}"
    with pytest.raises(DataSourceError) as caught:
        read(source)
    message = str(caught.value)
    assert source.name in message
    assert "no dataset found" not in message
    assert "terrio-pyogrio-" not in message


@pytest.mark.parametrize("before", [0, 1, 2])
def test_collection_dimensions_do_not_depend_on_sequence_position(tmp_path, before):
    """Infer a collection's declared dimensions from its first member at any position.

    Args:
        tmp_path: Temporary dataset directory.
        before: Number of null-geometry features before the collection.
    """
    collection = {
        "type": "GeometryCollection",
        "geometries": [
            {"type": "Point", "coordinates": [10, 46]},
            {"type": "Point", "coordinates": [10, 46, 7]},
        ],
    }
    records = [{"type": "Feature", "properties": {}, "geometry": None}] * before + [collection]
    data = b"".join(json.dumps(record).encode() + b"\n" for record in records)
    path = tmp_path / "collection.geojsonl"
    path.write_bytes(data)
    stream = io.BytesIO(data)
    stream.name = path.name
    # A single anonymous geometry is standalone GeoJSON, whose collection-Z rule differs.
    sources = (path, stream, data) if before else (path, stream)
    for source in sources:
        info = tpg.read_info(source)
        assert info["geometry_type"] == "GeometryCollection"
        assert info["features"] == len(records)


@pytest.mark.parametrize(
    "records,expected",
    [
        ([{"type": "Point", "coordinates": [10, 46]}], "Point"),
        ([{"type": "Point", "coordinates": [10, 46, 7]}], "Point Z"),
        (
            [
                {"type": "Point", "coordinates": [10, 46]},
                {"type": "Point", "coordinates": [11, 46, 7]},
            ],
            "Point Z",
        ),
        (
            [
                {
                    "type": "Feature",
                    "properties": {},
                    "geometry": {"type": "Point", "coordinates": [10, 46]},
                },
                {"type": "LineString", "coordinates": [[10, 46], [11, 47]]},
            ],
            "Unknown",
        ),
    ],
)
def test_sequence_geometry_types_include_bare_records(tmp_path, records, expected):
    """Include root geometries and later-record dimensions in declared layer types.

    Args:
        tmp_path: Temporary dataset directory.
        records: Feature and bare-geometry sequence records.
        expected: Declared geometry type exposed by the compatibility API.
    """
    data = b"".join(json.dumps(record).encode() + b"\n" for record in records)
    path = tmp_path / "geometry.geojsonl"
    path.write_bytes(data)
    stream = io.BytesIO(data)
    stream.name = path.name
    for source in (path, stream, data):
        info = tpg.read_info(source)
        assert info["features"] == len(records)
        assert info["geometry_type"] == expected


def test_geopackage_archive_without_vector_layers_is_refused(tmp_path):
    """Apply the no-layer rule to the fast GeoPackage layer-listing path.

    Args:
        tmp_path: Temporary dataset directory.
    """
    gpkg = tmp_path / "no_features.gpkg"
    with sqlite3.connect(gpkg) as database:
        database.executescript(
            "PRAGMA application_id=0x47504b47; "
            "CREATE TABLE gpkg_contents(table_name TEXT,data_type TEXT,description TEXT); "
            "INSERT INTO gpkg_contents VALUES('tiles','tiles','');"
        )
    assert terrio.list_layers(str(gpkg)) == []
    archive = tmp_path / "no_features.zip"
    with zipfile.ZipFile(archive, "w") as output:
        output.write(gpkg, gpkg.name)
    with pytest.raises(terrio.UnsupportedError, match="no dataset found"):
        terrio.list_layers(str(archive))
    with pytest.raises(DataSourceError, match="no dataset found"):
        tpg.list_layers(archive)


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
