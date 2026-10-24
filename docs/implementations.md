# Implementation Status

Tracks conformant implementations of MLP. A conformant implementation passes
the conformance vectors in `conformance/vectors.json` and (eventually) the
interop test suite.

## Implementations

| Implementation | Language | Wire format | Sender | Receiver | Scheduler | Status |
|---|---|---|---|---|---|---|
| mlp-core | Rust | full | full | full | full | reference |
| mlp-harness | Rust | full | (driver) | (driver) | full | verification layer |
| mlp-go | Go | partial | planned | planned | planned | in progress |
| mlp-py | Python | partial | planned | planned | planned | in progress |
| mlp-c | C | partial | planned | planned | planned | in progress |

## Conformance levels

- **Level 0 (wire):** parse/encode fragments and TLVs byte-for-byte.
- **Level 1 (assembly):** object assembly and dedup.
- **Level 2 (session):** handshake, media apply, ack, convergence.
- **Level 3 (full):** pacing, BWE, repair, resume, input.

## Interop

See `scripts/interop.sh` for the automated wire-level conformance run.
