"""Tests for the image archive helper (no Docker needed)."""

import hashlib
import importlib.util
import pathlib
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).with_name("release_image.py")
SPEC = importlib.util.spec_from_file_location("release_image", SCRIPT)
release_image = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_image)


class ReleaseImageTests(unittest.TestCase):
    def test_names_follow_the_documented_pattern(self):
        self.assertEqual(
            release_image.image_archive_name("0.1.0", "arm64"),
            "craftcloud-0.1.0-image-arm64.tar.zst",
        )
        self.assertEqual(release_image.image_reference("1.2.3+build.7"), "craftcloud:1.2.3_build.7")

    def test_architecture_selection(self):
        both = {"amd64", "arm64"}
        self.assertEqual(release_image.parse_architectures("", both), ["amd64", "arm64"])
        self.assertEqual(release_image.parse_architectures("", {"amd64"}), ["amd64"])
        self.assertEqual(release_image.parse_architectures("arm64", both), ["arm64"])
        for bad in ("riscv", "amd64,amd64", ",", "arm64"):
            available = {"amd64"} if bad == "arm64" else both
            with self.assertRaises(ValueError, msg=bad):
                release_image.parse_architectures(bad, available)

    def test_checksum_entry_matches_the_file_and_is_added_once(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = pathlib.Path(temporary)
            (output / "SHA256SUMS").write_text("0" * 64 + "  craftcloud-0.1.0.tar.gz\n", encoding="ascii")
            name = release_image.image_archive_name("0.1.0", "amd64")
            payload = b"image bytes"
            (output / name).write_bytes(payload)
            release_image.register_checksum(output, name)
            lines = (output / "SHA256SUMS").read_text(encoding="ascii").splitlines()
            self.assertEqual(len(lines), 2)
            self.assertEqual(lines[0], "0" * 64 + "  craftcloud-0.1.0.tar.gz")
            self.assertEqual(lines[1], f"{hashlib.sha256(payload).hexdigest()}  {name}")
            with self.assertRaises(FileExistsError):
                release_image.register_checksum(output, name)

    def test_checksum_file_is_created_when_missing(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = pathlib.Path(temporary)
            name = release_image.image_archive_name("0.1.0", "arm64")
            (output / name).write_bytes(b"x")
            release_image.register_checksum(output, name)
            self.assertTrue((output / "SHA256SUMS").read_text(encoding="ascii").endswith(f"  {name}\n"))

    def test_compression_round_trips_when_a_codec_is_available(self):
        try:
            from compression import zstd
        except ImportError:
            self.skipTest("Python 3.14 compression.zstd not available")
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source = root / "in.tar"
            source.write_bytes(b"abc" * 10000)
            packed = root / "out.tar.zst"
            release_image.compress_zstd(source, packed)
            with zstd.open(packed, "rb") as handle:
                self.assertEqual(handle.read(), source.read_bytes())


if __name__ == "__main__":
    unittest.main()
