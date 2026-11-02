"""Generate small generic fixtures with the existing h5netcdf/NumPy mechanism.
Run: python js/tests/scientific-fixtures.py. No Terrio writer is used as an oracle.
"""
from pathlib import Path
import h5netcdf
import numpy as np

out = Path(__file__).with_name("scientific-data")
out.mkdir(exist_ok=True)
with h5netcdf.File(out / "packed.nc", "w") as f:
    f.dimensions = {"time": 2, "y": 3, "x": 4}
    f.attrs["title"] = "Synthetic numeric conformance"
    for name, values in {"time": [0, 1.5], "y": [10, 20, 30], "x": [1, 2, 3, 4]}.items():
        v = f.create_variable(name, (name,), dtype="f8")
        v[:] = values
    f.variables["time"].attrs.update(units="hours since 2000-01-01", calendar="standard")
    a = f.create_variable("sample", ("time", "y", "x"), dtype="i2",
                          chunks=(1, 2, 2), compression="gzip", compression_opts=1,
                          fillvalue=np.int16(-32768))
    a.attrs.update(scale_factor=np.float32(0.25), add_offset=np.float32(100), units="K")
    raw = np.arange(24, dtype=np.int16).reshape(2, 3, 4) - 8
    raw[0, 0, 0] = -32768
    a[:] = raw
    for name, dtype, values in [
        ("unsigned", "u2", [0, 65535]),
        ("signed64", "i8", [-9007199254741001, 9007199254741001]),
        ("unsigned64", "u8", [0, 18446744073709551615]),
        ("floating", "f8", [float("nan"), float("inf")]),
    ]:
        v = f.create_variable(name, ("time",), dtype=dtype)
        v[:] = values
print(f"{out / 'packed.nc'}: {(out / 'packed.nc').stat().st_size} bytes")
