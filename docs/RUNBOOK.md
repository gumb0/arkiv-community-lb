# Running the LB box

**Scope:** operating the deployed LB host — what runs there, where
things live, and the day-to-day commands. The architecture is
[PROXY.md](PROXY.md); the tunnel design is [TUNNELING.md](TUNNELING.md);
how providers join and are paid is [MARKETPLACE.md](MARKETPLACE.md);
the integrity checks are [INTEGRITY.md](INTEGRITY.md).

## What runs where

One compose stack (`compose.yaml` at the repository root) holds every
service on the box: the LB itself, the chain-writer sidecar that signs
its entity writes, and the tunnel server (frps). All three use host
networking. Ports:

- `8545` — the public JSON-RPC endpoint (open in the firewall)
- `7000` — the frps control channel provider nodes dial (open)
- `9545` — the admin API, loopback only (never opened; see below)
- `8560` — the chain-writer sidecar, loopback only; the LB is its only
  client
- tunnel ports, one per marketplace slot from `remote_port_start`
  (`20000`) up — loopback only, bound there by frps itself; the
  firewall's default-deny is the second layer

Machine-local files, all untracked with a tracked `.example` beside
them:

- `config.toml` — the LB config. The container runs as uid 65534
  (nobody), and a bind mount keeps the host file's permissions, so the
  file must be world-readable: `chmod 644` — the usual default for new
  files — is right, and safe, since the config holds no secrets. With
  `chmod 600` the LB cannot read its config and refuses to start.
- `.env` — `ARKIV_RPC_URL` (and `ARKIV_API_KEY` if the endpoint is
  metered) for the reference endpoint, which the LB and the sidecar
  share; a real environment variable beats the file
- `writer.key` — the sidecar's signing key, one `0x`-prefixed hex line.
  Compose mounts it into the container as a secret, so it never enters
  the container's environment. The sidecar reads it as the container's
  `node` user, uid 1000, so the file must belong to that uid:
  `chown 1000:1000 writer.key && chmod 600 writer.key`. Root still
  edits it; nobody else on the box can read it. There is no example
  file: create it with the key's text, and keep the key funded (see Day
  to day)

The tunnel server's config, `tunnel/frps.toml`, is tracked as it is:
it holds no secret, since the LB admits each tunnel by a signature,
and nothing that differs between boxes. Its admission route must
match `listen.admin` in `config.toml`.

## First deployment

1. Firewall first: inbound allow TCP 22, 7000, 8545; everything else
   denied. ICMP is your choice — nothing here depends on ping either
   way.
2. Install Docker with the compose plugin:
   <https://docs.docker.com/engine/install/>.
3. Clone this repository and create the machine-local files from their
   examples:
   - `cp .env.example .env` — set `ARKIV_RPC_URL` to the Arkiv
     reference endpoint (and `ARKIV_API_KEY` if it is metered).
   - `writer.key` — the sidecar's signing key, as one line. This is the
     LB's on-chain identity: the address it derives to is what the
     provider tooling ships, so a new key is a new LB. Fund it before
     the first start: the first thing the LB does is write its listing.
   - `cp config.example.toml config.toml` — set `health.chain_id` to
     the network's chain id (a wrong value quarantines every
     provider). Under `[marketplace]`, set `wei_per_call` (the price)
     and `tunnel_server` (this box's public address and the frps
     port); the section's other values are defaults. Delete the whole
     section for an LB on static providers alone. The `[integrity]`
     section runs the integrity checks; delete it to judge providers
     by their probes alone.
4. `docker compose up -d --build` — the first build downloads the base
   images and compiles for a few minutes.
5. Verify from the box: `curl -s 127.0.0.1:9545/health` and `/nodes`,
   then `docker compose logs lb`. With the marketplace on, the log has
   `listing created` once, and a `discovery` line at every poll.

## Onboarding a provider

With the marketplace on, nothing is done on this box. The operator
follows "Joining the marketplace" in the
[arkiv-community-node](https://github.com/gumb0/arkiv-community-node)
README: a key, an offer, then the tunnel. The LB does the rest, and
each step leaves a line in `docker compose logs lb`:

1. `offer accepted` — the next discovery poll, at most
   `discovery_interval` after the offer, writes an agreement with a
   tunnel port. The provider appears in `/nodes` with `source`
   `marketplace` and its agreement id, ineligible for `probe`.
   `offer waits: the cap is full` instead means every slot is taken;
   the offer is seen again at each poll while it lives.
2. `tunnel admitted` — the operator ran `start-tunnel` and the tunnel
   server let the client in. A refused client reads the reason in its
   own tunnel log; the reasons are under Troubleshooting.
3. `admission: the probes passed, the agreement record is extended` —
   the node passed its probes, and the integrity check when
   `[integrity]` is configured, and is in rotation. From here its
   agreement is extended at every refresh while it stays eligible.

The operator has the record's first lifetime, `offer_max_lifetime`
from the acceptance, to get to step 3. If the probes have not passed
by then, the log says `admission: the probes did not pass before the
agreement record expired`. The agreement and its offer are both gone
by then, and the operator posts a new offer. A node found
serving wrong data at admission is logged as such and its agreement is
not extended either.

**A static provider** is a node the LB reaches at its own URL, without
the tunnel: the tunnel server admits only clients that hold an
agreement. Add a `[[providers]]` entry with its URL to `config.toml`
and `docker compose restart lb`; it turns eligible after its first
passing probes. Static providers are not paid: settle pays agreements
only.

## Day to day

- Logs: `docker compose logs -f lb` (rotation is capped in the compose
  file). Default level is info. For debug detail — per-attempt and
  per-probe lines — set `RUST_LOG=info,lb=debug` in `.env` and apply
  it with `docker compose up -d`; set it back the same way.
- Admin API from a workstation: the port is loopback-only by design,
  so forward it — `ssh -L 9545:127.0.0.1:9545 <box>` — and read
  `http://127.0.0.1:9545/nodes` locally. It shows each provider's
  eligibility and the reason when it is out, its counts, and its last
  integrity verdict with the height it was given at. A JSON-RPC
  request POSTed to `http://127.0.0.1:9545/node/{id}` is forwarded to
  that one provider, eligibility ignored.
- `config.toml` changes: `docker compose restart lb` — the file is
  mounted in and read at startup; no rebuild. With the marketplace on,
  a stop or a restart writes the counts to the chain first and waits
  for the receipt, which can take a few minutes. Let it finish rather
  than killing the container, or the counts since the last daily write
  are lost.
- `.env` changes (`RUST_LOG`, the reference endpoint):
  `docker compose up -d` — a restart is not enough, the values enter
  the container when it is created. No rebuild either.
- `writer.key` changes: `docker compose up -d` as well — a secret is
  mounted at creation.
- The sidecar's key needs gas for every write, and a dry key stops
  them all: `docker compose logs writer` shows the address at startup,
  and its balance is checked the same way as any account on the
  network. The LB checks it at every refresh and logs `the LB key is
  low on GLM` while it is under `gas_warn_below`.
- The reference endpoint is metered, and the LB, the sidecar and a
  settle run on this box all spend the same key's quota. The LB's
  reads have three knobs: `discovery_interval` (by far the largest
  consumer: each poll is several queries), `integrity.interval`, and
  `health.ref_height_interval`. When the quota runs short, lengthen
  them in that order. A spent quota answers 429: discovery, lag and
  integrity checks stop and the refresh cannot write, while serving
  goes on. Agreements then expire one `agreement_life` after their
  last refresh.
- **A lowered lifetime does not shorten records already written.** An
  expiry can only be moved later. After `agreement_life` or
  `listing_life` is lowered, a record written under the old value
  keeps its old expiry, and the refresh leaves it out until that
  expiry is within the new life; from then on it is refreshed to the
  new one. The refresh's log line counts the records left out this
  way as `expiring_later`.
- **Ending an agreement early.** There is no command for it, and a
  shorter `agreement_life` does not do it (above). Delete the record
  through the sidecar, with the agreement id from `/nodes`:

  ```sh
  curl -s -X POST http://127.0.0.1:8560/delete \
    -H 'content-type: application/json' \
    --data '{"entityKey":"0x…"}'
  ```

  The next discovery poll finds the record gone and frees the slot,
  and the next flush closes its counter record with the count it
  holds, so what the provider served is still paid. Ask the operator
  to stop their tunnel.
- Code updates: a release tag, below.
- Reboot safety: Docker's enabled service plus `restart:
  unless-stopped` bring the stack back on boot; there is no systemd
  unit to manage.

## Releases and updates

A release is a git tag with its notes on GitHub. Deploy one with

```sh
git fetch --tags && git checkout v0.2.0
docker compose up -d --build
```

The build takes a few minutes; `up -d` then stops the running LB,
which writes its counts first and can take up to `stop_grace_period`
(200 s), and starts the new one. The tunnel clients reconnect on their
own and `/nodes` fills in within the boot window. Nothing on the
providers' side changes unless the notes say so.

Before updating, check the config keys the new tag adds or removes:
the LB refuses to start on an unknown key, so a removed one has to
leave `config.toml` first. The release notes say, and
`git diff v0.1.0 v0.2.0 -- config.example.toml` shows every change
between two tags.

Going back is the same two commands with the previous tag.

## Changes that reach every provider

Most config changes are a restart and nothing more. Three are not:
each one has a consequence on the providers' side, and the operator
has to know it before making the change.

**Moving the LB to another host.** Copy `config.toml`, `.env` and
`writer.key` to the new box, set `tunnel_server` in `config.toml` to
the new box's address, and start the stack there; the old box is
stopped first, so the two never write with the same key at once. The
LB's identity is its key, so every agreement record and every
provider's admission token stay valid, and the LB patches its listing
with the new address at startup. But each provider's tunnel client
dials the address written into its own `.env` when it joined, so
every operator has to run `./marketplace.sh start-tunnel` again,
which reads the new address from the listing and restarts their
tunnel; the agreement, the token and the port are unchanged. Until
they do, their nodes are unreachable, so announce the move with the
deadline: an agreement expires `agreement_life` (three days) after its
last refresh, and an unreachable provider is not refreshed. A provider
that misses the deadline posts a new offer. If `tunnel_server` is a
DNS name rather than an address, the move needs nothing from the
operators: point the name at the new box and the clients reconnect
on their own.

**Changing the rate.** `wei_per_call` in `config.toml` and a restart;
the LB patches its listing. The new rate applies to offers accepted
from then on. Running agreements keep the rate in their record,
whatever the listing says, and their counter records carry it, so do
not expect the fleet to re-price: a provider moves to the new rate
when its agreement expires and it posts a new offer. With a lowered
rate that is only when the provider leaves and comes back; with a
raised one, operators under agreement may ask to end their agreement
early and re-post (Day to day, "Ending an agreement early").

**A network reset** deletes every record. Before an announced reset,
run settle: what was counted and not paid by then cannot be paid
after. Once the network is back, set `health.chain_id` if the reset
changed it, then `docker compose restart lb`: the LB writes its
listing again and waits for offers, and every provider posts a new
offer with the key it has, once their nodes have followed the reset.
A running LB that was not restarted shows every refresh failing with
a not-found error; that is the symptom, and the restart is the fix.

## Paying the providers

Settlement is a separate command, `settle/`, and it is not part of the
stack on this box. Its whole input and output is chain state: the
LB's closed counter records on Arkiv, the transfers on the payout
chain, and the receipts it writes back. It never talks to the LB, and
the LB never waits for it.

**Where it runs.** Anywhere with an endpoint for both chains. Off this
box by preference, an operator's machine included, because the key
that pays exists in the process environment for the length of a run,
and this box faces the internet. Same-box is allowed; then give the
key per invocation rather than leaving it in a file here.

**What it needs.** The same `.env` at the repository root that the
stack uses, with the settle block filled in: whose records to pay, and
the payout chain's endpoint, chain id and GLM contract. Every field is
documented in `.env.example`. The key is not among them — it is given
per invocation, below. It runs on Node 22 with npm, which the box does
not have unless you install it: the stack's containers bring their
own.

**How a run goes.** Rehearse first, always. It reads and prints, signs
nothing and writes nothing:

```
cd settle && npm ci
npm run settle
```

It prints, per provider, what it would pay and for which records,
then the balances it would pay from, and finally any reason it could
not pay: GLM short of what is owed, no gas on the payout chain where
the transfers are made, or no gas on Arkiv where the receipts are
written. Read the ledger, then pay:

```
SETTLE_PRIVATE_KEY=0x... npm run settle -- --pay
```

A run pays every closed record that has no receipt yet, so what it
pays is decided by the chain and not by a flag, and running it twice
pays nobody twice. A weekly cron and a command run by hand are the
same run.

**From a cron.** On the machine that runs it: a clone of this
repository with `settle/` installed (`npm ci`) and a `.env` at its
root holding the reference endpoint and the settle block, nothing
else; the key in a file only that user can read; and one line, here
for Monday 06:00:

```
0 6 * * 1  cd /home/settle/arkiv-community-lb/settle && SETTLE_PRIVATE_KEY=$(cat /home/settle/.settle.key) npm run settle -- --pay >> /home/settle/settle.log 2>&1
```

Keep the log: it is the only record of a run that paid and could not
write its receipts, below. Rehearse by hand (`npm run settle`, no
key) before the line goes in and after any change to `.env`. How often
the line fires is how often providers are paid; the settlement period
(`settlement_period`, a week) is how much each run finds, so a daily
line pays each provider as soon as its period closes and a weekly one
pays it up to a week later. A run that fails exits 1, which cron does
not report by itself; have the log checked, or mail it.

**When a transfer fails**, nothing is lost: the run says so, moves to
the next provider, and the next run finds those records unpaid.

**When a run says `PAID BUT NOT RECEIPTED`**, a person has to decide,
because the tool cannot repair it. The money has certainly moved: a
run waits for a transfer to be mined before it writes any receipt, so
that line means the transfer landed and the receipts did not. Settle
reads a record as unpaid until one of its own receipts names it, so
the next run would pay those records a second time.

Keep the output of that run. It lists every record the transfer
covered, above the line, and names the transaction. Confirm the
transaction and its amount on the payout chain, then choose one:

- **Pay again.** Let the next run proceed. It is the only choice that
  needs no work, and it costs the provider's records twice over.
- **Stop settle until the receipts exist.** Nothing must run, cron
  included, while those records have no receipt. Writing them is a
  manual job today: one receipt per record, under the settle key,
  naming that transaction (`ENTITIES.md` has the shape).

## Keys

Three secrets exist in this deployment, and the provider tooling ships
the addresses of the first two as network values, like genesis.

| | The LB key | The settle key | The reference API key |
|---|---|---|---|
| What it does | Signs every record the LB writes: the listing, agreements, counter records. Its address is the LB's identity: what the tooling trusts records by | Sends the GLM transfers on the payout chain and writes the receipts on Arkiv, one address on both chains, so a receipt's creator is the transfer's sender | Opens the metered reference endpoint for the LB, the sidecar and settle |
| Where it lives | `writer.key` on this box, mounted into the sidecar as a secret, never in an environment | Given to a settle run per invocation; never on this box | `ARKIV_API_KEY` in `.env` on this box and on the settle machine |
| What it must hold | Gas on Arkiv, for the writes (Day to day) | The GLM owed, and gas on both chains: the payout chain for the transfers, Arkiv for the receipts | Quota (Day to day) |
| If it leaks | Anyone can write records as the LB: forge agreements, alter counts. Rotate at once | Anyone can spend the GLM in it, and write receipts that mark records paid. Rotate at once | Anyone can spend the quota. Rotate at the endpoint |
| Rotating it | A new key is a new LB: the tooling ships the new address, every agreement expires unrefreshed, every provider re-onboards with a new offer. Settle the old key's records first. Announce it like a host move | Settle reads a record as paid only by a receipt from its own address, so a new key would pay every closed record again. Pay everything with the old key first, then switch, and accept that records closed before the switch and paid by the old key are paid again if any are left; a known limitation | Edit `.env`, then `docker compose up -d` here and nothing on the settle machine beyond its own `.env` |

The provider's key is the operator's own: it posts the offer, signs
the tunnel token and receives the payouts, and the node repository's
README covers it. The admin API has no key; it is loopback-only, and
that is its whole protection.

## Troubleshooting

Symptom, then where to look.

- **The LB exits at start with `the marketplace agent could not
  start`.** With `[marketplace]` configured, a start reads the LB's
  records from the reference and asks the sidecar for its address, and
  refuses to run without either; compose keeps retrying. Compose does
  not start the LB until the sidecar answers its own health check, so
  a sidecar that is merely slow to come up never shows here; one that
  answered and then died does, as does the hub. A running LB is not
  affected by a hub outage, so do not restart or deploy during one.
  Check `docker compose logs writer`, then `ARKIV_RPC_URL` as below. To bring the endpoint back before the hub does, delete the
  `[marketplace]` section: the LB then serves the static providers
  alone.

- **A provider never turns eligible.** `/nodes` says why in
  `ineligibility_reason`:
  - `probe` — its probes get no answer. Check the tunnel: is there a
    `tunnel admitted` line for it, and what do the operator's
    `./marketplace.sh status` and `status.sh` say. For a static
    provider, check its URL.
  - `chain` — it answered `eth_chainId` with another network; the log
    has a `wrong chain` line with both ids. Check `health.chain_id`
    and the node.
  - `lag` — it is behind the reference beyond the tolerance.
  - `traffic` — client requests to it failed. Only probes bring it
    back, so it recovers by itself once it answers again.
  - `integrity` — a round found it serving data that is not the
    chain's. The `integrity: divergence confirmed` warning holds both
    answers side by side. It comes back only by passing a later round,
    never by its probes; its agreement is not refreshed while it is
    out.
- **A tunnel client is refused.** The operator's tunnel log has the
  reason the LB gave:
  - `no agreement 0x…` — the agreement has expired or was deleted.
    The operator runs `./marketplace.sh status`, and posts a new offer
    if the agreement is gone.
  - `port N requested, agreement assigns M` or `signature was not made
    by the agreement's provider over this agreement id` — the tunnel
    settings are from another agreement or another key. The operator
    runs `./marketplace.sh start-tunnel` again.
  - Every client refused at once: the LB is down or still starting.
    The tunnel server cannot reach the admission route and turns
    everyone away until it answers; the clients retry on their own.
- **`a refresh did not land`.** The line carries the sidecar's error
  and the key's balance. A dry key is the usual cause (`the LB key is
  low on GLM` comes first).
- **`the offers could not be read: this poll's discovery is
  skipped`.** The reference failed the query: an outage, or a spent
  quota (429). Serving is not affected; offers wait for a poll that
  succeeds.
- **Every provider ineligible with `source=lag` at once.** The
  reference is the suspect, not the nodes: it is on another network or
  far ahead. Check `ARKIV_RPC_URL`.
- **`boot window closed with no provider admitted` at start, with no
  `eligibility flip` lines.** Nothing passed its first probes: check the
  tunnels and `health.chain_id`, then the reference as above.
- **`reference unanswered: chain head lag goes unchecked`.** Not an
  outage: serving continues, only lag verdicts stop. The line appears
  once per change of state, as does its counterpart `reference
  answered`, and carries the reason the read failed for. Check
  `ARKIV_RPC_URL` and `ARKIV_API_KEY` — a metered endpoint answers 429
  when the key is missing or the quota is spent.
- **Clients get `no healthy provider`.** No provider is eligible;
  `/nodes` says why for each, as above.
- **The LB exits at start.** A config error names the field. A config
  the container cannot read is the file-permission rule above.
- **After a crash** (the process killed, an out-of-memory stop, a host
  reboot): Docker starts the LB again, and nothing is to be done by
  hand. Started again, the LB reads its agreements and their open
  counter records back from the chain, so each provider's count
  resumes from what the last flush wrote; the tunnel clients reconnect
  on their own, with no re-onboarding, and `/nodes` fills in within
  the boot window. What is
  lost is the counting since the last flush, daily by default: a
  deliberate stop writes the counts first, a crash cannot. A flush
  that landed but whose answer the crash cut off is read back like any
  other, and nothing is counted twice. If crashes are a pattern, a
  shorter `flush_interval` bounds the loss, at a write per interval.
