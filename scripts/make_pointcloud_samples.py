"""Writes LAS and LAZ files of every point format (0 to 10) with laspy, for terrio's tests.

Each file has 3000 points with random values in every dimension, extra bytes of every scalar
type (one of them scaled, one a 3-element array), a WKT CRS, and, for LAS 1.4, an EVLR. The
files are written by laspy (LAZ with its lazrs backend), independently of terrio.

Usage:
    python scripts/make_pointcloud_samples.py OUTPUT_DIR
"""

import sys
from pathlib import Path

import laspy
import numpy as np

UTM15_WKT = (
    'PROJCS["NAD83 / UTM zone 15N",GEOGCS["NAD83",DATUM["North_American_Datum_1983",'
    'SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],'
    'AUTHORITY["EPSG","6269"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],'
    'UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4269"]],'
    'PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],'
    'PARAMETER["central_meridian",-93],PARAMETER["scale_factor",0.9996],'
    'PARAMETER["false_easting",500000],PARAMETER["false_northing",0],'
    'UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],'
    'AUTHORITY["EPSG","26915"]]'
)

EXTRA = [
    ("eb_u8", np.uint8),
    ("eb_i8", np.int8),
    ("eb_u16", np.uint16),
    ("eb_i16", np.int16),
    ("eb_u32", np.uint32),
    ("eb_i32", np.int32),
    ("eb_u64", np.uint64),
    ("eb_i64", np.int64),
    ("eb_f32", np.float32),
    ("eb_f64", np.float64),
]


def version_for(point_format: int) -> str:
    """The lowest LAS version of a point format.

    Args:
        point_format: The point data record format, 0 to 10.

    Returns:
        str: The version, such as "1.2".
    """
    if point_format <= 3:
        return "1.2"
    if point_format <= 5:
        return "1.3"
    return "1.4"


def random_values(rng: np.random.Generator, dtype: np.dtype, n: int) -> np.ndarray:
    """Random values covering a type's range.

    Args:
        rng: The random generator.
        dtype: The NumPy type.
        n: The number of values.

    Returns:
        numpy.ndarray: The values.
    """
    dtype = np.dtype(dtype)
    if dtype.kind == "f":
        return rng.normal(0, 1000, n).astype(dtype)
    info = np.iinfo(dtype)
    return rng.integers(info.min, info.max, n, dtype=dtype, endpoint=True)


def make(point_format: int, n: int = 3000, seed: int = 0) -> laspy.LasData:
    """A point cloud of a point format with random values.

    Args:
        point_format: The point data record format, 0 to 10.
        n: The number of points.
        seed: The random seed.

    Returns:
        laspy.LasData: The points.
    """
    rng = np.random.default_rng(seed + point_format)
    header = laspy.LasHeader(point_format=point_format, version=version_for(point_format))
    header.scales = np.array([0.001, 0.001, 0.01])
    header.offsets = np.array([450000.0, 4980000.0, 0.0])
    header.system_identifier = "terrio test"
    header.generating_software = "laspy"
    header.add_extra_dims([laspy.ExtraBytesParams(name, t) for name, t in EXTRA])
    header.add_extra_dim(laspy.ExtraBytesParams("eb_scaled", np.int32, scales=[0.01], offsets=[10.0]))
    header.add_extra_dim(laspy.ExtraBytesParams("eb_array", "3f8"))
    header.vlrs.append(laspy.vlrs.known.WktCoordinateSystemVlr(UTM15_WKT))
    if header.version.minor >= 4:
        header.global_encoding.wkt = True
    las = laspy.LasData(header)
    las.x = rng.uniform(450000, 451000, n)
    las.y = rng.uniform(4980000, 4981000, n)
    las.z = rng.uniform(-20, 400, n)
    las.intensity = random_values(rng, np.uint16, n)
    max_returns = 15 if point_format >= 6 else 7
    las.number_of_returns = rng.integers(1, max_returns, n, endpoint=True)
    las.return_number = np.minimum(rng.integers(1, max_returns, n, endpoint=True), las.number_of_returns)
    las.scan_direction_flag = rng.integers(0, 1, n, endpoint=True)
    las.edge_of_flight_line = rng.integers(0, 1, n, endpoint=True)
    las.classification = rng.integers(0, 255 if point_format >= 6 else 31, n, endpoint=True)
    las.synthetic = rng.integers(0, 1, n, endpoint=True)
    las.key_point = rng.integers(0, 1, n, endpoint=True)
    las.withheld = rng.integers(0, 1, n, endpoint=True)
    las.user_data = random_values(rng, np.uint8, n)
    las.point_source_id = random_values(rng, np.uint16, n)
    if point_format >= 6:
        las.overlap = rng.integers(0, 1, n, endpoint=True)
        las.scanner_channel = rng.integers(0, 3, n, endpoint=True)
        las.scan_angle = rng.integers(-30000, 30000, n, endpoint=True)
    else:
        las.scan_angle_rank = rng.integers(-90, 90, n, endpoint=True)
    names = set(las.point_format.dimension_names)
    if "gps_time" in names:
        las.gps_time = rng.uniform(1e8, 3e8, n)
    if "red" in names:
        las.red = random_values(rng, np.uint16, n)
        las.green = random_values(rng, np.uint16, n)
        las.blue = random_values(rng, np.uint16, n)
    if "nir" in names:
        las.nir = random_values(rng, np.uint16, n)
    if "wavepacket_index" in names:
        las.wavepacket_index = random_values(rng, np.uint8, n)
        las.wavepacket_offset = rng.integers(0, 2**40, n, dtype=np.uint64)
        las.wavepacket_size = random_values(rng, np.uint32, n)
        las.return_point_wave_location = rng.normal(0, 100, n).astype(np.float32)
        las.x_t = rng.normal(0, 1, n).astype(np.float32)
        las.y_t = rng.normal(0, 1, n).astype(np.float32)
        las.z_t = rng.normal(0, 1, n).astype(np.float32)
    for name, t in EXTRA:
        las[name] = random_values(rng, t, n)
    las["eb_scaled"] = np.round(rng.uniform(-1000, 1000, n), 2)
    las["eb_array"] = rng.normal(0, 10, (n, 3))
    if header.version.minor >= 4:
        las.evlrs = laspy.vlrs.vlrlist.VLRList([laspy.VLR("terrio", 42, "a test EVLR", b"extended variable length record")])
    return las


def main() -> None:
    """Writes the samples into the directory given on the command line."""
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    for point_format in range(11):
        las = make(point_format)
        las.write(out / f"format{point_format}.las")
        las.write(out / f"format{point_format}.laz", laz_backend=laspy.LazBackend.Lazrs)


if __name__ == "__main__":
    main()
