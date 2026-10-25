# Architecture (MLP)

## Crate layout

- `src/lib.rs` — crate root, module map.
- `src/action.rs` — Action, ConnectionEvent, Receive, Transmit.
- `src/state_machine.rs` — SessionPhase, StateMachine.
- `src/media/` — media lane core.
  - `pdu.rs` — fragment header, TLV registry, carriers, validation.
  - `object.rs` — assembly, dedup, open-object table.
  - `sender.rs` — MediaSenderMachine.
  - `receiver.rs` — MediaReceiverMachine.
  - `scheduler.rs` — priority scheduling, repair plans.
  - `pacer.rs` — token-bucket pacer.
  - `bwe.rs` — bandwidth estimator.
  - `cells.rs` — per-receiver convergence accounting.
  - `resume.rs` — parking, resume, reconnect ladder.
  - `input.rs` — input path.
  - `lifecycle.rs` — lane state vocabulary.
  - `flow.rs` — per-stream and connection flow control.
  - `loss.rs` — QUIC-style loss detection.
  - `migration.rs` — connection migration challenges.
  - `probe.rs` — path probe objects.
  - `qos.rs` — traffic-class to DSCP mapping.
  - `ticket.rs` — 0-RTT session tickets.
  - `metrics.rs` — Prometheus telemetry counters.
  - `error.rs` — rejection vocabulary.
- `src/cli/` — command-line tool (behind `cli` feature).

## Sans-IO

No sockets, no clocks, no runtime allocations. Time arrives as
`Instant`; outbound bytes leave as `Action` values.

## Feature gates

- `cli` — command-line tool.
- `pacing` — token-bucket pacer.
- `path-selection` — path selector.
- `resume` — session resume machinery.
- `input` — the input path's sender-side integration.

The estimator, the scheduler, the cell accounting, the receiver and the input
machines themselves are core code and are always compiled in.
`cargo build --no-default-features` builds the core without any of the five.
