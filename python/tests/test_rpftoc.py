"""RPF wrappers index metadata without requiring compressed pixel decoding."""

import math
import struct

import pytest

import terrio


def frame_metadata():
    """Synthetic metadata-only frame on the 1M zone-2 grid, without image pixels."""
    sx, sy = 360 / 201728, 90 / 66816
    west = -180 + 21 * sx * 1536
    south = math.floor(32 / (sy * 1536)) * (sy * 1536) + 3 * sy * 1536
    east, north = west + sx * 1536, south + sy * 1536
    coverage = [
        north,
        west,
        south,
        west,
        north,
        east,
        south,
        east,
        99.9 * 400384 / (4 * 66816),
        99.9 * 302592 / 201728,
        sy,
        sx,
    ]
    header = bytearray(b" " * 360)
    header[:9] = b"NITF02.00"
    header[119] = ord("U")
    header += b"0010000000000000000000000000000000"
    header += b"00062000RPFHDR00048"
    location = len(header) + 48 + 5
    header += (
        struct.pack(">BH", 0, 48)
        + b"000C9010.ON2"
        + b"\x00"
        + b"MIL-C-89038    19941006U    "
        + struct.pack(">I", location)
    )
    header += b"00000"
    header[354:360] = f"{location:06}".encode()
    header += struct.pack(">HIHHI", 34, 14, 2, 10, 116)
    header += struct.pack(">HII", 130, 96, location + 34)
    header += struct.pack(">HII", 136, 20, location + 130)
    header += struct.pack(">12d", *coverage)
    header += bytes([0, 1, 0, 36, 0, 1, 0, 1, 0, 6, 0, 6])
    header += struct.pack(">II", 256, 256)
    header[342:354] = f"{len(header):012}".encode()
    return bytes(header)


def test_named_frame_bytes_and_native_directory_agree(tmp_path):
    data = frame_metadata()
    frames = tmp_path / "frames"
    (frames / "zone").mkdir(parents=True)
    (frames / "zone/000C9010.ON2").write_bytes(data)
    expected = terrio.rpftoc_bytes([("frames/zone/000C9010.ON2", data)], country_code="US")
    output = tmp_path / "A.TOC"
    report = terrio.rpftoc_create(frames, output, country_code="US")
    assert report == dict(
        frames=1,
        skipped_files=0,
        groups=1,
        metadata_bytes=len(data) + 9,
        output_bytes=len(expected),
    )
    assert output.read_bytes() == expected
    assert expected[:9] == b"NITF02.00"
    assert int(expected[342:354]) == len(expected)
    assert b"./frames/zone/" in expected
    with pytest.raises(ValueError, match="exists"):
        terrio.rpftoc_create(frames, output)
    assert terrio.rpftoc_create(frames, output, country_code="US", overwrite=True) == report


def test_invalid_scale_and_paths_fail_without_overwriting(tmp_path):
    data = frame_metadata()
    (tmp_path / "000C9010.ON2").write_bytes(data)
    output = tmp_path / "A.TOC"
    output.write_bytes(b"original")
    with pytest.raises(ValueError, match="scale|aligned"):
        terrio.rpftoc_create(tmp_path, output, scale=500000, overwrite=True)
    assert output.read_bytes() == b"original"
    with pytest.raises(ValueError, match="relative"):
        terrio.rpftoc_bytes([("../000C9010.ON2", data)])
    with pytest.raises(ValueError, match="duplicate"):
        terrio.rpftoc_bytes([("000C9010.ON2", data)] * 2)
    with pytest.raises(ValueError, match="producer"):
        terrio.rpftoc_bytes([("000C9010.ON2", data)], producer_id="TOO LONG")


def test_index_cannot_downgrade_the_nitf_frame_classification():
    data = bytearray(frame_metadata())
    data[119] = ord("S")
    with pytest.raises(ValueError, match="highest frame"):
        terrio.rpftoc_bytes([("000C9010.ON2", bytes(data))])
    output = terrio.rpftoc_bytes([("000C9010.ON2", bytes(data))], classification="S")
    assert output[119:120] == b"S"


def test_frame_limit_is_checked_before_extracting_items():
    with pytest.raises(ValueError, match="100000"):
        terrio.rpftoc_bytes([object()] * 100001)


def test_truncated_payload_and_windows_device_paths_are_rejected():
    metadata = bytearray(frame_metadata())
    metadata += b"payload"
    metadata[342:354] = f"{len(metadata):012}".encode()
    assert terrio.rpftoc_bytes([("zone/000C9010.ON2", bytes(metadata))])
    with pytest.raises(ValueError, match="file length"):
        terrio.rpftoc_bytes([("zone/000C9010.ON2", bytes(metadata[:-1]))])
    for name in [
        "CON/000C9010.ON2",
        "aux.txt/000C9010.ON2",
        "COM1/000C9010.ON2",
        "COM¹/000C9010.ON2",
    ]:
        with pytest.raises(ValueError, match="relative paths"):
            terrio.rpftoc_bytes([(name, bytes(metadata))])


def test_custom_83_output_name_can_be_replaced(tmp_path):
    (tmp_path / "000C9010.ON2").write_bytes(frame_metadata())
    output = tmp_path / "INDEX001.TOC"
    output.write_bytes(b"placeholder")
    first = terrio.rpftoc_create(tmp_path, output, overwrite=True)
    second = terrio.rpftoc_create(tmp_path, output, overwrite=True)
    assert first == second
    assert first["frames"] == 1 and first["skipped_files"] == 0
    assert output.read_bytes().startswith(b"NITF02.00")
