# Interoperability (MLP)

## Conformance levels

- **Level 0 (wire):** parse/encode fragments and TLVs byte-for-byte.
- **Level 1 (assembly):** object assembly and dedup.
- **Level 2 (session):** handshake, media apply, ack, convergence.
- **Level 3 (full):** pacing, BWE, repair, resume, input.

## Test vectors

`conformance/vectors.json` contains golden fragments. Every implementation
MUST parse them byte-for-byte. See `tests/conformance.rs`.

## Interop runner

`scripts/interop.sh` runs the wire-level conformance checks across all
reference implementations (Rust, Go, Python, C).

## Implementations

See `docs/implementations.md`.
