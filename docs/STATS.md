# Stats — the hourly buckets the dashboard reads

**Scope:** `GET /stats` on the admin listener: what it serves, where
the numbers come from, and what a dashboard's backend does with them.
The counts behind them are the ones [PROXY.md](PROXY.md) describes for
`/nodes`. The counter records settle pays from
([MARKETPLACE.md](MARKETPLACE.md)) are separate and not described
here.

## What it serves

The route serves, per provider and per hour, how many requests the
provider answered and how many attempts at it produced no answer;
and, per hour, the requests the LB answered itself without any
provider. It keeps the last 31 days, in a ring in memory that is
saved to a file at every hour and at a stop
([RUNBOOK.md](RUNBOOK.md#what-runs-where)), so a restart keeps them.

```
GET /stats?since=2026-10-08T06:00:00Z
```

`since` is the start of an hour, UTC, as `YYYY-MM-DDTHH:MM:SSZ`, and
the answer holds the buckets from that hour on. Without `since`, every
hour kept is served. Any other value is answered with 400.

```json
{
  "lb": "0x8a1c…f3e2",
  "generated_at": "2026-10-08T09:41:12Z",
  "bucket": "1h",
  "since": "2026-10-08T06:00:00Z",
  "providers": [
    {
      "id": "0x3f9b…77d1",
      "agreement_id": "0x8863…9057",
      "buckets": [
        { "hour": "2026-10-08T06:00:00Z", "served": 4812, "transport_failures": 0 },
        { "hour": "2026-10-08T07:00:00Z", "served": 5107, "transport_failures": 2 },
        { "hour": "2026-10-08T08:00:00Z", "served": 4990, "transport_failures": 0 }
      ]
    },
    {
      "id": "node-1",
      "agreement_id": null,
      "buckets": [
        { "hour": "2026-10-08T07:00:00Z", "served": 1203, "transport_failures": 0, "partial": true },
        { "hour": "2026-10-08T08:00:00Z", "served": 3341, "transport_failures": 1 }
      ]
    }
  ],
  "lb_outcomes": [
    { "hour": "2026-10-08T06:00:00Z", "no_healthy_provider": 0, "timed_out": 1, "no_provider_answered": 0 },
    { "hour": "2026-10-08T07:00:00Z", "no_healthy_provider": 0, "timed_out": 0, "no_provider_answered": 0 },
    { "hour": "2026-10-08T08:00:00Z", "no_healthy_provider": 0, "timed_out": 0, "no_provider_answered": 2 }
  ]
}
```

- `lb` is the LB's address on the chain, the address its records are
  trusted by; `null` for an LB running on static providers alone.
- `id` is the provider's pool id, as in `/nodes`: a marketplace
  provider's address, a static provider's configured handle.
  `agreement_id` is the key of a marketplace provider's agreement
  record, `null` for a static one. A provider that left the pool
  keeps its buckets for the 31 days; one that joined appears at its
  first completed hour.
- `served` counts completed answers, the same number the counter
  records carry; `transport_failures` counts attempts at the provider
  that produced no answer. The LB's three counters are the requests it
  answered itself: `no_healthy_provider` when no provider was
  eligible, `timed_out` when the request deadline passed,
  `no_provider_answered` when the retry budget ran out.
- Every hour the LB ran has a bucket, a zero one if nothing happened;
  an hour with no bucket is one the LB was not running.

## Completed hours only

The hour in progress is not served; `/nodes` has the live counts.
A bucket marked `partial: true` is an hour the LB did not run whole:
cut by a start, a stop, or both, with a restart inside an hour leaving
one bucket for it with both sides' counts. It is final like any other.
A backend upserts by `(lb, id, hour)`, sums partial buckets like the
rest, and leaves them out of anything that compares hours with each
other.

## What the backend does

It polls `/stats` on every LB about once an hour (and `/nodes` for a
live view), stores the buckets, and sums them into days, weeks,
months and whatever else it shows. The LB keeps 31 days; any longer
history is the backend's to keep. The admin listener is loopback
only, so the backend reaches it the way an operator does, through a
tunnel or a port forward.