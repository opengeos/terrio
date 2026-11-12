"""GeoPackage raster limits precede tile decoding and include Float64 working grids."""

import os
import sqlite3
import struct
import subprocess
import sys

import numpy as np
import pytest

import terrio
import terrio.rio as rio


@pytest.fixture
def coverage(tmp_path, gdal):
    """A GDAL-authored Float32 coverage with a partial tile and explicit nodata."""
    source = gdal.GetDriverByName("MEM").Create("", 32, 16, 1, gdal.GDT_Float32)
    source.SetGeoTransform([0, 1, 0, 16, 0, -1])
    source.SetProjection("EPSG:4326")
    values = np.arange(512, dtype="float32").reshape(16, 32)
    values[0, 0] = -9999
    source.GetRasterBand(1).SetNoDataValue(-9999)
    source.GetRasterBand(1).WriteArray(values)
    path = tmp_path / "coverage.gpkg"
    result = gdal.Translate(str(path), source, format="GPKG", creationOptions=["RASTER_TABLE=grid"])
    assert result is not None
    result = None
    return path


def test_exact_limits_preserve_coverage_values_and_metadata(coverage, gdal):
    """A stored 256-square tile must not inflate the logical 32-by-16 declaration."""
    reference = gdal.Open(str(coverage))
    with rio.Env(TERRIO_MAX_BANDS="1", TERRIO_MAX_DECLARED_BYTES="2048"):
        native = terrio.open_raster(str(coverage))
        assert native.transform == reference.GetGeoTransform()
        assert native.crs == "EPSG:4326"
        with rio.open(coverage) as dataset:
            assert dataset.dtypes == ("float32",)
            np.testing.assert_array_equal(dataset.read(1), reference.ReadAsArray())
            np.testing.assert_array_equal(
                dataset.read_masks(1), reference.GetRasterBand(1).GetMaskBand().ReadAsArray()
            )
            assert dataset.crs.to_epsg() == 4326
            assert dataset.nodata == reference.GetRasterBand(1).GetNoDataValue()


def test_mixed_geopackage_keeps_explicit_vector_routes_after_raster_refusal(coverage, gdal):
    """A raster refusal stays terminal for raster-first drawing and complete dataset checks."""
    from osgeo import ogr, osr

    dataset = gdal.OpenEx(str(coverage), gdal.OF_UPDATE | gdal.OF_VECTOR)
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(4326)
    layer = dataset.CreateLayer("areas", srs=crs, geom_type=ogr.wkbPolygon)
    layer.CreateField(ogr.FieldDefn("name", ogr.OFTString))
    feature = ogr.Feature(layer.GetLayerDefn())
    feature.SetField("name", "kept")
    feature.SetGeometry(ogr.CreateGeometryFromWkt("POLYGON ((0 0,2 0,2 2,0 2,0 0))"))
    assert layer.CreateFeature(feature) == 0
    feature = layer = dataset = None

    summary = terrio.check_dataset(str(coverage))
    assert summary["raster_samples"] == 512
    assert summary["layers"] == summary["features"] == 1
    with rio.Env(TERRIO_MAX_DECLARED_BYTES="1"):
        for read in (
            lambda: terrio.open_raster(str(coverage)),
            lambda: terrio.render(str(coverage)),
            lambda: terrio.check_dataset(str(coverage)),
        ):
            with pytest.raises(terrio.ResourceLimitError, match="TERRIO_MAX_DECLARED_BYTES"):
                read()
        for vector in (terrio.open_vector(str(coverage)), terrio.open(str(coverage))):
            assert isinstance(vector, terrio.VectorDataset)
            assert vector.layer_names == ["areas"]
            assert len(vector[0]) == 1
            assert terrio.render(vector, max_size=32).startswith(b"\x89PNG")
            clipped = terrio.clip_vector(vector, like=str(coverage), like_layer="areas")
            assert isinstance(clipped, terrio.Layer)
            assert len(clipped) == 1
            assert clipped.to_geojson()["features"][0]["properties"]["name"] == "kept"


@pytest.mark.parametrize("tile_format", ["rgb-png", "palette-png", "rgb-tiff"])
@pytest.mark.parametrize(
    "option,value", [("TERRIO_MAX_BANDS", "1"), ("TERRIO_MAX_DECLARED_BYTES", "4096")]
)
@pytest.mark.parametrize("surface", ["native", "rio", "bytes"])
def test_tile_band_limits_precede_invalid_pixels(
    coverage, tmp_path, gdal, monkeypatch, tile_format, option, value, surface
):
    """Real tile headers determine the expanded count without decoding their corrupt payload."""
    palette = tile_format == "palette-png"
    source = gdal.GetDriverByName("MEM").Create("", 1, 1, 1 if palette else 3)
    if palette:
        colors = gdal.ColorTable()
        colors.SetColorEntry(0, (1, 2, 3, 255))
        source.GetRasterBand(1).SetRasterColorTable(colors)
    png = tile_format.endswith("png")
    path = tmp_path / ("tile.png" if png else "tile.tif")
    encoded = gdal.GetDriverByName("PNG" if png else "GTiff").CreateCopy(
        str(path), source, options=[] if png else ["INTERLEAVE=PIXEL"]
    )
    assert encoded is not None
    encoded = None
    payload = bytearray(path.read_bytes())
    if png:
        idat = payload.index(b"IDAT")
        payload[idat + 4] ^= 0xFF
    else:
        order = "<" if payload[:2] == b"II" else ">"
        ifd = struct.unpack_from(order + "I", payload, 4)[0]
        count = struct.unpack_from(order + "H", payload, ifd)[0]
        entry = next(
            i
            for i in range(ifd + 2, ifd + 2 + count * 12, 12)
            if struct.unpack_from(order + "H", payload, i)[0] == 273
        )
        assert struct.unpack_from(order + "I", payload, entry + 4)[0] == 1
        kind = struct.unpack_from(order + "H", payload, entry + 2)[0]
        struct.pack_into(
            order + ("H" if kind == 3 else "I"), payload, entry + 8, len(payload) + 1024
        )
    with sqlite3.connect(coverage) as database:
        database.execute("UPDATE grid SET tile_data=?", (bytes(payload),))
    monkeypatch.delenv("TERRIO_MAX_BANDS", raising=False)
    monkeypatch.delenv("TERRIO_MAX_DECLARED_BYTES", raising=False)
    with pytest.raises(terrio.InvalidDataError):
        terrio.open_raster(str(coverage))
    monkeypatch.setenv(option, value)
    with pytest.raises(terrio.ResourceLimitError, match=option):
        if surface == "rio":
            with rio.open(coverage):
                pass
        elif surface == "bytes":
            terrio.open_bytes(coverage.read_bytes(), filename="coverage.gpkg")
        else:
            terrio.open_raster(str(coverage))


@pytest.mark.skipif(sys.platform != "linux", reason="uses the Linux usable-memory policy")
@pytest.mark.parametrize("with_tile", [True, False], ids=["tile", "empty"])
def test_coverage_working_memory_refusal_reaches_python(coverage, with_tile):
    """A 100 MB Float32 result fits a 128 MiB policy; its 200 MB working grid does not."""
    with sqlite3.connect(coverage) as database:
        database.execute("UPDATE gpkg_contents SET max_x=5000,min_y=-4984")
        database.execute("UPDATE gpkg_tile_matrix_set SET max_x=5120,min_y=-5104")
        database.execute("UPDATE gpkg_tile_matrix SET matrix_width=20,matrix_height=20")
        if not with_tile:
            database.execute("DELETE FROM grid")
    script = """
import resource
import sys
import terrio
_, hard = resource.getrlimit(resource.RLIMIT_RSS)
resource.setrlimit(resource.RLIMIT_RSS, (min(hard, 128 << 20) if hard >= 0 else 128 << 20, hard))
try:
    terrio.open_raster(sys.argv[1])
except terrio.ResourceLimitError as error:
    assert "GeoPackage raster working set" in str(error), str(error)
else:
    raise AssertionError("unchecked Float64 coverage grid")
"""
    environment = dict(os.environ)
    for option in ("TERRIO_MAX_DECLARED_BYTES", "TERRIO_MAX_BANDS"):
        environment.pop(option, None)
    result = subprocess.run(
        [sys.executable, "-c", script, str(coverage)],
        env=environment,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stdout + result.stderr
