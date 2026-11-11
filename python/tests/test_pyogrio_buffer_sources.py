"""Buffer recognition and archive selection use the same readers as named vector files."""

import io
import subprocess
import sys
import zipfile
from pathlib import Path

import numpy as np
import pytest

import terrio
import terrio.pyogrio as tpg
from terrio.pyogrio.errors import DataSourceError


@pytest.fixture(scope="module")
def sources(tmp_path_factory):
    """Write real vector datasets with the formats reported in the buffer issues.

    Args:
        tmp_path_factory: Temporary directory factory.

    Returns:
        dict: Format extensions mapped to dataset paths.
    """
    directory = tmp_path_factory.mktemp("buffer-sources")
    layer = terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {"id": i},
                    "geometry": {"type": "Point", "coordinates": [10 + i, 46]},
                }
                for i in range(4)
            ],
        },
        name="points",
        crs="EPSG:4326",
    )
    paths = {}
    for extension in ["geojson", "gpkg", "fgb", "parquet", "kml"]:
        path = directory / f"points.{extension}"
        path.write_bytes(layer.to_bytes(path.name))
        paths[extension] = path
    gpx = directory / "points.gpx"
    gpx.write_text(
        '<gpx version="1.1" creator="terrio" xmlns="http://www.topografix.com/GPX/1/1">'
        + "".join(f'<wpt lat="46" lon="{10 + i}"><name>p{i}</name></wpt>' for i in range(4))
        + "</gpx>"
    )
    paths["gpx"] = gpx
    osm = directory / "points.osm"
    osm.write_text(
        '<osm version="0.6">'
        + "".join(
            f'<node id="{i + 1}" lat="46" lon="{10 + i}"><tag k="name" v="p{i}"/></node>'
            for i in range(4)
        )
        + "</osm>"
    )
    paths["osm"] = osm
    gml = directory / "points.gml"
    gml.write_text(
        '<gml:FeatureCollection xmlns:gml="http://www.opengis.net/gml" '
        'xmlns:app="https://example.test/points">'
        + "".join(
            f'<gml:featureMember><app:points gml:id="points.{i}"><app:name>p{i}</app:name>'
            '<app:geom><gml:Point srsName="EPSG:4326">'
            f"<gml:pos>46 {10 + i}</gml:pos></gml:Point></app:geom></app:points>"
            "</gml:featureMember>"
            for i in range(4)
        )
        + "</gml:FeatureCollection>"
    )
    paths["gml"] = gml
    return paths


def buffer(data, mode):
    """Wrap a file's bytes in one of the public input forms.

    Args:
        data: Dataset bytes.
        mode: Input representation.

    Returns:
        object: Bytes or a readable file object.
    """
    return {"bytes": bytes, "bytearray": bytearray, "memoryview": memoryview, "stream": io.BytesIO}[
        mode
    ](data)


@pytest.mark.parametrize(
    "extension", ["geojson", "gpkg", "fgb", "parquet", "kml", "gpx", "osm", "gml"]
)
@pytest.mark.parametrize("mode", ["bytes", "bytearray", "memoryview", "stream"])
def test_vector_buffers_match_named_files(sources, extension, mode):
    """Dataframes, metadata and layer geometry types agree for files and buffers.

    Args:
        sources: Dataset paths.
        extension: Vector format.
        mode: Public buffer representation.
    """
    pytest.importorskip("geopandas")
    from geopandas.testing import assert_geodataframe_equal

    path = sources[extension]
    data = path.read_bytes()
    expected = tpg.read_dataframe(path, layer=0)
    actual = tpg.read_dataframe(buffer(data, mode), layer=0)
    assert_geodataframe_equal(actual, expected)
    assert len(actual) == 4
    expected_layers = tpg.list_layers(path).tolist()
    if extension == "parquet":
        # Parquet has no stored layer name: anonymous bytes use the temporary file's data stem.
        expected_layers[0][0] = "data"
    assert tpg.list_layers(buffer(data, mode)).tolist() == expected_layers
    info = tpg.read_info(buffer(data, mode), layer=0)
    reference = tpg.read_info(path, layer=0)
    assert info["features"] == reference["features"] == 4
    assert info["driver"] == reference["driver"]
    assert info["geometry_type"] == reference["geometry_type"]
    for key in ["fields", "dtypes"]:
        np.testing.assert_array_equal(info[key], reference[key])


@pytest.mark.parametrize("extension", ["geojson", "gpkg", "fgb", "parquet", "kml", "gml"])
@pytest.mark.parametrize("nested", [False, True])
@pytest.mark.parametrize("mode", ["path", "bytes", "stream"])
def test_dataset_beside_readme_opens_as_the_core_does(tmp_path, sources, extension, nested, mode):
    """An archive's ancillary files do not hide its main vector dataset.

    Args:
        tmp_path: Temporary directory.
        sources: Dataset paths.
        extension: Archived vector format.
        nested: Whether the dataset is in a subdirectory.
        mode: Public path or buffer representation.
    """
    pytest.importorskip("geopandas")
    from geopandas.testing import assert_geodataframe_equal

    path = tmp_path / "published.zip"
    member = f"{'data/' if nested else ''}points.{extension}"
    with zipfile.ZipFile(path, "w") as archive:
        archive.write(sources[extension], member)
        archive.writestr("README.txt", "About these data\n")
    data = path.read_bytes()
    source = path if mode == "path" else buffer(data, mode)
    assert_geodataframe_equal(
        tpg.read_dataframe(source, layer=0), tpg.read_dataframe(sources[extension])
    )
    # Recreate readable streams because read_dataframe consumes them.
    source = path if mode == "path" else buffer(data, mode)
    assert tpg.list_layers(source).tolist() == tpg.list_layers(sources[extension]).tolist()
    assert terrio.list_layers(str(path)) == [row[0] for row in tpg.list_layers(path)]


@pytest.mark.parametrize(
    "data",
    [
        b"WKT,id\nPOINT (10 46),1\n",
        b"fgb_id,lat,lon\n1,2,3\n",
        b"PAR1_id,lat,lon\n1,2,3\n",
        b"PAR1,id\nx,PAR1",
        b"fgb,fgb,name\n1,2,3\n",
    ],
)
def test_signature_free_content_keeps_a_clear_refusal(data):
    """Unsigned CSV bytes still require a filename, including weak binary-prefix collisions.

    Args:
        data: CSV content that lacks a binary format signature.
    """
    with pytest.raises(DataSourceError, match="/vsimem/pyogrio.*not recognized"):
        tpg.read_info(data)


def test_top_level_json_arrays_are_refused_as_invalid_geojson():
    """Shared identification keeps the existing GeoJSON diagnostic for a feature array."""
    data = b'[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,2]}}]'
    with pytest.raises(DataSourceError, match="GeoJSON: no type"):
        tpg.read_info(data)


@pytest.mark.parametrize(
    "data,extension",
    [
        (
            b'{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"Topology":"survey"},"geometry":{"type":"Point","coordinates":[1,2]}}]}',
            "geojson",
        ),
        (
            b'<osm version="0.6"><node id="1" lat="2" lon="1">'
            b'<tag k="name" v="gml survey"/></node></osm>',
            "osm",
        ),
    ],
)
def test_property_text_cannot_override_the_document_format(data, extension):
    """Root identifiers take precedence over format-like property text.

    Args:
        data: A complete GeoJSON or OSM document containing another format's token.
        extension: Its correct filename hint.
    """
    assert terrio._terrio.vector_byte_extension(data) == extension
    assert len(tpg.read_dataframe(data, layer=0)) == 1


@pytest.mark.parametrize("prefix", [b"", b"\xef\xbb\xbf", b" \n"])
def test_osm_xml_buffers_have_an_xml_filename_hint(prefix):
    """An OSM XML upload uses the XML extension rather than the binary PBF suffix.

    Args:
        prefix: A supported XML byte prefix.
    """
    assert terrio._terrio.vector_byte_extension(prefix + b'<osm version="0.6"/>') == "osm"


@pytest.mark.parametrize("prefix", [b"", b"\xef\xbb\xbf", b"  \n"])
def test_geojson_buffer_prefixes_preserve_identification(prefix, sources):
    """GeoJSON byte-order marks and leading whitespace retain the anonymous read path.

    Args:
        prefix: Supported text prefix.
        sources: Real vector fixtures.
    """
    data = prefix + sources["geojson"].read_bytes()
    assert terrio._terrio.vector_byte_extension(data) == "geojson"
    assert tpg.read_info(data)["features"] == 4


@pytest.mark.parametrize("version", [2, 3])
def test_flatgeobuf_supported_magic_versions_read_the_payload(version, sources):
    """Hints retain older magic versions accepted by the underlying FlatGeobuf reader.

    Args:
        version: Header version accepted by the codec.
        sources: Valid FlatGeobuf payload.
    """
    data = bytearray(sources["fgb"].read_bytes())
    data[3] = version
    assert terrio._terrio.vector_byte_extension(bytes(data)) == "fgb"
    assert len(tpg.read_dataframe(data)) == 4


@pytest.mark.parametrize("form", ["path", "bytes", "file"])
def test_archives_without_vector_datasets_have_a_clear_error(tmp_path, form):
    """Ancillary files alone must not be accepted as an empty vector dataset.

    Args:
        tmp_path: Temporary archive directory.
        form: Path, anonymous bytes or a file object.
    """
    path = tmp_path / "notes.zip"
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("README.txt", "About the data")
        archive.writestr("LICENSE.txt", "License terms")
    source = (
        path
        if form == "path"
        else path.read_bytes()
        if form == "bytes"
        else io.BytesIO(path.read_bytes())
    )
    with pytest.raises(DataSourceError, match="no dataset found"):
        tpg.read_info(source)


@pytest.fixture
def buffer_reference(sources, extension, driver):
    """Run reference setup separately so crashes and timeouts are pytest errors.

    Args:
        sources: Dataset paths.
        extension: Signed vector format.
        driver: GDAL driver required by the reference.

    Returns:
        CompletedProcess: The comparison subprocess result.
    """
    return _run_buffer_reference(sources[extension], extension, driver)


def _run_buffer_reference(path, extension, driver):
    """Run an isolated comparison with distinct mismatch, skip and setup outcomes.

    Args:
        path: Dataset to compare.
        extension: Signed vector format.
        driver: Required GDAL driver.

    Returns:
        CompletedProcess: A successful comparison or an assertion mismatch.
    """
    # GDAL's Parquet driver registers C++ Arrow types. Keep those registrations out of
    # the parent process, where unrelated tests exercise third-party Arrow consumers.
    result = subprocess.run(
        [sys.executable, __file__, str(path), extension, driver],
        capture_output=True,
        text=True,
        timeout=60,
    )
    if result.returncode == 77:
        pytest.skip(result.stdout.strip())
    if result.returncode not in (0, 79):
        raise RuntimeError(
            f"buffer reference failed ({result.returncode}): {result.stdout}{result.stderr}"
        )
    return result


@pytest.mark.parametrize("status", [0, 79, 77, 1, 78, -11])
def test_buffer_reference_process_outcomes(monkeypatch, tmp_path, status):
    """Only explicit comparison assertions become data differences.

    Args:
        monkeypatch: Scoped process replacement.
        tmp_path: Temporary dataset directory.
        status: Child exit code or signal.
    """
    result = subprocess.CompletedProcess([], status, "oracle output", "oracle diagnostic")
    monkeypatch.setattr(subprocess, "run", lambda *args, **kwargs: result)
    if status == 77:
        with pytest.raises(pytest.skip.Exception):
            _run_buffer_reference(tmp_path / "data.fgb", "fgb", "FlatGeobuf")
    elif status not in (0, 79):
        with pytest.raises(RuntimeError, match="buffer reference failed"):
            _run_buffer_reference(tmp_path / "data.fgb", "fgb", "FlatGeobuf")
    else:
        assert _run_buffer_reference(tmp_path / "data.fgb", "fgb", "FlatGeobuf") is result


@pytest.mark.parametrize(
    "extension,driver", [("gpkg", "GPKG"), ("fgb", "FlatGeobuf"), ("parquet", "Parquet")]
)
def test_signed_buffers_match_gdal(extension, driver, buffer_reference):
    """Compare signed buffers after the reference process initialized successfully.

    Args:
        extension: Signed vector format.
        driver: Required reference driver.
        buffer_reference: The reference comparison result.
    """
    result = buffer_reference
    assert result.returncode == 0, result.stdout + result.stderr


def _compare_signed_buffer_with_gdal(path, extension, driver):
    """Compare a buffer in a process that owns the reference driver's Arrow registrations.

    Args:
        path: Generated vector dataset.
        extension: Signed vector format.
        driver: Required GDAL reference driver.
    """
    reference = pytest.importorskip("pyogrio")
    if reference is tpg or getattr(reference, "__terrio_version__", None):
        pytest.skip("reference pyogrio resolves to the Terrio compatibility adapter")
    if driver not in reference.list_drivers():
        pytest.skip(f"GDAL's {driver} driver is unavailable in pyogrio")
    pytest.importorskip("geopandas")
    from geopandas.testing import assert_geodataframe_equal

    data = path.read_bytes()
    expected = reference.read_dataframe(io.BytesIO(data))
    assert_geodataframe_equal(tpg.read_dataframe(data), expected)
    if extension == "parquet":
        # GDAL registers geoarrow.wkb globally, consuming extension field metadata on import.
        meta, _table = tpg.read_arrow(path)
        assert meta["geometry_name"] == "geometry"
        assert_geodataframe_equal(tpg.read_dataframe(path, use_arrow=True), expected)


@pytest.mark.parametrize("extension", ["gpkg", "fgb", "parquet", "kml"])
def test_buffer_readers_keep_the_native_driver_policy(sources, extension):
    """Recognizing a signature does not bypass scoped driver restrictions.

    Args:
        sources: Dataset paths.
        extension: Refused vector format.
    """
    import terrio.rio as rio

    with rio.Env(TERRIO_ALLOWED_DRIVERS="GeoJSON"):
        with pytest.raises(DataSourceError, match="driver is not allowed"):
            tpg.read_info(sources[extension].read_bytes(), layer=0)


if __name__ == "__main__":
    try:
        _compare_signed_buffer_with_gdal(Path(sys.argv[1]), sys.argv[2], sys.argv[3])
    except pytest.skip.Exception as error:
        print(str(error))
        raise SystemExit(77) from None
    except AssertionError:
        import traceback

        traceback.print_exc()
        raise SystemExit(79) from None
    except Exception:
        import traceback

        traceback.print_exc()
        raise SystemExit(78) from None
