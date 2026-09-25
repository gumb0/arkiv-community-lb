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
    │◄── a page of live keys ─────│                               │  3. once an hour
    │─── keys ───────────────────►│   one picked at random        │
    │                             │─── arkiv_query by key ───────►│  4. no block given,
    │                             │◄── the entity, at block B ────│     as a client asks
    │◄── the same query, at B ────│                               │  5. pinned to the
    │─── the entity at B ────────►│   compared whole              │     provider's block
    │                             │                               │
    │                             │   mismatch: wait, repeat 4–5  │  6. a reorg resolves,
    │                             │   still different: out of     │     a liar does not
    │                             │   rotation, evidence logged   │
```

## What is compared

Two things, every round, and both are reads any client makes.

**The block at the finalized height.** The LB reads the reference's
finalized block once, with its transactions, and asks every provider for
the block at that height. A fixed set of fields is compared: the block
hash, the parent hash, the state, transactions and receipts roots, and
the list of transaction hashes. Not the hash alone, which is one field
any node can fetch from anywhere; and not every field, because clients
of different versions render the same block with small differences in
fields that are not part of consensus, and comparing those would take
the whole fleet out of rotation whenever the reference runs a newer
client than the providers. Every honest node has the same block at a
finalized height, so a provider that does not is on a fork or on another
chain.

**One entity, read the way a client reads it.** The LB picks one entity
key and asks every provider for it with an ordinary query by key, with
no block parameter, exactly as a client would. Each answer says which
block it was served at. The LB then asks the reference for the same key
pinned at that block, and compares the two entities field by field:
attributes, payload, owner, creator, expiry, content type.

Pinning the reference to the provider's own block is what makes the
comparison exact. Blocks are two seconds apart, and two nodes asked at
the same moment are often a block apart; asked at the same block, they
must agree. The provider is never asked a pinned read, so the check
cannot be told from client traffic by its shape, and a provider that
lies to clients about an entity lies to the check.

One key serves the whole fleet each round, and the reference is asked
once, at the block most providers answered at. A provider that answered
at another block, which happens when one is a block ahead of the rest,
is not judged this round and is compared at the next. So a round costs
the reference one block and one entity read whatever the size of the
fleet, and every reference call is metered.

## Where the key comes from

Once an hour the LB reads one page of live entities from the reference,
every writer's records and not only its own, and each round picks a key
from that page at random. The page carries each entity's expiry, so a
round skips a key whose entity has expired since the page was read and
picks another.

Known weakness: the page a node returns for the same query is in the
node's own order, so a stable set of long-lived entities can make the
same page come back every hour, and a provider could in time learn which
entities are sampled. The fix, when it matters, is to walk a random
number of pages before taking one. It is not done today.

## Verdicts

Each provider gets one of four verdicts per round.

- **match** — both reads answered, and the block and the entity are the
  reference's. Nothing happens. Only this verdict counts as a pass: a
  block that matches while the entity read could not be judged is not
  one, or a provider serving right blocks and wrong entities would be
  let back in on the strength of its blocks.
- **stale** — the provider does not have the finalized block yet, or
  answered the entity at a block further behind the reference than the
  lag tolerance allows. This is not lying and is not treated as such: the
  ordinary health check already notices a lagging provider within seconds
  and takes it out of rotation until it catches up. The round only logs
  the verdict. The distance check matters because a provider stuck in
  the past would otherwise agree with the reference at its own old block.
- **divergence** — the block differs, or the entity differs. The round
  does not act on the first mismatch. It waits `confirm_after`, asks both
  sides again the same way, and only a second mismatch counts. The wait
  lets a reorganisation of the chain's tip resolve; a provider that was
  briefly on the losing side of one matches on the second try. A liar
  does not.
- **unknown** — the reference could not be reached or has no finalized
  block, so nobody is judged; or a provider answered the entity at a
  block other than the one the reference was asked at, so that provider
  waits for the next round; or a provider did not answer a round's read
  in time, which is unknown for that provider and not unhealthy, since
  liveness is the health check's job and a round must not count the
  same failure twice. An unknown changes nothing, in either direction.

Every round logs one line with the count of each verdict, so a quiet
fleet still leaves a record that it was checked. Per provider, a match
is logged at debug level and a stale or unknown at info; a confirmed
divergence is the warning-level event below. Until the first page of
keys has been read after a start, no round can pass, and the log says
so.

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
restart from serving for long after it.

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
that is the height and the two block hashes. For the entity, it is the
key, the block the provider answered at, and the two entities with their
attributes in full and their payloads as hashes.

That event is the evidence. The nodes view on the admin API also shows
each provider's last verdict and the block height it was judged at, in
memory only, and while a provider is out for integrity its reason names
the check and the height.

## Configuration

The checks run when the config has an `[integrity]` section and the LB
has a reference endpoint; without the section there are no rounds, and
the section without a reference is refused at start, since it cannot
work. Two fields, documented in `config.example.toml`:

- `interval`, how often a round runs. Every round costs the reference a
  few metered calls, so this is the knob to turn if the reference's quota
  is short, the same way as the health check's reference sampling.
- `confirm_after`, how long the LB waits before asking again after a
  mismatch. Longer if the chain shows deeper reorganisations than
  expected.

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
- **A provider that serves wrong data to some clients and not others.**
  The checks reach a provider the way clients do, so it cannot tell them
  apart by origin, but a provider that lies at random will be caught only
  as often as the sampled read lands on a lie.

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
