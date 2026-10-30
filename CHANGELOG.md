# Changelog

Notable changes per release, newest first. The LB box deploys a tag:
`git checkout <tag> && docker compose up -d --build`
([docs/RUNBOOK.md](docs/RUNBOOK.md)).

## v0.5.0

The move to the rabdi testnet and its node line, arkiv-reth v1.0.0.
This release does not run on an earlier network: a node's entity is
now its typed attributes, with no payload field and no content type.

- A record's JSON payload travels in a `bytes` attribute named
  `payload`, never indexed. The records keep their fields and `v`
  stays 1 (docs/ENTITIES.md). `updatedAt` is selected with every read.
- The writer sidecar and settle are on `@arkiv-network/sdk` 1.0.0. The
  sidecar's wire format changes: attributes alone, with an `indexed`
  flag and a `bytes` type in `0x` hex; no `payload`, no `contentType`,
  no `bytes32`; `expires` is `expiration`, and it has no `"permanent"`
  form (docs/CHAIN_WRITER.md).
- A query page is 100 rows (was 200), and `max_providers` defaults to
  90, which leaves room in the page for the duplicate records the LB
  tolerates for a while. Reading through the cursor instead is #69.
- The chain caps a lifetime at 7,884,000 blocks, about 182 days, and
  refuses a permanent expiry. A lifetime in the config above the cap
  is refused at startup (#72 is about the cap being a constant).
- Settle's receipts live the chain's maximum lifetime, which outlasts
  the record each one pays, so they are no longer permanent. Settle
  pages by offset and starts a walk over if the match count changes.
- **The integrity check is the block check alone.** Nodes serve state
  at the chain head only, and the entity check read at the finalized
  height, so it is switched off in the code until it is reworked. A
  provider serving wrong entities behind right blocks is not caught
  (docs/INTEGRITY.md). The rig's `wrong-entity` scenario is left out
  of `rig all` until then.
- `gas_warn_below` defaults to 0.1 GLM (was 0.02): a batch needs more
  in the key than it costs. RUNBOOK's costs are measured again, since
  a write is now charged for compute and for storage by lifetime.

Upgrade notes: a new chain, so a new start. In `.env`, point
`ARKIV_RPC_URL` and `ARKIV_API_KEY` at the network. In `config.toml`,
set `health.chain_id` and check `tunnel_server`, which the listing
publishes. Fund the LB and settle keys on the new chain, delete the
hourly stats (`docker compose down`, then `docker volume rm
arkiv-community-lb_lb-state`), and build: `docker compose up -d
--build`. Settle what the previous chain owes with the previous
release first, if it matters: this settle cannot read it.

## v0.4.0

The load test and what it changed: the open-files limit, the attempt
timeout, a slow provider no longer quarantined for timeouts, a 2xx
without JSON failing over; hourly stats for a dashboard; the integrity
check pinned at the finalized height and hardened against oversized
answers; settle signing its transfers; admission by the agreement's
proxy name; the CA bundles after eMudhra.

- The integrity check compares an entity's attributes by name, not in
  the order the node rendered them, so a client version that renders
  them in another order on one side is not a divergence that would
  quarantine every honest provider at once (#49).
- A provider that is slow but answering is no longer quarantined by
  attempts that time out on it: a timeout ticks its health only when
  the provider answered nothing, request or probe, while the attempt
  was waiting. Under a load the nodes could not keep up with, every
  provider was quarantined within one attempt timeout, readmitted by
  its probes and quarantined again, and clients got `no healthy
  provider` in between (#62).
- Tunnel admission requires the proxy's name to be the one for the
  agreement's port, `node-rpc-<port>`, as the tooling renders it. A
  client under another provider's name was admitted on its own port
  and held that name, so the other provider's reconnects were refused
  as duplicates (#67). The rejection names the fix.
- Settle signs each transfer itself before broadcasting it, so the
  transaction's hash is known before it leaves. A send whose reply is
  lost is now `MAY HAVE PAID` with that hash, for a person to check;
  it used to read as a failure that wrote nothing, and the next run
  paid the same records again.
- The CA bundles no longer trust the compromised eMudhra intermediates
  (October 2026): the writer's Node image moves to 22.23.4, and the
  LB's TLS path validation (`rustls-webpki` 0.103.16) rejects chains
  through those keys whatever the system bundle holds. The LB image
  takes its bundle from Debian at build time, which Docker caches:
  build it once with `docker compose build --no-cache lb`.
- A 2xx from a provider with an empty body or one that is not JSON, a
  204 or an HTML page from something in front of the node, is no
  longer passed to the client and counted as served: it fails over
  like a non-2xx status, with the same health tick. The check is the
  first byte, not a parse; what a well-formed body says is the
  integrity check's business.
- The integrity checker reads a provider's answers up to 4 MiB and
  refuses the rest; an answer over that is a divergence, since the
  reference's answer to the same read is a small fraction of it. A
  provider could answer a check with a valid envelope of any size and
  take the LB down with it, round after round.
- The integrity check reads the entity at the finalized height on both
  sides: the provider's read is pinned there like the reference's. The
  provider used to be asked with no block parameter, and an answer at
  a block the reference did not have yet could not be judged, so a
  provider answering ahead of the reference every time was never
  judged at all. There is no stale verdict for an entity answered far
  back any more: a provider with the finalized block is compared at
  it, and lag is the health check's business
  ([docs/INTEGRITY.md](docs/INTEGRITY.md)). The round no longer reads
  the reference's head: the key is picked among those whose entity
  exists at the finalized height, one metered call less a round.
- Hourly stats for a dashboard: `GET /stats` on the admin listener
  serves, per provider and per hour, the requests answered and the
  attempts that produced no answer, and per hour the requests the LB
  answered itself, for the last 31 days ([docs/STATS.md](docs/STATS.md)).
  The rings are saved to a file (`stats.file`, in a named volume in
  the stack) at every hour and at a stop, so a restart keeps them;
  only a crash loses the hour in progress. `/nodes` shows
  `served_period` and `served_total` where it showed `served`.
- The startup line names the version.
- The stack raises the open-files limit of the LB and the tunnel
  server to 65536. Docker's default of 1024 ran out at about 500
  client requests in flight: the LB could reach no provider, counted
  the failures against every provider's health, and stopped accepting
  connections.
- `attempt_timeout` defaults to 15 s (was 10 s), half the request
  timeout. Under load a heavy page from a provider on a slow uplink
  takes over ten seconds; at 10 s the attempt failed over and counted
  against the provider's health while it was answering.
- `rig load` takes several concurrency steps in one run, with a line
  every 10 s and a table of the steps at the end, counts failures by
  kind, and takes the query's filter as `--query`. The load test it
  ran, its results and how to run it again are in
  [docs/LOAD_TEST.md](docs/LOAD_TEST.md).

## v0.3.0

Integrity: a provider serving wrong data is found and taken out of
rotation, with both answers logged side by side; the handover
package; the two design notes.

- Integrity checks (`crates/lb/src/integrity.rs`, the `[integrity]`
  section in config): once a round, every provider's block at the
  finalized height and one entity read by key are compared with the
  reference, the entity pinned to the block the provider answered at.
  A difference that survives a second look is a divergence; the
  provider leaves rotation, and only a later passing round brings it
  back, never its probes. Stale is left to the lag check. The verdict
  and its height show in `/nodes`; the evidence event carries both
  answers whole ([docs/INTEGRITY.md](docs/INTEGRITY.md)). Without the
  section, providers are judged by their probes alone.
- A newcomer is checked at admission: once its admitted tunnel passes
  its probes, the integrity check runs at that moment and the
  agreement record is extended on the spot, not at the next hourly
  refresh; a provider found serving wrong data keeps its first
  lifetime and nothing more.
- An accepted agreement's first lifetime is `offer_max_lifetime`, so
  a record ends at or after its offer and an offer is accepted once;
  `accept_window` is gone, and `offer_max_lifetime` defaults to one
  day, the provider tooling's offer life. A lifetime lowered in config
  no longer fails every refresh: a record that already expires later
  is left out until it is within the new value.
- A close whose answer was lost is found closed at the next flush, not
  written again; a flush that landed through a crash is read back and
  nothing is counted twice.
- The LB's own reads carry a random request id, so a provider cannot
  tell them from client traffic by it. The eligibility flip event is
  named `eligibility flip`; the reference-unanswered warning carries
  the reason.
- The rig: a relay that can lie (`rig relay --lie entity|block|
  frozen-head`, and `scripts/relay.sh` for a live node), and five
  scenarios on it: `wrong-entity`, `wrong-block`, `frozen-head`,
  `reference-down`, and `offer-accepted`, which runs the provider
  tooling's own offer and tunnel token through the LB
  ([docs/TESTING.md](docs/TESTING.md)).
- The handover package: the runbook gains what the stack leaves to
  the deployment (TLS, DNS, rate limiting), what normal operation
  costs, releases and updates, the changes that reach every provider,
  settle from a cron, and the keys in one table
  ([docs/RUNBOOK.md](docs/RUNBOOK.md)); every known limitation in one
  document ([docs/LIMITATIONS.md](docs/LIMITATIONS.md)); an index of
  the documents ([docs/README.md](docs/README.md)).
- Two design notes: [docs/MISBEHAVING_NODES.md](docs/MISBEHAVING_NODES.md),
  the full problem of providers that serve wrong data and the steps
  from here, and [docs/MARKETPLACE_FUTURE.md](docs/MARKETPLACE_FUTURE.md),
  what an open marketplace needs beyond this version.

Upgrade notes: `config.toml` changes in two places. `accept_window`
must be removed, or the LB refuses to start on the unknown key; and
the `[integrity]` section turns the checks on (`interval`,
`confirm_after`, both with defaults), and needs the reference endpoint
in `.env`. `offer_max_lifetime` now defaults to one day; set it back
to two if the old value is wanted. Records on the chain are unchanged.

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
