"""CRS serialization, alpha masks and COG photometric options against rasterio (GDAL).

CRSs read from WKT2 (morecantile's tile CRS, CF grid mappings through pyproj) export GDAL's
WKT1 and their units; `to_dict(projjson=True)` gives PROJ's PROJJSON (a STAC generator's
`proj:projjson`) through pyproj; alpha bands act as masks as `GetMaskBand()` makes them (Byte,
or UInt16 through `GDALRescaledAlphaBand`, never float); `rio.open(driver="COG")` ignores
PHOTOMETRIC as GDAL's COG driver does. Skipped without rasterio.
"""

import pickle

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from affine import Affine  # noqa: E402
from rasterio.crs import CRS as RCRS  # noqa: E402
from rasterio.enums import ColorInterp as RCI  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.crs import CRS as TCRS  # noqa: E402
from terrio.rio.enums import ColorInterp as TCI  # noqa: E402

CODES = [3857, 4326, 32633, 2263, 3035, 4269]


@pytest.mark.parametrize("code", CODES)
def test_wkt2_definitions_export_gdals_wkt1(code):
    """GDAL exports a CRS read from WKT2 as WKT1_GDAL, axes included (needs pyproj)."""
    pytest.importorskip("pyproj")
    wkt2 = RCRS.from_epsg(code).to_wkt(version="WKT2_2019")
    assert TCRS.from_wkt(wkt2).to_wkt() == RCRS.from_wkt(wkt2).to_wkt()


@pytest.mark.parametrize("code", [3857, 32633, 2263, 27700])
def test_wkt2_units_are_read_from_the_axes(code):
    """WKT2 declares a projected CRS's unit inside each AXIS (no pyproj needed)."""
    wkt2 = RCRS.from_epsg(code).to_wkt(version="WKT2_2019")
    assert TCRS.from_wkt(wkt2).linear_units_factor == RCRS.from_wkt(wkt2).linear_units_factor


def _proj_minor(version):
    return tuple(int(v) for v in version.split(".")[:2])


def _skip_unless_projjson_comparable():
    pyproj = pytest.importorskip("pyproj")
    ours, gdal = _proj_minor(pyproj.proj_version_str), _proj_minor(rasterio.__proj_version__)
    if ours != gdal and min(ours, gdal) < (9, 5):
        pytest.skip("PROJ 9.5 changed PROJJSON (base_crs type): compare equal PROJ versions")


def test_projjson_matches_rasterio(tmp_path):
    """Dataset and WKT CRSs: rasterio's `to_dict(projjson=True)` (PROJ through pyproj)."""
    _skip_unless_projjson_comparable()
    p = str(tmp_path / "utm.tif")
    with rasterio.open(
        p,
        "w",
        driver="GTiff",
        width=8,
        height=8,
        count=1,
        dtype="uint8",
        crs="EPSG:32633",
        transform=Affine(10, 0, 500000, 0, -10, 5800000),
    ) as dst:
        dst.write(np.zeros((1, 8, 8), "uint8"))
    with rasterio.open(p) as r, trio.open(p) as t:
        assert t.crs.to_dict(projjson=True) == r.crs.to_dict(projjson=True)
    for wkt in (RCRS.from_epsg(3035).to_wkt(), RCRS.from_epsg(3857).to_wkt(version="WKT2_2019")):
        assert TCRS.from_wkt(wkt).to_dict(projjson=True) == RCRS.from_wkt(wkt).to_dict(
            projjson=True
        )


CODE_CRSS = {
    "from_epsg": lambda C, c: C.from_epsg(c),
    "from_string": lambda C, c: C.from_string(f"epsg:{c}"),
    "from_user_input": lambda C, c: C.from_user_input(c),
    "from_dict": lambda C, c: C.from_dict(init=f"epsg:{c}"),
    "+init": lambda C, c: C.from_string(f"+init=epsg:{c}"),
    "pickled": lambda C, c: pickle.loads(pickle.dumps(C.from_epsg(c))),
}


@pytest.mark.parametrize("make", list(CODE_CRSS))
@pytest.mark.parametrize("code", [4326, 3857, 32633])
def test_code_projjson_matches_rasterio(make, code):
    """CRSs from codes give PROJ's database object (area, bbox, scope) as GDAL builds them, and
    pickled ones come back from WKT. These codes' EPSG records read alike in PROJ 9.7 and 9.8
    (others gained `remarks`)."""
    _skip_unless_projjson_comparable()
    expected = CODE_CRSS[make](RCRS, code).to_dict(projjson=True)
    assert CODE_CRSS[make](TCRS, code).to_dict(projjson=True) == expected


WRITE_CRSS = {
    "string": lambda C: "EPSG:32633",
    "int": lambda C: 4326,
    "from_epsg": lambda C: C.from_epsg(32633),
}


@pytest.mark.parametrize("crs", list(WRITE_CRSS))
def test_written_dataset_crs_projjson_matches_rasterio(tmp_path, crs):
    """A dataset's CRS while it is written: GDAL reads it back from the dataset (WKT, no area or
    scope), whichever constructor the caller's CRS came from."""
    _skip_unless_projjson_comparable()
    out = {}
    for name, mod, C in (("rasterio", rasterio, RCRS), ("terrio", trio, TCRS)):
        with mod.open(
            str(tmp_path / f"{name}.tif"),
            "w",
            driver="GTiff",
            width=8,
            height=8,
            count=1,
            dtype="uint8",
            crs=WRITE_CRSS[crs](C),
            transform=Affine(10, 0, 500000, 0, -10, 5800000),
        ) as dst:
            dst.write(np.zeros((1, 8, 8), "uint8"))
            out[name] = dst.crs.to_dict(projjson=True)
    assert out["terrio"] == out["rasterio"]


def _alpha_file(path, count, dtype):
    yy, xx = np.mgrid[0:64, 0:80]
    data = np.stack([(xx * (b + 1) + yy) % 200 for b in range(count)]).astype(dtype)
    top = {"uint8": 255, "uint16": 65535}.get(dtype, 255)
    alpha = np.full((64, 80), top, dtype)
    alpha[:, 60:] = 0
    alpha[10:20, :30] = 128  # UInt16: 1 to 256 are 1 in the mask
    alpha[30:40, :30] = min(300, top)
    alpha[45:55, :30] = min(30000, top) if dtype != "uint8" else 77
    data[-1] = alpha
    photometric = "RGB" if count == 4 else "MINISBLACK"
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=80,
        height=64,
        count=count,
        dtype=dtype,
        photometric=photometric,
        alpha="YES",
        crs="EPSG:3857",
        transform=Affine(1, 0, 0, 0, -1, 0),
    ) as dst:
        dst.write(data)


@pytest.mark.parametrize(
    "count,dtype", [(2, "uint16"), (4, "uint16"), (4, "uint8"), (4, "float32"), (2, "float32")]
)
def test_alpha_bands_mask_as_gdal(tmp_path, count, dtype):
    """Byte alpha is the mask, UInt16 alpha is rescaled, float alpha is not a mask."""
    p = str(tmp_path / "a.tif")
    _alpha_file(p, count, dtype)
    with rasterio.open(p) as r, trio.open(p) as t:
        assert [c.name for c in t.colorinterp] == [c.name for c in r.colorinterp]
        assert [[f.name for f in x] for x in t.mask_flag_enums] == [
            [f.name for f in x] for x in r.mask_flag_enums
        ]
        np.testing.assert_array_equal(t.read_masks(), r.read_masks())
        np.testing.assert_array_equal(t.dataset_mask(), r.dataset_mask())
        np.testing.assert_array_equal(
            np.ma.getmaskarray(t.read(masked=True)), np.ma.getmaskarray(r.read(masked=True))
        )


@pytest.mark.parametrize("photometric", ["YCBCR", "MINISBLACK", "RGB"])
@pytest.mark.parametrize("count,dtype", [(3, "uint8"), (4, "uint16"), (1, "uint8")])
def test_cog_writes_ignore_photometric(tmp_path, photometric, count, dtype):
    """GDAL's COG driver has no PHOTOMETRIC option: the colour interpretation decides."""
    data = np.arange(count * 48 * 64).reshape(count, 48, 64).astype(dtype)
    out = []
    for mod, ci in ((rasterio, RCI), (trio, TCI)):
        p = str(tmp_path / f"{mod.__name__}.tif")
        with mod.open(
            p,
            "w",
            driver="COG",
            width=64,
            height=48,
            count=count,
            dtype=dtype,
            crs="EPSG:3857",
            transform=Affine(1, 0, 0, 0, -1, 0),
            photometric=photometric,
        ) as dst:
            if count >= 3:
                dst.colorinterp = [ci.red, ci.green, ci.blue] + [ci.alpha] * (count - 3)
            dst.write(data)
        with rasterio.open(p) as s:
            out.append(([c.name for c in s.colorinterp], s.read()))
    assert out[1][0] == out[0][0]
    np.testing.assert_array_equal(out[1][1], out[0][1])
