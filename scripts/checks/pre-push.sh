#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

bash scripts/checks/rust-quality.sh
bash scripts/checks/tui-quality.sh
bash scripts/checks/startup-smoke.sh