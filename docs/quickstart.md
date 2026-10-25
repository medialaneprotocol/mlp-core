# Quickstart (MLP)

## Build

```sh
cargo build
cargo test
```

## Basic usage

```rust
use mlp_core::media::pdu::{Carrier, Fragment, FragmentClass, Priority};
use mlp_core::media::sender::{MediaSenderConfig, MediaSenderMachine};

let grid = mlp_core::media::cells::CellGrid::with_cell_size(320, 240, 64);
let epoch = std::time::Instant::now();
let sender = MediaSenderMachine::new(
    MediaSenderConfig::new(Carrier::Ws, grid),
    grid,
    epoch,
);
```

## Feature gates

```sh
# Core only (no pacing, path-selection, resume)
cargo build --no-default-features --features "cli input"

# Everything
cargo build
```

## Conformance

```sh
cargo test --test conformance
bash scripts/interop.sh
```

## Examples

```sh
cargo run --example basic
cargo run --example full
cargo run --example fragment
```
