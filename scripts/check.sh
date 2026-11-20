#!/usr/bin/env bash
# Runs the same gate as .github/workflows/ci.yml, so a change that passes
# here passes CI and the two definitions cannot drift.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo fmt --check
cargo clippy --locked --all-targets -- -D clippy::correctness
cargo test --locked

if ! command -v node >/dev/null 2>&1; then
  echo "check: node is missing; the Claude mod delivery tests cannot run" >&2
  exit 1
fi
node --test src/claude_mod/deliver.test.mjs
