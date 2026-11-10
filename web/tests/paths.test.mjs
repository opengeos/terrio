// SPDX-License-Identifier: MIT
// The page's rewriting of remote paths (web/paths.js), and the form code it shares with `terrio ui`.
//   node --test web/tests/paths.test.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { toWorkerPath } from "../paths.js";

test("remote paths are read under /https/", () => {
  const cases = [
    ["https://example.com/a/b.tif", "/https/example.com/a/b.tif"],
    ["/vsicurl/https://example.com/b.tif", "/https/example.com/b.tif"],
    ["/vsizip/https://example.com/a.zip/b.shp", "/vsizip//https/example.com/a.zip/b.shp"],
    ["/vsizip//vsicurl/https://example.com/a.zip/b.shp", "/vsizip//https/example.com/a.zip/b.shp"],
    ["/vsizip/{https://example.com/a.zip}/b.shp", "/vsizip/{/https/example.com/a.zip}/b.shp"],
    ["s3://sentinel-cogs/a/b.tif", "/https/sentinel-cogs.s3.amazonaws.com/a/b.tif"],
    ["/vsis3/sentinel-cogs/a/b.tif", "/https/sentinel-cogs.s3.amazonaws.com/a/b.tif"],
    ["s3://my.dotted.bucket/b.tif", "/https/s3.amazonaws.com/my.dotted.bucket/b.tif"],
    ["gs://bucket/b.tif", "/https/storage.googleapis.com/bucket/b.tif"],
    ["/vsigs/bucket/b.tif", "/https/storage.googleapis.com/bucket/b.tif"],
    ["--like=https://example.com/c.gpkg", "--like=/https/example.com/c.gpkg"],
    // Object keys name objects: `#` and `?` are part of the key, not a fragment or a query.
    ["s3://bucket-a/dir/a#1?b.tif", "/https/bucket-a.s3.amazonaws.com/dir/a%231%3Fb.tif"],
    ["gs://bucket/dir/a b#1.tif", "/https/storage.googleapis.com/bucket/dir/a%20b%231.tif"],
  ];
  for (const [token, path] of cases) assert.equal(toWorkerPath(token), path, token);
});

test("other tokens, URLs in free text among them, are left as they are", () => {
  for (const token of [
    "local.tif",
    "out/dir/a.gpkg",
    "--metadata=source=https://example.com/x",
    "source=https://example.com/x",
    "name LIKE 'https://%'",
    "--t-srs=EPSG:4326",
    "http://example.com/a.tif",
    "/vsizip/local.zip/a.shp",
    // Not a bucket name: left alone, not made into a host.
    "s3://a?b/key",
    "s3://Upper/key",
  ]) {
    assert.equal(toWorkerPath(token), token, token);
  }
});

/** The source of each named function or `const` of `html`'s script. */
function definitions(html, names) {
  const script = html.slice(html.indexOf("<script"), html.lastIndexOf("</script>"));
  const lines = script.split("\n");
  const found = {};
  for (const name of names) {
    const start = lines.findIndex((l) => l.startsWith(`function ${name}(`) || l.startsWith(`const ${name} =`));
    assert.notEqual(start, -1, `${name} is defined`);
    let end = start;
    if (lines[start].startsWith("function")) {
      while (end < lines.length && lines[end] !== "}") end++;
      assert.ok(end < lines.length, `${name} ends with a closing brace on a line of its own`);
    }
    found[name] = lines.slice(start, end + 1).join("\n");
  }
  return found;
}

test("the page's form code is the same as terrio ui's", () => {
  // Copied from crates/terrio-cli/src/ui.html, which `terrio ui` serves from the binary: a change
  // there is made here too.
  const names = ["el", "leaves", "isFlag", "isShown", "flagName", "layout", "occurrences", "commandLine", "shellQuote", "field", "renderList", "formatSize"];
  const ui = definitions(readFileSync(new URL("../../crates/terrio-cli/src/ui.html", import.meta.url), "utf8"), names);
  const web = definitions(readFileSync(new URL("../index.html", import.meta.url), "utf8"), names);
  for (const name of names) assert.equal(web[name], ui[name], `${name} differs from ui.html`);

  const runCommandLine = new Function(
    `${web.isFlag}\n${web.flagName}\n${web.layout}\n${web.occurrences}\n${web.commandLine}\nreturn commandLine;`,
  )();
  const cmd = { path: ["terrio", "raster", "translate"] };
  const posArg = { position: 0, num_args: { min: 1, max: 1 }, action: "set", allow_hyphen_values: false };
  const fixedOpt = { long: "of", num_args: { min: 1, max: 1 }, action: "set" };
  const variadicOpt = { long: "scale", num_args: { min: 2, max: 4 }, action: "append" };

  assert.deepEqual(
    runCommandLine(cmd, [
      { arg: variadicOpt, read: () => "0 255" },
      { arg: posArg, read: () => "in.tif" },
    ]),
    ["translate", "--scale", "0", "255", "--", "in.tif"],
  );
  assert.deepEqual(
    runCommandLine(cmd, [
      { arg: variadicOpt, read: () => "0 255" },
      { arg: fixedOpt, read: () => "GTiff" },
      { arg: posArg, read: () => "in.tif" },
    ]),
    ["translate", "--scale", "0", "255", "--of=GTiff", "in.tif"],
  );
  assert.deepEqual(
    runCommandLine(cmd, [
      { arg: fixedOpt, read: () => "GTiff" },
      { arg: posArg, read: () => "in.tif" },
    ]),
    ["translate", "--of=GTiff", "in.tif"],
  );
});


test("local file patterns avoid exponential backtracking and match Unicode characters", () => {
  const { patternMatcher } = definitions(readFileSync(new URL("../../crates/terrio-cli/src/ui.html", import.meta.url), "utf8"), ["patternMatcher"]);
  const matcher = new Function("separator", `${patternMatcher}\nreturn patternMatcher;`)("/");
  assert.equal(matcher("*a*a*a*a*a*b").test("a".repeat(10000)), false);
  assert.equal(matcher("?.tif").test("🌍.tif"), true);
  assert.equal(matcher("*.tif").test("a\nb.tif"), true);
  assert.equal(matcher("[a]*.tif").test("[a]1.tif"), true);
  assert.equal(matcher("*.tif").test("a.TIF"), false);
  const windows = new Function("separator", `${patternMatcher}\nreturn patternMatcher;`)("\\");
  assert.equal(windows("*.tif").test("a.TIF"), true);
});
