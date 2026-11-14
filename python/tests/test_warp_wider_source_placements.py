# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Wider-than-turn rasters cover a crossing footprint once, including its second part."""

import json

import numpy as np
import pytest

import terrio

CRS = "+proj=longlat +datum=WGS84 +lon_0=180"


@pytest.mark.parametrize("invariant", [True, False])
def test_wider_source_sum_covers_the_reported_two_degree_pixel(invariant):
    """Count the full 20 source-pixel area from issue #884 without duplicate coverage.

    Args:
        invariant: Stable or chunk-dependent sampling.
    """
    source = terrio.Raster.from_array(
        np.ones((10, 361)), (-180.5, 1, 0, 10, 0, -1), crs="EPSG:4326"
    )
    result = terrio.warp(
        source,
        t_srs=CRS,
        dst_transform=(-1, 2, 0, 10, 0, -10),
        ts=(1, 1),
        resampling="sum",
        ot="Float64",
        grid_invariant=invariant,
    )
    assert result.read(1)[0, 0] == pytest.approx(20, abs=1e-9)


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("reverse", [True, False])
@pytest.mark.parametrize(
    "method,expected",
    [
        ("sum", 100),
        ("average", 4),
        ("rms", np.sqrt(20.8)),
        ("min", 2),
        ("max", 8),
        ("mode", 2),
        ("med", 4),
        ("q1", 2),
        ("q3", 8),
    ],
)
def test_wider_source_placements_clip_duplicate_longitudes(invariant, reverse, method, expected):
    """Weight distinguishable edge columns and ignore the duplicate high-valued column.

    Args:
        invariant: Stable or chunk-dependent sampling.
        reverse: Store the same field with decreasing source longitudes.
        method: Area resampler or sum.
        expected: Independent weighted result for the covered columns.
    """
    values = np.ones((10, 361))
    values[:, 0], values[:, 1], values[:, 359], values[:, 360] = 2, 4, 8, 1000
    grid = (-180.5, 1, 0, 10, 0, -1)
    if reverse:
        values = values[:, ::-1].copy()
        grid = (180.5, -1, 0, 10, 0, -1)
        # Weights 1, 1, 0.5 tie 2 and 4; reversal puts 4 first in source-column order.
        if method == "mode":
            expected = 4
    source = terrio.Raster.from_array(values, grid, crs="EPSG:4326")
    result = terrio.warp(
        source,
        t_srs=CRS,
        dst_transform=(-1, 2.5, 0, 10, 0, -10),
        ts=(1, 1),
        resampling=method,
        ot="Float64",
        grid_invariant=invariant,
    )
    assert result.read(1)[0, 0] == pytest.approx(expected, abs=1e-9)


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("reverse", [True, False])
def test_mode_ties_follow_source_columns_across_placements(invariant, reverse):
    """Visit a low-column extra part before a high-column preferred part for mode ties.

    Args:
        invariant: Stable or chunk-dependent sampling.
        reverse: Reverse source-column storage and therefore its tie-breaking order.
    """
    values = np.full((1, 361), -999.0)
    values[0, 358], values[0, 1] = 8, 2
    grid = (-180.5, 1, 0, 1, 0, -1)
    if reverse:
        values = values[:, ::-1].copy()
        grid = (180.5, -1, 0, 1, 0, -1)
    source = terrio.Raster.from_array(values, grid, crs="EPSG:4326", nodata=-999)
    actual = terrio.warp(
        source,
        t_srs=CRS,
        dst_transform=(-2, 3, 0, 1, 0, -1),
        ts=(1, 1),
        resampling="mode",
        grid_invariant=invariant,
        ot="Float64",
        dstnodata=-999,
    ).read(1)[0, 0]
    assert actual == (8 if reverse else 2)


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("width", [721, 1081])
@pytest.mark.parametrize(
    "method,expected",
    [
        ("sum", 7200),
        ("average", 360),
        ("rms", np.sqrt(129600.5)),
        ("min", 359),
        ("max", 361),
        ("mode", 360),
        ("med", 360),
        ("q1", 359),
        ("q3", 361),
    ],
)
def test_multiple_turns_use_one_complete_preferred_placement(invariant, width, method, expected):
    """Prefer a fully covered footprint on two- and three-turn sources without duplicates.

    Args:
        invariant: Stable or chunk-dependent sampling.
        width: Source columns, each spanning one degree.
        method: Exact sum or weighted average.
        expected: Analytic result of columns 359, 360 and 361 with weights 0.5, 1, 0.5.
    """
    values = np.tile(np.arange(width, dtype="float64"), (10, 1))
    source = terrio.Raster.from_array(values, (-180.5, 1, 0, 10, 0, -1), crs="EPSG:4326")
    actual = terrio.warp(
        source,
        t_srs=CRS,
        dst_transform=(-1, 2, 0, 10, 0, -10),
        ts=(1, 1),
        resampling=method,
        grid_invariant=invariant,
        ot="Float64",
    ).read(1)[0, 0]
    assert actual == pytest.approx(expected, abs=1e-9)


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("method", ["average", "sum"])
def test_real_country_coverage_matches_single_turn_reference(
    data_dir, invariant, method, record_property
):
    """Compare a duplicated Natural Earth edge with an independent GDAL full-turn control.

    Args:
        data_dir: Directory containing countries.geojson.
        invariant: Stable or chunk-dependent sampling.
        method: Weighted average or source-area sum.
        record_property: Record the known GDAL sum-weight difference.
    """
    rasterio = pytest.importorskip("rasterio")
    from rasterio.enums import Resampling
    from rasterio.features import rasterize
    from rasterio.warp import reproject

    path = data_dir / "countries.geojson"
    if not path.exists():
        pytest.skip("needs countries.geojson")
    collection = json.loads(path.read_text())
    grid = rasterio.transform.from_origin(-180.5, 90, 1, 1)
    values = rasterize(
        [(feature["geometry"], 1) for feature in collection["features"]],
        out_shape=(180, 360),
        transform=grid,
        fill=0,
        dtype="float64",
    )
    output_grid = rasterio.transform.from_origin(-1, 85, 2, 5)
    expected = np.full((34, 1), -999.0)
    reproject(
        values,
        expected,
        src_transform=grid,
        src_crs="EPSG:4326",
        dst_transform=output_grid,
        dst_crs=CRS,
        dst_nodata=-999,
        resampling=getattr(Resampling, method),
    )
    wide = np.concatenate([values, values[:, :1]], axis=1)
    source = terrio.Raster.from_array(wide, grid.to_gdal(), crs="EPSG:4326")
    actual = terrio.warp(
        source,
        t_srs=CRS,
        dst_transform=output_grid.to_gdal(),
        ts=(1, 34),
        resampling=method,
        ot="Float64",
        grid_invariant=invariant,
        dstnodata=-999,
    ).read(1)
    # Independently integrate the three covered columns and each five-row output band.
    weighted_rows = 0.5 * values[5:175, 359] + values[5:175, 0] + 0.5 * values[5:175, 1]
    analytic = weighted_rows.reshape(34, 5).sum(axis=1)[:, None]
    if method == "average":
        analytic /= 10
    np.testing.assert_allclose(actual, analytic, rtol=0, atol=1e-9)
    baseline = terrio.warp(
        terrio.Raster.from_array(values, grid.to_gdal(), crs="EPSG:4326"),
        t_srs=CRS,
        dst_transform=output_grid.to_gdal(),
        ts=(1, 34),
        resampling=method,
        ot="Float64",
        grid_invariant=invariant,
        dstnodata=-999,
    ).read(1)
    np.testing.assert_allclose(actual, baseline, rtol=0, atol=1e-9)
    record_property("gdal_max_abs_difference", float(np.max(np.abs(actual - expected))))
    # GDAL's approximate sum weights differ from exact source-pixel intersection areas.
    np.testing.assert_allclose(actual, expected, rtol=0, atol=0.03 if method == "sum" else 1e-9)


@pytest.mark.parametrize("invariant", [True, False])
def test_remote_wider_source_keeps_a_bounded_latitude_window(tmp_path, record_property, invariant):
    """Measure range traffic when two placements share a narrow source-row window.

    Args:
        tmp_path: Temporary directory for a real tiled GeoTIFF.
        record_property: Record bytes and requests for validation reports.
        invariant: Stable or chunk-dependent source-window planning.
    """
    rasterio = pytest.importorskip("rasterio")
    from rasterio.shutil import copy
    from test_remote_reads import Server

    values = np.random.default_rng(884).integers(1, 65535, (1024, 3601), dtype="uint16")
    values[:, [0, 1, 3599, 3600]] = 1
    raw = tmp_path / "source.tif"
    path = tmp_path / "wide.tif"
    with rasterio.open(
        raw,
        "w",
        driver="GTiff",
        width=3601,
        height=1024,
        count=1,
        dtype="uint16",
        crs="EPSG:4326",
        transform=rasterio.transform.from_origin(-180.05, 51.2, 0.1, 0.1),
    ) as destination:
        destination.write(values, 1)
    copy(raw, path, driver="COG", blocksize=128, compress="deflate")
    server = Server(tmp_path)
    terrio.clear_http_cache()
    try:
        source = terrio.open_raster(server.url + "/wide.tif", lazy=True)
        actual = terrio.warp(
            source,
            t_srs=CRS,
            dst_transform=(-0.1, 0.2, 0, 0.5, 0, -1),
            ts=(1, 1),
            resampling="sum",
            ot="Float64",
            grid_invariant=invariant,
            overview="NONE",
        ).read(1)[0, 0]
        assert actual == pytest.approx(20, abs=1e-8)
        transferred = server.bytes_sent()
        record_property("bytes_transferred", transferred)
        record_property("file_bytes", path.stat().st_size)
        record_property("requests", len(server.log))
        assert transferred < path.stat().st_size / 2
        assert all(row[2] == 206 for row in server.log if row[0] == "GET" and row[2] != 404)
    finally:
        server.close()
        terrio.clear_http_cache()
