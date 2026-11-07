# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Exercise the catalog guard against actual temporary Git base commits."""

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("catalog_guard", Path(__file__).parents[1] / "check_cli_catalog_version.py")
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


class CatalogVersionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        self.git("init", "--quiet", "--initial-branch=main")
        self.write("0.1.1", "a" * 64)
        self.base = self.commit()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, check=True, capture_output=True, text=True).stdout.strip()

    def write(self, version, digest):
        schema = self.repo / guard.SCHEMA
        schema.parent.mkdir(parents=True, exist_ok=True)
        schema.write_text(json.dumps({"properties": {"schema_version": {"const": version}}}))
        golden = self.repo / guard.GOLDEN
        golden.parent.mkdir(parents=True, exist_ok=True)
        if digest is None:
            golden.unlink(missing_ok=True)
        else:
            golden.write_text(f'const CATALOG_SHA256: &str = "{digest}";\n')

    def commit(self):
        self.git("add", ".")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "-m", "baseline")
        return self.git("rev-parse", "HEAD")

    def test_unchanged_catalog_passes(self):
        guard.check(self.base, self.repo)

    def test_digest_only_update_fails(self):
        self.write("0.1.1", "b" * 64)
        with self.assertRaisesRegex(ValueError, "bump the catalog patch"):
            guard.check(self.base, self.repo)

    def test_newer_patch_passes(self):
        self.write("0.1.2", "b" * 64)
        guard.check(self.base, self.repo)

    def test_older_version_fails(self):
        self.write("0.1.0", "b" * 64)
        with self.assertRaisesRegex(ValueError, "newer schema_version"):
            guard.check(self.base, self.repo)

    def test_initial_golden_requires_bump(self):
        self.write("0.1", None)
        base = self.commit()
        self.write("0.1", "b" * 64)
        with self.assertRaisesRegex(ValueError, "newer schema_version"):
            guard.check(base, self.repo)
        self.write("0.1.1", "b" * 64)
        guard.check(base, self.repo)

    def test_missing_base_is_not_treated_as_initial_catalog(self):
        with self.assertRaisesRegex(ValueError, "base revision is not available"):
            guard.check("missing-revision", self.repo)


if __name__ == "__main__":
    unittest.main()
