"""Raster creation, overview management and in-place update APIs."""

import os

import numpy as np
import pytest

import terrio


def test_create_manage_overviews_and_update(tmp_path):
    path = str(tmp_path / "target.tif")
    raster = terrio.create_raster(
        path, size=(64, 64), ot="Float32", burn=[7], crs="EPSG:3857", bbox=(0, 0, 64, 64)
    )
    np.testing.assert_array_equal(raster.read(1), np.full((64, 64), 7))
    assert terrio.add_overviews(path, [2, 4], resampling="average", compression="DEFLATE") == [
        (32, 32),
        (16, 16),
    ]
    assert terrio.refresh_overviews(path, bbox=[(0, 0, 32, 32)]) == [(32, 32), (16, 16)]
    src = terrio.Raster.from_array(
        np.full((16, 16), 3, dtype="float32"), (16, 1, 0, 48, 0, -1), crs="EPSG:3857"
    )
    assert terrio.update_raster(src, path) is not None
    values = terrio.open_raster(path).read(1)
    np.testing.assert_array_equal(values[16:32, 16:32], np.full((16, 16), 3))
    assert values[0, 0] == 7
    assert terrio.open_raster(path, lazy=True).overviews == [(32, 32), (16, 16)]
    terrio.delete_overviews(path)
    assert terrio.open_raster(path, lazy=True).overviews == []


def test_create_like_and_invalid_overwrite_preserve_output(tmp_path):
    path = str(tmp_path / "target.tif")
    template = terrio.create_raster(
        size=(8, 6), ot="UInt16", burn=[9], crs="EPSG:3857", bbox=(0, 0, 8, 6)
    )
    created = terrio.create_raster(path, like=template, burn=[2])
    np.testing.assert_array_equal(created.read(1), np.full((6, 8), 2))
    before = (tmp_path / "target.tif").read_bytes()
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.create_raster(path, overwrite=True)
    assert (tmp_path / "target.tif").read_bytes() == before
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.create_raster(size=(2, 2), copy_overviews=True)


def test_edit_and_move_preserve_pixels(tmp_path):
    path = str(tmp_path / "edited.tif")
    terrio.create_raster(path, size=(8, 6), burn=[7], crs="EPSG:3857", bbox=(0, 0, 8, 6))
    terrio.edit_raster(path, nodata=255, metadata={"SOURCE": "A & <B>"}, stats=True)
    raster = terrio.open_raster(path)
    np.testing.assert_array_equal(raster.read(1), np.full((6, 8), 7))
    assert raster.nodata == 255
    report = terrio.move_raster(path, "EPSG:3857", error_threshold=0.01)
    assert report["updated"] and report["max_error"] < 1e-10
    np.testing.assert_array_equal(terrio.open_raster(path).read(1), np.full((6, 8), 7))


def test_invalid_refresh_levels_are_errors(tmp_path):
    path = str(tmp_path / "levels.tif")
    terrio.create_raster(path, size=(8, 8), burn=[1])
    assert terrio.add_overviews(path, external=True) == []
    terrio.add_overviews(path, levels=[2])
    before = (tmp_path / "levels.tif").read_bytes()
    for level in (0, 1):
        with pytest.raises(terrio.InvalidArgumentError):
            terrio.refresh_overviews(path, levels=[level])
        assert (tmp_path / "levels.tif").read_bytes() == before


def test_external_overviews_follow_supplied_symlink_name(tmp_path):
    target = tmp_path / "real.tif"
    link = tmp_path / "link.tif"
    terrio.create_raster(str(target), size=(32, 32), burn=[2])
    link.symlink_to(target.name)
    terrio.add_overviews(str(link), levels=[2, 4], external=True)
    assert (tmp_path / "link.tif.ovr").is_file()
    assert not (tmp_path / "real.tif.ovr").exists()
    terrio.refresh_overviews(str(link), external=True)
    terrio.delete_overviews(str(link), external=True)
    assert link.is_symlink() and not (tmp_path / "link.tif.ovr").exists()


@pytest.mark.parametrize("external", [False, True])
def test_requested_overview_compression_preserves_unrequested_level(tmp_path, external):
    import struct

    path = str(tmp_path / "selective.tif")
    raster = terrio.Raster.from_array(
        np.arange(64, dtype="float32").reshape(8, 8), (0, 1, 0, 8, 0, -1)
    )
    raster.write(path)
    terrio.add_overviews(
        path, levels=[2, 4], resampling="average", external=external, compression="DEFLATE"
    )
    before = terrio.open_raster(path, lazy=True).read(1, overview=1).copy()
    terrio.add_overviews(
        path, levels=[2], resampling="nearest", external=external, compression="LZW"
    )
    np.testing.assert_array_equal(terrio.open_raster(path, lazy=True).read(1, overview=1), before)

    # Inspect public TIFF compression tags in the two output levels.
    data = (tmp_path / ("selective.tif.ovr" if external else "selective.tif")).read_bytes()
    assert data[:4] == b"II*\x00"
    offset = struct.unpack_from("<I", data, 4)[0]
    compression = {}
    while offset:
        count = struct.unpack_from("<H", data, offset)[0]
        tags = {}
        for index in range(count):
            tag, _, length, value = struct.unpack_from("<HHII", data, offset + 2 + index * 12)
            if length == 1:
                tags[tag] = value
        compression[(tags[256], tags[257])] = tags[259]
        offset = struct.unpack_from("<I", data, offset + 2 + count * 12)[0]
    assert compression[(4, 4)] == 5  # TIFF LZW
    assert compression[(2, 2)] in (8, 32946)  # retained Deflate


def test_noop_edit_and_updates_preserve_cog_bytes(tmp_path):
    path = tmp_path / "intact.tif"
    terrio.create_raster(
        str(path),
        size=(16, 16),
        burn=[7],
        crs="EPSG:3857",
        bbox=(0, 0, 16, 16),
        metadata={"SOURCE": "fixture"},
        format="COG",
    )
    original = path.read_bytes()
    assert b"KNOWN_INCOMPATIBLE_EDITION=NO" in original
    terrio.edit_raster(str(path), ignore_cog_layout_break=True)
    assert path.read_bytes() == original
    for origin, value in [(1000, 42), (0, float("nan"))]:
        source = terrio.Raster.from_array(
            np.full((2, 2), value, dtype="float32"),
            (origin, 1, 0, 16, 0, -1),
            crs="EPSG:3857",
        )
        assert terrio.update_raster(source, str(path), ignore_cog_layout_break=True) is None
        assert path.read_bytes() == original


def _float_raster(path, cols=256, rows=192):
    values = np.arange(cols * rows, dtype="float32").reshape(rows, cols)
    terrio.Raster.from_array(values, (0, 1, 0, rows, 0, -1), crs="EPSG:3857").write(str(path))


@pytest.mark.parametrize("external", [False, True])
def test_add_overviews_progress_and_cancellation(tmp_path, external):
    from terrio.errors import CancelledError, TerrioError

    path = tmp_path / "progress.tif"
    _float_raster(path)
    seen = []
    # `list.append` returns None, which goes on.
    sizes = terrio.add_overviews(
        str(path), [2], resampling="average", external=external, progress=seen.append
    )
    assert sizes == [(128, 96)]
    assert seen[0] == 0.0 and seen[-1] == 1.0 and seen == sorted(seen) and len(seen) > 2
    changed = tmp_path / ("progress.tif.ovr" if external else "progress.tif")
    before = changed.read_bytes()

    # A falsy return cancels partway; the file (and the sidecar built before) is kept.
    calls = []

    def until(fraction):
        calls.append(fraction)
        return fraction < 0.3

    with pytest.raises(CancelledError, match="cancelled") as info:
        terrio.add_overviews(
            str(path), [2, 4, 8], resampling="average", external=external, progress=until
        )
    assert isinstance(info.value, TerrioError)
    assert 0.3 <= calls[-1] < 1.0
    assert changed.read_bytes() == before

    # An exception raised by the callback cancels too, and is raised again.
    def fail(fraction):
        if fraction > 0:
            raise KeyError("stop")

    with pytest.raises(KeyError, match="stop"):
        terrio.add_overviews(str(path), [2, 4, 8], external=external, progress=fail)
    assert changed.read_bytes() == before
    assert terrio.open_raster(str(path), lazy=True).overviews == [(128, 96)]
    assert not [p.name for p in tmp_path.iterdir() if ".terrio-" in p.name]


@pytest.mark.skipif(os.name == "nt", reason="POSIX file permissions")
def test_external_overviews_of_read_only_file(tmp_path):
    path = tmp_path / "read-only.tif"
    _float_raster(path)
    path.chmod(0o444)
    before = path.read_bytes()
    try:
        # Internal overviews change the file: refused.
        with pytest.raises(terrio.TerrioIOError, match="read-only"):
            terrio.add_overviews(str(path), [2])
        assert terrio.add_overviews(str(path), [2, 4], resampling="average", external=True) == [
            (128, 96),
            (64, 48),
        ]
        assert terrio.refresh_overviews(str(path), external=True) == [(128, 96), (64, 48)]
        overview = terrio.open_raster(str(path), lazy=True).read(1, overview=0)
        np.testing.assert_array_equal(overview[0, :2], [128.5, 130.5])
        terrio.delete_overviews(str(path), external=True)
        assert not (tmp_path / "read-only.tif.ovr").exists()
        assert path.read_bytes() == before
    finally:
        path.chmod(0o644)
