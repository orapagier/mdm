"""Exercise Linux resource staging without compiling or installing the app.
Run: python3 -m unittest discover -s tests -p '*_test.py'
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import zipfile

REPO = Path(__file__).resolve().parents[1]


class BundleTests(unittest.TestCase):
    def build(self, signed=False, custom_target=False):
        with tempfile.TemporaryDirectory(prefix="mdm-bundle-test-") as temp:
            root = Path(temp)
            for name in ("packaging", "extension", "src-tauri"):
                (root / name).mkdir()
            for name in ("bundle.sh", "packaging/common.sh",
                         "src-tauri/tauri.bundle.linux.conf.json"):
                shutil.copyfile(REPO / name, root / name)
            (root / "extension/manifest.json").write_text('{}')
            (root / "extension/manifest.chrome.json").write_text('{"key":"test-key"}')
            target = root / ("custom-output" if custom_target else "target")
            target.mkdir()
            env = dict(os.environ, CARGO_TARGET_DIR=str(target))
            env.pop("MDM_XPI", None)
            expected = None
            if signed:
                xpi = root / "external-signed.xpi"
                with zipfile.ZipFile(xpi, "w") as archive:
                    archive.writestr("META-INF/mozilla.rsa", "fixture metadata, not a real signature")
                expected = xpi.read_bytes()
                env["MDM_XPI"] = str(xpi)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            cargo = bin_dir / "cargo"
            cargo.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path.cwd()
target = pathlib.Path(os.environ["CARGO_TARGET_DIR"])
if sys.argv[1:3] == ["tauri", "--version"]:
    sys.exit(0)
if sys.argv[1] == "build":
    (target / "release").mkdir(parents=True, exist_ok=True)
    host = target / "release/mdm-host"
    host.write_text("fixture")
    host.chmod(0o755)
elif sys.argv[1:3] == ["tauri", "build"]:
    config = json.loads((root / "src-tauri/tauri.bundle.linux.conf.json").read_text())
    for resource in config["bundle"]["resources"]:
        assert (root / "src-tauri" / resource).exists(), resource
    out = target / "release/bundle/rpm"
    out.mkdir(parents=True, exist_ok=True)
    (out / "mdm-test.rpm").write_text("fixture")
else:
    raise RuntimeError(sys.argv)
''')
            cargo.chmod(0o755)
            rustc = bin_dir / "rustc"
            rustc.write_text('#!/bin/sh\nprintf "host: x86_64-unknown-linux-gnu\\n"\n')
            rustc.chmod(0o755)
            env["PATH"] = str(bin_dir) + os.pathsep + env["PATH"]
            result = subprocess.run(["bash", "bundle.sh", "--rpm"], cwd=root,
                                    env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            staged = root / "target/mdm-firefox.xpi"
            if signed:
                self.assertEqual(staged.read_bytes(), expected)
                self.assertEqual((target / "release/bundle/mdm-firefox.xpi").read_bytes(), expected)
            else:
                self.assertTrue(zipfile.is_zipfile(staged))
                self.assertIn("unsigned add-on", result.stdout)
                self.assertTrue((target / "release/bundle/mdm-firefox-unsigned.xpi").exists())
            manifest = json.loads((root / "target/mdm-chrome/manifest.json").read_text())
            self.assertEqual(manifest["key"], "test-key")

    def test_clean_checkout_without_signed_extension(self):
        self.build()

    def test_explicit_signed_extension(self):
        self.build(signed=True)

    def test_redirected_cargo_output(self):
        self.build(custom_target=True)


if __name__ == "__main__":
    unittest.main()
