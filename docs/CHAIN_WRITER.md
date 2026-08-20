# Chain writer — how the LB writes entities

All entity writes go through one small sidecar: a Node service
(`writer/src/service.ts`) that wraps the official
[Arkiv TS SDK](https://github.com/Arkiv-Network/arkiv-sdk-js) behind five
HTTP routes. Chain reads never go through it — the Rust side reads with
plain JSON-RPC. The wire format and the error contract below are what the 
Rust `ArkivWriter` client codes against.

## Why a sidecar

Writing an entity means ABI-encoding operations, signing, sending, waiting
for the receipt, and decoding typed reverts. The official SDK does all of
that and tracks the protocol as it changes; it is TypeScript, and the LB is
Rust. A parallel Rust encoder would have to chase every protocol change on
its own, so instead the write path crosses one process boundary and stays on
the official code.

The service is deliberately thin. It holds the deployment's Arkiv key and
translates HTTP requests into SDK calls — nothing else. Down, it looks
exactly like the chain being down, and every write path degrades for that
already.

## Schema-blind

The sidecar mirrors the SDK's generic entity operations and knows nothing
about what the LB stores: payloads are opaque bytes, attributes are generic
name→value pairs. Domain encoding (agreement records, usage counters,
receipts) lives in Rust, where the same records are also parsed on the read
path.

The alternative — a schema-aware sidecar with one endpoint per record
type — was rejected because it spreads every schema across two languages.
Schema-blind, each record type is one Rust struct that serializes and
deserializes symmetrically, and schema changes never touch the sidecar.

## Wire format

All routes are POST with JSON bodies; the listener binds `127.0.0.1` by
default (`WRITER_HOST=0.0.0.0` inside a compose network — a container's
loopback is not reachable from other containers). The listener is meant for
the compose-internal network only; never expose it publicly.

| Route | Body | Does |
|---|---|---|
| `/create` | payload, contentType, expires, attributes?, flags?, salt? | one entity |
| `/patch` | entityKey, set?, unset?, payload?, contentType? | change named parts, leave the rest |
| `/delete` | entityKey | remove |
| `/extend` | entityKey, expires | set a later expiry |
| `/execute-batch` | creates?, patches?, deletes?, extensions? | several operations, one transaction |

JSON cannot carry bytes, 64-bit integers, or the SDK's tagged values, so:

- **payload** is base64 both ways.
- **attributes** are `{ "name": { "type": "...", "value": ... } }` with the
  SDK's type tags: `str`, `bool`, `i32`, `u64`, `u256`, `dec`, `addr`,
  `key`, `bytes32`. Values for `u64`/`u256`/`dec` (and `salt`) are decimal
  strings.
- **salt** defaults to 128 random bits, which keeps the new entity's key
  unpredictable. `"salt": "0"` is the SDK's `NO_SALT`: the key derives from
  the owner and nonce alone, so anyone who knows both can compute it in
  advance — only for entities that are meant to be found that way.
- **expires** is `"permanent"`, `{ "seconds": n }`, `{ "blocks": n }`, or
  `{ "atBlock": "n" }`. Ask in blocks when the chain's block time may
  differ from 2 s — the SDK converts durations at a fixed 2 s per block.
- **responses** are the SDK's return shape with bigints as decimal strings.

For example, creating an entity whose payload is `{"hello":"arkiv"}`, alive
for 300 blocks:

```
POST /create
{
  "payload": "eyJoZWxsbyI6ImFya2l2In0=",
  "contentType": "application/json",
  "attributes": {
    "kind":  { "type": "str",  "value": "example" },
    "rank":  { "type": "i32",  "value": 7 },
    "price": { "type": "dec",  "value": "1.25" },
    "payee": { "type": "addr", "value": "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266" }
  },
  "expires": { "blocks": 300 }
}

200
{ "entityKey": "0x8863…9057", "txHash": "0x925d…33c7", "expiresAt": "248397" }
```

Bodies over 2 MiB are refused with 413. The SDK's own validation runs
during decoding, so a bad value (an out-of-range `i32`, a 129-byte string)
is a 400 with the SDK's error, not a failed transaction.

## One write at a time

Writes are serialized through a single queue: one transaction in flight,
later requests wait. With one transaction in flight there is no nonce to
manage, no race to lose, and failure recovery is trivial — a failure is
seen before the next transaction is ever built. The write volume this
serves (periodic refresh and flush cycles, occasional event-driven writes)
never needs more.

Batching belongs to the caller, through `/execute-batch`. The queue never merges
waiting requests into one transaction, because a transaction is atomic:
all operations apply or none do. Merging whatever happened to be queued
would make unrelated writes share a fate, and hand callers errors caused by
operations they never sent.

Cost intuition: a write costs one block inclusion (~5 s wall time on a 2 s
chain, receipt polling included) regardless of how many operations it
carries. Coalescing a cycle's operations into one `/execute-batch` is how writes
stay cheap.

## Three outcomes of a write operation

A write fails, succeeds — or goes **unresolved**, and the third outcome
needs different handling from the first:

- **400** — the body did not decode. Nothing was sent. Fix the request.
- **500** — the write failed: the transaction reverted or could not be
  sent. The body carries the walked error chain,
  `{ "error": [{ "name", "message" }, ...] }`.
- **504** — the transaction was sent but no receipt arrived within the
  wait (180 s). It may still be mined. The body carries
  `{ "pending": { "txHash": ... } }`, and the caller resolves it by
  polling that hash — **never by resending**: a resent create gets a fresh
  salt, so it makes a *second* entity if the original lands after all.

The status follows from *where* the failure happened (decoding vs
writing), not from matching error types, so new SDK validation errors land
on the 400 side automatically.

## Limits that bind

- The node caps raw transaction size at 128 KiB (txpool default), at every
  endpoint. A `/execute-batch` batch must stay under it; with ~200-byte payloads
  the ceiling is roughly a hundred creates. The Rust caller chunks.
- Attribute string values are capped at 128 bytes by the SDK. Larger data
  belongs in the payload.
- An attribute-filtered query can briefly trail a just-delivered receipt
  (observed on fast dev chains). Reading your own write back may need a
  retry; a `$key`-filtered lookup has not shown this lag.

## Testing

Two tiers with different needs, in different directories:

- `writer/tests/` — the suite CI runs on every push: the HTTP shell driven
  against a fake writer, no chain, no key, no gas. Covers the wire format
  both ways, the refusals, the outcome split (including 504, which a live
  test cannot trigger on demand), and the serialization.
- `writer/probes/` — live-chain scripts, run by hand
  (`scripts/run-probes.sh`): smoke tests for the writer module and the
  service, plus probes that measure what the engine and SDK actually do.
  Their lifetimes are in blocks so they mean the same thing at any block
  time.

`scripts/dev-node.sh` raises a local throwaway chain (the upstream
`arkiv-reth-dev` image, pinned to the release the devnet runs, 250 ms
blocks) so probes need no devnet and spend no devnet gas. CI runs the two
smoke probes against it.
