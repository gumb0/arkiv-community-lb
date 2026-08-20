#!/usr/bin/env bash
# run-probes.sh [npm-script...] — probes against a throwaway chain on this
# machine. Defaults to `probes`, which is all of them: what to run after
# moving the SDK version or the node image, since those are what most of the
# probes measure. CI names the two that follow our own code instead.
set -euo pipefail
cd "$(dirname "$0")/.."

scripts=("$@")
[ "${#scripts[@]}" -eq 0 ] && scripts=(probes)

# First account of the standard test mnemonic, funded by --dev. Public
# knowledge, and the chain it spends on lasts as long as this run.
export WRITER_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
export ARKIV_API_KEY=

started_here=0
if ! ./scripts/dev-node.sh url >/dev/null 2>&1; then
  ./scripts/dev-node.sh start
  started_here=1
fi
ARKIV_RPC_URL="$(./scripts/dev-node.sh url)"
export ARKIV_RPC_URL
# Leave a node that was already up alone: it is someone's, not ours.
trap '[ "$started_here" = 1 ] && ./scripts/dev-node.sh stop >/dev/null' EXIT

cd writer
for script in "${scripts[@]}"; do
  npm run "$script"
done
