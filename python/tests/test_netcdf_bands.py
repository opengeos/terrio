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


@pytest.mark.parametrize("gdal_version_attribute", ["YES", "NO"])
def test_row_order_of_gdal_written_netcdf_is_gdals(tmp_path, gdal, gdal_version_attribute):
    """Files GDAL writes (rows south to north under a north-up `GeoTransform`), with and without
    the `GDAL` global attribute that GDAL 3.13 takes the row order from: terrio reads the rows,
    and the geotransform, as the installed GDAL does (see `TERRIO_GDAL_COMPAT`)."""
    version = ".".join(gdal.__version__.split(".")[:2])
    if version != terrio.gdal_compat():
        pytest.skip(f"terrio reproduces GDAL {terrio.gdal_compat()}, osgeo.gdal is {version}")
    gdal.UseExceptions()
    values = np.arange(12, dtype="float32").reshape(3, 4)
    mem = gdal.GetDriverByName("MEM").Create("", 4, 3, 1, gdal.GDT_Float32)
    mem.SetGeoTransform([500000, 30, 0, 4100000, 0, -30])
    mem.SetProjection("EPSG:32611")
    mem.GetRasterBand(1).WriteArray(values)
    path = str(tmp_path / f"gdal_{gdal_version_attribute}.nc")
    gdal.GetDriverByName("netCDF").CreateCopy(
        path, mem, options=[f"WRITE_GDAL_VERSION={gdal_version_attribute}"]
    )
    mem = None
    reference = gdal.Open(path)
    expected = reference.GetRasterBand(1).ReadAsArray()
    r = terrio.open_raster(path)
    np.testing.assert_array_equal(r.read().reshape(expected.shape), expected)
    np.testing.assert_allclose(r.transform, reference.GetGeoTransform(), rtol=0, atol=1e-9)
