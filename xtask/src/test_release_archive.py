"""Tests for the dependency-free archive and SBOM generator."""

import hashlib
import importlib.util
import json
import pathlib
import tarfile
import tempfile
import unittest
from io import BytesIO


SCRIPT = pathlib.Path(__file__).with_name("release_archive.py")
SPEC = importlib.util.spec_from_file_location("release_archive", SCRIPT)
release_archive = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_archive)


class ReleaseArchiveTests(unittest.TestCase):
    def test_lockfiles_produce_cyclonedx_with_package_lock_names(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            cargo_lock = b'version = 4\n\n[[package]]\nname = "crate-a"\nversion = "1.2.3"\n'
            npm_lock = json.dumps(
                {
                    "lockfileVersion": 3,
                    "packages": {
                        "": {"name": "app", "version": "0.0.0"},
                        "node_modules/react": {"version": "19.3.1"},
                        "node_modules/@scope/pkg": {"version": "2.0.0"},
                        "node_modules/react/node_modules/nested": {"version": "1.0.0"},
                    },
                }
            ).encode("utf-8")
            purls_expected = {
                "pkg:cargo/crate-a@1.2.3",
                "pkg:npm/react@19.3.1",
                "pkg:npm/%40scope/pkg@2.0.0",
                "pkg:npm/nested@1.0.0",
            }

            for version, commit in [("0.1.0", "0123456789ab"), ("0.1.1", "abcdef012345")]:
                raw_tar = root / f"{version}.tar"
                staging = root / f"stage-{version}"
                staging.mkdir()
                root_name = f"craftcloud-{version}"
                with tarfile.open(raw_tar, "w:") as tar:
                    for name, payload in [
                        (f"{root_name}/tracked.txt", b"tracked fixture\n"),
                        (f"{root_name}/Cargo.lock", cargo_lock),
                        (f"{root_name}/apps/craftcloud-admin/package-lock.json", npm_lock),
                    ]:
                        info = tarfile.TarInfo(name)
                        info.size = len(payload)
                        tar.addfile(info, BytesIO(payload))
                release_archive.make_archive(
                    raw_tar,
                    staging,
                    root / "dist",
                    version,
                    commit,
                    f"{root_name}.tar.gz",
                )
            archive = root / "dist/craftcloud-0.1.0.tar.gz"
            bom = json.loads((root / "dist/craftcloud-0.1.0.cdx.json").read_text(encoding="utf-8"))
            purls = {item["purl"] for item in bom["components"]}
            self.assertEqual(purls, purls_expected)
            with tarfile.open(archive, "r:gz") as tar:
                names = set(tar.getnames())
                extracted = tar.extractfile("craftcloud-0.1.0/CRAFTCLOUD-RELEASE.json")
                self.assertIsNotNone(extracted)
                metadata = json.load(extracted)
            self.assertIn("craftcloud-0.1.0/tracked.txt", names)
            self.assertEqual(metadata, {"version": "0.1.0", "commit": "0123456789ab"})
            sums = (root / "dist/SHA256SUMS").read_text(encoding="ascii")
            self.assertTrue(sums.startswith(hashlib.sha256(archive.read_bytes()).hexdigest()))
            self.assertIn(archive.name, sums)
            self.assertEqual(len(sums.splitlines()), 2)


if __name__ == "__main__":
    unittest.main()
