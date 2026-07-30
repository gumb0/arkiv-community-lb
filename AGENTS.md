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
- The marketplace record showing the LB still honours a deal is an **active
  agreement** — not a "lease".

## Architecture rules

- **Single instance by design.** No distributed coordination; restart safety is
  the availability story. All durable state lives on-chain — local state is a
  cache.
- **Logic in Rust, encoding in TypeScript.** The TS chain-writer sidecar only
  encodes and submits entity writes via the official Arkiv SDK; it holds no
  decisions. Chain reads are plain JSON-RPC from Rust.
- **No shared TS package** with the provider scripts in `arkiv-community-node`:
  the entity schemas are a documented contract, and drift is caught by the test
  rig's round-trip scenarios — not by a shared library.
- **The LB never holds the payout key.** Settlement is a separate CLI run;
  the LB's own key can write entities, nothing more.
- Client headers are never forwarded to providers, and a provider's JSON-RPC
  error is an answer, not a failure — passed through, never retried elsewhere.

## Testing

- The LB is built as a library so integration tests run the real service
  in-process against fake providers — fast, on every change. Never combine
  tokio's paused time with real sockets; scale durations and poll conditions
  instead.
- The heavier rig tier uses real node containers and the provider tooling from
  `arkiv-community-node`, located at `../node` by default (env-var
  overridable). The rig invokes the same scripts and CLIs that ship — no
  parallel test-only implementations.
