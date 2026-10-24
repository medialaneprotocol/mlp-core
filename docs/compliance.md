# Compliance Matrix (MLP)

Tracks which features each implementation supports.

## Feature Support

| Feature | Rust (mlp-core) | Go (mlp-go) | Python (mlp-py) | C (mlp-c) | Required Level |
|---|---|---|---|---|---|
| Wire format (Level 0) | ✅ Full | ✅ Full | ✅ Full | ✅ Full | 0 |
| Object assembly (Level 1) | ✅ Full | ✅ Partial | ✅ Partial | ✅ Partial | 1 |
| Session (Level 2) | ✅ Full | 🔶 Skeleton | 🔶 Skeleton | 🔶 Skeleton | 2 |
| Pacing (Level 3) | ✅ Full | 🔶 Skeleton | 🔶 Skeleton | 🔶 Skeleton | 3 |
| BWE (Level 3) | ✅ Full | 🔶 Skeleton | 🔶 Skeleton | 🔶 Skeleton | 3 |
| Cell accounting (Level 3) | ✅ Full | ✅ Partial | ✅ Partial | ✅ Partial | 3 |
| Input path (Level 3) | ✅ Full | 🔶 Skeleton | 🔶 Skeleton | 🔶 Skeleton | 3 |
| Resume/parking (Level 3) | ✅ Full | 🔶 Skeleton | 🔶 Skeleton | 🔶 Skeleton | 3 |
| Flow control (Level 3) | ✅ Full | ✅ Partial | ✅ Partial | ❌ None | 3 |
| QoS (Level 3) | ✅ Full | ✅ Partial | ✅ Partial | ✅ Partial | 3 |
| Loss detection (Level 3) | ✅ Full | ✅ Partial | ✅ Partial | ✅ Partial | 3 |
| FEC (Level 3) | ✅ Full | ✅ Partial | ✅ Partial | ✅ Partial | 3 |
| 0-RTT (Level 3) | ✅ Full | ✅ Partial | ✅ Partial | ✅ Partial | 3 |
| Connection migration (Level 3) | ✅ Full | ✅ Partial | ✅ Partial | ✅ Partial | 3 |
| Multipath (Level 3) | ✅ Full | 🔶 Skeleton | 🔶 Skeleton | 🔶 Skeleton | 3 |
| Stream multiplexing (Level 3) | ✅ Full | 🔶 Skeleton | 🔶 Skeleton | 🔶 Skeleton | 3 |
| Path probing (Level 3) | ✅ Full | ✅ Partial | ✅ Partial | ✅ Partial | 3 |
| Prometheus metrics | ✅ Full | ✅ Partial | ✅ Partial | ✅ Partial | 3 |

Legend:
- ✅ Full — complete implementation
- ✅ Partial — functional but not full parity
- 🔶 Skeleton — basic structure only
- ❌ None — not implemented

## Conformance Test Status

| Test | Rust | Go | Python | C |
|---|---|---|---|---|
| Wire format round-trip | ✅ | ✅ | ✅ | ✅ |
| Unknown TLV skip | ✅ | ✅ | ✅ | ✅ |
| Malformed rejection | ✅ | ✅ | ✅ | ✅ |
| Object assembly | ✅ | ✅ | ✅ | ✅ |
| Dedup | ✅ | ✅ | ✅ | ✅ |
| Conformance vectors | ✅ | ✅ | ✅ | ✅ |

## How to verify

```sh
# Rust
cargo test --test conformance

# Go
go test ./...

# Python
python -m pytest

# C
make test

# All
bash scripts/interop.sh
```
