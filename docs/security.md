# Security Considerations (MLP)

Draft security considerations for the Media Lane Protocol. To be folded into
the eventual Internet-Draft.

## Threat model

MLP runs over an untrusted network between a host (sender) and one or more
viewers (receivers). Intermediaries (hops) may drop, reorder, duplicate or
delay fragments. Endpoints are mutually authenticated by the carrier (TLS/WSS
or gRPC-TLS); MLP itself does not define authentication.

## Denial of service

- **Amplification.** A sender never emits more than the receiver's advertised
  budget (`BUDGET_BYTES`, `MAX_OBJECT_BYTES`). A receiver that lies about its
  budget only harms itself. Senders MUST clamp an advertised budget to a sane
  maximum before use.
- **Queue exhaustion.** The write queue is bounded by `in_flight_cap` and the
  assembly table by `MAX_OPEN_OBJECTS`. Excess fragments are dropped, never
  queued without bound (INV-5).
- **Memory.** `MAX_SURFACE_CELLS`, `MAX_PARTS_PER_UPDATE`, `MAX_REGIONS_PER_UPDATE`
  bound every allocation; decoders reject oversized inputs with `BadLength`.
- **CPU.** The op walk is bounded by `MAX_REGION_OPS_BYTES`; the decoder never
  loops without a bound.

## Replay and injection

- Fragments carry `update_id` and `chain_epoch`. A receiver applies an update
  only if its `update_base` is either 0 or a previously applied update id;
  otherwise the update is refused (gap detection). This prevents a replayed
  stale update from overwriting newer state.
- `reflects_input_seq` is monotone per session; a receiver never applies an
  input twice (INV-3).

## Privacy

- MLP carries media bytes verbatim; confidentiality is the carrier's (TLS).
- No user identifiers are placed in MLP headers. `send_epoch_ms` is a
  session-local clock, not wall time.

## Loss and healing

- Loss is expected and healed by the repair plan (anchor + delta). A malicious
  dropper can delay convergence but cannot corrupt state: every applied update
  is verified whole (INV-1) and the receiver's surface equals the sender's
  emitted state at convergence (INV-2).

## Open issues

- Key rotation / session resumption across reconnects is not yet specified.
- Rate-limit interaction with unauthenticated peers is carrier-dependent.
