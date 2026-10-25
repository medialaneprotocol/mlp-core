# Changelog

## Unreleased

### Added
- Reference implementations: Go, Python, C (wire format, golden-vector parity).
- `docs/security.md` and `docs/iana.md` drafts.
- `conformance/vectors.json` golden vectors.
- `fast_path` config on `MediaSenderConfig` (skip in-flight cap on clean links).
- Cursor objects bypass the pacer (tiny, frequent, latest-wins).
- Echo-critical pulses bypass the rate limiter while staying within the 1-second pacing window (INV-7).
- Builder methods for all configuration structs.
- 12+ usage examples in `examples/`.
- Real cell accounting support (via tessellio-core `changed_cell_indices`).

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
