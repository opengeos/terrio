"""Regional geographic rasters are readable at equivalent longitude numberings."""

import json
import math

import numpy as np
import pytest
from test_warp_longitude_period import METHODS

import terrio
from terrio.rio.warp import transform

RADIUS = 6378137.0


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("method,expected", [("average", 349.0), ("sum", 279200.0)])
def test_regional_cut_window_contains_the_continuous_footprint(invariant, method, expected):
    """Include early source columns when sampled bounds take the long way around a cut.

    Args:
        invariant: Stable or chunk-dependent grid policy.
        method: Normalized or extensive area resampling method.
        expected: Independent result for the 40 by 20 intersected source pixels.
    """
    rows, cols = np.indices((200, 40))
    source = terrio.Raster.from_array(
        (1 + cols + 3 * rows).astype(float), (180, 0.05, 0, 10, 0, -0.05), crs="EPSG:4326"
    )
    xs, ys = transform("EPSG:4326", "EPSG:3832", [179.625, 219.625], [5, 4])
    grid = (xs[0], xs[1] - xs[0], 0, ys[0], 0, ys[1] - ys[0])
    actual = warped(source, "EPSG:3832", grid, (1, 1), method, invariant)
    assert actual[0, 0] == pytest.approx(expected, abs=1e-5 if method == "sum" else 1e-9)


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("kernel", ["3.12", "3.13"])
@pytest.mark.parametrize("method", ["bilinear", "cubic", "cubicspline", "lanczos"])
@pytest.mark.parametrize("extent", [(-2, 0, 6, 6), (6, 0, 14, 6), (-2, 0, 14, 6)])
def test_unshifted_regional_filter_scales_keep_off_source_margins(
    invariant, kernel, method, extent
):
    """Keep ordinary affine sampling unchanged when no longitude shift is needed.

    Args:
        invariant: Stable or chunk-dependent grid policy.
        kernel: Reference GDAL kernel version.
        method: Interpolating point resampling method.
        extent: Output bounds overlapping a source edge.
    """
    values = np.resize([0.0, 100.0, 4.0, 80.0, 0.0, 15.0, 3.0], (6, 12))
    outputs = []
    for crs in ("EPSG:4326", "EPSG:3857"):
        source = terrio.Raster.from_array(values, (0, 1, 0, 6, 0, -1), crs=crs)
        outputs.append(
            terrio.warp(
                source,
                t_srs=crs,
                te=extent,
                ts=(4, 3),
                resampling=method,
                kernel_version=kernel,
                grid_invariant=invariant,
                ot="Float64",
                dstnodata=-999,
            ).read(1)
        )
    np.testing.assert_allclose(outputs[0], outputs[1], rtol=0, atol=1e-10)


def warped(source, crs, grid, size, method, invariant):
    """Warp a source onto a prescribed grid with an explicit missing-data value.

    Args:
        source: Raster or raster list.
        crs: Target coordinate reference system.
        grid: Six-element target geotransform.
        size: Target width and height.
        method: Resampling method.
        invariant: Whether kernel scales are independent of output windows.

    Returns:
        numpy.ndarray: Warped Float64 band.
    """
    return np.asarray(
        terrio.warp(
            source,
            t_srs=crs,
            dst_transform=grid,
            ts=size,
            resampling=method,
            ot="Float64",
            dstnodata=-999,
            grid_invariant=invariant,
        ).read(1)
    )


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("method", METHODS)
@pytest.mark.parametrize("target", ["EPSG:4326", "EPSG:3832"])
def test_pacific_regional_source_is_read_past_180(method, invariant, target):
    """Read both halves of a source stored from 160 to 200 degrees east.

    Args:
        method: Resampling method.
        invariant: Stable or chunk-dependent grid policy.
        target: Geographic world or Pacific Mercator target.
    """
    values = np.where(np.arange(40) >= 20, 2.0, 1.0)[None, :].repeat(40, axis=0)
    source = terrio.Raster.from_array(values, (160, 1, 0, 0, 0, -1), crs="EPSG:4326")
    if target == "EPSG:4326":
        actual = warped(source, target, (-180, 1, 0, 0, 0, -1), (360, 40), method, invariant)
        west, east = actual[:, 340:], actual[:, :20]
        assert np.all(actual[:, 20:340] == -999)
    else:
        grid = (
            RADIUS * math.radians(10),
            RADIUS * math.radians(1),
            0,
            0,
            0,
            RADIUS * math.log(math.tan(math.radians(25))) / 40,
        )
        actual = warped(source, target, grid, (40, 40), method, invariant)
        west, east = actual[:, :20], actual[:, 20:]
    assert not np.any(west == -999)
    assert not np.any(east == -999)
    if method != "sum":
        np.testing.assert_allclose(west[:, 5:15], 1, rtol=0, atol=1e-9)
        np.testing.assert_allclose(east[:, 5:15], 2, rtol=0, atol=1e-9)
    else:
        assert np.all(west > 0)
        assert np.all(east > 0)


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("method", METHODS)
def test_rounded_full_world_keeps_its_true_small_gap(method, invariant):
    """A near-full world shifts by its actual angular turn, not by its stored width.

    Args:
        method: Resampling method.
        invariant: Stable or chunk-dependent grid policy.
    """
    resolution = 0.0833333333
    source = terrio.Raster.from_array(
        np.ones((4, 4320)), (0, resolution, 0, 2 * resolution, 0, -resolution), crs="EPSG:4326"
    )
    top = RADIUS * math.log(math.tan(math.pi / 4 + math.radians(resolution)))
    grid = (-math.pi * RADIUS, 2 * math.pi * RADIUS / 360, 0, top, 0, -top)
    actual = warped(source, "EPSG:3857", grid, (360, 2), method, invariant)
    assert not np.any(actual == -999)
    if method != "sum":
        np.testing.assert_allclose(actual, 1, rtol=0, atol=1e-9)


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("method", METHODS)
def test_west_tile_owns_the_meridian_and_east_right_edge_stays_excluded(method, invariant):
    """Use the west tile's first column at +180 while excluding the east tile's end.

    Args:
        method: Point resampling method.
        invariant: Stable or chunk-dependent grid policy.
    """
    west = terrio.Raster.from_array(
        np.full((30, 10), 3.0), (-180, 1, 0, 90, 0, -1), crs="EPSG:4326"
    )
    east = terrio.Raster.from_array(np.full((30, 10), 7.0), (170, 1, 0, 90, 0, -1), crs="EPSG:4326")
    grid, size = (-2987500, 25000, 0, 2987500, 0, -25000), (240, 40)
    cols, rows = np.meshgrid(np.arange(size[0]) + 0.5, np.arange(size[1]) + 0.5)
    lon, _ = transform(
        "EPSG:3995",
        "EPSG:4326",
        (grid[0] + cols * grid[1]).ravel().tolist(),
        (grid[3] + rows * grid[5]).ravel().tolist(),
    )
    meridian = np.abs(np.asarray(lon).reshape(size[1], size[0])) == 180
    assert np.count_nonzero(meridian) == 40
    actual = warped([west, east], "EPSG:3995", grid, size, method, invariant)
    assert not np.any(actual[meridian] == -999)
    if method in ("near", "bilinear", "cubic", "cubicspline", "lanczos"):
        np.testing.assert_allclose(actual[meridian], 3, rtol=0, atol=1e-9)
        assert np.all(warped(east, "EPSG:3995", grid, size, method, invariant)[meridian] == -999)
    elif method != "sum":
        assert np.all((actual[meridian] >= 3 - 1e-9) & (actual[meridian] <= 7 + 1e-9))
    else:
        assert np.all(actual[meridian] > 0)


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("method", ["near", "bilinear", "cubic", "average"])
def test_pacific_source_fills_the_eastern_web_map_tile(method, invariant):
    """Fill the 228 tile columns covered by a regional source beyond 180 east.

    Args:
        method: Point or area resampling method.
        invariant: Stable or chunk-dependent grid policy.
    """
    source = terrio.Raster.from_array(
        np.full((120, 160), 2.0), (160, 0.25, 0, 30, 0, -0.25), crs="EPSG:4326"
    )
    step = 2 * math.pi * RADIUS / 16
    grid = (-math.pi * RADIUS, step / 256, 0, step, 0, -step / 256)
    actual = warped(source, "EPSG:3857", grid, (256, 256), method, invariant)
    np.testing.assert_allclose(actual[:, :228], 2, rtol=0, atol=1e-9)
    assert np.all(actual[:, 228:] == -999)


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("method", ["near", "bilinear", "average", "sum"])
def test_a_real_gap_in_a_rounded_world_stays_empty(method, invariant):
    """Do not treat a slightly incomplete longitude turn as a periodic stored grid.

    Args:
        method: Point or area resampling method.
        invariant: Stable or chunk-dependent grid policy.
    """
    resolution = 0.0833333333
    width = 4320 * resolution
    gap = 360 - width
    source = terrio.Raster.from_array(
        np.ones((4, 4320)), (0, resolution, 0, 2 * resolution, 0, -resolution), crs="EPSG:4326"
    )
    grid = (width + gap / 2 - gap / 16, gap / 8, 0, resolution / 2, 0, -resolution)
    actual = warped(source, "EPSG:4326", grid, (1, 1), method, invariant)
    assert actual[0, 0] == -999


def test_shifted_regional_remote_footprint_uses_bounded_reads(tmp_path, record_property):
    """Read a small region a longitude turn away without downloading the entire COG.

    Args:
        tmp_path: Temporary dataset directory.
        record_property: Attach measured transfer sizes to the test report.
    """
    rasterio = pytest.importorskip("rasterio")
    from rasterio.shutil import copy
    from test_remote_reads import Server

    source_path, path = tmp_path / "source.tif", tmp_path / "pacific.tif"
    resolution = 0.015625
    values = np.random.default_rng(52).integers(1, 1000, (2560, 2560), dtype="uint16")
    with rasterio.open(
        source_path,
        "w",
        driver="GTiff",
        width=2560,
        height=2560,
        count=1,
        dtype="uint16",
        crs="EPSG:4326",
        transform=rasterio.transform.from_origin(160, 20, resolution, resolution),
    ) as target:
        target.write(values, 1)
    copy(source_path, path, driver="COG", blocksize=128, compress="deflate")
    server = Server(tmp_path)
    try:
        for invariant in (True, False):
            terrio.clear_http_cache()
            before = server.bytes_sent()
            source = terrio.open_raster(server.url + "/pacific.tif", lazy=True)
            grid = (-175 - resolution / 2, resolution, 0, resolution / 2, 0, -resolution)
            actual = warped(source, "EPSG:4326", grid, (1, 1), "average", invariant)
            assert actual[0, 0] == pytest.approx(
                float(values[1279:1281, 1599:1601].mean()), abs=1e-9
            )
            transferred = server.bytes_sent() - before
            record_property(f"bytes_transferred_{invariant}", transferred)
            record_property("file_bytes", path.stat().st_size)
            assert transferred < path.stat().st_size / 4
        assert all(row[2] == 206 for row in server.log if row[0] == "GET" and row[2] != 404)
    finally:
        server.close()
        terrio.clear_http_cache()


@pytest.mark.parametrize("invariant", [True, False])
def test_real_country_regional_coverage_matches_gdal(data_dir, invariant):
    """Compare a Pacific country mask against GDAL reading its canonical world layout.

    Args:
        data_dir: Real public country boundaries from the test-data collection.
        invariant: Stable or chunk-dependent grid policy.
    """
    rasterio = pytest.importorskip("rasterio")
    from rasterio.enums import Resampling
    from rasterio.features import rasterize
    from rasterio.warp import reproject

    path = data_dir / "countries.geojson"
    if not path.exists():
        pytest.skip("needs countries.geojson")
    features = json.loads(path.read_text())["features"]
    transform = rasterio.transform.from_origin(-180, 90, 1, 1)
    world = rasterize(
        [(feature["geometry"], 1) for feature in features],
        out_shape=(180, 360),
        transform=transform,
        fill=0,
        dtype="int16",
    )
    values = np.concatenate([world[:, 340:], world[:, :20]], axis=1)
    reference = np.full_like(world, -999)
    reference[:, 340:], reference[:, :20] = world[:, 340:], world[:, :20]
    source = terrio.Raster.from_array(values, (160, 1, 0, 90, 0, -1), crs="EPSG:4326")
    top = RADIUS * math.log(math.tan(math.radians(75)))
    grid = (RADIUS * math.radians(10), RADIUS * math.radians(1), 0, top, 0, -2 * top / 120)
    actual = warped(source, "EPSG:3832", grid, (40, 120), "near", invariant)
    expected = np.full((120, 40), -999.0)
    reproject(
        reference,
        expected,
        src_transform=transform,
        src_crs="EPSG:4326",
        src_nodata=-999,
        dst_transform=rasterio.Affine.from_gdal(*grid),
        dst_crs="EPSG:3832",
        dst_nodata=-999,
        resampling=Resampling.nearest,
    )
    assert np.any(expected[:, 20:] == 1), "the real Pacific half includes land"
    np.testing.assert_array_equal(actual, expected)
