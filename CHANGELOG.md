# Changelog

Notable changes per release, newest first. The LB box deploys a tag:
`git checkout <tag> && docker compose up -d --build`
([docs/RUNBOOK.md](docs/RUNBOOK.md)).

## v0.2.0

The marketplace: a provider joins by posting an offer on Arkiv, serves
through a tunnel its own signature admits, is counted on chain, and is
paid in GLM for the requests it answered.

- Marketplace agent (`crates/lb/src/marketplace/`, the `[marketplace]`
  section in config): writes the LB listing on Arkiv, discovers offers
  against it, accepts them into agreements with a tunnel port each,
  refreshes the agreements of the eligible providers hourly, and
  reloads all of it from the chain at start. The flow is
  [docs/MARKETPLACE.md](docs/MARKETPLACE.md); the records every codebase
  encodes against are [docs/ENTITIES.md](docs/ENTITIES.md).
- Tunnel admission by signature: the tunnel server posts every login
  and proxy registration to `/admission` on the admin listener, and a
  client is let in only if its token is the agreement provider's
  signature over the agreement id and it asks for the assigned port.
  The shared token is gone ([docs/TUNNELING.md](docs/TUNNELING.md)).
- Counters on chain: one counter record per agreement per settlement
  period, written at every flush, closed when its period ends with a
  successor opened at zero, and the count of a provider whose agreement
  ended written one last time. A deliberate stop writes the counts
  before the process exits. New fields `settlement_period` and
  `flush_interval`.
- Settle CLI (`settle/`): pays every closed counter record that has no
  receipt, from chain state alone: one GLM transfer per provider on the
  payout chain, then one permanent receipt per record. A run rehearses
  unless given `--pay`, needs no key to rehearse, reads its balances
  before it signs anything, stops at the first provider it could not
  pay, and says plainly which state needs a person. The runbook's
  "Paying the providers" section covers running it.
- The chain path: reads are plain JSON-RPC from Rust through the
  reference endpoint; writes go through the chain-writer sidecar,
  which joins the host stack. The LB's writer client sends a batch as
  one transaction, and splits it in halves when the node refuses it as
  too large.
- `/nodes` shows each marketplace provider's source and agreement id.
- Graceful shutdown: SIGTERM as well as Ctrl-C, requests in flight
  finish, and the stop grace covers the final flush.
- The dev node for tests moves to arkiv-reth-dev v0.2.0, and the chain
  smoke runs in CI against it.

Upgrade notes: three files on the box change. `config.toml` gains the
`[marketplace]` section (`wei_per_call` and `tunnel_server` are
required, everything else has a default; see `config.example.toml`).
`.env` names the writer's key by file, `WRITER_PRIVATE_KEY_FILE`,
instead of holding it. `frps.toml` loses `auth.token` and gains the
`[[httpPlugins]]` admission block pointing at the admin listener (see
`tunnel/frps.example.toml`); a provider tunnel configured by hand with
the old shared token no longer connects, and joins through the
marketplace instead. `docker compose up -d --build` starts the sidecar
with the rest. A restart now waits up to `stop_grace_period`, 200 s,
for the counts to be written.

## v0.1.0

First release: the proxy core over a configured provider list.

- Round-robin forwarding with failover under one request deadline; a
  provider's JSON-RPC error is an answer, never retried elsewhere.
- Health probing: a shared streak per provider fed by probes and
  traffic, quarantine and automatic readmission, chain identity check,
  chain head lag against the reference RPC, backoff from dead
  providers. Providers are born ineligible.
- Method denylist in code; request and response size caps; LB-generated
  errors in the −32050…−32054 range ([docs/ENDPOINT.md](docs/ENDPOINT.md)).
- Admin API on loopback: `/health` with readiness, `/nodes` pool view,
  `POST /node/{id}` forward to one provider regardless of eligibility.
- Chain-writer sidecar (`writer/`): entity writes over the Arkiv TS SDK
  behind a small HTTP service.
- Host stack: multi-stage Dockerfile, `compose.yaml` with the LB and
  the tunnel server, runbook.
- Test rig (`crates/rig/`): scenarios over the shipped binary and real
  dev-node containers, plus a standalone load generator.
