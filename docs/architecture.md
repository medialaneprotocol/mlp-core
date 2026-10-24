# Architecture (MLP)

## Crate layout

- `src/lib.rs` — crate root, module map.
- `src/action.rs` — Action, ConnectionEvent, Receive, Transmit.
- `src/state_machine.rs` — SessionPhase, StateMachine.
- `src/media/` — media lane core.
  - `pdu.rs` — fragment header, TLV registry, validation.
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
  - `error.rs` — rejection vocabulary.
- `src/cli/` — command-line tool (behind `cli` feature).

## Sans-IO

No sockets, no clocks, no runtime allocations. Time arrives as
`Instant`; outbound bytes leave as `Action` values.

## Feature gates

- `cli` — command-line tool.
- `pacing` — token-bucket pacer (optional).
- `bwe` — bandwidth estimator (core).
- `path-selection` — path selector (optional).
- `resume` — session resume (optional).
- `input` — input path (core).
