"""Writes the netCDF files of `crates/terrio/tests/data/netcdf_bands` and what GDAL reads of them.

Usage:
    python scripts/make_netcdf_bands_fixtures.py

The files hold 3-D variables laid out as EMIT's L1B radiance and L2A reflectance,
`(downtrack, crosstrack, bands)`, and variants of them: a last dimension named `band`, `Bands`,
`nbands` or `wavelengths`, other names for the first two dimensions, `bands` first or in the
middle, a 4-D variable, a `GeoTransform`, coordinate axes, and EMIT's `location` group of
longitudes and latitudes, missing or on dimensions of other names. `expected.json` holds, for each
variable, what GDAL (through rasterio) reads: its size, pixels, geotransform (null without one)
and `GEOLOCATION` metadata (the file's path replaced by `<file>`). Needs netCDF4 and rasterio.
"""

from __future__ import annotations

import json
from pathlib import Path

import netCDF4
import numpy as np
import rasterio

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "netcdf_bands"
ROWS, COLS = 3, 4
WGS84 = (
    'GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],'
    'UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4326"]]'
)


def cube(shape: tuple[int, ...], start: int) -> np.ndarray:
    """Distinct values for a variable.

    Args:
        shape: Its shape.
        start: Its first value.

    Returns:
        The values, as float32.
    """
    return (start + np.arange(int(np.prod(shape)))).astype("float32").reshape(shape)


def variable(nc: netCDF4.Dataset, name: str, dims: tuple[str, ...], start: int) -> netCDF4.Variable:
    """Writes a float variable of distinct values.

    Args:
        nc: The file.
        name: The variable's name.
        dims: Its dimensions.
        start: Its first value.

    Returns:
        The variable.
    """
    v = nc.createVariable(name, "f4", dims)
    v[:] = cube(tuple(len(nc.dimensions[d]) for d in dims), start)
    return v


def location(nc: netCDF4.Dataset, dims: tuple[str, str]) -> None:
    """Writes EMIT's `location` group: 2-D `lon` and `lat` (and `glt_x`) on `dims`.

    Args:
        nc: The file.
        dims: The dimensions of the arrays, defined in the group when not in the root.
    """
    loc = nc.createGroup("location")
    for d, n in zip(dims, (ROWS, COLS)):
        if d not in nc.dimensions:
            loc.createDimension(d, n)
    lon = -104.2 + 0.0006 * np.add.outer(np.arange(ROWS) * 0.1, np.arange(COLS))
    lat = 32.6 - 0.0006 * np.add.outer(np.arange(ROWS), np.arange(COLS) * 0.1)
    loc.createVariable("lon", "f8", dims)[:] = lon
    loc.createVariable("lat", "f8", dims)[:] = lat
    loc.createVariable("glt_x", "i4", dims)[:] = np.arange(ROWS * COLS).reshape(ROWS, COLS)


def write_files() -> dict[str, list[str]]:
    """Writes the files.

    Returns:
        The variables to read, by file name.
    """
    files = {}
    with netCDF4.Dataset(OUT / "emit_bands.nc", "w") as nc:
        for d, n in [("downtrack", ROWS), ("crosstrack", COLS), ("bands", 5), ("band", 1), ("Bands", 2), ("nbands", 2),
                     ("wavelengths", 2), ("channel", 2), ("a", ROWS), ("b", COLS), ("t", 2)]:
            nc.createDimension(d, n)
        cases = [
            ("radiance", ("downtrack", "crosstrack", "bands")),
            ("band_of_one_last", ("downtrack", "crosstrack", "band")),
            ("capital_bands_last", ("downtrack", "crosstrack", "Bands")),
            ("nbands_last", ("downtrack", "crosstrack", "nbands")),
            ("wavelengths_last", ("downtrack", "crosstrack", "wavelengths")),
            ("channel_last", ("downtrack", "crosstrack", "channel")),
            ("other_dims", ("a", "b", "bands")),
            ("bands_first", ("bands", "downtrack", "crosstrack")),
            ("bands_middle", ("downtrack", "bands", "crosstrack")),
            ("four_d", ("t", "downtrack", "crosstrack", "bands")),
            ("two_d", ("downtrack", "crosstrack")),
        ]
        for i, (name, dims) in enumerate(cases):
            variable(nc, name, dims, 1000 * i)
        crs = nc.createVariable("crs", "i4")
        crs.grid_mapping_name = "latitude_longitude"
        crs.spatial_ref = WGS84
        crs.GeoTransform = "-104.2 0.0006 0 32.6 0 -0.0006"
        variable(nc, "with_geotransform", ("downtrack", "crosstrack", "bands"), 50000).grid_mapping = "crs"
        location(nc, ("downtrack", "crosstrack"))
        files["emit_bands.nc"] = [n for n, _ in cases] + ["with_geotransform", "/location/lon", "/location/glt_x"]
    # Without the location group, with a `band` dimension of three.
    with netCDF4.Dataset(OUT / "no_location.nc", "w") as nc:
        for d, n in [("downtrack", ROWS), ("crosstrack", COLS), ("band", 3)]:
            nc.createDimension(d, n)
        variable(nc, "reflectance", ("downtrack", "crosstrack", "band"), 0)
        files["no_location.nc"] = ["reflectance"]
    # The location arrays on dimensions of the same sizes and other names.
    with netCDF4.Dataset(OUT / "renamed_location.nc", "w") as nc:
        for d, n in [("downtrack", ROWS), ("crosstrack", COLS), ("bands", 2)]:
            nc.createDimension(d, n)
        variable(nc, "radiance", ("downtrack", "crosstrack", "bands"), 0)
        location(nc, ("y", "x"))
        files["renamed_location.nc"] = ["radiance"]
    # Coordinate axes on the first two dimensions.
    with netCDF4.Dataset(OUT / "axes.nc", "w") as nc:
        for d, n in [("lat", ROWS), ("lon", COLS), ("bands", 2)]:
            nc.createDimension(d, n)
        lat = nc.createVariable("lat", "f8", ("lat",))
        lat.units = "degrees_north"
        lat[:] = 40.0 - np.arange(ROWS) * 0.5
        lon = nc.createVariable("lon", "f8", ("lon",))
        lon.units = "degrees_east"
        lon[:] = -100.0 + np.arange(COLS) * 0.25
        variable(nc, "radiance", ("lat", "lon", "bands"), 0)
        files["axes.nc"] = ["radiance"]
    # Irregular longitude and latitude axes: geolocation arrays.
    with netCDF4.Dataset(OUT / "irregular_axes.nc", "w") as nc:
        for d, n in [("lat", ROWS), ("lon", COLS), ("bands", 2)]:
            nc.createDimension(d, n)
        lat = nc.createVariable("lat", "f8", ("lat",))
        lat.units = "degrees_north"
        lat[:] = [40.0, 39.5, 38.0]
        lon = nc.createVariable("lon", "f8", ("lon",))
        lon.units = "degrees_east"
        lon[:] = [-100.0, -99.75, -99.0, -97.0]
        variable(nc, "radiance", ("lat", "lon", "bands"), 0)
        files["irregular_axes.nc"] = ["radiance"]
    # 2-D `lon` and `lat` in the root group, named by `coordinates` or not.
    with netCDF4.Dataset(OUT / "lon_lat.nc", "w") as nc:
        for d, n in [("y", ROWS), ("x", COLS), ("bands", 2)]:
            nc.createDimension(d, n)
        lon = nc.createVariable("lon", "f8", ("y", "x"))
        lon.units = "degrees_east"
        lon[:] = np.add.outer(np.arange(ROWS) * 0.1, np.arange(COLS))
        lat = nc.createVariable("lat", "f8", ("y", "x"))
        lat.units = "degrees_north"
        lat[:] = np.add.outer(np.arange(ROWS), np.arange(COLS) * 0.1)
        variable(nc, "radiance", ("y", "x", "bands"), 0)
        variable(nc, "with_coordinates", ("y", "x", "bands"), 100).coordinates = "lon lat"
        variable(nc, "bands_first", ("bands", "y", "x"), 200)
        files["lon_lat.nc"] = ["radiance", "with_coordinates", "bands_first"]
    return files


def main() -> None:
    """Writes the files and `expected.json`."""
    OUT.mkdir(parents=True, exist_ok=True)
    for f in OUT.glob("*"):
        f.unlink()
    expected = {}
    for file, names in write_files().items():
        path = str(OUT / file)
        for name in names:
            with rasterio.open(f'NETCDF:"{path}":{name}') as r:
                geolocation = {k: v.replace(path, "<file>") for k, v in r.tags(ns="GEOLOCATION").items()}
                expected[f"{file}:{name}"] = {
                    "count": r.count,
                    "height": r.height,
                    "width": r.width,
                    "values": r.read().astype("float64").ravel().tolist(),
                    "geotransform": None if r.transform.is_identity else list(r.transform.to_gdal()),
                    "geolocation": dict(sorted(geolocation.items())),
                }
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")


if __name__ == "__main__":
    main()
