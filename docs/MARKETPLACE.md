# The marketplace — how providers join and get paid

**Scope:** how a community node becomes a provider of the load balancer
and how it is paid, from the first offer to the payout. The records are
specified field by field in [ENTITIES.md](ENTITIES.md); the tunnel
transport is [TUNNELING.md](TUNNELING.md); how requests are served is
[PROXY.md](PROXY.md). Vocabulary: the whole service is the LB; a
*provider* is a community node under agreement with it.

## The flow in one picture

```
provider                        Arkiv                             LB
   │                              │◄── LB listing: rate, tunnel ───│  once, at startup
   │◄── reads the listing ────────│    server, cap                 │
   │ offer: node specs ──────────►│                                │
   │                              │◄── discovery poll ─────────────│  every 5 minutes
   │                              │◄── agreement record: rate, ────│  acceptance
   │◄── reads the record ─────────│    tunnel port                 │
   │ tunnel connects with the signed token ───────────────────────►│  admission
   │                              │        probes pass → provider serves traffic
   │                              │◄── hourly refresh of the record│  while it serves
   │                              │◄── counters, per period ───────│  requests served
   │                              │◄── receipts ───── settle ──────│  after payout
```

Two on-chain writes make an agreement: the provider's offer and the
LB's acceptance. There is no negotiation and no separate acceptance step
on the provider's side. The provider consents by connecting its tunnel
with a token only it can produce.

## Two shipped addresses

The provider tooling ships with two addresses as network values, the
same way it ships genesis and bootnodes: the LB's Arkiv address and the
settle address. The provider tooling finds every record it trusts by
the record's creator: the LB's address for marketplace records, the
settle address for receipts. The chain sets the creator and nobody can
change it, so nobody can forge an LB record or a receipt. A provider
never learns the LB's address from the chain, only from the
distribution.

## The LB advertises itself

At startup the LB writes its **[listing](ENTITIES.md#lb-listing)**: the
rate it pays, the tunnel server address, and its provider cap. The LB
sets the rate: one number, GLM wei per completed request. An offer
carries no price. A provider that does not accept the rate does not post
an offer.

The listing is updated at a restart when the configuration changed, and
it is kept alive by the same hourly refresh as the agreement records, so
a listing that exists belongs to a running LB.

## Offers

A provider posts an **[offer](ENTITIES.md#offer)** addressed to one LB:
its node's chain id, head height, client versions and a few hardware
facts, all read from the node by the tooling. The offer lives for a day.
An unanswered offer expires by itself; to try again, the provider posts
a new offer.

The tooling refuses to post while the node is still syncing, and while
the provider already has a live agreement, so an operator cannot waste
gas on an offer the LB would skip.

The LB polls for offers addressed to it every five minutes and
considers those that expire within two days, name its own chain, and
report a head height close to the current one. Offers from providers
that already have an agreement are skipped. When there are more offers
than free slots, older offers win.

## Acceptance

Accepting an offer is one write: the **[agreement
record](ENTITIES.md#agreement-record)**, whose key is the agreement id.
It carries the rate the agreement was accepted at and the tunnel port
assigned to this provider. The record is created with a short lifetime,
two hours, called the accept window.

The provider reads its record, signs its agreement id with the key that
posted the offer, and starts its tunnel client with that signature as
its token and the assigned port. The tunnel server asks the LB whether
to admit each client that connects. The LB admits it only if the
signature was made by the record's provider and the requested port is
the record's port. A rejection is shown word for word in the provider's
tunnel log, and each one names the fix:

- `no agreement for this signature`
- `signature does not match the agreement's provider`
- `port 20007 requested, agreement assigns 20003`
- `load balancer starting, retry`

The token is a signature, not a secret: the record is public, and
copying it does not let anyone produce the signature. There is no
shared tunnel password.

Once the tunnel is up, the LB probes the node like any other provider.
After the probes pass, the provider is in rotation and serves traffic.

## Staying under agreement

One rule keeps agreements alive: **every hour, the LB refreshes the
records of the providers that are healthy at that moment**, setting each
record's expiry to three days from then. Nothing else decides who stays.

- A provider that was accepted but never connected is never healthy, so
  its record is never refreshed and expires at the end of the accept
  window.
- A provider whose tunnel is down, or whose node stopped answering, is
  not healthy, so its record expires three days after its last refresh.
- A provider that was unhealthy for a moment misses one refresh and is
  refreshed the next hour.
- If the LB itself is down, nothing is refreshed, and every record
  survives up to three days. A restart of the LB never costs a provider
  its agreement.

An expired record frees its slot. Rejoining is a new offer. There is no
ban and no eviction in this version; a provider that misbehaves is taken
out of rotation by the health checks and stops being refreshed.

## Slots and the cap

The LB accepts up to a configured number of providers, one agreement
per provider address. A slot is held from the moment the record is
written and freed when the record expires. At the cap, a valid offer is
not accepted; it waits and expires, and the provider posts again. The
cap is published in the listing, so the provider tooling can show "N of
M slots taken" by counting the LB's live agreement records.

## Counting

The LB counts completed requests per agreement (answers relayed to a
client.) A node's JSON-RPC error is an answer and counts. Requests are
grouped into settlement periods, a day by default, and the counts are
written to the chain every hour as one
**[counters](ENTITIES.md#counters)** record per period. At the end of a
period the LB writes the final counts and marks the record closed; a
closed record never changes again.

Counts live in memory between writes, so a crash loses at most an hour
of counting. Everything else is on chain, and the LB rebuilds its state
from the chain at every start.

## Settlement

**Settle** is a separate command-line tool with its own keys, run on a
schedule, once a week by default. A run reads every closed period that
has no receipts yet and computes what each provider is owed: count
times rate, from the counters records. It pays each provider once for
all those periods, with one GLM transfer on the payout chain, and
writes one **[receipt](ENTITIES.md#receipt)** per agreement and period,
all carrying that transfer's hash. Receipts are permanent and cannot be
changed, not even by settle.

A run can be repeated: agreements that already have a receipt for a
period are skipped. A rehearsal mode computes the same ledger without
paying or writing anything.

Payouts go to the address that posted the offer, on the payout chain.
The provider's key is therefore also its payout key.

## Restarts and resets

The LB keeps no marketplace state that is not on the chain. At startup
it reads its agreement records and its open counters record and
continues from there; the tunnel server rejects logins until that has
finished, and tunnel clients retry on their own. With the marketplace
enabled, the LB refuses to start if it cannot reach the chain; a
configuration switch runs it on statically configured providers alone.

If the network is reset, every record is gone. The LB is restarted and
starts from nothing: it writes its listing again and waits for offers.
Providers post again; their keys are unchanged. Work that was counted
but not yet settled cannot be paid after a reset, so the operator runs
settle before an announced reset.

## What the provider does

Four commands, each a step, and the tunnel client the node already
runs:

1. `keystore` creates the provider key once.
2. `post-offer` reads the LB listing, shows the rate, and posts the offer
   through the provider's own node. No API key is needed anywhere.
3. `status` shows the offer, the agreement, the slot count, and the
   receipts.
4. `tunnel-token` signs the agreement id and writes the token and the
   assigned port into the node's configuration; the existing setup
   script renders the tunnel client's config from them.

## Future improvements

- **Rate competition.** Offers could carry a rate and the LB could
  choose among them. The records already allow it; the economics need
  a decision first.
- **Batched acceptances.** Several offers found by one poll could be
  accepted in one transaction.
- **Capacity in the listing.** A live free-slot count, next to the cap.
- **A retry of a failed refresh in the same hour**, rebuilding the
  batch from the chain, instead of waiting for the next one.
