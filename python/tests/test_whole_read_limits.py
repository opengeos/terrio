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
