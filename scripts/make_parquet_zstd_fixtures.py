"""Writes the GeoParquet files of `crates/terrio/tests/data/parquet_zstd`: the same table in Snappy
and in Zstandard with v1 and v2 data pages, and with each column compressed differently.

Usage:
    python scripts/make_parquet_zstd_fixtures.py
"""

import json
import struct
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

OUT = Path(__file__).parent.parent / "crates/terrio/tests/data/parquet_zstd"
ROWS = 500
BBOX = ("xmin", "ymin", "xmax", "ymax")


def table():
    """Builds the table: points with a covering column, nulls, dictionary text and a list."""
    xs = [float(i % 50) - 25 for i in range(ROWS)]
    ys = [float(i % 20) - 10 for i in range(ROWS)]
    columns = {
        "id": pa.array(range(ROWS), pa.int64()),
        "name": pa.array([None if i % 7 == 0 else f"category-{i % 5}" for i in range(ROWS)]),
        "value": pa.array([None if i % 11 == 0 else i * 0.5 for i in range(ROWS)], pa.float64()),
        "tags": pa.array([[f"t{i % 3}", f"u{i % 2}"] if i % 13 else None for i in range(ROWS)], pa.list_(pa.string())),
        "bbox": pa.array(
            [{"xmin": x, "ymin": y, "xmax": x, "ymax": y} for x, y in zip(xs, ys)],
            pa.struct([(k, pa.float64()) for k in BBOX]),
        ),
        "geometry": pa.array([struct.pack("<BIdd", 1, 1, x, y) for x, y in zip(xs, ys)], pa.binary()),
    }
    column = {"encoding": "WKB", "geometry_types": ["Point"], "covering": {"bbox": {k: ["bbox", k] for k in BBOX}}}
    geo = {"version": "1.1.0", "primary_column": "geometry", "columns": {"geometry": column}}
    return pa.table(columns).replace_schema_metadata({"geo": json.dumps(geo)})


def main():
    """Writes the files."""
    t = table()
    options = {"row_group_size": 120, "data_page_size": 512, "write_statistics": True}
    mixed = {
        "id": "zstd",
        "name": "snappy",
        "value": "gzip",
        "tags.list.element": "zstd",
        "bbox.xmin": "zstd",
        "bbox.ymin": "none",
        "bbox.xmax": "zstd",
        "bbox.ymax": "lz4",
        "geometry": "zstd",
    }
    pq.write_table(t, OUT / "snappy.parquet", compression="snappy", **options)
    pq.write_table(t, OUT / "zstd_v1.parquet", compression="zstd", **options)
    pq.write_table(t, OUT / "zstd_v2.parquet", compression="zstd", data_page_version="2.0", **options)
    pq.write_table(t, OUT / "zstd_mixed.parquet", compression=mixed, **options)


if __name__ == "__main__":
    main()
