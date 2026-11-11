"""Categorical Deflate TIFF sizes and exact GDAL decoding (reference GDAL 3.13.3).

Size ceilings protect against the issue #712 regression; compressed streams are
not expected to equal GDAL's encoder output. A WorldCover crop is included when
DATA/esa_worldcover_colormap.tif exists. No data is downloaded or modified.
"""

import argparse
import json
from pathlib import Path
import tempfile

import numpy as np
from osgeo import gdal, osr
import terrio.rio as rio
from terrio.rio.transform import Affine, from_origin


def compare(values, name, directory, *, ceiling=None, nodata=None, transform=None, crs="EPSG:32633"):
    path = directory / f"{name}.tif"
    transform = transform or from_origin(500000, 5800000, 10, 10)
    with rio.open(
        path, "w", driver="GTiff", width=values.shape[1], height=values.shape[0],
        count=1, dtype="uint8", crs=crs, transform=transform, nodata=nodata,
        tiled=True, blockxsize=512, blockysize=512, compress="DEFLATE",
    ) as dst:
        dst.write(values, 1)
    with rio.open(path) as native:
        np.testing.assert_array_equal(native.read(1), values)
    ref = gdal.Open(str(path))
    band = ref.GetRasterBand(1)
    np.testing.assert_array_equal(ref.ReadAsArray(), values)
    if band.DataType != gdal.GDT_Byte or band.GetNoDataValue() != nodata:
        raise ValueError("sample type or nodata changed")
    mask = np.full(values.shape, 255, dtype="uint8") if nodata is None else np.where(values == nodata, 0, 255).astype("uint8")
    np.testing.assert_array_equal(band.GetMaskBand().ReadAsArray(), mask)
    if ref.GetGeoTransform() != transform.to_gdal():
        raise ValueError("affine changed")
    expected = osr.SpatialReference()
    expected.SetFromUserInput(crs)
    if not ref.GetSpatialRef().IsSame(expected):
        raise ValueError("CRS changed")
    size = path.stat().st_size
    if ceiling is not None:
        if size > ceiling:
            raise ValueError(f"{name}: {size} > {ceiling} bytes")
    return dict(name=name, status="identical", detail=f"{size} bytes; exact samples, mask, Byte type, nodata, affine and CRS; GDAL {gdal.VersionInfo('RELEASE_NAME')}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    y, x = np.mgrid[:1024, :1024]
    cases = {
        "squares_64": ((x // 64 + y // 64) % 10).astype("uint8"),
        "blocks_37x53": ((x // 37 + y // 53) % 10).astype("uint8"),
        "stripes_37": ((x // 37) % 10).astype("uint8"),
        "disc_mask": ((x - 512) ** 2 + (y - 512) ** 2 < 400 ** 2).astype("uint8"),
        "random": np.random.default_rng(7).integers(0, 256, (1024, 1024)).astype("uint8"),
    }
    rows = []
    with tempfile.TemporaryDirectory(prefix="terrio-deflate-") as directory:
        directory = Path(directory)
        for name, values in cases.items():
            try:
                rows.append(compare(values, name, directory, ceiling=8500 if name in list(cases)[:3] else None))
            except Exception as error:
                rows.append(dict(name=name, status="differs", detail=str(error)))
        source = args.data / "esa_worldcover_colormap.tif"
        if source.exists():
            try:
                src = gdal.Open(str(source))
                width, height = min(1024, src.RasterXSize), min(1024, src.RasterYSize)
                xoff, yoff = (src.RasterXSize - width) // 2, (src.RasterYSize - height) // 2
                values = src.ReadAsArray(xoff, yoff, width, height)
                transform = Affine.from_gdal(*src.GetGeoTransform()) * Affine.translation(xoff, yoff)
                rows.append(compare(values, "WorldCover crop", directory, nodata=src.GetRasterBand(1).GetNoDataValue(), transform=transform, crs=src.GetProjection()))
            except Exception as error:
                rows.append(dict(name="WorldCover crop", status="differs", detail=str(error)))
    print(json.dumps(rows, indent=2) if args.json else "\n".join(f"{row['name']}: {row['status']}: {row['detail']}" for row in rows))
    return int(any(row["status"] == "differs" for row in rows))


if __name__ == "__main__":
    raise SystemExit(main())
