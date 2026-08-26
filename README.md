# Arkiv Community Load Balancer

The load balancer behind `community.rpc.arkiv.network`: one public read
endpoint over community-run [Arkiv](https://arkiv.network) RPC nodes.
Providers are discovered through an on-chain marketplace, paid in GLM for the
requests they serve, and checked for integrity — the nodes are permissionless,
not operator-chosen.

**Status: under construction.** The pieces below exist; the LB service
itself does not yet.

## What is here

- **Chain-writer sidecar** (`writer/`): entity writes over the official
  Arkiv TS SDK, behind a small HTTP service — wire format and error
  contract in [docs/CHAIN_WRITER.md](docs/CHAIN_WRITER.md). Unit-tested
  without a chain; live smoke tests run against a throwaway local node
  (`scripts/dev-node.sh`) in CI.
- **Tunnel server stack** (`compose.yaml`, `tunnel/`): how NAT'd providers
  reach the LB — decision and measurements in
  [docs/TUNNELING.md](docs/TUNNELING.md).

## Still to come

- The LB service (Rust): health checking, failover, load balancing across
  provider nodes, admin API.
- A marketplace agent: discovers provider offers on Arkiv and keeps agreements
  alive on-chain.
- A settle CLI: computes per-period payouts from on-chain records and pays GLM
  on Polygon.
- A test rig that drives the whole system — real node containers, fault
  injection, end-to-end demo scenarios.

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
- Nothing limits how many requests a client may send, or how many run at
  once, so memory use scales with concurrency times the response cap.

## License

[Apache-2.0](LICENSE)
