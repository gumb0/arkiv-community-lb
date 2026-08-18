# Tunneling — how NAT'd providers reach the LB

**Decision: [frp](https://github.com/fatedier/frp) v0.61.1.** One `frps`
sidecar on the LB host; the `frpc` client ships in the
[node distribution](https://github.com/gumb0/arkiv-community-node) as an
optional compose profile. Verified end to end in August 2026; measurements
below.

## The problem and the requirements

Most community nodes sit behind NAT — the LB cannot dial them. The tunnel
inverts the connection: the provider's machine dials out once, and the LB
reaches that provider locally on its own host. What the mechanism must
deliver:

- One local HTTP port per provider (the execution client's JSON-RPC),
  exposed to the LB only.
- Concurrent JSON-RPC requests to one provider are multiplexed over the
  single connection.
- Admission control: only the keypair that posted the marketplace offer
  may connect as that provider.
- Unattended reconnect — the LB is single-instance, so every restart drops
  every tunnel at once and every tunnel must come back with no operator
  action.
- A footprint volunteers can run: one client process, boring config.
- No third party in the read path — avoid centralized services.

## Why frp

- **Production-proven, nearby:** the Web3 Pi project runs this exact stack
  in production; its operational knowledge transfers.
- **Zero custom transport code:** a tunneled provider is
  `http://127.0.0.1:<port>` on the LB host — public and tunneled providers
  are the same thing to the proxy, a URL.
- **Reconnect is already solved** and matches the restart story: the
  client retries with backoff indefinitely (`loginFailExit = false`) and
  re-registers on its own.
- **Port-per-provider is trivial** when one process (the LB) assigns and
  consumes the ports.

Measured during verification (server on a cloud host, client on a NAT'd
machine):

- Overhead ≈ 1–2 ms per request over the raw network round trip.
- 50 parallel requests over one tunnel: all answered, none serialized.
- Server down 5 minutes → client retried unattended (2 s, 5 s, then every
  20 s) and re-registered ~10 s after the server returned.
- Dead client → connecting to its port on the LB host fails instantly with
  connection refused: a clean signal for failover.

## Admission (planned)

frp's shared `auth.token` cannot distinguish providers, and any secret
delivered through public on-chain terms would be public too. So the token
is unforgeable instead of secret: the provider signs
`(agreement id, LB address)` with the key that posted its offer and uses
the signature as its token. frps forwards every `Login`/`NewProxy` to a
local HTTP endpoint on the LB (`httpPlugins`), which verifies the
signature, checks that the requested port is the one assigned in the
agreement, and accepts or rejects.

The interface is verified against v0.61.1: client metadata and the
requested port arrive in the callbacks; a rejection reason is shown
verbatim in the provider's own client log; a rejected client retries about
every 33 s, so a fixed agreement admits on the next retry with no restart
on the provider side.

## Alternatives considered

- **Custom reverse-HTTP inside the LB** (provider dials in, the LB speaks
  HTTP/2 over the inbound socket): the right long-term shape — no sidecar,
  admission is the handshake itself — but it means owning reconnect,
  keepalive, and a bespoke client binary. May be considered as the phase-2
  replacement.
- **rathole** (frp's concept in Rust): lighter, but a far smaller
  deployment base and no nearby operational experience. Held as the
  fallback if frp develops a concrete problem.
- **WireGuard / headscale**: a VPN puts the provider's whole machine on
  a shared network, when the job is to expose exactly one port. It also
  needs its own setup workflow (keys, virtual addresses) that we would
  have to build, and VPN MTU problems — small requests work, large
  responses hang — are miserable to debug remotely with a volunteer.
- **Cloudflare Tunnel**: a third party in the read path of a product whose
  point is verified, censorship-resistant reads. Rejected on principle.
- **The rest of the Web3 Pi tunnel stack**: the frp half is what we
  adopted, along with its server-side pattern of verifying the client at
  NewProxy time (the admission design above). The browser-facing
  routing layer (Traefik, wildcard HTTPS subdomains) solves a problem
  this system does not have — the tunnel's only client is the LB on the
  same host.

## Server config

`tunnel/frps.toml.example`. Points that matter: the control port is the
only firewall opening; forwarded provider ports bind to loopback
(the LB is their only client); the admission hook is present but
commented until the marketplace lands.

The server runs from the repo's compose stack: copy the example to
`tunnel/frps.toml`, set a real token, `docker compose up -d`. The image
is built locally from the pinned release, checksum verified — same as
the client in the node distribution.
