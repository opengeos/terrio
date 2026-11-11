"""Driver aliases preserve TIFF policy and output; GDAL 3.13.3 is the reference."""

import os

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio.io import DatasetWriter
from terrio.rio.transform import from_origin


@pytest.mark.parametrize(
    "canonical,aliases",
    [("GTiff", ["gtiff", "GTIFF", " gTiFf "]), ("COG", ["cog", "CoG", "\tCOG "])],
)
@pytest.mark.parametrize("masked", [False, True])
@pytest.mark.parametrize("limit", ["1", "1000000"])
def test_aliases_preserve_domains_internal_masks_and_spilling(
    tmp_path, monkeypatch, canonical, aliases, masked, limit
):
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    monkeypatch.setenv("GDAL_TIFF_INTERNAL_MASK", "YES")
    pixels = np.arange(3 * 48 * 64, dtype="uint8").reshape(3, 48, 64)
    mask = np.full((48, 64), 255, dtype="uint8")
    mask[:8] = 0
    streams = []
    original = DatasetWriter._write_streaming

    def record(self, *args, **kwargs):
        streams.append(self._out_driver)
        return original(self, *args, **kwargs)

    monkeypatch.setattr(DatasetWriter, "_write_streaming", record)
    outputs = []
    for index, driver in enumerate([canonical, *aliases]):
        path = tmp_path / f"alias-{index}.tif"
        previous = len(streams)
        with rio.open(
            path,
            "w",
            driver=driver,
            width=64,
            height=48,
            count=3,
            dtype="uint8",
            crs="EPSG:32611",
            transform=from_origin(500000, 4100000, 30, 30),
            compress="deflate",
        ) as dst:
            assert dst.driver == dst.profile["driver"] == canonical
            assert isinstance(dst._data, np.memmap) == (limit == "1")
            spill = list(dst._spill)
            assert bool(spill) == (limit == "1")
            dst.write(pixels)
            dst.update_tags(ns="SURVEY", flight="7")
            if masked:
                dst.write_mask(mask)
        assert all(not os.path.exists(p) for p in spill)
        assert len(streams) - previous == int(
            (canonical == "COG" and not masked) or (canonical == "GTiff" and limit == "1")
        )
        assert not path.with_suffix(".tif.msk").exists()
        with rio.open(path) as src:
            np.testing.assert_array_equal(src.read(), pixels)
            if masked:
                np.testing.assert_array_equal(src.dataset_mask(), mask)
            assert src.tags(ns="SURVEY") == ({"flight": "7"} if canonical == "GTiff" else {})
            assert src.tags(ns="IMAGE_STRUCTURE").get("LAYOUT") == (
                "COG" if canonical == "COG" else None
            )
        outputs.append(path.read_bytes())
    assert all(output == outputs[0] for output in outputs)
