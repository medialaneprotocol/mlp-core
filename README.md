# mlp-core

The reference implementation of the **Media Lane Protocol (MLP)** core, in Rust.

MLP is a transport for real-time interactive media over links that are slow,
lossy, or both. This crate is where the protocol's bytes are defined, encoded,
and validated. It is the normative implementation: when this crate and the
[specification](../mlp) disagree, that is a bug in one of them.

## What the protocol does

A *lane* carries three kinds of traffic and keeps them apart:

- **control** — the handshake, the session's continuity, and its teardown;
- **media** — the updates a sender produces, moved whole and in order;
- **feedback** — what the receiver applied, how fast the link is, and what an
  intermediary dropped.

The design goal is that a receiver never holds a state it cannot verify, and
a sender always knows what to re-send, under a rate it measured.

## What this crate is not

It is **sans-IO**. It never touches a socket, never reads a clock, and never
drives a runtime:

- time arrives as an `Instant` parameter;
- outbound bytes leave as `Action` values;
- state changes are driven by the `StateMachine` trait.

It also does not define what an update's bytes *mean*. That is a codec's
business. The protocol moves the bytes, accounts for them, and paces them, and
it never interprets their content — so a codec plugs in without the protocol
learning anything about it.

## The two boundaries

| Direction | Trait | What it carries |
|---|---|---|
| outbound | `MediaWireCodec` | the serialization of a class body |
| outbound | `MediaFragmentSink` | one released fragment to the carrier |

A driver implements both and owns the carrier.

## Layout

| Module | Responsibility |
|---|---|
| `media::pdu` | the fragment header, the TLV registry, and their validation |
| `media::object` | fragment assembly, deduplication, the open-object table |
| `media::bwe` | the bandwidth estimator |
| `media::pacer` | the token-bucket pacer |
| `media::scheduler` | priority scheduling and repair plans |
| `media::blocks` | per-receiver convergence accounting over a repair-cell grid |
| `media::resume` | parking, resume, the reconnect ladder |
| `media::input` | the input path: sequencing, deadlines, the pressed set |
| `media::lifecycle` | the lane's state vocabulary |
| `media::error` | the rejection vocabulary |
| `testing` | the deterministic harness and the lossy-link simulator |

## Using it

```rust
use mlp_core::media::pdu::{Carrier, Fragment, FragmentClass, Priority};

let mut bytes = Vec::new();
bytes.extend_from_slice(&mlp_core::media::pdu::FRAGMENT_MAGIC);
bytes.push(mlp_core::media::pdu::MEDIA_PROTOCOL_VERSION);
bytes.push(FragmentClass::Control as u8);
bytes.push(0);                                  // flags
bytes.push(Priority::CONTROL.get());
bytes.extend_from_slice(&1_u32.to_le_bytes());   // object id
bytes.extend_from_slice(&0_u16.to_le_bytes());   // fragment index
bytes.extend_from_slice(&1_u16.to_le_bytes());   // fragment count
bytes.extend_from_slice(&0_u32.to_le_bytes());   // payload length
bytes.extend_from_slice(&0_u16.to_le_bytes());   // header extension length

let (fragment, consumed) = Fragment::parse(&bytes, Carrier::Ws)?;
assert_eq!(fragment.header.class, FragmentClass::Control);
# Ok::<(), mlp_core::media::error::RejectReason>(())
```

## Building and testing

```bash
cargo build
cargo test
cargo test --features testing   # the deterministic harness and the simulator
```

The crate has no runtime dependency and no `unsafe` code; the lint table
forbids `unsafe` outright.

## License

PolyForm Noncommercial 1.0.0 — free for non-commercial use. Commercial use
requires a separate license. See [LICENSE](LICENSE).

The [specification](../mlp) and the
[conformance suite](../mlp-conformance) are under Apache-2.0, so any
implementation may interoperate with this one.
