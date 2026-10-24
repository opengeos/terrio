"""Catalog APIs on in-memory sources, including the WASM-compatible input model."""

from collections import UserDict
from types import MappingProxyType

import numpy as np
import pytest

import terrio


def index():
    return {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"location": name, "z": z},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[[x, 0], [x + 4, 0], [x + 4, 4], [x, 4], [x, 0]]],
                },
            }
            for name, x, z in [("left", 0, 2), ("right", 2, 1), ("missing", 100, 3)]
        ],
    }


def test_catalog_windows_and_sort_order():
    left = terrio.Raster.from_array(
        np.full((4, 4), 10, dtype="float32"), (0, 1, 0, 4, 0, -1), crs="EPSG:4326"
    )
    right = terrio.Raster.from_array(
        np.full((4, 4), 20, dtype="float32"), (2, 1, 0, 4, 0, -1), crs="EPSG:4326"
    )
    catalog = terrio.open_catalog(
        index(),
        {"left": left, "right": right},
        tr=(1, 1),
        te=(0, 0, 6, 4),
        sort_field="z",
        cache_size=1,
    )
    assert (catalog.width, catalog.height, catalog.count) == (6, 4, 1)
    np.testing.assert_array_equal(catalog.read(1)[0], [10, 10, 10, 10, 20, 20])
    np.testing.assert_array_equal(catalog.read(1, window=(4, 0, 2, 2)), np.full((2, 2), 20))
    # The unselected missing source is never opened.
    assert left.read(1)[0, 0] == 10
    with pytest.raises(ValueError, match="positive"):
        terrio.open_catalog(index(), {"left": left}, cache_size=0)


def test_catalog_coverage_mask():
    left = terrio.Raster.from_array(
        np.full((4, 4), 10, dtype="float32"), (0, 1, 0, 4, 0, -1), crs="EPSG:4326"
    )
    catalog = terrio.open_catalog(
        index(), {"left": left}, tr=(1, 1), te=(0, 0, 8, 4), bands=1, ot="Float32"
    )
    # The unavailable right tile is outside this mask window.
    np.testing.assert_array_equal(catalog.read_mask(window=(0, 0, 2, 2)), np.full((2, 2), 255))
    np.testing.assert_array_equal(catalog.read_mask(window=(6, 0, 2, 2)), np.zeros((2, 2)))


def test_named_layer_in_memory_index(tmp_path):
    left = terrio.Raster.from_array(
        np.full((4, 4), 10, dtype="float32"), (0, 1, 0, 4, 0, -1), crs="EPSG:4326"
    )
    selected = terrio.Layer.from_geojson(index(), name="selected")
    other = terrio.Layer.from_geojson(index(), name="other")
    path = tmp_path / "multiple.gpkg"
    terrio.write_vector([other, selected], str(path))
    dataset = terrio.open_vector(str(path))
    for supplied in [dataset, [other, selected]]:
        catalog = terrio.open_catalog(
            supplied,
            {"left": left},
            layer="selected",
            tr=(1, 1),
            te=(0, 0, 4, 4),
            bands=1,
            ot="Float32",
        )
        np.testing.assert_array_equal(catalog.read(1, window=(0, 0, 1, 1)), [[10]])


def test_band_subsets_keep_order_duplicates_and_alpha():
    footprints = index()
    footprints["features"] = footprints["features"][:1]
    values = np.stack([np.full((4, 4), band * 10, dtype="float64") for band in range(1, 6)])
    alpha = np.full((1, 4, 4), 255, dtype="float64")
    alpha[0, 0, 0] = 0
    source = terrio.Raster.from_array(
        np.concatenate([values, alpha]), (0, 1, 0, 4, 0, -1), crs="EPSG:4326"
    )
    catalog = terrio.open_catalog(
        footprints, {"left": source}, tr=(1, 1), te=(0, 0, 4, 4), bands=6, alpha_band=6
    )
    selected = catalog.read([4, 2, 4])
    assert selected.dtype == np.dtype("uint8")  # GTI defaults to Byte without a tile inspection.
    for output, value in zip(selected, [40, 20, 40], strict=True):
        np.testing.assert_array_equal(output[0], [0, value, value, value])
        np.testing.assert_array_equal(output[1:], np.full((3, 4), value))
    np.testing.assert_array_equal(catalog.read_mask()[0], [0, 255, 255, 255])

    untagged = terrio.open_catalog(
        footprints, {"left": source}, tr=(1, 1), te=(0, 0, 4, 4), bands=5
    )
    np.testing.assert_array_equal(untagged.read(3), np.full((4, 4), 30))


@pytest.mark.parametrize(
    "mapping",
    [
        dict,
        pytest.param(MappingProxyType, id="mapping-proxy"),
        pytest.param(UserDict, id="user-dict"),
    ],
)
def test_supplied_lazy_sources_and_paths_are_deferred(tmp_path, mapping):
    path = tmp_path / "source.tif"
    source = terrio.Raster.from_array(
        np.full((4, 4), 10, dtype="uint8"), (0, 1, 0, 4, 0, -1), crs="EPSG:4326"
    )
    source.write(str(path))
    lazy = terrio.open_raster(str(path))
    for left in [lazy, path]:
        catalog = terrio.open_catalog(
            index(),
            mapping({"left": left, "right": tmp_path / "missing.tif"}),
            tr=(1, 1),
            te=(0, 0, 6, 4),
            bands=1,
        )
        np.testing.assert_array_equal(catalog.read(1, window=(0, 0, 1, 1)), [[10]])
    np.testing.assert_array_equal(lazy.read(1, window=(0, 0, 1, 1)), [[10]])


@pytest.mark.parametrize("mode", ["physical-index", "supplied-vrt", "supplied-path"])
def test_nested_vrt_catalog_overwrite_reads_source_before_create(tmp_path, mode):
    path = tmp_path / "source.tif"
    terrio.Raster.from_array(
        np.full((4, 4), 10, dtype="float32"), (0, 1, 0, 4, 0, -1), crs="EPSG:4326"
    ).write(str(path))

    def description(name):
        return (
            '<VRTDataset rasterXSize="4" rasterYSize="4"><SRS>EPSG:4326</SRS>'
            '<GeoTransform>0,1,0,4,0,-1</GeoTransform><VRTRasterBand dataType="Float32">'
            f'<SimpleSource><SourceFilename relativeToVRT="1">{name}</SourceFilename>'
            "<SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"
        )

    child = tmp_path / "child.vrt"
    outer = tmp_path / "outer.vrt"
    child.write_text(description("source.tif"))
    outer.write_text(description("child.vrt"))
    footprints = index()
    footprints["features"] = footprints["features"][:1]
    if mode == "physical-index":
        footprints["features"][0]["properties"]["location"] = "outer.vrt"
        index_path = tmp_path / "index.gti.gpkg"
        terrio.write_vector(terrio.Layer.from_geojson(footprints), str(index_path))
        catalog = terrio.open_catalog(
            str(index_path), tr=(1, 1), te=(0, 0, 4, 4), bands=1, ot="Float32"
        )
    else:
        source = terrio.open_raster(str(outer), lazy=True) if mode == "supplied-vrt" else outer
        catalog = terrio.open_catalog(
            footprints, {"left": source}, tr=(1, 1), te=(0, 0, 4, 4), bands=1, ot="Float32"
        )
    catalog.write(str(path))
    np.testing.assert_array_equal(terrio.open_raster(str(path)).read(1), np.full((4, 4), 10))


def test_vrt_subdataset_inventory_protects_physical_container(tmp_path):
    gdal = pytest.importorskip("osgeo.gdal")
    source = gdal.GetDriverByName("MEM").Create("", 4, 4, 1, gdal.GDT_Float32)
    source.SetGeoTransform((0, 1, 0, 4, 0, -1))
    osr = pytest.importorskip("osgeo.osr")
    srs = osr.SpatialReference()
    srs.ImportFromEPSG(4326)
    source.SetSpatialRef(srs)
    source.GetRasterBand(1).Fill(10)
    path = tmp_path / "source.nc"
    gdal.Translate(str(path), source, format="netCDF")
    xml = (
        '<VRTDataset rasterXSize="4" rasterYSize="4"><SRS>EPSG:4326</SRS>'
        "<GeoTransform>0,1,0,4,0,-1</GeoTransform>"
        '<VRTRasterBand dataType="Float32"><SimpleSource>'
        f'<SourceFilename>NETCDF:"{path}":Band1</SourceFilename><SourceBand>1</SourceBand>'
        "</SimpleSource></VRTRasterBand></VRTDataset>"
    )
    vrt = terrio.open_vrt(xml, {})
    footprints = index()
    footprints["features"] = footprints["features"][:1]
    catalog = terrio.open_catalog(
        footprints, {"left": vrt}, tr=(1, 1), te=(0, 0, 4, 4), bands=1, ot="Float32"
    )
    catalog.write(str(path), driver="GTiff")
    np.testing.assert_array_equal(gdal.Open(str(path)).ReadAsArray(), np.full((4, 4), 10))
