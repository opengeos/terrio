"""Minimal pole-corner and mirror-symmetry regression for Terrio issue 986."""

import numpy as np
import pytest
from test_remote_reads import Server

import terrio
from terrio.rio import Env

METHODS = ["average", "min", "max", "mode", "rms", "med", "q1", "q3"]


def test_nonpolar_float_mode_selects_the_greatest_overlap():
    """A first-seen float value may lead when its source cell has greater overlap."""
    source = terrio.Raster.from_array(
        np.array([[1.25, 2.5, 3.75]]), (0, 1, 0, 1, 0, -1), crs="EPSG:3857"
    )
    result = terrio.warp(
        source,
        t_srs="EPSG:3857",
        dst_transform=(0.9, 1.2, 0, 1, 0, -1),
        ts=(1, 1),
        resampling="mode",
        ot="Float64",
    ).read(1)
    # Cell overlaps are 0.1, 1.0 and 0.1 source pixels.
    np.testing.assert_array_equal(result, [[2.5]])


@pytest.fixture(scope="module")
def wave():
    """Create a longitude-only field with known meridian symmetry.

    Returns:
        Raster: Global one-degree field of 100 cos(center longitude).
    """
    values = np.repeat((100 * np.cos(np.radians(-179.5 + np.arange(360))))[None, :], 180, axis=0)
    return terrio.Raster.from_array(values, (-180, 1, 0, 90, 0, -1), crs="EPSG:4326")


@pytest.mark.parametrize("method", METHODS)
@pytest.mark.parametrize("invariant", [False, True])
@pytest.mark.parametrize("epsg", [3031, 3413])
def test_pole_corner_stays_in_longitude_sector(wave, method, invariant, epsg):
    """Constrain every pole-touching pixel to its physical longitude sector.

    Args:
        wave: Longitude-only source raster.
        method: Area resampling kernel.
        invariant: Grid sampling policy.
        epsg: North or south polar stereographic projection.
    """
    result = terrio.warp(
        wave,
        t_srs=f"EPSG:{epsg}",
        dst_transform=(-25000, 25000, 0, 25000, 0, -25000),
        ts=(2, 2),
        resampling=method,
        grid_invariant=invariant,
        ot="Float64",
        dstnodata=-999,
    ).read(1)
    # Sector boundaries coincide with source-cell edges, so positive-area
    # overlaps cannot borrow a center from another longitude sector.
    lower, upper = (
        (np.array([[0, 0], [-100, -100]]), np.array([[100, 100], [0, 0]]))
        if epsg == 3031
        else (
            np.array([[-100, -100 / np.sqrt(2)], [-100 / np.sqrt(2), 100 / np.sqrt(2)]]),
            np.array([[-100 / np.sqrt(2), 100 / np.sqrt(2)], [100 / np.sqrt(2), 100]]),
        )
    )
    if method == "rms":
        low_abs = np.where(lower * upper <= 0, 0, np.minimum(abs(lower), abs(upper)))
        lower, upper = low_abs, np.maximum(abs(lower), abs(upper))
    assert np.all(result >= lower - 1e-9), (method, epsg, result, lower)
    assert np.all(result <= upper + 1e-9), (method, epsg, result, upper)


@pytest.mark.parametrize("method", METHODS)
@pytest.mark.parametrize("invariant", [False, True])
@pytest.mark.parametrize("offset", [0, 1, 2, 5, 20])
def test_meridian_mirrors_have_equal_area_statistics(wave, method, invariant, offset):
    """Require equal statistics on mirror pixels below the South Pole.

    Args:
        wave: Meridian-symmetric source field.
        method: Area resampling kernel.
        invariant: Grid sampling policy.
        offset: Number of pixels below the pole.
    """
    result = terrio.warp(
        wave,
        t_srs="EPSG:3031",
        dst_transform=(-25000, 25000, 0, -offset * 25000, 0, -25000),
        ts=(2, 1),
        resampling=method,
        grid_invariant=invariant,
        ot="Float64",
        dstnodata=-999,
    ).read(1)
    if method == "mode" and offset == 0:
        # Both sectors have two equally weighted central longitude columns.
        # The first column in stored source order differs across the mirror;
        # a tied mode need not be reflection invariant.
        expected = 100 * np.cos(np.radians([[-135.5, 134.5]]))
        np.testing.assert_allclose(result, expected, rtol=0, atol=1e-9)
    else:
        np.testing.assert_allclose(result[:, 0], result[:, 1], rtol=0, atol=1e-9)


@pytest.mark.parametrize("invariant", [False, True])
@pytest.mark.parametrize("masked", [False, True])
def test_average_matches_normalized_overlap_away_from_pole(wave, invariant, masked):
    """Use the independent sum path as an overlap oracle off the singular corner.

    Args:
        wave: Source longitude field.
        invariant: Grid sampling policy.
        masked: Whether to omit selected source columns as nodata.
    """
    data = wave.read(1).copy()
    valid = np.ones_like(data)
    if masked:
        data[:, ::7] = np.nan
        valid[:, ::7] = np.nan
    source = terrio.Raster.from_array(data, (-180, 1, 0, 90, 0, -1), crs="EPSG:4326", nodata=np.nan)
    weights = terrio.Raster.from_array(
        valid, (-180, 1, 0, 90, 0, -1), crs="EPSG:4326", nodata=np.nan
    )
    options = dict(
        t_srs="EPSG:3031",
        dst_transform=(-25000, 25000, 0, -25000, 0, -25000),
        ts=(2, 1),
        ot="Float64",
        grid_invariant=invariant,
    )
    expected = terrio.warp(source, resampling="sum", **options).read(1) / terrio.warp(
        weights, resampling="sum", **options
    ).read(1)
    result = terrio.warp(source, resampling="average", **options).read(1)
    np.testing.assert_allclose(result, expected, rtol=1e-12, atol=1e-10)


def test_remote_polar_area_reads_only_its_source_strip(wave, tmp_path):
    """Correct footprint planning must retain lazy, bounded source range reads.

    Args:
        wave: Real source raster to write as an uncompressed tiled TIFF.
        tmp_path: Directory served by the loopback range server.
    """
    path = tmp_path / "source.tif"
    wave.write(str(path), compress="none", tile_size=32)
    options = dict(
        t_srs="EPSG:3031",
        dst_transform=(-25000, 25000, 0, 25000, 0, -25000),
        ts=(2, 2),
        resampling="average",
        ot="Float64",
    )
    expected = terrio.warp(wave, **options).read(1)
    server = Server(tmp_path)
    terrio.clear_http_cache()
    try:
        with Env(
            TERRIO_HTTP_CACHE_SIZE=0,
            TERRIO_HTTP_BLOCK_SIZE=16384,
            TERRIO_DISABLE_READDIR_ON_OPEN="EMPTY_DIR",
        ):
            result = terrio.warp(server.url + "/source.tif", **options).read(1)
        np.testing.assert_allclose(result, expected, rtol=1e-12, atol=1e-10)
        transfers = [row for row in server.log if row[0] == "GET" and row[2] < 300]
        assert transfers and all(row[1] and row[2] == 206 for row in transfers)
        assert server.bytes_sent() < path.stat().st_size / 4
    finally:
        server.close()
        terrio.clear_http_cache()
