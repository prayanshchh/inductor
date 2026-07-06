#!/usr/bin/env sh
set -eu

repo="${INDUCTOR_REPO:-prayanshchhablani/inductor}"
release_base_url="${INDUCTOR_DOWNLOAD_BASE_URL:-https://github.com/${repo}/releases/download}"
version_override="${INDUCTOR_VERSION:-}"
install_dir_override="${INDUCTOR_INSTALL_DIR:-}"

fail() {
  printf '%s\n' "$*" >&2
  exit 1
}

require() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

require curl
require tar

sha256_cmd() {
  if command -v sha256sum >/dev/null 2>&1; then
    printf 'sha256sum'
  else
    require shasum
    printf 'shasum -a 256'
  fi
}

os_name="$(uname -s | tr '[:upper:]' '[:lower:]')"
machine="$(uname -m)"
case "$os_name:$machine" in
  darwin:arm64|darwin:aarch64)
    target_label="aarch64-apple-darwin"
    ;;
  *)
    fail "Inductor release binaries currently support Apple Silicon macOS only."
    ;;
esac

if [ -n "$version_override" ]; then
  latest_tag="$version_override"
  case "$latest_tag" in
    v*) : ;;
    *) latest_tag="v$latest_tag" ;;
  esac
else
  latest_tag="$(curl -fsSL "https://api.github.com/repos/${repo}/releases/latest" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
  [ -n "$latest_tag" ] || fail "could not determine latest release tag for ${repo}"
fi
version="${latest_tag#v}"
archive_name="inductor-${target_label}.tar.gz"
checksum_name="${archive_name}.sha256"
download_url="${release_base_url}/${latest_tag}/${archive_name}"
checksum_url="${release_base_url}/${latest_tag}/${checksum_name}"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT HUP INT TERM

archive_path="$workdir/$archive_name"
checksum_path="$workdir/$checksum_name"

printf 'Installing Inductor %s for %s...\n' "$version" "$target_label"
printf 'Downloading %s...\n' "$archive_name"
curl -fL --retry 3 --retry-delay 1 -o "$archive_path" "$download_url"
curl -fL --retry 3 --retry-delay 1 -o "$checksum_path" "$checksum_url"

expected_sha="$(awk 'NR==1 { print $1 }' "$checksum_path")"
actual_sha="$(eval "$(sha256_cmd)" "$archive_path" | awk '{ print $1 }')"
[ "$expected_sha" = "$actual_sha" ] || fail "checksum mismatch for ${archive_name}"

extract_dir="$workdir/extract"
mkdir -p "$extract_dir"
tar -xzf "$archive_path" -C "$extract_dir"

bundle_dir="$(find "$extract_dir" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
[ -n "$bundle_dir" ] || fail "release archive did not contain a bundle directory"

install_dir="$install_dir_override"
install_mode="direct"
if [ -z "$install_dir" ]; then
  for candidate in "/opt/homebrew/bin" "/usr/local/bin" "$HOME/.local/bin"; do
    if [ -d "$candidate" ] && [ -w "$candidate" ]; then
      install_dir="$candidate"
      break
    fi
    if [ ! -e "$candidate" ] && [ "$candidate" = "$HOME/.local/bin" ]; then
      mkdir -p "$candidate"
      install_dir="$candidate"
      break
    fi
  done
fi

if [ -z "$install_dir" ]; then
  for candidate in "/opt/homebrew/bin" "/usr/local/bin"; do
    if command -v sudo >/dev/null 2>&1; then
      install_dir="$candidate"
      install_mode="sudo"
      break
    fi
  done
fi

[ -n "$install_dir" ] || fail "could not find a writable install directory"

printf 'Installing to %s...\n' "$install_dir"
if [ "$install_mode" = "sudo" ]; then
  sudo install -d "$install_dir"
  sudo install -m 755 "$bundle_dir/inductor" "$install_dir/inductor"
  sudo install -m 755 "$bundle_dir/inductor-open-tui" "$install_dir/inductor-open-tui"
else
  install -m 755 "$bundle_dir/inductor" "$install_dir/inductor"
  install -m 755 "$bundle_dir/inductor-open-tui" "$install_dir/inductor-open-tui"
fi

printf 'Verifying installation...\n'
"$install_dir/inductor" --version-info >/dev/null
"$install_dir/inductor" --help >/dev/null

case ":$PATH:" in
  *":$install_dir:"*)
    ;;
  *)
    printf 'Installed to %s, which is not on your PATH. Add it to use inductor immediately.\n' "$install_dir" >&2
    ;;
esac

printf 'Inductor %s installed successfully.\n' "$version"
printf 'Run: inductor --help\n'