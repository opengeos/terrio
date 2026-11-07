"""`translate` reads a raster by window, from the overview `gdal_translate -ovr` selects."""

import numpy as np
import pytest

import terrio

GT = (0.0, 1.0, 0.0, 1024.0, 0.0, -1.0)


@pytest.fixture(scope="module")
def pyramid(tmp_path_factory, gdal):
    """A 1024 x 1024 Float32 GeoTIFF with nearest-neighbour overviews (factors 2, 4), by GDAL.

    Args:
        tmp_path_factory: pytest's temporary directory factory.
        gdal: GDAL's Python bindings.

    Returns:
        str: The path of the GeoTIFF.
    """
    path = str(tmp_path_factory.mktemp("translate") / "pyramid.tif")
    values = np.random.default_rng(7).uniform(0, 1000, (1024, 1024)).astype("float32")
    ds = gdal.GetDriverByName("GTiff").Create(path, 1024, 1024, 1, gdal.GDT_Float32, ["TILED=YES"])
    ds.SetGeoTransform(GT)
    ds.GetRasterBand(1).WriteArray(values)
    ds.BuildOverviews("NEAREST", [2, 4])
    ds = None
    return path


def gdal_translate(gdal, path, options):
    """The first band of `gdal_translate` of `path` with `options`, as an array.

    Args:
        gdal: GDAL's Python bindings.
        path: The source raster.
        options: The `gdal_translate` arguments, as a list.

    Returns:
        tuple: The band's values and the output's geotransform.
    """
    ds = gdal.Translate("", path, options=["-of", "MEM", *options])
    return ds.GetRasterBand(1).ReadAsArray(), ds.GetGeoTransform()


@pytest.mark.parametrize("lazy", [False, True])
def test_an_average_to_a_quarter_size_matches_gdal_translate(gdal, pyramid, lazy):
    # gdal_translate reads the 256 x 256 output from the factor-4 overview by default: the
    # average of the full-resolution pixels differs from its (nearest-neighbour) values by hundreds.
    src = terrio.open_raster(pyramid, lazy=True) if lazy else pyramid
    out = terrio.translate(src, outsize=("25%", "25%"), resampling="average")
    expected, transform = gdal_translate(gdal, pyramid, ["-outsize", "25%", "25%", "-r", "average"])
    np.testing.assert_array_equal(out.read(1), expected)
    assert out.transform == pytest.approx(transform)


@pytest.mark.parametrize(
    ("mode", "ovr"),
    [
        (None, None),
        ("AUTO", "AUTO"),
        ("auto-1", "AUTO-1"),
        ("NONE", "NONE"),
        (0, "0"),
        ("1", "1"),
    ],
)
@pytest.mark.parametrize("lazy", [False, True])
def test_overview_selects_the_level_as_gdal_translate_ovr(gdal, pyramid, lazy, mode, ovr):
    src = terrio.open_raster(pyramid, lazy=True) if lazy else pyramid
    windows = [["-outsize", "200", "200", "-r", "bilinear"], ["-srcwin", "100", "60", "400", "300"]]
    if str(mode).upper().startswith("AUTO-"):
        # Known difference, shared with `terrio translate --overview AUTO-1`: without an output
        # size, gdal_translate keeps the window's full-resolution size (400 x 300) and reads it
        # from overview 0, where terrio writes the 200 x 150 of that level.
        windows = windows[:1]
    for options in windows:
        kwargs = (
            {"outsize": (200, 200), "resampling": "bilinear"}
            if options[0] == "-outsize"
            else {"srcwin": (100, 60, 400, 300)}
        )
        out = terrio.translate(src, overview=mode, **kwargs)
        extra = [] if ovr is None else ["-ovr", ovr]
        expected, transform = gdal_translate(gdal, pyramid, options + extra)
        np.testing.assert_allclose(out.read(1), expected, rtol=0, atol=1e-3)
        assert out.transform == pytest.approx(transform)


def test_a_written_output_is_the_one_returned(pyramid, tmp_path):
    dst = str(tmp_path / "quarter.tif")
    out = terrio.translate(
        pyramid, dst, outsize=("25%", "25%"), resampling="average", overview="NONE"
    )
    np.testing.assert_array_equal(terrio.open_raster(dst).read(1), out.read(1))


def test_a_raster_in_memory_is_translated_whole():
    values = np.arange(64, dtype="float32").reshape(8, 8)
    r = terrio.Raster.from_array(values, (0.0, 1.0, 0.0, 8.0, 0.0, -1.0))
    for mode in [None, "AUTO", "NONE"]:
        out = terrio.translate(r, outsize=(4, 4), resampling="average", overview=mode)
        expected = values.reshape(4, 2, 4, 2).mean(axis=(1, 3))
        np.testing.assert_array_equal(out.read(1), expected)


def test_invalid_overview_values_are_refused(pyramid):
    for bad in ["invalid", True, 1.5]:
        with pytest.raises(terrio.InvalidArgumentError):
            terrio.translate(pyramid, outsize=(8, 8), overview=bad)
