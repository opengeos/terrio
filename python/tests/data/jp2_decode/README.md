# Irreversible JPEG 2000 decode references

These four synthetic payloads freeze the cases from `test_jp2_writer.py` whose
live GDAL comparisons allow sparse one-step differences on macOS arm64 (#887).
`decoded.npz` contains the corresponding arrays in bands, rows, columns order.
Every dtype, shape and sample must match exactly on every platform. Tests read
the frozen payloads rather than invoking an encoder, so an encoder change cannot
change the input to this decoder regression. No rasterio dependency is needed
to run the regression.

Generated on Linux x86_64 from Terrio commit fa06830e with NumPy 2.3.3 and
rasterio 1.5.0 / GDAL 3.12.1. GDAL's JP2OpenJPEG decode and Terrio's native decode
agree exactly for all four arrays. The arrays are captured from GDAL as an
independent reference. There is no platform tolerance in this fixture check.

To deliberately regenerate, build the current Python bindings, install NumPy,
pytest and rasterio with JP2OpenJPEG, then run the following from the repository
root on Linux x86_64. Review any changed arrays against the prior reference;
do not regenerate just to make a decoder regression pass.

```python
import platform
import runpy
import sys
from pathlib import Path

import numpy as np
from rasterio.io import MemoryFile as GdalMemoryFile

from terrio.rio.io import MemoryFile

assert sys.platform == "linux" and platform.machine() == "x86_64"
encode = runpy.run_path("python/tests/test_jp2_writer.py")["encode"]
rgb = np.random.default_rng(7).integers(0, 256, (3, 128, 128), dtype="uint8")
values = np.random.default_rng(11).integers(0, 20000, (1, 64, 64))
cases = {
    "uint8_quality10": (rgb, {"QUALITY": 10}),
    "uint8_irreversible": (rgb, {"REVERSIBLE": "NO"}),
    "int16_quality25": ((values - 10000).astype("int16"),
                        {"QUALITY": 25, "REVERSIBLE": "NO"}),
    "uint16_quality25": (values.astype("uint16"),
                         {"QUALITY": 25, "REVERSIBLE": "NO"}),
}
root = Path("python/tests/data/jp2_decode")
arrays = {}
for name, (data, options) in cases.items():
    payload = encode(data, **options)
    with GdalMemoryFile(payload) as mem, mem.open() as oracle:
        assert oracle.driver == "JP2OpenJPEG"
        expected = oracle.read()
    with MemoryFile(payload, ext=".jp2") as mem, mem.open() as actual:
        decoded = actual.read()
    assert decoded.dtype == expected.dtype
    np.testing.assert_array_equal(decoded, expected)
    (root / f"{name}.jp2").write_bytes(payload)
    arrays[name] = expected
np.savez_compressed(root / "decoded.npz", **arrays)
```
