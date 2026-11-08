"""`translate` reads a raster by window, from the overview `gdal_translate -ovr` selects."""

import numpy as np
import pytest

import terrio

GT = (0.0, 1.0, 0.0, 1024.0, 0.0, -1.0)


@pytest.fixture(scope="module")
def pyramid(tmp_path_factory, gdal):
    """A 1024 x 1024 two-band Float32 GeoTIFF (EPSG:3857, nodata -9999) with nearest-neighbour
    overviews (factors 2, 4), made by GDAL.

    Args:
        tmp_path_factory: pytest's temporary directory factory.
        gdal: GDAL's Python bindings.

    Returns:
        str: The path of the GeoTIFF.
    """
    path = str(tmp_path_factory.mktemp("translate") / "pyramid.tif")
    values = np.random.default_rng(7).uniform(0, 1000, (1024, 1024)).astype("float32")
    ds = gdal.GetDriverByName("GTiff").Create(path, 1024, 1024, 2, gdal.GDT_Float32, ["TILED=YES"])
    ds.SetGeoTransform(GT)
    srs = gdal.osr.SpatialReference()
    srs.ImportFromEPSG(3857)
    ds.SetProjection(srs.ExportToWkt())
    for band, data in [(1, values), (2, values * 2)]:
        ds.GetRasterBand(band).WriteArray(data)
        ds.GetRasterBand(band).SetNoDataValue(-9999)
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
        tuple: The band's values, the output's geotransform and the output dataset.
    """
    ds = gdal.Translate("", path, options=["-of", "MEM", *options])
    return ds.GetRasterBand(1).ReadAsArray(), ds.GetGeoTransform(), ds


def assert_same_header(gdal, out, ds):
    """Asserts that a translated raster has the data type, nodata, CRS and bands of GDAL's.

    Args:
        gdal: GDAL's Python bindings.
        out: terrio's output.
        ds: gdal_translate's output.
    """
    assert out.count == ds.RasterCount
    assert out.dtype == gdal.GetDataTypeName(ds.GetRasterBand(1).DataType).lower()
    assert out.nodata == ds.GetRasterBand(1).GetNoDataValue()
    srs = gdal.osr.SpatialReference(wkt=ds.GetProjection())
    assert out.crs == f"EPSG:{srs.GetAuthorityCode(None)}"


@pytest.mark.parametrize("lazy", [False, True])
def test_an_average_to_a_quarter_size_matches_gdal_translate(gdal, pyramid, lazy):
    # gdal_translate reads the 256 x 256 output from the factor-4 overview by default: the
    # average of the full-resolution pixels differs from its (nearest-neighbour) values by hundreds.
    src = terrio.open_raster(pyramid, lazy=True) if lazy else pyramid
    out = terrio.translate(src, outsize=("25%", "25%"), resampling="average")
    expected, transform, ds = gdal_translate(
        gdal, pyramid, ["-outsize", "25%", "25%", "-r", "average"]
    )
    np.testing.assert_array_equal(out.read(1), expected)
    np.testing.assert_array_equal(out.read(2), ds.GetRasterBand(2).ReadAsArray())
    assert out.transform == pytest.approx(transform)
    assert_same_header(gdal, out, ds)


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
        expected, transform, ds = gdal_translate(gdal, pyramid, options + extra)
        np.testing.assert_allclose(out.read(1), expected, rtol=0, atol=1e-3)
        assert out.transform == pytest.approx(transform)
        assert_same_header(gdal, out, ds)


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


@pytest.mark.parametrize("lazy", [False, True])
def test_a_raster_is_not_borrowed_while_it_is_translated(pyramid, lazy):
    # translate releases the GIL while it reads, from a handle sharing the Raster's reader: other
    # threads translating the same Raster, or asking for its properties, meanwhile do not find it
    # borrowed ("Already borrowed").
    import threading

    r = (
        terrio.open_raster(pyramid, lazy=True)
        if lazy
        else terrio.Raster.from_array(np.arange(512 * 512, dtype="float32").reshape(512, 512), GT)
    )
    expected = terrio.translate(r, outsize=(300, 300), resampling="bilinear", overview="NONE").read(
        1
    )
    errors = []
    done = threading.Event()

    def translate():
        try:
            for _ in range(10):
                out = terrio.translate(
                    r, outsize=(300, 300), resampling="bilinear", overview="NONE"
                )
                np.testing.assert_array_equal(out.read(1), expected)
        except BaseException as e:  # noqa: BLE001
            errors.append(e)

    def properties():
        try:
            while not done.is_set():
                assert r.width > 0 and r.height > 0
                assert len(r.transform) == 6
        except BaseException as e:  # noqa: BLE001
            errors.append(e)

    workers = [threading.Thread(target=translate) for _ in range(2)]
    watcher = threading.Thread(target=properties)
    for t in [*workers, watcher]:
        t.start()
    for t in workers:
        t.join()
    done.set()
    watcher.join()
    assert not errors, errors


@pytest.mark.parametrize("mode", [0, "AUTO-1"])
def test_a_level_is_refused_for_a_path_read_whole(tmp_path, mode):
    # A file the lazy reader refuses is read whole, at full resolution, where no level can be
    # chosen: refused, with the lazy reader's error, as `terrio translate --overview` refuses it.
    path = tmp_path / "broken.tif"
    path.write_bytes(b"II*\x00" + b"\xff" * 60)
    with pytest.raises(terrio.UnsupportedError, match=r"is read whole \(.+\)"):
        terrio.translate(str(path), outsize=(8, 8), overview=mode)
    # Without a level, the whole read's own error.
    with pytest.raises(terrio.TerrioError) as e:
        terrio.translate(str(path), outsize=(8, 8))
    assert not isinstance(e.value, terrio.UnsupportedError)


def test_a_level_of_a_raster_without_overviews_warns_and_reads_full_resolution(tmp_path):
    # As gdal_translate -ovr 0 and terrio translate --overview 0 on a raster without overviews.
    path = tmp_path / "grid.asc"
    path.write_text("ncols 4\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2 3 4\n5 6 7 8\n")
    with pytest.warns(UserWarning, match="cannot get overview level 0"):
        out = terrio.translate(str(path), srcwin=(1, 0, 2, 2), overview=0)
    np.testing.assert_array_equal(out.read(1), [[2, 3], [6, 7]])
