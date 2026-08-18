# AGENTS.md

Guidance for coding agents working in this repository.

## What this repository is

The community load balancer for Arkiv RPC nodes: a single Rust service (plus
two sidecars) that bundles community-run provider nodes into one public read
endpoint, discovers providers on the Arkiv marketplace, meters their served
requests, and pays them GLM. Providers are permissionless and economically
adversarial — integrity checking is a first-class concern, not an add-on.

## Vocabulary (use these terms, not their rejected alternatives)

- The whole service is **the LB**. The hot-path component that forwards client
  requests is **the Proxy** — never call the whole service "the proxy", and
  never use "gateway".
- The chain record showing the LB still honours a deal is an **agreement
  record** — not a "lease", and not an "active agreement": **active** and
  **dormant** are the two liveness states of an agreement (tunnel up / down),
  never part of the record's name.
- The in-memory set of provider entries is the **provider pool**; "registry"
  is reserved for the on-chain marketplace records.

## Architecture rules

- **Single instance by design.** No distributed coordination; restart safety is
  the availability story. All durable state lives on-chain — local state is a
  cache.
- **Logic and domain encoding live in Rust; the sidecar speaks SDK, not
  marketplace.** The TS chain-writer sidecar exposes only the SDK's generic
  entity operations (opaque payloads, generic annotations) and holds no
  decisions and no schemas. Chain reads are plain JSON-RPC from Rust — no SDK
  on any read path. One deliberate exception: the **settle CLI is fully
  TypeScript** — a short auditable script that imports the writer module and
  uses viem for Polygon transfers.
- **No shared TS package** with the provider scripts in `arkiv-community-node`:
  the entity schemas are a documented contract, and drift is caught by the test
  rig's round-trip scenarios — not by a shared library.
- **The Rust process holds no keys.** The writer sidecar holds the
  deployment's Arkiv key (env-injected); settle signs with its own keys,
  provided per invocation; the Polygon payout key never exists on the LB
  host's long-running processes.
- Client headers are never forwarded to providers, and a provider's JSON-RPC
  error is an answer, not a failure — passed through, never retried elsewhere.
- Providers behind NAT reach the LB through **frp tunnels**; a tunneled
  provider is a plain `http://127.0.0.1:<port>` URL to the Proxy. The choice,
  the admission design, and the measurements are in `docs/TUNNELING.md` — not
  to be reopened there.

## Deployment

- `compose.yaml` at the repository root is the **LB host stack**: every service
  that runs on the LB box belongs in it (so far only the tunnel server).
- Service images are built locally from pinned upstream releases, checksum
  verified in the Dockerfile — no third-party image in the trust chain.
- Secrets and machine-local configuration stay untracked; the committed
  reference is an `.example` file beside them (`tunnel/frps.toml.example`,
  `.env.example`).

## Testing

- The LB is built as a library so integration tests run the real service
  in-process against fake providers — fast, on every change. Never combine
  tokio's paused time with real sockets; scale durations and poll conditions
  instead.
- The heavier rig tier uses real node containers and the provider tooling from
  `arkiv-community-node`, located at `../node` by default (env-var
  overridable). The rig invokes the same scripts and CLIs that ship — no
  parallel test-only implementations.
- In the TS writer package, `src/` is what ships (the writer module and the
  HTTP service) and `probes/` holds the scripts that are run by hand against a
  live network with a funded key — the smokes and the one-off experiments.
  Both directories are typechecked. **`tests/` is reserved for suites CI can
  run unattended**: a probe spends gas, needs a key, and fails when the chain
  stalls, so it never belongs in a CI job or in `npm test`.
