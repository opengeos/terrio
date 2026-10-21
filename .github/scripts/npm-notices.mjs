// Adds terrio's license files to the npm package that wasm-pack built in the given directory:
// copies LICENSE, NOTICE and LICENSE-THIRD-PARTY.md next to package.json and lists them in its
// `files` (npm packs LICENSE and README unlisted, but not the other two).
//
// Usage: node .github/scripts/npm-notices.mjs js/pkg
import { copyFileSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const dir = process.argv[2];
if (!dir) throw new Error("usage: node npm-notices.mjs <package directory>");
const names = ["LICENSE", "NOTICE", "LICENSE-THIRD-PARTY.md"];
for (const name of names) copyFileSync(name, join(dir, name));

const path = join(dir, "package.json");
const pkg = JSON.parse(readFileSync(path, "utf8"));
// Without `files`, npm packs the whole directory, the license files included.
if (Array.isArray(pkg.files)) {
  for (const name of names) if (!pkg.files.includes(name)) pkg.files.push(name);
  writeFileSync(path, JSON.stringify(pkg, null, 2) + "\n");
}
