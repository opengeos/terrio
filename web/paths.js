// SPDX-License-Identifier: MIT
// The paths the browser build reads remote datasets at: `/https/host/path` for
// `https://host/path`, a folder the worker fetches from with range requests (web/wasi.js).

/**
 * The path that command-line token `token` names in the browser build. Only a token that is a
 * remote path, or the value of a `--name=value` token that is one, is rewritten:
 * - `https://…` and `/vsicurl/https://…`, also inside GDAL's archive prefixes
 *   (`/vsizip/https://…/a.zip/b.shp`, `/vsizip//vsicurl/https://…`, `/vsizip/{https://…}/b.shp`);
 * - public S3 objects (`s3://bucket/key`, `/vsis3/bucket/key`), unsigned, from the global endpoint,
 *   which serves every region; or path-style for a bucket whose name has dots, which the
 *   endpoint's certificate does not cover (path-style serves us-east-1 buckets only);
 * - public Google Cloud Storage objects (`gs://bucket/key`, `/vsigs/bucket/key`).
 * Any other token, a URL in free text among them, is left as it is.
 * @param {string} token
 * @returns {string}
 */
export function toWorkerPath(token) {
  const option = /^(--[^=\s]+=)([\s\S]*)$/.exec(token);
  return option ? option[1] + remotePath(option[2]) : remotePath(token);
}

/**
 * An object key as a URL path: each segment percent-encoded, so that `#`, `?` and the like name
 * the object instead of starting a fragment or a query.
 */
function encodeKey(key) {
  return key.split("/").map(encodeURIComponent).join("/");
}

/** The archive prefixes before a remote path: `/vsizip/`, `/vsitar/{`, …. */
const WRAPPERS = /^((?:\/vsi(?!curl\/|s3\/|gs\/)[a-z0-9_]+\/\{?)*)/;

function remotePath(value) {
  const prefix = WRAPPERS.exec(value)[1];
  const rest = value.slice(prefix.length);
  let m;
  if ((m = /^(?:\/vsicurl\/)?https:\/\/([\s\S]*)$/.exec(rest))) return `${prefix}/https/${m[1]}`;
  // A bucket named as S3 names them, so that nothing else can end up in the host or query.
  if ((m = /^(?:s3:\/\/|\/vsis3\/)([a-z0-9][a-z0-9.-]{1,61}[a-z0-9])\/([\s\S]*)$/.exec(rest))) {
    const [, bucket, key] = m;
    return bucket.includes(".") ? `${prefix}/https/s3.amazonaws.com/${bucket}/${encodeKey(key)}` : `${prefix}/https/${bucket}.s3.amazonaws.com/${encodeKey(key)}`;
  }
  if ((m = /^(?:gs:\/\/|\/vsigs\/)([^/]+)\/([\s\S]*)$/.exec(rest))) return `${prefix}/https/storage.googleapis.com/${m[1]}/${encodeKey(m[2])}`;
  return value;
}
