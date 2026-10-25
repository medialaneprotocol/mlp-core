# Compliance Matrix (MLP)

Tracks which features each implementation supports. The statuses are the
implementations' own test results — a level is marked full only when its
suite passes.

## Feature Support

| Feature | Rust (mlp-core) | TS (mlp-ts) | Go (mlp-go) | Python (mlp-py) | C (mlp-c) |
|---|---|---|---|---|---|
| Wire format (Level 0) | ✅ full | ✅ full (receiver side) | ✅ full | ✅ full | ✅ full |
| Object assembly (Level 1) | ✅ full | ✅ full | ✅ full | ✅ full | 🔶 partial |
| Receiver machine (ack, feedback, RTT) | ✅ full | ✅ full | 🔶 skeleton | 🔶 skeleton | ❌ none |
| Sender machine (scheduler, repair plans) | ✅ full | — (receiver pkg) | 🔶 skeleton | 🔶 skeleton | 🔶 fragmenter only |
| Pacing | ✅ full | — | 🔶 skeleton | 🔶 skeleton | ❌ none |
| Bandwidth estimation | ✅ full | — | ❌ none | ❌ none | ❌ none |
| Cell accounting | ✅ full | ✅ full | ✅ partial | ✅ partial | ✅ partial |
| Input path | ✅ full | envelope types | 🔶 skeleton | 🔶 skeleton | 🔶 skeleton |
| Resume/parking | ✅ full | ✅ full (receiver half) | 🔶 skeleton | 🔶 skeleton | ❌ none |
| Flow control | ✅ full | — | ✅ partial | ✅ partial | ❌ none |
| QoS mapping | ✅ full | — | ✅ partial | ✅ partial | ✅ partial |
| Loss detection | ✅ full | ✅ full | ✅ partial | ✅ partial | ❌ none |
| 0-RTT tickets | ✅ full | — | ✅ partial | ✅ partial | ✅ partial |
| Connection migration | ✅ full | policy engine | ✅ partial | ✅ partial | ✅ partial |
| Multipath | ✅ full (feature-gated) | policy engine | ❌ none | ❌ none | ❌ none |
| Path probing | ✅ full | — | ✅ partial | ✅ partial | ❌ none |
| Prometheus metrics | ✅ sink + mlp-server | — | ❌ none | ❌ none | ❌ none |

Legend:
- ✅ full — passes its own suite at parity with the reference
- ✅ partial — functional but not full parity
- 🔶 skeleton — basic structure only
- ❌ none — not implemented

## Conformance Test Status

| Test | Rust | TS | Go | Python | C |
|---|---|---|---|---|---|
| Wire format round-trip | ✅ | ✅ | ✅ | ✅ | ✅ |
| Unknown TLV skip | ✅ | ✅ | ✅ | ✅ | ✅ |
| Malformed rejection | ✅ | ✅ | ✅ | ✅ | ✅ |
| Object assembly | ✅ | ✅ | ✅ | ✅ | 🔶 |
| Dedup | ✅ | ✅ | ✅ | ✅ | 🔶 |
| Conformance vectors | ✅ | ✅ | ✅ | ✅ | ✅ |

## How to verify

```sh
# Rust
cargo test

# TypeScript
cd ../mlp-ts && npm test

# Go
cd ../mlp-go && go test ./...

# Python
cd ../mlp-py && python3 -m pytest -q

# C
cd ../mlp-c && make test

# All of the above plus interop
bash scripts/validate_all.sh
```
