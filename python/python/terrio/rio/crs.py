"""Coordinate reference systems, with rasterio's `CRS` class.

CRSs are parsed and identified by terrio (EPSG codes, WKT, PROJ strings); WKT exported for an EPSG
code is the WKT1 that GDAL writes.
"""

from __future__ import annotations

import math
import re
from collections.abc import Mapping
from typing import Any

from terrio import _terrio
from terrio.rio.enums import WktVersion
from terrio.rio.errors import CRSError

_ANGULAR = {"degree": math.pi / 180.0}
_EXACT_UNITS = {"ussurveyfoot": 0.30480060960121924, "footus": 0.30480060960121924, "foot": 0.3048}


def _parse_wkt(wkt: str) -> list[Any] | None:
    """Parses WKT into nested lists: `[keyword, value, value, [keyword, ...], ...]`.

    Args:
        wkt: A WKT string.

    Returns:
        list | None: The root node, or None when the text is not WKT.
    """
    tokens = re.findall(r'"(?:[^"]|"")*"|[A-Za-z_][A-Za-z0-9_]*|[-+0-9.eE]+|[\[\](),]', wkt)
    stack: list[list[Any]] = []
    root: list[Any] | None = None
    keyword: str | None = None
    for tok in tokens:
        if tok in "[(":
            node: list[Any] = [keyword or ""]
            if stack:
                stack[-1].append(node)
            stack.append(node)
            keyword = None
        elif tok in "])":
            if keyword is not None and stack:
                stack[-1].append(keyword)
                keyword = None
            if not stack:
                return None
            done = stack.pop()
            if not stack:
                root = done
        elif tok == ",":
            if keyword is not None and stack:
                stack[-1].append(keyword)
                keyword = None
        elif tok.startswith('"'):
            if stack:
                stack[-1].append(tok[1:-1].replace('""', '"'))
        elif re.match(r"[-+0-9.]", tok):
            if stack:
                try:
                    stack[-1].append(float(tok))
                except ValueError:
                    stack[-1].append(tok)
        else:
            keyword = tok
    return root


def _child(node: list[Any], *names: str) -> list[Any] | None:
    """The first child node with one of the keywords.

    Args:
        node: A parsed WKT node.
        *names: Keywords, upper case.

    Returns:
        list | None: The child, or None.
    """
    for c in node[1:]:
        if isinstance(c, list) and c and str(c[0]).upper() in names:
            return c
    return None


class CRS(Mapping[str, Any]):
    """A coordinate reference system.

    Create one with `CRS.from_epsg()`, `CRS.from_string()`, `CRS.from_wkt()` or
    `CRS.from_user_input()`. A CRS compares equal to another describing the same system, and to
    strings and EPSG codes defining it.
    """

    def __init__(self, initialdata: Any = None, **kwargs: Any) -> None:
        """Creates a CRS from PROJ parameters (deprecated rasterio style) or another CRS.

        Args:
            initialdata: A CRS, a mapping of PROJ parameters, or None.
            **kwargs: PROJ parameters.

        Raises:
            CRSError: If the parameters do not define a CRS.
        """
        self._info: dict[str, Any] = {}
        self._input = ""
        if isinstance(initialdata, CRS):
            self._info = dict(initialdata._info)
            self._input = initialdata._input
            return
        params = dict(initialdata or {})
        params.update(kwargs)
        if params:
            other = CRS.from_dict(params)
            self._info, self._input = other._info, other._input

    @classmethod
    def _from(cls, definition: str) -> CRS:
        """A CRS from a definition terrio parses.

        Args:
            definition: An `EPSG:n` code, WKT or PROJ string.

        Returns:
            CRS: The CRS.

        Raises:
            CRSError: If terrio cannot parse it.
        """
        try:
            info = _terrio.crs_info(definition)
        except Exception as err:
            raise CRSError(f"The definition '{definition}' is not recognized: {err}") from err
        crs = cls()
        crs._info = dict(info)
        crs._input = definition
        return crs

    @classmethod
    def from_epsg(cls, code: int | str) -> CRS:
        """A CRS from an EPSG code.

        Args:
            code: The code, such as 4326.

        Returns:
            CRS: The CRS.

        Raises:
            CRSError: If the code is not valid.
        """
        try:
            n = int(code)
        except (TypeError, ValueError) as err:
            raise CRSError(f"Invalid EPSG code: {code!r}") from err
        if n <= 0:
            raise CRSError("EPSG codes are positive integers")
        return cls._from(f"EPSG:{n}")

    @classmethod
    def from_authority(cls, auth_name: str, code: int | str) -> CRS:
        """A CRS from an authority and code.

        Args:
            auth_name: The authority, such as `EPSG`.
            code: The code.

        Returns:
            CRS: The CRS.
        """
        return cls._from(f"{auth_name}:{code}")

    @classmethod
    def from_wkt(cls, wkt: str, morph_from_esri_dialect: bool = False) -> CRS:
        """A CRS from WKT.

        Args:
            wkt: WKT1 or WKT2.
            morph_from_esri_dialect: Unused.

        Returns:
            CRS: The CRS.

        Raises:
            CRSError: If the text is not valid WKT.
        """
        if not isinstance(wkt, str):
            raise ValueError("A string is expected")
        if _parse_wkt(wkt) is None:
            raise CRSError(f"Invalid WKT: {wkt[:80]}")
        return cls._from(wkt)

    @classmethod
    def from_proj4(cls, proj: str) -> CRS:
        """A CRS from a PROJ string.

        Args:
            proj: A PROJ string such as `+proj=utm +zone=33 +datum=WGS84`.

        Returns:
            CRS: The CRS.
        """
        m = re.search(r"\+init=epsg:(\d+)", proj, re.IGNORECASE)
        if m:
            return cls.from_epsg(m.group(1))
        return cls._from(proj)

    @classmethod
    def from_dict(cls, initialdata: Mapping[str, Any] | None = None, **kwargs: Any) -> CRS:
        """A CRS from PROJ parameters.

        Args:
            initialdata: A mapping such as `{"init": "epsg:4326"}` or `{"proj": "utm", ...}`.
            **kwargs: More parameters.

        Returns:
            CRS: The CRS.

        Raises:
            CRSError: If the parameters do not define a CRS.
        """
        params = dict(initialdata or {})
        params.update(kwargs)
        if not params:
            raise CRSError("CRS is empty or invalid")
        if "init" in params:
            return cls.from_user_input(str(params["init"]))
        parts = []
        for k, v in params.items():
            if v is True:
                parts.append(f"+{k}")
            elif v is False:
                continue
            else:
                parts.append(f"+{k}={v}")
        return cls.from_proj4(" ".join(parts))

    @classmethod
    def from_string(cls, value: str, morph_from_esri_dialect: bool = False) -> CRS:
        """A CRS from a string: an authority code, WKT or a PROJ string.

        Args:
            value: The string.
            morph_from_esri_dialect: Unused.

        Returns:
            CRS: The CRS.

        Raises:
            CRSError: If the string is empty or not valid.
        """
        if not value:
            raise CRSError(f"CRS is empty or invalid: {value!r}")
        value = value.strip()
        m = re.match(r"^(?:\+init=)?epsg:(\d+)$", value, re.IGNORECASE)
        if m:
            return cls.from_epsg(m.group(1))
        if value.startswith("{") or value.startswith("["):
            raise CRSError("PROJJSON is not supported")
        if value.lstrip().startswith("+") or "+proj=" in value:
            return cls.from_proj4(value)
        if re.match(r"^[A-Z]+\[", value.lstrip(), re.IGNORECASE):
            return cls.from_wkt(value)
        return cls._from(value)

    @classmethod
    def from_user_input(cls, value: Any, morph_from_esri_dialect: bool = False) -> CRS:
        """A CRS from any supported input.

        Args:
            value: A `CRS`, an EPSG code (int), a string, a mapping of PROJ parameters, or an
                object with a `to_wkt()` method (such as a pyproj CRS).
            morph_from_esri_dialect: Unused.

        Returns:
            CRS: The CRS.

        Raises:
            CRSError: If the input is not a CRS.
        """
        if isinstance(value, CRS):
            return value
        if isinstance(value, int) and not isinstance(value, bool):
            return cls.from_epsg(value)
        if isinstance(value, Mapping):
            return cls.from_dict(value)
        if isinstance(value, str):
            return cls.from_string(value)
        if hasattr(value, "to_wkt"):
            return cls.from_wkt(value.to_wkt())
        raise CRSError(f"CRS is invalid: {value!r}")

    # Properties.

    @property
    def wkt(self) -> str:
        """The WKT1 definition.

        Returns:
            str: The WKT, or an empty string for an empty CRS.
        """
        return str(self._info.get("wkt") or "")

    @property
    def _root(self) -> list[Any] | None:
        """The parsed WKT.

        Returns:
            list | None: The root node.
        """
        return _parse_wkt(self.wkt) if self.wkt else None

    @property
    def is_valid(self) -> bool:
        """Whether the CRS is defined.

        Returns:
            bool: True when not empty.
        """
        return bool(self._info)

    @property
    def is_geographic(self) -> bool:
        """Whether the CRS is geographic (longitude and latitude).

        Returns:
            bool: True for geographic CRSs.
        """
        return bool(self._info.get("is_geographic"))

    @property
    def is_projected(self) -> bool:
        """Whether the CRS is projected.

        Returns:
            bool: True for projected CRSs.
        """
        root = self._root
        if root is not None:
            return str(root[0]).upper() in ("PROJCS", "PROJCRS", "PROJECTEDCRS")
        return self.is_valid and not self.is_geographic

    @property
    def is_epsg_code(self) -> bool:
        """Whether the CRS is known by an EPSG code.

        Returns:
            bool: True when it has one (not for an ESRI code such as `ESRI:54009`).
        """
        return self.to_epsg() is not None

    def _unit(self) -> tuple[str, float] | None:
        """The unit node directly under the root.

        Returns:
            tuple | None: `(name, factor)`.
        """
        root = self._root
        if root is None:
            return None
        node = root
        if str(root[0]).upper() in ("COMPD_CS", "COMPOUNDCRS"):
            first = next((c for c in root[1:] if isinstance(c, list)), None)
            if first is None:
                return None
            node = first
        unit = _child(node, "UNIT", "LENGTHUNIT", "ANGLEUNIT")
        if unit is None:
            cs = _child(node, "CS")
            unit = _child(cs, "UNIT") if cs else None
        if unit is None:
            if str(node[0]).upper() in ("GEOGCS", "GEOGCRS", "GEODCRS"):
                return ("degree", _ANGULAR["degree"])
            return None
        name = str(unit[1])
        factor = float(unit[2]) if len(unit) > 2 and isinstance(unit[2], float) else 1.0
        if name.lower() in _ANGULAR:
            factor = _ANGULAR[name.lower()]
        # Exact sizes, as PROJ knows them (WKT rounds them).
        exact = _EXACT_UNITS.get(_norm(name))
        if exact is not None and abs(exact - factor) < 1e-12:
            factor = exact
        return name, factor

    @property
    def linear_units(self) -> str:
        """The name of the linear unit, or `unknown` for a geographic CRS.

        Returns:
            str: The unit name, such as `metre`.
        """
        if not self.is_projected:
            return "unknown"
        u = self._unit()
        return u[0] if u else "unknown"

    @property
    def linear_units_factor(self) -> tuple[str, float]:
        """The linear unit and its size in metres.

        Returns:
            tuple: `(name, metres)`.

        Raises:
            CRSError: If the CRS is not projected.
        """
        if not self.is_projected:
            raise CRSError("Linear units factor is not defined for non projected CRS")
        u = self._unit()
        if u is None:
            raise CRSError("No linear units found")
        return u

    @property
    def units_factor(self) -> tuple[str, float]:
        """The unit of the CRS's axes and its size (metres or radians).

        Returns:
            tuple: `(name, factor)`.

        Raises:
            CRSError: If the unit is unknown.
        """
        u = self._unit()
        if u is None:
            raise CRSError("Units factor is not defined")
        return u

    @property
    def data(self) -> dict[str, Any]:
        """The PROJ parameters of the CRS.

        Returns:
            dict: The parameters.
        """
        return self.to_dict()

    # Conversions.

    def to_epsg(self, confidence_threshold: int = 70) -> int | None:
        """The EPSG code of the CRS, when it has one.

        Args:
            confidence_threshold: Unused: terrio identifies codes by their definition.

        Returns:
            int | None: The code.
        """
        code = self._info.get("epsg")
        if code is None and self._info.get("proj4") and self.wkt:
            try:
                code = _terrio.crs_info(self.wkt).get("epsg")
            except Exception:
                code = None
        return int(code) if code is not None else None

    def to_authority(self, confidence_threshold: int = 70) -> tuple[str, str] | None:
        """The authority and code of the CRS.

        Args:
            confidence_threshold: Unused.

        Returns:
            tuple | None: `("EPSG", "4326")`, `("ESRI", "54009")`, or None.
        """
        code = self.to_epsg()
        if code is not None:
            return ("EPSG", str(code))
        esri = self._info.get("esri")
        if esri is not None:
            return ("ESRI", str(esri))
        m = re.match(r"^\s*([A-Za-z]+):(\d+)\s*$", self._input)
        if m:
            return (m.group(1).upper(), m.group(2))
        return None

    def to_wkt(self, morph_to_esri_dialect: bool = False, version: Any = None) -> str:
        """The WKT definition.

        Args:
            morph_to_esri_dialect: Unused.
            version: Only WKT1 (`WKT1_GDAL`, the default) is supported.

        Returns:
            str: The WKT.

        Raises:
            CRSError: If another WKT version is asked for.
        """
        if version is not None:
            v = WktVersion(version) if not isinstance(version, WktVersion) else version
            if v not in (WktVersion.WKT1_GDAL, WktVersion.WKT1):
                raise CRSError(f"terrio.rio writes WKT1 only, not {v.value}")
        return self.wkt

    def to_string(self) -> str:
        """The CRS as a string: `EPSG:n` (or `ESRI:n`) when it has a code, else its WKT.

        Returns:
            str: The string.
        """
        auth = self.to_authority()
        if auth:
            return ":".join(auth)
        return self.to_wkt() or self.to_proj4()

    def to_proj4(self) -> str:
        """The CRS as a PROJ string.

        Returns:
            str: The PROJ string.
        """
        return " ".join(f"+{k}={v}" for k, v in self.to_dict().items())

    def to_dict(self, projjson: bool = False) -> dict[str, Any]:
        """The CRS as PROJ parameters.

        Args:
            projjson: Unused: PROJJSON is not supported.

        Returns:
            dict: The parameters.
        """
        proj4 = self._info.get("proj4")
        if proj4:
            return _proj_dict(proj4)
        root = self._root
        return _wkt_to_proj(root) if root is not None else {}

    # Comparison.

    def __eq__(self, other: object) -> bool:
        """Whether two CRSs are the same.

        Args:
            other: A `CRS` or anything `from_user_input` accepts.

        Returns:
            bool: True when they describe the same system.
        """
        try:
            o = CRS.from_user_input(other)
        except Exception:
            return False
        if not self.is_valid or not o.is_valid:
            return self.is_valid == o.is_valid
        a, b = self.to_epsg(), o.to_epsg()
        if a is not None and b is not None:
            return a == b
        a, b = self._info.get("esri"), o._info.get("esri")
        if a is not None and b is not None:
            return a == b
        if self.wkt and self.wkt == o.wkt:
            return True
        da, db = self.to_dict(), o.to_dict()
        return bool(da) and da == db

    def __hash__(self) -> int:
        """The hash of the WKT.

        Returns:
            int: The hash.
        """
        return hash(self.to_wkt())

    def __bool__(self) -> bool:
        """Whether the CRS is defined.

        Returns:
            bool: True when not empty.
        """
        return self.is_valid

    def __str__(self) -> str:
        """The CRS as a string.

        Returns:
            str: See `to_string`.
        """
        return self.to_string()

    def __repr__(self) -> str:
        """The representation.

        Returns:
            str: `CRS.from_epsg(n)` or `CRS.from_wkt(...)`.
        """
        code = self.to_epsg()
        if code is not None:
            return f"CRS.from_epsg({code})"
        return f"CRS.from_wkt('{self.wkt}')"

    def __getstate__(self) -> dict[str, Any]:
        """The state to pickle.

        Returns:
            dict: The state.
        """
        return {"info": self._info, "input": self._input}

    def __setstate__(self, state: dict[str, Any]) -> None:
        """Restores a pickled state.

        Args:
            state: The state.
        """
        self._info = state["info"]
        self._input = state["input"]

    def __copy__(self) -> CRS:
        """A copy.

        Returns:
            CRS: The copy.
        """
        return CRS(self)

    # Mapping interface over the PROJ parameters, as rasterio's CRS.

    def __getitem__(self, item: str) -> Any:
        """A PROJ parameter.

        Args:
            item: The parameter name.

        Returns:
            The value.
        """
        return self.data[item]

    def __iter__(self) -> Any:
        """Iterates over PROJ parameter names.

        Returns:
            iterator: The names.
        """
        return iter(self.data)

    def __len__(self) -> int:
        """The number of PROJ parameters.

        Returns:
            int: The count.
        """
        return len(self.data)


def _proj_dict(proj4: str) -> dict[str, Any]:
    """PROJ parameters from a PROJ string.

    Args:
        proj4: The PROJ string.

    Returns:
        dict: The parameters, numbers converted.
    """
    out: dict[str, Any] = {}
    for part in proj4.split():
        part = part.lstrip("+")
        if not part:
            continue
        if "=" in part:
            k, v = part.split("=", 1)
            try:
                num = float(v)
                out[k] = int(num) if num.is_integer() and "." not in v else num
            except ValueError:
                out[k] = v
        else:
            out[part] = True
    return out


# WKT1 projection names and parameters (OGC and ESRI spellings, normalized to lower case
# without punctuation), and their PROJ equivalents.
_PROJECTIONS = {
    "transversemercator": "tmerc",
    "mercator1sp": "merc",
    "mercator2sp": "merc",
    "mercator": "merc",
    "lambertconformalconic1sp": "lcc",
    "lambertconformalconic2sp": "lcc",
    "lambertconformalconic": "lcc",
    "albersconicequalarea": "aea",
    "albers": "aea",
    "lambertazimuthalequalarea": "laea",
    "polarstereographic": "stere",
    "obliquestereographic": "sterea",
    "doublestereographic": "sterea",
    "sinusoidal": "sinu",
    "equirectangular": "eqc",
    "platecarree": "eqc",
    "mollweide": "moll",
    "azimuthalequidistant": "aeqd",
    "orthographic": "ortho",
}
_PARAMS = {
    "latitudeoforigin": "lat_0",
    "latitudeofcenter": "lat_0",
    "centralmeridian": "lon_0",
    "longitudeofcenter": "lon_0",
    "standardparallel1": "lat_1",
    "standardparallel2": "lat_2",
    "scalefactor": "k",
    "falseeasting": "x_0",
    "falsenorthing": "y_0",
}
_DATUMS = {
    "wgs1984": "WGS84",
    "wgs84": "WGS84",
    "northamericandatum1983": "NAD83",
    "nad83": "NAD83",
    "northamericandatum1927": "NAD27",
    "nad27": "NAD27",
}
_UNITS = {"metre": "m", "meter": "m", "ussurveyfoot": "us-ft", "footus": "us-ft", "foot": "ft"}


def _norm(name: Any) -> str:
    """A WKT name normalized for lookups: lower case, without punctuation or an ESRI `D_`.

    Args:
        name: The name.

    Returns:
        str: The normalized name.
    """
    s = re.sub(r"[^a-z0-9]", "", str(name).lower())
    return s[1:] if str(name).lower().startswith("d_") else s


def _num(v: float) -> float | int:
    """A number, as an int when integral.

    Args:
        v: The number.

    Returns:
        float | int: The number.
    """
    return int(v) if float(v).is_integer() else v


def _wkt_to_proj(root: list[Any]) -> dict[str, Any]:
    """PROJ parameters of a parsed WKT1 CRS, for common projections.

    Args:
        root: The parsed WKT.

    Returns:
        dict: The parameters (empty when the projection is not handled).
    """
    kind = str(root[0]).upper()
    geog = root if kind == "GEOGCS" else _child(root, "GEOGCS")
    out: dict[str, Any] = {}
    if kind == "PROJCS":
        proj = _child(root, "PROJECTION")
        name = _norm(proj[1]) if proj else ""
        if name not in _PROJECTIONS:
            return {}
        out["proj"] = _PROJECTIONS[name]
        for c in root[1:]:
            if isinstance(c, list) and str(c[0]).upper() == "PARAMETER" and len(c) > 2:
                key = _PARAMS.get(_norm(c[1]))
                if key:
                    out[key] = _num(c[2])
        if name == "lambertconformalconic1sp":
            out["lat_1"] = out.get("lat_0", 0)
        if (
            out["proj"] == "tmerc"
            and out.get("k") == 0.9996
            and out.get("lat_0", 0) == 0
            and out.get("x_0") == 500000
        ):
            zone = (float(out.get("lon_0", 0)) + 183) / 6
            if zone == int(zone) and 1 <= zone <= 60 and out.get("y_0", 0) in (0, 10000000):
                south = out.get("y_0", 0) == 10000000
                out = {"proj": "utm", "zone": int(zone)}
                if south:
                    out["south"] = True
    elif kind == "GEOGCS":
        out["proj"] = "longlat"
    else:
        return {}
    datum = _child(geog, "DATUM") if geog else None
    if datum is not None:
        dname = _norm(datum[1])
        if dname in _DATUMS:
            out["datum"] = _DATUMS[dname]
        else:
            sph = _child(datum, "SPHEROID")
            if sph is not None and len(sph) > 3:
                out["a"] = _num(sph[2])
                rf = float(sph[3])
                if rf:
                    out["rf"] = _num(rf)
                else:
                    out["b"] = _num(sph[2])
    if kind == "PROJCS":
        unit = _child(root, "UNIT")
        if unit is not None:
            uname = _norm(unit[1])
            if uname in _UNITS:
                out["units"] = _UNITS[uname]
            else:
                out["to_meter"] = _num(unit[2])
    out["no_defs"] = True
    return out


__all__ = ["CRS"]
