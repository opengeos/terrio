"""Chunk-dependent windows include both stored edges across a polar longitude cut."""

import numpy as np
import pytest

import terrio
from terrio import _terrio
from terrio.rio import Env
from terrio.rio.enums import Resampling
from terrio.rio.transform import Affine
from terrio.rio.warp import reproject, transform

NORTH = [
    (0, 64, 65, 32, 64),
    (0, 64.5, 65, 32, 64),
    (0, 65, 64, 64, 32),
    (0, 65, 64.5, 64, 32),
    (-1, 65, 64, 64, 32),
    (-1, 66, 64, 64, 32),
    (-1, 66, 64.5, 64, 32),
    (-1, 66, 65, 64, 32),
]
SOUTH = [
    (0, 64, 62, 32, 32),
    (0, 64, 63, 32, 32),
    (-1, 63, 62, 64, 32),
    (-1, 63.5, 62, 64, 32),
    (-1, 63.5, 63, 64, 32),
    (-1, 64, 62, 64, 32),
    (-1, 64, 63, 64, 32),
]
CASES = [("EPSG:3413", case) for case in NORTH] + [("EPSG:3031", case) for case in SOUTH]
SOURCE = ((np.arange(200)[:, None] * 7 + np.arange(3600)[None, :]) % 250 + 1).astype("uint8")


def tile_grid(case):
    """Place one 32-pixel tile relative to a specified pole position.

    Args:
        case: Shear, pole column/row and tile column/row.

    Returns:
        Affine: Destination tile geotransform.
    """
    shear, u, v, tx, ty = case
    grid = Affine(25000, shear * 25000, -25000 * (u + shear * v), 0, -25000, 25000 * v)
    return grid * Affine.translation(tx, ty)


@pytest.mark.parametrize("crs,case", CASES)
@pytest.mark.parametrize("method", ["nearest", "average"])
@pytest.mark.parametrize("compatibility", ["3.12", "3.13"])
def test_polar_edge_cut_keeps_every_covered_center(crs, case, method, compatibility):
    """Cover meridian-adjacent centers on north-up and sheared grids at either pole.

    Args:
        crs: Northern or southern polar stereographic CRS.
        case: Pole position and destination tile placement.
        method: Point or area resampling.
        compatibility: Public kernel selector.
    """
    grid = tile_grid(case)
    north = 90 if crs == "EPSG:3413" else -70
    source_grid = Affine(0.1, 0, -180, 0, -0.1, north)
    columns, rows = np.meshgrid(np.arange(32) + 0.5, np.arange(32) + 0.5)
    longitudes, latitudes = transform(
        crs,
        "EPSG:4326",
        (grid.c + columns * grid.a + rows * grid.b).ravel().tolist(),
        (grid.f + columns * grid.d + rows * grid.e).ravel().tolist(),
    )
    source_rows = np.floor((north - np.asarray(latitudes).reshape(32, 32)) / 0.1)
    source_columns = (
        np.floor((np.asarray(longitudes).reshape(32, 32) + 180) / 0.1).astype(int) % 3600
    )
    covered = (source_rows >= 0) & (source_rows < 200)
    assert covered.any()
    with Env(TERRIO_GDAL_COMPAT=compatibility):
        planner = _terrio.RegionWarp(
            (3600, 200),
            source_grid.to_gdal(),
            "EPSG:4326",
            crs,
            grid.to_gdal(),
            "near" if method == "nearest" else method,
            0,
            "uint8",
            [None],
            0,
            grid_invariant=False,
        )
        sx, _, width, _ = planner.source_window((0, 0, 32, 32))
        assert (sx, width) == (0, 3600)
        ordinary = None
        for invariant, steps in ((False, None), (False, "ALL"), (True, None)):
            output = np.zeros((32, 32), dtype="uint8")
            options = {} if steps is None else {"SAMPLE_STEPS": steps}
            reproject(
                SOURCE,
                output,
                src_transform=source_grid,
                src_crs="EPSG:4326",
                dst_transform=grid,
                dst_crs=crs,
                resampling=Resampling[method],
                dst_nodata=0,
                tolerance=0,
                grid_invariant=invariant,
                **options,
            )
            assert np.all(output[covered] != 0)
            if not invariant:
                if ordinary is None:
                    ordinary = output.copy()
                else:
                    np.testing.assert_array_equal(output, ordinary)
            if method == "nearest":
                np.testing.assert_array_equal(
                    output[covered],
                    SOURCE[source_rows[covered].astype(int), source_columns[covered]],
                )


def test_cut_detection_with_invalid_orthographic_corners():
    """Retain coverage when invalid corners require dense edges and an interior grid."""
    crs = "+proj=ortho +lat_0=0 +lon_0=180 +datum=WGS84"
    radius = 6378137
    grid = Affine(radius / 16, 0, -radius, 0, -radius / 16, radius)
    columns, rows = np.meshgrid(np.arange(32) + 0.5, np.arange(32) + 0.5)
    lon, lat = transform(
        crs,
        "EPSG:4326",
        (grid.c + columns * grid.a).ravel().tolist(),
        (grid.f + rows * grid.e).ravel().tolist(),
    )
    covered = (np.isfinite(lon) & np.isfinite(lat)).reshape(32, 32)
    assert 0 < covered.sum() < covered.size
    output = np.zeros((32, 32), dtype="uint8")
    reproject(
        np.ones((180, 360), dtype="uint8"),
        output,
        src_transform=Affine(1, 0, -180, 0, -1, 90),
        src_crs="EPSG:4326",
        dst_transform=grid,
        dst_crs=crs,
        dst_nodata=0,
        grid_invariant=False,
        resampling=Resampling.nearest,
    )
    assert np.all(output[covered] == 1)
    assert np.all(output[~covered] == 0)


def test_polar_edge_remote_window_keeps_reads_bounded(tmp_path, record_property):
    """Measure a two-edge source window without fetching the whole global raster.

    Args:
        tmp_path: Temporary raster directory.
        record_property: Record observed HTTP traffic.
    """
    rasterio = pytest.importorskip("rasterio")
    from rasterio.shutil import copy
    from test_remote_reads import Server

    path, original = tmp_path / "world.tif", tmp_path / "original.tif"
    values = np.random.default_rng(11).integers(1, 1000, (1800, 3600), dtype="uint16")
    with rasterio.open(
        original,
        "w",
        driver="GTiff",
        width=3600,
        height=1800,
        count=1,
        dtype="uint16",
        crs="EPSG:4326",
        transform=Affine(0.1, 0, -180, 0, -0.1, 90),
    ) as target:
        target.write(values, 1)
    copy(original, path, driver="COG", blocksize=128, compress="deflate")
    server = Server(tmp_path)
    terrio.clear_http_cache()
    try:
        source = terrio.open_raster(server.url + "/world.tif", lazy=True)
        assert source.is_lazy
        output = terrio.warp(
            source,
            t_srs="EPSG:3413",
            dst_transform=tile_grid(NORTH[0]).to_gdal(),
            ts=(32, 32),
            resampling="near",
            grid_invariant=False,
            overview="NONE",
            dstnodata=0,
        ).read(1)
        assert np.all(output != 0)
        transferred = server.bytes_sent()
        record_property("bytes_transferred", transferred)
        record_property("file_bytes", path.stat().st_size)
        record_property("requests", len(server.log))
        assert transferred < path.stat().st_size / 2
        assert all(row[2] == 206 for row in server.log if row[0] == "GET" and row[2] != 404)
    finally:
        server.close()
        terrio.clear_http_cache()
