# The documents

## Using the endpoint

- [ENDPOINT.md](ENDPOINT.md) — the client contract: what a client of
  the community endpoint can rely on, the methods refused, the errors
  the LB itself returns.

## Running an LB

- [RUNBOOK.md](RUNBOOK.md) — operating the box: what runs where, the
  first deployment, onboarding, day to day, what it costs, releases,
  the changes that reach every provider, paying the providers, the
  keys, troubleshooting.
- [LIMITATIONS.md](LIMITATIONS.md) — everything this version does not
  do, by area, with what an operator does about each and the issue
  where one exists.

## How it works

- [PROXY.md](PROXY.md) — the architecture: how requests are served
  and how provider health is judged.
- [MARKETPLACE.md](MARKETPLACE.md) — how a community node becomes a
  provider and is paid, from the offer to the receipt.
- [ENTITIES.md](ENTITIES.md) — the records on Arkiv, field by field;
  the one page the LB, the provider tooling and settle all encode
  against.
- [INTEGRITY.md](INTEGRITY.md) — how a provider serving wrong data is
  found, and what the checks cannot see.
- [TUNNELING.md](TUNNELING.md) — how NAT'd providers reach the LB:
  the frp decision, admission by signature, the measurements.
- [CHAIN_WRITER.md](CHAIN_WRITER.md) — the sidecar that signs the LB's
  writes: its wire format and error contract.
- [TESTING.md](TESTING.md) — the test tiers, what runs where, and the
  rig's scenarios.

## What would come next

Two design notes, analysis rather than plans, each ending with what
the code would change first:

- [MISBEHAVING_NODES.md](MISBEHAVING_NODES.md) — the full problem of
  providers that serve wrong data: what v1 catches, what it does not,
  and the steps from here.
- [MARKETPLACE_FUTURE.md](MARKETPLACE_FUTURE.md) — what an open
  marketplace needs that v1 does not have: pricing, payment, a bond,
  several LBs.
