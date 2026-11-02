# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Compare the public WFS services in issues #494/#495 with GDAL 3.13.3.

Run with --remote to contact services. Unavailable references are reported as not
compared, never passes. Downloads and converted output stay in a temporary folder.
"""

from __future__ import annotations

import argparse
import json
import math
import subprocess
import tempfile
from pathlib import Path

from osgeo import gdal

BOLZANO = "https://geoservices1.civis.bz.it/geoserver/p_bz-TransportNetwork/ows"
SERVICES = [
    ("Bolzano WFS 2.0", BOLZANO + "?service=WFS&version=2.0.0&request=GetCapabilities"),
    ("Bolzano WFS 1.0", BOLZANO + "?service=WFS&version=1.0.0&request=GetCapabilities"),
    (
        "Quarrata WFS 1.1",
        "https://sit.comunequarrata.duckdns.org/ows/metadati-rndt/qdjango/102/?SERVICE=WFS&VERSION=1.1.0&REQUEST=GetCapabilities",
    ),
    (
        "Piemonte hydrography",
        "https://geoservices.csi.it/ms/wfs/taims/rp-01/taimswfs/bdtre_idro?service=WFS&version=1.0.0&request=getCapabilities",
    ),
    (
        "Piemonte protected areas",
        "https://geomap.reteunitaria.piemonte.it/ws/gsareprot/rp-01/areeprotwfs/wfs_gsareprot_1?service=WFS&version=1.0.0&request=getCapabilities",
    ),
    (
        "PCN landslides",
        "http://wms.pcn.minambiente.it/ogc?map=/ms_ogc/wfs/Catalogo_Frane.map&service=wfs&request=getCapabilities",
    ),
]


def close(a, b):
    """Compare geometry JSON recursively, with absolute XY/Z tolerance 1e-8 CRS units."""
    if isinstance(a, (float, int)) and isinstance(b, (float, int)):
        return math.isclose(a, b, rel_tol=0, abs_tol=1e-8)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(close(x, y) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        # GDAL may include a coordinate envelope in exported geometry JSON.
        a = {k: v for k, v in a.items() if k != "bbox"}
        b = {k: v for k, v in b.items() if k != "bbox"}
        return a.keys() == b.keys() and all(close(a[k], b[k]) for k in a)
    return a == b


def rows(layer):
    """Keep source gml_id values; generated numeric FIDs vary between drivers."""
    result = {}
    for feature in layer:
        values = dict(feature.items())
        key = values.get("gml_id", str(feature.GetFID()))
        geometry = feature.GetGeometryRef()
        result[key] = (
            values,
            json.loads(geometry.ExportToJson()) if geometry else None,
        )
    return result


def compare_layer(reference, actual):
    """Check source fields, CRS, every attribute, geometry type and vertex."""
    ref_crs, got_crs = reference.GetSpatialRef(), actual.GetSpatialRef()
    assert (ref_crs is None) == (got_crs is None), "CRS presence differs"
    if ref_crs:
        ref_crs.SetAxisMappingStrategy(0)
        got_crs.SetAxisMappingStrategy(0)
        assert ref_crs.IsSame(got_crs), "CRS differs"
    ref_rows, got_rows = rows(reference), rows(actual)
    assert len(ref_rows) == reference.GetFeatureCount(), (
        "reference source IDs are not unique"
    )
    assert len(got_rows) == actual.GetFeatureCount(), "output source IDs are not unique"
    assert ref_rows.keys() == got_rows.keys(), (
        f"source IDs/counts differ ({len(ref_rows)} vs {len(got_rows)})"
    )
    ref_fields = {f.GetName(): f.GetType() for f in reference.schema}
    got_fields = {f.GetName(): f.GetType() for f in actual.schema}
    assert ref_fields.keys() == got_fields.keys(), (
        f"fields differ: {ref_fields.keys()} vs {got_fields.keys()}"
    )

    # GeoJSON has one integral storage class and may infer Float from decimal values.
    def category(kind):
        return {0: "integer", 12: "integer", 2: "real"}.get(kind, kind)

    assert {k: category(v) for k, v in ref_fields.items()} == {
        k: category(v) for k, v in got_fields.items()
    }, "field types differ"
    for key, (attributes, geometry) in ref_rows.items():
        got_attributes, got_geometry = got_rows[key]
        assert attributes == got_attributes, f"attributes differ at {key}"
        assert close(geometry, got_geometry), f"geometry differs at {key}"
    return f"{len(ref_rows)} features; fields/attributes/CRS exact, all vertices within 1e-8 CRS units; generated numeric FIDs ignored"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--remote", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    gdal.SetConfigOption("GDAL_HTTP_TIMEOUT", "20")
    gdal.SetConfigOption("GDAL_HTTP_MAX_RETRY", "0")
    cases = []

    def record(name, status, detail):
        cases.append(dict(name=name, status=status, detail=detail))

    def command(*arguments):
        proc = subprocess.run(
            [args.terrio, *arguments],
            capture_output=True,
            text=True,
            timeout=90,
            check=False,
        )
        if proc.returncode:
            raise RuntimeError(proc.stderr.strip())
        return proc.stdout

    if not args.remote:
        record("public WFS", "not compared", "pass --remote to contact public services")
    else:
        with tempfile.TemporaryDirectory(prefix="terrio-wfs-") as directory:
            for index, (name, url) in enumerate(SERVICES):
                try:
                    reference = gdal.OpenEx(
                        "WFS:" + url, gdal.OF_VECTOR | gdal.OF_READONLY
                    )
                    if reference is None:
                        raise RuntimeError("GDAL could not open service")
                    names = [
                        reference.GetLayer(i).GetName()
                        for i in range(reference.GetLayerCount())
                    ]
                except Exception as error:
                    record(
                        name + " catalog",
                        "not compared",
                        f"GDAL {gdal.VersionInfo('RELEASE_NAME')}: {error}",
                    )
                    continue
                try:
                    info = json.loads(command("info", url, "--json"))
                    assert names == [layer["name"] for layer in info["layers"]], (
                        "advertised layer names/order differ"
                    )
                    assert info["metadata_only"] and all(
                        layer["feature_count"] is None for layer in info["layers"]
                    )
                    record(
                        name + " catalog",
                        "identical",
                        f"{len(names)} feature types; counts remain unknown without downloading features",
                    )
                except Exception as error:
                    record(name + " catalog", "differs", str(error))
                    continue
                if not names:
                    continue
                try:
                    first = reference.GetLayer(0)
                    # Force reference I/O before treating a failure in terrio as a difference.
                    expected = rows(first)
                    if first.GetFeatureCount() != len(expected):
                        raise RuntimeError("GDAL feature read was incomplete")
                    first.ResetReading()
                except Exception as error:
                    record(
                        name + " features",
                        "not compared",
                        f"reference feature request unavailable: {error}",
                    )
                    continue
                try:
                    output = str(Path(directory) / f"service-{index}.geojson")
                    command(
                        "convert",
                        "WFS:" + url,
                        output,
                        "--layer",
                        names[0],
                        "--oo",
                        "WFS_PAGE_SIZE=500",
                    )
                    actual = gdal.OpenEx(output, gdal.OF_VECTOR)
                    detail = compare_layer(first, actual.GetLayer(0))
                    record(name + " features", "within tolerance", detail)
                except Exception as error:
                    record(name + " features", "differs", str(error))
            feature_url = (
                BOLZANO
                + "?service=WFS&version=2.0.0&request=GetFeature&count=5&typeNames=p_bz-TransportNetwork:UseConditions-Limitations"
            )
            for suffix, label in [
                ("", "GML"),
                ("&outputFormat=application/json", "GeoJSON"),
            ]:
                url = feature_url + suffix
                try:
                    reference = gdal.OpenEx(url, gdal.OF_VECTOR)
                    if reference is None:
                        raise RuntimeError("GDAL could not open response")
                    # Use an output file to compare complete attributes and geometries, not just counts.
                    output = str(Path(directory) / f"response-{label}.geojson")
                    command("convert", url, output)
                    actual = gdal.OpenEx(output, gdal.OF_VECTOR)
                    record(
                        "extensionless " + label,
                        "within tolerance",
                        compare_layer(reference.GetLayer(0), actual.GetLayer(0)),
                    )
                except Exception as error:
                    record("extensionless " + label, "differs", str(error))
    if args.json:
        print(json.dumps(cases, indent=2))
    else:
        print(f"GDAL {gdal.VersionInfo('RELEASE_NAME')}")
        for case in cases:
            print(f"{case['status']}: {case['name']}: {case['detail']}")
    return int(any(case["status"] == "differs" for case in cases))


if __name__ == "__main__":
    raise SystemExit(main())
