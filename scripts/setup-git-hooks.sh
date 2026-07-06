#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

git rev-parse --is-inside-work-tree >/dev/null
git config core.hooksPath .githooks

echo "Configured Git hooks to use .githooks"
echo "pre-commit  -> bash scripts/checks/pre-commit.sh"
echo "pre-push    -> bash scripts/checks/pre-push.sh"