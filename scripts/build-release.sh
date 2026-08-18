#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
host_triple="$(rustc -vV | awk '/^host:/ { print $2 }')"

if [[ -z "$host_triple" ]]; then
  echo "Could not determine the Rust host triple." >&2
  exit 1
fi

# Keep source coordinates stable without allowing ambient compiler flags to contaminate release artifacts.
unset RUSTFLAGS
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=${repo_root}=."

if [[ "$host_triple" == *-apple-darwin ]]; then
  # ld64 otherwise writes a content-dependent UUID that differs between clean absolute roots.
  cargo rustc --locked --release --bin disc -- -C link-arg=-Wl,-no_uuid
else
  cargo rustc --locked --release --bin disc
fi
