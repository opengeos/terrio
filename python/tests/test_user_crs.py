"""GeoTIFFs whose CRS has no EPSG code, written by GDAL (`scripts/make_user_crs_fixtures.py`),
read with `terrio.open_raster` (eagerly and lazily) and `terrio.rio`, and checked by
transforming a point into the CRS read, against PROJ's coordinates for GDAL's CRS.
"""

import json
import shutil
import warnings
from pathlib import Path

import pytest

import terrio
import terrio.rio as rio

DATA = Path(__file__).resolve().parents[2] / "crates" / "terrio" / "tests" / "data" / "user_crs"
EXPECTED = json.loads((DATA / "expected.json").read_text())
# terrio computes these projections differently from PROJ: only the presence of a CRS is checked
# (see crates/terrio/tests/user_crs.rs).
PROJECTION_DIFFERS = {"gnom", "mill", "robin", "sinu", "vandg"}


def _crs_of(name: str, how: str) -> str | None:
    """The CRS of a fixture, read one way.

    Args:
        name: The fixture.
        how: `"eager"`, `"lazy"` or `"rio"`.

    Returns:
        str | None: The CRS, as terrio or terrio.rio gives it.
    """
    path = str(DATA / f"{name}.tif")
    if how == "rio":
        with rio.open(path) as ds:
            return ds.crs.to_wkt() if ds.crs else None
    return terrio.open_raster(path, lazy=how == "lazy").crs


@pytest.mark.parametrize("how", ["eager", "lazy", "rio"])
@pytest.mark.parametrize("name", sorted(EXPECTED))
def test_reads_user_defined_crs_as_gdal(name, how):
    """A user-defined CRS is read, and transforms as the one GDAL reads."""
    crs = _crs_of(name, how)
    assert crs is not None and not crs.startswith("EPSG:9001")
    if name in PROJECTION_DIFFERS:
        return
    e = EXPECTED[name]
    (x,), (y,) = terrio.transform_points([e["lon"]], [e["lat"]], "EPSG:4326", crs)
    assert abs(x - e["x"]) < 1e-3 and abs(y - e["y"]) < 1e-3


def test_warns_of_a_projection_it_cannot_read(tmp_path):
    """A GeoKeys projection method terrio does not map warns and leaves the raster without CRS."""
    data = (DATA / "tmerc.tif").read_bytes()
    # ProjCoordTransGeoKey (3075) = 1 (transverse Mercator) -> 4 (oblique Mercator, Laborde).
    key = (3075).to_bytes(2, "little") + bytes([0, 0, 1, 0])
    assert data.count(key + bytes([1, 0])) == 1
    path = tmp_path / "laborde.tif"
    path.write_bytes(data.replace(key + bytes([1, 0]), key + bytes([4, 0])))
    for lazy in (False, True):
        with pytest.warns(UserWarning, match="projection method 4"):
            assert terrio.open_raster(str(path), lazy=lazy).crs is None
    # A sidecar's CRS takes its place, with no warning.
    shutil.copy(DATA / "sidecar_only.tif.aux.xml", f"{path}.aux.xml")
    for lazy in (False, True):
        with warnings.catch_warnings():
            warnings.simplefilter("error")
            assert terrio.open_raster(str(path), lazy=lazy).crs == "EPSG:32613"


def test_sidecar_crs_overrides_geokeys(tmp_path):
    """A `.aux.xml` sidecar's CRS overrides the GeoKeys, as GDAL reads it."""
    for name in ("sidecar_override.tif", "sidecar_override.tif.aux.xml"):
        shutil.copy(DATA / name, tmp_path / name)
    path = str(tmp_path / "sidecar_override.tif")
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        assert terrio.open_raster(path).crs == "EPSG:32614"
    (tmp_path / "sidecar_override.tif.aux.xml").unlink()
    assert terrio.open_raster(path).crs == "EPSG:32613"
