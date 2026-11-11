"""Point clouds: PLY, E57 and EPT; reading and writing in chunks; waveforms; PDAL's filters;
TIN and nearest-neighbour gridding.

The synthetic tests always run. The real-data tests run when `TERRIO_TEST_DATA` names the
directory filled by `scripts/fetch_test_data.sh`; their expected values come from PDAL 2.9 and
GDAL 3.12 (`gdal_grid`).
"""

import http.server
import json
import os
import subprocess
import sys
import threading

import numpy as np
import pytest

import terrio


def make_points(n=20000):
    """Makes points on the 0.01 grid over 100 x 100 m.

    Args:
        n: The number of points.

    Returns:
        terrio.PointCloud: The points.
    """
    rng = np.random.default_rng(7)
    arrays = {
        "x": np.round(500000 + rng.uniform(0, 100, n), 2),
        "y": np.round(4000000 + rng.uniform(0, 100, n), 2),
        "z": np.round(100 + rng.uniform(0, 10, n), 2),
        "intensity": rng.integers(0, 1000, n).astype(np.uint16),
        "return_number": np.ones(n, np.uint8),
        "number_of_returns": np.ones(n, np.uint8),
        "classification": rng.choice([1, 2, 6], n).astype(np.uint8),
    }
    return terrio.PointCloud.from_dict(arrays, crs="EPSG:32617")


@pytest.mark.parametrize("ext", ["ply", "e57"])
def test_ply_and_e57_round_trip(tmp_path, ext):
    pc = make_points()
    path = tmp_path / f"points.{ext}"
    pc.write(str(path))
    f = terrio.open_points(str(path))
    assert f.format == ext.upper()
    back = f.read()
    for name in ["x", "y", "z", "intensity"]:
        np.testing.assert_array_equal(back[name], pc[name])


def test_chunks_and_writer(tmp_path):
    pc = make_points()
    pc.write(str(tmp_path / "a.laz"))
    f = terrio.open_points(str(tmp_path / "a.laz"))
    chunks = list(f.chunks(5000))
    assert len(chunks) == 4
    with terrio.PointCloudWriter(str(tmp_path / "b.copc.laz"), f.template()) as w:
        for c in chunks:
            w.write(c)
    back = terrio.read_points(str(tmp_path / "b.copc.laz"))
    assert len(back) == len(pc)
    np.testing.assert_array_equal(np.sort(back["x"]), np.sort(pc["x"]))


def test_filters_and_grid_methods():
    pc = make_points()
    assert len(pc.range("Classification[2:2]")) == int(np.sum(pc["classification"] == 2))
    assert len(pc.voxel_downsize(5.0, "first")) <= 21 * 21 * 3
    assert len(pc.voxel_center_nearest_neighbor(5.0)) <= 20 * 20 * 3
    assert 0 < len(pc.sample(1.0)) < len(pc)
    classes = pc.outlier()["classification"]
    assert np.sum(classes == 7) > 0
    ground = pc.smrf()["classification"]
    assert set(np.unique(ground)) <= {1, 2}
    tin = pc.grid(1.0, method="linear")
    nearest = pc.grid(1.0, method="nearest")
    assert tin.read(1).shape == nearest.read(1).shape
    assert np.all(nearest.read(1) >= 100) and np.all(nearest.read(1) <= 110)


def test_more_pdal_filters():
    pc = make_points()
    for ground in [pc.pmf(), pc.csf(resolution=2.0)]:
        assert set(np.unique(ground["classification"])) <= {1, 2}
    assert np.sum(pc.elm(cell=5.0, threshold=0.5)["classification"] == 7) > 0
    classified = pc.smrf()
    for hag in [classified.hag_nn(count=3), classified.hag_delaunay()]:
        h = hag["height_above_ground"]
        assert h.dtype == np.float64 and np.all(h[classified["classification"] == 2] == 0)
    box = pc.crop(bounds=["([500010, 500020], [4000010, 4000020])"])
    x, y = pc["x"], pc["y"]
    inside = (x >= 500010) & (x <= 500020) & (y >= 4000010) & (y <= 4000020)
    np.testing.assert_array_equal(box["x"], x[inside])
    assert len(pc.crop(bounds=["([500010, 500020], [4000010, 4000020])"], outside=True)) == len(
        pc
    ) - int(inside.sum())
    assert len(pc.crop(points=[[500050.0, 4000050.0]], distance=10.0)) > 0
    assert (
        len(
            pc.crop(
                polygons=[
                    "POLYGON((500000 4000000, 500100 4000000, 500000 4000100, 500000 4000000))"
                ]
            )
        )
        > 0
    )
    np.testing.assert_array_equal(pc.decimation(3)["x"], x[::3])
    high = pc.expression("Classification == 2 && Z > 105")
    assert np.all(high["classification"] == 2) and np.all(high["z"] > 105)
    assigned = pc.assign(
        assignment="Classification[6:6]=5", value="Score = Z * 2 WHERE Classification == 5"
    )
    assert not np.any(assigned["classification"] == 6)
    np.testing.assert_array_equal(
        assigned["score"], np.where(assigned["classification"] == 5, pc["z"] * 2, 0)
    )
    merged = terrio.merge_points([pc, pc.decimation(2)])
    assert len(merged) == len(pc) + (len(pc) + 1) // 2
    tiles = pc.splitter(50.0, origin=(500000.0, 4000000.0))
    assert len(tiles) == 4 and sum(len(t) for t in tiles) == len(pc)
    chips = pc.chipper(5000)
    assert [len(c) for c in chips] == [5000] * 4
    normals = pc.normal(knn=8)
    assert np.all(normals["normal_z"] >= 0) and np.all(normals["curvature"] >= 0)
    ids = pc.cluster(2.0, is3d=False)["cluster_id"]
    assert ids.dtype == np.uint64 and ids.min() >= 1


def test_neighbourhood_and_ordering_filters():
    pc = make_points()
    ids = pc.dbscan(2.0, min_points=4, dimensions=["X", "Y"])["cluster_id"]
    assert ids.dtype == np.int64 and ids.min() >= -1
    with pytest.raises(terrio.InvalidArgumentError, match="OptimalKNN"):
        pc.covariance_features(feature_set="All")
    features = pc.covariance_features(
        feature_set=["Linearity", "Planarity", "Scattering"], mode="raw"
    )
    lin, plan, scat = (features[d] for d in ("linearity", "planarity", "scattering"))
    np.testing.assert_allclose(lin + plan + scat, 1.0, rtol=0, atol=1e-9)
    ev = pc.eigenvalues(normalize=True)
    np.testing.assert_allclose(
        ev["eigenvalue0"] + ev["eigenvalue1"] + ev["eigenvalue2"], 1.0, rtol=0, atol=1e-12
    )
    assert set(np.unique(pc.approximate_coplanar()["coplanar"])) <= {0, 1}
    assert pc.estimate_rank()["rank"].max() <= 3
    lof = pc.lof(10)
    assert np.all(lof["nndistance"] > 0) and np.all(lof["local_outlier_factor"] > 0)
    assert np.all(pc.nndistance(4, mode="avg")["nndistance"] > 0)
    assert np.all(pc.radial_density(5.0)["radial_density"] > 0)
    assert np.all((pc.reciprocity()["reciprocity"] >= 0) & (pc.reciprocity()["reciprocity"] <= 100))
    assert np.all((pc.miniball()["miniball"] >= 0) & (pc.miniball()["miniball"] <= 1))
    smooth = pc.zsmooth("Smoothed", radius=2.0)["smoothed"]
    assert smooth.dtype == np.float64 and smooth.min() >= 100 and smooth.max() <= 110
    ground = pc.skewness_balancing()
    assert np.all(np.diff(ground["z"]) >= 0) and set(np.unique(ground["classification"])) <= {1, 2}
    np.testing.assert_array_equal(pc.head(5)["x"], pc["x"][:5])
    np.testing.assert_array_equal(pc.tail(5, invert=True)["x"], pc["x"][:-5])
    assert np.all(np.diff(pc.sort("Z", order="DESC")["z"]) <= 0)
    assert pc.locate("Intensity")["intensity"][0] == pc["intensity"].max()
    assert len(pc.iqr("Z")) == len(pc) and len(pc.mad("Z", k=1.0)) < len(pc)


def test_hexbin():
    # A ring of the six hexagons around an empty one, and the first point's hexagon apart: PDAL
    # 2.10 gives two polygons, one with a hole, of 18.1865335 square units.
    h = 3**0.5

    def centre(c, r):
        return (0.5 + 1.5 * c, h / 2 + h * r + (h / 2 if c % 2 else 0.0))

    xy = [(0.0, 0.0)] + [centre(c, r) for c, r in [(1, 1), (1, 2), (2, 1), (2, 3), (3, 1), (3, 2)]]
    pc = terrio.PointCloud.from_dict(
        {
            "x": np.array([p[0] for p in xy]),
            "y": np.array([p[1] for p in xy]),
            "z": np.zeros(len(xy)),
        },
        crs="EPSG:32614",
    )
    out = pc.hexbin(1.0, threshold=1, smooth=False)
    assert out["boundary"].startswith("MULTIPOLYGON (((")
    assert out["area"] == pytest.approx(18.1865335, abs=1e-6)
    assert out["density"] == pytest.approx(7 / out["area"])
    assert (out["point_count"], out["hex_count"], out["threshold"]) == (7, 7, 1)
    assert out["origin"] == (0.0, 0.0)
    assert out["boundary_json"]["type"] == "MultiPolygon"
    polygons = out["boundary_json"]["coordinates"]
    assert len(polygons) == 2 and sorted(len(p) - 1 for p in polygons) == [0, 1]
    layer = out["boundary_layer"]
    assert (
        len(layer) == 1 and layer.geometry_type == "MultiPolygon" and "32614" in (layer.crs or "")
    )
    hexagons = out["hexagons"].__geo_interface__["features"]
    assert sorted(f["properties"]["count"] for f in hexagons) == [1] * 7
    # A threshold no hexagon reaches; no points; no edge length.
    assert pc.hexbin(1.0, threshold=2)["boundary"] == "MULTIPOLYGON EMPTY"
    assert pc.head(0).hexbin(1.0)["area"] == 0.0
    with pytest.raises(terrio.InvalidArgumentError, match="edge length"):
        pc.hexbin(-1.0)
    # Without an edge length, PDAL's estimate: threshold x the mean distance between consecutive
    # points (the sum over the 6 steps / 7 points), the hexagons' height.
    steps = np.hypot(np.diff(pc["x"]), np.diff(pc["y"])).sum()
    estimated = pc.hexbin(threshold=1, smooth=False)
    assert estimated["estimated_edge"] == pytest.approx(steps / 7)
    assert estimated["edge_length"] == pytest.approx(steps / 7 / h)
    with pytest.raises(terrio.InvalidArgumentError, match="topology"):
        pc.hexbin(1.0, preserve_topology=False)
    # Smoothing (the default) drops the lone hexagon, smaller than 6 x tolerance².
    assert len(pc.hexbin(1.0, threshold=1)["boundary_json"]["coordinates"]) == 1


def test_raster_and_vector_filters(tmp_path):
    pc = make_points()
    # A plane z = 100 + x / 10 over the points, two bands; and two squares of zones.
    cols = np.arange(100)
    plane = np.tile(100 + (cols + 0.5) / 10, (100, 1))
    raster = terrio.Raster.from_array(
        np.stack([plane, plane * 2]), (500000.0, 1.0, 0.0, 4000100.0, 0.0, -1.0), crs="EPSG:32617"
    )
    near = pc.dem(raster, "Z[0:2]")
    assert 0 < len(near) < len(pc)
    path = tmp_path / "plane.tif"
    raster.write(str(path))
    assert len(pc.dem(str(path), "Z[0:2]")) == len(near)
    colored = pc.colorization(raster, dimensions="Red:1:1.0, Green:2:0.5")
    assert colored["red"].dtype == np.float64
    np.testing.assert_allclose(colored["red"], colored["green"])

    def square(x0, zone):
        ring = [[x0, 4e6], [x0 + 50, 4e6], [x0 + 50, 4000100], [x0, 4000100], [x0, 4e6]]
        geometry = {"type": "Polygon", "coordinates": [ring]}
        return {"type": "Feature", "properties": {"name": "z", "zone": zone}, "geometry": geometry}

    squares = {"type": "FeatureCollection", "features": [square(500000, 3), square(500050, 4)]}
    layer = terrio.Layer.from_geojson(squares, crs="EPSG:32617")
    zoned = pc.overlay(layer, "Classification")
    expected = np.where(pc["x"] <= 500050, 3, 4)
    np.testing.assert_array_equal(zoned["classification"], expected)


def test_ept_written_and_read(tmp_path):
    pc = make_points()
    for data_type in ["laszip", "binary", "zstandard"]:
        path = tmp_path / data_type / "ept.json"
        pc.write(str(path), ept_data_type=data_type)
        f = terrio.open_points(str(path))
        assert f.format == "EPT" and f.ept_info["data_type"] == data_type
        back = f.read()
        np.testing.assert_allclose(np.sort(back["x"]), np.sort(pc["x"]), rtol=0, atol=1e-6)
    terrio.convert_points(
        str(tmp_path / "laszip" / "ept.json"), str(tmp_path / "points.e57"), chunk_points=3000
    )
    assert len(terrio.read_points(str(tmp_path / "points.e57"))) == len(pc)


def test_ept_append(tmp_path):
    pc = make_points()
    x0, y0, z0 = (float(np.min(pc[k])) for k in "xyz")
    x1, y1, z1 = (float(np.max(pc[k])) for k in "xyz")
    mid = (x0 + x1) / 2
    first, second = pc.range(f"X[:{mid})"), pc.range(f"X[{mid}:]")
    second.write(str(tmp_path / "second.laz"))
    for data_type in ["laszip", "binary", "zstandard"]:
        path = tmp_path / data_type / "ept.json"
        first.write(str(path), ept_data_type=data_type, ept_bounds=(x0, y0, z0, x1, y1, z1))
        terrio.append_points(str(path), [str(tmp_path / "second.laz")], leaf_points=500)
        f = terrio.open_points(str(path))
        assert f.ept_info["points"] == len(pc)
        back = f.read()
        np.testing.assert_allclose(np.sort(back["x"]), np.sort(pc["x"]), rtol=0, atol=1e-6)
        manifest = json.loads((tmp_path / data_type / "ept-sources" / "manifest.json").read_text())
        assert [s["points"] for s in manifest] == [len(first), len(second)]
    # Through convert_points, and refused outside the cube.
    terrio.convert_points(
        str(tmp_path / "second.laz"), str(tmp_path / "laszip" / "ept.json"), append=True
    )
    assert terrio.open_points(str(tmp_path / "laszip" / "ept.json")).ept_info["points"] == len(
        pc
    ) + len(second)
    with pytest.raises(terrio.TerrioError):
        terrio.convert_points(str(tmp_path / "second.laz"), str(tmp_path / "x.laz"), append=True)


EPT_READ_CHILD = r"""
import json, sys
import terrio, terrio.rio as rio
source, where, options = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])


def read():
    try:
        return len(terrio.read_points(source))
    except Exception as e:
        return f"{type(e).__name__}: {e}"


if where == "Env":
    with rio.Env(**options):
        print(json.dumps(read()))
else:
    print(json.dumps(read()))
"""


@pytest.fixture(scope="module")
def ept_nodes(tmp_path_factory):
    """EPT datasets whose nodes are LAZ, of one node (100 points) and of nine (100,001 points:
    the root and its eight children), served by a local HTTP server that records each request's
    path and its `X-Probe` and `User-Agent` headers."""
    root = tmp_path_factory.mktemp("ept-nodes")
    rng = np.random.default_rng(0)
    for name, n, nodes in [("one", 100, 1), ("nine", 100_001, 9)]:
        points = {axis: rng.uniform(0, 1000, n) for axis in "xyz"}
        cloud = terrio.PointCloud.from_dict(points, crs="EPSG:32633")
        cloud.write(str(root / name), format="EPT", ept_data_type="laszip")
        assert len(os.listdir(root / name / "ept-data")) == nodes
    seen = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            seen.append((self.path, self.headers.get("X-Probe"), self.headers.get("User-Agent")))
            path = (root / self.path.lstrip("/")).resolve()
            inside = path.is_relative_to(root.resolve()) and path.is_file()
            data = path.read_bytes() if inside else b""
            self.send_response(200 if inside else 404)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield root, f"http://127.0.0.1:{server.server_address[1]}", seen
    server.shutdown()


def read_in_child(source, where, **options):
    """The points read from `source` in a child process, or its error: with the options set with
    `terrio.rio.Env`, or in the child's environment."""
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith(("GDAL_HTTP", "TERRIO_HTTP", "TERRIO_ALLOWED_DRIVERS"))
    }
    if where == "environment":
        env.update(options)
    p = subprocess.run(
        [sys.executable, "-c", EPT_READ_CHILD, source, where, json.dumps(options)],
        capture_output=True,
        text=True,
        timeout=120,
        env=env,
    )
    assert p.returncode == 0, p.stderr[-600:]
    return json.loads(p.stdout)


@pytest.mark.parametrize("where", ["Env", "environment"])
@pytest.mark.parametrize(("name", "nodes", "points"), [("one", 1, 100), ("nine", 9, 100_001)])
def test_every_request_of_an_ept_read_carries_the_http_options(
    ept_nodes, name, nodes, points, where
):
    """Every request of a read of a remote EPT dataset carries the header and the user agent set
    with `terrio.rio.Env` or in the environment, its nodes' too, whether the read takes one node
    or nine."""
    _, base, seen = ept_nodes
    seen.clear()
    options = {"GDAL_HTTP_HEADERS": "X-Probe: set", "GDAL_HTTP_USERAGENT": "probe/1.0"}
    assert read_in_child(f"{base}/{name}/ept.json", where, **options) == points
    assert len([path for path, _, _ in seen if path.startswith(f"/{name}/ept-data/")]) == nodes
    assert {(probe, agent) for _, probe, agent in seen} == {("set", "probe/1.0")}, seen


@pytest.mark.parametrize("where", ["Env", "environment"])
@pytest.mark.parametrize("name", ["one", "nine"])
def test_an_ept_dataset_in_laz_is_refused_without_its_nodes_driver(ept_nodes, name, where):
    """Under `TERRIO_ALLOWED_DRIVERS=EPT`, set with `terrio.rio.Env` or in the environment, an
    EPT dataset whose nodes are LAZ is refused, whether a read takes one node or nine: the LAS
    reader would read them."""
    root, _, _ = ept_nodes
    assert read_in_child(str(root / name / "ept.json"), where, TERRIO_ALLOWED_DRIVERS="EPT") == (
        "UnsupportedError: '0-0-0-0.laz' holds LAS data, and the LAS driver is not allowed"
        " (allowed: EPT)"
    )


def test_ept_and_filters_as_pdal(data_dir):
    pc = os.path.join(data_dir, "pointcloud")
    f = terrio.open_points(os.path.join(pc, "ept", "lone-star-laszip", "ept.json"))
    assert f.format == "EPT"
    assert f.ept_info["data_type"] == "laszip"
    assert len(f.read(resolution=1.0)) == 41998
    autzen = terrio.read_points(os.path.join(pc, "autzen_trim.las"))
    assert int(np.sum(autzen.smrf()["classification"] == 2)) == 86660
    assert int(np.sum(autzen.outlier()["classification"] == 7)) == 4133
    assert len(autzen.sample(1.0)) == 103846
    assert int(np.sum(autzen.pmf()["classification"] == 2)) == 81954
    assert int(np.sum(autzen.csf(resolution=5.0, rigidness=2)["classification"] == 2)) == 35440
    assert int(np.sum(autzen.elm()["classification"] == 7)) == 202
    assert autzen.smrf().hag_nn()["height_above_ground"].sum() == pytest.approx(625301.02)
    assert len(autzen.chipper()) == 22
    ids = autzen.dbscan(3.0, min_points=10)["cluster_id"]
    assert (ids.max(), int(np.sum(ids == -1))) == (359, 95169)
    assert autzen.covariance_features()["linearity"][0] == 0.6777488111278865
    assert autzen.lof()["local_outlier_factor"][0] == 1.0315897563238794
    assert autzen.reciprocity()["reciprocity"].sum() == 1061100.0
    assert len(autzen.iqr("Z")) == 82454 and len(autzen.mad("Z")) == 80067
    dem = terrio.grid_points(os.path.join(pc, "autzen_trim.las"), 10.0, method="nearest")
    assert dem.read(1).sum() == pytest.approx(2833138.81)


def test_waveforms(data_dir):
    f = terrio.open_points(
        os.path.join(data_dir, "pointcloud", "waveform", "full_waveform_tutorial_data.las")
    )
    d = f.wave_packet_descriptors[0]
    assert (d["bits_per_sample"], d["number_of_samples"]) == (8, 256)
    samples = f.waveform(1, 60, 256)
    assert int(samples.sum()) == 3782
