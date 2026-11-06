# Web App

[**Open terrio Web →**](web/){ .md-button .md-button--primary }

[terrio Web](web/) runs the `terrio` command line in your browser. Pick any
of its commands, fill in the form and run it. The forms come from the same command catalog as
[`terrio ui`](cli.md) (`terrio --json-usage`), so every command and option the CLI has is there.

## Your data stays in your browser

terrio Web is a static website, hosted on GitHub Pages with the rest of this documentation. There is
no terrio server behind it: the page, its scripts and the command line compiled to WebAssembly
(`wasm32-wasip1`) are downloaded once, and every command runs in a Web Worker in your browser tab.

- **Uploaded files stay in the browser worker.** Uploading copies a file from your disk into the
  worker's memory. It is never sent to any server, and neither are the files commands write.
- **Remote files are fetched by your browser**, straight from the server that hosts them. No other
  server sees the URL or the data.
- Closing or reloading the tab discards the workspace.

## Files

- **Upload** files, or drop them on the page, to add them to the workspace. Upload every part of
  a multi-file dataset, such as a shapefile's `.shp`, `.shx`, `.dbf` and `.prj`, or upload it as a
  zip and read it with `/vsizip/`.
- **Outputs** a command writes appear in the workspace and under the form, with a Download button.
  Images a command writes are shown on the page.
- **`preview`** without `--output` shows its PNG on the page with a Download PNG button, as
  `terrio ui` does, and adds nothing to the workspace.
- The workspace is held in the worker's memory. Reloading the page empties it.

## Map

**Open map** on the `preview` form opens a pan-and-zoom map of the dataset in its own grid, as
`terrio view` and the Open map button of `terrio ui` show it, with the same band, colormap, stretch,
layer and attribute options. Drag or use the arrow keys to pan, scroll or press + and − to zoom,
press 0 to fit, and press Escape or ✕ to close it. The status line shows the pixel and coordinates
under the pointer.

Tiles are drawn by the command line in the worker, a screenful at a time, and kept while the map is
open. For a remote dataset, each part of the file is fetched once and reused as you pan and zoom.

## Remote files

Type an `https://` URL wherever a command takes a path, or add it to the workspace with the URL
box. The page reads the file with HTTP range requests as the command needs it, as the native
command line does. A Cloud Optimized GeoTIFF is read by window and overview, not downloaded whole.
After each run, the page shows how many requests the command made and how many bytes it fetched.

- `/vsicurl/https://…`, and URLs inside a path (`/vsizip/https://…/data.zip/a.shp`), work too.
- Public S3 and Google Cloud Storage objects (`s3://bucket/key`, `gs://bucket/key`, `/vsis3/`,
  `/vsigs/`) are read over HTTPS without signing. S3 objects are read from the global endpoint
  (`bucket.s3.amazonaws.com`), which serves buckets in every region. A bucket whose name contains
  dots is read path-style (`s3.amazonaws.com/bucket`), which works only for buckets in us-east-1.
- Private buckets need a presigned `https://` URL. A presigned URL is signed for GET only, so the
  page learns the file's size from the first range it reads: the bucket's CORS rules must expose
  the `Content-Range` header for the file to be read by ranges. Otherwise it is read whole.
- The server must allow cross-origin requests (CORS). The browser blocks reads from servers that
  don't, and the page reports the URL it could not read.
- The page reads a file whole instead of by ranges when it is smaller than 1 MB, when the server
  ignores `Range` requests, or when the server compresses its responses (a compressed response's
  ranges count compressed bytes).
- Plain `http://` URLs can't be read: the page is served over HTTPS, and browsers block insecure
  requests from it.
- Fetched parts of a remote file are kept while the page is open, so later commands on the same
  URL (and the map) do not fetch them again. A file that changes on its server during that time is
  seen as it was; reload the page to read it afresh.
- Sidecar files next to a remote dataset, such as `.aux.xml` or a shapefile's `.dbf`, are looked up
  on the server by name. A command that lists a remote folder sees only the names it has already
  looked up.

## Limits

- **Memory.** The workspace and every file a command reads whole are held in the tab's memory. A
  WebAssembly module can address at most 4 GB.
- **One thread.** Commands run on one thread, so heavy processing such as `warp` and `viewshed`
  takes longer than with the native command line.
- **No network commands.** STAC searches, WFS, `vsi` operations on object stores and other
  commands that talk to a server API are not available. Reading files from URLs works.
- **No `ui`, `view` or `mcp`.** These commands run servers. The [map](#map) takes the place of
  `view`.
- **Cancelling** a command ends it and restarts terrio, with the workspace as it was before the
  command started.

The [Playground](playground.md) is a lighter page built on the [npm package](javascript.md). It
previews, converts and reprojects a file, and reads remote files the same way.
