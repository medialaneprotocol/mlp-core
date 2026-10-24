# Telemetry Schema (MLP)

Adapted from QUIC's qlog (draft-ietf-quic-qlog-main-schema). MLP emits
structured events so that a deployment's measurements can be compared against
the harness's predictions.

## Event format

Each event is a JSON object:

```json
{
  "time": 1234.5,
  "name": "mlp:fragment_sent",
  "data": { ... }
}
```

`time` is milliseconds since session start (session-relative, not wall clock).

## Event catalog

| Event | Fields | Description |
|---|---|---|
| `mlp:session_started` | `carrier`, `grid_cells` | Lane opened. |
| `mlp:fragment_sent` | `object_id`, `fragment_index`, `bytes`, `priority`, `echo_critical` | A fragment left the host. |
| `mlp:fragment_received` | `object_id`, `fragment_index`, `bytes` | A fragment arrived. |
| `mlp:object_assembled` | `object_id`, `update_id`, `bytes` | An object became whole. |
| `mlp:update_applied` | `update_id`, `update_base`, `cells` | Receiver applied an update. |
| `mlp:ack_sent` | `echo_of_input_seq`, `applied_update_id` | Acknowledgement emitted. |
| `mlp:echo_measured` | `input_seq`, `echo_ms` | Sender measured an echo. |
| `mlp:plan_started` | `plan_id`, `cells` | Repair plan started. |
| `mlp:plan_completed` | `plan_id` | Repair plan completed. |
| `mlp:parked` | `reason` | Session parked. |
| `mlp:resumed` | `accepted` | Session resumed. |
| `mlp:converged` | `pending_cells` | Receiver converged. |

## Collection

The harness writes these events to its trace JSON. A deployment SHOULD
export them via its existing metrics pipeline (OpenTelemetry-compatible).
