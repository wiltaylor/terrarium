#!/usr/bin/env bash
# Write a release version into [workspace.package] and the workspace
# entries of Cargo.lock, so `cargo build --locked` in the Dockerfile still
# passes. Usage: .github/stamp-version.sh 0.2.0
set -euo pipefail
v="$1"
sed -i '/^\[workspace.package\]/,/^\[/ s/^version = ".*"$/version = "'"$v"'"/' Cargo.toml
grep -q "^version = \"$v\"$" Cargo.toml || { echo "stamp-version: no [workspace.package] version" >&2; exit 1; }
cargo update --workspace --offline 2>/dev/null || cargo update --workspace
