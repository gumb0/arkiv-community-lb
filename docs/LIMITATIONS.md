# Known limitations

**Scope:** everything this version does not do, or does less well than
it could, in one place: deliberate choices and open issues alike. Each
entry says when it matters and what an operator does about it, and
links the issue where one exists. The design notes say what would
come next: [MISBEHAVING_NODES.md](MISBEHAVING_NODES.md) for providers
that serve wrong data, [MARKETPLACE_FUTURE.md](MARKETPLACE_FUTURE.md)
for the market. How to operate with them is [RUNBOOK.md](RUNBOOK.md).

## Serving

- **One instance, no redundancy.** A restart is a short outage for
  every client; with the marketplace on, up to `stop_grace_period`
  (200 s) while the counts are written. A choice for this version.
- **Nothing limits a client.** No cap on how many requests one client
  may send or how many run at once; the only bounds are per request,
  2 MiB in and 64 MiB out (`max_request_size`, `max_response_size`),
  so memory scales with concurrency times 64 MiB. The rate limiter in
  front of the LB is the answer
  ([RUNBOOK.md](RUNBOOK.md#what-the-stack-leaves-to-you)); a cap on
  requests in flight is
  [#54](https://github.com/gumb0/arkiv-community-lb/issues/54).
- **Method filtering is a text search** over the request body, not a
  JSON parse: a request that merely mentions a refused method name is
  refused, and requests cannot be counted per method
  ([ENDPOINT.md](ENDPOINT.md)).
- **An LB error for a batch request is one error object** with
  `id: null`, not a response array ([ENDPOINT.md](ENDPOINT.md)); a
  batch a node answers arrives as the node's array, and it counts as
  one request.
- **Failover does not remember what it tried.** Retries draw from the
  shared round-robin cursor, so at the moment a provider dies a small
  share of in-flight requests can spend their whole retry budget on
  it. It takes heavily concurrent traffic, and quarantine closes the
  window after a few failures.
- **An attempt cut short by the request deadline counts against the
  provider's health** as a failure
  ([#9](https://github.com/gumb0/arkiv-community-lb/issues/9)); three
  in a row on one provider quarantine it.

## Health

- **Health is binary and the probes decide it.** A provider that
  answers its probes within the probe timeout keeps its full share of
  traffic however slow it is; one slower than the probe timeout leaves
  rotation entirely. A slow provider is visible in `/nodes` and
  nothing acts on it.
- **Lag is measured against the reference.** While the reference is
  unreachable, a provider falling behind goes unnoticed; a reference
  that is itself behind shows every provider as lagging
  ([RUNBOOK.md](RUNBOOK.md#troubleshooting)).
- **A provider whose first chain-id check goes unanswered waits a
  whole chain interval** (five minutes) before it is asked again
  ([#18](https://github.com/gumb0/arkiv-community-lb/issues/18)).

## Integrity

What the checks do not catch is its own section in
[INTEGRITY.md](INTEGRITY.md#what-this-does-not-catch), and the full
analysis is [MISBEHAVING_NODES.md](MISBEHAVING_NODES.md).
In short: a wrong answer to anything but a block at the finalized
height and an entity read by key; a provider that lies to clients and
not to the checks; a provider that proxies an honest node; one
operator behind several identities. Open issues on the mechanism
itself:

- A divergence whose second look comes back unknown is forgotten
  until a later round sees it twice again
  ([#46](https://github.com/gumb0/arkiv-community-lb/issues/46)).
- The entity check compares attributes in the node's order, so two
  honest nodes that order them differently would diverge
  ([#49](https://github.com/gumb0/arkiv-community-lb/issues/49)).
- A key deleted since the page of keys was read tests nothing: both
  sides answer "no such entity", which is a match
  ([#50](https://github.com/gumb0/arkiv-community-lb/issues/50)).
- A shutdown during a round waits for the round
  ([#47](https://github.com/gumb0/arkiv-community-lb/issues/47)).

## Marketplace

- **A provider is paid for traffic it sends itself.** The public
  endpoint is open and the pay is per answer; the rate limiter bounds
  the pace per address, nothing bounds the count per agreement
  ([MARKETPLACE.md](MARKETPLACE.md#counting),
  [#56](https://github.com/gumb0/arkiv-community-lb/issues/56)).
- **Slots can be held without a node.** An accepted offer holds a slot
  and a port for a day whether the provider connects or not, and one
  operator with many keys can hold them all
  ([#57](https://github.com/gumb0/arkiv-community-lb/issues/57)).
- **Discovery reads one page of offers.** More than two hundred offers
  against the listing hide the rest for as long as the flood lasts
  ([MARKETPLACE.md](MARKETPLACE.md#offers)); a page of large offers
  makes the read time out
  ([#58](https://github.com/gumb0/arkiv-community-lb/issues/58)), and
  on the reference endpoint the query's expiry bound alone can do the
  same ([#44](https://github.com/gumb0/arkiv-community-lb/issues/44)). A
  skipped poll delays an offer by one interval and says so in the
  log.
- **A tunnel outlives its agreement** and keeps its port bound; the LB
  skips the port and takes it back once the client leaves
  ([#23](https://github.com/gumb0/arkiv-community-lb/issues/23),
  [#59](https://github.com/gumb0/arkiv-community-lb/issues/59) for the
  change that removes it).
- **A listing that disappears while the LB runs fails every refresh**
  until a restart recreates it
  ([#22](https://github.com/gumb0/arkiv-community-lb/issues/22)).
- **While the chain is stalled, the same offer is accepted at every
  poll**, since the acceptance's answer never arrives; the extra
  records expire unrefreshed ([MARKETPLACE.md](MARKETPLACE.md#acceptance),
  [#19](https://github.com/gumb0/arkiv-community-lb/issues/19)).
- **The offer lifetime is the same fact in two repositories**
  (`offer_max_lifetime` here, the tooling's offer life there), and
  nothing says when they disagree
  ([#45](https://github.com/gumb0/arkiv-community-lb/issues/45)).
- **A misconfigured chain id writes the listing once** before the LB
  refuses to start
  ([#24](https://github.com/gumb0/arkiv-community-lb/issues/24)).
- **One read of the chain that misses a record is believed.** An
  agreement missing from one poll is dropped
  ([#28](https://github.com/gumb0/arkiv-community-lb/issues/28)); a
  counter record missing from one read is counted twice when it comes
  back ([#35](https://github.com/gumb0/arkiv-community-lb/issues/35)).
  Both need the reference to answer without a record it has.
- **Counts lost in a window.** A counter record the flush opens is
  unknown to the LB until the next poll
  ([#34](https://github.com/gumb0/arkiv-community-lb/issues/34)); a
  stop loses a provider's counts when its record is missing
  ([#32](https://github.com/gumb0/arkiv-community-lb/issues/32)); a
  crash loses the counts since the last flush, a day by default
  ([RUNBOOK.md](RUNBOOK.md#troubleshooting)).
- **A settlement period longer than a counter record's life is
  accepted** and loses counts silently
  ([#36](https://github.com/gumb0/arkiv-community-lb/issues/36)).
- **A record written under a longer lifetime keeps that lifetime**
  after the value is lowered in config
  ([RUNBOOK.md](RUNBOOK.md#day-to-day)).

## Settlement

- **A run that paid and could not write its receipts** leaves records
  that look unpaid, and there is no way to write those receipts
  afterwards; the run says so and stops, and a person decides
  ([RUNBOOK.md](RUNBOOK.md#paying-the-providers),
  [#37](https://github.com/gumb0/arkiv-community-lb/issues/37)).
- **A rotated settle key pays every closed record again**, since a
  record is read as paid only by a receipt from settle's own address
  ([RUNBOOK.md](RUNBOOK.md#keys),
  [#60](https://github.com/gumb0/arkiv-community-lb/issues/60)).
- **One counter record settle cannot decode stops every provider
  being paid** ([#40](https://github.com/gumb0/arkiv-community-lb/issues/40)).
- **A receipt is written after one confirmation**, so a reorg of the
  transfer underpays the provider permanently
  ([#41](https://github.com/gumb0/arkiv-community-lb/issues/41)).
- **The gas checks only ask whether a balance is zero**, so a run can
  start and stop part-way
  ([#39](https://github.com/gumb0/arkiv-community-lb/issues/39)).
- **Settle needs Node on the machine that runs it**
  ([#38](https://github.com/gumb0/arkiv-community-lb/issues/38)).

## Operations

- **The reference endpoint's quota.** At the default intervals the LB
  spends about three times the hub's default quota a month; the
  intervals are the knobs, and running your own node as the reference
  removes the meter ([RUNBOOK.md](RUNBOOK.md#what-normal-operation-costs)).
- **No deployment during a reference outage.** With the marketplace
  on, the LB refuses to start when it cannot reach the chain; a running
  LB is unaffected ([RUNBOOK.md](RUNBOOK.md#troubleshooting)).
- **A host move needs every operator to act**, unless `tunnel_server`
  is a DNS name
  ([RUNBOOK.md](RUNBOOK.md#changes-that-reach-every-provider)).
- **Rotating the LB key is a new LB**: every provider re-onboards
  ([RUNBOOK.md](RUNBOOK.md#keys)).
- **The admin API has no authentication**; it is loopback-only, and
  that is its whole protection.
- **No command-line tool for the LB's records**: ending an agreement
  early is a `curl` to the sidecar
  ([RUNBOOK.md](RUNBOOK.md#day-to-day),
  [#55](https://github.com/gumb0/arkiv-community-lb/issues/55)).
