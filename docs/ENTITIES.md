# Marketplace records — the entity contract

**Scope:** every Arkiv entity that the community load balancer, the
provider tooling, and the settle CLI write or read, field by field.
Three codebases encode records exactly as described here: the LB in
Rust, the provider tooling and the settle CLI in TypeScript. They share
no library. A test rig writes records from one side and parses them on
the other, and checks both against this page. The reasoning behind the
records is in [MARKETPLACE.md](MARKETPLACE.md).

## Rules for every record

- **Attributes hold only the fields that queries filter by.** All other
  data is in the payload, as JSON (`application/json`). Each attribute
  costs gas, and a string attribute value is at most 128 bytes. The
  payload has no fixed structure and can be up to 128 KiB.
- **A field lives in exactly one place**, as an attribute or in the
  payload, never both. A record's fields are the union of the two. A
  reader gets attributes and payload together, so a copy would add
  nothing except the question of which copy wins.
- **Every record has `kind` and `v`.** `kind` is a string that starts
  with `rpc.`. One query, `kind STARTSWITH "rpc."`, returns every record
  of this system, and the names cannot collide with another
  application's records on a shared network. `v` is the schema version,
  an `i32`, currently `1`. A reader skips records with a `v` it does not
  know.
- **The writer's identity is the entity's creator**, never an attribute.
  The chain sets the creator and nobody can change it, so every trusted
  read filters on it. A record stores another party's address only when
  a query needs it.
- **Records point at each other by entity key.** An offer points at the
  LB listing it answers, an agreement record at its offer, a counter
  record at its agreement record, a receipt at the counter record it
  pays. Each pointer is a `key` attribute, so the chain can be walked in
  both directions. A pointer finds a record; only the creator makes it
  trusted. A pointer can be followed only while its target lives. So
  each record copies the fields it still needs after its target has
  expired.
- **Names are lowercase `snake_case`.** Attribute names are
  case-sensitive on Arkiv. A query with the wrong case matches nothing
  and reports no error.
- **Keys are random.** Records are found by their attributes. The
  agreement id is the key that the write returned.
- **The rate is one number, `wei_per_call`**: GLM wei owed per completed
  request, written as a decimal string. Owed = count × wei_per_call,
  computed with bigint arithmetic on both sides.
- **Every time in a record is a block number** of the chain the records
  live on. Entity expiry is already a block number. The counter record
  adds two: the blocks its count starts and ends at. No record carries
  unix seconds.
- **Addresses** are `0x`-prefixed lowercase hex. **Entity keys** are
  `0x`-prefixed 32-byte hex.

## Who writes what

| Record | `kind` | Writer | Reader |
|---|---|---|---|
| LB listing | `rpc.lb_listing` | the LB | provider tooling |
| Offer | `rpc.offer` | provider tooling | the LB |
| Agreement record | `rpc.agreement` | the LB | provider tooling, the LB after a restart |
| Counter record | `rpc.counter` | the LB | settle, provider tooling, the LB after a restart |
| Receipt | `rpc.receipt` | settle | provider tooling, settle |

## LB listing

The LB's advertisement. The LB writes it once at deployment and updates
it at a restart when the configuration changed. Each LB has exactly one
live listing.

Attributes: `kind = "rpc.lb_listing"`, `v`.

Payload:

```json
{
  "wei_per_call": "1000000000000000",
  "tunnel_server": "203.0.113.10:7000",
  "max_providers": 100
}
```

- `tunnel_server` is the address that every provider's tunnel client
  connects to. It is the same for all providers.
- `max_providers` is the number of agreement slots. Tooling can show
  "N of M slots taken" by counting the LB's live agreement records.

Lifetime: 30 days, extended by the LB's hourly refresh. A listing that
exists belongs to an LB that has run within the last month.

Query: `$creator == LB_ADDRESS AND kind == "rpc.lb_listing"`. The LB
address is shipped with the provider tooling. A provider never learns
the LB address from a listing.

## Offer

A provider's request to join, answering one LB listing.

Attributes: `kind = "rpc.offer"`, `lb_listing` (`key`, the listing it
answers), `v`.

Payload:

```json
{
  "specs": {
    "chain_id": 7738577,
    "head": 123456,
    "el": "arkiv-reth/v0.2.0",
    "cl": "lighthouse/v8.2.1",
    "hw": { "cpus": 8, "mem_gb": 32 }
  }
}
```

The tooling fills `specs` from the node itself. The operator types
nothing. The LB stores and logs the specs and acts on one field: it
skips an offer whose `chain_id` differs from its own. The rest,
`head` included, is information for the operator. The hardware fields
are self-reported and not checked; the probes decide whether a node
serves the chain it should. An offer has no rate. The LB sets the
price in its listing, and a provider that does not accept it does not
post an offer.

The provider is the offer's creator. Lifetime: one day. An unanswered
offer expires on its own; to try again, the provider posts a new offer.
An offer is bound to one listing key: if the LB's listing is ever
replaced, offers against the old one are not found.

Query, by the LB: `kind == "rpc.offer" AND lb_listing == MY_LISTING_KEY
AND $expiresAt > head`. By the tooling: `$creator == me AND kind ==
"rpc.offer"`.

## Agreement record

The LB's acceptance of an offer. The record's key is the agreement id.
The LB keeps extending the record's lifetime while the agreement is in
force, so an existing record means "the LB still keeps this
agreement". It does not mean that the provider is online.

Attributes: `kind = "rpc.agreement"`, `provider` (`addr`), `offer`
(`key`, the accepted offer), `v`.

Payload:

```json
{
  "wei_per_call": "1000000000000000",
  "remote_port": 20007
}
```

- `wei_per_call` is the rate at which this agreement was accepted. The
  listing's rate may change later; this one does not.
- `remote_port` is the port assigned to this provider on the tunnel
  server. The tunnel client must request exactly this port.

The tunnel server's address is not in the record. The provider reads it
from the listing.

The LB writes the agreement record, then the agreement's first counter
record (below), which points at it. The two are separate writes: a
record's key is known only once it has landed, so the counter record
cannot be in the same transaction. An agreement whose counter record
did not follow gets one at the LB's next daily write.

Lifetime: two hours at creation (the accept window). Every hour, the LB
refreshes the records of the providers that are healthy at that moment,
each time setting the expiry to three days from now. A provider that is
not healthy is skipped, so its record expires three days after its last
refresh. An accepted provider that never connects is never healthy, so
its record expires at the end of the accept window.

Query, by a provider: `$creator == LB_ADDRESS AND kind ==
"rpc.agreement" AND provider == <my address>`. Query, by the LB at
startup and at every discovery poll: `$creator == LB_ADDRESS AND kind ==
"rpc.agreement"`; the same set answers "does an agreement point at this
offer" and "does this provider have a live agreement".

### The admission token

The tunnel server admits a provider's tunnel by a signature, not by a
shared secret. The provider signs the UTF-8 message

```
arkiv-rpc:<agreement id>
```

with the key that posted the offer, as an EIP-191 personal message
(viem's `signMessage`; the agreement id lowercase and `0x`-prefixed),
and its tunnel client carries two metadata values: `agreement`, the
id, and `token`, the signature as a `0x`-prefixed hex string of 65
bytes. The LB looks the id up among its own records, recovers the
signer from the token and the message, and admits the tunnel only if
the signer equals that record's `provider` and the requested remote
port equals the record's `remote_port`. The agreement id is an entity
key, and entity keys are derived from the creator's address among
other inputs, so a signature is valid for one LB only.

## Counter record

One agreement's request count for one settlement period. An agreement
has exactly one open counter record while it lives; a long agreement is
covered by several in sequence. A record is paid once it is closed,
always in full, by exactly one receipt.

Attributes: `kind = "rpc.counter"`, `agreement` (`key`), `provider`
(`addr`, so a provider can read what the LB counted for it), `state`
(`str`, `"open"` or `"closed"`), `v`.

Payload:

```json
{
  "count": 48213,
  "wei_per_call": "1000000000000000",
  "opened_block": 1204000,
  "closed_block": 1506400
}
```

- `count` is the number of completed requests: answers relayed to
  clients, not attempts. A provider's JSON-RPC error is an answer and
  is counted.
- `wei_per_call` is copied from the agreement record, so the record can
  be settled after that record has expired.
- `opened_block` is the chain head when counting for this record
  started: at acceptance for an agreement's first record, at the
  previous record's close for the rest.
- `closed_block` is the head at the closing write. It is absent while
  the record is open.

The block stamps say which span of the chain the count covers, for a
provider to check against its own logs. They are read a few blocks
before the transaction that carries them lands, so they are
approximate.

Lifecycle. The LB creates the first record at zero right after the
agreement record, and writes the count into the open record once a
day. When the record's settlement period is over (a week
by default, counted from `opened_block`) and the record has a count,
that daily write closes it: it sets the final count, `state = "closed"`
and `closed_block`, and creates the next record at zero. When the next
record did not follow the close, the next daily write creates it. A
closed record is never changed again.

A record with no count is not closed; it stays open until it has one.
When an agreement ends, its open record is closed with the count it
holds, or deleted if it never counted. So there is no closed record
with a zero count, and no zero receipt.

Lifetime: 180 days from creation, never extended. A record is never
open longer than its agreement, so a closed one stays for months for
settle to pay it.

Query, by settle: `$creator == LB_ADDRESS AND kind == "rpc.counter"
AND state == "closed"`. By the LB at startup: the same with `state ==
"open"`; at most one per live agreement. By a provider: `$creator ==
LB_ADDRESS AND kind == "rpc.counter" AND provider == <my address>`.

## Receipt

Settle's record of one payout: one counter record, paid in full.

Attributes: `kind = "rpc.receipt"`, `counter` (`key`, the counter
record paid), `provider` (`addr`), `v`. Written with the `readonly`
flag, so not even its writer can change it.

Payload:

```json
{
  "agreement": "0x8863…9057",
  "count": 48213,
  "wei_per_call": "1000000000000000",
  "amount_wei": "48213000000000000000",
  "payout": {
    "chain_id": 560048,
    "tx": "0x925d…33c7"
  }
}
```

- `agreement`, `count` and `wei_per_call` are copied from the counter
  record: the receipt is permanent and the counter record is not.
- `amount_wei` is `count × wei_per_call`.
- `payout` names the chain the transfer was made on and the transaction
  hash. A settle run pays each provider once for all of its closed
  records, so a provider's receipts from one run share a transaction
  hash, and `amount_wei` is this receipt's share of it. Settle uses one
  key on both chains: the transfer's sender is the receipt's creator,
  so anyone can check a receipt against the payout chain.

Lifetime: permanent.

Query, by settle before paying, per provider with closed records:
`$creator == SETTLE_ADDRESS AND kind == "rpc.receipt" AND provider ==
P`. The counter records whose key appears in a receipt are already paid
and are skipped, so a settle run can be repeated safely. Query, by a
provider: the same with its own address. The settle address is shipped
with the provider tooling, next to the LB address.

## Reading rules

- Request at most 200 rows per query. The node's page maximum is 200,
  and a larger limit is an error, not a smaller page.
- Add `$expiresAt > <head>` to every query even though the node hides
  expired records. It is cheap, and it guards against records that
  expire in the same block.
- Parse tolerantly: read the fields named here, ignore unknown fields,
  and skip records with an unknown `v`.
- The queries above are written in a simplified form. The node's query
  language wraps every value in its type: `kind = str('rpc.offer') AND
  lb_listing = key(0x8863…9057) AND $expiresAt > u64(1204000)`.
