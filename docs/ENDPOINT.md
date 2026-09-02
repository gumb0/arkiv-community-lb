# The community endpoint — client contract

**Scope:** what a client of the community RPC endpoint can rely on: the
served methods, the denied ones, the errors the load balancer itself
returns, and the size limits. The endpoint is under construction; this
is the contract it is built against.

The endpoint speaks JSON-RPC 2.0 over HTTP POST, single requests and
batches, with permissive CORS — browser applications can call it
directly. No API key, no registration.

POST to the root path is the only request it serves. Any other path
gets a plain-text 404, any other method a plain-text 405 with
`Allow: POST, OPTIONS`, and a POST with an empty body a plain-text
400; none of these reaches a node. Everything else this endpoint
returns is JSON-RPC.

## Served methods

- The read set of `eth_*`, `net_*` and `web3_*` — balances, blocks,
  transactions, receipts, logs, calls, gas estimation.
- The `arkiv_*` read namespace — `arkiv_query` and the entity views.
- `eth_getProof` — kept deliberately: it is the primitive for verifying
  query results against the chain's state root.
- **`eth_sendRawTransaction` is relayed.** It is the SDK's entity-write
  path, and on Arkiv relaying is safe by construction: the chain accepts
  only storage-operation transactions, there are no contracts.

A read method not listed here but served by the underlying nodes will
generally work; the lists below are what is deliberately blocked.

## Denied methods

| Group | Methods | Why |
|---|---|---|
| Node control | `admin_*`, `engine_*`, `miner_*`, `debug_set*`, `debug_verbosity`, `debug_vmodule`, `debug_freezeClient` | They do things to a node; the rest of `debug_` stays served |
| Accounts and signing | `eth_sendTransaction`, `eth_sign*`, `eth_accounts`, `personal_*` | Community nodes hold no user keys |
| Per-node state | `eth_newFilter` and the other filter methods, `eth_subscribe`, `eth_unsubscribe` | They bind state to one node, which does not work behind a load balancer |

A denied method gets a normal JSON-RPC error response (below), HTTP 200 —
it is an answered request, same as a node's own method-not-found.

**Known limitation:** the check is a text search over the request body,
so a request that merely mentions one of these names anywhere is refused
too — a query filtering on the string `admin_`, for example.

## Errors the load balancer returns

Errors from the serving node pass through unchanged, including the
standard codes (−32700, −32601, −32602, and Arkiv's `arkiv_query`
errors). The load balancer's own errors use codes −32050…−32055, and
**every message it generates starts with `lb: `** — that prefix is how
you tell an LB error from a node error.

A node's answer — success or error — always arrives as HTTP 2xx. A
non-2xx status from a node means it did not answer (overloaded, broken,
or its tunnel speaking in its place); the load balancer treats that as
a failed attempt and tries another provider, so such responses never
reach the client.

| Code | Meaning | HTTP |
|---|---|---|
| −32050 | method not supported | 200 |
| −32051 | no healthy provider | 503 |
| −32052 | request timed out | 504 |
| −32053 | response too large | 502 |
| −32054 | request too large | 413 |
| −32055 | overloaded | 429 |

An LB error echoes the request's `id` when the body yields one.
**Known limitation:** for a batch request, an LB error is still a
single error object with `id: null`, not a JSON-RPC response array —
treat a top-level object in a batch response as an error for the whole
batch. (Answers from a node are unaffected: a batch answered by a node
arrives as the array the node sent.)

## Limits

- **Request bodies: 2 MiB.** Far above the chain's own transaction size
  limit, so any valid transaction fits.
- **Responses: 64 MiB.** Sized to clear any legitimate `arkiv_query`
  page; hitting it indicates a misbehaving node, and the request fails
  with −32053 rather than returning truncated data.
- **Timeouts:** a request is answered or failed within 30 seconds.
