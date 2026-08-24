#!/usr/bin/env bash
# The Rust CI gate, runnable locally: exactly what the workflow runs.
# (The TS writer has its own job; see .github/workflows/ci.yml.)
set -euo pipefail
cd "$(dirname "$0")"

note(){ printf '== %s\n' "$*"; }

note "format"
cargo fmt --all -- --check

note "clippy, warnings are errors"
cargo clippy --all-targets --all-features -- -D warnings

note "tests"
cargo test --all-targets

note "all green"
