"""Rational polynomial coefficients, as `rasterio.rpc`."""

from __future__ import annotations

from dataclasses import asdict, dataclass
from typing import Any

_COEFFS = ("LINE_NUM_COEFF", "LINE_DEN_COEFF", "SAMP_NUM_COEFF", "SAMP_DEN_COEFF")

# The order of the 92 values of the GeoTIFF RPCCoefficientTag.
_TAG_SCALARS = (
    "err_bias",
    "err_rand",
    "line_off",
    "samp_off",
    "lat_off",
    "long_off",
    "height_off",
    "line_scale",
    "samp_scale",
    "lat_scale",
    "long_scale",
    "height_scale",
)
_TAG_LISTS = ("line_num_coeff", "line_den_coeff", "samp_num_coeff", "samp_den_coeff")


@dataclass
class RPC:
    """Rational polynomial coefficients mapping (x, y, z) to (row, col) and back.

    Attributes:
        height_off: The height offset.
        height_scale: The height scale.
        lat_off: The latitude offset.
        lat_scale: The latitude scale.
        line_den_coeff: The 20 coefficients of the line denominator.
        line_num_coeff: The 20 coefficients of the line numerator.
        line_off: The line (row) offset.
        line_scale: The line (row) scale.
        long_off: The longitude offset.
        long_scale: The longitude scale.
        samp_den_coeff: The 20 coefficients of the sample denominator.
        samp_num_coeff: The 20 coefficients of the sample numerator.
        samp_off: The sample (column) offset.
        samp_scale: The sample (column) scale.
        err_bias: The RMS bias error, optional.
        err_rand: The RMS random error, optional.
    """

    height_off: float
    height_scale: float
    lat_off: float
    lat_scale: float
    line_den_coeff: list[float]
    line_num_coeff: list[float]
    line_off: float
    line_scale: float
    long_off: float
    long_scale: float
    samp_den_coeff: list[float]
    samp_num_coeff: list[float]
    samp_off: float
    samp_scale: float
    err_bias: float | None = None
    err_rand: float | None = None

    def to_dict(self) -> dict[str, Any]:
        """The coefficients as a dictionary.

        Returns:
            dict: The attributes.
        """
        return asdict(self)

    def to_gdal(self) -> dict[str, str]:
        """The coefficients as GDAL's `RPC` metadata domain holds them.

        Returns:
            dict: GDAL's item names and values (without the errors, which GDAL does not write).
        """
        out = {
            "HEIGHT_OFF": str(self.height_off),
            "HEIGHT_SCALE": str(self.height_scale),
            "LAT_OFF": str(self.lat_off),
            "LAT_SCALE": str(self.lat_scale),
            "LINE_DEN_COEFF": " ".join(map(str, self.line_den_coeff)),
            "LINE_NUM_COEFF": " ".join(map(str, self.line_num_coeff)),
            "LINE_OFF": str(self.line_off),
            "LINE_SCALE": str(self.line_scale),
            "LONG_OFF": str(self.long_off),
            "LONG_SCALE": str(self.long_scale),
            "SAMP_DEN_COEFF": " ".join(map(str, self.samp_den_coeff)),
            "SAMP_NUM_COEFF": " ".join(map(str, self.samp_num_coeff)),
            "SAMP_OFF": str(self.samp_off),
            "SAMP_SCALE": str(self.samp_scale),
        }
        if self.err_bias is not None:
            out["ERR_BIAS"] = str(self.err_bias)
        if self.err_rand is not None:
            out["ERR_RAND"] = str(self.err_rand)
        return out

    @classmethod
    def from_gdal(cls, rpcs: dict[str, str]) -> RPC:
        """Reads GDAL's `RPC` metadata items.

        Args:
            rpcs: The items, such as `{"LINE_OFF": "1234.0", ...}`.

        Returns:
            RPC: The coefficients.
        """
        out: dict[str, Any] = {}
        for key, val in rpcs.items():
            if key in _COEFFS:
                out[key] = [float(v) for v in val.split(maxsplit=20)[:20]]
            else:
                out[key] = float(val.split(maxsplit=1)[0])
        return cls(
            err_bias=out.get("ERR_BIAS"),
            err_rand=out.get("ERR_RAND"),
            height_off=out["HEIGHT_OFF"],
            height_scale=out["HEIGHT_SCALE"],
            lat_off=out["LAT_OFF"],
            lat_scale=out["LAT_SCALE"],
            line_den_coeff=out["LINE_DEN_COEFF"],
            line_num_coeff=out["LINE_NUM_COEFF"],
            line_off=out["LINE_OFF"],
            line_scale=out["LINE_SCALE"],
            long_off=out["LONG_OFF"],
            long_scale=out["LONG_SCALE"],
            samp_den_coeff=out["SAMP_DEN_COEFF"],
            samp_num_coeff=out["SAMP_NUM_COEFF"],
            samp_off=out["SAMP_OFF"],
            samp_scale=out["SAMP_SCALE"],
        )

    @classmethod
    def from_tiff_tag(cls, values: list[float]) -> RPC:
        """Reads the 92 values of a GeoTIFF `RPCCoefficientTag`, as GDAL reads them.

        GDAL turns them into its RPC metadata items, numbers rounded to 15 significant digits.

        Args:
            values: The tag's values.

        Returns:
            RPC: The coefficients.

        Raises:
            ValueError: If there are not 92 values.
        """
        if len(values) != 92:
            raise ValueError(f"an RPCCoefficientTag has 92 values, not {len(values)}")
        # GDAL turns the tag into its RPC metadata items, numbers printed with 15 digits.
        items = {
            k.upper(): f"{float(v):.15g}" for k, v in zip(_TAG_SCALARS, values[:12], strict=True)
        }
        for i, k in enumerate(_TAG_LISTS):
            vals = values[12 + 20 * i : 32 + 20 * i]
            items[k.upper()] = " ".join(f"{float(v):.15g}" for v in vals)
        return cls.from_gdal(items)

    def to_tiff_tag(self) -> list[float]:
        """The 92 values of a GeoTIFF `RPCCoefficientTag`.

        Returns:
            list: The values (missing errors as -1, as GDAL writes them).
        """
        vals = [-1.0 if getattr(self, k) is None else float(getattr(self, k)) for k in _TAG_SCALARS]
        for k in _TAG_LISTS:
            coeffs = [float(v) for v in getattr(self, k)]
            if len(coeffs) != 20:
                raise ValueError(f"{k} has 20 coefficients, not {len(coeffs)}")
            vals.extend(coeffs)
        return vals


def read_rpc_file(path: str) -> dict[str, str] | None:
    """Reads the RPCs of a `_RPC.TXT` or `.RPB` sidecar.

    As GDAL's `GDALLoadRPCFile` and `GDALLoadRPBFile` do.

    Args:
        path: The sidecar's path.

    Returns:
        dict | None: GDAL's `RPC` items, or None when the file holds none.
    """
    try:
        with open(path, encoding="latin-1") as f:
            text = f.read()
    except OSError:
        return None
    out: dict[str, str] = {}
    if path.lower().endswith(".rpb"):
        names = {
            "errBias": "ERR_BIAS",
            "errRand": "ERR_RAND",
            "lineOffset": "LINE_OFF",
            "sampOffset": "SAMP_OFF",
            "latOffset": "LAT_OFF",
            "longOffset": "LONG_OFF",
            "heightOffset": "HEIGHT_OFF",
            "lineScale": "LINE_SCALE",
            "sampScale": "SAMP_SCALE",
            "latScale": "LAT_SCALE",
            "longScale": "LONG_SCALE",
            "heightScale": "HEIGHT_SCALE",
            "lineNumCoef": "LINE_NUM_COEFF",
            "lineDenCoef": "LINE_DEN_COEFF",
            "sampNumCoef": "SAMP_NUM_COEFF",
            "sampDenCoef": "SAMP_DEN_COEFF",
        }
        flat = " ".join(text.split())
        for src, dst in names.items():
            i = flat.find(src + " =")
            if i < 0:
                continue
            rest = flat[i + len(src) + 2 :].lstrip()
            value = rest.split(";", 1)[0].strip().strip("()")
            out[dst] = " ".join(v.strip() for v in value.split(",") if v.strip())
    else:
        # Coefficients are placed by their number (LINE_NUM_COEFF_1 to _20), as GDAL reads them,
        # whatever order the file lists them in.
        coeffs: dict[str, dict[int, str]] = {k: {} for k in _COEFFS}
        for line in text.splitlines():
            if ":" not in line:
                continue
            key, value = (p.strip() for p in line.split(":", 1))
            value = value.split()[0] if value.split() else ""
            for c in _COEFFS:
                suffix = key[len(c) + 1 :] if key.startswith(c + "_") else ""
                if suffix.isdigit():
                    coeffs[c][int(suffix)] = value
                    break
            else:
                out[key] = value
        for c, by_index in coeffs.items():
            if sorted(by_index) == list(range(1, 21)):
                out[c] = " ".join(by_index[i] for i in range(1, 21))
    needed = {"LINE_OFF", "SAMP_OFF", "LAT_OFF", "LONG_OFF", "HEIGHT_OFF", *_COEFFS}
    return out if needed <= out.keys() else None


__all__ = ["RPC"]
