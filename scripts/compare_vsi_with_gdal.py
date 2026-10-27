#!/usr/bin/env python3
"""``terrio vsi`` against ``gdal vsi`` (GDAL 3.12 or later; the reference is 3.13.3) on real files.

Listings (``-lR -f json``) of the test data directory, of real zip archives, of a Zarr store
and of tar archives made from it; trees copied, extracted and synchronized by both tools;
and the same uploads, listings, server-side copies and bounded deletions against a local
S3-compatible server, with the requests each tool sends. With ``--remote``, listings of public
Amazon S3, Google Cloud Storage, HTTP index pages and a remote zip archive, with requests and
bytes.

Compared: entry names, types and file sizes exactly; local modification dates to the second;
zip member dates as stored. Entry order is not compared (GDAL prints local directories in the
order the file system returns them, terrio sorts them).

Usage:
    python scripts/compare_vsi_with_gdal.py ../terrio-data [--terrio target/release/terrio]
        [--remote] [--json]
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import urllib.parse
import zipfile
from email.utils import formatdate
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

RESULTS: list[dict] = []


def record(name: str, ok: bool, detail: str, tolerance: bool = False) -> None:
    """Records one compared case.

    Args:
        name: The case's name.
        ok: Whether terrio agrees with GDAL.
        detail: What was compared, or the differences.
        tolerance: Whether agreement is within a tolerance rather than exact.
    """
    status = ("within tolerance" if tolerance else "identical") if ok else "differs"
    RESULTS.append({"name": name, "status": status, "detail": detail})


def run(cmd: list[str], env: dict | None = None) -> subprocess.CompletedProcess:
    """Runs a command, capturing its output.

    Args:
        cmd: The command line.
        env: Extra environment variables.

    Returns:
        subprocess.CompletedProcess: The finished process.
    """
    full = dict(os.environ)
    full.update(env or {})
    return subprocess.run(cmd, capture_output=True, text=True, env=full, timeout=600)


def listing(tool: list[str], path: str, env: dict | None = None, extra: list[str] | None = None) -> list[dict]:
    """Lists a location recursively in long JSON form.

    Args:
        tool: The program and subcommand prefix (``["gdal", "vsi"]`` or ``[terrio, "vsi"]``).
        path: The location.
        env: Extra environment variables.
        extra: Extra options.

    Returns:
        list[dict]: The entries.
    """
    p = run(tool + ["list", "-l", "-R", "-f", "json", *(extra or []), path], env)
    if p.returncode:
        raise RuntimeError(f"{' '.join(tool)} list {path}: {p.stderr.strip()[-500:]}")
    return json.loads(p.stdout)


def keyed(entries: list[dict], dates: bool) -> dict:
    """Indexes entries by name: type, file size and (optionally) date to the minute or second.

    Args:
        entries: Listing entries.
        dates: Whether to keep the modification dates.

    Returns:
        dict: name -> (type, size of files, date).
    """
    out = {}
    for e in entries:
        is_dir = e["type"] == "directory"
        out[e["name"].rstrip("/")] = (e["type"], None if is_dir else e.get("size"), e.get("last_modification_date") if dates and not is_dir else None)
    return out


def compare_listing(
    name: str, terrio: str, path: str, dates: bool = True, env: dict | None = None, extra: list[str] | None = None, object_store: bool = False
) -> None:
    """Compares the recursive long listings of terrio and GDAL.

    Args:
        name: The case's name.
        terrio: The terrio executable.
        path: The location.
        dates: Whether to compare file dates.
        env: Extra environment variables.
        extra: Extra options for both tools.
        object_store: GDAL (3.12.2 to 3.13.3) lists only the objects of an object store recursively, not the
            directories their keys imply (which terrio lists, as for local directories): compare
            the files, and check that terrio's directories are those the files imply.
    """
    try:
        ours = keyed(listing([terrio, "vsi"], path, env, extra), dates)
        theirs = keyed(listing(["gdal", "vsi"], path, env, extra), dates)
    except Exception as e:  # noqa: BLE001 - reported as a difference
        record(name, False, str(e))
        return
    note = ""
    if object_store:
        dirs = {k for k, v in ours.items() if v[0] == "directory"}
        files = {k: v for k, v in ours.items() if v[0] == "file"}
        implied = {"/".join(k.split("/")[:i]) for k in list(files) + list(dirs) for i in range(1, k.count("/") + 1)}
        theirs_dirs = {k for k, v in theirs.items() if v[0] == "directory"}
        if not dirs <= implied | theirs_dirs:
            record(name, False, f"directories not implied by the keys: {sorted(dirs - implied)[:6]}")
            return
        ours = {k: v for k, v in ours.items() if k not in dirs or k in theirs_dirs}
        note = f"; terrio also lists the {len(dirs - theirs_dirs)} directories the keys imply, which GDAL omits"
    diff = sorted(set(ours.items()) ^ set(theirs.items()))
    files = sum(1 for v in ours.values() if v[0] == "file")
    if diff:
        record(name, False, f"{len(diff)} differing entries of {len(theirs)}: {diff[:6]}")
    else:
        what = "names, types, file sizes" + (" and dates" if dates else "")
        record(name, True, f"{len(ours)} entries ({files} files): {what} identical{note}")


def tree(root: Path) -> dict:
    """The files below a directory with their SHA-256.

    Args:
        root: The directory.

    Returns:
        dict: relative path -> digest (directories map to ``"dir"``).
    """
    out = {}
    for p in sorted(root.rglob("*")):
        rel = p.relative_to(root).as_posix()
        out[rel] = "dir" if p.is_dir() else hashlib.sha256(p.read_bytes()).hexdigest()
    return out


def compare_trees(name: str, ours: Path, theirs: Path) -> None:
    """Compares two copied trees, file by file.

    Args:
        name: The case's name.
        ours: terrio's copy.
        theirs: GDAL's copy.
    """
    a, b = tree(ours), tree(theirs)
    if not a and not b:
        record(name, False, "both output trees are empty or missing")
        return
    diff = sorted(set(a.items()) ^ set(b.items()))
    if diff:
        record(name, False, f"{len(diff)} differences: {diff[:6]}")
    else:
        record(name, True, f"{sum(v != 'dir' for v in a.values())} files byte-identical, {sum(v == 'dir' for v in a.values())} directories")


# --- A local S3-compatible server ---------------------------------------------------------------

class S3:
    """An in-memory S3 store with a request log."""

    def __init__(self):
        self.objects: dict[str, tuple[bytes, float]] = {}
        self.log: list[str] = []
        self.lock = threading.Lock()


def make_handler(store: S3):
    """A request handler serving ``store`` as S3's REST API (path-style).

    Args:
        store: The store.

    Returns:
        type: The handler class.
    """

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args):  # noqa: D401 - silence
            pass

        def reply(self, status: int, body: bytes = b"", headers: dict | None = None):
            self.send_response(status)
            for k, v in (headers or {}).items():
                self.send_header(k, v)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)

        def parts(self):
            url = urllib.parse.urlsplit(self.path)
            query = dict(urllib.parse.parse_qsl(url.query, keep_blank_values=True))
            path = urllib.parse.unquote(url.path.lstrip("/"))
            bucket, _, key = path.partition("/")
            with store.lock:
                store.log.append(f"{self.command} {url.path}{'?' + '&'.join(sorted(query)) if query else ''}")
            return bucket, key, query

        def body(self) -> bytes:
            n = int(self.headers.get("Content-Length") or 0)
            return self.rfile.read(n) if n else b""

        def do_GET(self):
            bucket, key, q = self.parts()
            if not key and ("list-type" in q or "prefix" in q or "delimiter" in q or not q):
                return self.list(bucket, q)
            obj = store.objects.get(f"{bucket}/{key}")
            if obj is None:
                return self.reply(404, b"<Error><Code>NoSuchKey</Code></Error>")
            data, at = obj
            h = {"Last-Modified": formatdate(at, usegmt=True), "ETag": f'"{hashlib.md5(data).hexdigest()}"'}
            rng = self.headers.get("Range")
            if rng and data:
                a, b = rng.removeprefix("bytes=").split("-")
                a, b = int(a), min(int(b) if b else len(data) - 1, len(data) - 1)
                h["Content-Range"] = f"bytes {a}-{b}/{len(data)}"
                return self.reply(206, data[a:b + 1], h)
            if rng:
                return self.reply(416, b"", {**h, "Content-Range": "bytes */0"})
            return self.reply(200, data, h)

        def do_HEAD(self):
            bucket, key, _ = self.parts()
            obj = store.objects.get(f"{bucket}/{key}")
            if obj is None:
                return self.reply(404)
            data, at = obj
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Last-Modified", formatdate(at, usegmt=True))
            self.send_header("ETag", f'"{hashlib.md5(data).hexdigest()}"')
            self.end_headers()

        def do_PUT(self):
            bucket, key, _ = self.parts()
            data = self.body()
            src = self.headers.get("x-amz-copy-source")
            if src:
                found = store.objects.get(urllib.parse.unquote(src).lstrip("/"))
                if found is None:
                    return self.reply(404)
                data = found[0]
            import time

            with store.lock:
                store.objects[f"{bucket}/{key}"] = (data, time.time())
            self.reply(200, b"<CopyObjectResult/>" if src else b"", {"ETag": f'"{hashlib.md5(data).hexdigest()}"'})

        def do_DELETE(self):
            bucket, key, _ = self.parts()
            with store.lock:
                store.objects.pop(f"{bucket}/{key}", None)
            self.reply(204)

        def do_POST(self):
            bucket, _, q = self.parts()
            body = self.body().decode()
            if "delete" in q:
                keys = [urllib.parse.unquote(k) for k in re.findall(r"<Key>(.*?)</Key>", body)]
                with store.lock:
                    for k in keys:
                        store.objects.pop(f"{bucket}/{k}", None)
                deleted = "".join(f"<Deleted><Key>{k}</Key></Deleted>" for k in keys)
                return self.reply(200, f"<DeleteResult>{deleted}</DeleteResult>".encode())
            self.reply(501)

        def list(self, bucket: str, q: dict):
            prefix, delim = q.get("prefix", ""), q.get("delimiter")
            v2 = q.get("list-type") == "2"
            start = q.get("continuation-token") if v2 else q.get("marker")
            items = []
            for full in sorted(store.objects):
                b, _, key = full.partition("/")
                if b != bucket or not key.startswith(prefix):
                    continue
                rest = key[len(prefix):]
                if delim and delim in rest:
                    p = prefix + rest[: rest.index(delim) + 1]
                    if ("p", p) not in items:
                        items.append(("p", p))
                else:
                    items.append(("k", key))
            if start:
                items = [i for i in items if i[1] > start]
            page, more = items[:1000], len(items) > 1000
            xml = ["<ListBucketResult>", f"<Prefix>{prefix}</Prefix>", f"<IsTruncated>{'true' if more else 'false'}</IsTruncated>"]
            for kind, k in page:
                if kind == "p":
                    xml.append(f"<CommonPrefixes><Prefix>{k}</Prefix></CommonPrefixes>")
                else:
                    data, at = store.objects[f"{bucket}/{k}"]
                    stamp = datetime.datetime.fromtimestamp(at, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000Z")
                    xml.append(
                        f"<Contents><Key>{k}</Key><LastModified>{stamp}</LastModified>"
                        f"<ETag>&quot;{hashlib.md5(data).hexdigest()}&quot;</ETag><Size>{len(data)}</Size></Contents>"
                    )
            if more:
                xml.append(f"<NextContinuationToken>{page[-1][1]}</NextContinuationToken>" if v2 else f"<NextMarker>{page[-1][1]}</NextMarker>")
            xml.append("</ListBucketResult>")
            self.reply(200, "".join(xml).encode(), {"Content-Type": "application/xml"})

    return Handler


def s3_cases(terrio: str, source: Path, work: Path) -> None:
    """Uploads, lists, copies and deletes the same tree on a local S3 server with both tools.

    Args:
        terrio: The terrio executable.
        source: A real directory to upload.
        work: A scratch directory.
    """
    store = S3()
    server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(store))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    host = f"127.0.0.1:{server.server_address[1]}"
    env = {
        "AWS_S3_ENDPOINT": f"http://{host}", "AWS_HTTPS": "NO", "AWS_VIRTUAL_HOSTING": "FALSE",
        "AWS_ACCESS_KEY_ID": "AKID", "AWS_SECRET_ACCESS_KEY": "secret", "AWS_REGION": "us-east-1",
        "CPL_VSIL_CURL_NON_CACHED": "/vsis3/", "GDAL_DISABLE_READDIR_ON_OPEN": "NO",
    }
    gdal_env = {**env, "AWS_S3_ENDPOINT": host}
    tools = {"gdal": (["gdal", "vsi"], gdal_env), "terrio": ([terrio, "vsi"], env)}

    def keys(bucket: str, prefix: str = "") -> dict:
        return {k.split("/", 1)[1]: hashlib.sha256(v[0]).hexdigest() for k, v in store.objects.items() if k.startswith(f"{bucket}/{prefix}")}

    # Serve an actual source file in an archive behind a query-bearing URL.
    source_files = sorted(p for p in source.rglob("*") if p.is_file())[:2]
    payload = io.BytesIO()
    with zipfile.ZipFile(payload, "w", zipfile.ZIP_DEFLATED) as archive:
        for source_file in source_files:
            archive.write(source_file, source_file.name)
    store.objects["archives/real.zip"] = (payload.getvalue(), 0)
    archive_url = f"/vsizip/{{/vsicurl/http://{host}/archives/real.zip?sig=test%2Fsignature}}"
    compare_listing("query-bearing remote ZIP archive", terrio, archive_url, dates=False)
    copied_members = []
    for name, (tool, e) in tools.items():
        destination = work / f"query_zip_{name}"
        result = run(tool + ["copy", "-q", f"{archive_url}/{source_files[0].name}", str(destination)], e)
        copied_members.append(result.returncode == 0 and destination.is_file()
                              and destination.read_bytes() == source_files[0].read_bytes())
    record("query-bearing remote ZIP member bytes", all(copied_members),
           "both tools extract bytes identical to the real source file")

    counts = {}
    for name, (tool, e) in tools.items():
        store.objects[f"{name}/dirkeep"] = (b"k", 0)
        store.objects[f"{name}/data2/keep.txt"] = (b"k", 0)
        store.log.clear()
        p = run(tool + ["copy", "-r", "-q", str(source), f"/vsis3/{name}/data"], e)
        counts[f"{name} upload"] = len(store.log)
        if p.returncode:
            record(f"S3 upload ({name})", False, p.stderr.strip()[-300:])
            return
    a, b = keys("terrio", "data/"), keys("gdal", "data/")
    record("S3 upload of a real tree (keys and bytes)", a == b and len(a) > 0,
           f"{len(a)} objects identical; requests: terrio {counts['terrio upload']}, GDAL {counts['gdal upload']}" if a == b else f"terrio {sorted(a)[:5]} vs GDAL {sorted(b)[:5]}")
    for name, (tool, e) in tools.items():
        store.log.clear()
        listing(tool, "/vsis3/terrio/data", e)
        counts[f"{name} list"] = len(store.log)
    try:
        ours = keyed(listing([terrio, "vsi"], "/vsis3/terrio/data", env), True)
        theirs = keyed(listing(["gdal", "vsi"], "/vsis3/terrio/data", gdal_env), True)
        diff = sorted(set(ours.items()) ^ set(theirs.items()))
        record("S3 recursive listing (-lR, names, sizes, dates)", not diff,
               f"{len(ours)} entries identical; requests: terrio {counts['terrio list']}, GDAL {counts['gdal list']}" if not diff else f"{diff[:6]}")
    except Exception as ex:  # noqa: BLE001
        record("S3 recursive listing (-lR, names, sizes, dates)", False, str(ex))
    # Server-side copy and download.
    for name, (tool, e) in tools.items():
        run(tool + ["copy", "-q", "-r", f"/vsis3/{name}/data", f"/vsis3/{name}/copied"], e)
        run(tool + ["copy", "-q", "-r", f"/vsis3/{name}/data", str(work / f"s3down_{name}")], e)
    a, b = keys("terrio", "copied/"), keys("gdal", "copied/")
    record("S3 to S3 recursive copy", a == b and len(a) > 0, f"{len(a)} objects identical" if a == b else f"terrio {len(a)} vs GDAL {len(b)}")
    compare_trees("S3 download of a tree", work / "s3down_terrio", work / "s3down_gdal")
    # Sync twice: the second copies nothing.
    for name, (tool, e) in tools.items():
        first = run(tool + ["sync", "-r", "-q", f"{source}/", f"/vsis3/{name}/synced"], e)
        store.log.clear()
        second = run(tool + ["sync", "-r", "-q", f"{source}/", f"/vsis3/{name}/synced"], e)
        counts[f"{name} sync succeeded"] = first.returncode == second.returncode == 0
        counts[f"{name} resync"] = sum(1 for line in store.log if line.startswith("PUT"))
    a, b = keys("terrio", "synced/"), keys("gdal", "synced/")
    record("S3 sync (timestamp), then again", a == b and bool(a) and counts["terrio sync succeeded"] and counts["gdal sync succeeded"]
           and counts["terrio resync"] == counts["gdal resync"] == 0,
           f"{len(a)} objects identical; uploads on the second run: terrio {counts['terrio resync']}, GDAL {counts['gdal resync']}")
    # Bounded deletion: data/ goes, data2/ and dirkeep stay.
    for name, (tool, e) in tools.items():
        run(tool + ["delete", "-r", f"/vsis3/{name}/data"], e)
    a, b = sorted(keys("terrio")), sorted(keys("gdal"))
    kept = all(k in a for k in ["dirkeep", "data2/keep.txt"]) and not any(k.startswith("data/") for k in a)
    record("S3 recursive delete bounded by prefix/", a == b and kept, f"remaining keys identical ({len(a)})" if a == b else f"terrio {a[:8]} vs GDAL {b[:8]}")
    # Moving a file must delete the exact source object, never its directory prefix.
    moved_files = []
    for name, (tool, e) in tools.items():
        store.objects[f"{name}/move_file"] = (b"exact object", 0)
        store.objects[f"{name}/move_file/child.tif"] = (b"child", 0)
        result = run(tool + ["move", f"/vsis3/{name}/move_file", f"/vsis3/{name}/moved_file"], e)
        remaining = keys(name)
        moved_files.append(result.returncode == 0 and "move_file" not in remaining
                           and "move_file/child.tif" in remaining and "moved_file" in remaining)
    record("S3 file move preserves coexisting prefix children", all(moved_files),
           "exact source removed, destination written, children retained")
    # An exact object and its directory prefix can coexist. Recursive deletion
    # selects the directory, preserving the exact object (GDAL 3.13.3 semantics).
    for name, (tool, e) in tools.items():
        store.objects[f"{name}/coexisting"] = (b"exact object", 0)
        store.objects[f"{name}/coexisting/child.tif"] = (b"child", 0)
        store.objects[f"{name}/coexisting2/keep.tif"] = (b"neighbor", 0)
        result = run(tool + ["delete", "-r", f"/vsis3/{name}/coexisting"], e)
        if result.returncode:
            record(f"S3 coexisting object/prefix delete ({name})", False, result.stderr[-300:])
    a, b = keys("terrio"), keys("gdal")
    record("S3 recursive delete preserves coexisting exact object", a == b
           and "coexisting" in a and "coexisting/child.tif" not in a
           and "coexisting2/keep.tif" in a, "exact object and neighboring prefix retained")
    # GDAL refuses -r on a lone object: no directory prefix exists to delete.
    refused_files = []
    for name, (tool, e) in tools.items():
        store.objects[f"{name}/single_file"] = (b"keep exact object", 0)
        result = run(tool + ["delete", "-r", f"/vsis3/{name}/single_file"], e)
        refused_files.append(result.returncode != 0 and "single_file" in keys(name))
    record("S3 recursive delete refuses a lone exact object", all(refused_files),
           "both commands fail and retain the exact object when no directory children exist")
    # Repeated nested tar members must not repeat writes of the same marker key.
    tar_source = work / "nested-marker.tar"
    with tarfile.open(tar_source, "w") as archive:
        for index in range(100):
            archive.add(source_files[index % len(source_files)], arcname=f"nested/member-{index}.tif")
    markers = []
    for name, (tool, e) in tools.items():
        store.log.clear()
        result = run(tool + ["sync", "-q", f"/vsitar/{tar_source}/", f"/vsis3/{name}/tar-sync"], e)
        marker_writes = sum(line == f"PUT /{name}/tar-sync/nested/" for line in store.log)
        markers.append(result.returncode == 0 and f"{name}/tar-sync/nested/" in store.objects
                       and (name != "terrio" or marker_writes == 1))
    record("nonrecursive tar sync creates each S3 directory marker once", all(markers),
           "100 nested real-file members retain the directory without repeated Terrio marker PUTs")
    # Refusals.
    p = run([terrio, "vsi", "delete", "-r", "/vsis3/terrio"], env)
    record("S3 whole-bucket delete refused", p.returncode != 0 and "dirkeep" in keys("terrio"), (p.stderr.strip() or "accepted")[-200:])
    server.shutdown()
    server.server_close()


def record_zip_transfer(process):
    """Every requested transfer measurement produces a success or failure result."""
    match = re.search(r"(\d+) HTTP request\(s\), (\d+) byte", process.stderr)
    if process.returncode or not match:
        record("remote zip listing transfer", False,
               "listing failed or did not report HTTP request/byte statistics")
    else:
        record("remote zip listing transfer", int(match.group(2)) < 100_000,
               f"terrio: {match.group(1)} requests, {match.group(2)} bytes of a 4,930,492-byte archive", tolerance=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("data", type=Path)
    ap.add_argument("--terrio", default=str(Path(__file__).resolve().parents[1] / "target" / "release" / "terrio"))
    ap.add_argument("--remote", action="store_true", help="list public remote stores too")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()
    terrio = args.terrio
    if shutil.which("gdal") is None:
        print("needs GDAL's gdal command (3.12 or later)", file=sys.stderr)
        return 2
    data = args.data.resolve()
    with tempfile.TemporaryDirectory(prefix="terrio-vsi-") as tmp:
        work = Path(tmp)
        # Listings of real directories and archives.
        compare_listing("local recursive listing of the test data", terrio, str(data))
        for z in sorted(data.glob("*.zip")):
            compare_listing(f"zip archive {z.name}", terrio, f"/vsizip/{z}")
        zarr = data / "lv_dem.zarr"
        if zarr.is_dir():
            compare_listing("Zarr store directory", terrio, str(zarr))
            for suffix, mode in [(".tar", "w"), (".tar.gz", "w:gz")]:
                archive = work / f"lv_dem{suffix}"
                with tarfile.open(archive, mode) as t:
                    t.add(zarr, arcname=".")
                compare_listing(f"tar archive (lv_dem.zarr{suffix})", terrio, f"/vsitar/{archive}")
        # Copies, extractions and syncs of real trees.
        source = data / "ne_places"
        source_file = next(path for path in sorted(source.iterdir()) if path.is_file())
        missing_destinations = []
        for tool, name in [([terrio, "vsi"], "terrio"), (["gdal", "vsi"], "gdal")]:
            destination = work / f"missing_{name}" / "directory"
            result = run(tool + ["copy", str(source_file), f"{destination}/"])
            missing_destinations.append(result.returncode != 0 and not destination.exists())
        record("missing local destination directory with trailing slash", all(missing_destinations),
               "both commands fail without creating the missing destination directory")
        for tool, name in [([terrio, "vsi"], "terrio"), (["gdal", "vsi"], "gdal")]:
            run(tool + ["copy", "-r", "-q", str(source), str(work / f"copy_{name}")])
            run(tool + ["copy", "-r", "-q", f"/vsizip/{data / 'nybb.zip'}", str(work / f"unzip_{name}")])
            run(tool + ["sync", "-r", "-q", str(source), str(work / f"sync_{name}")])
            run(tool + ["sync", "-q", f"{source}/", str(work / f"syncc_{name}")])
        compare_trees("recursive copy of ne_places", work / "copy_terrio", work / "copy_gdal")
        compare_trees("extraction of nybb.zip", work / "unzip_terrio", work / "unzip_gdal")
        compare_trees("sync of ne_places (directory itself)", work / "sync_terrio", work / "sync_gdal")
        compare_trees("sync of ne_places/ (contents)", work / "syncc_terrio", work / "syncc_gdal")
        s3_cases(terrio, source, work)
        if args.remote:
            unsigned = {"AWS_NO_SIGN_REQUEST": "YES"}
            compare_listing("public S3 (hrrrzarr, a Zarr array)", terrio,
                            "/vsis3/hrrrzarr/sfc/20240101/20240101_00z_anl.zarr/2m_above_ground/TMP", env=unsigned, object_store=True)
            compare_listing("public Google Cloud Storage (CMIP6 Zarr group)", terrio,
                            "/vsigs/cmip6/CMIP6/CMIP/NCAR/CESM2/historical/r1i1p1f1/Amon/tas/gn/v20190308", env={"GS_NO_SIGN_REQUEST": "YES"}, object_store=True)
            try:
                import urllib.request

                token_url = "https://planetarycomputer.microsoft.com/api/sas/v1/token/daymeteuwest/daymet-zarr"
                with urllib.request.urlopen(token_url, timeout=60) as r:
                    sas = json.load(r)["token"]
                azure = {"AZURE_STORAGE_ACCOUNT": "daymeteuwest", "AZURE_STORAGE_SAS_TOKEN": sas}
                compare_listing("Azure Blob Storage with a SAS token (Daymet Zarr array)", terrio,
                                "/vsiaz/daymet-zarr/daily/hi.zarr/lat", env=azure, object_store=True)
            except (OSError, ValueError, KeyError) as e:
                record("Azure Blob Storage with a SAS token (Daymet Zarr array)", False, f"no SAS token: {e}")
            compare_listing("HTTP index page (download.osgeo.org)", terrio, "/vsicurl/https://download.osgeo.org/gdal/data/gtiff/", extra=["--depth", "1"])
            url = "https://naciscdn.org/naturalearth/10m/cultural/ne_10m_admin_0_countries.zip"
            compare_listing("remote zip archive (Natural Earth, 4.9 MB)", terrio, f"/vsizip//vsicurl/{url}")
            p = run([terrio, "vsi", "list", "--stats", f"/vsizip//vsicurl/{url}"])
            record_zip_transfer(p)
    if args.json:
        print(json.dumps(RESULTS, indent=1))
    else:
        for r in RESULTS:
            print(f"{r['status']:>16}  {r['name']}: {r['detail']}")
    return 1 if any(r["status"] == "differs" for r in RESULTS) else 0


if __name__ == "__main__":
    sys.exit(main())
