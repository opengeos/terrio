#!/usr/bin/env bash
# Downloads and prepares the large files of `scripts/benchmark.py --large` into a directory
# (default: ../terrio-large-data), about 3.5 GB in all. Needs curl and the GDAL command-line tools.
#
# - 16 USGS 3DEP 1 arc-second tiles of Colorado (n39-n42, w104-w107), and their mosaic as one
#   tiled Deflate GeoTIFF (dem16.tif, 14412 × 14412 Float32).
# - A Sentinel-2 L1C red band (10980 × 10980, JPEG 2000), and the GeoTIFF GDAL makes of it
#   (s2_b04.tif, 15-bit samples packed with NBITS=15).
# - The Geofabrik extract of Colorado (colorado.osm.pbf, about 400 MB), and the GeoPackage GDAL
#   makes of it (colorado.gpkg: points, lines, multilinestrings, multipolygons, other_relations).
#   Geofabrik updates its extracts daily, so counts and timings drift a little over time.
#
# Usage: scripts/fetch_large_data.sh [DIR]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dir="${1:-$here/../../terrio-large-data}"
mkdir -p "$dir"
cd "$dir"
usgs=https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current
tiles=()
for lat in 39 40 41 42; do
  for lon in 104 105 106 107; do
    t="USGS_1_n${lat}w${lon}.tif"
    [ -e "$t" ] || curl -sSfL -o "$t" "$usgs/n${lat}w${lon}/$t"
    tiles+=("$t")
  done
done
if [ ! -e dem16.tif ]; then
  gdalbuildvrt -q tiles16.vrt "${tiles[@]}"
  gdal_translate -q -co TILED=YES -co COMPRESS=DEFLATE tiles16.vrt dem16.tif
  rm tiles16.vrt
fi
s2=https://storage.googleapis.com/gcp-public-data-sentinel-2
l1c=$s2/tiles/10/S/EG/S2B_MSIL1C_20240630T185919_N0510_R013_T10SEG_20240630T211017.SAFE/GRANULE/L1C_T10SEG_A038220_20240630T190812/IMG_DATA
[ -e s2_l1c_B04.jp2 ] || curl -sSfL -o s2_l1c_B04.jp2 "$l1c/T10SEG_20240630T185919_B04.jp2"
[ -e s2_b04.tif ] || gdal_translate -q s2_l1c_B04.jp2 s2_b04.tif
[ -e colorado.osm.pbf ] || curl -sSfL -o colorado.osm.pbf https://download.geofabrik.de/north-america/us/colorado-latest.osm.pbf
[ -e colorado.gpkg ] || ogr2ogr -f GPKG colorado.gpkg colorado.osm.pbf
ls -l "$dir"
