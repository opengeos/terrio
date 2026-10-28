"""Tests of tile archives (PMTiles, MBTiles): rasters and vector layers written and read back, and
archives written by GDAL compared with GDAL's Python bindings when they are installed."""

import numpy as np
import pytest

import terrio

# Half the width of the Web Mercator world, and the resolution of zoom level 11.
MAX_GM = 20037508.342789244
RES_Z11 = 2 * MAX_GM / 256 / 2048
# A 512 x 256 raster on the tiles of zoom level 11.
TRANSFORM = (
    -MAX_GM + 1000 * 256 * RES_Z11,
    RES_Z11,
    0.0,
    MAX_GM - 700 * 256 * RES_Z11,
    0.0,
    -RES_Z11,
)

PLACES = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"name": name, "rank": rank},
            "geometry": {"type": "Point", "coordinates": [x, y]},
        }
        for name, rank, x, y in [
            ("sf", 1, -122.4, 37.8),
            ("paris", 2, 2.35, 48.85),
            ("tokyo", 3, 139.7, 35.7),
        ]
    ],
}


def rgb_array():
    """A 3-band Byte array with a pattern.

    Returns:
        numpy.ndarray: The (bands, rows, cols) array.
    """
    y, x = np.mgrid[0:256, 0:512]
    return np.stack([(x * 3 + y * 5 + b * 70) % 251 for b in range(3)]).astype("uint8")


@pytest.mark.parametrize("ext", ["pmtiles", "mbtiles"])
def test_raster_tiles_round_trip(tmp_path, ext):
    data = rgb_array()
    r = terrio.Raster.from_array(data, TRANSFORM, crs="EPSG:3857")
    path = tmp_path / f"rgb.{ext}"
    r.write(str(path))
    lazy = terrio.open_raster(str(path), lazy=True)
    assert lazy.is_lazy
    assert (lazy.width, lazy.height) == (512, 256)
    assert lazy.count == 4  # with an alpha band
    assert lazy.crs == "EPSG:3857"
    assert lazy.overviews == [(256, 128)]
    assert lazy.transform == pytest.approx(TRANSFORM)
    np.testing.assert_array_equal(lazy.read(2, window=(100, 50, 64, 32)), data[1, 50:82, 100:164])
    assert lazy.read(overview=0).shape[1:] == (128, 256)
    # From bytes too.
    back = terrio.open_bytes(path.read_bytes(), f"x.{ext}", lazy=True)
    np.testing.assert_array_equal(back.read(1), data[0])
    info = terrio.info(str(path))
    assert info["driver"] in ("PMTiles", "MBTiles")


@pytest.mark.parametrize("ext", ["pmtiles", "mbtiles"])
@pytest.mark.parametrize("resampling", ["NEAREST", "BILINEAR", "AVERAGE"])
def test_lazy_rasters_are_tiled_as_in_memory_ones(tmp_path, ext, resampling):
    # A raster opened lazily is tiled a tile row at a time (`Raster.write` and `convert`): the
    # archive is the one written from the raster in memory, byte for byte.
    data = rgb_array()
    diagonals = np.arange(512)[None, :] + np.arange(256)[:, None]
    alpha = np.where(diagonals % 97 < 9, 0, 255).astype("uint8")
    rgba = np.concatenate([data, alpha[None]])
    tif = tmp_path / "rgba.tif"
    terrio.Raster.from_array(rgba, TRANSFORM, crs="EPSG:3857").write(str(tif), tile_size=256)
    options = {"RESAMPLING": resampling, "MAXZOOM": "11", "NAME": "t"}
    whole, lazy = tmp_path / f"whole.{ext}", tmp_path / f"lazy.{ext}"
    terrio.open_raster(str(tif)).write(str(whole), options=options)
    terrio.open_raster(str(tif), lazy=True).write(str(lazy), options=options)
    assert lazy.read_bytes() == whole.read_bytes()
    converted = tmp_path / "c" / f"rgba.{ext}"
    converted.parent.mkdir()
    terrio.convert(str(tif), str(converted))
    reference = tmp_path / f"rgba.{ext}"
    terrio.open_raster(str(tif)).write(str(reference))
    assert converted.read_bytes() == reference.read_bytes()
    assert not [p for p in tmp_path.rglob("*.tmp")]


@pytest.mark.parametrize("ext", ["pmtiles", "mbtiles"])
def test_vector_tiles_round_trip(tmp_path, ext):
    path = tmp_path / f"places.{ext}"
    terrio.write_vector(PLACES, str(path))
    ds = terrio.open_vector(str(path))
    layer = ds[0]
    assert layer.name == "places"
    assert len(layer) == 3
    assert layer.crs == "EPSG:3857"
    assert sorted(layer.columns()["name"]) == ["paris", "sf", "tokyo"]
    assert terrio.list_layers(str(path)) == ["places"]


def test_drivers_list_tile_formats():
    names = {(d["name"], d["kind"]) for d in terrio.drivers()}
    assert {
        ("PMTiles", "raster"),
        ("PMTiles", "vector"),
        ("MBTiles", "raster"),
        ("MVT", "vector"),
    } <= names


def test_gdal_raster_mbtiles(data_dir, gdal):
    path = data_dir / "landsat.mbtiles"
    ours = terrio.open_raster(str(path), lazy=True)
    ref = gdal.Open(str(path))
    assert (ours.width, ours.height, ours.count) == (
        ref.RasterXSize,
        ref.RasterYSize,
        ref.RasterCount,
    )
    assert ours.transform == pytest.approx(ref.GetGeoTransform())
    window = (1000, 700, 300, 200)
    for b in range(1, 5):
        np.testing.assert_array_equal(
            ours.read(b, window=window), ref.GetRasterBand(b).ReadAsArray(*window)
        )
        np.testing.assert_array_equal(
            ours.read(b, overview=1, window=(300, 200, 64, 32)),
            ref.GetRasterBand(b).GetOverview(1).ReadAsArray(300, 200, 64, 32),
        )


@pytest.mark.parametrize("name", ["countries_mvt.mbtiles", "countries_mvt.pmtiles"])
def test_gdal_vector_tiles(data_dir, ogr, name):
    path = data_dir / name
    layer = terrio.open_vector(str(path))[0]
    src = ogr.Open(str(path))
    ref = src.GetLayer(0)
    features = list(ref)
    assert len(layer) == len(features)
    assert layer.name == ref.GetName()
    ours = sorted(layer.columns()["ADM0_A3"])
    theirs = sorted(f.GetField("ADM0_A3") for f in features)
    assert ours == theirs
