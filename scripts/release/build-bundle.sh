#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

release_dir="${INDUCTOR_RELEASE_DIR:-$repo_root/dist/release}"
target_label="${INDUCTOR_RELEASE_TARGET_LABEL:-$(uname | tr '[:upper:]' '[:lower:]')-$(uname -m)}"
version="$(awk -F '"' '/^version = / { print $2; exit }' crates/agent/Cargo.toml)"
bundle_name="inductor-${version}-${target_label}"
bundle_dir="$release_dir/$bundle_name"
archive_path="$release_dir/${bundle_name}.tar.gz"
checksum_path="$archive_path.sha256"

mkdir -p "$release_dir"
rm -rf "$bundle_dir" "$archive_path" "$checksum_path"
mkdir -p "$bundle_dir"

cargo build --release -p agent
cp "target/release/inductor" "$bundle_dir/inductor"

INDUCTOR_TUI_OUTFILE="$bundle_dir/inductor-open-tui" bun run build:tui

cp README.md "$bundle_dir/README.md"

tar -C "$release_dir" -czf "$archive_path" "$bundle_name"
shasum -a 256 "$archive_path" > "$checksum_path"

printf '%s\n' "$archive_path"