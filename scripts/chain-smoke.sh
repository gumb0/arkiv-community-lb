#!/usr/bin/env bash
# chain-smoke.sh — the Rust chain path's live smoke (tests/chain_live.rs)
# against a throwaway chain on this machine: a dev node sealing every 2 s,
# so the test's lifetimes in seconds mean what they mean on the devnet,
# and the sidecar in front of it with the dev chain's key. Leaves a node
# that was already up alone.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

# First account of the standard test mnemonic, funded by --dev. Public
# knowledge, and the chain it spends on lasts as long as this run.
WRITER_PRIVATE_KEY_FILE="$(mktemp)"
echo 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 > "$WRITER_PRIVATE_KEY_FILE"
export WRITER_PRIVATE_KEY_FILE
export ARKIV_API_KEY=
export DEV_NODE_BLOCK_TIME=2s
# Its own port, so a sidecar someone left running does not take the writes.
export WRITER_PORT=8561
export WRITER_URL="http://127.0.0.1:${WRITER_PORT}/"

started_here=0
sidecar=
cleanup() {
  set +e
  [ -n "$sidecar" ] && kill "$sidecar" 2>/dev/null
  rm -f "$WRITER_PRIVATE_KEY_FILE"
  [ "$started_here" = 1 ] && "$root/scripts/dev-node.sh" stop >/dev/null
}
trap cleanup EXIT

if ! ./scripts/dev-node.sh url >/dev/null 2>&1; then
  ./scripts/dev-node.sh start
  started_here=1
fi
ARKIV_RPC_URL="$(./scripts/dev-node.sh url)"
export ARKIV_RPC_URL

(cd writer && node src/service.ts) &
sidecar=$!
# Up once it answers at all; the startup line names its address and chain.
for _ in $(seq 1 50); do
  curl -s -o /dev/null "$WRITER_URL" && break
  sleep 0.2
done

cargo test -p lb --test chain_live -- --ignored --nocapture
