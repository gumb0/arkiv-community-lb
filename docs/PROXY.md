# The load balancer — architecture

**Scope:** how the LB serves requests and judges provider health, and the
reasoning behind the shape. The client-visible contract is
[ENDPOINT.md](ENDPOINT.md); the tunnel transport is
[TUNNELING.md](TUNNELING.md). Vocabulary: the whole service is the LB;
the hot-path component that forwards requests is the Proxy.

## Two listeners

The LB serves two HTTP listeners with strictly separate surfaces: the
**public** endpoint (JSON-RPC, permissive CORS) and the **admin** API
(`/health`, `/nodes`, `/node/{id}`) — loopback by default,
unauthenticated, so widening its bind is a firewall decision.
Nothing from one listener exists on the other.

## The provider pool

Providers live in a pool of long-lived entries built at startup from the
configuration; a tunneled provider is simply a loopback URL. All mutable
state on an entry — eligibility, the health counter, request counters — is
atomic, so the hot path reads it without locks. Selection is plain round
robin over eligible entries; when a full lap finds none, the client gets
the no-healthy-provider error.

## Forwarding and the credential boundary

The Proxy forwards the request body untouched and builds fresh headers —
nothing of the client's (auth, cookies, IPs) ever reaches a provider,
and nothing of a provider's operational headers leaks back. The response
is relayed within a size cap; a provider's JSON-RPC error is an answer,
returned to the client, never retried elsewhere.

Transport failures are retried on the next provider within a small
budget, all under one request deadline (each attempt gets the smaller of
the attempt timeout and the time remaining). This is failover: in the
short window when a provider is already dead but not yet quarantined,
the client's request moves to the next provider and the client never
notices. The budget is the other half: when the request itself is the
problem, it can hit only a few providers before it fails, instead of
walking the whole fleet. Retrying blindly is safe
only because every method the endpoint admits is replay-safe — the
denylist excludes stateful methods, and a repeated raw transaction
deduplicates by hash.

## Health

One background loop probes every provider on a fixed interval with a
cheap JSON-RPC call; probe outcomes and live traffic outcomes feed the
same per-provider health counter. A run of consecutive failures quarantines; a
run of consecutive successes readmits. There are no weights and no
scores — eligibility is binary, and no single result can ever flip it,
thanks to the minimum success/failure run length (`flip_after` in the
config) — which is what makes racing, stale results harmless. Quarantined providers keep being probed,
with backoff once a provider looks abandoned; recovery is automatic.

Providers are **born ineligible**: a new entry serves nothing until its
first probes pass, and the very first contact verifies the chain
identity — a node on the wrong chain never serves a single request, and
is flagged as misconfigured rather than failing. The chain check repeats
periodically, so a provider switched to another chain after admission is
caught the same way.

Chain height **lag** is judged against the official reference RPC, per provider: a
provider ahead of the reference or within a tolerance behind it is
healthy; one behind beyond the tolerance accumulates failures like any
other fault. The reference's height is sampled alongside the probes.
When the reference is unreachable there are simply no lag verdicts — a
reference outage must never fault providers. This shape is deliberately
stall-tolerant: when the chain itself halts (devnets do), the reference
halts with it, differences stay near zero, and a uniformly quiet fleet
remains healthy instead of being mass-quarantined for the network's
pause.

## Restart safety

The LB is a single instance by design. Durable state lives on-chain;
everything in memory is a cache rebuilt at startup, and the health
machinery re-learns provider state within a probe cycle. Stopping and
starting the LB is therefore always safe — availability is provided by
a fast restart.
