# Arkiv Community Load Balancer

The load balancer behind `community.rpc.arkiv.network`: one public read
endpoint over community-run [Arkiv](https://arkiv.network) RPC nodes.
Providers are discovered through an on-chain marketplace, paid in GLM for the
requests they serve, and checked for integrity — the nodes are permissionless,
not operator-chosen.

**Status: under construction.**

## What is here

- **The LB service** (`crates/lb/`): round-robin load balancing with
  failover over a configured provider list. Health probing quarantines
  and readmits providers automatically, checks chain identity and
  chain head lag, and backs off from dead providers. An admin API
  serves health and pool views on `/health` and `/nodes`, plus pinned
  forwarding to one provider on `/node/{id}`, including providers
  outside rotation. To run it locally: copy `config.example.toml` to
  `config.toml`, list your providers, `cargo run --bin arkiv-lb`.
  The client contract is [docs/ENDPOINT.md](docs/ENDPOINT.md); the
  architecture note is [docs/PROXY.md](docs/PROXY.md).
- **Chain-writer sidecar** (`writer/`): entity writes over the official
  Arkiv TS SDK, behind a small HTTP service — wire format and error
  contract in [docs/CHAIN_WRITER.md](docs/CHAIN_WRITER.md). Unit-tested
  without a chain; live smoke tests run against a throwaway local node
  (`scripts/dev-node.sh`) in CI.
- **The marketplace agent** (`crates/lb/src/marketplace/`): with the
  `[marketplace]` section configured, the LB writes its listing on
  Arkiv, discovers provider offers against it, accepts them into
  agreements with a tunnel port each, keeps the agreements of the
  providers that are healthy alive with an hourly refresh, admits each
  provider's tunnel by a signature over its agreement id, and writes
  the requests each provider served into its counter record on Arkiv,
  once a day by default, closing one record per settlement period,
  which is what settle pays. The flow is
  [docs/MARKETPLACE.md](docs/MARKETPLACE.md); the records every
  codebase encodes against are [docs/ENTITIES.md](docs/ENTITIES.md).
  Tested in-process over a fake chain.
- **The settle CLI** (`settle/`): pays the providers. It reads the
  closed counter records and the receipts of earlier runs from Arkiv,
  sends one GLM transfer per provider on the payout chain, and writes
  one permanent receipt per record it paid, which is what keeps a
  record from being paid twice. A run rehearses unless told to pay,
  and rehearsing needs no key at all. It talks to no part of the load
  balancer, so it runs anywhere with access to both chains. How to run
  it is in [docs/RUNBOOK.md](docs/RUNBOOK.md).
- **The host stack** (`compose.yaml`, `Dockerfile`, `tunnel/`): the LB
  and the tunnel server for NAT'd providers, deployed together —
  operations in [docs/RUNBOOK.md](docs/RUNBOOK.md), the tunnel decision
  and measurements in [docs/TUNNELING.md](docs/TUNNELING.md).
- **The test rig** (`crates/rig/`): scenarios that drive the shipped
  LB binary over real dev-node containers — boot, load distribution,
  method denial, admin forward to a quarantined node, kill and
  recovery under load. `rig all` runs every scenario; `rig load` is a
  standalone load generator pointable at any endpoint. Runs locally
  and as an on-demand CI workflow. The testing approach across the
  repository is [docs/TESTING.md](docs/TESTING.md).

The node-operator side lives in the companion repo,
[arkiv-community-node](https://github.com/gumb0/arkiv-community-node).

## Known limitations

Deliberate for the first version, not oversights:

- One load balancer instance, no redundancy: a restart is a short outage
  for everyone using the endpoint.
- Method filtering is a text search over the request body rather than a
  JSON parse, so it can refuse a request that merely mentions a refused
  name ([docs/ENDPOINT.md](docs/ENDPOINT.md)) and cannot count requests
  per method.
- An error the load balancer itself generates for a batch request is a
  single JSON-RPC error object with `id: null`, not a response array
  ([docs/ENDPOINT.md](docs/ENDPOINT.md)); batches a node answers arrive
  as the node's array.
- Nothing limits how many requests a client may send, or how many run at
  once, so memory use scales with concurrency times the response cap.
- Health is binary and probes decide it: a provider that answers its
  probes within the probe timeout keeps its full share of traffic,
  however slow its answers; one consistently slower than the probe
  timeout leaves rotation entirely.
- A provider's chain head lag is measured against a reference endpoint,
  so while that endpoint is unreachable, one provider falling behind its
  peers goes unnoticed.
- Failover retries draw from the shared round-robin cursor rather than
  remembering which providers a request already tried. So at the moment
  a provider dies, a small share of the requests in flight can spend
  their whole retry budget on it and fail, even though a healthy
  provider was available. It takes heavily concurrent traffic to hit,
  and quarantine closes the window after a few failures.
- A settlement run that pays a provider and then cannot write that
  provider's receipts leaves records that still look unpaid, and there
  is no way to write those receipts afterwards. The run says so, names
  the transfer it made and stops; what an operator can do from there
  is in [docs/RUNBOOK.md](docs/RUNBOOK.md).

## License

[Apache-2.0](LICENSE)
