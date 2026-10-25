# Implementation Status

Tracks conformant implementations of MLP. A conformant implementation passes
the conformance vectors in `conformance/vectors.json` and (eventually) the
interop test suite.

## Implementations

| Implementation | Language | Wire format | Sender | Receiver | Scheduler | Status |
|---|---|---|---|---|---|---|
| mlp-core | Rust | full | full | full | full | reference |
| mlp-ts | TypeScript | full (receiver side) | — | full | — | browser reference (receiver) |
| mlp-server | Rust | via mlp-core | via mlp-core | via mlp-core | via mlp-core | reference WebSocket server |
| mlp-harness | Rust | full | (driver) | (driver) | full | verification layer |
| mlp-go | Go | full | fragmenter | skeleton | skeleton | in progress |
| mlp-py | Python | full | fragmenter | skeleton | skeleton | in progress |
| mlp-c | C | full | fragment helper | — | — | in progress |

## Conformance levels

- **Level 0 (wire):** parse/encode fragments and TLVs byte-for-byte.
- **Level 1 (assembly):** object assembly and dedup.
- **Level 2 (session):** handshake, media apply, ack, convergence.
- **Level 3 (full):** pacing, BWE, repair, resume, input.

## Interop

See `scripts/interop.sh` for the automated wire-level conformance run and
`scripts/validate_all.sh` for the full matrix (Rust, TypeScript, Go, Python, C
plus interop).
