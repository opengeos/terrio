"""Native numeric scientific arrays: no optional xarray/zarr runtime needed."""

import json

import numpy as np
import pytest

import terrio


def cube_store():
    """An in-memory chunked cube whose integers exceed floating-point precision."""
    files = {".zgroup": b'{"zarr_format":2}', ".zattrs": b"{}"}
    values = np.arange(24, dtype="<i8").reshape(3, 2, 4) + (2**53 + 7)
    arrays = {
        "cube": (values, ["time", "y", "x"], [1, 2, 2]),
        "time": (np.array([0, 6, 12], dtype="<i8"), ["time"], [3]),
        "y": (np.array([0, 1], dtype="<i8"), ["y"], [2]),
        "x": (np.arange(4, dtype="<i8"), ["x"], [4]),
    }
    for name, (array, dims, chunks) in arrays.items():
        files[f"{name}/.zarray"] = json.dumps(
            {
                "zarr_format": 2,
                "shape": list(array.shape),
                "chunks": chunks,
                "dtype": "<i8",
                "compressor": None,
                "fill_value": None,
                "order": "C",
                "filters": None,
            }
        ).encode()
        files[f"{name}/.zattrs"] = json.dumps({"_ARRAY_DIMENSIONS": dims}).encode()
        grid = tuple(
            (size + chunk - 1) // chunk for size, chunk in zip(array.shape, chunks, strict=True)
        )
        for index in np.ndindex(grid):
            region = tuple(
                slice(i * c, min((i + 1) * c, size))
                for i, c, size in zip(index, chunks, array.shape, strict=True)
            )
            files[f"{name}/" + ".".join(map(str, index))] = array[region].tobytes()
    return files, values


def test_native_lazy_named_views_and_exact_integer_snapshots(tmp_path):
    files, expected = cube_store()
    ds = terrio.open_multidim(files)
    assert "/cube" in ds.array_paths
    assert ds.group_paths == ["/"]
    array = ds["cube"]
    assert array.shape == (3, 2, 4)
    assert array.dimension_names == ["time", "y", "x"]
    assert array.info()["chunks"] == [1, 2, 2]
    assert array.coordinates == ["/time", "/y", "/x"]
    view = array.isel("time", 1).slice("x", 1, 2, 2).transpose(["x", "y"])
    np.testing.assert_array_equal(
        array.isel_axis(0, 1).slice_axis(1, 1, 2, 2).read(), expected[1, :, 1::2]
    )
    assert view.shape == (2, 2)
    np.testing.assert_array_equal(view.read(), expected[1, :, 1::2].T)
    assert view.read().dtype == np.int64
    assert view.read().flags.writeable
    assert array.slice("x", 4, 0).read().shape == (3, 2, 0)

    nc = terrio.open_multidim(ds.to_bytes())
    np.testing.assert_array_equal(nc["cube"].read(), expected)
    zarr = terrio.open_multidim(nc.to_zarr_store())
    np.testing.assert_array_equal(zarr["cube"].read(), expected)
    ds.write(tmp_path / "cube.nc")
    opened = terrio.open_multidim(tmp_path / "cube.nc")
    np.testing.assert_array_equal(opened["cube"].read(), expected)
    with pytest.raises(terrio.errors.InvalidArgumentError):
        ds.write(tmp_path / "cube.nc")
    del ds, array, files
    np.testing.assert_array_equal(view.read(), expected[1, :, 1::2].T)


def test_invalid_named_selections_are_clear_errors():
    ds = terrio.open_multidim(cube_store()[0])
    a = ds["cube"]
    for dimension, index in [("missing", 0), ("time", 3)]:
        with pytest.raises(terrio.errors.InvalidArgumentError):
            a.isel(dimension, index)
    with pytest.raises(terrio.errors.InvalidArgumentError):
        a.transpose(["x", "x", "time"])
    with pytest.raises(terrio.errors.InvalidArgumentError):
        a.slice("x", 0, 2, 0)


def test_unsupported_payloads_are_listed_without_hiding_numeric_arrays():
    files, expected = cube_store()
    files["text/.zarray"] = json.dumps(
        {
            "zarr_format": 2,
            "shape": [2],
            "chunks": [2],
            "dtype": "|S3",
            "compressor": None,
            "fill_value": None,
            "order": "C",
            "filters": None,
        }
    ).encode()
    files["text/.zattrs"] = b"{}"
    files["text/0"] = b"onetwo"
    ds = terrio.open_multidim(files)
    assert ds.info()["root"]["unsupported_arrays"][0]["path"] == "/text"
    np.testing.assert_array_equal(ds["cube"].read(), expected)
    with pytest.raises(terrio.UnsupportedError, match="/text"):
        ds.to_bytes()


def test_snapshot_rejects_malformed_fill_metadata():
    files, expected = cube_store()
    attributes = json.loads(files["cube/.zattrs"])
    attributes["_FillValue"] = "missing"
    files["cube/.zattrs"] = json.dumps(attributes).encode()
    ds = terrio.open_multidim(files)
    np.testing.assert_array_equal(ds["cube"].read(), expected)
    for write in (ds.to_bytes, ds.to_zarr_store):
        with pytest.raises(terrio.errors.InvalidArgumentError, match="_FillValue"):
            write()
