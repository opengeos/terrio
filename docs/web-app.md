# Web App

[**Open terrio Web →**](https://terrio.dev/web/){ .md-button .md-button--primary }

[terrio Web](https://terrio.dev/web/) runs the `terrio` command line in your browser. Pick any
of its commands, fill in the form and run it. The forms come from the same command catalog as
[`terrio ui`](cli.md) (`terrio --json-usage`), so every command and option the CLI has is there.

The command line is compiled to WebAssembly (`wasm32-wasip1`) and runs in your browser tab. Your
files are not uploaded to a server.

## Files

- **Upload** files, or drop them on the page, to add them to the workspace. Upload every part of
  a multi-file dataset, such as a shapefile's `.shp`, `.shx`, `.dbf` and `.prj`, or upload it as a
  zip and read it with `/vsizip/`.
- **Outputs** a command writes appear in the workspace and under the form, with a Download button.
  Images, such as the PNG that `terrio preview` writes, are shown on the page.
- The workspace is held in memory. Reloading the page empties it.

## Remote files

Type an `https://` URL wherever a command takes a path, or add it to the workspace with the URL
box. The page reads the file with HTTP range requests as the command needs it, as the native
command line does. A Cloud Optimized GeoTIFF is read by window and overview, not downloaded whole.
After each run, the page shows how many requests the command made and how many bytes it fetched.

- `/vsicurl/https://…`, and URLs inside a path (`/vsizip/https://…/data.zip/a.shp`), work too.
- Public S3 and Google Cloud Storage objects (`s3://bucket/key`, `gs://bucket/key`, `/vsis3/`,
  `/vsigs/`) are read over HTTPS without signing. Private buckets need a presigned `https://` URL.
- The server must allow cross-origin requests (CORS). The browser blocks reads from servers that
  don't, and the page reports the URL it could not read.
- The page reads a file whole instead of by ranges when it is smaller than 1 MB, when the server
  ignores `Range` requests, or when the server compresses its responses (a compressed response's
  ranges count compressed bytes).
- Plain `http://` URLs can't be read: the page is served over HTTPS, and browsers block insecure
  requests from it.
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
- **No `ui`, `view` or `mcp`.** These commands run servers. `preview` writes a PNG, which the page
  shows.
- **Cancelling** a command ends it and restarts terrio, with the workspace as it was before the
  command started.

The [Playground](playground.md) is a lighter page built on the [npm package](javascript.md). It
previews, converts and reprojects a file, and reads remote files the same way.
