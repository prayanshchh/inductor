#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

threshold="${TUI_COVERAGE_THRESHOLD:-70}"
mkdir -p coverage

coverage_output="$(bun test --coverage packages/tui/test 2>&1)"
printf '%s\n' "$coverage_output" | tee coverage/tui-coverage.txt

line_coverage="$({ printf '%s\n' "$coverage_output" | awk -F'|' '/^All files[[:space:]]*\|/ { gsub(/^[[:space:]]+|[[:space:]]+$/, "", $3); print $3; exit }'; } || true)"

if [[ -z "$line_coverage" ]]; then
  echo "Could not determine TUI line coverage." >&2
  exit 1
fi

if ! awk -v actual="$line_coverage" -v threshold="$threshold" 'BEGIN { exit !(actual + 0 >= threshold + 0) }'; then
  echo "TUI line coverage ${line_coverage}% is below required ${threshold}%." >&2
  exit 1
fi

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "## TUI coverage"
    echo
    echo "Threshold: ${threshold}% lines"
    echo
    echo "Reported line coverage: ${line_coverage}%"
    echo
    echo '```text'
    cat coverage/tui-coverage.txt
    echo '```'
  } >> "$GITHUB_STEP_SUMMARY"
fi