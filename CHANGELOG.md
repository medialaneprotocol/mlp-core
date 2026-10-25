# Changelog

## Unreleased

### Added
- **Forward error correction on the wire (spec §6.9).** While the freshest
  loss picture sits in the 2–15 % band — the closed window's measurement,
  with the prompt attributions (§6.2.1) folded in as they arrive — a
  multi-part update leaves together with one XOR parity object: each part
  rides as a length-prefixed block, the parity is the blocks' XOR plus the
  group's CRC-32, and a receiver holding k−1 of k parts rebuilds the
  missing one on arrival and proves it against the CRC — no round trip, no
  repair plan. A parity that lies fails the proof, recovers nothing, is
  counted (`parity_rejected_total`), and the repair ladder answers. A
  single-part update takes no parity: its parity is the deliberate
  duplicate the echo-critical rule already sends. Above 15 % two losses in
  one group are common enough that the parity answers nothing while
  doubling the bytes a deep-loss lane cannot afford; there the repair
  ladder and its anchors are the honest tool.

### Removed
- **The `fec` feature and the `media::fec` module.** The XOR-parity helper
  was never wired to a sender or a receiver, referenced a spec section that
  does not exist (§6.9), and built parity fragments in a class the wire
  reserves (`5..255 ⇒ BAD_CLASS`) — every conformant receiver, this crate's
  own included, would have dropped them. Forward error correction moves to
  the v0.2 plan, to be designed into the spec before it exists in code.
- The `Datagram`/`Fec` fragment-class variants and their CLI spellings, for
  the same reason: the wire freezes classes at `0..=4`. Their `class_slot`
  mapping was a latent out-of-bounds panic next to the five-slot
  `objects_by_class` counter.
- The `bwe` cargo feature: nothing was gated behind it (the estimator is
  core code and always compiled in). The declared-but-dead feature is gone.
- The `FEC_GROUP_ID`/`FEC_K` TLV kinds from the registry.

### Fixed
- The `pdu.rs` layout table said magic `52 4D 4C 50` and `version = 2`; the
  wire carries `4D 4C 50 00` ("MLP\0") and version 1. The same wrong version
  sat in `error.rs`'s `UnsupportedVersion` doc.
- `docs/wire-format.md` taught the pre-freeze flag layout (bit 0 ANCHOR,
  bit 1 ECHO_REQUESTED, bit 2 DUPLICATE) — the layout the Go/Python/C
  skeletons were written against. It now teaches the real one (FIN 0x01,
  ANCHOR 0x02, RETRANSMIT 0x04, DUP 0x08, ECHO_REQUESTED 0x10, PROBE 0x20,
  URGENT 0x40) and the full TLV registry.
- `docs/architecture.md` listed 12 of the 19 media modules and two feature
  gates that gate nothing; `docs/telemetry.md` described a 12-event qlog
  catalog no code emits — it now documents the eight Prometheus counters
  that exist; `docs/compliance.md` marked the satellite implementations'
  wire format "Full" while they drifted — the matrix now carries the
  implementations' own test results and a TypeScript column;
  `docs/implementations.md` gained the two implementations it omitted
  (mlp-ts, mlp-server).
- The crate claimed the `no-std::no-alloc` category while using `std`;
  `rust-version` is now the one the sibling crates already require (1.88).
- `scripts/validate_all.sh` and `scripts/interop.sh` no longer default to a
  foreign machine's `/tmp/opencode/bench-work`; they resolve the sibling
  repositories relative to this checkout.

### Added
- Reference implementations: Go, Python, C (wire format, golden-vector parity).
- `docs/security.md` and `docs/iana.md` drafts.
- `conformance/vectors.json` golden vectors.
- `fast_path` config on `MediaSenderConfig` (skip in-flight cap on clean links).
- Cursor objects bypass the pacer (tiny, frequent, latest-wins).
- Echo-critical pulses bypass the rate limiter while staying within the 1-second pacing window (INV-7).
- Builder methods for all configuration structs.
- 12+ usage examples in `examples/`.
- `UpdateRequest::cells` carries the encoder's measured covered-cell
  indices, so the accounting advances on what the codec actually changed
  (the tessellio codec's `changed_cell_indices` feeds it; see tessellio-bench).

### Changed
- **The ack's own window is a loss report.** An update inside the
  acknowledgement bitmap's reach that the bitmap does not name is
  missing by the receiver's own statement; its cells go due the moment
  the ack arrives instead of waiting `repair_delay`. Canonical T4
  residue 40 -> 15 cells, T2 convergence 226 -> 174 ms, T0 echo p95
  19 -> 15 ms. `AckOutcome.window_lost_updates` counts the declared
  losses.
- **The estimate can grow in `Steady` again (the freeze fix).** Three
  changes, one contract: a probe is paced at `1.25 × max(btlbw, pace)`
  (never below the estimate), a probe pending in `Steady` opens a
  one-interval pacing-gain window (`pace × 1.25`, `PROBE_PACING_GAIN`), and
  three consecutive healthy *filled* windows with `demand > pace` re-enter
  `StartUp`. Before, `pace = 0.9 × btlbw` capped the goodput the max-filter
  reads at `0.9 × btlbw` and a probe tested a rate below the estimate, so a
  healthy lane froze at whatever the first four reports reached. A
  regression test holds it: under sustained demand the pace climbs past
  20× the settled rate, to the link's own capacity.
- `repair_delay` floor is 50 ms (was 250): the floor guards an unmeasured
  round trip, it is not a wait a measured one must also pay — a 10 ms-RTT
  lane repairs in ~2 RTT instead of a fixed quarter second (MLP spec §6.5.4,
  §9.1 updated).
- The write queue's in-flight cap is **earned pipelining**: the conservative
  pair (one being sent + one replacement) until three consecutive filled
  windows stayed under 1 % loss, up to `in_flight_cap` (default 8) after,
  revoked by any other window. Time-weighted evidence, not per-window
  threshold flapping.
- `BandwidthEstimator::estimate`/`estimate_with_pace_floor` became
  `estimate_at(now)`/`estimate_with_pace_floor_at(floor, now)` (the probe's
  pacing-gain window is part of the estimate and needs the injected clock).
- The pacer's burst-window check is O(1): a running sum replaces the
  per-fragment walk of the whole window.
- Real cfg-gates: `pacing`, `path-selection`, and `resume` features now actually gate their code (modules, types, match arms). `--no-default-features` builds the core without them.
- BWE probe_interval default 30s, startup_max_reports 4 (reverted from tuning).
