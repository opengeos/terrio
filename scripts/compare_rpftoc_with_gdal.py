#!/usr/bin/env python3
"""Cross-read CADRG RPF indexes from all public surfaces with GDAL 3.13.3."""

import argparse
import json
import os
import queue
import signal
import threading
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile

import numpy as np
from osgeo import gdal

gdal.UseExceptions()


def index_records(path):
    """Read the independently specified frame-index metadata from a big-endian TOC."""
    data = path.read_bytes()
    header = data.index(b"RPFHDR") + 11
    location = struct.unpack_from(">I", data, header + 44)[0]
    count = struct.unpack_from(">H", data, location + 6)[0]
    components = {}
    for i in range(count):
        ident, size, offset = struct.unpack_from(">HII", data, location + 14 + i * 10)
        components[ident] = (size, offset)
    count = struct.unpack_from(">I", data, components[150][1] + 5)[0]
    start = components[151][1]
    records = {}
    for i in range(count):
        record = data[start + i * 33 : start + (i + 1) * 33]
        name = record[10:22].decode("ascii")
        if name in records:
            raise AssertionError("fixture contains ambiguous duplicate frame basenames")
        records[name] = record[
            22:33
        ]  # GEOREF, frame classification, country and release marking.
    return records


def compare(actual, reference):
    """Compare every mosaic, sample, mask, palette and relevant raster property."""
    actual, reference = gdal.Open(str(actual)), gdal.Open(str(reference))
    a, b = actual.GetSubDatasets(), reference.GetSubDatasets()
    if len(a) != len(b) or not a:
        raise AssertionError("mosaic count differs or no mosaics are exposed")
    exact, maximum = True, 0.0
    for (actual_name, _), (reference_name, _) in zip(a, b, strict=True):
        x, y = gdal.Open(actual_name), gdal.Open(reference_name)
        if (x.RasterXSize, x.RasterYSize, x.RasterCount) != (
            y.RasterXSize,
            y.RasterYSize,
            y.RasterCount,
        ):
            raise AssertionError("mosaic dimensions or band count differ")
        if not x.GetSpatialRef().IsSame(y.GetSpatialRef()):
            raise AssertionError("CRS differs")
        xgt, ygt = np.array(x.GetGeoTransform()), np.array(y.GetGeoTransform())
        delta = float(np.max(np.abs(xgt - ygt)))
        if delta > 1e-7:
            raise AssertionError("geotransform differs beyond 1e-7 CRS units")
        maximum = max(maximum, delta)
        exact &= bool(np.array_equal(xgt, ygt))
        for band in range(1, x.RasterCount + 1):
            p, q = x.GetRasterBand(band), y.GetRasterBand(band)
            if (
                p.DataType,
                p.GetNoDataValue(),
                p.GetMaskFlags(),
                p.GetColorInterpretation(),
            ) != (
                q.DataType,
                q.GetNoDataValue(),
                q.GetMaskFlags(),
                q.GetColorInterpretation(),
            ):
                raise AssertionError(
                    "sample type, nodata, mask flags or color interpretation differ"
                )
            if not np.array_equal(p.ReadAsArray(), q.ReadAsArray()):
                raise AssertionError("mosaic samples differ")
            if not np.array_equal(
                p.GetMaskBand().ReadAsArray(), q.GetMaskBand().ReadAsArray()
            ):
                raise AssertionError("mosaic masks differ")
            pc, qc = p.GetColorTable(), q.GetColorTable()
            if (pc is None) != (qc is None):
                raise AssertionError("palette presence differs")
            if pc is not None and (
                pc.GetCount() != qc.GetCount()
                or any(
                    pc.GetColorEntry(i) != qc.GetColorEntry(i)
                    for i in range(pc.GetCount())
                )
            ):
                raise AssertionError("palette entries differ")
    return (
        "identical" if exact else "tolerance",
        f"{len(a)} mosaics; exact samples/masks/palettes/types/nodata/CRS; maximum affine difference {maximum}, limit 1e-7 CRS units",
    )


def mcp_report(binary, arguments, timeout=30):
    process = subprocess.Popen(
        [str(binary), "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=os.name == "posix",
    )
    reader = None
    try:
        messages = [
            dict(
                jsonrpc="2.0",
                id=0,
                method="initialize",
                params=dict(
                    protocolVersion="2025-06-18",
                    capabilities={},
                    clientInfo=dict(name="rpftoc-conformance", version="1"),
                ),
            ),
            dict(jsonrpc="2.0", id=1, method="tools/list"),
            dict(
                jsonrpc="2.0",
                id=2,
                method="tools/call",
                params=dict(name="rpftoc-create", arguments=dict(args=arguments)),
            ),
        ]
        for message in messages:
            process.stdin.write(json.dumps(message) + "\n")
        process.stdin.flush()
        completed = queue.Queue()

        def read_replies():
            try:
                replies = {}
                for _ in messages:
                    reply = json.loads(process.stdout.readline())
                    replies[reply["id"]] = reply
                completed.put((replies, None))
            except Exception as error:
                completed.put((None, error))

        reader = threading.Thread(target=read_replies, daemon=True)
        reader.start()
        try:
            replies, error = completed.get(timeout=timeout)
        except queue.Empty as error:
            raise AssertionError(f"MCP replies timed out after {timeout}s") from error
        if error is not None:
            raise error
        tool = next(
            tool
            for tool in replies[1]["result"]["tools"]
            if tool["name"] == "rpftoc-create"
        )
        if tool["annotations"]["readOnlyHint"]:
            raise AssertionError("RPF MCP tool must advertise a write operation")
        result = replies[2]["result"]
        if result.get("isError"):
            raise AssertionError(str(result))
        process.stdin.close()
        if process.wait(timeout=30):
            raise AssertionError("MCP server failed")
        return json.loads("".join(item["text"] for item in result["content"]))
    finally:
        # EOF asks Terrio to cancel and reap its running tool children.
        if not process.stdin.closed:
            process.stdin.close()
        try:
            process.wait(timeout=1)
        except subprocess.TimeoutExpired:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
            process.wait()
        if reader is not None:
            reader.join(timeout=1)


def run_case(kind, source, root, backend, options, args):
    import terrio

    work = root / f"{kind}-{backend}"
    frames = work / "frames"
    frames.mkdir(parents=True)
    sources = sorted(
        p
        for p in source.rglob("*")
        if p.is_file()
        and len(p.name) == 12
        and p.suffix.upper() in {".ON2", ".TP2", ".ON9", ".ONJ", ".WI2"}
    )
    if not sources:
        raise RuntimeError(
            f"no prepared CADRG frames in {source}; run scripts/make_rpftoc_fixtures.py"
        )
    uploads = []
    for path in sources:
        relative = path.relative_to(source)
        target = frames / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
        uploads.append(dict(filename="frames/" + relative.as_posix(), path=str(target)))
    reference = frames / "reference.A.TOC"
    # GDAL stores paths relative to its input directory even when output is elsewhere.
    # Place its reference inside frames; Terrio also verifies ancestor-directory output.
    reference_options = dict(options)
    if kind == "fractional":
        reference_options["scale"] = (
            3500000  # Correct GDAL's independently verified WI 3M truncation.
        )
    gdal.alg.driver.rpftoc.create(
        input=str(frames), output=str(reference), **reference_options
    )
    actual = work / "actual.A.TOC"
    if backend == "python-native":
        report = terrio.rpftoc_create(frames, actual, **options)
        if (
            report["frames"] != len(uploads)
            or report["metadata_bytes"] >= len(uploads) * 1024
        ):
            raise AssertionError("native inventory/read budget differs")
    elif backend == "python-bytes":
        actual.write_bytes(
            terrio.rpftoc_bytes(
                [(u["filename"], Path(u["path"]).read_bytes()) for u in uploads],
                **options,
            )
        )
    elif backend in ["cli", "mcp"]:
        command = [str(args.terrio), "rpftoc-create", str(frames), str(actual)]
        for key, value in options.items():
            command += ["--" + key.replace("_", "-"), str(value)]
        report = (
            json.loads(subprocess.check_output(command, text=True, timeout=600))
            if backend == "cli"
            else mcp_report(args.terrio, command[2:])
        )
        if (
            report["frames"] != len(uploads)
            or report["metadata_bytes"] >= len(uploads) * 1024
        ):
            raise AssertionError("CLI inventory/read budget differs")
    else:
        camel = lambda key: key.split("_")[0] + "".join(
            word.title() for word in key.split("_")[1:]
        )
        request = work / "request.json"
        request.write_text(
            json.dumps(
                dict(
                    frames=uploads,
                    output=str(actual),
                    options={camel(k): v for k, v in options.items()},
                )
            )
        )
        subprocess.run(
            [
                "node",
                str(Path(__file__).with_name("run_rpftoc.mjs")),
                str(args.js),
                str(request),
            ],
            check=True,
            capture_output=True,
            text=True,
            timeout=600,
        )
    header = actual.read_bytes()
    if (
        header[119:120] != b"R"
        or header[15:20] != b"ACME "
        or header[297:301] != b"Test"
    ):
        raise AssertionError("index classification or producer metadata differs")
    if index_records(actual) != index_records(reference):
        raise AssertionError("frame GEOREF, classification or country metadata differs")
    status, detail = compare(actual, reference)
    if kind == "fractional":
        detail += "; nominal WI 3.5M matches GDAL with explicit scale, correcting its default 3M truncation"
    return dict(name=f"{kind}/{backend}", status=status, detail=detail)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", type=Path)
    parser.add_argument("--js", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if args.terrio:
        args.terrio = args.terrio.resolve()
    if args.js:
        args.js = args.js.resolve()
    data = args.data.resolve()
    options = dict(
        producer_id="ACME", producer_name="Test", country_code="US", classification="R"
    )
    backends = (
        ["python-native", "python-bytes"]
        + (["cli", "mcp"] if args.terrio else [])
        + (["js"] if args.js else [])
    )
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-rpftoc-") as temp:
        root = Path(temp)
        for kind, source in [
            ("flat", data / "rpftoc_landsat"),
            ("polar", data / "rpftoc_polar"),
            ("fractional", data / "rpftoc_fractional3500000"),
            ("north-pole-center", data / "rpftoc_pole_centers/north-rgb"),
            ("south-pole-center", data / "rpftoc_pole_centers/south-rgb"),
        ]:
            for backend in backends:
                try:
                    results.append(run_case(kind, source, root, backend, options, args))
                except Exception as error:
                    results.append(
                        dict(
                            name=f"{kind}/{backend}",
                            status="differs",
                            detail=f"{type(error).__name__}: {error}",
                        )
                    )
    print(json.dumps(results, indent=None if args.json else 2))
    return int(any(row["status"] == "differs" for row in results))


if __name__ == "__main__":
    raise SystemExit(main())
