# Grafana

`relay-dashboard.json` — import it, pick a Prometheus datasource, done. Every
panel carries a description saying what a bad shape looks like; hover the (i).

The rows answer four questions in order: what is generating load, where the
time is going, whether the work pays for itself, and whether the plumbing is
healthy.

## Cardinality

Label sets are deliberately bounded. Programs are labelled by an 8-character
prefix (bounded by how many protocols a turner cranks); conditions are not
labelled at all. A registry of 10,000 watches must not become 30,000 series,
so per-condition drilldown belongs in the `relay` CLI — `relay condition list`
and `relay condition explain` — which reads the chain directly and has no
cardinality budget to blow.

This is also why there is no per-target "top talkers" panel. If a dashboard
says one program is generating all the load, `relay condition list --json` on
that program's targets is the next step.

## Alerts worth having

Ordered by how much they mean when they fire.

| Condition | What it means |
| --- | --- |
| `increase(relay_skips_total{reason="executor_named_signer"}[1h]) > 0` | A target program tried to take the keeper's signature. Page. |
| `min_over_time(chain_feed_healthy[5m]) == 0` | The subscription is dead; every read has fallen back to polling. |
| `increase(relay_registry_rejected_total{reason=~"unparseable\|owner_drift"}[1h]) > 0` | A target shipped a layout change without re-registering. Its cranks have silently stopped. |
| `rate(relay_saturated_ticks_total[15m]) > 0` | More work is due than `--concurrency` can run. Cranks are late for no reason but the turner's own limits. |
| `histogram_quantile(0.99, sum by (le) (rate(relay_wake_lag_seconds_bucket[15m]))) > 30` | Cranks are landing badly late. Check the contention delay first — some of this may be deliberate. |
| `rate(relay_transactions_total{result="expired"}[15m]) > 0` | Transactions are not landing inside a blockhash's life. Raise `--max-priority-fee`. |
| `sum(rate(relay_lamports_total{direction="spent"}[1h])) > sum(rate(relay_lamports_total{direction="earned"}[1h]))` | The turner is losing money. Expected briefly while a contention delay ramps; not for an hour. |
| `rate(chain_cache_reads_total{outcome="uncovered"}[15m]) > rate(chain_cache_reads_total{outcome="covered"}[15m])` | Simulation is mostly refetching. Add a `--watch-program`. |

Deliberately not alerts: `relay_skips_total{reason="not_due"}` and
`{reason="backoff"}` are the healthy baseline and will dominate every graph,
and `relay_cranks_total{outcome="no_work"}` is the designed cheap path for a
conservative wake hint. High `no_work` next to low `sent` is worth
*investigating* — the hints are firing too often — but it costs a local
simulation and no transaction, so it is not an incident.
