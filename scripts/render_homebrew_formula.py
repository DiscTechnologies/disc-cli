#!/usr/bin/env python3

from __future__ import annotations

import argparse
from pathlib import Path


TARGETS = (
    ("aarch64-apple-darwin", "OS.mac? && Hardware::CPU.arm?"),
    # ("x86_64-apple-darwin", "OS.mac? && Hardware::CPU.intel?"),
    ("x86_64-unknown-linux-gnu", "OS.linux? && Hardware::CPU.intel?"),
)


def parse_checksums(path: Path) -> dict[str, str]:
    checksums: dict[str, str] = {}
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if line == "":
            continue
        parts = line.split()
        if len(parts) < 2:
            raise ValueError(f"Invalid checksum line: {raw_line}")
        checksum = parts[0]
        filename = Path(parts[-1]).name
        checksums[filename] = checksum
    return checksums


def render_formula(version: str, release_base_url: str, checksums: dict[str, str]) -> str:
    branches: list[str] = []
    for index, (target, condition) in enumerate(TARGETS):
        archive_name = f"disc-{target}.tar.gz"
        checksum = checksums.get(archive_name)
        if checksum is None:
            raise ValueError(f"Missing checksum for {archive_name}")
        keyword = "if" if index == 0 else "elsif"
        branches.append(
            "\n".join(
                (
                    f"  {keyword} {condition}",
                    f'    url "{release_base_url}/{archive_name}"',
                    f'    sha256 "{checksum}"',
                )
            )
        )

    branches.append(
        """  else
    odie "Unsupported platform"
  end"""
    )

    return f"""class Disc < Formula
  desc "Disc command-line interface"
  homepage "https://github.com/disctech/disc-cli"
  version "{version}"

{chr(10).join(branches)}

  def install
    bin.install "disc"
    doc.install "README.md", "LICENSE"
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/disc --version")
  end
end
"""


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--release-base-url", required=True)
    parser.add_argument("--checksums", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    checksums = parse_checksums(Path(args.checksums))
    formula = render_formula(args.version, args.release_base_url, checksums)
    Path(args.output).write_text(formula, encoding="utf-8")


if __name__ == "__main__":
    main()
