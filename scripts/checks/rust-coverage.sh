#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

threshold="${RUST_COVERAGE_THRESHOLD:-66}"
mkdir -p coverage

cargo llvm-cov --workspace --all-features --fail-under-lines "$threshold" --summary-only \
  | tee coverage/rust-summary.txt

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "## Rust coverage"
    echo
    echo "Threshold: ${threshold}% lines"
    echo
    echo '```text'
    cat coverage/rust-summary.txt
    echo '```'
  } >> "$GITHUB_STEP_SUMMARY"
fi
