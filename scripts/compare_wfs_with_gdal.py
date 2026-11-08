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


def check(condition, message):
    """Keep verification active even under python -O or PYTHONOPTIMIZE."""
    if not condition:
        raise AssertionError(message)


def rows(layer):
    """Key complete source IDs; otherwise preserve rows, duplicates and multiplicity."""
    result = []
    for feature in layer:
        values = dict(feature.items())
        geometry = feature.GetGeometryRef()
        result.append(
            (
                values.get("gml_id"),
                values,
                json.loads(geometry.GetLinearGeometry().ExportToJson())
                if geometry
                else None,
            )
        )
    if all(key is not None for key, _, _ in result):
        keyed = {key: (values, geometry) for key, values, geometry in result}
        check(len(keyed) == len(result), "source IDs are not unique")
        return keyed
    return [(values, geometry) for _, values, geometry in result]


def snapshot(layer):
    """Finish reference I/O before testing terrio, so reference failures stay separate."""
    crs = layer.GetSpatialRef()
    data = rows(layer)
    return {
        "crs": crs.Clone() if crs else None,
        "rows": data,
        "count": layer.GetFeatureCount(),
        "fields": {f.GetName(): f.GetType() for f in layer.schema},
    }


def compare_layer(reference, actual, *, inferred_gml=False):
    """Check source fields, CRS, every attribute, geometry type and vertex."""
    ref_crs, got_crs = reference["crs"], actual.GetSpatialRef()
    check((ref_crs is None) == (got_crs is None), "CRS presence differs")
    if ref_crs:
        ref_crs.SetAxisMappingStrategy(0)
        got_crs.SetAxisMappingStrategy(0)
        check(ref_crs.IsSame(got_crs), "CRS differs")
    ref_rows, got_rows = reference["rows"], rows(actual)
    check(len(ref_rows) == reference["count"], "reference feature read was incomplete")
    check(
        len(got_rows) == actual.GetFeatureCount(), "output feature read was incomplete"
    )
    check(
        isinstance(ref_rows, dict) == isinstance(got_rows, dict),
        "source ID presence differs",
    )
    check(
        len(ref_rows) == len(got_rows),
        f"feature counts differ ({len(ref_rows)} vs {len(got_rows)})",
    )
    if isinstance(ref_rows, dict):
        check(ref_rows.keys() == got_rows.keys(), "source IDs differ")
    ref_fields = reference["fields"]
    got_fields = {f.GetName(): f.GetType() for f in actual.schema}
    check(
        ref_fields.keys() == got_fields.keys(),
        f"fields differ: {ref_fields.keys()} vs {got_fields.keys()}",
    )

    # GeoJSON has one integral storage class and may infer Float from decimal values.
    def category(kind):
        return {0: "integer", 12: "integer", 2: "real"}.get(kind, kind)

    differences = []
    for key, kind in ref_fields.items():
        ref_kind, got_kind = category(kind), category(got_fields[key])
        if ref_kind != got_kind:
            check(
                inferred_gml and ref_kind == "real" and got_kind == "integer",
                f"field type differs at {key}",
            )
            differences.append(key)
    if isinstance(ref_rows, dict):
        for key, (attributes, geometry) in ref_rows.items():
            got_attributes, got_geometry = got_rows[key]
            check(attributes == got_attributes, f"attributes differ at {key}")
            check(close(geometry, got_geometry), f"geometry differs at {key}")
    else:
        unmatched = list(got_rows)
        for attributes, geometry in ref_rows:
            match = next(
                (
                    i
                    for i, (values, shape) in enumerate(unmatched)
                    if attributes == values and close(geometry, shape)
                ),
                None,
            )
            check(
                match is not None,
                "a feature without gml_id has no matching attributes/geometry",
            )
            unmatched.pop(match)
    detail = f"{len(ref_rows)} features; field names/attributes/CRS exact, every linear geometry vertex within 1e-8 CRS units; generated numeric FIDs ignored"
    if differences:
        detail += (
            "; known schema difference: direct GML without a companion XSD infers integer values for GDAL schema Real fields "
            + ", ".join(differences)
        )
    return detail


def compare_gml_axis_order(command, record):
    """Compare local GML axis conventions, exports and reprojection with GDAL.

    Args:
        command: Callable running the terrio CLI with positional arguments.
        record: Callable collecting a case name, status and comparison detail.
    """
    controls = [
        (6708, (4823832.72, 561996.39), (561996.39, 4823832.72)),
        (7792, (561996.39, 4823832.72), (561996.39, 4823832.72)),
        (4326, (43.56, 12.24), (12.24, 43.56)),
    ]
    forms = [
        ("urn", "urn:ogc:def:crs:EPSG::{code}", True),
        ("legacy-urn", "urn:x-ogc:def:crs:EPSG:{code}", True),
        ("url", "http://www.opengis.net/def/crs/EPSG/0/{code}", True),
        ("legacy-url", "http://www.opengis.net/gml/srs/epsg.xml#{code}", False),
        ("short", "EPSG:{code}", False),
    ]
    with tempfile.TemporaryDirectory(prefix="terrio-gml-axes-") as directory:
        root = Path(directory)
        for code, authority, xy in controls:
            for label, template, uses_authority in forms:
                name = f"GML EPSG:{code} {label}"
                pos = authority if uses_authority else xy
                source = root / f"{code}-{label}.gml"
                source.write_text(
                    '<gml:FeatureCollection xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:ex="urn:test">'
                    '<gml:featureMember><ex:p gml:id="p.0"><ex:name>a</ex:name><ex:geom>'
                    f'<gml:Point srsName="{template.format(code=code)}"><gml:pos>{pos[0]} {pos[1]}</gml:pos>'
                    "</gml:Point></ex:geom></ex:p></gml:featureMember></gml:FeatureCollection>",
                    encoding="utf-8",
                )
                try:
                    source_dataset = gdal.OpenEx(str(source), gdal.OF_VECTOR)
                    expected = snapshot(source_dataset.GetLayer(0))
                    source_dataset = None
                except Exception as error:
                    record(
                        f"{name} reference",
                        "not compared",
                        f"GDAL {gdal.VersionInfo('RELEASE_NAME')}: {error}",
                    )
                    continue
                for extension in ["gpkg", "shp", "gml"]:
                    output = root / f"{code}-{label}-out.{extension}"
                    try:
                        command("convert", str(source), str(output))
                        actual = gdal.OpenEx(str(output), gdal.OF_VECTOR)
                        detail = compare_layer(expected, actual.GetLayer(0))
                        record(f"{name} to {extension}", "within tolerance", detail)
                    except Exception as error:
                        record(f"{name} to {extension}", "differs", str(error))
                ref_json = root / f"{code}-{label}-reference.geojson"
                try:
                    ref_dataset = gdal.VectorTranslate(
                        str(ref_json), str(source), format="GeoJSON", dstSRS="EPSG:4326"
                    )
                    if ref_dataset is None:
                        raise RuntimeError("GDAL could not reproject the reference")
                    ref_dataset = None  # Close and flush output before reopening it.
                    ref_dataset = gdal.OpenEx(str(ref_json), gdal.OF_VECTOR)
                    expected_reprojected = snapshot(ref_dataset.GetLayer(0))
                    ref_dataset = None
                except Exception as error:
                    record(
                        f"{name} to EPSG:4326 reference",
                        "not compared",
                        f"GDAL {gdal.VersionInfo('RELEASE_NAME')}: {error}",
                    )
                    continue
                output = root / f"{code}-{label}-out.geojson"
                try:
                    command("convert", str(source), str(output), "--t-srs", "EPSG:4326")
                    actual = gdal.OpenEx(str(output), gdal.OF_VECTOR)
                    detail = compare_layer(expected_reprojected, actual.GetLayer(0))
                    record(f"{name} to EPSG:4326", "within tolerance", detail)
                except Exception as error:
                    record(f"{name} to EPSG:4326", "differs", str(error))


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

    compare_gml_axis_order(command, record)
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
                    check(
                        names == [layer["name"] for layer in info["layers"]],
                        "advertised layer names/order differ",
                    )
                    check(
                        info["metadata_only"]
                        and all(
                            layer["feature_count"] is None for layer in info["layers"]
                        ),
                        "capability summary must be metadata-only with unknown feature counts",
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
                    first = snapshot(reference.GetLayer(0))
                    if first["count"] != len(first["rows"]):
                        raise RuntimeError("GDAL feature read was incomplete")
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
                    first = snapshot(reference.GetLayer(0))
                except Exception as error:
                    record(
                        "extensionless " + label,
                        "not compared",
                        f"reference response unavailable: {error}",
                    )
                    continue
                try:
                    # Use an output file to compare complete attributes and geometries, not just counts.
                    output = str(Path(directory) / f"response-{label}.geojson")
                    command("convert", url, output)
                    actual = gdal.OpenEx(output, gdal.OF_VECTOR)
                    record(
                        "extensionless " + label,
                        "within tolerance",
                        compare_layer(
                            first,
                            actual.GetLayer(0),
                            inferred_gml=label == "GML",
                        ),
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
