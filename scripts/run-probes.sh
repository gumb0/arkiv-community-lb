#!/usr/bin/env bash
# run-probes.sh [npm-script...] — probes against a throwaway chain on this
# machine. Defaults to the writer's `probes` (the two smokes and the probes
# that measure the pinned SDK and node image) and settle's pay probe, which
# is what to run after moving either pin; CI runs the smokes. A script
# named `settle:<name>` runs in the settle package.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

scripts=("$@")
[ "${#scripts[@]}" -eq 0 ] && scripts=(probes settle:pay-probe)

# First account of the standard test mnemonic, funded by --dev. Public
# knowledge, and the chain it spends on lasts as long as this run. The
# probes read the key from a file, as the sidecar does.
WRITER_PRIVATE_KEY_FILE="$(mktemp)"
echo 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 > "$WRITER_PRIVATE_KEY_FILE"
export WRITER_PRIVATE_KEY_FILE
export ARKIV_API_KEY=

started_here=0
if ! ./scripts/dev-node.sh url >/dev/null 2>&1; then
  ./scripts/dev-node.sh start
  started_here=1
fi
ARKIV_RPC_URL="$(./scripts/dev-node.sh url)"
export ARKIV_RPC_URL
# Leave a node that was already up alone: it is someone's, not ours. The
# absolute path matters: the trap fires after the cd below.
trap 'rm -f "$WRITER_PRIVATE_KEY_FILE"; [ "$started_here" = 1 ] && "$root/scripts/dev-node.sh" stop >/dev/null' EXIT

for script in "${scripts[@]}"; do
  case "$script" in
    # Settle takes its key from the environment, the way its CLI does.
    settle:*) (cd settle && SETTLE_PRIVATE_KEY="$(cat "$WRITER_PRIVATE_KEY_FILE")" npm run "${script#settle:}") ;;
    *) (cd writer && npm run "$script") ;;
  esac
done
