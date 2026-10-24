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
- BWE probe_interval default 30s, startup_max_reports 4 (reverted from tuning).
