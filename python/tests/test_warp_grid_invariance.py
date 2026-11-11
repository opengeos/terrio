"""Warped pixels and validity depend on their grid, independent of requested tiles (#704)."""

import numpy as np
import pytest
from affine import Affine

import terrio
import terrio.rio as rio
from terrio.rio.enums import Resampling
from terrio.rio.vrt import WarpedVRT
from terrio.rio.windows import Window

METHODS = {
    "near": Resampling.nearest,
    "bilinear": Resampling.bilinear,
    "cubic": Resampling.cubic,
    "cubicspline": Resampling.cubic_spline,
    "lanczos": Resampling.lanczos,
    "average": Resampling.average,
    "rms": Resampling.rms,
    "mode": Resampling.mode,
    "min": Resampling.min,
    "max": Resampling.max,
    "med": Resampling.med,
    "q1": Resampling.q1,
    "q3": Resampling.q3,
    "sum": Resampling.sum,
}


def assert_same_pixels(actual, expected):
    np.testing.assert_array_equal(np.isnan(actual), np.isnan(expected))
    np.testing.assert_allclose(actual, expected, rtol=0, atol=1e-7, equal_nan=True)


@pytest.mark.parametrize("validity", ["all", "nodata", "mask"])
@pytest.mark.parametrize("version", ["3.12", "3.13"])
@pytest.mark.parametrize("error_threshold", [0.0, 0.125])
@pytest.mark.parametrize("method", METHODS)
def test_reprojected_tiles_and_vrt_windows_retain_whole_grid_values(
    tmp_path, monkeypatch, validity, version, error_threshold, method
):
    monkeypatch.delenv("TERRIO_WARP_GRID", raising=False)
    monkeypatch.setenv("TERRIO_GDAL_COMPAT", version)
    assert terrio.gdal_compat() == version
    y, x = np.mgrid[:256, :256]
    values = 500 + 300 * np.sin(x / 37.0) * np.cos(y / 53.0) + 0.2 * ((x * 31 + y * 17) % 97)
    if validity == "nodata":
        values[40:180, 70:130] = np.nan
        values[:, 220:] = np.nan
    raster = terrio.Raster.from_array(
        values,
        (447000, 10, 0, 6022000, 0, -10),
        crs="EPSG:32633",
        nodata=float("nan") if validity == "nodata" else None,
    )
    source = tmp_path / "surface.tif"
    raster.write(str(source))
    if validity == "mask":
        mask = np.full(values.shape, 255, dtype="uint8")
        mask[40:180, 70:130] = 0
        mask[:, 220:] = 0
        with rio.open(source, "r+") as dst:
            dst.write_mask(mask)
        raster = terrio.open_raster(str(source))
    options = dict(
        t_srs="EPSG:3857",
        tr=(38.0, 38.0),
        resampling=method,
        ot="Float64",
        dstnodata=float("nan"),
        et=error_threshold,
    )
    whole = terrio.warp(raster, **options)
    expected = whole.read(1)
    gt = whole.transform
    height, width = expected.shape
    xs, ys = [0, width // 3, width], [0, height // 3, height]
    with (
        rio.open(source) as src,
        WarpedVRT(
            src,
            crs="EPSG:3857",
            transform=Affine.from_gdal(*gt),
            width=width,
            height=height,
            resampling=METHODS[method],
            tolerance=error_threshold,
            nodata=float("nan"),
        ) as vrt,
    ):
        vrt_whole = vrt.read(1)
        for y0, y1 in zip(ys[:-1], ys[1:], strict=True):
            for x0, x1 in zip(xs[:-1], xs[1:], strict=True):
                extent = (
                    gt[0] + x0 * gt[1],
                    gt[3] + y1 * gt[5],
                    gt[0] + x1 * gt[1],
                    gt[3] + y0 * gt[5],
                )
                tile = terrio.warp(raster, te=extent, **options).read(1)
                assert_same_pixels(tile, expected[y0:y1, x0:x1])
                window = vrt.read(1, window=Window(x0, y0, x1 - x0, y1 - y0))
                assert_same_pixels(window, vrt_whole[y0:y1, x0:x1])


def test_explicit_grid_policy_overrides_configuration(tmp_path, monkeypatch):
    """Native warp and VRT accept a per-call policy independent of the environment."""
    y, x = np.mgrid[:128, :128]
    raster = terrio.Raster.from_array(
        np.sin(x / 13) + np.cos(y / 17),
        (447000, 10, 0, 6022000, 0, -10),
        crs="EPSG:32633",
    )
    options = dict(t_srs="EPSG:3857", tr=(38.0, 38.0), resampling="bilinear", ot="Float64")
    monkeypatch.delenv("TERRIO_WARP_GRID", raising=False)
    stable = terrio.warp(raster, **options)
    reference = terrio.warp(raster, grid_invariant=False, **options)
    assert not np.array_equal(stable.read(1), reference.read(1))
    monkeypatch.setenv("TERRIO_WARP_GRID", "GDAL")
    assert_same_pixels(terrio.warp(raster, **options).read(1), reference.read(1))
    assert_same_pixels(terrio.warp(raster, grid_invariant=True, **options).read(1), stable.read(1))
    source = tmp_path / "policy.tif"
    raster.write(str(source))
    with rio.open(source) as src:
        options = dict(
            crs="EPSG:3857",
            transform=Affine.from_gdal(*stable.transform),
            width=stable.width,
            height=stable.height,
            resampling=Resampling.bilinear,
        )
        with WarpedVRT(src, grid_invariant=True, **options) as explicit:
            expected = explicit.read(1)
        monkeypatch.delenv("TERRIO_WARP_GRID")
        with WarpedVRT(src, **options) as default:
            assert_same_pixels(default.read(1), expected)


@pytest.mark.parametrize("version", ["3.12", "3.13"])
@pytest.mark.parametrize("error_threshold", [0.0, 0.125])
@pytest.mark.parametrize("validity", ["masked", "alpha"])
@pytest.mark.parametrize("method", METHODS)
def test_reproject_validity_retains_whole_grid_samples(
    monkeypatch, version, error_threshold, validity, method
):
    """Masked arrays and binary alpha retain the same samples in independent tiles."""
    from terrio.rio.warp import reproject

    # Explicit True must also override the ambient GDAL reference policy.
    monkeypatch.setenv("TERRIO_WARP_GRID", "GDAL")
    monkeypatch.setenv("TERRIO_GDAL_COMPAT", version)
    y, x = np.mgrid[:96, :120]
    values = 10 + np.sin(x / 13) + np.cos(y / 17)
    valid = np.ones(values.shape, bool)
    valid[20:70, 40:80] = False
    src_transform = Affine(10, 0, 498000, 0, -10, 4985000)
    transform = Affine(40, 0, 1666900, 0, -40, 5624300)
    options = dict(
        src_transform=src_transform,
        src_crs="EPSG:32633",
        dst_crs="EPSG:3857",
        resampling=METHODS[method],
        tolerance=error_threshold,
        grid_invariant=True,
    )
    if validity == "masked":
        source = np.ma.array(values, mask=~valid, fill_value=-9999)
        options["dst_nodata"] = -9999
        shape = (1, 30, 38)
    else:
        source = np.stack([values, valid.astype("float64") * 255])
        options.update(src_alpha=2, dst_alpha=2)
        shape = (2, 30, 38)
    expected = np.zeros(shape)
    reproject(source, expected, dst_transform=transform, **options)
    for y0, y1 in ((0, 11), (11, 30)):
        for x0, x1 in ((0, 13), (13, 38)):
            tile = np.zeros((shape[0], y1 - y0, x1 - x0))
            tile_transform = Affine(40, 0, transform.c + x0 * 40, 0, -40, transform.f - y0 * 40)
            reproject(source, tile, dst_transform=tile_transform, **options)
            assert_same_pixels(tile, expected[:, y0:y1, x0:x1])


def test_small_remote_warp_window_keeps_range_reads_bounded(tmp_path, monkeypatch):
    """Geometry planning preserves lazy range I/O for a small reprojected window."""
    from test_remote_reads import Server

    monkeypatch.delenv("TERRIO_WARP_GRID", raising=False)
    values = np.random.default_rng(704).random((2048, 2048), dtype="float32")
    raster = terrio.Raster.from_array(values, (447000, 10, 0, 6022000, 0, -10), crs="EPSG:32633")
    path = tmp_path / "range.tif"
    raster.write(str(path), compress="DEFLATE", tile_size=64)
    whole = terrio.warp(raster, t_srs="EPSG:3857", tr=(38.0, 38.0), resampling="bilinear")
    window = Window(whole.width // 3, whole.height // 3, 32, 32)
    y0, x0 = int(window.row_off), int(window.col_off)
    expected = whole.read(1)[y0 : y0 + 32, x0 : x0 + 32]
    server = Server(tmp_path)
    transferred = {}
    try:
        for policy in (False, True):
            terrio.clear_http_cache()
            start = server.bytes_sent()
            with (
                rio.open(f"{server.url}/range.tif") as src,
                WarpedVRT(
                    src,
                    crs="EPSG:3857",
                    transform=Affine.from_gdal(*whole.transform),
                    width=whole.width,
                    height=whole.height,
                    resampling=Resampling.bilinear,
                    grid_invariant=policy,
                ) as vrt,
            ):
                actual = vrt.read(1, window=window)
            transferred[policy] = server.bytes_sent() - start
            if policy:
                np.testing.assert_allclose(actual, expected, rtol=0, atol=1e-6)
    finally:
        server.close()
        terrio.clear_http_cache()
    # VRT reads an aligned warp block around the request; count its real HTTP bodies,
    # including TIFF metadata, rather than assuming a 32-by-32 source footprint.
    assert transferred[True] * 2 < path.stat().st_size, transferred
    assert transferred[True] <= transferred[False], transferred
    print(
        f"remote warp bytes (GDAL grid / stable): {transferred}; file bytes: {path.stat().st_size}"
    )


@pytest.mark.parametrize("version", ["3.12", "3.13"])
def test_nearest_rounding_retains_centres_and_selected_taps(monkeypatch, version):
    monkeypatch.setenv("TERRIO_GDAL_COMPAT", version)
    y, x = np.mgrid[:8, :8]
    raster = terrio.Raster.from_array(
        (x + y * 10).astype("float64"), (0, 1, 0, 8, 0, -1), crs="EPSG:4326"
    )
    edge = 0.49999999995
    options = dict(
        resampling="near", ot="Float64", dstnodata=float("nan"), et=0, grid_invariant=True
    )
    whole = terrio.warp(
        raster, te=(edge, 8 - edge - 3, edge + 3, 8 - edge), ts=(3, 3), **options
    ).read(1)
    tile = terrio.warp(
        raster, te=(edge + 1, 8 - edge - 2, edge + 2, 8 - edge - 1), ts=(1, 1), **options
    ).read(1)
    np.testing.assert_array_equal(whole, (x + y * 10)[1:4, 1:4])
    np.testing.assert_array_equal(tile, whole[1:2, 1:2])


@pytest.mark.parametrize("version", ["3.12", "3.13"])
def test_affine_global_pixel_keeps_full_longitude_footprint(monkeypatch, version):
    monkeypatch.setenv("TERRIO_GDAL_COMPAT", version)
    y, x = np.mgrid[:4, :360]
    values = np.where((x < 60) | (x >= 300), 1000.0, 0.0)
    raster = terrio.Raster.from_array(values, (-180, 1, 0, 2, 0, -1), crs="EPSG:4326")
    options = dict(ts=(1, 2), resampling="bilinear", ot="Float64", et=0)
    stable = terrio.warp(raster, grid_invariant=True, **options).read(1)
    reference = terrio.warp(raster, grid_invariant=False, **options).read(1)
    assert np.all(reference > 100), reference
    np.testing.assert_allclose(stable, reference, rtol=0, atol=1e-9)
