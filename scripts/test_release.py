"""Verify release artifact contents and fail-closed publication inputs."""

import hashlib
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
import zipfile

import release


class ReleaseTests(unittest.TestCase):
    def test_tag_must_match_manifest(self):
        release.validate(f"v{release.VERSION}")
        for tag in (release.VERSION, "v999.0.0", "--help", "v0.1.0/extra"):
            with self.subTest(tag=tag), self.assertRaises(SystemExit):
                release.validate(tag)

    def test_archives_and_checksum_manifest(self):
        original = Path.cwd()
        with tempfile.TemporaryDirectory() as temporary:
            try:
                os.chdir(temporary)
                for name in ("LICENSE-MIT", "LICENSE-APACHE"):
                    Path(name).write_text(name)
                for target in release.TARGETS:
                    binary = "buildq.exe" if "windows" in target else "buildq"
                    source = Path("target") / target / "release" / binary
                    source.parent.mkdir(parents=True)
                    source.write_bytes(b"release test fixture")
                    source.chmod(0o755)
                    release.package(target)
                    output = Path("dist") / release.archive_name(target)
                    expected = {binary, "LICENSE-MIT", "LICENSE-APACHE"}
                    if "windows" in target:
                        with zipfile.ZipFile(output) as archive:
                            self.assertEqual(set(archive.namelist()), expected)
                            self.assertEqual(archive.read(binary), source.read_bytes())
                    else:
                        with tarfile.open(output) as archive:
                            self.assertEqual(set(archive.getnames()), expected)
                            self.assertTrue(archive.getmember(binary).mode & 0o111)
                            self.assertEqual(archive.extractfile(binary).read(), source.read_bytes())
                release.checksums()
                for line in Path("dist/SHA256SUMS").read_text().splitlines():
                    digest, name = line.split("  ")
                    self.assertEqual(digest, hashlib.sha256((Path("dist") / name).read_bytes()).hexdigest())
                self.assertEqual(len(Path("dist/SHA256SUMS").read_text().splitlines()), 5)
                # Extra assets and missing targets must prevent publication.
                with self.assertRaises(SystemExit):
                    release.checksums()
                Path("dist/SHA256SUMS").unlink()
                (Path("dist") / release.archive_name(release.TARGETS[0])).unlink()
                with self.assertRaises(SystemExit):
                    release.checksums()
            finally:
                os.chdir(original)


if __name__ == "__main__":
    unittest.main()
