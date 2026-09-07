# Changelog

Notable changes per release, newest first. The LB box deploys a tag:
`git checkout <tag> && docker compose up -d --build`
([docs/RUNBOOK.md](docs/RUNBOOK.md)).

## v0.1.0

First release: the proxy core over a configured provider list.

- Round-robin forwarding with failover under one request deadline; a
  provider's JSON-RPC error is an answer, never retried elsewhere.
- Health probing: a shared streak per provider fed by probes and
  traffic, quarantine and automatic readmission, chain identity check,
  chain head lag against the reference RPC, backoff from dead
  providers. Providers are born ineligible.
- Method denylist in code; request and response size caps; LB-generated
  errors in the −32050…−32054 range ([docs/ENDPOINT.md](docs/ENDPOINT.md)).
- Admin API on loopback: `/health` with readiness, `/nodes` pool view,
  `POST /node/{id}` forward to one provider regardless of eligibility.
- Chain-writer sidecar (`writer/`): entity writes over the Arkiv TS SDK
  behind a small HTTP service.
- Host stack: multi-stage Dockerfile, `compose.yaml` with the LB and
  the tunnel server, runbook.
- Test rig (`crates/rig/`): scenarios over the shipped binary and real
  dev-node containers, plus a standalone load generator.
