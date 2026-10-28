"""Footprint catalogs preserve geometry, source identity and GTI metadata."""

import numpy as np
import pytest

import terrio


def test_raster_catalog_geometry_and_metadata(tmp_path):
    raster = terrio.Raster.from_array(
        np.zeros((6, 4), dtype="uint8"), (10, 2, 0.5, 20, 0.25, -3), crs="EPSG:4326"
    )
    index = terrio.raster_index(
        {"tile.tif": raster},
        source_crs_field_name="srs",
        min_pixel_size=2.7,
        fetch_metadata=[("{PIXEL_SIZE}", "pixel_size", "Real")],
        band_count=1,
        ot="Byte",
        nodata=[255],
    )
    for options in ({"nodata": [1, 2, 3]}, {"color_interpretations": ["red", "green", "blue"]}):
        with pytest.raises(terrio.InvalidArgumentError, match="inferred"):
            terrio.raster_index({"tile.tif": raster}, **options)
    assert index.count == 1
    assert index.metadata["NODATA"] == "255"
    feature = index.layer.to_geojson()["features"][0]
    assert feature["properties"]["srs"] == "EPSG:4326"
    assert feature["properties"]["pixel_size"] == pytest.approx((209 / 24) ** 0.5)
    assert feature["geometry"]["coordinates"][0] == [[10, 20], [18, 21], [21, 3], [13, 2], [10, 20]]
    for suffix in ("gpkg", "fgb"):
        data = index.to_bytes(f"index.{suffix}")
        assert len(data) > 100
    assert len(terrio.raster_index({"tile.tif": raster}).to_bytes("index.shp.zip")) > 100
    path = str(tmp_path / "index.gpkg")
    index.write(path)
    before = (tmp_path / "index.gpkg").read_bytes()
    with pytest.raises(terrio.InvalidArgumentError):
        index.write(path)
    assert (tmp_path / "index.gpkg").read_bytes() == before
    index.write(path, overwrite=True)


def test_native_discovery_and_source_protection(tmp_path):
    path = str(tmp_path / "source.tif")
    terrio.create_raster(path, size=(4, 4), burn=[7], crs="EPSG:4326", bbox=(0, 0, 4, 4))
    index = terrio.raster_index([str(tmp_path)], filename_filters=["*.tif"], absolute_path=True)
    assert index.count == 1
    assert index.layer.to_geojson()["features"][0]["properties"]["location"] == path
    before = (tmp_path / "source.tif").read_bytes()
    with pytest.raises(terrio.InvalidArgumentError):
        index.write(path, format="GPKG", overwrite=True)
    assert (tmp_path / "source.tif").read_bytes() == before


def test_vector_catalog_selection_and_validation():
    geojson = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"x": 1},
                "geometry": {"type": "LineString", "coordinates": [[1, 2], [4, 6]]},
            }
        ],
    }
    index = terrio.vector_index({"roads.json": geojson}, source_layer_indexes=[0])
    feature = index.layer.to_geojson()["features"][0]
    assert feature["properties"]["location"] == "roads.json,0"
    assert feature["geometry"]["coordinates"][0] == [[1, 2], [1, 6], [4, 6], [4, 2], [1, 2]]
    for options in ({"source_layer_indexes": [9]}, {"min_pixel_size": 1}, {"typo": True}):
        with pytest.raises(terrio.InvalidArgumentError):
            terrio.vector_index({"roads.json": geojson}, **options)


def test_real_dem_catalog_is_readable_as_gti(data_dir, gdal, tmp_path):
    dem = data_dir / "usgs_dem.tif"
    if not dem.exists():
        dem = next(data_dir.glob("USGS*.tif"))
    path = str(tmp_path / "catalog.fgb")
    index = terrio.raster_index([str(dem)], path, absolute_path=True, band_count=1, ot="Float32")
    assert index.count == 1
    reference = gdal.Open(str(dem))
    catalog = gdal.Open("GTI:" + path)
    assert (catalog.RasterXSize, catalog.RasterYSize) == (
        reference.RasterXSize,
        reference.RasterYSize,
    )
    np.testing.assert_array_equal(
        catalog.ReadAsArray(0, 0, 8, 8), reference.ReadAsArray(0, 0, 8, 8)
    )
