"""Point clouds: PLY, E57 and EPT; reading and writing in chunks; waveforms; PDAL's filters;
TIN and nearest-neighbour gridding.

The synthetic tests always run. The real-data tests run when `TERRIO_TEST_DATA` names the
directory filled by `scripts/fetch_test_data.sh`; their expected values come from PDAL 2.9 and
GDAL 3.12 (`gdal_grid`).
"""

import os

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
