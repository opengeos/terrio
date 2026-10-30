"""Typed usage of the terrio API, checked with `mypy --strict` against the stubs.

It also runs: `python tests/typing/usage.py` exercises every call on small synthetic data.
"""

from __future__ import annotations

import tempfile
from pathlib import Path
from typing import Any

import numpy as np
from numpy.typing import NDArray

import terrio

TRANSFORM = (500000.0, 30.0, 0.0, 4100000.0, 0.0, -30.0)

POLYGONS: dict[str, Any] = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"name": "west", "value": 3},
            "geometry": {
                "type": "Polygon",
                "coordinates": [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]]],
            },
        },
        {
            "type": "Feature",
            "properties": {"name": "east", "value": 7},
            "geometry": {
                "type": "Polygon",
                "coordinates": [[[20, 0], [30, 0], [30, 10], [20, 10], [20, 0]]],
            },
        },
    ],
}


def rasters(tmp: Path) -> terrio.Raster:
    """Exercises `Raster` and the raster operations.

    Args:
        tmp: A directory for the outputs.

    Returns:
        The raster made from an array.
    """
    data: NDArray[np.int16] = np.arange(2 * 6 * 8, dtype=np.int16).reshape(2, 6, 8)
    r = terrio.Raster.from_array(data, TRANSFORM, crs="EPSG:32610", nodata=-1)
    width: int = r.width
    height: int = r.height
    count: int = r.count
    bands, rows, cols = r.shape
    assert (count, height, width) == (bands, rows, cols)
    dtype: str = r.dtype
    nodata: float | None = r.nodata
    crs: str | None = r.crs
    wkt: str | None = r.crs_wkt
    x0, xres, _, y0, _, yres = r.transform
    res: tuple[float, float] = r.res
    min_x, min_y, max_x, max_y = r.bounds
    driver: str = r.driver
    overviews: list[tuple[int, int]] = r.overviews
    lazy: bool = r.is_lazy
    print(dtype, nodata, crs, wkt is not None, x0, xres, y0, yres, res, driver, overviews, lazy)
    print(min_x, min_y, max_x, max_y)

    band1 = r.read(1)
    window = r.read([1, 2], window=(0, 0, 4, 3))
    everything = np.asarray(r)
    as_float = r.__array__(np.float32)
    assert band1.shape == (6, 8) and window.shape == (2, 3, 4)
    assert everything.shape == (2, 6, 8) and as_float.dtype == np.float32

    path = tmp / "r.tif"
    r.write(str(path), compress="DEFLATE", tile_size=256)
    terrio.write_raster(r, str(tmp / "r_cog.tif"), driver="COG")
    blob: bytes = r.to_bytes("r.tif")
    opened = terrio.open_bytes(blob, "r.tif")
    assert isinstance(opened, terrio.Raster)
    lazy_r = terrio.open_raster(str(path), lazy=True)
    assert lazy_r.read(overview=None).shape == (2, 6, 8)

    small = terrio.translate(r, outsize=("50%", 0), resampling="average", a_nodata="none")
    scaled = terrio.translate(str(path), bands=[1], ot="Float32", scale=(0, 100, 0, 1))
    warped = terrio.warp(
        r, t_srs="EPSG:4326", resampling="bilinear", srcnodata=-1, kernel_version="3.13"
    )
    merged = terrio.mosaic([r, str(path)], resolution="highest")
    assert merged is not None
    shade = terrio.dem(r, "hillshade", z=2.0, multidirectional=True)
    slope = terrio.dem(str(path), "slope", percent=True, alg="Horn")
    ratio = terrio.calc("(B - A) / (B + A * 1.0)", A=r, B=str(path), A_band=1, B_band=2)
    dist = terrio.proximity(r, values=[1, 2], distunits="GEO", ot="Float32")
    clean = terrio.sieve(r, threshold=2, connectedness=8, mask="none")
    for out in (small, scaled, warped, merged, shade, slope, ratio, dist, clean):
        print(repr(out))

    info: dict[str, Any] = terrio.info(str(path), stats=True)
    drivers: list[dict[str, Any]] = terrio.drivers()
    print(info["driver"], len(drivers))
    xs, ys = terrio.transform_points(
        [500000.0],
        [4100000.0],
        "EPSG:32610",
        "EPSG:4326",
        allow_ballpark=False,
        only_best=True,
        promote_to_3d=False,
    )
    lon: float = float(xs[0])
    lat: float = float(ys[0])
    print(lon, lat)
    _, _, heights = terrio.transform_points(
        [-100.0], [40.0], "EPSG:4979", "EPSG:4326+5773", [100.0], s_coord_epoch=None
    )
    egm96: float = float(heights[0])
    print(egm96)
    terrio.set_grid_search_paths(["grids"])
    terrio.set_grid_search_paths(None)
    terrio.set_network_enabled(None)
    return r


def vectors(tmp: Path, raster: terrio.Raster) -> None:
    """Exercises `Layer`, `VectorDataset` and the vector operations.

    Args:
        tmp: A directory for the outputs.
        raster: A raster to polygonize.
    """
    layer = terrio.Layer.from_geojson(POLYGONS, name="parcels", crs="EPSG:32610")
    layer.name = "parcels"
    print(layer.name, layer.crs, layer.crs_wkt is not None, layer.geometry_type, len(layer))
    fields: dict[str, str] = layer.fields
    bounds = layer.bounds
    if bounds is not None:
        min_x, min_y, max_x, max_y = bounds
        print(min_x, min_y, max_x, max_y)
    collection: dict[str, Any] = layer.to_geojson()
    geo: dict[str, Any] = layer.__geo_interface__
    wkb: list[bytes | None] = layer.to_wkb()
    columns: dict[str, list[Any]] = layer.columns()
    print(fields, collection["type"], geo["type"], len(wkb), sorted(columns))
    reprojected = layer.to_crs("EPSG:4326")

    gpkg = str(tmp / "parcels.gpkg")
    layer.write(gpkg)
    reprojected.write(str(tmp / "parcels.geojson"), driver="GeoJSON")
    terrio.write_vector([layer], str(tmp / "both.gpkg"), overwrite=True)
    terrio.write_vector(POLYGONS, str(tmp / "dict.fgb"), crs="EPSG:32610")
    fgb: bytes = layer.to_bytes("parcels.fgb")
    print(len(fgb))

    # Arrow: no pyarrow needed to export; any PyCapsule consumer reads the result.
    table: terrio.ArrowTable = layer.to_arrow()
    native = layer.to_arrow(
        geometry_encoding="geoarrow", coordinates="interleaved", fid_column="fid"
    )
    print(table.num_rows, table.num_columns, table.column_names, table.num_batches, len(native))
    back: terrio.Layer = terrio.from_arrow(table, name="parcels")
    again: terrio.Layer = terrio.Layer.from_arrow(native, crs="EPSG:32610")
    from_layer = terrio.from_arrow(layer)
    terrio.write_vector(table, str(tmp / "arrow.gpkg"), layer_name="parcels")
    terrio.write_vector(layer, str(tmp / "arrow.gpkg"), append=True)
    print(len(back), len(again), len(from_layer))

    names: list[str] = terrio.list_layers(gpkg)
    ds = terrio.open_vector(gpkg, layers=names)
    inside = terrio.open_vector(gpkg, bbox=(0.0, 0.0, 5.0, 5.0))
    print(len(inside[0]))
    # Driver open options by their GDAL names (CSV, tile archives, HECRAS).
    csv = tmp / "places.csv"
    csv.write_text("name,lon,lat\nwest,1.5,2.5\n")
    csv_options = {"X_POSSIBLE_NAMES": "lon", "Y_POSSIBLE_NAMES": "lat"}
    opened = terrio.open_vector(str(csv), options=csv_options)
    summary: dict[str, Any] = terrio.info(str(csv), options=csv_options)
    print(len(opened), summary["driver"])
    print(ds.driver, ds.layer_names, len(ds), repr(ds))
    first: terrio.Layer = ds[0]
    by_name: terrio.Layer = ds["parcels"]
    all_layers: list[terrio.Layer] = ds.layers
    for each in ds:
        print(each.name)
    ds.write(str(tmp / "copy.gpkg"))
    print(first.name, by_name.name, len(all_layers))
    anything = terrio.open(gpkg)
    if isinstance(anything, terrio.VectorDataset):
        print(anything.layer_names)

    terrio.convert(
        gpkg,
        str(tmp / "west.fgb"),
        where="value < 5",
        select=["name"],
        ct_opt={"ALLOW_BALLPARK": "NO"},
    )
    stream: terrio.FeatureStream = terrio.open_stream(
        gpkg, "parcels", batch_size=1, bbox=(0.0, 0.0, 50.0, 50.0)
    )
    print(
        stream.name,
        stream.crs,
        stream.geometry_type,
        stream.fields,
        stream.batch_size,
        repr(stream),
    )
    chunk: terrio.Layer
    for chunk in stream:
        print(len(chunk))
    rest: terrio.Layer = terrio.open_stream(gpkg).read_all()
    capsule: object = terrio.open_stream(gpkg).__arrow_c_stream__()
    feature: dict[str, Any]
    for feature in terrio.iter_features(gpkg, batch_size=2):
        print(feature["properties"])
    print(len(rest), capsule is not None)
    burned = terrio.rasterize(layer, attribute="value", tr=(1.0, 1.0), ot="Int32")
    like = terrio.rasterize(gpkg, burn=1, like=burned, all_touched=True)
    polygons = terrio.polygonize(raster, band=1, connectedness=8)
    cut = terrio.warp(burned, cutline=layer, crop_to_cutline=True)
    print(repr(like), repr(polygons), repr(cut))


def points(tmp: Path) -> None:
    """Exercises `PointCloud`, `PointCloudFile` and the point cloud functions.

    Args:
        tmp: A directory for the outputs.
    """
    rng = np.random.default_rng(42)
    n = 1000
    arrays = {
        "x": 500000 + rng.uniform(0, 100, n),
        "y": 4000000 + rng.uniform(0, 100, n),
        "z": 100 + rng.uniform(0, 50, n),
        "classification": rng.choice([1, 2, 6], n).astype(np.uint8),
    }
    pc = terrio.PointCloud.from_dict(arrays, crs="EPSG:32617")
    z: NDArray[Any] = pc["z"]
    print(len(pc), "z" in pc, z.mean(), pc.dimensions, pc.crs, pc.bounds, pc.header)
    columns: dict[str, NDArray[Any]] = pc.to_dict()
    table = pc.to_numpy()
    counts: dict[int, int] = pc.classification_counts()
    print(sorted(columns), table.dtype.names, counts)
    ground = pc.filter(classes=[2], z_range=(100.0, 150.0))
    lonlat = pc.reproject("EPSG:4326")
    print(len(ground), lonlat.crs)

    path = str(tmp / "points.copc.laz")
    pc.write(path, point_format=6, scale=(0.01, 0.01, 0.01))
    las: bytes = pc.to_bytes("LAS")
    again = terrio.read_points_bytes(las)
    print(len(again))

    f = terrio.open_points(path)
    print(f.format, f.point_count, f.crs, f.bounds, f.header["point_format"], f.dimensions)
    summary: dict[str, Any] = f.info(stats=True)
    nodes: list[dict[str, Any]] = f.copc_nodes()
    print(len(summary), len(nodes))
    near = f.read(bounds=(500000.0, 4000000.0, 500050.0, 4000050.0), max_level=2)
    some = terrio.read_points(path, returns=["first"], max_points=500)
    print(len(near), len(some))

    dem = pc.grid(1.0, stat=["min", "max"], window_size=2)
    idw = pc.grid(2.0, stat="idw", power=2.0, ot="Float32")
    layer = pc.to_layer(dimensions=["z", "classification"], name="points")
    print(dem.count, idw.dtype, len(layer))
    terrio.convert_points(path, str(tmp / "points.gpkg"), classes=[2], dimensions=["z"])

    # Formats, chunks, filters and interpolation.
    pc.write(str(tmp / "points.ply"), ply_encoding="binary_little_endian")
    terrio.convert_points(path, str(tmp / "points.e57"), chunk_points=1000)
    with terrio.PointCloudWriter(str(tmp / "chunks.laz"), f.template()) as w:
        for chunk in f.chunks(1000):
            w.write(chunk)
    ground = pc.smrf(cell=2.0).range("Classification[2:2]")
    thin = pc.outlier(method="radius", radius=5.0).voxel_downsize(2.0, "first").sample(1.0)
    nn = pc.voxel_center_nearest_neighbor(2.0)
    more = (
        pc.pmf(cell_size=2.0)
        .csf(resolution=2.0)
        .elm()
        .hag_nn(count=3)
        .hag_delaunay()
        .crop(bounds=["([0, 10], [0, 10])"], outside=True)
        .decimation(2.0)
        .expression("Z > 0")
        .assign(value="Classification = 2 WHERE Z < 1")
        .normal(knn=8)
        .cluster(1.0)
    )
    tiles: list[terrio.PointCloud] = pc.splitter(100.0) + pc.chipper(5000)
    merged = terrio.merge_points([pc, more, *tiles])
    pc.write(str(tmp / "ept" / "ept.json"), ept_data_type="binary")
    terrio.convert_points(path, str(tmp / "ept2" / "ept.json"), ept_data_type="zstandard")
    tin = pc.grid(1.0, method="linear", radius=0.0)
    big = terrio.grid_points(path, 2.0, stat="max")
    print(
        len(ground),
        len(thin),
        len(nn),
        len(merged),
        tin.count,
        big.count,
        f.ept_info,
        f.wave_packet_descriptors,
    )


def rio(tmp: Path) -> None:
    """The rasterio-compatible API: writing, reading, windows, CRSs, features and warping.

    Args:
        tmp: A scratch directory.
    """
    import terrio.rio as rasterio
    from terrio.rio.enums import Resampling
    from terrio.rio.features import rasterize, shapes
    from terrio.rio.warp import calculate_default_transform, reproject
    from terrio.rio.windows import Window

    path = str(tmp / "rio.tif")
    transform = rasterio.transform.from_origin(500000.0, 4100000.0, 30.0, 30.0)
    data: NDArray[np.int16] = np.arange(40 * 50, dtype="int16").reshape(1, 40, 50)
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=50,
        height=40,
        count=1,
        dtype="int16",
        crs="EPSG:32611",
        transform=transform,
        nodata=-1,
        tiled=True,
    ) as dst:
        dst.write(data)
        dst.update_tags(source="usage")
        dst.build_overviews([2], Resampling.nearest)
    with rasterio.open(path) as src:
        window = Window(5, 5, 20, 10)
        part = src.read(1, window=window, masked=True)
        small = src.read(out_shape=(1, 20, 25), resampling=Resampling.bilinear)
        print(src.profile["blockxsize"], src.crs, src.bounds, part.shape, small.shape)
        print(src.window_transform(window), src.index(500100.0, 4099900.0), src.overviews(1))
        values = [v.tolist() for v in src.sample([(500100.0, 4099900.0)])]
        dst_t, w, h = calculate_default_transform(
            src.crs, "EPSG:4326", src.width, src.height, *src.bounds
        )
        out = np.zeros((h, w), "int16")
        reproject(rasterio.band(src, 1), out, dst_transform=dst_t, dst_crs="EPSG:4326")
        polys = list(shapes(src.read(1) // 500, transform=src.transform))
        print(values, out.shape, len(polys))
    mask = rasterize(
        [({"type": "Point", "coordinates": (500100.0, 4099900.0)}, 1)],
        (40, 50),
        transform=transform,
    )
    crs = rasterio.crs.CRS.from_epsg(4326)
    print(mask.sum(), crs.to_epsg(), crs.is_geographic)


def xarray_api(tmp: Path) -> None:
    """terrio.xarray: rioxarray's API on terrio.

    Args:
        tmp: A directory for outputs.
    """
    import xarray as xr

    import terrio.xarray as txr

    data = np.arange(40 * 50, dtype="float32").reshape(1, 40, 50)
    terrio.Raster.from_array(data, TRANSFORM, crs="EPSG:32611", nodata=-1.0).write(
        str(tmp / "xr.tif")
    )
    opened = txr.open_rasterio(str(tmp / "xr.tif"), masked=True, chunks={"x": 25})
    assert isinstance(opened, xr.DataArray)
    acc: txr.TerrioArray = opened.terrio
    left, bottom, right, top = acc.bounds()
    clipped = acc.clip_box(left, bottom, (left + right) / 2, (bottom + top) / 2)
    reprojected = clipped.terrio.reproject("EPSG:4326", resampling="bilinear")
    reprojected.terrio.to_raster(str(tmp / "xr_4326.tif"), compress="DEFLATE")
    merged = txr.merge_arrays([opened, opened])
    ds = xr.open_dataset(str(tmp / "xr.tif"), engine="terrio")
    print(acc.crs, acc.transform(), acc.resolution(), merged.shape, ds.terrio.crs)


def main() -> None:
    """Runs every example in a temporary directory."""
    with tempfile.TemporaryDirectory() as d:
        tmp = Path(d)
        raster = rasters(tmp)
        filled_dem: terrio.Raster = terrio.fill_nodata(
            raster, max_distance=20, interpolation="nearest", smoothing_iterations=1
        )
        assert filled_dem.read(1).shape == raster.read(1).shape
        vectors(tmp, raster)
        points(tmp)
        rio(tmp)
        xarray_api(tmp)
    try:
        terrio.open_raster("does-not-exist.tif")
    except terrio.NotFoundError as e:
        print(type(e).__name__)


if __name__ == "__main__":
    main()
