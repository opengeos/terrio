"""netCDF variables of a trailing `bands` dimension, as EMIT's radiance and reflectance, and their
variants (`scripts/make_netcdf_bands_fixtures.py`), read with `terrio.open_raster` and
`terrio.rio` as GDAL reads them: the bands, rows, columns, pixels and `GEOLOCATION` metadata.
"""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio
import terrio.rio as rio

DATA = Path(__file__).resolve().parents[2] / "crates" / "terrio" / "tests" / "data" / "netcdf_bands"
EXPECTED = json.loads((DATA / "expected.json").read_text())


def _name(case: str) -> tuple[str, str]:
    """The subdataset name of a case and its file's path.

    Args:
        case: `file:variable`.

    Returns:
        tuple[str, str]: The subdataset name and the file's path.
    """
    file, var = case.split(":", 1)
    path = str(DATA / file)
    return f'NETCDF:"{path}":{var}', path


@pytest.mark.parametrize("case", sorted(EXPECTED))
def test_read_as_gdal(case):
    """Each variable's size, pixels and geolocation, as GDAL reads them."""
    e = EXPECTED[case]
    name, path = _name(case)
    with rio.open(name) as ds:
        assert (ds.count, ds.height, ds.width) == (e["count"], e["height"], e["width"])
        np.testing.assert_array_equal(ds.read().astype("float64").ravel(), e["values"])
        geolocation = {k: v.replace(path, "<file>") for k, v in ds.tags(ns="GEOLOCATION").items()}
        assert geolocation == e["geolocation"]
        assert ("GEOLOCATION" in ds.tag_namespaces()) == bool(e["geolocation"])
    r = terrio.open_raster(name)
    geolocation = {k: v.replace(path, "<file>") for k, v in (r.geolocation or [])}
    assert geolocation == e["geolocation"]
