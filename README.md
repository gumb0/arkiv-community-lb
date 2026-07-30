# Arkiv Community Load Balancer

The load balancer behind `community.rpc.arkiv.network`: one public read
endpoint over community-run [Arkiv](https://arkiv.network) RPC nodes.
Providers are discovered through an on-chain marketplace, paid in GLM for the
requests they serve, and checked for integrity — the nodes are permissionless,
not operator-chosen.

**Status: under construction.** Nothing here is usable yet.

## What this will contain

- The LB service (Rust): health checking, failover, load balancing across
  provider nodes, admin API.
- A marketplace agent: discovers provider offers on Arkiv and keeps agreements
  alive on-chain.
- A chain-writer sidecar (TypeScript, official Arkiv SDK) for entity writes.
- A settle CLI: computes per-period payouts from on-chain records and pays GLM
  on Polygon.
- A test rig that drives the whole system — real node containers, fault
  injection, end-to-end demo scenarios.

The node-operator side lives in the companion repo,
[arkiv-community-node](https://github.com/gumb0/arkiv-community-node).

## License

[Apache-2.0](LICENSE)
