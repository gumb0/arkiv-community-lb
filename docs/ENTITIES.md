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
- **Names are lowercase `snake_case`.** Attribute names are
  case-sensitive on Arkiv. A query with the wrong case matches nothing
  and reports no error.
- **Keys are random.** Records are found by their attributes. The
  agreement id is the key that the write returned.
- **The rate is one number, `wei_per_call`**: GLM wei owed per completed
  request, written as a decimal string. Owed = count × wei_per_call,
  computed with bigint arithmetic on both sides.
- **A period is named by its start**, in unix seconds as a `u64`. The
  same value appears in counters and receipts.
- **Addresses** are `0x`-prefixed lowercase hex. **Entity keys** are
  `0x`-prefixed 32-byte hex.

## Who writes what

| Record | `kind` | Writer | Reader |
|---|---|---|---|
| LB listing | `rpc.lb_listing` | the LB | provider tooling |
| Offer | `rpc.offer` | provider tooling | the LB |
| Agreement record | `rpc.agreement` | the LB | provider tooling, the LB after a restart |
| Counters | `rpc.counters` | the LB | settle, the LB after a restart |
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

Query: `$creator == LB_ADDRESS AND kind == "rpc.lb_listing"`. The LB
address is shipped with the provider tooling. A provider never learns
the LB address from a listing.

## Offer

A provider's request to join, addressed to one LB.

Attributes: `kind = "rpc.offer"`, `lb` (`addr`, the LB it addresses), `v`.

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
nothing. The LB stores and logs the specs and uses one field: it skips
an offer whose `chain_id` differs from its own. The hardware fields are
self-reported and not checked. An offer has no rate. The LB sets the
price in its listing, and a provider that does not accept it does not
post an offer.

The provider is the offer's creator. Lifetime: one day. An unanswered
offer expires on its own; to try again, the provider posts a new offer.

Query, by the LB: `kind == "rpc.offer" AND lb == LB_ADDRESS AND
$expiresAt > head`.

## Agreement record

The LB's acceptance of an offer. The record's key is the agreement id.
The LB keeps extending the record's lifetime while the agreement is in
force, so an existing record means "the LB still keeps this
agreement". It does not mean that the provider is online.

Attributes: `kind = "rpc.agreement"`, `provider` (`addr`), `v`.

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

Lifetime: two hours at creation (the accept window). Every hour, the LB
refreshes the records of the providers that are healthy at that moment,
each time setting the expiry to three days from now. A provider that is
not healthy is skipped, so its record expires three days after its last
refresh. An accepted provider that never connects is never healthy, so
its record expires at the end of the accept window.

Query, by a provider: `$creator == LB_ADDRESS AND kind ==
"rpc.agreement" AND provider == <my address>`. Query, by the LB at
startup: `$creator == LB_ADDRESS AND kind == "rpc.agreement"`.

### The admission token

The tunnel server admits a provider's tunnel by a signature, not by a
shared secret. The provider signs the UTF-8 message

```
arkiv-rpc:<agreement id>
```

with the key that posted the offer, as an EIP-191 personal message
(viem's `signMessage`; the agreement id lowercase and `0x`-prefixed),
and presents the signature as its tunnel token. The LB recovers the
signer, looks the agreement id up among its own records, and admits the
tunnel only if the signer equals that record's `provider` and the
requested remote port equals the record's `remote_port`. The agreement
id is an entity key, and entity keys are derived from the creator's
address among other inputs, so a signature is valid for one LB only.

## Counters

One entity per settlement period. It holds the request count of every
agreement for that period.

Attributes: `kind = "rpc.counters"`, `period` (`u64`, the period's
start), `state` (`str`, `"open"` or `"closed"`), `v`.

Payload:

```json
{
  "period_end": 1789086400,
  "rows": [
    {
      "agreement": "0x8863…9057",
      "provider": "0x411e…878c",
      "count": 48213,
      "wei_per_call": "1000000000000000"
    }
  ]
}
```

- `period_end` is unix seconds, set when the entity is created. The
  start is the `period` attribute.
- `count` is the number of completed requests: answers relayed to
  clients, not attempts. A provider's JSON-RPC error is an answer and
  is counted.
- `wei_per_call` is copied from the agreement record, so a period can
  be settled after the agreement record has expired.

The LB creates the entity at the period's first flush and rewrites the
payload at every flush (hourly by default). At the end of the period it
writes the final rows, sets `state = "closed"`, and never changes the
entity again. A closed entity is the complete and final input to
settlement. Lifetime: 180 days.

Query, by settle: `$creator == LB_ADDRESS AND kind == "rpc.counters"
AND state == "closed"`. Query, by the LB at startup: the same with
`state == "open"`; there is at most one.

## Receipt

Settle's record of one payout: one agreement, one period.

Attributes: `kind = "rpc.receipt"`, `period` (`u64`), `agreement`
(`key`), `provider` (`addr`), `v`. Written with the `readonly` flag, so
not even its writer can change it.

Payload:

```json
{
  "count": 48213,
  "wei_per_call": "1000000000000000",
  "amount_wei": "48213000000000000000",
  "payout": {
    "chain_id": 560048,
    "token": "0x55555555555556acff9c332ed151758858bd7a26",
    "tx": "0x925d…33c7"
  }
}
```

`payout` names the chain and the token contract the transfer was made
on, and the transaction hash. A settle run pays each provider once for
every period it settles, so the receipts of one provider from one run
share a transaction hash, and `amount_wei` is this receipt's share of
it. A row with a zero count gets a receipt with `amount_wei` `"0"` and
`payout.tx` `null`. The agreement, provider and period are the
attributes. The receipt repeats the count and the rate, so it stays
readable after its agreement record and counters have expired.
Lifetime: permanent.

Query, by settle before paying: `$creator == SETTLE_ADDRESS AND kind
== "rpc.receipt" AND period == P` for each closed period. The
agreements listed are already paid and are skipped, so a settle run can
be repeated safely.
Query, by a provider: `$creator == SETTLE_ADDRESS AND kind ==
"rpc.receipt" AND provider == <my address>`. The settle address is
shipped with the provider tooling, next to the LB address.

## Reading rules

- Request at most 200 rows per query. The node's page maximum is 200,
  and a larger limit is an error, not a smaller page.
- Add `$expiresAt > <head>` to every query even though the node hides
  expired records. It is cheap, and it guards against records that
  expire in the same block.
- Parse tolerantly: read the fields named here, ignore unknown fields,
  and skip records with an unknown `v`.
