"""Focal statistics, collar cleaning and LUT enhancement bindings, compared with GDAL."""

import shutil
import subprocess

import numpy as np
import pytest

import terrio

TRANSFORM = (500000.0, 10.0, 0.0, 4100000.0, 0.0, -10.0)


def raster(a, nodata=None):
    return terrio.Raster.from_array(a, TRANSFORM, crs="EPSG:32610", nodata=nodata)


def scene():
    """A 3-band Byte image: a noisy dark collar around a textured middle."""
    rng = np.random.default_rng(7)
    a = rng.integers(0, 12, size=(3, 40, 50)).astype("uint8")
    yy, xx = np.mgrid[:40, :50]
    inside = (yy - 20) ** 2 / 15**2 + (xx - 25) ** 2 / 20**2 < 1
    a[:, inside] = rng.integers(60, 250, size=(3, int(inside.sum())))
    return a


def test_neighbors_api(tmp_path):
    a = np.array([[1, 2, 3], [4, -1, 6], [7, 8, 9]], dtype="float32")
    src = raster(a, -1)
    out = terrio.neighbors(src, kernel="equal")
    assert out.read(1).dtype == np.float64
    assert out.nodata == -1
    assert out.read(1)[1, 1] == -1
    tagged = terrio.neighbors(src, kernel="equal", nodata=" -9999 ")
    assert tagged.nodata == -9999 and tagged.read(1)[1, 1] == -1
    assert terrio.neighbors(src, kernel="equal", nodata=" NONE ").nodata is None
    assert out.read(1)[0, 0] == (1 + 1 + 2 + 1 + 1 + 2 + 4 + 4) / 8
    path = str(tmp_path / "max.tif")
    both = terrio.neighbors(
        src, path, kernel=["equal", "u"], method=["max", "sum"], ot="Float32", nodata="none"
    )
    assert both.count == 2 and both.nodata is None
    np.testing.assert_array_equal(terrio.open_raster(path).read(2), both.read(2))
    with pytest.raises(terrio.errors.InvalidArgumentError):
        terrio.neighbors(src, kernel="sharpen", size=5)
    with pytest.raises(terrio.errors.InvalidArgumentError):
        terrio.neighbors(src, kernel="[[1,2],[3,4]]")


@pytest.mark.parametrize("operation", [terrio.nearblack, terrio.clean_collar])
def test_alpha_and_mask_are_exclusive(operation):
    with pytest.raises(terrio.errors.InvalidArgumentError, match="exclusive"):
        operation(raster(scene()), add_alpha=True, add_mask=True)


def test_collar_api(tmp_path):
    src = terrio.Raster.from_array(scene(), TRANSFORM, crs="EPSG:32610")
    out = terrio.clean_collar(src, add_alpha=True, pixel_distance=0)
    assert out.count == 4
    alpha = out.read(4)
    assert alpha[0, 0] == 0 and alpha[20, 25] == 255
    assert out.read(1)[0, 0] == 0
    two = terrio.nearblack(src, colors=["0,0,0", (5, 5, 5)], color_threshold=6)
    assert two.count == 3
    path = str(tmp_path / "masked.tif")
    masked = terrio.nearblack(src, path, add_mask=True)
    assert masked.has_mask
    assert masked.read_mask()[0, 0] == 0 and masked.read_mask()[20, 25] == 255
    with pytest.raises(terrio.errors.InvalidArgumentError):
        terrio.nearblack(src, add_mask=True)
    with pytest.raises(terrio.errors.InvalidArgumentError):
        terrio.nearblack(src, colors=["0,0"])


@pytest.mark.parametrize("operation", [terrio.nearblack, terrio.clean_collar])
def test_flat_color_sequences_and_empty_defaults(operation):
    src = raster(scene())
    expected = operation(src, colors=[(0, 0, 0)]).read()
    for colors in ([0, 0, 0], (0, 0, 0)):
        np.testing.assert_array_equal(operation(src, colors=colors).read(), expected)
    np.testing.assert_array_equal(operation(src, colors=[]).read(), operation(src).read())
    np.testing.assert_array_equal(
        operation(src, colors=[], white=True).read(), operation(src, white=True).read()
    )


def test_enhance_preserves_float64_nodata_precision():
    nodata = 1.0000000001
    src = raster(np.array([[nodata, 1.0]], dtype="float64"), nodata)
    out = terrio.enhance(src, src_scale=(0, 256), ot="Float64")
    assert out.nodata == nodata
    assert out.read(1)[0, 0] == nodata
    assert out.read(1)[0, 1] == 1.0


def test_enhance_api(tmp_path):
    a = (np.arange(64, dtype="uint8") * 4).reshape(8, 8)
    src = raster(a)
    config = str(tmp_path / "lut.txt")
    text = terrio.enhance_luts(src, config=config)
    assert text.startswith("1:Band -0.5:ScaleMin 255.5:ScaleMax ")
    path = str(tmp_path / "source.tif")
    src.write(path)
    assert terrio.enhance_luts(path) == text
    eq = terrio.enhance(src, equalize=True)
    np.testing.assert_array_equal(terrio.enhance(src, config=config).read(1), eq.read(1))
    linear = terrio.enhance(src, src_scale=(0, 256), ot="UInt16")
    assert linear.read(1).dtype == np.uint16
    np.testing.assert_array_equal(linear.read(1), a)
    with pytest.raises(terrio.errors.InvalidArgumentError):
        terrio.enhance(src)


@pytest.fixture
def gdal():
    """Only reference comparisons require GDAL; API tests still run without it."""
    module = pytest.importorskip("osgeo.gdal")
    module.UseExceptions()
    return module


def gdal_cli(*args):
    if shutil.which(args[0]) is None:
        pytest.skip(f"{args[0]} is not installed")
    p = subprocess.run(list(args), capture_output=True, text=True)
    if p.returncode != 0:
        pytest.fail(f"{' '.join(args)}: {p.stderr.strip()}")


def write(path, a, nodata=None):
    raster(a, nodata).write(path)


@pytest.mark.parametrize(
    "kernel,method", [("equal", "median"), ("gaussian", None), ("edge2", None), ("equal", "mode")]
)
def test_neighbors_against_gdal(tmp_path, kernel, method, gdal):
    a = scene()[0].astype("float32")
    a[5:9, 10:14] = -9999
    src = str(tmp_path / "src.tif")
    write(src, a, -9999)
    ref = str(tmp_path / "gdal.tif")
    opts = ["--kernel", kernel] + (["--method", method] if method else [])
    gdal_cli("gdal", "raster", "neighbors", "-q", *opts, src, ref)
    out = terrio.neighbors(src, kernel=kernel, method=method)
    expected = gdal.Open(ref)
    np.testing.assert_array_equal(out.read(1), expected.ReadAsArray())
    assert out.nodata == expected.GetRasterBand(1).GetNoDataValue()


@pytest.mark.parametrize("algorithm", ["twopasses", "floodfill"])
@pytest.mark.parametrize("distance", [0, 2])
def test_nearblack_against_gdal(tmp_path, algorithm, distance, gdal):
    src = str(tmp_path / "scene.tif")
    terrio.Raster.from_array(scene(), TRANSFORM, crs="EPSG:32610").write(src)
    ref = str(tmp_path / "gdal.tif")
    gdal.Nearblack(ref, src, options=f"-setalpha -nb {distance} -alg {algorithm}")
    out = terrio.nearblack(src, add_alpha=True, pixel_distance=distance, algorithm=algorithm)
    np.testing.assert_array_equal(
        np.stack([out.read(b) for b in range(1, 5)]), gdal.Open(ref).ReadAsArray()
    )


def test_enhance_against_gdal(tmp_path, gdal):
    src = str(tmp_path / "scene.tif")
    terrio.Raster.from_array(scene(), TRANSFORM, crs="EPSG:32610").write(src)
    copy = str(tmp_path / "copy.tif")
    shutil.copy(src, copy)
    ref = str(tmp_path / "gdal.tif")
    gdal_cli("gdalenhance", "-quiet", "-equalize", copy, ref)
    out = terrio.enhance(src, equalize=True)
    np.testing.assert_array_equal(
        np.stack([out.read(b) for b in range(1, 4)]), gdal.Open(ref).ReadAsArray()
    )
