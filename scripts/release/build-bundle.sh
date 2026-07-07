#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

release_dir="${INDUCTOR_RELEASE_DIR:-$repo_root/dist/release}"
case "$(uname -s):$(uname -m)" in
  Darwin:arm64|Darwin:aarch64)
    default_target_label="aarch64-apple-darwin"
    ;;
  *)
    printf '%s\n' "Inductor release bundles currently support Apple Silicon macOS only." >&2
    exit 1
    ;;
esac
target_label="${INDUCTOR_RELEASE_TARGET_LABEL:-$default_target_label}"
version="$(awk -F '"' '/^version = / { print $2; exit }' crates/agent/Cargo.toml)"
bundle_name="inductor-${version}-${target_label}"
archive_name="inductor-${target_label}.tar.gz"
bundle_dir="$release_dir/$bundle_name"
archive_path="$release_dir/$archive_name"
checksum_path="$archive_path.sha256"

sha256_cmd() {
  if command -v sha256sum >/dev/null 2>&1; then
    printf 'sha256sum'
  else
    printf 'shasum -a 256'
  fi
}

mkdir -p "$release_dir"
rm -rf "$bundle_dir" "$archive_path" "$checksum_path"
mkdir -p "$bundle_dir"

cargo build --release -p agent
cp "target/release/inductor" "$bundle_dir/inductor"

INDUCTOR_TUI_OUTFILE="$bundle_dir/inductor-open-tui" bun run build:tui

cp README.md "$bundle_dir/README.md"
printf '%s\n' "$version" > "$bundle_dir/VERSION"

tar -C "$release_dir" -czf "$archive_path" "$bundle_name"
$(sha256_cmd) "$archive_path" > "$checksum_path"

printf '%s\n' "$archive_path"
