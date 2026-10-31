// Installs the Pyodide wheel in Pyodide under Node.js and checks that terrio works there:
// a GeoTIFF and a JPEG 2000 file written and read back (from Pyodide's file system and bytes),
// terrio.rio reads by window, and a CRS transformation.
//
//   npm install pyodide@<version>
//   node python/tests/pyodide/smoke.mjs python/dist/terrio-*-pyodide_*_wasm32.whl
import { readFileSync } from "node:fs";
import { basename } from "node:path";
import { loadPyodide } from "pyodide";

const wheel = process.argv[2];
if (!wheel) {
  console.error("usage: node smoke.mjs <terrio wheel>");
  process.exit(2);
}
const pyodide = await loadPyodide();
await pyodide.loadPackage(["micropip", "numpy"]);
const target = `/tmp/${basename(wheel)}`;
pyodide.FS.writeFile(target, readFileSync(wheel));
pyodide.globals.set("wheel_path", target);
await pyodide.runPythonAsync(`
import micropip
await micropip.install(["affine", "emfs:" + wheel_path])
`);
const result = pyodide.runPython(`
import numpy as np
import terrio
import terrio.rio as rasterio
from terrio.rio.windows import Window

data = (np.arange(64 * 48, dtype="int16").reshape(1, 48, 64) - 1000)
gt = (500000.0, 30.0, 0.0, 4100000.0, 0.0, -30.0)
terrio.Raster.from_array(data, gt, "EPSG:32611", -9999).write(
    "/tmp/t.tif", compress="DEFLATE", tile_size=16
)

r = terrio.open_raster("/tmp/t.tif", lazy=True)
assert (r.width, r.height, r.count, r.dtype) == (64, 48, 1, "int16"), r
assert r.nodata == -9999 and r.transform == gt
assert np.array_equal(r.read(1), data[0])

# JPEG 2000, through the OpenJPEG port and Emscripten's C library.
terrio.Raster.from_array(data, gt, "EPSG:32611").write("/tmp/t.jp2")
assert np.array_equal(terrio.open_raster("/tmp/t.jp2").read(1), data[0])

b = terrio.open_bytes(open("/tmp/t.tif", "rb").read(), "t.tif")
assert np.array_equal(b.read(1), data[0])

with rasterio.open("/tmp/t.tif") as src:
    assert src.crs.to_epsg() == 32611
    w = src.read(1, window=Window(10, 5, 20, 7))
    assert np.array_equal(w, data[0, 5:12, 10:30])

xs, ys = terrio.transform_points([-117.0], [37.0], "EPSG:4326", "EPSG:32611")
assert abs(xs[0] - 500000.0) < 1e-6 and abs(ys[0] - 4094872.370454) < 1e-5, (xs, ys)

# Palettes and indexes work on in-memory datasets; paths and their output protection do not.
rgb = np.stack([np.arange(64 * 48, dtype="uint8").reshape(48, 64)] * 3)
rgb_raster = terrio.Raster.from_array(rgb, gt, "EPSG:32611")
assert terrio.rgb_to_palette(rgb_raster, color_count=8).count == 1
index = terrio.raster_index({"a.tif": rgb_raster})
assert index.to_bytes("index.fgb")
refused = [
    lambda: terrio.rgb_to_palette(rgb_raster, "/tmp/p.tif", color_count=8),
    lambda: terrio.read_color_table("/tmp/t.tif"),
    lambda: terrio.raster_index("/tmp/t.tif"),
    lambda: terrio.vector_index("/tmp/v.fgb"),
    lambda: index.write("/tmp/index.fgb"),
]
for call in refused:
    try:
        call()
    except Exception as error:
        assert "this build" in str(error), error
    else:
        raise AssertionError("expected this build to refuse the call")
f"terrio {terrio.__version__} in Pyodide: ok"
`);
console.log(result);
