"""Buffer recognition and archive selection use the same readers as named vector files."""

import io
import zipfile

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


def test_signature_free_content_keeps_a_clear_refusal():
    """Unsigned CSV bytes still require a filename, instead of an arbitrary driver guess."""
    with pytest.raises(DataSourceError, match="/vsimem/pyogrio.*not recognized"):
        tpg.read_dataframe(b"WKT,id\nPOINT (10 46),1\n")


@pytest.mark.parametrize(
    "extension,driver", [("gpkg", "GPKG"), ("fgb", "FlatGeobuf"), ("parquet", "Parquet")]
)
def test_signed_buffers_match_gdal(sources, extension, driver):
    """Compare signed buffer reads with the installed Pyogrio/GDAL reference.

    Args:
        sources: Dataset paths.
        extension: Signed vector format.
        driver: GDAL driver required by the reference.
    """
    reference = pytest.importorskip("pyogrio")
    if reference is tpg or getattr(reference, "__terrio_version__", None):
        pytest.skip("reference pyogrio resolves to the Terrio compatibility adapter")
    if driver not in reference.list_drivers():
        pytest.skip(f"GDAL's {driver} driver is unavailable in pyogrio")
    from geopandas.testing import assert_geodataframe_equal

    data = sources[extension].read_bytes()
    expected = reference.read_dataframe(io.BytesIO(data))
    assert_geodataframe_equal(tpg.read_dataframe(data), expected)


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
