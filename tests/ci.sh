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

# The image builds the lb crate alone, where no sibling crate's features
# are unified in; a feature the crate needs but does not name fails
# there and nowhere above.
note "lb crate on its own"
cargo check -p lb --locked

note "all green"
