# Playground

Try terrio in your browser. This page runs the [npm package](javascript.md), terrio compiled to
WebAssembly: files are read and written on your computer and never uploaded. The first visit
downloads about 6 MB.

<div id="terrio-playground" markdown="0">
  <p class="tp-status" id="tp-status">Loading terrio…</p>

  <div class="tp-drop" id="tp-drop">
    <p><strong>Drop a file here</strong> or <label class="tp-link">choose one<input type="file" id="tp-file" class="tp-visually-hidden"></label>.</p>
    <p class="tp-hint">GeoTIFF/COG (including ZSTD, LERC, JPEG XL and LZMA), JPEG 2000, netCDF, HDF5, Esri ASCII grid, GeoPackage, PMTiles, MBTiles, GeoJSON, FlatGeobuf, GeoParquet, CSV, KML/KMZ, GML, GPX, TopoJSON, OSM, MapInfo, or a zip of a shapefile, a MapInfo TAB file or a Zarr store.</p>
    <p class="tp-samples">Or a sample:
      <button type="button" data-sample="https://raw.githubusercontent.com/opengeos/data/main/world/countries.geojson">countries (GeoJSON, 240 KB)</button>
      <button type="button" data-sample="https://raw.githubusercontent.com/opengeos/data/main/vector/cables.geojson">submarine cables (GeoJSON, 590 KB)</button>
      <button type="button" data-sample="https://raw.githubusercontent.com/opengeos/data/main/raster/landsat7.tif">Landsat 7 (GeoTIFF, 4.9 MB)</button>
      <button type="button" data-sample="https://raw.githubusercontent.com/opengeos/data/main/raster/srtm90.tif">SRTM DEM (GeoTIFF, 16 MB)</button>
    </p>
  </div>

  <div id="tp-result" hidden>
    <h2 id="tp-name"></h2>
    <div class="tp-grid">
      <div>
        <h3>Preview</h3>
        <canvas id="tp-canvas" width="640" height="400"></canvas>
        <p class="tp-hint" id="tp-preview-note"></p>
      </div>
      <div>
        <h3>Convert</h3>
        <label>Format <select id="tp-format"></select></label>
        <label>Reproject to <input id="tp-tsrs" placeholder="e.g. EPSG:3857 (optional)"></label>
        <label id="tp-where-row">Filter <input id="tp-where" placeholder="e.g. name LIKE 'United%' (optional)"></label>
        <button type="button" id="tp-convert" class="md-button md-button--primary">Convert and download</button>
        <p class="tp-hint" id="tp-convert-note"></p>
      </div>
    </div>
    <h3>Info</h3>
    <p class="tp-hint">What <code>terrio info --json</code> prints for this file.</p>
    <pre id="tp-info"></pre>
  </div>

  <h2>Transform coordinates</h2>
  <div class="tp-transform">
    <label>x / lon <input id="tp-x" value="-122.4194"></label>
    <label>y / lat <input id="tp-y" value="37.7749"></label>
    <label>from <input id="tp-src" value="EPSG:4326"></label>
    <label>to <input id="tp-dst" value="EPSG:32610"></label>
    <button type="button" id="tp-transform" class="md-button">Transform</button>
  </div>
  <pre id="tp-transform-out"></pre>
</div>

The page uses the same functions a web application would; see [JavaScript](javascript.md) for the
API.
