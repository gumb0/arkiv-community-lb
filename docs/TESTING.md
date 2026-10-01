# Testing

**Scope:** how this repository is tested — the tiers, what runs where,
and the conventions the test code follows. The architecture under test
is [PROXY.md](PROXY.md); the writer sidecar's contract is
[CHAIN_WRITER.md](CHAIN_WRITER.md).

## Three tiers

**1. The Rust gate.** `tests/ci.sh` — format check, clippy with
warnings as errors, and every test. It is the single source of truth:
the CI `rust` job only calls it, and it runs the same way locally.
The integration tests boot the real service in-process (the LB is
built as a library with a thin binary on top) against fake providers
on real sockets, so the full serving path — listeners, forwarding,
failover, probing, the admin API — is exercised on every change in
seconds. This tier gates every push.

The marketplace agent's suites run over a fake chain
(`tests/common/fake_chain.rs`): an in-memory store behind the same two
traits the real read client and writer client implement, with a head
the test moves by hand, so expiry is a number and not a wait. Its own
suite (`tests/chain_fake.rs`) walks the same steps as the live chain
smoke, so the fake keeps the promises the node keeps. The admission
suite (`tests/admission.rs`) signs tokens with a real key
(`tests/common/signer.rs`), the way the provider tooling does, so the
signature recovery is exercised and not faked. No test in this tier
talks to a chain.

**2. The writer package.** Typecheck and unit tests need no chain and
run in CI unattended. The live smokes run against a throwaway local
dev node (`scripts/dev-node.sh`) in the CI `probes` job — but only
the probes that follow our own code. The rest measure the pinned SDK
and node image, so they run by hand when a pin moves, not on every
push. `writer/tests/` is reserved for suites CI can run unattended;
`writer/probes/` holds what is run by hand against a live network.
The Rust chain path has a live smoke of the same kind
(`crates/lb/tests/chain_live.rs`, ignored by default): every route of
the writer client and every call of the read client in one run —
records written through the sidecar, read back, counted, extended in
a batch, patched, deleted, and left to expire. `scripts/chain-smoke.sh`
runs it against a dev node it starts itself, with the sidecar in
front, and the `probes` job runs that after the writer probes; by hand
it runs against any network with the endpoint variables set and a
sidecar up.

**3. The rig.** Real containers, the shipped `arkiv-lb` binary as a
separate process, a real config file, real sockets. It runs by hand or
through the on-demand `rig` CI workflow;
it is deliberately not a gate — it boots a full stack per scenario
and takes minutes.

## The rig

Every scenario starts from the same stack: N `arkiv-reth-dev`
containers started through `scripts/dev-node.sh` (the same script the
writer smokes use — the rig never talks to the Docker API directly),
a config rendered for them with shipped defaults, and the compiled
`arkiv-lb` binary from the same build profile as the rig itself. The
rig waits until `/health` reports ready, and tears the stack down
however the scenario ends — containers stop on drop.

```
cargo build --workspace
cargo run -p rig -- all
```

The scenarios, each also runnable alone (`cargo run -p rig -- <name>`):

- `boot` — the stack comes up, every provider is admitted, teardown
  leaves nothing behind.
- `distribution` — load lands on every provider, and the `/nodes`
  served counters sum to exactly the answered-request count.
- `denylist` — a refused method is answered by the LB itself with the
  documented error envelope; no provider sees it, nothing is billed.
- `forward-to-node` — a quarantined provider is out of rotation but
  reachable through `POST /node/{id}`: dead it answers 502, alive it
  answers as itself, and neither touches billing.
- `kill-recover` — a provider dies under load: zero failed client
  requests, quarantine visible in `/nodes`, and readmission after it
  returns.
- `wrong-entity` — one provider serves entities that are not the
  chain's: it is taken out of rotation with a divergence verdict, the
  evidence event is in the LB's log, the honest providers are matched,
  and the public endpoint serves through them only.
- `wrong-block` — the same with a provider serving blocks whose hash
  is not the chain's; the evidence names the block read.
- `frozen-head` — a provider stuck behind the chain leaves rotation on
  the lag path, with no integrity verdict against it, and is readmitted
  once it catches up.
- `reference-down` — the reference taken away judges nobody: every
  verdict stands, every provider keeps serving, and the rounds judge
  again once it is back.
- `offer-accepted` — the provider tooling from `arkiv-community-node`
  posts an offer; the LB parses it and accepts it; the tooling's
  `status` parses the agreement and the counter record the LB wrote,
  and `start-tunnel` signs the tunnel token, which the LB's admission
  route admits, and refuses altered or for another port.

The rig observes through the admin API and the LB's log — what it
asserts is what an operator can see.

### One chain, and a provider that lies

The first five scenarios run over N independent dev chains, which is
enough for routing, health and failover: they compare counts and
liveness, never data. `wrong-entity` needs providers that agree on
data, so it runs **one dev node behind N relays**: each relay is a
provider with its own URL, the dev node itself is the reference, and a
difference between two providers is one the LB has to explain.

The relay is `rig relay`, a JSON-RPC relay that passes everything
through, or lies in one way: `--lie entity` adds a byte to the payload
of every entity answered to a query by key, `--lie block` changes the
hash of every block answered to `eth_getBlockByNumber`, and with either
nothing else changes, so the probes see an honest node and only the
integrity round can notice; `--lie frozen-head` keeps answering the
first head it saw to `eth_blockNumber`, so the probes see a node stuck
behind the chain. In the scenarios the relays run inside the rig; the
same command runs alone in front of any node:

```
rig relay --listen <host:port> --upstream <url> [--lie entity|block|frozen-head]
```

A fresh dev chain holds no entities, and a round samples one. So the
scenario configures the marketplace, and the LB writes its listing on
start through the writer sidecar, which the rig runs from `writer/`
with the dev chain's prefunded test key, as `scripts/chain-smoke.sh`
does. That needs Node and `npm ci` in `writer/`.

### The offer, accepted: the records between the two codebases

The LB and the provider tooling share no code: each encodes the
records from [ENTITIES.md](ENTITIES.md) and the tunnel token's message
on its own. `offer-accepted` holds them to it with both sides as they
ship. It runs the tooling's CLI
from a checkout of `arkiv-community-node` at `../node` (or wherever
`RIG_NODE_DIR` says), with `npm ci` run in its `marketplace/`, outside
the container it normally runs in. What the container would give it,
the rig gives instead: the node's address (the dev node), a key file
for the dev chain's second prefunded account, the LB's address (the
sidecar's), and the two beacon API paths an offer's specs are read
from, which a dev node has no consensus client to answer. The tunnel
itself is not part of it: the rig posts the tunnel server's admission
callbacks with the token the tooling signed, the way frps does, rather
than running frp, which every provider that connects exercises for
real. A future rig improvement would run both ends of the tunnel.

`rig load` is the load generator as its own command:
`rig load --target <url> [--concurrency N] [--duration SECONDS]`.
It runs the same request loop the scenarios use, against any endpoint
you point it at; a run with failures exits nonzero and reports the
first failure's reason.

The dev-node image lives in a credentialed registry, so
`docker login ghcr.io` is a prerequisite, locally and in CI.

## Conventions

- **Never paused time with real sockets.** The in-process tests scale
  the configured intervals down to milliseconds and poll for
  conditions; the rig runs the shipped intervals and polls the same
  way. Poll timeouts are generous on purpose: a passing run exits the
  moment its condition holds, so the timeout only decides how long a
  broken run takes to give up.
- **Counter assertions avoid absolute numbers.** A test checking
  cadence or load compares counters gathered over the same window — a
  ratio, or a bound derived from the configured intervals — instead of
  asserting an absolute count, which would flake on a slow machine.
- **Test-only switches are `#[serde(skip)]` config fields** (for
  example `health.disable_probing`), unreachable from the toml so an
  operator cannot flip them — and each carries a parse test proving
  the toml refuses its name.
- **Suites are grouped by the subsystem that drives the behavior**,
  not by endpoint: the proxy suite covers the forwarding path, the
  monitor suite covers everything probes cause, the service suite
  covers wiring and boot.
- **No test stand-ins for shipped components.** Where the rig
  exercises something an operator runs — the LB binary, its config
  format — it uses the real one, not a test build with hooks. Purely
  test-side utilities (the dev-node script, the fake providers) are
  fine; the rule bans reimplementing shipped pieces for tests.
