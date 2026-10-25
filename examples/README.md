# Examples

- `basic.rs` — enqueue a media update and poll fragments.
- `cursor.rs` — cursor channel (latest-wins).
- `input.rs` — input queue.
- `parking.rs` — parking/resume.
- `resume.rs` — resume reject reasons.
- `fragment.rs` — fragment build/inspect.
- `ack.rs` — ack build/inspect.
- `cells.rs` — cell grid.
- `cell_accounting.rs` — cell accounting and repair rounds.
- `scheduler.rs` — scheduler.
- `lifecycle.rs` — lane policy.
- `pacer.rs` — token bucket.
- `pacer_demo.rs` — the pacer meeting a burst.
- `path_selection.rs` — path selection.
- `path_probing.rs` — path probing.
- `bwe.rs` — bandwidth estimator.
- `assembly.rs` — object assembly.
- `full.rs` — a full sender/receiver loop in one process.
- `conformance.rs` — the conformance vectors.
- `gen_vectors.rs` — generate conformance vectors.
- `genvec.rs` — the golden-vector generator.
- `greasing.rs` — GREASE TLV round-trip.
- `error_handling.rs` — error and reject reasons.
- `telemetry.rs` — Prometheus telemetry.
- `cfg_features.rs` — print which optional features are compiled in.

Run with `cargo run --example <name>`.
