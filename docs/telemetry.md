# Telemetry (MLP)

The core is sans-IO: it keeps no clock and opens no socket, so it does not
export metrics itself. It publishes a telemetry sink
(`media::metrics::PrometheusTelemetry`) that a deployment feeds and scrapes.

## Counter catalog

| Counter | Type | Meaning |
|---|---|---|
| `mlp_fragments_sent_total` | counter | Fragments that left the host. |
| `mlp_fragments_received_total` | counter | Fragments that arrived. |
| `mlp_bytes_sent_total` | counter | Payload bytes sent. |
| `mlp_bytes_received_total` | counter | Payload bytes received. |
| `mlp_updates_applied_total` | counter | Updates the receiver applied whole. |
| `mlp_convergences_total` | counter | Times the receiver reached zero pending cells. |
| `mlp_losses_total` | counter | Losses the receiver declared. |
| `mlp_echoes_total` | counter | Input-to-echo measurements completed. |

`PrometheusTelemetry::render()` produces the Prometheus text exposition
format for all eight counters. `mlp-server` wires the sink into a session's
two machines and serves the rendered page at `/metrics`.

## Predictions, not just measurements

The harness publishes, per tier (`t0..`t4``), the predicted protocol
budgets — echo percentiles, convergence bounds, repair cadence — as data
(`mlp-harness tiers`, mirrored in `mlp-conformance/tiers.json`). A
deployment's scraped counters are meant to be compared against those
predictions; the conformance corpus (`mlp-conformance/decisions/`) holds the
reference traces a comparison runs against.
