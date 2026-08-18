from __future__ import annotations

import hashlib
import importlib.util
import os
import tarfile
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).resolve().parent / "package_release.py"
SPEC = importlib.util.spec_from_file_location("package_release", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("Could not load package_release.py")
PACKAGE_RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGE_RELEASE)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def archive_inventory(path: Path) -> list[dict[str, int | str]]:
    with tarfile.open(path, mode="r:gz") as archive:
        return [
            {
                "path": member.name,
                "length": member.size,
                "mode": member.mode,
                "sha256": hashlib.sha256(archive.extractfile(member).read()).hexdigest(),
            }
            for member in archive.getmembers()
        ]


class ReleaseArtifactTests(unittest.TestCase):
    def test_archives_and_semantic_inventories_match_across_clean_roots(self) -> None:
        with tempfile.TemporaryDirectory(prefix="disc-cli-release-a-") as root_a_value:
            with tempfile.TemporaryDirectory(prefix="disc-cli-release-b-") as root_b_value:
                roots = [Path(root_a_value), Path(root_b_value)]
                archives: list[Path] = []
                for index, root in enumerate(roots):
                    (root / "README.md").write_text("Disc CLI\n", encoding="utf-8")
                    (root / "LICENSE").write_text("License\n", encoding="utf-8")
                    binary = root / "target" / "release" / "disc"
                    binary.parent.mkdir(parents=True)
                    binary.write_bytes(b"deterministic-binary")
                    timestamp = 1_700_000_000 + index * 10_000
                    for source_path in (binary, root / "README.md", root / "LICENSE"):
                        os.utime(source_path, (timestamp, timestamp))
                    archives.append(
                        PACKAGE_RELEASE.create_release_archive(
                            binary_path=binary,
                            target_triple="aarch64-apple-darwin",
                            output_dir=root / "dist",
                            repository_root=root,
                        )
                    )

                self.assertEqual(sha256(archives[0]), sha256(archives[1]))
                self.assertEqual(archive_inventory(archives[0]), archive_inventory(archives[1]))
                self.assertEqual(archives[0].read_bytes()[4:8], b"\x00\x00\x00\x00")
                self.assertEqual(
                    archive_inventory(archives[0]),
                    [
                        {
                            "path": "disc",
                            "length": 20,
                            "mode": 0o755,
                            "sha256": "aa13330ffb95b9e7b359d4a93c58b770e0f36fa2d667bff98270335ca8f205c9",
                        },
                        {
                            "path": "README.md",
                            "length": 9,
                            "mode": 0o644,
                            "sha256": "f023a3d364b0364faa86bdd8a255a27bbdbc287d05e17c80e2c2663cc15def99",
                        },
                        {
                            "path": "LICENSE",
                            "length": 8,
                            "mode": 0o644,
                            "sha256": "a10b32814e19bb00adb24f76a1ec5f4e8a2111e0a4f8ff0834d3c9485abfc9b4",
                        },
                    ],
                )

    def test_archive_metadata_is_normalized(self) -> None:
        with tempfile.TemporaryDirectory(prefix="disc-cli-release-metadata-") as root_value:
            root = Path(root_value)
            (root / "README.md").write_text("Disc CLI\n", encoding="utf-8")
            (root / "LICENSE").write_text("License\n", encoding="utf-8")
            binary = root / "disc.exe"
            binary.write_bytes(b"deterministic-binary")
            archive_path = PACKAGE_RELEASE.create_release_archive(
                binary_path=binary,
                target_triple="x86_64-pc-windows-msvc",
                output_dir=root / "dist",
                repository_root=root,
            )

            with tarfile.open(archive_path, mode="r:gz") as archive:
                members = archive.getmembers()
            self.assertEqual([member.name for member in members], ["disc.exe", "README.md", "LICENSE"])
            self.assertEqual([member.mtime for member in members], [0, 0, 0])
            self.assertEqual([member.uid for member in members], [0, 0, 0])
            self.assertEqual([member.gid for member in members], [0, 0, 0])
            self.assertEqual([member.mode for member in members], [0o755, 0o644, 0o644])


if __name__ == "__main__":
    unittest.main()
