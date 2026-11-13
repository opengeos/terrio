"""Engineering CRS definitions survive GeoTIFF writes and cannot be warped as Earth CRSs."""

import numpy as np
import pytest

import terrio

TRANSFORM = (1000.0, 10.0, 0.0, 2000.0, 0.0, -10.0)
VALUES = np.arange(12, dtype="float32").reshape(1, 3, 4)
LOCAL = (
    'LOCAL_CS["site grid",LOCAL_DATUM["site",32767],UNIT["metre",1],'
    'AXIS["Easting",EAST],AXIS["Northing",NORTH]]'
)
ENG = (
    'ENGCRS["site grid",EDATUM["site"],CS[Cartesian,2],AXIS["(E)",east,ORDER[1]],'
    'AXIS["(N)",north,ORDER[2]],LENGTHUNIT["metre",1]]'
)
ENGINEERING = (
    'ENGINEERINGCRS["Astra Minas Grid",ENGINEERINGDATUM["Astra Minas"],CS[Cartesian,2],'
    'AXIS["northing (X)",north,ORDER[1]],AXIS["westing (Y)",west,ORDER[2]],LENGTHUNIT["metre",1]]'
)
DEFINITIONS = [
    LOCAL,
    LOCAL.replace('UNIT["metre",1]', 'UNIT["metre",1,AUTHORITY["EPSG","9001"]]'),
    LOCAL[:-1] + ',AUTHORITY["EPSG","5800"]]',
    LOCAL.replace('UNIT["metre",1]', 'UNIT["metre",1,AUTHORITY["EPSG","9001"]]')[:-1]
    + ',AUTHORITY["EPSG","5800"]]',
    ENG,
    ENG[:-1] + ',ID["EPSG",5800]]',
    ENGINEERING,
    ENGINEERING[:-1] + ',ID["EPSG",5800]]',
]


@pytest.mark.parametrize("definition", DEFINITIONS, ids=range(len(DEFINITIONS)))
@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("surface", ["raster", "bytes", "rio", "stream"])
@pytest.mark.parametrize("use_pyproj", [True, False])
def test_engineering_crs_survives_gtiff_roundtrip(
    tmp_path, monkeypatch, definition, driver, surface, use_pyproj
):
    """Retain engineering WKT, user-defined GeoKeys, samples and the raster grid.

    Args:
        tmp_path: Directory for the output GeoTIFF.
        monkeypatch: Scoped override for the optional CRS conversion dependency.
        definition: An engineering WKT with optional root or unit identifiers.
        driver: The ordinary or cloud-optimized GeoTIFF writer.
        surface: The native array, byte or rasterio-compatible writing surface.
        use_pyproj: Whether the rasterio facade can use pyproj's WKT conversion.
    """
    tifffile = pytest.importorskip("tifffile")
    if use_pyproj:
        pytest.importorskip("pyproj")
    else:
        import importlib

        def missing_pyproj():
            """Represent an unavailable optional pyproj dependency."""
            return None

        monkeypatch.setattr(importlib.import_module("terrio.rio.crs"), "_pyproj", missing_pyproj)
    path = tmp_path / "site.tif"
    expected = definition
    if surface == "rio":
        import terrio.rio as rio
        from terrio.rio.transform import from_origin

        expected = rio.CRS.from_user_input(definition).to_wkt()
        with rio.open(
            path,
            "w",
            driver=driver,
            width=4,
            height=3,
            count=1,
            dtype="float32",
            transform=from_origin(1000, 2000, 10, 10),
            crs=definition,
        ) as target:
            target.write(VALUES)
    else:
        source = terrio.Raster.from_array(VALUES, TRANSFORM, crs=definition)
        assert source.crs_wkt == definition
        if surface == "bytes":
            path.write_bytes(source.to_bytes(driver=driver))
        elif surface == "stream":
            held = tmp_path / "source.tif"
            source.write(str(held))
            terrio.open_raster(str(held), lazy=True).write(str(path), driver=driver)
        else:
            source.write(str(path), driver=driver)
    with tifffile.TiffFile(path) as tiff:
        keys = tiff.pages[0].tags[34735].value
        entries = {keys[i]: keys[i + 3] for i in range(4, len(keys), 4)}
        assert entries[1024] == 32767
        assert 1026 in entries
        assert 3072 not in entries and 2048 not in entries
    for lazy in (False, True):
        result = terrio.open_raster(str(path), lazy=lazy)
        assert result.crs_wkt == expected
        assert tuple(result.transform) == TRANSFORM
        samples = np.asarray(result)
        assert samples.dtype == VALUES.dtype
        np.testing.assert_array_equal(samples, VALUES)
    with pytest.raises(terrio.CRSError):
        terrio.warp(str(path), t_srs="EPSG:4326")
    import terrio.rio as rio

    with rio.open(path) as result:
        assert result.crs is not None
        assert result.crs.to_wkt() == rio.CRS.from_user_input(expected).to_wkt()


@pytest.mark.parametrize(
    "definition", ["EPSG:32615", "EPSG:4326", "EPSG:4979", "EPSG:4978", "EPSG:5773"]
)
@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_registered_horizontal_codes_and_other_coded_definitions(tmp_path, definition, driver):
    """Use compact keys only for horizontal EPSG CRSs, retaining other WKT definitions.

    Args:
        tmp_path: Directory for the output raster.
        definition: A projected, geographic, geocentric or vertical EPSG CRS.
        driver: The ordinary or cloud-optimized writer.
    """
    tifffile = pytest.importorskip("tifffile")
    source = terrio.Raster.from_array(VALUES, TRANSFORM, crs=definition)
    path = tmp_path / "coded.tif"
    source.write(str(path), driver=driver)
    with tifffile.TiffFile(path) as tiff:
        keys = tiff.pages[0].tags[34735].value
        entries = {keys[i]: keys[i + 3] for i in range(4, len(keys), 4)}
    code = int(definition.split(":")[1])
    if code == 32615:
        assert entries[1024] == 1 and entries[3072] == code
    elif code in (4326, 4979):
        assert entries[1024] == 2 and entries[2048] == code
    else:
        assert entries[1024] == 32767
        assert 3072 not in entries and 2048 not in entries
    result = terrio.open_raster(str(path))
    assert result.crs == definition
    assert result.crs_wkt == source.crs_wkt
