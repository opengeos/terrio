"""GRIB files read with `terrio.open_raster` and `terrio.rio` as GDAL 3.12.2's GRIB driver reads
them (the small files of `crates/terrio/tests/data/grib`, from GDAL's test suite and written by
GDAL): band metadata, descriptions, per-band nodata values and pixels.
"""

from pathlib import Path

import numpy as np

import terrio
import terrio.rio as rio

DATA = Path(__file__).resolve().parents[2] / "crates" / "terrio" / "tests" / "data" / "grib"


def test_open_raster():
    """A GRIB2 message of two fields, the second reusing the first's bitmap."""
    r = terrio.open_raster(str(DATA / "subgrids_reuse_bitmap.grib2"), lazy=True)
    assert r.driver == "GRIB"
    assert (r.count, r.height, r.width) == (2, 20, 20)
    assert r.dtype == "float64"
    assert r.nodata == 9999.0
    meta = dict(r.metadata)
    assert meta["BAND_1#GRIB_ELEMENT"] == "UGRD"
    assert meta["BAND_2#GRIB_ELEMENT"] == "VGRD"
    assert meta["BAND_1#GRIB_DISCIPLINE"] == "0(Meteorological)"
    # The sums of GDAL's values.
    assert [float(r.read(b).sum()) for b in (1, 2)] == [50706.0, 51294.0]


def test_rio_descriptions_tags_and_nodata():
    """`terrio.rio` gives GDAL's band descriptions, tags and nodata values."""
    with rio.open(DATA / "gfs.t06z.pgrb2.1p0.grib2") as ds:
        assert ds.driver == "GRIB"
        assert ds.descriptions == ('0[-] MSL="Mean sea level"',)
        assert ds.tags(1)["GRIB_ELEMENT"] == "PRMSL"
        assert ds.tags(1)["GRIB_UNIT"] == "[Pa]"
        assert ds.nodatavals == (None,)
        # A global grid from 0 to 360 degrees, rewrapped to start at -180 (GDAL's -185, as its
        # first column is centered on 355 degrees).
        assert tuple(ds.transform)[:6] == (10.0, 0.0, -185.0, 0.0, -10.0, 90.125)
        assert float(ds.read(1).sum()) == 65415633.90625
    with rio.open(DATA / "spatial_differencing_order_1.grb2") as ds:
        assert ds.nodatavals == (9999.0,)
        assert ds.tags(1)["GRIB_UNIT"] == "[C]"
        a = ds.read(1, masked=True)
        assert np.ma.count_masked(a) > 0
