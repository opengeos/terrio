"""A datacube library's `.rio` calls, against rioxarray.

Its datacube reader resolves the CRS of NetCDF/Zarr variables, clips to the requested
bounds (`clip_box`) and warps onto tile grids (`reproject` with an explicit shape and
transform). Daymet's Zarr carries its CRS only as CF grid mapping parameters, which terrio
did not read; and morecantile hands the tile CRS over as pyproj WKT2, whose units terrio's
`write_coordinate_system` did not find. Compared exactly, object for object. Skipped without
rioxarray.
"""

import numpy as np
import pytest

xarray = pytest.importorskip("xarray")
pytest.importorskip("rioxarray")
pyproj = pytest.importorskip("pyproj")

import rasterio  # noqa: E402
from rasterio.enums import Resampling as RR  # noqa: E402

import terrio.rio as trio  # noqa: E402
import terrio.xarray  # noqa: E402,F401
from terrio.rio.enums import Resampling as TR  # noqa: E402

E = {"semi_major_axis": 6378137.0, "inverse_flattening": 298.257223563}
DAYMET = {
    "grid_mapping_name": "lambert_conformal_conic",
    "standard_parallel": [25.0, 60.0],
    "latitude_of_projection_origin": 42.5,
    "longitude_of_central_meridian": -100.0,
    "false_easting": 0.0,
    "false_northing": 0.0,
    **E,
}
CF_CASES = {
    "lcc-2sp-daymet": DAYMET,
    "lcc-1sp": {
        "grid_mapping_name": "lambert_conformal_conic",
        "standard_parallel": 33.0,
        "longitude_of_central_meridian": -95.0,
        **E,
    },
    "transverse-mercator": {
        "grid_mapping_name": "transverse_mercator",
        "longitude_of_central_meridian": 15.0,
        "scale_factor_at_central_meridian": 0.9996,
        "false_easting": 500000.0,
        **E,
    },
    "polar-stereographic-b": {
        "grid_mapping_name": "polar_stereographic",
        "standard_parallel": 70.0,
        "straight_vertical_longitude_from_pole": -45.0,
        "latitude_of_projection_origin": 90.0,
        "semi_major_axis": 6378273.0,
        "semi_minor_axis": 6356889.449,
    },
    "albers": {
        "grid_mapping_name": "albers_conical_equal_area",
        "standard_parallel": [29.5, 45.5],
        "latitude_of_projection_origin": 23.0,
        "longitude_of_central_meridian": -96.0,
        **E,
    },
    "laea": {
        "grid_mapping_name": "lambert_azimuthal_equal_area",
        "longitude_of_projection_origin": 10.0,
        "latitude_of_projection_origin": 52.0,
        "false_easting": 4321000.0,
        "false_northing": 3210000.0,
        **E,
    },
    "latlon-sphere": {"grid_mapping_name": "latitude_longitude", "earth_radius": 6371229.0},
    "lcc-no-ellipsoid": {k: v for k, v in DAYMET.items() if k not in E},
}


def _grid(values, x0, y0, res, attrs):
    """A DataArray on a north-up grid whose grid mapping coordinate holds `attrs` (no WKT)."""
    h, w = values.shape
    da = xarray.DataArray(
        values,
        dims=("y", "x"),
        coords={"y": y0 - res * (np.arange(h) + 0.5), "x": x0 + res * (np.arange(w) + 0.5)},
        name="tmax",
    )
    da.coords["lambert_conformal_conic"] = xarray.Variable((), 0, attrs=attrs)
    # As xarray decodes it with decode_coords="all", as the datacube reader opens them.
    da.encoding.update({"grid_mapping": "lambert_conformal_conic", "_FillValue": np.nan})
    return da


@pytest.fixture(scope="module")
def daymet():
    """A Daymet-like float32 array on Daymet's Hawaii grid (LCC from CF parameters), NaN ocean."""
    yy, xx = np.mgrid[0:584, 0:284]
    v = (np.sin(xx / 6.0) * 10 + np.cos(yy / 9.0) * 5 + 20).astype("float32")
    v[(xx - 142) ** 2 / 120**2 + (yy - 292) ** 2 / 250**2 > 1] = np.nan
    return _grid(v, -5802750.0, -38500.0, 1000.0, DAYMET)


def _same(a, b):
    """rioxarray's result `a` and terrio's `b` are the same array on the same grid."""
    assert a.dims == b.dims
    np.testing.assert_array_equal(b.values, a.values)
    for d in ("x", "y"):
        np.testing.assert_array_equal(b[d].values, a[d].values)
        assert b[d].attrs == a[d].attrs
    assert b.terrio.transform() == a.rio.transform()
    assert a.rio.crs is not None
    assert b.terrio.crs.to_wkt() == a.rio.crs.to_wkt()
    na, nb = a.rio.nodata, b.terrio.nodata
    assert (na is None and nb is None) or na == nb or (np.isnan(na) and np.isnan(nb))


@pytest.mark.parametrize("name", sorted(CF_CASES))
def test_crs_from_cf_matches_rioxarray(name):
    """A grid mapping with only CF parameters gives rioxarray's CRS, WKT for WKT."""
    da = _grid(np.zeros((3, 4), "float32"), 0.0, 0.0, 1.0, CF_CASES[name])
    assert da.rio.crs is not None
    assert da.terrio.crs.to_wkt() == da.rio.crs.to_wkt()


TILES = [  # Web Mercator tile bounds over Hawaii, as the datacube reader's tile() passes them
    (-17532819.79994059, 2191602.4749925737, -16906647.84422466, 2817774.4307085015),
    (-17454548.30547610, 2348145.4637807915, -17141462.32773858, 2661231.4414785616),
]


@pytest.mark.parametrize("tile", range(len(TILES)))
@pytest.mark.parametrize("nodata", [None, -7.0])
def test_reproject_onto_tile_grid_matches_rioxarray(daymet, tile, nodata):
    """A part() call: reproject(dst_crs, shape=, transform=, resampling=, nodata=)."""
    t = rasterio.transform.from_bounds(*TILES[tile], 64, 48)
    a = daymet.rio.reproject(
        "EPSG:3857", shape=(48, 64), transform=t, resampling=RR.nearest, nodata=nodata
    )
    b = daymet.terrio.reproject(
        "EPSG:3857",
        shape=(48, 64),
        transform=trio.transform.from_bounds(*TILES[tile], 64, 48),
        resampling=TR.nearest,
        nodata=nodata,
    )
    assert np.isfinite(a.values).any()
    _same(a, b)


def test_reproject_to_pyproj_wkt2_crs_matches_rioxarray(daymet):
    """morecantile's `rasterio_crs`: a CRS from pyproj's WKT2, whose units sit in its axes."""
    wkt2 = pyproj.CRS.from_epsg(3857).to_wkt()
    t = rasterio.transform.from_bounds(*TILES[0], 32, 32)
    a = daymet.rio.reproject(rasterio.crs.CRS.from_wkt(wkt2), shape=(32, 32), transform=t)
    b = daymet.terrio.reproject(trio.crs.CRS.from_wkt(wkt2), shape=(32, 32), transform=t)
    np.testing.assert_array_equal(b.values, a.values)
    for d in ("x", "y"):
        np.testing.assert_array_equal(b[d].values, a[d].values)
        assert b[d].attrs == a[d].attrs


def test_default_grid_from_lambert_conformal_conic_matches_rioxarray(daymet):
    """reproject(dst_crs) alone: the grid GDAL suggests from the edges of the LCC grid.

    Its top is the largest northing of the edges' latitudes, which were a few units in the last
    place off GDAL's, and the whole `y` coordinate with it.
    """
    a = daymet.rio.reproject("EPSG:3857")
    b = daymet.terrio.reproject("EPSG:3857")
    assert a.rio.transform().f == 2694860.677396626
    _same(a, b)


@pytest.mark.parametrize("method", ["nearest", "bilinear"])
def test_reproject_ascending_latitudes_matches_rioxarray(method):
    """Latitudes stored south to north, as NetCDF and Zarr datacubes often store them.

    A destination row whose centre is on the edge between two source rows, and a global grid
    onto Web Mercator's default grid, whose top rows' centres are within 1e-10 degree of the
    north pole.
    """
    v = np.random.default_rng(42).random((10, 10), dtype=np.float32)
    tie = xarray.DataArray(
        v,
        dims=("y", "x"),
        coords={"y": np.linspace(19.0, 20.0, 10), "x": np.linspace(-156.0, -155.0, 10)},
    )
    t = rasterio.transform.from_bounds(-155.9, 19.1, -155.1, 19.9, 7, 7)
    a = tie.rio.write_crs("EPSG:4326").rio.reproject(
        "EPSG:4326", shape=(7, 7), transform=t, resampling=RR[method]
    )
    b = tie.terrio.write_crs("EPSG:4326").terrio.reproject(
        "EPSG:4326", shape=(7, 7), transform=t, resampling=TR[method]
    )
    _same(a, b)
    world = xarray.DataArray(
        np.ones((180, 360), "float32"),
        dims=("lat", "lon"),
        coords={"lat": np.arange(-89.5, 90.0), "lon": np.arange(-179.5, 180.0)},
    )
    a = (
        world.rio.set_spatial_dims(x_dim="lon", y_dim="lat")
        .rio.write_crs("EPSG:4326")
        .rio.reproject("EPSG:3857", resampling=RR[method], nodata=np.nan)
    )
    b = (
        world.terrio.set_spatial_dims(x_dim="lon", y_dim="lat")
        .terrio.write_crs("EPSG:4326")
        .terrio.reproject("EPSG:3857", resampling=TR[method], nodata=np.nan)
    )
    assert np.isnan(a.values[0]).all() and not np.isnan(a.values[-1]).any()
    _same(a, b)


@pytest.mark.parametrize(
    "bounds,crs,auto_expand",
    [
        ((-5790321.7, -120233.9, -5761002.3, -60501.2), None, False),  # fractional native bounds
        ((-5790000.0, -121000.0, -5760000.0, -60000.0), None, False),  # on pixel edges
        ((-158.0, 19.5, -155.5, 21.0), "EPSG:4326", True),  # part(bbox, crs=4326)
        ((-157.3, 20.6, -156.8, 21.1), "EPSG:4326", False),
        ((-5770100.0, -90100.0, -5769900.0, -89900.0), None, True),  # one pixel: auto-expanded
    ],
)
def test_clip_box_matches_rioxarray(daymet, bounds, crs, auto_expand):
    """The datacube reader's clip_box(*bbox, crs=bounds_crs, auto_expand=...)."""
    a = daymet.rio.clip_box(*bounds, crs=crs, auto_expand=auto_expand)
    b = daymet.terrio.clip_box(*bounds, crs=crs, auto_expand=auto_expand)
    _same(a, b)
