#!/usr/bin/env bash
# dev-node.sh — a throwaway Arkiv chain on this machine, for tests.
set -euo pipefail

usage(){
  cat <<'EOF'
Usage: ./scripts/dev-node.sh start|stop|url|chain-id

Runs the upstream arkiv-reth-dev image: one node in --dev mode sealing
blocks on a timer, so expiry and other time-based behaviour advance without
traffic. Point ARKIV_RPC_URL at it to run probes and rig tests without a
devnet, and without spending devnet gas.

  start      run the container and wait until it answers; prints the RPC URL
  stop       remove the container
  url        print the RPC URL (fails if nothing is running)
  chain-id   print the chain id reported by the running node

start is safe to repeat: an already-running node is reported, not restarted.
State lives in the container only, so stop discards the whole chain.

Blocks are sealed every 2 s by default, matching the devnet. Faster sealing
is available but not trustworthy: at 250 ms the node executes a transaction
twice in consecutive blocks now and then (probes/create-dup.ts reproduces
it), which mints phantom entities and breaks anything that counts or
deletes them. A lifetime asked for in seconds assumes 2 s blocks (the SDK
converts durations at that rate); ask in blocks if you change the interval.

Environment:
  DEV_NODE_BLOCK_TIME  seal interval (default 2s; see the note above)
  DEV_NODE_PORT    host port for JSON-RPC (default 8645)
  DEV_NODE_IMAGE   image (default ghcr.io/arkiv-network/arkiv-reth-dev:latest)
  DEV_NODE_NAME    container name (default arkiv-dev-node)

The image lives in a credentialed registry, so `docker login ghcr.io` with a
token that can read packages is a prerequisite.
EOF
}

PORT="${DEV_NODE_PORT:-8645}"
# v0.1.0 is the release cheesecake's own EL is pinned to, so the probes
# measure the engine the devnet runs. The digest holds the tag still.
IMAGE="${DEV_NODE_IMAGE:-ghcr.io/arkiv-network/arkiv-reth-dev:v0.1.0@sha256:7e93bcbd5b16ccc6552ff466ad3431578cfd2186e72b4a4af7fe0fb683c8b3ed}"
NAME="${DEV_NODE_NAME:-arkiv-dev-node}"
BLOCK_TIME="${DEV_NODE_BLOCK_TIME:-2s}"
URL="http://127.0.0.1:${PORT}"
READY_TIMEOUT=60

rpc(){ # rpc <method> — one JSON-RPC call, result field only
  curl -sf -m 5 -X POST -H 'Content-Type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":[]}" "$URL" \
    | grep -o '"result":"[^"]*"' | cut -d'"' -f4
}

running(){ [ -n "$(docker ps -q -f "name=^${NAME}$")" ]; }

start(){
  if running; then
    printf 'already running: %s\n' "$URL"
    return 0
  fi
  # A stopped container of the same name would block the run.
  docker rm -f "$NAME" >/dev/null 2>&1 || true

  # Everything but the block time is the image's own default command.
  if ! docker run -d --name "$NAME" -p "${PORT}:8545" "$IMAGE" \
      node --dev --dev.block-time "$BLOCK_TIME" \
      --http --http.addr 0.0.0.0 --http.api eth,net,web3,txpool \
      --datadir /data >/dev/null; then
    printf 'could not start %s\n' "$IMAGE" >&2
    printf 'if the pull was denied: docker login ghcr.io -u <github-user>\n' >&2
    exit 1
  fi

  local waited=0
  until chain_id=$(rpc eth_chainId 2>/dev/null) && [ -n "$chain_id" ]; do
    if [ "$waited" -ge "$READY_TIMEOUT" ]; then
      printf 'no answer on %s after %ss. Container log:\n' "$URL" "$READY_TIMEOUT" >&2
      docker logs --tail 20 "$NAME" >&2
      exit 1
    fi
    sleep 1
    waited=$((waited + 1))
  done

  printf 'dev node on %s (chain id %d, %s blocks, ready in %ss)\n' \
    "$URL" "$((chain_id))" "$BLOCK_TIME" "$waited"
}

case "${1:-}" in
  start)    start;;
  stop)     docker rm -f "$NAME" >/dev/null 2>&1 || true; printf 'stopped %s\n' "$NAME";;
  url)      running || { printf '%s is not running\n' "$NAME" >&2; exit 1; }; printf '%s\n' "$URL";;
  chain-id) running || { printf '%s is not running\n' "$NAME" >&2; exit 1; }
            printf '%d\n' "$(($(rpc eth_chainId)))";;
  -h|--help|"") usage;;
  *) usage >&2; printf 'dev-node.sh: unknown command: %s\n' "$1" >&2; exit 2;;
esac
