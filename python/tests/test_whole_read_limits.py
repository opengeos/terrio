"""Native integer buffers must honor the process's usable-memory limit."""

import os
import subprocess
import sys

import pytest


@pytest.mark.skipif(sys.platform != "linux", reason="uses the Linux usable-memory policy")
@pytest.mark.parametrize("surface", ["native", "rio"])
def test_whole_integer_read_checks_usable_memory(tmp_path, gdal, surface):
    """Refuse an integer pixel buffer before allocation, as floating-point reads do."""
    path = tmp_path / "larger-than-budget.tif"
    output = gdal.GetDriverByName("GTiff").Create(
        str(path),
        8192,
        8192,
        1,
        gdal.GDT_Byte,
        options=["TILED=YES", "BLOCKXSIZE=512", "BLOCKYSIZE=512", "COMPRESS=DEFLATE"],
    )
    output.GetRasterBand(1).Fill(7)
    output = None
    script = """
import resource
import sys
import terrio
import terrio.rio
resource.setrlimit(resource.RLIMIT_RSS, (32 << 20, 32 << 20))
try:
    if sys.argv[2] == "native":
        raster = terrio.open_raster(sys.argv[1], lazy=True)
    else:
        raster = terrio.rio.open(sys.argv[1])
    raster.read()
except terrio.ResourceLimitError as error:
    print("RESOURCE_LIMIT", error)
else:
    raise AssertionError("whole integer read ignored usable memory")
"""
    env = dict(os.environ, OPENBLAS_NUM_THREADS="1", RAYON_NUM_THREADS="2")
    for option in ("TERRIO_MAX_DECLARED_BYTES", "TERRIO_MAX_BANDS"):
        env.pop(option, None)
    result = subprocess.run(
        [sys.executable, "-c", script, str(path), surface],
        env=env,
        capture_output=True,
        text=True,
        timeout=20,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "RESOURCE_LIMIT" in result.stdout
    assert "memory the process can use" in result.stdout


@pytest.mark.skipif(sys.platform != "linux", reason="uses the Linux usable-memory policy")
@pytest.mark.parametrize(
    ("surface", "mask_kind"),
    [(surface, kind) for surface in ["native", "rio"] for kind in ["all-valid", "nodata"]]
    + [("native", kind) for kind in ["dataset", "alpha8", "alpha16", "nodata8"]],
)
def test_resampled_masks_budget_only_their_required_buffers(tmp_path, gdal, surface, mask_kind):
    """Reference: GDAL 3.13.3. A 64 MiB mask fits where Float64 pixels do not."""
    path = tmp_path / "source.tif"
    alpha = mask_kind.startswith("alpha")
    if mask_kind in ("alpha8", "nodata8"):
        dtype = gdal.GDT_Byte
    elif alpha:
        dtype = gdal.GDT_UInt16
    else:
        dtype = gdal.GDT_Float64
    output = gdal.GetDriverByName("GTiff").Create(str(path), 16, 16, 2 if alpha else 1, dtype)
    output.GetRasterBand(1).Fill(7)
    if mask_kind in ("nodata", "nodata8"):
        output.GetRasterBand(1).SetNoDataValue(0)
    elif mask_kind == "dataset":
        output.CreateMaskBand(gdal.GMF_PER_DATASET)
        output.GetRasterBand(1).GetMaskBand().Fill(255)
    elif alpha:
        output.GetRasterBand(1).SetColorInterpretation(gdal.GCI_GrayIndex)
        output.GetRasterBand(2).SetColorInterpretation(gdal.GCI_AlphaBand)
        output.GetRasterBand(2).Fill(255 if mask_kind == "alpha8" else 65535)
    # Reference masks are all valid. Nodata and UInt16 alpha still need working buffers.
    reference = output.GetRasterBand(1).GetMaskBand().ReadAsArray(buf_xsize=8192, buf_ysize=8192)
    assert reference.shape == (8192, 8192)
    assert reference.min() == reference.max() == 255
    del reference
    output = None
    script = """
import resource
import sys
import terrio
import terrio.rio
resource.setrlimit(resource.RLIMIT_RSS, (128 << 20, 128 << 20))
if sys.argv[2] == "native":
    raster = terrio.open_raster(sys.argv[1], lazy=True)
    masks = lambda: raster.read_resampled_masks((8192, 8192), band=[1])
    pixels = lambda: raster.read_resampled((8192, 8192))
    together = lambda: raster.read_resampled_with_masks((8192, 8192))
else:
    raster = terrio.rio.open(sys.argv[1])
    masks = lambda: raster.read_masks(out_shape=(1, 8192, 8192))
    pixels = lambda: raster.read(out_shape=(1, 8192, 8192))
    together = lambda: raster.read(out_shape=(1, 8192, 8192), masked=True)
if sys.argv[3] in ("nodata", "nodata8", "alpha16"):
    try:
        masks()
    except terrio.ResourceLimitError:
        pass
    else:
        raise AssertionError("mask ignored its required pixel or conversion buffers")
else:
    result = masks()
    assert result.shape == (1, 8192, 8192)
    assert result.dtype.name == "uint8"
    assert result.min() == result.max() == 255
    del result
if sys.argv[3] == "nodata8":
    result = pixels()
    assert result.dtype.name == "uint8" and result.min() == result.max() == 7
    del result
    try:
        together()
    except terrio.ResourceLimitError:
        pass
    else:
        raise AssertionError("combined read ignored its output and nodata mask temporary")
mask_only_case = sys.argv[3].startswith("alpha") or sys.argv[3] == "nodata8"
for read in ([] if mask_only_case else [pixels, together]):
    try:
        read()
    except terrio.ResourceLimitError:
        pass
    else:
        raise AssertionError("Float64 pixel output ignored the memory budget")
print("MASK_BUDGET_OK")
"""
    env = dict(os.environ, OPENBLAS_NUM_THREADS="1", RAYON_NUM_THREADS="2")
    for option in ("TERRIO_MAX_DECLARED_BYTES", "TERRIO_MAX_BANDS"):
        env.pop(option, None)
    result = subprocess.run(
        [sys.executable, "-c", script, str(path), surface, mask_kind],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "MASK_BUDGET_OK" in result.stdout


@pytest.mark.skipif(sys.platform != "linux", reason="uses the Linux usable-memory policy")
@pytest.mark.parametrize("surface", ["native", "rio"])
def test_window_checks_decoded_chunk_with_unlimited_config(tmp_path, gdal, surface):
    """Reference: GDAL 3.13.3. One pixel still requires a 256 MiB decoded tile."""
    path = tmp_path / "large-tile.tif"
    output = gdal.GetDriverByName("GTiff").Create(
        str(path),
        32768,
        8192,
        1,
        gdal.GDT_Byte,
        options=[
            "TILED=YES",
            "BLOCKXSIZE=32768",
            "BLOCKYSIZE=8192",
            "COMPRESS=DEFLATE",
            "NUM_THREADS=1",
        ],
    )
    output.GetRasterBand(1).Fill(0)
    output = None
    assert 1000 < path.stat().st_size < 1 << 20
    reference = gdal.Open(str(path))
    assert reference.GetRasterBand(1).GetBlockSize() == [32768, 8192]
    assert reference.ReadAsArray(0, 0, 1, 1)[0, 0] == 0
    reference = None
    script = """
import resource
import sys
import terrio
import terrio.rio
resource.setrlimit(resource.RLIMIT_RSS, (128 << 20, 128 << 20))
try:
    if sys.argv[2] == "native":
        raster = terrio.open_raster(sys.argv[1], lazy=True)
        raster.read(window=(0, 0, 1, 1))
    else:
        with terrio.rio.open(sys.argv[1]) as raster:
            raster.read(window=((0, 1), (0, 1)))
except terrio.ResourceLimitError as error:
    assert "decoded TIFF chunk" in str(error), str(error)
    print("CHUNK_RESOURCE_LIMIT", error)
else:
    raise AssertionError("unlimited chunk configuration bypassed usable memory")
"""
    env = dict(
        os.environ,
        OPENBLAS_NUM_THREADS="1",
        RAYON_NUM_THREADS="2",
        TERRIO_TIFF_MAX_CHUNK_BYTES=str((1 << 63) - 1),
    )
    for option in ("TERRIO_MAX_DECLARED_BYTES", "TERRIO_MAX_BANDS"):
        env.pop(option, None)
    result = subprocess.run(
        [sys.executable, "-c", script, str(path), surface],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "CHUNK_RESOURCE_LIMIT" in result.stdout
