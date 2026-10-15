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
    dem = terrio.grid_points(os.path.join(pc, "autzen_trim.las"), 10.0, method="nearest")
    assert dem.read(1).sum() == pytest.approx(2833138.81)


def test_waveforms(data_dir):
    f = terrio.open_points(os.path.join(data_dir, "pointcloud", "waveform", "full_waveform_tutorial_data.las"))
    d = f.wave_packet_descriptors[0]
    assert (d["bits_per_sample"], d["number_of_samples"]) == (8, 256)
    samples = f.waveform(1, 60, 256)
    assert int(samples.sum()) == 3782
