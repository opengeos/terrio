"""A full-turn geographic source has the same samples wherever its stored grid starts."""

import json
import math

import numpy as np
import pytest

import terrio


@pytest.fixture(scope="module")
def global_sources():
    """Create the same longitude field stored from zero and from minus 180 degrees.

    Returns:
        list: Two native rasters with matching geographic values.
    """
    sources = []
    for west in [0, -180]:
        longitude = (west + np.arange(360)) % 360
        row = np.where(longitude == 359, 2.0, np.where(longitude == 0, 4.0, 1.0))
        sources.append(
            terrio.Raster.from_array(
                np.repeat(row[None, :], 20, axis=0),
                (west, 1, 0, 10, 0, -1),
                crs="EPSG:4326",
            )
        )
    return sources


METHODS = [
    "near",
    "bilinear",
    "cubic",
    "cubicspline",
    "lanczos",
    "average",
    "mode",
    "min",
    "max",
    "med",
    "q1",
    "q3",
    "sum",
    "rms",
]


@pytest.mark.parametrize("method", METHODS)
@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("target", ["EPSG:4326", "EPSG:3857", "180", "90"])
def test_world_storage_layouts_agree(global_sources, method, invariant, target):
    """All world pixels are filled under every resampler and both grid policies.

    Args:
        global_sources: Equivalent longitude fields.
        method: Resampling method.
        invariant: Stable or chunk-dependent grid policy.
        target: Geographic, Mercator or a shifted geographic CRS.
    """
    crs = target if target.startswith("EPSG:") else f"+proj=longlat +datum=WGS84 +lon_0={target}"
    extent = (-180, -10, 180, 10)
    if target == "EPSG:3857":
        radius = 6378137
        height = radius * math.log(math.tan(math.radians(50)))
        extent = (-math.pi * radius, -height, math.pi * radius, height)
    outputs = [
        np.asarray(
            terrio.warp(
                source,
                t_srs=crs,
                te=extent,
                ts=(360, 20),
                resampling=method,
                ot="Float64",
                dstnodata=-999,
                grid_invariant=invariant,
            ).read(1)
        )
        for source in global_sources
    ]
    assert not np.any(outputs[0] == -999)
    np.testing.assert_allclose(outputs[0], outputs[1], rtol=0, atol=1e-8)
    if method == "sum":
        assert float(outputs[0].sum()) == pytest.approx(7280, abs=1e-6)


@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("method,expected", [("sum", 1.8), ("average", 3.6)])
def test_projected_pixel_across_zero_takes_both_sides(global_sources, invariant, method, expected):
    """A projected footprint counts its fractional shares west and east of zero.

    Args:
        global_sources: Equivalent longitude fields.
        invariant: Stable or chunk-dependent grid policy.
        method: Sum or area-weighted average.
        expected: The geometric result in source-pixel area units.
    """
    radius = 6378137
    height = radius * math.log(math.tan(math.radians(45.5)))
    transform = (-radius * math.radians(0.1), radius * math.radians(0.5), 0, height, 0, -height)
    for source in global_sources:
        value = terrio.warp(
            source,
            t_srs="EPSG:3857",
            dst_transform=transform,
            ts=(1, 1),
            resampling=method,
            ot="Float64",
            dstnodata=-999,
            grid_invariant=invariant,
        ).read(1)[0, 0]
        assert value == pytest.approx(expected, abs=1e-9)


@pytest.mark.parametrize("method", ["near", "bilinear", "cubic"])
@pytest.mark.parametrize("invariant", [True, False])
def test_polar_meridian_is_filled_but_regional_edge_is_not(method, invariant):
    """A full-turn seam is interior while a regional source keeps its excluded right edge.

    Args:
        method: Point resampling method.
        invariant: Stable or chunk-dependent grid policy.
    """
    world = terrio.Raster.from_array(np.ones((180, 360)), (-180, 1, 0, 90, 0, -1), crs="EPSG:4326")
    east = terrio.Raster.from_array(np.ones((180, 180)), (0, 1, 0, 90, 0, -1), crs="EPSG:4326")
    for crs, top in [("EPSG:3995", 1_250_000), ("EPSG:3031", -1_250_000)]:
        options = dict(
            t_srs=crs,
            dst_transform=(-262_500, 25_000, 0, top, 0, -25_000),
            ts=(21, 2),
            resampling=method,
            ot="Float64",
            dstnodata=-999,
            grid_invariant=invariant,
        )
        actual = terrio.warp(world, **options).read(1)
        np.testing.assert_allclose(actual, 1, rtol=0, atol=1e-10)
        assert np.all(terrio.warp(east, **options).read(1)[:, 10] == -999)


def test_remote_periodic_footprint_keeps_windowed_reads(tmp_path, record_property):
    """Read both longitude edges of a remote COG without downloading the complete file.

    Args:
        tmp_path: Temporary directory.
        record_property: Record measured network traffic in the test report.
    """
    rasterio = pytest.importorskip("rasterio")
    from rasterio.shutil import copy
    from test_remote_reads import Server

    path = tmp_path / "world.tif"
    source_path = tmp_path / "source.tif"
    values = np.random.default_rng(7).integers(1, 1000, (1440, 720), dtype="uint16")
    values[712, 0], values[712, -1] = 4, 2
    with rasterio.open(
        source_path,
        "w",
        driver="GTiff",
        width=720,
        height=1440,
        count=1,
        dtype="uint16",
        crs="EPSG:4326",
        transform=rasterio.transform.from_origin(0, 90, 0.5, 0.125),
    ) as dst:
        dst.write(values, 1)
    copy(source_path, path, driver="COG", blocksize=128, compress="deflate")
    server = Server(tmp_path)
    terrio.clear_http_cache()
    try:
        source = terrio.open_raster(server.url + "/world.tif", lazy=True)
        result = terrio.warp(
            source,
            t_srs="EPSG:4326",
            dst_transform=(-0.1, 0.5, 0, 1, 0, -0.125),
            ts=(1, 1),
            resampling="average",
            ot="Float64",
            dstnodata=-999,
            overview="NONE",
        ).read(1)
        assert result[0, 0] == pytest.approx(3.6, abs=1e-9)
        transferred = server.bytes_sent()
        record_property("bytes_transferred", transferred)
        record_property("file_bytes", path.stat().st_size)
        record_property("requests", len(server.log))
        assert transferred < path.stat().st_size
        assert all(row[2] == 206 for row in server.log if row[0] == "GET" and row[2] != 404)
    finally:
        server.close()
        terrio.clear_http_cache()


@pytest.mark.parametrize(
    "method", ["bilinear", "cubic", "cubicspline", "lanczos", "average", "sum"]
)
@pytest.mark.parametrize("invariant", [True, False])
def test_periodic_nodata_matches_the_same_field_away_from_the_cut(method, invariant):
    """Periodic neighbors retain the same nodata and interpolation rules as interior pixels.

    Args:
        method: Point or area resampling method.
        invariant: Stable or chunk-dependent grid policy.
    """
    outputs = []
    for west in [0, -180]:
        longitude = (west + np.arange(360)) % 360
        row = np.where(longitude == 359, 2.0, np.where(longitude == 0, -999.0, 1.0))
        source = terrio.Raster.from_array(
            np.repeat(row[None, :], 20, axis=0),
            (west, 1, 0, 10, 0, -1),
            crs="EPSG:4326",
            nodata=-999,
        )
        outputs.append(
            terrio.warp(
                source,
                t_srs="EPSG:4326",
                dst_transform=(-0.25, 0.1, 0, 1, 0, -1),
                ts=(1, 1),
                resampling=method,
                ot="Float64",
                dstnodata=-999,
                grid_invariant=invariant,
            ).read(1)
        )
    np.testing.assert_allclose(outputs[0], outputs[1], rtol=0, atol=1e-9)


@pytest.mark.parametrize("method", ["near", "bilinear", "cubic"])
def test_periodicity_does_not_wrap_the_south_latitude_edge(method):
    """The excluded south edge remains excluded because latitude is not periodic.

    Args:
        method: Point resampling method.
    """
    source = terrio.Raster.from_array(np.ones((180, 360)), (-180, 1, 0, 90, 0, -1), crs="EPSG:4326")
    actual = terrio.warp(
        source,
        t_srs="EPSG:4326",
        dst_transform=(-0.5, 1, 0, -89.5, 0, -1),
        ts=(1, 1),
        resampling=method,
        dstnodata=-999,
    ).read(1)
    assert actual[0, 0] == -999


@pytest.mark.parametrize("method", ["near", "bilinear", "average", "sum"])
def test_real_world_field_storage_equivalence(data_dir, method, record_property):
    """Real country coverage retains storage equivalence, with independent GDAL controls.

    Args:
        data_dir: Directory containing countries.geojson.
        method: Resampling method.
        record_property: Record measured differences from GDAL's sampling policy.
    """
    rasterio = pytest.importorskip("rasterio")
    from rasterio.enums import Resampling
    from rasterio.features import rasterize
    from rasterio.warp import reproject

    path = data_dir / "countries.geojson"
    if not path.exists():
        pytest.skip("needs countries.geojson")
    collection = json.loads(path.read_text())
    transform = rasterio.transform.from_origin(-180, 90, 1, 1)
    values = rasterize(
        [(feature["geometry"], 1) for feature in collection["features"]],
        out_shape=(180, 360),
        transform=transform,
        fill=0,
        dtype="uint8",
    )
    radius = 6378137
    north = radius * math.log(math.tan(math.radians(85)))
    destination = (-math.pi * radius, 2 * math.pi * radius / 360, 0, north, 0, -2 * north / 160)
    expected = np.full((160, 360), -999.0)
    reproject(
        values,
        expected,
        src_transform=transform,
        src_crs="EPSG:4326",
        dst_transform=rasterio.Affine.from_gdal(*destination),
        dst_crs="EPSG:3857",
        dst_nodata=-999,
        resampling=getattr(Resampling, "nearest" if method == "near" else method),
        ERROR_THRESHOLD=0,
    )
    controls = {}
    for west, stored in [(-180, values), (0, np.roll(values, -180, axis=1))]:
        source = terrio.Raster.from_array(stored, (west, 1, 0, 90, 0, -1), crs="EPSG:4326")
        for invariant in [True, False]:
            actual = terrio.warp(
                source,
                t_srs="EPSG:3857",
                dst_transform=destination,
                ts=(360, 160),
                resampling=method,
                ot="Float64",
                dstnodata=-999,
                grid_invariant=invariant,
                et=0,
            ).read(1)
            assert not np.any(actual == -999)
            if west == -180:
                controls[invariant] = actual
                record_property(
                    f"gdal_max_error_invariant_{invariant}",
                    float(np.max(np.abs(actual - expected))),
                )
            np.testing.assert_allclose(actual, controls[invariant], rtol=0, atol=1e-8)
            if method in {"near", "average"} or (method == "bilinear" and not invariant):
                np.testing.assert_allclose(actual, expected, rtol=0, atol=1e-6)
            if method == "sum":
                # Exact footprint sums retain the source-area total; GDAL distributes
                # that same total differently among Mercator rows.
                assert float(actual.sum()) == pytest.approx(float(values[10:170].sum()), abs=1e-6)
