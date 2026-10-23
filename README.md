# mlp-core

The reference implementation of the **Media Lane Protocol (MLP)** core, in Rust,
and the protocol's command-line tool.

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

## The command-line tool

The crate ships `mlp-core`, the protocol's tool: it is how a person and a
script use the protocol without writing Rust.

```console
$ mlp-core fragment build --class media --priority 6 --flags fin \
      --object-id 7 --tlv UPDATE_ID=0700000000000000 --body 00010203
fragment built
  bytes            4d4c5000010201060700000000000100040000000a000108070000000000000000010203
  version          1
  class            media
  flags            fin
  priority         6
  object id        7
  fragment         0 of 1
  header extension 10 bytes
  body             4 bytes
  body sha256      054edec1d0211f624fed0cbca9d4f9400b0e491c43742af2c5b0abebf0c990d8
```

| command | what it does |
|---|---|
| `fragment build` | build one fragment from its fields and print its bytes |
| `fragment inspect` | read a fragment's bytes and print its header, its TLVs and its body |
| `object assemble` | assemble fragments into one object and report what the assembler decided |
| `ack build` | build the receiver's acknowledgement and print its bytes |
| `ack inspect` | read an acknowledgement's bytes and print its fields and its window |
| `limits` | print the protocol's hard limits: the caps, the timeouts, the invariants |

Every command prints a table for a person and, with `--json`, a documented
machine contract; the shapes are in [`docs/cli.md`](docs/cli.md). The exit
status is a contract too: `0` succeeded, `1` the check failed, `2` the usage
was wrong, `3` the input could not be read.

Build it with `cargo build --release`; the binary is
`target/release/mlp-core`.

## Layout

| Module | Responsibility |
|---|---|
| `media::pdu` | the fragment header, the TLV registry, and their validation |
| `media::object` | fragment assembly, deduplication, the open-object table |
| `media::bwe` | the bandwidth estimator |
| `media::pacer` | the token-bucket pacer |
| `media::scheduler` | priority scheduling and repair plans |
| `media::cells` | per-receiver convergence accounting over a repair-cell grid |
| `media::resume` | parking, resume, the reconnect ladder |
| `media::input` | the input path: sequencing, deadlines, the pressed set |
| `media::lifecycle` | the lane's state vocabulary |
| `media::error` | the rejection vocabulary |
| `cli` | the command-line tool: the grammar and the JSON shapes |

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

## Cursor channel

`MediaSenderMachine::enqueue_cursor` sends a cursor object (latest-wins,
never retransmitted, bypasses the pacer). Use it for pointer moves and
other high-frequency, low-priority updates that must not block media.

## Fast path

`MediaSenderConfig::fast_path` (default `false`) skips the in-flight cap on
clean links (loss < 1 %, RTT < 50 ms), trading reliability for latency.
Cursor objects bypass the pacer entirely (tiny, frequent, latest-wins).

## Building and testing

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The crate's only runtime dependencies are `serde` and, for the command-line
tool, `serde_json`. There is no `unsafe` code: the lint table forbids
`unsafe` outright.

The crate's own suites run against the protocol's verification layer,
[`mlp-harness`](../mlp-harness), which drives this crate's machines over a
modelled link and checks the protocol's invariants as the run proceeds. The
harness depends on the core, and the core's suites depend on the harness:
the cycle runs through a dev-dependency, which cargo allows, and the
manifest patches the harness's dependency on the core back to this checkout
so that the two crates share one copy of the machines.

## Status

In development. One moving version, `v0.1.0`, naming the current commit. The
wire format is frozen by the [specification](../mlp); the tool and the
estimator are still moving.

## License

PolyForm Noncommercial 1.0.0 — free for non-commercial use. Commercial use
requires a separate license. See [LICENSE](LICENSE).

The [specification](../mlp) and the
[conformance suite](../mlp-conformance) are under Apache-2.0, so any
implementation may interoperate with this one.

## Builders

Every configuration struct carries `with_*` builder methods for ergonomic
construction: `MediaSenderConfig`, `MediaReceiverConfig`, `BweConfig`,
`PacerConfig`, `CellAccountingConfig`, `PlanConfig`, `SchedulerConfig`,
`InputQueueConfig`, `HostInputConfig`, `AssemblyConfig`, `MediaLanePolicy`,
and `PathSelectionConfig`.

## Sibling repositories

| repository | what it holds |
|---|---|
| [`mlp`](https://github.com/medialaneprotocol/mlp) | the normative specification |
| [`mlp-ts`](https://github.com/medialaneprotocol/mlp-ts) | the browser implementation |
| [`mlp-proto`](https://github.com/medialaneprotocol/mlp-proto) | the wire schema |
| [`mlp-harness`](https://github.com/medialaneprotocol/mlp-harness) | the verification layer: the link model, the driver, the invariants |
| [`mlp-conformance`](https://github.com/medialaneprotocol/mlp-conformance) | the conformance corpus and its gate |
