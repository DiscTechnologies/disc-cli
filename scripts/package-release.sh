#!/usr/bin/env bash

set -euo pipefail

binary_path=""
target_triple=""
output_dir=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary)
      binary_path="$2"
      shift 2
      ;;
    --target)
      target_triple="$2"
      shift 2
      ;;
    --output-dir)
      output_dir="$2"
      shift 2
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

if [[ -z "$binary_path" ]]; then
  echo "--binary is required" >&2
  exit 1
fi

if [[ -z "$target_triple" ]]; then
  echo "--target is required" >&2
  exit 1
fi

if [[ -z "$output_dir" ]]; then
  echo "--output-dir is required" >&2
  exit 1
fi

if [[ ! -f "$binary_path" ]]; then
  echo "Binary not found: $binary_path" >&2
  exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
archive_name="disc-${target_triple}.tar.gz"

mkdir -p "$output_dir"

temp_dir="$(mktemp -d)"
cleanup() {
  rm -rf "$temp_dir"
}
trap cleanup EXIT

cp "$binary_path" "$temp_dir/disc"
cp "$repo_root/README.md" "$temp_dir/README.md"
cp "$repo_root/LICENSE" "$temp_dir/LICENSE"
chmod 0755 "$temp_dir/disc"

tar -C "$temp_dir" -czf "$output_dir/$archive_name" disc README.md LICENSE

echo "Created $output_dir/$archive_name"
