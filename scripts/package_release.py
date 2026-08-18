from __future__ import annotations

import argparse
import gzip
import io
import os
import tarfile
import tempfile
from pathlib import Path


def create_release_archive(
    *, binary_path: Path, target_triple: str, output_dir: Path, repository_root: Path
) -> Path:
    if not binary_path.is_file():
        raise ValueError(f"Binary not found: {binary_path}")

    binary_name = "disc.exe" if "-windows-" in target_triple else "disc"
    inputs = (
        (binary_name, binary_path, 0o755),
        ("README.md", repository_root / "README.md", 0o644),
        ("LICENSE", repository_root / "LICENSE", 0o644),
    )
    for _, source_path, _ in inputs:
        if not source_path.is_file():
            raise ValueError(f"Release input not found: {source_path}")

    output_dir.mkdir(parents=True, exist_ok=True)
    archive_path = output_dir / f"disc-{target_triple}.tar.gz"
    temporary_file = tempfile.NamedTemporaryFile(dir=output_dir, delete=False)
    temporary_path = Path(temporary_file.name)

    try:
        with temporary_file:
            with gzip.GzipFile(filename="", mode="wb", fileobj=temporary_file, mtime=0) as gzip_file:
                with tarfile.open(fileobj=gzip_file, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                    for archive_name, source_path, mode in inputs:
                        contents = source_path.read_bytes()
                        metadata = tarfile.TarInfo(archive_name)
                        metadata.size = len(contents)
                        metadata.mode = mode
                        metadata.mtime = 0
                        metadata.uid = 0
                        metadata.gid = 0
                        metadata.uname = ""
                        metadata.gname = ""
                        archive.addfile(metadata, io.BytesIO(contents))
        os.replace(temporary_path, archive_path)
    except BaseException:
        temporary_path.unlink(missing_ok=True)
        raise

    return archive_path


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Create a deterministic Disc CLI release archive.")
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output-dir", required=True, type=Path)
    return parser.parse_args()


def main() -> None:
    arguments = parse_arguments()
    repository_root = Path(__file__).resolve().parent.parent
    try:
        archive_path = create_release_archive(
            binary_path=arguments.binary,
            target_triple=arguments.target,
            output_dir=arguments.output_dir,
            repository_root=repository_root,
        )
    except ValueError as error:
        raise SystemExit(str(error)) from error
    print(f"Created {archive_path}")


if __name__ == "__main__":
    main()
