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

**2. The writer package.** Typecheck and unit tests need no chain and
run in CI unattended. The live smokes run against a throwaway local
dev node (`scripts/dev-node.sh`) in the CI `probes` job — but only
the probes that follow our own code. The rest measure the pinned SDK
and node image, so they run by hand when a pin moves, not on every
push. `writer/tests/` is reserved for suites CI can run unattended;
`writer/probes/` holds what is run by hand against a live network.

**3. The rig.** Real everything: dev-node containers, the shipped
`arkiv-lb` binary as a separate process, real config file, real
sockets. It runs by hand or through the on-demand `rig` CI workflow;
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

The rig observes only through the admin API — what it asserts is what
an operator can see.

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
