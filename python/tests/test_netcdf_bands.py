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


def test_write_netcdf_groups(tmp_path):
    """Rasters written into netCDF-4 groups read back by group, as they were written."""
    a = np.arange(12, dtype="float32").reshape(3, 4)
    b = (np.arange(20, dtype="int16").reshape(4, 5) - 3).astype("int16")
    ra = terrio.Raster.from_array(a, transform=(10.0, 1.0, 0.0, 50.0, 0.0, -1.0), crs="EPSG:4326")
    rb = terrio.Raster.from_array(
        b, transform=(500000.0, 30.0, 0.0, 4500000.0, 0.0, -30.0), crs="EPSG:32613"
    )
    path = tmp_path / "groups.nc"
    terrio.write_netcdf_groups(
        {"/terrain/a": ra, "/b": rb}, str(path), options={"COMPRESS": "DEFLATE"}
    )
    for group, values in [("/terrain/a", a), ("/b", b)]:
        r = terrio.open_raster(f'NETCDF:"{path}":{group}/Band1')
        np.testing.assert_array_equal(r.read().reshape(values.shape), values)
    assert terrio.open_raster(f'NETCDF:"{path}":/b/Band1').crs == "EPSG:32613"
    terrio.write_netcdf_groups([("/", ra)], str(tmp_path / "root.nc"))
    np.testing.assert_array_equal(
        terrio.open_raster(str(tmp_path / "root.nc")).read().reshape(a.shape), a
    )
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.write_netcdf_groups([("/x", ra), ("x", rb)], str(tmp_path / "bad.nc"))
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.write_netcdf_groups([], str(tmp_path / "none.nc"))
