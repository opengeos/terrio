"""Exercise correctness gates with corrupt outputs, including GDAL statistics."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import numpy as np
from osgeo import gdal, ogr

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from benchmark_validation import (
    compare_rasters,
    compare_vectors,
    numpy_statistics,
    validate_summary,
)


class ValidationTests(unittest.TestCase):
    def raster(self, value=7):
        ds = gdal.GetDriverByName("MEM").Create("", 4, 3, 1, gdal.GDT_Int16)
        ds.SetGeoTransform((1, 2, 0, 9, 0, -2))
        ds.GetRasterBand(1).Fill(value)
        return ds

    def test_wrong_pixels_and_masks_fail(self):
        a, b = self.raster(), self.raster()
        self.assertEqual(compare_rasters(a, b), [])
        b.GetRasterBand(1).Fill(8)
        self.assertTrue(compare_rasters(a, b))
        b.GetRasterBand(1).Fill(7)
        b.GetRasterBand(1).CreateMaskBand(0)
        b.GetRasterBand(1).GetMaskBand().Fill(0)
        self.assertTrue(compare_rasters(a, b))

    def test_metadata_and_overviews_fail(self):
        a, b = self.raster(), self.raster()
        b.GetRasterBand(1).SetNoDataValue(-9999)
        self.assertTrue(compare_rasters(a, b))
        b = self.raster()
        b.SetGeoTransform((1, 3, 0, 9, 0, -2))
        self.assertTrue(compare_rasters(a, b))
        b = self.raster()
        b.BuildOverviews("NEAREST", [2])
        self.assertTrue(compare_rasters(a, b))

    def test_complex_raster_mismatch_with_nan_is_reported(self):
        a, b = (gdal.GetDriverByName("MEM").Create("", 2, 1, 1, gdal.GDT_CFloat64) for _ in range(2))
        for ds in (a, b):
            ds.SetGeoTransform((1, 2, 0, 9, 0, -2))
        a.GetRasterBand(1).WriteArray(np.array([[complex(float("nan"), 1), 2 + 3j]], dtype=np.complex128))
        b.GetRasterBand(1).WriteArray(np.array([[complex(float("nan"), 1), 2 + 4j]], dtype=np.complex128))
        differences = compare_rasters(a, b)
        self.assertEqual(len(differences), 1)
        self.assertIn("1 differing pixels", differences[0])

    def test_independent_statistics_and_poisoned_gdal_summary(self):
        with tempfile.TemporaryDirectory() as root:
            path = str(Path(root) / "source.tif")
            ds = gdal.GetDriverByName("GTiff").CreateCopy(path, self.raster())
            ds.Close()
            t = {
                "type": "raster",
                "size": [4, 3],
                "band_count": 1,
                "geo_transform": [1, 2, 0, 9, 0, -2],
                "crs": {"epsg": None},
                "bands": [
                    {
                        "band": 1,
                        "data_type": "Int16",
                        "nodata": None,
                        "statistics": {"min": 7, "max": 7, "mean": 7, "std_dev": 0},
                    }
                ],
            }
            g = {
                "size": [4, 3],
                "geoTransform": t["geo_transform"],
                "bands": [
                    {
                        "band": 1,
                        "type": "Int16",
                        "metadata": {
                            "": {
                                "STATISTICS_MINIMUM": "7",
                                "STATISTICS_MAXIMUM": "7",
                                "STATISTICS_MEAN": "7",
                                "STATISTICS_STDDEV": "0",
                            }
                        },
                    }
                ],
            }
            dirs = [Path(root) / name for name in ("t", "g")]
            for d in dirs:
                d.mkdir()
            (dirs[0] / "stdout.json").write_text(json.dumps(t))
            (dirs[1] / "stdout.json").write_text(json.dumps(g))
            cmd = ["terrio", "info", "--json", "--stats", path]
            self.assertEqual(validate_summary(cmd, [], *dirs), [])
            g["bands"][0]["metadata"][""]["STATISTICS_MEAN"] = "8"
            (dirs[1] / "stdout.json").write_text(json.dumps(g))
            self.assertIn("GDAL statistics differ", validate_summary(cmd, [], *dirs)[0])
            # Ordinary info has nullable statistics; --stats must not silently omit them.
            t["bands"][0]["statistics"] = None
            (dirs[0] / "stdout.json").write_text(json.dumps(t))
            self.assertEqual(validate_summary(["terrio", "info", "--json", path], [], *dirs), [])
            self.assertIn("requested statistics missing", validate_summary(cmd, [], *dirs)[0])

    def test_statistics_excludes_nodata(self):
        ds = self.raster()
        band = ds.GetRasterBand(1)
        band.WriteArray(np.array([[1, 2, 3, 4], [1, 2, 3, 4], [-9999] * 4], dtype=np.int16))
        band.SetNoDataValue(-9999)
        self.assertTrue(np.allclose(numpy_statistics(band), [1, 4, 2.5, np.sqrt(1.25)]))

    def vector(self, values):
        """Create numeric point features for whole-layer and sort-dependency checks."""
        ds = ogr.GetDriverByName("MEM").CreateDataSource("")
        layer = ds.CreateLayer("points", geom_type=ogr.wkbPoint)
        layer.CreateField(ogr.FieldDefn("value", ogr.OFTInteger))
        for value in values:
            feature = ogr.Feature(layer.GetLayerDefn())
            feature.SetField("value", value)
            feature.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({value} 0)"))
            layer.CreateFeature(feature)
        return ds

    def test_every_vector_feature_and_order(self):
        a, b = self.vector([1, 2]), self.vector([2, 1])
        self.assertEqual(compare_vectors(a, b), [])
        self.assertTrue(compare_vectors(a, self.vector([1, 3])))

    def test_missing_sort_reports_a_validation_failure(self):
        a, b = self.vector([1]), self.vector([1])
        with patch("benchmark_validation.shutil.which", return_value=None):
            self.assertIn("requires external sort", compare_vectors(a, b)[0])

    def test_incompatible_sort_reports_its_stderr(self):
        a, b = self.vector([1]), self.vector([1])
        failure = subprocess.CalledProcessError(2, ["sort"], stderr="unsupported -S option")
        with patch("benchmark_validation.subprocess.run", side_effect=failure):
            difference = compare_vectors(a, b)[0]
        self.assertIn("sort failed", difference)
        self.assertIn("unsupported -S option", difference)


if __name__ == "__main__":
    unittest.main()
