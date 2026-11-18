# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate crates/terrio-crs/tests/data/projjson_corpus_{epsg,esri}.jsonl.zst: every CRS of terrio's EPSG table
(geographic, geocentric, projected, vertical and compound) and ESRI table, as PROJ 9.9.0 writes it
in WKT 2 (`WKT2_2019`), GDAL WKT 1 (`WKT1_GDAL`) and ESRI WKT 1 (`WKT1_ESRI`), each with the
PROJJSON PROJ writes for that text (`CRS.from_wkt(text).to_json_dict()`), without the fields the
test leaves out. A form PROJ cannot write is left out. One JSON array per line:
`["EPSG:2000", "wkt2", text, projjson]`.

The test `projjson_corpus.rs` reads each text with terrio-crs and compares its PROJJSON with
PROJ's (opengeos/terrio#1058).

Usage: PROJ_NETWORK=OFF python scripts/gen_projjson_corpus.py (needs pyproj 3.8.0 and the zstd
command)
"""

import json
import os
import subprocess
from pathlib import Path

import pyproj
import pyproj.network
from pyproj import CRS

ROOT = Path(__file__).resolve().parents[1]
DATA = ROOT / "crates/terrio-crs/data"
OUT = ROOT / "crates/terrio-crs/tests/data"

FORMS = (("wkt2", "WKT2_2019"), ("wkt1", "WKT1_GDAL"), ("esri", "WKT1_ESRI"))
LEFT_OUT = {"$schema", "usages", "scope", "area", "bbox", "remarks", "remark"}


def rows(table, kinds):
    text = subprocess.run(["zstd", "-dc", str(DATA / table)], check=True, capture_output=True, text=True).stdout
    return sorted({int(f[1]) for f in (line.split("\t") for line in text.splitlines()) if f[0] in kinds})


def strip(value, field=""):
    """The PROJJSON without what the test leaves out: usages, remarks, the names of axes and
    conversions, axis abbreviations, ensemble members and accuracy, and the IDs of objects other
    than CRSs, methods and parameters. Parameters keep their names and IDs, which key them."""
    if isinstance(value, dict):
        is_crs = str(value.get("type", "")).endswith("CRS")

        def kept(k):
            if k in ("id", "ids"):
                return is_crs or field in ("method", "parameters")
            if k == "name":
                return field not in ("axis", "conversion")
            return k not in LEFT_OUT and k not in ("abbreviation", "members", "accuracy")

        return {k: strip(v, k) for k, v in value.items() if kept(k)}
    if isinstance(value, list):
        return [strip(v, field) for v in value]
    return value


def main():
    assert pyproj.proj_version_str == "9.9.0", pyproj.proj_version_str
    assert pyproj.__version__ == "3.8.0", pyproj.__version__
    pyproj.network.set_network_enabled(False)
    assert os.environ.get("PROJ_NETWORK") == "OFF"
    codes = [("EPSG", c) for c in rows("epsg.tsv.zst", {"C", "V", "K"})]
    codes += [("ESRI", c) for c in rows("esri.tsv.zst", {"C"})]
    lines, skipped, unread = [], [], []
    for authority, code in codes:
        try:
            crs = CRS.from_authority(authority, code)
        except pyproj.exceptions.CRSError:
            skipped.append(f"{authority}:{code}")
            continue
        for form, version in FORMS:
            try:
                text = crs.to_wkt(version)
            except pyproj.exceptions.CRSError:
                text = None
            if not text:
                continue
            # PROJ reads back the text it wrote; a text it cannot read is reported, not kept.
            try:
                projjson = strip(CRS.from_wkt(text).to_json_dict())
            except pyproj.exceptions.CRSError:
                unread.append(f"{authority}:{code} {form}")
                continue
            lines.append(json.dumps([f"{authority}:{code}", form, text, projjson], separators=(",", ":"), ensure_ascii=False))
    # One file per authority, each under the repository's 1 MiB limit.
    for authority in ("EPSG", "ESRI"):
        data = "".join(f"{line}\n" for line in lines if line.startswith(f'["{authority}:')).encode()
        out = OUT / f"projjson_corpus_{authority.lower()}.jsonl.zst"
        out.write_bytes(subprocess.run(["zstd", "-19", "-c"], input=data, check=True, capture_output=True).stdout)
        print(f"{out.name}: {out.stat().st_size} bytes")
    print(f"{len(codes)} CRSs, {len(lines)} texts")
    print(f"PROJ has no CRS for {len(skipped)}: {' '.join(skipped)}")
    print(f"PROJ cannot read back {len(unread)} texts it wrote: {', '.join(unread)}")


if __name__ == "__main__":
    main()
