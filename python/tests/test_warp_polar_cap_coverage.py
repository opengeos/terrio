"""Protect polar-cap coverage restored by grid-invariant warps (#825, #847)."""

import subprocess
import sys

import numpy as np
import pytest

import terrio
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_origin
from terrio.rio.warp import reproject, transform


def warp_array(source, source_transform, crs, grid):
    """Reproject an array into the requested grid using the public default policy.

    Args:
        source: Nonzero source samples.
        source_transform: Source affine transform.
        crs: Destination CRS.
        grid: Left, top, pixel size, width and height.

    Returns:
        ndarray: Destination samples, with zero identifying unwritten pixels.
    """
    left, top, size, width, height = grid
    output = np.zeros((height, width), dtype="uint8")
    reproject(
        source,
        output,
        src_transform=source_transform,
        src_crs="EPSG:4326",
        dst_transform=from_origin(left, top, size, size),
        dst_crs=crs,
        resampling=Resampling.nearest,
        dst_nodata=0,
    )
    return output


@pytest.mark.parametrize(
    "crs,north,south,grid,covered",
    [
        ("EPSG:3413", 90, 30, (-3850000, 5850000, 25000, 304, 448), 136192),
        ("EPSG:3976", -60, -90, (-3950000, 4350000, 25000, 316, 332), 55512),
        ("EPSG:3035", 65, 35, (2321000, 5210000, 25000, 160, 160), 22040),
        ("EPSG:32633", 65, 35, (0, 7000000, 25000, 40, 120), 4800),
    ],
)
def test_source_covered_centres_are_written_by_api_and_cli(
    tmp_path, monkeypatch, crs, north, south, grid, covered
):
    """Polar interiors and ordinary projected controls retain every covered centre.

    Args:
        tmp_path: Temporary raster directory.
        monkeypatch: Scoped configuration changes.
        crs: Destination projection.
        north: Source's northern latitude.
        south: Source's southern latitude.
        grid: Destination origin, resolution and shape.
        covered: Expected number of centres within the source's latitude range.
    """
    monkeypatch.delenv("TERRIO_WARP_GRID", raising=False)
    source = np.repeat(np.arange(1, north - south + 1, dtype="uint8")[:, None], 360, axis=1)
    source_transform = from_origin(-180, north, 1, 1)
    left, top, size, width, height = grid
    columns, rows = np.meshgrid(np.arange(width) + 0.5, np.arange(height) + 0.5)
    _, latitudes = transform(
        crs,
        "EPSG:4326",
        (left + columns * size).ravel().tolist(),
        (top - rows * size).ravel().tolist(),
    )
    latitudes = np.asarray(latitudes).reshape(height, width)
    inside = (latitudes <= north) & (latitudes >= south)
    assert int(inside.sum()) == covered
    direct = warp_array(source, source_transform, crs, grid)
    assert np.count_nonzero(inside & (direct == 0)) == 0

    path, output = tmp_path / "source.tif", tmp_path / "warped.tif"
    terrio.Raster.from_array(source, source_transform.to_gdal(), crs="EPSG:4326").write(str(path))
    subprocess.run(
        [
            sys.executable,
            "-m",
            "terrio",
            "warp",
            str(path),
            str(output),
            "--t-srs",
            crs,
            "--te",
            str(left),
            str(top - height * size),
            str(left + width * size),
            str(top),
            "--tr",
            str(size),
            str(size),
            "-r",
            "near",
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=60,
    )
    cli = terrio.open_raster(str(output))
    assert cli.transform == pytest.approx(from_origin(left, top, size, size).to_gdal())
    pixels = cli.read(1)
    assert pixels.shape == direct.shape
    assert pixels.dtype == source.dtype
    assert np.count_nonzero(inside & (pixels == 0)) == 0
    np.testing.assert_array_equal(pixels, direct)

    column, row = round(-left / size), round(top / size)
    if 0 < column < width and 0 < row < height:
        parts = np.zeros_like(direct)
        for y0, y1 in [(0, row), (row, height)]:
            for x0, x1 in [(0, column), (column, width)]:
                tile_grid = (left + x0 * size, top - y0 * size, size, x1 - x0, y1 - y0)
                parts[y0:y1, x0:x1] = warp_array(source, source_transform, crs, tile_grid)
        assert np.count_nonzero(inside & (parts == 0)) == 0
        np.testing.assert_array_equal(parts, direct)
