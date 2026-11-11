#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Check JP2 channel roles and exact real Landsat components against GDAL 3.13.3."""

import argparse
import json
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal

import terrio
import terrio.rio as rio
from terrio.rio.enums import ColorInterp


def compare(path, values, roles):
    """GDAL and Native reads must retain the declared components, masks and absence of a grid."""
    reference = gdal.Open(str(path))
    expected_roles = [gdal.GetColorInterpretationName(reference.GetRasterBand(i+1).GetColorInterpretation()) for i in range(len(roles))]
    assert expected_roles == list(roles), (expected_roles, roles)
    actual = reference.ReadAsArray()
    np.testing.assert_array_equal(actual, values)
    assert actual.dtype == values.dtype
    assert reference.GetGeoTransform(can_return_null=True) is None, ("unexpected grid", reference.GetGeoTransform())
    masks = np.stack([reference.GetRasterBand(i+1).GetMaskBand().ReadAsArray() for i in range(len(roles))])
    with rio.open(path) as native:
        assert native.dtypes == (str(values.dtype),) * len(roles)
        assert [role.name.lower() for role in native.colorinterp] == [role.lower() for role in roles]
        np.testing.assert_array_equal(native.read(), values)
        np.testing.assert_array_equal(native.read_masks(), masks)
    reference = None
    assert not terrio.open_raster(str(path), lazy=True).has_transform
    return dict(same_size=True, same_type=True, same_metadata=True, same_georeferencing=True,
                valid_mismatch=0, differing=0, max_diff=0.0, within_tol=1.0, exact=1.0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--node-package", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    source = next((args.data / name for name in ("landsat7.tif", "landsat.tif") if (args.data / name).is_file()), None)
    if source is None:
        raise FileNotFoundError("landsat7.tif or landsat.tif is required")
    original = gdal.Open(str(source))
    assert original.RasterCount >= 3
    width, height = min(64, original.RasterXSize), min(64, original.RasterYSize)
    pixels = np.stack([original.GetRasterBand(i).ReadAsArray(0, 0, width, height) for i in (1, 2, 3)])
    original = None
    assert pixels.dtype in (np.dtype("uint8"), np.dtype("uint16")), pixels.dtype
    alpha = (np.arange(width * height).reshape(height, width) * 31 % np.iinfo(pixels.dtype).max).astype(pixels.dtype)
    cases = [
        ("undefined", ("Undefined",) * 3, pixels),
        ("BGR", ("Blue", "Green", "Red"), pixels[[2, 1, 0]]),
        ("RGBA", ("Red", "Green", "Blue", "Alpha"), np.concatenate([pixels, alpha[None]])),
        ("ARGB", ("Alpha", "Red", "Green", "Blue"), np.concatenate([alpha[None], pixels])),
    ]
    records = []
    with tempfile.TemporaryDirectory(prefix="terrio-jp2-colors-") as directory:
        root = Path(directory)
        for name, roles, values in cases:
            input_path = root / f"{name}.tif"
            profile = dict(width=width, height=height, count=len(roles), dtype=str(values.dtype))
            colors = [ColorInterp[role.lower()] for role in roles]
            with rio.open(input_path, "w", driver="GTiff", **profile) as dst:
                dst.colorinterp = colors
                dst.write(values)
            surfaces = ["python", "python-bytes", "cli"] + (["node"] if args.node_package else [])
            for surface in surfaces:
                record = dict(name=f"JP2 {name} {surface}", reference_gdal=gdal.VersionInfo("RELEASE_NAME"))
                output = root / f"{name}-{surface}.jp2"
                try:
                    if surface == "python":
                        with rio.open(output, "w", driver="JP2", **profile) as dst:
                            dst.colorinterp = colors
                            dst.write(values)
                    elif surface == "python-bytes":
                        output.write_bytes(terrio.open_raster(str(input_path), lazy=True).to_bytes("image.jp2"))
                    elif surface == "cli":
                        subprocess.run([args.terrio, "convert", str(input_path), str(output)], check=True, capture_output=True, text=True)
                    else:
                        program = """
const fs = require('node:fs');
const t = require(process.argv[1]);
const source = t.open(fs.readFileSync(process.argv[2]), 'input.tif');
fs.writeFileSync(process.argv[3], source.toBytes('output.jp2'));
source.free();
"""
                        subprocess.run(["node", "-e", program, str(args.node_package.resolve()), str(input_path), str(output)],
                                       check=True, capture_output=True, text=True)
                    record.update(compare(output, values, roles))
                except Exception as error:
                    record["error"] = f"{type(error).__name__}: {error}"
                records.append(record)
    if args.json:
        print(json.dumps(records, indent=2))
    else:
        for record in records:
            print(f"{record['name']}: {record.get('error', 'exact values, types, masks and roles')}")
    return int(any("error" in record for record in records))


if __name__ == "__main__":
    raise SystemExit(main())
