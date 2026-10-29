#!/usr/bin/env python3
"""Compare metadata-only aggregation on Natural Earth partitions with GDAL 3.13."""

import argparse
import json
import os
import shutil
import sys
import threading
import time
import traceback
from http.server import ThreadingHTTPServer
import subprocess
import tempfile
from pathlib import Path

import pyarrow as pa
import pyarrow.dataset as ds
import pyarrow.parquet as pq
from osgeo import gdal
from compare_vsi_with_gdal import S3, make_handler

gdal.UseExceptions()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []

    def call(inputs, output, overwrite=False, success=True, env=None):
        command = [
            args.terrio,
            "parquet-create-metadata",
            *map(str, inputs),
            "--output",
            str(output),
            "--json",
        ]
        if overwrite:
            command.append("--overwrite")
        process = subprocess.run(command, capture_output=True, text=True, env=env)
        if (process.returncode == 0) != success:
            raise RuntimeError(process.stderr)
        return json.loads(process.stdout) if success else process.stderr

    def snapshot(directory):
        source = gdal.OpenEx(
            str(directory) if Path(directory).is_file() else "PARQUET:" + str(directory),
            gdal.OF_VECTOR,
        )
        layer = source.GetLayer(0)
        crs = layer.GetSpatialRef()
        definition = layer.GetLayerDefn()
        return dict(
            crs=crs.ExportToWkt() if crs else None,
            geometry_type=layer.GetGeomType(),
            fields=[
                (definition.GetFieldDefn(i).GetName(), definition.GetFieldDefn(i).GetType())
                for i in range(definition.GetFieldCount())
            ],
            features=[
                (item["properties"], item["geometry"])
                for feature in layer
                for item in [json.loads(feature.ExportToJson())]
            ],
        )

    with tempfile.TemporaryDirectory(prefix="terrio-parquet-metadata-") as temporary:
        root = Path(temporary)
        actual, reference = root / "actual", root / "reference"
        actual.mkdir()
        reference.mkdir()
        table = pq.read_table(args.data / "countries.parquet")
        source = gdal.OpenEx(str(args.data / "countries.parquet"), gdal.OF_VECTOR)
        envelopes = []
        for feature in source.GetLayer(0):
            geometry = feature.GetGeometryRef()
            if geometry is None:
                raise RuntimeError("Natural Earth reference fixture unexpectedly contains a null geometry")
            envelopes.append(geometry.GetEnvelope())
        inputs = []
        expected = []
        for index, (start, end) in enumerate([(0, 80), (80, table.num_rows)]):
            part = table.slice(start, end - start)
            metadata = dict(part.schema.metadata)
            geo = json.loads(metadata[b"geo"])
            bounds = envelopes[start:end]
            geo["columns"]["geometry"]["bbox"] = [
                min(b[0] for b in bounds),
                min(b[2] for b in bounds),
                max(b[1] for b in bounds),
                max(b[3] for b in bounds),
            ]
            metadata[b"geo"] = json.dumps(geo).encode()
            metadata[b"partition-note"] = str(index).encode()
            part = part.replace_schema_metadata(metadata)
            path = actual / f"part-{index}.parquet"
            pq.write_table(part, path, row_group_size=30, write_page_index=True)
            shutil.copyfile(path, reference / path.name)
            inputs.append(path)
            expected.append(part)
        try:
            report = call(inputs, actual / "_metadata")
            subprocess.run(
                [
                    "gdal",
                    "driver",
                    "parquet",
                    "create-metadata-file",
                    "--input",
                    str(reference / inputs[0].name),
                    "--input",
                    str(reference / inputs[1].name),
                    "--output",
                    str(reference / "_metadata"),
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            metadata = pq.read_metadata(actual / "_metadata")
            baseline = pq.read_metadata(reference / "_metadata")
            assert metadata.num_rows == baseline.num_rows == table.num_rows == report["rows"]
            assert metadata.num_row_groups == baseline.num_row_groups == report["row_groups"]
            for index in range(metadata.num_row_groups):
                ours, theirs = metadata.row_group(index), baseline.row_group(index)
                assert ours.num_rows == theirs.num_rows
                for column in range(ours.num_columns):
                    assert ours.column(column).to_dict() == theirs.column(column).to_dict()
                    assert (
                        ours.column(column).has_column_index
                        == theirs.column(column).has_column_index
                    )
                    assert (
                        ours.column(column).has_offset_index
                        == theirs.column(column).has_offset_index
                    )
            combined = pa.concat_tables(expected)
            assert (
                ds.parquet_dataset(str(actual / "_metadata"))
                .to_table()
                .equals(combined, check_metadata=False)
            )
            merged = json.loads(metadata.metadata[b"geo"])
            arrow_geo = json.loads(pq.read_schema(actual / "_metadata").metadata[b"geo"])
            assert merged == arrow_geo
            assert merged["columns"]["geometry"]["bbox"] == [
                min(b[0] for b in envelopes),
                min(b[2] for b in envelopes),
                max(b[1] for b in envelopes),
                max(b[3] for b in envelopes),
            ]
            assert "partition-note" in report["omitted_metadata_keys"]
            # GDAL 3.13.3 needs the directory dataset for external column paths;
            # opening _metadata directly raises Invalid column metadata. PyArrow
            # above independently opens _metadata and verifies every partition.
            ours, theirs = snapshot(actual), snapshot(reference)
            original = snapshot(args.data / "countries.parquet")
            assert ours == original
            # The reference drops geo metadata: covering bbox fields become attributes,
            # and Polygon WKB is no longer promoted to the declared MultiPolygon type.
            extra = {
                "geometry_bbox.xmin",
                "geometry_bbox.ymin",
                "geometry_bbox.xmax",
                "geometry_bbox.ymax",
            }
            normalized = []
            for properties, geometry in theirs["features"]:
                properties = {key: value for key, value in properties.items() if key not in extra}
                if geometry and geometry["type"] == "Polygon":
                    geometry = dict(
                        geometry, type="MultiPolygon", coordinates=[geometry["coordinates"]]
                    )
                normalized.append((properties, geometry))
            assert ours["features"] == normalized
            assert ours["fields"] == [field for field in theirs["fields"] if field[0] not in extra]
            assert ours["crs"] and ours["geometry_type"] == source.GetLayer(0).GetGeomType()
            correction = (
                ours["crs"] != theirs["crs"]
                or ours["geometry_type"] != theirs["geometry_type"]
                or normalized != theirs["features"]
                or ours["fields"] != theirs["fields"]
            )
            results.append(
                dict(
                    name="Natural Earth multi-row-group metadata",
                    status="tolerance" if correction else "identical",
                    detail=f"{report['rows']} rows/{report['row_groups']} row groups; exact column offsets/statistics and all Arrow values; GDAL snapshot exact against original; reference covering attributes removed and Polygon declarations promoted for exact value comparison; merged bounds/Arrow schema verified; reference CRS/type/covering loss corrected"
                    if correction
                    else "exact values, row groups, columns and geospatial metadata",
                )
            )
            saved = (actual / "_metadata").read_bytes()
            call(inputs, actual / "_metadata", success=False)
            call(inputs, inputs[0], overwrite=True, success=False)
            call([actual / "_metadata"], inputs[0], overwrite=True, success=False)
            mismatch = root / "mismatch.parquet"
            pq.write_table(pa.table({"different": [1]}), mismatch)
            call([inputs[0], mismatch], actual / "_metadata", overwrite=True, success=False)
            assert (actual / "_metadata").read_bytes() == saved
            results.append(
                dict(
                    name="Overwrite, referenced input and schema guards",
                    status="identical",
                    detail="existing output and both direct/referenced input partitions preserved; mismatched schema rejected before output mutation",
                )
            )
        except Exception as error:
            traceback.print_exc()
            results.append(
                dict(name="Parquet metadata aggregation", status="differs", detail=str(error))
            )
        try:
            spaced = root / "spaced"
            spaced.mkdir()
            unusual = [spaced / "part 0.parquet", spaced / "part%201.parquet"]
            for original, target in zip(inputs, unusual):
                shutil.copyfile(original, target)
            call(unusual, spaced / "_metadata")
            metadata = pq.read_metadata(spaced / "_metadata")
            assert {
                metadata.row_group(i).column(0).file_path for i in range(metadata.num_row_groups)
            } == {p.name for p in unusual}
            loaded = ds.parquet_dataset(str(spaced / "_metadata")).to_table()
            assert loaded.combine_chunks().equals(
                pa.concat_tables(expected).combine_chunks(), check_metadata=False
            )
            # Explicit inputs are an ordered multiset, including repeated partitions.
            repeated = spaced / "repeated_metadata"
            repeated_report = call([unusual[0], unusual[0]], repeated)
            repeated_values = ds.parquet_dataset(str(repeated)).to_table()
            assert repeated_report["rows"] == 2 * expected[0].num_rows
            assert repeated_values.combine_chunks().equals(
                pa.concat_tables([expected[0], expected[0]]).combine_chunks(), check_metadata=False
            )
            results.append(
                dict(
                    name="logical partition names with spaces and percent characters",
                    status="identical",
                    detail="PyArrow reads the metadata file and resolves every column from the actual space/percent filenames; all values exact; repeated explicit inputs preserve duplicate rows",
                )
            )
        except Exception as error:
            traceback.print_exc()
            results.append(
                dict(
                    name="logical partition names with spaces and percent characters",
                    status="differs",
                    detail=str(error),
                )
            )
        try:
            large = root / "large.parquet"
            repeated = pa.concat_tables([table] * 40)
            pq.write_table(
                repeated, large, compression="NONE", use_dictionary=False, row_group_size=2000
            )
            payload = large.read_bytes()
            store = S3()
            store.objects["metadata/large.parquet"] = (payload, time.time())
            base = make_handler(store)
            metrics = dict(bytes=0, requests=0)

            class Handler(base):
                def reply(self, status, body=b"", headers=None):
                    if (
                        self.command == "GET"
                        and status in (200, 206)
                        and self.path.split("?")[0] == "/metadata/large.parquet"
                    ):
                        metrics["bytes"] += len(body)
                        metrics["requests"] += 1
                    return super().reply(status, body, headers)

            class Server(ThreadingHTTPServer):
                def handle_error(self, request, client_address):
                    if not isinstance(sys.exc_info()[1], (ConnectionResetError, BrokenPipeError)):
                        super().handle_error(request, client_address)

            server = Server(("127.0.0.1", 0), Handler)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            env = dict(
                os.environ,
                AWS_S3_ENDPOINT=f"http://127.0.0.1:{server.server_address[1]}",
                AWS_HTTPS="NO",
                AWS_VIRTUAL_HOSTING="FALSE",
                AWS_ACCESS_KEY_ID="AKID",
                AWS_SECRET_ACCESS_KEY="secret",
                AWS_REGION="us-east-1",
            )
            try:
                report = call(
                    ["/vsis3/metadata/large.parquet"], "/vsis3/metadata/_metadata", env=env
                )
                assert report["rows"] == repeated.num_rows
                assert 0 < metrics["bytes"] < len(payload) // 4, metrics
                encoded = store.objects["metadata/_metadata"][0]
                metadata = pq.read_metadata(pa.BufferReader(encoded))
                assert metadata.num_rows == repeated.num_rows
                assert metadata.row_group(0).column(0).file_path == "large.parquet"
                assert store.objects["metadata/large.parquet"][0] == payload
                (root / "_metadata").write_bytes(encoded)
                assert (
                    ds.parquet_dataset(str(root / "_metadata"))
                    .to_table()
                    .equals(repeated, check_metadata=False)
                )
                results.append(
                    dict(
                        name="S3 range-read metadata aggregation",
                        status="identical",
                        detail=f"real Natural Earth derivative {len(payload)} bytes/{report['rows']} rows; {metrics['bytes']} downloaded bytes in {metrics['requests']} GETs; metadata output uploaded and every Arrow value verified; input unchanged",
                    )
                )
            finally:
                server.shutdown()
                worker.join()
                server.server_close()
        except Exception as error:
            traceback.print_exc()
            results.append(
                dict(name="S3 range-read metadata aggregation", status="differs", detail=str(error))
            )
    if args.json:
        print(json.dumps(results))
    else:
        for result in results:
            print(result["status"], result["name"], result["detail"])
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
