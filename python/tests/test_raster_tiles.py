"""Folder tiles preserve pixels/grids through lazy and in-memory Python inputs."""

import numpy as np
import pytest

import terrio


@pytest.mark.parametrize("kind", ["loaded", "lazy", "path"])
def test_folder_tiles(tmp_path, kind):
    array = np.arange(15, dtype="uint8").reshape(1, 3, 5)
    raster = terrio.Raster.from_array(array, (0, 1, 0, 3, 0, -1), crs="EPSG:4326", nodata=0)
    if kind != "loaded":
        path = tmp_path / "input.tif"
        raster.write(str(path))
        raster = path if kind == "path" else terrio.open_raster(str(path), lazy=True)
    output = tmp_path / "tiles"
    options = dict(tiling_scheme="raster", grid={"tile_size": 2}, resampling="nearest")
    plan = terrio.raster_tiles(raster, output, dry_run=True, **options)
    assert len(plan["levels"]) == 1
    assert not output.exists()
    report = terrio.raster_tiles(raster, output, **options)
    assert report["written"] == 6
    first = terrio.open_raster(str(output / "1/0/0.png"))
    np.testing.assert_array_equal(first.read(1), array[0, :2, :2])
    np.testing.assert_array_equal(first.read(2), [[0, 255], [255, 255]])
    report = terrio.raster_tiles(raster, output, resume=True, **options)
    assert report["resumed"] == 6
    assert report["written"] == 0
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.raster_tiles(raster, output, **options)


def test_valid_zero_tiff_samples_are_preserved(tmp_path):
    raster = terrio.Raster.from_array(
        np.zeros((1, 3, 5), dtype="float32"),
        (0, 1, 0, 3, 0, -1),
        crs="EPSG:4326",
        nodata=-999999,
    )
    output = tmp_path / "tiles"
    report = terrio.raster_tiles(
        raster,
        output,
        tiling_scheme="raster",
        grid={"tile_size": 2},
        resampling="nearest",
        format="GTiff",
        skip_blank=True,
    )
    assert report["written"] == 6
    for path in output.rglob("*.tif"):
        tile = terrio.open_raster(str(path))
        assert tile.nodata == -999999
        np.testing.assert_array_equal(tile.read(1), np.zeros((2, 2), dtype="float32"))
