# Running the LB box

**Scope:** operating the deployed LB host — what runs there, where
things live, and the day-to-day commands. The architecture is
[PROXY.md](PROXY.md); the tunnel design is [TUNNELING.md](TUNNELING.md).

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
- per-provider tunnel ports (`18545`, `18546`, …) — loopback only,
  bound there by frps itself; the firewall's default-deny is the
  second layer

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
- `tunnel/frps.toml` — the tunnel server config with the shared token

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
     provider tooling ships, so a new key is a new LB.
   - `cp tunnel/frps.example.toml tunnel/frps.toml`. There is no
     shared token: the LB admits each provider's tunnel by a signature
     (`TUNNELING.md`).
   - `cp config.example.toml config.toml` — set
     `listen.public = "0.0.0.0:8545"`, set `health.chain_id` to the
     network's chain id (a wrong value quarantines every provider),
     and add one block per assigned tunnel port:

     ```toml
     [[providers]]
     id = "node-1"
     url = "http://127.0.0.1:18545"
     ```

     Providers may be listed before their tunnels exist; they sit
     ineligible until the node connects. Under `[marketplace]`, set
     `wei_per_call` (the price) and `tunnel_server` (this box's public
     address and the frps port); the section's other values are
     defaults. Delete the whole section for an LB on static providers
     alone.
4. `docker compose up -d --build` — the first build downloads the base
   images and compiles for a few minutes.
5. Verify from the box: `curl -s 127.0.0.1:9545/health` and `/nodes`,
   then `docker compose logs lb`.

## Onboarding a provider

Assign the operator a unique remote port and give them three values:
this box's public address, the token from `tunnel/frps.toml`, and that
port — their setup renders the rest. Add the matching
`[[providers]]` entry to `config.toml` and `docker compose restart lb`
— the config is read once at startup. Admission is automatic: the
provider turns eligible after its first passing probe rounds, visible
in `/nodes`.

## Day to day

- Logs: `docker compose logs -f lb` (rotation is capped in the compose
  file). Default level is info. For debug detail — per-attempt and
  per-probe lines — set `RUST_LOG=info,lb=debug` in `.env` and apply
  it with `docker compose up -d`; set it back the same way.
- Admin API from a workstation: the port is loopback-only by design,
  so forward it — `ssh -L 9545:127.0.0.1:9545 <box>` — and read
  `http://127.0.0.1:9545/nodes` locally. A JSON-RPC request POSTed to
  `http://127.0.0.1:9545/node/{id}` is forwarded to that one provider,
  eligibility ignored.
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
  network.
- Code updates: `git pull` (or check out a release tag), then
  `docker compose up -d --build` — `--build` is only ever needed here.
- Reboot safety: Docker's enabled service plus `restart:
  unless-stopped` bring the stack back on boot; there is no systemd
  unit to manage.

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
per invocation, below.

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
  - `probe` — its probes get no answer. Check the tunnel: the node's
    `status.sh`, and that its remote port matches the `[[providers]]`
    entry.
  - `chain` — it answered `eth_chainId` with another network; the log
    has a `wrong chain` line with both ids. Check `health.chain_id`
    and the node.
  - `lag` — it is behind the reference beyond the tolerance.
- **Every provider ineligible with `source=lag` at once.** The
  reference is the suspect, not the nodes: it is on another network or
  far ahead. Check `ARKIV_RPC_URL`.
- **`boot window closed with no provider admitted` at start, with no
  `health flip` lines.** Nothing passed its first probes: check the
  tunnels and `health.chain_id`, then the reference as above.
- **`reference unanswered: chain head lag goes unchecked`.** Not an
  outage: serving continues, only lag verdicts stop. The line appears
  once per change of state, as does its counterpart `reference
  answered`. Check `ARKIV_RPC_URL` and `ARKIV_API_KEY` — a metered
  endpoint answers 429 when the key is missing or the quota is spent.
- **Clients get `no healthy provider`.** No provider is eligible;
  `/nodes` says why for each, as above.
- **The LB exits at start.** A config error names the field. A config
  the container cannot read is the file-permission rule above.
