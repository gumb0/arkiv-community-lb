# Running the LB box

**Scope:** operating the deployed LB host — what runs there, where
things live, and the day-to-day commands. The architecture is
[PROXY.md](PROXY.md); the tunnel design is [TUNNELING.md](TUNNELING.md).

## What runs where

One compose stack (`compose.yaml` at the repository root) holds every
service on the box: the LB itself and the tunnel server (frps). Both
use host networking. Ports:

- `8545` — the public JSON-RPC endpoint (open in the firewall)
- `7000` — the frps control channel provider nodes dial (open)
- `9545` — the admin API, loopback only (never opened; see below)
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
  metered) for the reference endpoint; a real environment variable
  beats the file
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
     reference endpoint (and `ARKIV_API_KEY` if it is metered). The
     writer's variables stay untouched until that sidecar deploys.
   - `cp tunnel/frps.example.toml tunnel/frps.toml` — set `auth.token`
     to a fresh secret: `openssl rand -hex 16`. Provider operators get
     this token.
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
     ineligible until the node connects.
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
  file). Default level is info; `RUST_LOG=info,lb=debug` in `.env`
  adds the per-attempt and per-probe lines.
- Admin API from a workstation: the port is loopback-only by design,
  so forward it — `ssh -L 9545:127.0.0.1:9545 <box>` — and read
  `http://127.0.0.1:9545/nodes` locally. A JSON-RPC request POSTed to
  `http://127.0.0.1:9545/node/{id}` is forwarded to that one provider,
  eligibility ignored.
- Config changes: edit `config.toml`, then `docker compose restart lb`.
- Update: `git pull` (or check out a release tag), then
  `docker compose up -d --build`.
- Reboot safety: Docker's enabled service plus `restart:
  unless-stopped` bring the stack back on boot; there is no systemd
  unit to manage.
