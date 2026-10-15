"""terrio.rio against rasterio: the checks of `scripts/compare_rio_with_rasterio.py`.

Every check runs the same code with rasterio and with terrio.rio and compares the results. The
tests are skipped when rasterio is not installed; checks on real files need `TERRIO_TEST_DATA`, and
the remote one `TERRIO_TEST_REMOTE`.
"""

import dataclasses
import importlib.util
import os
import platform
import sys
from pathlib import Path

import pytest

rasterio = pytest.importorskip("rasterio")
pytest.importorskip("shapely")

for _sub in ("features", "io", "mask", "merge", "vrt", "warp"):
    importlib.import_module(f"rasterio.{_sub}")

import terrio.rio as trio  # noqa: E402

SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "compare_rio_with_rasterio.py"
if not SCRIPT.exists():
    pytest.skip("the comparison script is not available", allow_module_level=True)

_spec = importlib.util.spec_from_file_location("compare_rio_with_rasterio", SCRIPT)
compare = importlib.util.module_from_spec(_spec)
sys.modules["compare_rio_with_rasterio"] = compare
_spec.loader.exec_module(compare)


def _id(c):
    return f"{c.section}: {c.name}"


@pytest.fixture(scope="module")
def scratch(tmp_path_factory):
    return tmp_path_factory.mktemp("rio-compare")


def _data():
    value = os.environ.get("TERRIO_TEST_DATA")
    return Path(value) if value and Path(value).is_dir() else None


# rasterio's macOS arm64 wheel builds GDAL with fused multiply-adds, so map coordinates on pixel
# boundaries (a line's end at x = 500000 on a grid starting there) can turn into pixel coordinates
# a hair below the integer: GDAL then burns the neighbouring pixel (the whole line shifts by one
# column). Linux and Windows wheels agree with terrio exactly.
FMA_SENSITIVE = {
    "rasterize: replace, all_touched, add, out, dtypes",
    "geometry_mask, geometry_window, raster_geometry_mask",
    "mask: crop, filled, invert, all_touched, pad, indexes, nodata",
}
MACOS_ARM = sys.platform == "darwin" and platform.machine() == "arm64"


@pytest.mark.parametrize("check", compare.CHECKS, ids=_id)
def test_check(check, scratch):
    if MACOS_ARM and check.name in FMA_SENSITIVE:
        check = dataclasses.replace(check, expect="tolerance", max_frac=0.01)
    data = _data()
    if check.needs_data and data is None:
        pytest.skip("TERRIO_TEST_DATA is not set")
    if check.needs_remote and not os.environ.get("TERRIO_TEST_REMOTE"):
        pytest.skip("TERRIO_TEST_REMOTE is not set")
    if check.min_rasterio and compare.rasterio_version(rasterio) < check.min_rasterio:
        wanted = ".".join(map(str, check.min_rasterio))
        pytest.skip(
            f"terrio.rio follows rasterio {wanted} here; {rasterio.__version__} is installed"
        )
    outcome = compare.run_check(check, rasterio, trio, data, scratch)
    row = compare.Row(check.section, check.name, check.expect, outcome, check.reason)
    assert not compare.failed(row), outcome.detail


@pytest.mark.parametrize("script", sorted(compare.EXAMPLES.glob("*.py")), ids=lambda p: p.name)
def test_example(script, scratch):
    data = _data()
    if data is None:
        pytest.skip("TERRIO_TEST_DATA is not set")
    outcome = compare.run_example(script, data, scratch)
    row = compare.Row("examples", script.name, "tolerance", outcome, "")
    assert not compare.failed(row), outcome.detail
