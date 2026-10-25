"""Ensure diagnostics expose errors hidden behind the benchmark's first-strip failure."""

import sys
import json
import unittest
from pathlib import Path

import numpy as np
from osgeo import gdal

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from diagnose_benchmark_outputs import json_safe, max_ulp, raster_differences


class DiagnosticTests(unittest.TestCase):
    def test_complex_error_and_strict_json(self):
        a, b = [gdal.GetDriverByName("MEM").Create("", 1, 1, 1, gdal.GDT_CFloat64) for _ in range(2)]
        a.GetRasterBand(1).WriteArray(np.array([[1 + 2j]]))
        b.GetRasterBand(1).WriteArray(np.array([[1 + 3j]]))
        stats = raster_differences(a, b)["bands"][0]["levels"][0]
        self.assertEqual(stats["max_absolute"], 1.0)
        self.assertAlmostEqual(stats["max_relative"], 1 / np.sqrt(10))
        self.assertEqual(json.loads(json.dumps(json_safe({"overflow": float("inf")}), allow_nan=False)), {"overflow": "Infinity"})

    def test_ulp_signs_and_adjacent_values(self):
        for dtype in (np.float32, np.float64):
            a = np.array([-1.0, -0.0, 0.0, 1.0], dtype=dtype)
            self.assertEqual(max_ulp(a, a), 0)
            self.assertEqual(
                max_ulp(np.array([-0.0], dtype=dtype), np.array([0.0], dtype=dtype)), 0
            )
            b = np.nextafter(a, np.array(np.inf, dtype=dtype))
            self.assertEqual(max_ulp(a, b), 1)

    def test_counts_later_strips_and_masks_separately(self):
        datasets = [
            gdal.GetDriverByName("MEM").Create("", 2, 260, 1, gdal.GDT_Float32)
            for _ in range(2)
        ]
        a, b = datasets
        data = np.ones((260, 2), dtype=np.float32)
        a.GetRasterBand(1).WriteArray(data)
        data[1, 0] = 2
        data[259, 1] = 4
        b.GetRasterBand(1).WriteArray(data)
        for ds in datasets:
            ds.GetRasterBand(1).CreateMaskBand(gdal.GMF_PER_DATASET)
            ds.GetRasterBand(1).GetMaskBand().Fill(255)
        b.GetRasterBand(1).GetMaskBand().WriteArray(
            np.zeros((1, 1), dtype=np.uint8), 1, 259
        )
        stats = raster_differences(a, b)["bands"][0]["levels"][0]
        self.assertEqual(stats["differing_pixels"], 2)
        self.assertEqual(stats["mask_differences"], 1)
        self.assertEqual(stats["differing_valid"], 1)
        self.assertEqual(stats["max_absolute"], 1)


if __name__ == "__main__":
    unittest.main()
