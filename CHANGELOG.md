# Changelog

## Unreleased

### Added
- `fast_path` config on `MediaSenderConfig` (skip in-flight cap on clean links).
- Cursor objects bypass the pacer (tiny, frequent, latest-wins).
- Overhead reduction: skip zero-value TLVs in header extension.
- Builder methods for all configuration structs.
- 12+ usage examples in `examples/`.
- Real cell accounting support (via tessellio-core `changed_cell_indices`).

### Changed
- BWE probe_interval default 30s, startup_max_reports 4 (reverted from tuning).
