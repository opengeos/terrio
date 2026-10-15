"""terrio.xarray against rioxarray: the checks of `scripts/compare_xarray_with_rioxarray.py`.

Every check runs the same code with rioxarray and with terrio.xarray and compares the results with
`xarray.testing.assert_identical` (or within a stated tolerance). The tests are skipped when
xarray, dask or rioxarray is not installed; checks on real files need `TERRIO_TEST_DATA`, and the
remote one `TERRIO_TEST_REMOTE`.
"""

import importlib.util
import os
import sys
from pathlib import Path

import pytest

pytest.importorskip("xarray")
pytest.importorskip("dask")
pytest.importorskip("rioxarray")

SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "compare_xarray_with_rioxarray.py"
if not SCRIPT.exists():
    pytest.skip("the comparison script is not available", allow_module_level=True)

_spec = importlib.util.spec_from_file_location("compare_xarray_with_rioxarray", SCRIPT)
compare = importlib.util.module_from_spec(_spec)
sys.modules["compare_xarray_with_rioxarray"] = compare
_spec.loader.exec_module(compare)


def _data():
    value = os.environ.get("TERRIO_TEST_DATA")
    return Path(value) if value and Path(value).is_dir() else None


@pytest.fixture(scope="module")
def scratch(tmp_path_factory):
    out = tmp_path_factory.mktemp("xarray-compare")
    compare.synthetic_files(out)
    return out


@pytest.fixture(scope="module")
def servers(scratch):
    """Local HTTP servers of the synthetic and the real files, counting bytes."""
    synth = compare.RangeServer(str(scratch / "synth"))
    data = _data()
    real = compare.RangeServer(str(data)) if data is not None else None
    yield {"synth": synth, "data": real}
    synth.close()
    if real is not None:
        real.close()


def _id(c):
    return f"{c.section}: {c.name}"


@pytest.mark.parametrize("check", compare.CHECKS, ids=_id)
def test_check(check, scratch, servers):
    data = _data()
    server = servers["synth"] if check.name.startswith("synth") else servers["data"]
    why = compare.available(check, data, bool(os.environ.get("TERRIO_TEST_REMOTE")), server)
    if why:
        pytest.skip(why)
    outcome = compare.run_check(check, data, scratch, server)
    assert not compare.failed(check, outcome), f"{outcome.status}: {outcome.detail}"
