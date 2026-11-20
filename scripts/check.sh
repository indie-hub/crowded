#!/usr/bin/env bash
# Runs the same gate as .github/workflows/ci.yml, so a change that passes
# here passes CI and the two definitions cannot drift.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo fmt --check
cargo clippy --locked --all-targets -- -D clippy::correctness
cargo test --locked
