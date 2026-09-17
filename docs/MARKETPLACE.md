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
   │                              │◄── agreement record: rate, ────│  acceptance, then
   │◄── reads the record ─────────│    tunnel port; then a counter │  its counter
   │                              │    record at zero              │  record
   │ tunnel connects with the signed token ───────────────────────►│  admission
   │                              │        probes pass → provider serves traffic
   │                              │◄── hourly refresh of the record│  while it serves
   │                              │◄── daily count into the counter│  requests served
   │                              │    record; closed per period   │
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
it is kept alive by the same hourly refresh as the agreement records,
with a lifetime of thirty days, so a listing that exists belongs to an
LB that has run within the last month.

## Offers

A provider posts an **[offer](ENTITIES.md#offer)** that points at the
LB's listing: its node's chain id, head height, client versions and a
few hardware facts, all read from the node by the tooling. The offer
lives for a day. An unanswered offer expires by itself; to try again,
the provider posts a new offer.

The tooling refuses to post while the node is still syncing, while the
provider already has a live offer, and while it has a live agreement,
so an operator cannot waste gas on an offer the LB would skip.

The LB polls for offers against its listing every five minutes and
considers those that expire within two days and name its own chain.
Offers from providers
that already have an agreement, and offers an agreement already points
at, are skipped; a provider's duplicate offers are tolerated and the
oldest taken. When there are more offers than free slots, older offers
win. The poll reads one page of offers, two hundred at most. A flood
of more offers than that would hide the real ones for as long as it
lasts; this is a known limitation.

## Acceptance

Accepting an offer is two writes: the **[agreement
record](ENTITIES.md#agreement-record)**, whose key is the agreement
id, and then the agreement's first **[counter
record](ENTITIES.md#counter-record)**, at zero, pointing at it. The
agreement record carries the rate the agreement was accepted at, the
tunnel port assigned to this provider, and the key of the offer it
accepted. It is created with a short lifetime, two hours, called the
accept window. If the second write fails, the agreement stands and the
LB opens its counter record at the next daily write.

The LB accepts one offer at a time, oldest first, up to its free
slots, and gives each provider the lowest tunnel port not held by a
live agreement. When an acceptance's answer is lost (the write may or
may not have landed), its port and slot stay held, and the LB reads
its own records back at the next poll: an agreement that landed is
adopted, and one that did not is written then, its port and slot free
again. While the chain is stalled that answer stays lost, and
the LB accepts the same offer again at every poll until blocks come
again; the extra agreements expire unrefreshed. A known limitation.

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

The LB notices an agreement's end at its next discovery poll, when it
reads its own records back and finds the record gone: it closes the
agreement's counter record with the count it holds, and frees the slot
and the port. That is up to five minutes after the record expired. The
delay changes nothing for the provider: a record only expires after
its provider has been out of rotation for three days, so no request
reaches it in those five minutes.

## Slots and the cap

The LB accepts up to a configured number of providers, one agreement
per provider address. A slot is held from the moment the record is
written and freed when the record expires. At the cap, a valid offer is
not accepted; it waits and expires, and the provider posts again. The
cap is published in the listing, so the provider tooling can show "N of
M slots taken" by counting the LB's live agreement records.

## Counting

The LB counts completed requests per agreement (answers relayed to a
client.) A node's JSON-RPC error is an answer and counts. Each
agreement has one open **[counter record](ENTITIES.md#counter-record)**
on the chain, created at zero when the agreement is accepted, and the
LB writes the count into it once a day. Every record covers one
settlement period, a week by default, counted from the record's own
opening: when the period is over and the record has a count, the daily
write closes it with the final count and opens the next record at zero,
in the same transaction. A closed record never changes again, and it is
what settle pays. A record with no count is not closed; it stays open
until it has one. Each agreement has its own periods, starting when
the provider joined; nothing is shared between providers.

Each record names the first and last block its count covers, so a
provider can check it against its own logs. Counts live in memory between
writes, so a crash loses at most a day of counting; a deliberate stop
writes them first. Everything else is on chain, and the LB rebuilds its
state from the chain at every start.

## Settlement

**Settle** is a separate command-line tool with its own key, run on a
schedule, once a week by default. A run reads every closed counter
record that has no receipt yet and computes what each provider is
owed: count times rate, from the records. It pays each provider once
for all of its records, with one GLM transfer on the payout chain, and
writes one **[receipt](ENTITIES.md#receipt)** per counter record. A
provider's receipts from one run all carry the hash of that one
transfer, and each names its own share of it. Receipts are permanent
and cannot be changed, not even by settle.

Settle uses one key on both chains: the same address sends the
transfer and writes the receipt, so anyone can check a receipt against
the payout chain. A run can be repeated: records that already have a
receipt are skipped. A rehearsal mode computes the same ledger without
paying or writing anything.

Payouts go to the address that posted the offer, on the payout chain.
The provider's key is therefore also its payout key.

## Restarts and resets

The LB keeps no marketplace state that is not on the chain. At startup
it reads its agreement records and its open counter records and
continues from there; the tunnel server rejects logins until that has
finished, and tunnel clients retry on their own. The same read runs at
every discovery poll; a restart is the same read with nothing
remembered yet. With the marketplace
configured, the LB refuses to start if it cannot reach the chain or the
sidecar; a configuration without the marketplace section runs it on
statically configured providers alone.

If the network is reset, every record is gone. The LB is restarted and
starts from nothing: it writes its listing again and waits for offers.
Providers post again; their keys are unchanged. Work that was counted
but not yet settled cannot be paid after a reset, so the operator runs
settle before an announced reset. A replaced listing has the same
effect on offers: they point at the old listing's key and are never
found, so a new listing is a new deployment for everyone.

## What the provider does

Four commands, each a step, and the tunnel client the node already
runs:

1. `keystore` creates the provider key once.
2. `post-offer` reads the LB listing, shows the rate, and posts the offer
   through the provider's own node. No API key is needed anywhere.
3. `status` shows the offer, the agreement, the slot count, the open
   counter record, the closed records not yet paid, and the receipts,
   following the pointers from one record to the next.
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
- **Paging the offer read.** Reading past the first page of offers,
  oldest first, so a flood of offers cannot hide real ones.
- **A registry.** A network-level key that creates every LB listing
  and hands it to its LB, so the tooling ships one address for any
  number of LBs.
