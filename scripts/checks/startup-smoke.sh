#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

cargo build --release -p agent
INDUCTOR_TUI_OUTFILE=target/release/inductor-open-tui bun run build:tui

test -x ./target/release/inductor-open-tui

./target/release/inductor --version-info >/dev/null
./target/release/inductor --help >/dev/null
./target/release/inductor session demo-events >/dev/null
./target/release/inductor context count --text hello >/dev/null
./target/release/inductor diff show --repo . --summary >/dev/null