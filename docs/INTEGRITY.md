# Integrity — how a provider serving wrong data is found

**Scope:** how the LB decides that a provider is serving wrong data, what
it does about it, and what it cannot see. Health, which is about whether
a provider answers at all and keeps up with the chain, is in
[PROXY.md](PROXY.md); how providers join is in
[MARKETPLACE.md](MARKETPLACE.md). Vocabulary: the *reference* is the
official Arkiv RPC endpoint the LB is configured with; a *round* is one
pass of the checks over every provider.

## Why it exists

Providers are permissionless and paid per request. Nothing stops one
from answering quickly and confidently with data that is not the
chain's. Health checks cannot see that: a node that answers `eth_chainId`
and `eth_blockNumber` correctly looks healthy however wrong the rest of
its answers are. So the LB compares what providers serve against the
reference, on a cadence of its own, and takes a provider out of rotation
when the two disagree in a way that lag cannot explain.

The reference is the single oracle. If the official endpoint itself
serves wrong data, the network has a bigger problem than this LB, and
comparing providers against each other is a later improvement.

## The check in one picture

```
reference                        LB                          provider
    │◄── finalized block H ───────│                               │  1. once a round
    │─── the whole block ────────►│─── the block at H ───────────►│  2. every provider,
    │                             │◄── its block at H ────────────│     compared whole
    │                             │                               │
    │◄── a page of live keys ─────│                               │  3. once a day
    │─── keys ───────────────────►│   one picked at random        │
    │◄── arkiv_query by key at H ─│                               │  4. once a round
    │─── the entity at H ────────►│─── the same query at H ──────►│  5. every provider,
    │                             │◄── its entity at H ───────────│     compared whole
    │                             │                               │
    │                             │   mismatch: wait, repeat 2, 5 │  6. one wrong read
    │                             │   still different: out of     │     is not a verdict
    │                             │   rotation, evidence logged   │
```

## What is compared

Two things, every round, and both are reads any client makes.

**The block at the finalized height.** The LB reads the reference's
finalized block once, with its transaction hashes, and asks every
provider for the block at that height. A fixed set of fields is compared: the block
hash, the parent hash, the state, transactions and receipts roots, and
the list of transaction hashes. Not the hash alone, which is one field
any node can fetch from anywhere; and not every field, because clients
of different versions render the same block with small differences in
fields that are not part of consensus, and comparing those would take
the whole fleet out of rotation whenever the reference runs a newer
client than the providers. Every honest node has the same block at a
finalized height, so a provider that does not is on a fork or on another
chain.

**One entity, at the same height.** The LB picks one entity key, asks
the reference for it with a query by key pinned at the finalized
height, then asks every provider the same query pinned the same way,
and compares the two entities field by field: creator, creation and
expiry heights, payload, attributes.

Pinning both sides to one height is what makes the comparison exact.
Blocks are two seconds apart, and two nodes asked at the same moment
are often a block apart; asked at the same block, they must agree. The
finalized height is the one every eligible provider has, since the
block check demands it, and the one no reorganisation of the tip can
change. Asking the provider with no block parameter, as most clients
do, would leave the choice of block to the provider: an answer at a
block the reference does not have yet cannot be compared, and a
provider could answer there every time. The price is that the entity
read does not look like most clients' reads (see below).

One key serves the whole fleet each round, and the reference is asked
once, not once per provider, so a round costs the reference one block
read and one entity read whatever the size of the fleet, and every
reference call is metered.

## Where the key comes from

Once a day the LB reads one page of live entities from the reference,
every writer's records and not only its own, and each round picks a key
from that page at random among those whose entity exists at the
finalized height: created at or before it, and not expired by it. The
page carries each entity's creation and expiry heights, so a round
skips a key whose entity has expired by then, or was created after it
(the page is read at the head, where it already exists), and picks
another. When every entity on the page has expired, the page is read
again at the next round instead of at the end of the day: a page of
short-lived entities would otherwise leave the rounds with nothing to
sample for hours.

Known weakness: the page a node returns for the same query is in the
node's own order, so a stable set of long-lived entities can make the
same page come back every day, and a provider could in time learn which
entities are sampled. The fix, when it matters, is to walk a random
number of pages before taking one. It is not done today.

## Verdicts

Each provider gets one of four verdicts per round.

- **match** — both reads answered, and the block and the entity are the
  reference's. Nothing happens. Only this verdict counts as a pass: a
  block that matches while the entity read could not be judged is not
  one, or a provider serving right blocks and wrong entities would be
  let back in on the strength of its blocks.
- **stale** — the provider does not have the finalized block yet. This
  is not lying and is not treated as such: the ordinary health check
  already notices a lagging provider within seconds and takes it out of
  rotation until it catches up. The round only logs the verdict. A
  provider that has the finalized block and is behind the head by any
  distance is compared at that block like any other; how far behind it
  is remains the health check's business. Stale is measured against
  the provider's own head, as its probes last reported it, not only
  against the reference: a provider whose head was past the finalized
  height and has no block there is not behind the chain. That is a
  divergence, or stale would be a verdict a provider can choose and
  never be judged.
- **divergence** — any of these, from either read:
  - the block at the finalized height differs from the reference's;
  - the provider answers that it has no block there while its probes
    report a head at or past that height (see stale, above);
  - the entity differs from the reference's at that height;
  - an answer is larger than the LB will read, 4 MiB. The reference's
    answer to the same read is a small fraction of that (a payload is
    at most 128 KiB), so an honest node's is too; as unknown, padding
    would be a way past every round.

  The round does not act on the first mismatch. It waits
  `confirm_after`, asks the provider again the same way, and only a
  second mismatch counts: one wrong read is not a verdict. The
  reference is not asked again, since at a finalized height its
  answers do not change.
- **unknown** — the reference could not be reached or has no finalized
  block, so nobody is judged; or the reference could not answer the
  entity read, so no entity is judged this round and the blocks alone
  are; or a provider did not answer a round's read in time, which is
  unknown for that provider and not unhealthy, since liveness is the
  health check's job and a round must not count the same failure
  twice. An unknown changes nothing, in either direction.

Every round logs one line with the count of each verdict, so a quiet
fleet still leaves a record that it was checked. Per provider, a match
is logged at debug level and a stale or unknown at info; a confirmed
divergence is the warning-level event below. Until the first page of
keys has been read after a start, no round can pass, and the log says
so. Every read of the page of keys logs one line at info with the
number of keys and the block at which the last of them expires, since
the read is a metered call and the line is how its cost is counted.

## Out of rotation, and back

A confirmed divergence takes the provider out of rotation at once, with
one log line naming the source, exactly as a health failure does. What
brings it back is different, and deliberately so.

A health failure clears when the provider passes a few probes in a row.
An integrity failure clears only when the provider passes an integrity
round. The rounds keep running over providers that are out for
integrity and healthy otherwise, which is how one that fixes itself gets
back. A provider that is out for health is not checked at all until it
is healthy again: its answers would mean nothing.

The first round runs as soon as the LB has admitted its providers after
a start, then every `interval`. What a round decided is not kept across
a restart, so this is what keeps a provider found lying before the
restart from serving for long after it. A provider that joins later is
checked the moment it is first admitted, not at the next round, so a
new provider serving wrong data is out within the time one check and
its confirming second look take.

Being out of rotation changes nothing about the tunnel. The provider
keeps answering the LB's checks and the operator's direct requests. Its
agreement is no longer refreshed while it is out, as for any provider
out of rotation, so it expires after its life unless the provider is
back before then. What it does not get is client traffic, and requests
it does not serve are not paid.

## Evidence

When a divergence is confirmed, the LB logs one event at warning level
with everything it held at that moment: the provider, its agreement id,
which check failed, and the two answers side by side. For the block,
that is the height, the two block hashes, and every compared field that
differs, with both values. For the entity, it is the key, the block the
provider answered at, and the two entities with their attributes in
full and their payloads as hashes.

That event is the evidence. The nodes view on the admin API also shows
each provider's last verdict and the block height it was judged at, in
memory only; while a provider is out for integrity its reason says so,
in one word like the health reasons, and the verdict beside it says
when.

## Configuration

The checks run when the config has an `[integrity]` section and the LB
has a reference endpoint; without the section there are no rounds, and
the section without a reference is refused at start, since it cannot
work. Two fields, documented in `config.example.toml`:

- `interval`, how often a round runs. Every round costs the reference a
  few metered calls, so this is the knob to turn if the reference's quota
  is short, the same way as the health check's reference sampling.
- `confirm_after`, how long the LB waits before asking a provider again
  after a mismatch.

## What this does not catch

- **A wrong answer to a query**, as opposed to a wrong entity. The checks
  read entities by key and compare their contents. Which entities match
  a filter is not checked, and neither is any other served method.
- **Altered transaction bodies behind true hashes.** The block check
  compares the transaction hashes a node reports, not the transaction
  contents. A node that reports the right hashes and serves changed
  contents behind them passes. Catching it would mean hashing the served
  transactions ourselves, which depends on the client version's encoding.
- **A provider that proxies an honest node.** It passes every check its
  upstream would pass. Nothing in the answers can tell it apart, and
  nothing here tries to.
- **Wrong blocks at any other height.** The block check reads the
  finalized block and no other. A node that serves it right and wrong
  blocks above or below it passes every round.
- **A dropped transaction.** A provider that accepts a transaction and
  never broadcasts it answers nothing a read can see.
- **A provider that serves wrong data to some clients and not others.**
  The checks reach a provider the way clients do, so it cannot tell them
  apart by origin; it can by shape and cadence. The entity read is
  pinned to a block where most client reads are not, the two reads
  come as a pair, and the rounds are regular where client traffic is
  not. A provider that answers what looks like a check honestly and
  lies to the rest passes every round; one that lies at random is
  caught only as often as the sampled read lands on a lie. Only
  replaying client requests catches the first, and for this version
  it is a known limitation.
- **A provider that fails the round's reads on purpose.** A read that
  errors or times out is unknown, never a verdict, since liveness is the
  health check's job. A provider that answers its probes and errors on
  everything else is never judged here; it errors on clients too, which
  only replaying client requests would show.

## Future improvements

- **Replaying client requests.** Take a sample of real read-only client
  requests, send each one to the reference pinned at the block the
  provider answered at, and compare per method. This covers queries and
  every other method, at the cost of a sampler in the request path, a
  memory bound for large responses, a metered reference call per sample,
  and a comparison rule for each method.
- **Several clean rounds before readmission**, so a provider that stops
  lying when caught and resumes once back is out for longer each time.
- **A random walk through the pages** when harvesting keys, so the
  sampled population cannot be learned.
- **A finality stall.** If the chain keeps producing blocks but stops
  finalizing them, the rounds repeat at the same finalized height, which
  is harmless but checks nothing new. A fallback would pin the height a
  fixed depth below the head after the finalized block has not moved for
  a configured time.
- **Evidence that survives a restart**, on the nodes view or on chain.
- **Comparing providers against each other**, so the reference is not
  the only oracle.

The whole problem, what lies beyond these, and the order they would
be worth taking in is [MISBEHAVING_NODES.md](MISBEHAVING_NODES.md).
