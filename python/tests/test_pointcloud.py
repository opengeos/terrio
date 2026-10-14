"""Point clouds: LAS, LAZ and COPC as NumPy arrays.

The synthetic tests always run. The real-data tests run when `TERRIO_TEST_DATA` names the
directory filled by `scripts/fetch_test_data.sh`, and compare every value with laspy's when
laspy (with lazrs) is installed; their other expected values come from PDAL.
"""

import numpy as np
import pytest

import terrio


def make_points(n=4000, crs="EPSG:32617"):
    rng = np.random.default_rng(42)
    arrays = {
        "x": np.round(500000 + rng.uniform(0, 1000, n), 2),
        "y": np.round(4000000 + rng.uniform(0, 1000, n), 2),
        "z": np.round(100 + rng.uniform(0, 50, n), 2),
        "intensity": rng.integers(0, 65535, n).astype(np.uint16),
        "classification": rng.choice([1, 2, 6], n).astype(np.uint8),
        "return_number": np.ones(n, np.uint8),
        "number_of_returns": rng.integers(1, 4, n).astype(np.uint8),
        "gps_time": rng.uniform(1e8, 1e8 + 100, n),
        "red": rng.integers(0, 65535, n).astype(np.uint16),
        "green": rng.integers(0, 65535, n).astype(np.uint16),
        "blue": rng.integers(0, 65535, n).astype(np.uint16),
        "height": rng.normal(0, 5, n).astype(np.float32),
    }
    return terrio.PointCloud.from_dict(arrays, crs=crs), arrays


@pytest.mark.parametrize("ext", ["las", "laz", "copc.laz"])
def test_round_trip(tmp_path, ext):
    pc, arrays = make_points()
    path = tmp_path / f"points.{ext}"
    pc.write(str(path))
    f = terrio.open_points(str(path))
    assert f.format == {"las": "LAS", "laz": "LAZ", "copc.laz": "COPC"}[ext]
    assert f.point_count == len(pc)
    assert f.crs == "EPSG:32617"
    back = f.read()
    order = np.argsort(back["gps_time"]) if ext == "copc.laz" else np.arange(len(back))
    ref = np.argsort(arrays["gps_time"]) if ext == "copc.laz" else np.arange(len(back))
    for name, values in arrays.items():
        got = back[name][order]
        assert got.dtype == (values.dtype if name not in "xyz" else np.float64), name
        np.testing.assert_allclose(got, values[ref], rtol=0, atol=1e-9, err_msg=name)


def test_bytes_and_structured_arrays():
    pc, arrays = make_points(500)
    back = terrio.read_points_bytes(pc.to_bytes("LAZ"))
    table = back.to_numpy()
    assert table.dtype.names == tuple(back.dimensions)
    np.testing.assert_array_equal(table["intensity"], arrays["intensity"])
    d = back.to_dict()
    assert set(arrays) <= set(d)
    assert back.classification_counts() == {
        int(c): int(n) for c, n in zip(*np.unique(arrays["classification"], return_counts=True))
    }


def test_filters():
    pc, arrays = make_points()
    bounds = (500100.0, 4000100.0, 500600.0, 4000500.0)
    sub = pc.filter(bounds=bounds, classes=[2], returns=["only"])
    keep = (
        (arrays["x"] >= bounds[0])
        & (arrays["x"] <= bounds[2])
        & (arrays["y"] >= bounds[1])
        & (arrays["y"] <= bounds[3])
        & (arrays["classification"] == 2)
        & (arrays["number_of_returns"] == 1)
    )
    assert len(sub) == keep.sum() > 0
    np.testing.assert_array_equal(sub["intensity"], arrays["intensity"][keep])


def test_grid():
    arrays = {
        "x": np.array([0.5, 1.5, 1.6, 3.0]),
        "y": np.array([0.5, 0.5, 0.6, 2.0]),
        "z": np.array([10.0, 20.0, 30.0, 40.0]),
    }
    pc = terrio.PointCloud.from_dict(arrays)
    dem = pc.grid(1.0, stat="all", bounds=(0, 0, 3, 2))
    assert dem.shape == (6, 3, 4)
    a = dem.read()
    # Bottom-left cell: min, max, mean, idw, count, stdev of the three points within sqrt(2).
    np.testing.assert_allclose(a[:, 2, 0], [10, 30, 20, 10, 3, np.sqrt(200 / 3)])
    assert a[0, 0, 0] == -9999


def test_reproject_and_layer():
    pc, _ = make_points(100)
    wgs84 = pc.reproject("EPSG:4326")
    assert wgs84.crs == "EPSG:4326"
    assert -82 < wgs84["x"].min() < -80
    layer = pc.to_layer(dimensions=["z", "classification"])
    fc = layer.to_geojson()
    assert len(fc["features"]) == 100
    assert set(fc["features"][0]["properties"]) == {"z", "classification"}


def laspy_or_skip():
    return pytest.importorskip("laspy")


def test_autzen(data_dir):
    path = data_dir / "pointcloud" / "autzen_trim.laz"
    pc = terrio.read_points(str(path))
    assert len(pc) == 110000
    assert pc["x"][0] == 637177.98
    assert pc.classification_counts() == {1: 83893, 2: 26107}
    info = terrio.info(str(path))
    assert info["type"] == "pointcloud"
    assert info["driver"] == "LAZ"
    assert info["points_by_return"][:4] == [99257, 9021, 1623, 99]


def test_copc_levels_and_bounds(data_dir):
    f = terrio.open_points(str(data_dir / "pointcloud" / "madison.copc.laz"))
    assert f.format == "COPC"
    assert f.crs == "EPSG:2255"
    # PDAL's readers.copc reads the same counts.
    assert [len(f.read(max_level=level)) for level in range(3)] == [36870, 422783, 1436089]
    bounds = (2354499.99, 1077499.996, 2355999.99, 1079499.992)
    assert len(f.read(bounds=bounds)) == 371839
    nodes = f.copc_nodes()
    assert sum(n["point_count"] for n in nodes) == f.point_count


@pytest.mark.parametrize(
    "name",
    ["1.2-with-color.las", "extrabytes.las", "test1_4.las", "1_4_w_evlr.laz", "simple.copc.laz"]
    + [f"synthetic/format{f}.{e}" for f in range(11) for e in ("las", "laz")],
)
def test_matches_laspy(data_dir, name):
    laspy = laspy_or_skip()
    path = data_dir / "pointcloud" / name
    if not path.exists():
        pytest.skip(f"{name} is missing")
    las = laspy.read(path)
    pc = terrio.read_points(str(path))
    np.testing.assert_array_equal(pc["x"], las.x)
    np.testing.assert_array_equal(pc["z"], las.z)
    for dim in las.point_format.dimension_names:
        if dim in ("X", "Y", "Z"):
            continue
        values = np.asarray(las[dim])
        if values.ndim == 2:
            for k in range(values.shape[1]):
                np.testing.assert_array_equal(pc[f"{dim}_{k}"], values[:, k], err_msg=f"{dim}_{k}")
        else:
            np.testing.assert_array_equal(pc[dim], values, err_msg=dim)


def test_convert_to_vector(data_dir, tmp_path):
    out = tmp_path / "ground.gpkg"
    terrio.convert_points(
        str(data_dir / "pointcloud" / "test_utm16.las"), str(out), dimensions=["z", "intensity"]
    )
    layer = terrio.open_vector(str(out)).layers[0]
    assert len(layer) == 10
    assert layer.crs == "EPSG:26916"
