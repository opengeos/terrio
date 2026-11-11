"""Reduced first-image masks, with a normalized GDAL 3.13.3 control."""

import struct

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.transform import from_origin


def mark_image(path, index, subfile):
    data = bytearray(path.read_bytes())
    assert data[:4] == b"II*\0"
    pointer = 4
    for _ in range(index):
        offset = struct.unpack_from("<I", data, pointer)[0]
        pointer = offset + 2 + 12 * struct.unpack_from("<H", data, offset)[0]
    offset = struct.unpack_from("<I", data, pointer)[0]
    count = struct.unpack_from("<H", data, offset)[0]
    entries = [data[offset + 2 + 12 * i : offset + 14 + 12 * i] for i in range(count)]
    entries = [e for e in entries if struct.unpack_from("<H", e)[0] != 254]
    entries.append(struct.pack("<HHII", 254, 4, 1, subfile))
    entries.sort(key=lambda e: struct.unpack_from("<H", e)[0])
    next_ifd = data[offset + 2 + 12 * count : offset + 6 + 12 * count]
    data.extend(b"\0" * (len(data) % 2))
    struct.pack_into("<I", data, pointer, len(data))
    data.extend(struct.pack("<H", len(entries)) + b"".join(entries) + next_ifd)
    path.write_bytes(data)


@pytest.mark.parametrize("reduced,mask_reduced", [(False, False), (True, True), (True, False)])
def test_reduced_first_masks_match_gdal_before_and_after_external_refresh(
    tmp_path, data_dir, gdal, reduced, mask_reduced
):
    source = data_dir / "landsat7.tif"
    if not source.is_file():
        pytest.skip("landsat7.tif is required")
    reference = gdal.Open(str(source))
    pixels = reference.GetRasterBand(1).ReadAsArray(0, 0, 256, 192)
    transform = reference.GetGeoTransform()
    base = tmp_path / "image.tif"
    level = tmp_path / "image.tif.ovr"
    mask = np.full((96, 128), 255, dtype="uint8")
    mask[50:90, 60:125] = 0
    for path, values, validity, scale in (
        (base, pixels, np.full((192, 256), 255, dtype="uint8"), 1),
        (level, pixels[::2, ::2], mask, 2),
    ):
        with rio.open(
            path,
            "w",
            driver="GTiff",
            width=values.shape[1],
            height=values.shape[0],
            count=1,
            dtype=values.dtype,
            crs=reference.GetProjection(),
            transform=from_origin(
                transform[0], transform[3], transform[1] * scale, -transform[5] * scale
            ),
            tiled=True,
            blockxsize=64,
            blockysize=64,
        ) as dst:
            dst.write(values, 1)
            dst.write_mask(validity)
    terrio.add_overviews(str(level), [2])
    mark_image(level, 0, int(reduced))
    mark_image(level, 1, 4 | int(mask_reduced))
    for refresh in (False, True):
        if refresh:
            x, dx, _, y, _, dy = transform
            terrio.refresh_overviews(
                str(base),
                external=True,
                resampling="average",
                bbox=[[x, y + dy * 96, x + dx * 128, y]],
            )
        for path, overview in ((level, None), (base, 0), (base, 1)):
            # GDAL 3.13.3 also ignores a first-image mask marked 5. Compare decoding
            # with an equivalent ordinary-first-image control rather than claiming parity.
            control = tmp_path / "control.tif"
            control.write_bytes(base.read_bytes())
            control_level = tmp_path / "control.tif.ovr"
            control_level.write_bytes(level.read_bytes())
            mark_image(control_level, 0, 0)
            mark_image(control_level, 1, 4)
            expected_ds = gdal.Open(str(control_level if path == level else control))
            band = expected_ds.GetRasterBand(1)
            if overview is not None:
                band = band.GetOverview(overview)
            expected = band.GetMaskBand().ReadAsArray()
            with rio.open(path, overview_level=overview) as src:
                np.testing.assert_array_equal(src.dataset_mask(), expected)
                np.testing.assert_array_equal(src.read(1), band.ReadAsArray())
            if not refresh and overview != 1:
                assert np.count_nonzero(expected == 0) == 2600
