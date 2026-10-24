"""Compare native multidimensional metadata, reads and snapshots with GDAL MD and xarray.

Real inputs are NOAA wind, JPSS geolocation, Sentinel-5P methane and a GDAL-derived
SRTM Zarr store from fetch_test_data.sh. A four-dimensional chunked cube uses real
wind values to exercise time/depth indexing, coordinate preservation, unlimited
axes and damaged-unselected-chunk checks. Values stay packed and source ordered.
"""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

import h5py
import numpy as np
import xarray as xr
from osgeo import gdal

gdal.UseExceptions()


def run(cli, *args, expect_error=False):
    """Run a command, failing on unexpected exit status."""
    result = subprocess.run([str(cli), *map(str, args)], capture_output=True, text=True)
    if expect_error:
        assert result.returncode != 0, "damaged unselected payload was unexpectedly readable"
        return None
    if result.returncode:
        raise RuntimeError(result.stderr.strip())
    return json.loads(result.stdout) if result.stdout.strip() else None


def arrays(group):
    """Numeric array metadata indexed by absolute name."""
    out = {a["path"]: a for a in group["arrays"]}
    for child in group["groups"]:
        out.update(arrays(child))
    return out


def mdarray(dataset, path):
    """Open one GDAL MD array by absolute name."""
    parts = path.strip("/").split("/")
    group = dataset.GetRootGroup()
    for name in parts[:-1]:
        group = group.OpenGroup(name)
    return group.OpenMDArray(parts[-1])


def dtype(array):
    """GDAL's numeric name and native NumPy type."""
    code = array.GetDataType().GetNumericDataType()
    names = {"Byte": "uint8", "Int8": "int8", "UInt16": "uint16", "Int16": "int16", "UInt32": "uint32", "Int32": "int32", "UInt64": "uint64", "Int64": "int64", "Float32": "float32", "Float64": "float64"}
    name = gdal.GetDataTypeName(code)
    return name, np.dtype(names[name])


def read_case(cli, source, name, indices=None, slices=None, transpose=None):
    """Compare a named-axis Terrio view with GDAL's exact source hyperslab."""
    info = arrays(run(cli, "mdinfo", source, "--json")["root"])[name]
    dimensions = info["dimensions"]
    names = [d["name"] for d in dimensions]
    start = [0] * len(names)
    count = [d["size"] for d in dimensions]
    step = [1] * len(names)
    args = ["mdread", source, "--array", name]
    indices = indices or {}
    for dimension, index in indices.items():
        axis = names.index(dimension)
        start[axis], count[axis] = index, 1
        args += ["--isel", f"{dimension}={index}"]
    for dimension, (origin, length, stride) in (slices or {}).items():
        axis = names.index(dimension)
        start[axis], count[axis], step[axis] = origin, length, stride
        args += ["--slice", f"{dimension}={origin},{length},{stride}"]
    active = [i for i, name in enumerate(names) if name not in indices]
    if transpose:
        args += ["--transpose", ",".join(transpose)]
    ds = gdal.OpenEx(str(source), gdal.OF_MULTIDIM_RASTER)
    ref = mdarray(ds, name)
    ref_type, np_type = dtype(ref)
    expected = np.asarray(ref.ReadAsArray(array_start_idx=start, count=count, array_step=step)).reshape(count)
    expected = np.squeeze(expected, axis=tuple(i for i in range(len(names)) if i not in active))
    if transpose:
        active_names = [names[i] for i in active]
        expected = expected.transpose([active_names.index(n) for n in transpose])
    actual = run(cli, *args)
    assert actual["data_type"] == ref_type, (actual["data_type"], ref_type)
    values = np.asarray(actual["values"], dtype=np_type).reshape(actual["shape"])
    np.testing.assert_array_equal(values, expected)
    return values


def compare_metadata(cli, source, selected):
    """Check named dimensions and coordinate-array associations against GDAL MD."""
    info = arrays(run(cli, "mdinfo", source, "--json")["root"])
    ds = gdal.OpenEx(str(source), gdal.OF_MULTIDIM_RASTER)
    for path in selected:
        actual = info[path]
        reference = mdarray(ds, path)
        assert actual["data_type"] == dtype(reference)[0]
        dimensions = reference.GetDimensions()
        assert [(d["name"], d["size"]) for d in actual["dimensions"]] == [(d.GetName(), d.GetSize()) for d in dimensions]
        for a, d in zip(actual["dimensions"], dimensions):
            coordinate = d.GetIndexingVariable()
            if coordinate:
                assert a["indexing_variable"] == coordinate.GetFullName()


def snapshot_equal(cli, source, destination):
    """Compare every wind/cube variable, coordinates, attributes and source chunks."""
    run(cli, "mdconvert", source, destination)
    open_xr = lambda p: xr.open_zarr(p, decode_cf=False) if p.suffix == ".zarr" else xr.open_dataset(p, decode_cf=False)
    with open_xr(source) as expected, open_xr(destination) as actual:
        assert set(actual.variables) == set(expected.variables)
        for name in expected.variables:
            assert actual[name].dims == expected[name].dims, (name, actual[name].dims, expected[name].dims)
            assert actual[name].dtype == expected[name].dtype
            np.testing.assert_array_equal(actual[name].values, expected[name].values)
            for key in ("units", "calendar", "scale_factor", "add_offset"):
                assert actual[name].attrs.get(key) == expected[name].attrs.get(key)
        gdal_ds = gdal.OpenEx(str(destination), gdal.OF_MULTIDIM_RASTER)
        for name in expected.variables:
            np.testing.assert_array_equal(mdarray(gdal_ds, "/" + name).ReadAsArray(), expected[name].values)
    before = arrays(run(cli, "mdinfo", source, "--json")["root"])
    after = arrays(run(cli, "mdinfo", destination, "--json")["root"])
    for name, array in before.items():
        if array["chunks"]:
            assert after[name]["chunks"] == array["chunks"]
        assert [(d["path"], d["size"], d["unlimited"], d["indexing_variable"]) for d in after[name]["dimensions"]] == [(d["path"], d["size"], d["unlimited"], d["indexing_variable"]) for d in array["dimensions"]]


def comparisons(data, work, cli):
    """Execute independent cases and retain a useful report when a case fails."""
    results = []
    def case(operation, name, function):
        try:
            function()
            results.append({"operation": operation, "name": name, "status": "identical", "detail": ""})
        except Exception as exc:
            results.append({"operation": operation, "name": name, "status": "differs", "detail": f"{type(exc).__name__}: {exc}"})
    wind = data / "wind_global.nc"
    case("metadata", "NOAA wind named axes and indexing variables", lambda: compare_metadata(cli, wind, ["/u_wind", "/v_wind", "/lat", "/lon"]))
    case("read", "NOAA wind positive-stride transpose", lambda: read_case(cli, wind, "/u_wind", slices={"lat": (5, 4, 3), "lon": (12, 5, 2)}, transpose=["lon", "lat"]))
    case("read", "SRTM packed hyperslab", lambda: read_case(cli, data / "srtm_sub.nc", "/Band1", slices={"lat": (10, 3, 2), "lon": (20, 4, 3)}))
    hdf = data / "GATMO_j01_d20230101_t0000026_e0000343_b26525_c20230101002941320788_oeac_ops.h5"
    case("read", "JPSS HDF5 user-block latitude slab", lambda: read_case(cli, hdf, "/All_Data/ATMS-SDR-GEO_All/Latitude", slices={"Latitude_dim0": (1, 2, 1), "Latitude_dim1": (3, 3, 2)}))
    sentinel = data / "S5P_OFFL_L2__CH4____20230101T000319_20230101T014449_27036_03_020400_20230103T110255.nc"
    path = "/PRODUCT/SUPPORT_DATA/INPUT_DATA/methane_profile_apriori"
    case("metadata", "Sentinel-5P inherited axes and auxiliary coordinates", lambda: compare_metadata(cli, sentinel, [path]))
    case("read", "Sentinel-5P time/layer slice and transpose", lambda: read_case(cli, sentinel, path, indices={"time": 0, "layer": 3}, slices={"scanline": (20, 3, 2), "ground_pixel": (30, 4, 2)}, transpose=["ground_pixel", "scanline"]))
    case("read", "SRTM Zarr v3 chunk-edge slab", lambda: read_case(cli, data / "srtm_sub_v3.zarr", "/srtm_sub_v3", slices={"Y": (250, 10, 1), "X": (250, 12, 2)}, transpose=["X", "Y"]))
    zarr = work / "wind.zarr"
    back = work / "wind.nc"
    case("snapshot", "NOAA wind netCDF to Zarr coordinates", lambda: snapshot_equal(cli, wind, zarr))
    case("snapshot", "NOAA wind Zarr to netCDF coordinates and chunks", lambda: snapshot_equal(cli, zarr, back))

    cube = work / "cube.nc"
    cube_zarr = work / "cube.zarr"
    cube_back = work / "cube_back.nc"
    def make_cube():
        with xr.open_dataset(wind, decode_cf=False) as original:
            sample = original.u_wind.isel(lat=slice(0, 8), lon=slice(0, 12)).load()
        values = np.broadcast_to(sample.values, (3, 2, 8, 12)).copy()
        ds = xr.Dataset({"wind": (("time", "depth", "lat", "lon"), values)}, coords={"time": [0, 6, 12], "depth": [0., 10.], "lat": sample.lat.values, "lon": sample.lon.values})
        ds.time.attrs["units"] = "hours since 2000-01-01"
        ds.depth.attrs.update({"units": "m", "positive": "down"})
        ds.to_netcdf(cube, engine="h5netcdf", unlimited_dims=["time"], encoding={"wind": {"chunksizes": (1, 1, 4, 6), "zlib": True, "complevel": 1}})
    case("metadata", "wind-derived four-dimensional chunked cube", make_cube)
    case("read", "wind-derived cube time/depth view", lambda: read_case(cli, cube, "/wind", indices={"time": 1, "depth": 1}, slices={"lat": (1, 3, 1), "lon": (2, 4, 2)}, transpose=["lon", "lat"]))
    case("snapshot", "four-dimensional netCDF to Zarr chunk and coordinate preservation", lambda: snapshot_equal(cli, cube, cube_zarr))
    case("snapshot", "four-dimensional Zarr to netCDF unlimited axis preservation", lambda: snapshot_equal(cli, cube_zarr, cube_back))
    def lazy_zarr():
        # The uncompressed chunk holds 24 Float64 values; truncating it must fail if fetched.
        (cube_zarr / "wind/2.1.0.0").write_bytes(b"x")
        run(cli, "mdinfo", cube_zarr, "--json")
        values = read_case(cli, cube_zarr, "/wind", indices={"time": 0, "depth": 0}, slices={"lat": (0, 2, 1), "lon": (0, 3, 1)})
        assert values.shape == (2, 3)
        run(cli, "mdread", cube_zarr, "--array", "/wind", expect_error=True)
    case("lazy", "Zarr slice skips damaged unselected chunks", lazy_zarr)
    def lazy_netcdf():
        damaged = work / "damaged.nc"
        shutil.copyfile(cube, damaged)
        with h5py.File(damaged, "r") as f:
            chunk = f["wind"].id.get_chunk_info_by_coord((2, 1, 0, 0))
        with damaged.open("r+b") as f:
            f.seek(chunk.byte_offset)
            f.write(b"x" * chunk.size)
        run(cli, "mdinfo", damaged, "--json")
        read_case(cli, damaged, "/wind", indices={"time": 0, "depth": 0}, slices={"lat": (0, 2, 1), "lon": (0, 3, 1)})
        run(cli, "mdread", damaged, "--array", "/wind", expect_error=True)
    case("lazy", "netCDF slice skips damaged unselected compressed chunks", lazy_netcdf)
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", type=Path, default=Path("target/release/terrio"))
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as tmp:
        results = comparisons(args.data.resolve(), Path(tmp), args.terrio.resolve())
    if args.json:
        print(json.dumps(results))
    else:
        for result in results:
            print(f"{result['name']}: {result['status']} {result['detail']}")
    return int(any(r["status"] != "identical" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
