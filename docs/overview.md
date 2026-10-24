# Protocol Overview (MLP)

MLP (Media Lane Protocol) is a transport for real-time interactive media
over links that are slow, lossy, or both. A *lane* carries three kinds of
traffic at once and keeps them apart:

- **control** — handshake, session continuity, teardown;
- **media** — updates moved whole and in order;
- **feedback** — what the receiver applied, link rate, intermediary drops.

## Design goals

- A receiver never holds state it cannot verify.
- A sender always knows what to re-send, under a rate it measured.
- The protocol moves bytes and accounts for them; it never interprets
  their content (codec-agnostic).

## Sans-IO

MLP is sans-IO: no sockets, no clocks, no runtime allocations. Time
arrives as `Instant`, outbound bytes leave as `Action` values, state
changes are driven by `StateMachine`.

## Key mechanisms

- **Framing:** 22-byte fixed header + TLV extension + payload.
- **Object assembly:** fragments reassembled by object_id; dedup by
  (object_id, fragment_index).
- **Cell accounting:** per-receiver convergence over a repair-cell grid.
- **Pacing:** token bucket + 1-second window burst (INV-7).
- **BWE:** bandwidth estimation, rate adaptation.
- **Repair:** anchor + delta, repair plan with deadline.
- **Resume:** parking, reconnect ladder, session continuity.
- **Input:** sequencing, deadlines, pressed set.
