#!/usr/bin/env bash
# relay.sh — `rig relay` in front of a provider's node, on the node's box.
set -euo pipefail

usage(){
  cat <<'USAGE'
Usage: ./relay.sh honest|lie|stop

Runs `rig relay` as a container on the compose network of the node
stack, in front of its execution client, so a tunnel client pointed at
the relay serves the node through it. Run it from the node checkout.

  honest   (re)start the relay passing everything through
  lie      (re)start it adding a byte to every entity answered to a
           query by key; the probes still see an honest node
  stop     remove the relay

Restarting the relay leaves the tunnel up: the tunnel client dials its
local target per incoming connection.

Environment:
  RIG_BINARY   the rig binary (default ~/rig), built for Debian 12
  RELAY_NAME   container name, the host name on the network (default rig-relay)
  RELAY_PORT   port it listens on in the network (default 8546)
USAGE
}

BINARY="${RIG_BINARY:-$HOME/rig}"
NAME="${RELAY_NAME:-rig-relay}"
PORT="${RELAY_PORT:-8546}"
# Has the system's CA certificates, which the relay's HTTP client needs
# to start, and Debian 12's glibc.
IMAGE="gcr.io/distroless/cc-debian12"

network(){
  local tunnel
  tunnel="$(docker compose ps -q tunnel)"
  if [ -z "$tunnel" ]; then
    echo "no tunnel container here: run this from the node checkout, with the tunnel up" >&2
    exit 1
  fi
  docker inspect "$tunnel" --format '{{range $k, $_ := .NetworkSettings.Networks}}{{$k}}{{end}}'
}

start(){
  [ -x "$BINARY" ] || { echo "no rig binary at $BINARY" >&2; exit 1; }
  local net
  net="$(network)"
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" --network "$net" -v "$BINARY":/rig:ro "$IMAGE" \
    /rig relay --listen "0.0.0.0:$PORT" --upstream http://execution:8545 "$@" >/dev/null
  sleep 1
  docker logs "$NAME"
}

case "${1:-}" in
  honest) start ;;
  lie) start --lie entity ;;
  stop) docker rm -f "$NAME" >/dev/null && echo "stopped $NAME" ;;
  *) usage; exit 2 ;;
esac
