"""Parquet metadata API: pathlib inputs, metadata preservation and mutation guards."""

import json

import pytest

import terrio

pq = pytest.importorskip("pyarrow.parquet")
pa = pytest.importorskip("pyarrow")


def test_partition_metadata_and_referenced_input_protection(tmp_path):
    first, second = tmp_path / "a.parquet", tmp_path / "b.parquet"
    table = pa.table({"value": [2, 7, 11]}).replace_schema_metadata({b"source": b"survey"})
    for path in [first, second]:
        pq.write_table(table, path)
    target = tmp_path / "_metadata"
    report = terrio.parquet_create_metadata([first, second], target)
    assert (report["files"], report["rows"], report["row_groups"]) == (2, 6, 2)
    metadata = pq.read_metadata(target)
    assert metadata.metadata[b"source"] == b"survey"
    assert metadata.row_group(1).column(0).file_path == "b.parquet"
    original = first.read_bytes()
    with pytest.raises(ValueError, match="exists"):
        terrio.parquet_create_metadata([first, second], target)
    with pytest.raises(ValueError, match="overlap"):
        terrio.parquet_create_metadata([target], first, overwrite=True)
    assert first.read_bytes() == original
    terrio.parquet_create_metadata([first], target, overwrite=True)
    assert pq.read_metadata(target).num_rows == 3
    with pytest.raises(ValueError, match="at least one"):
        terrio.parquet_create_metadata([], target, overwrite=True)


def test_vsimem_metadata_is_readable_and_geo_schema_updates():
    root = "/vsimem/parquet-metadata-python"
    geo = {
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {
            "geometry": {"encoding": "WKB", "geometry_types": ["Point"], "bbox": [1, 2, 1, 2]}
        },
    }
    table = pa.table(
        {"geometry": [bytes.fromhex("0101000000000000000000f03f0000000000000040")]}
    ).replace_schema_metadata({b"geo": json.dumps(geo).encode()})
    buffer = pa.BufferOutputStream()
    pq.write_table(table, buffer)
    terrio.vsi_write(root + "/a.parquet", buffer.getvalue().to_pybytes())
    try:
        report = terrio.parquet_create_metadata([root + "/a.parquet"], root + "/_metadata")
        encoded = terrio.vsi_read(root + "/_metadata")
        assert report["rows"] == 1
        assert json.loads(pq.read_schema(pa.BufferReader(encoded)).metadata[b"geo"]) == geo
    finally:
        terrio.vsi_delete(root, recursive=True)


def test_missing_geo_metadata_is_rejected_before_overwrite(tmp_path):
    geo = {
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {"geometry": {"encoding": "WKB", "geometry_types": []}},
    }
    table = pa.table({"geometry": [bytes.fromhex("0101000000000000000000f03f0000000000000040")]})
    plain, geographic = tmp_path / "plain.parquet", tmp_path / "geo.parquet"
    pq.write_table(table, plain)
    pq.write_table(table.replace_schema_metadata({b"geo": json.dumps(geo).encode()}), geographic)
    target = tmp_path / "_metadata"
    target.write_bytes(b"preserve existing output")
    with pytest.raises(ValueError, match="missing from some partitions"):
        terrio.parquet_create_metadata([plain, geographic], target, overwrite=True)
    assert target.read_bytes() == b"preserve existing output"
