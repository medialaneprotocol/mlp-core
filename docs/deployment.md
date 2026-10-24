# Deployment (MLP)

## Building

```sh
cargo build --release --features cli
```

## Docker

```sh
docker build -t mlp-core .
docker run mlp-core limits --json
```

## Running

```sh
mlp-core limits --json
mlp-core fragment build --help
```

## Configuration

- `MLP_HARNESS_DEBUG` — enable debug output in the harness.
- Carrier: WebSocket (wss://), gRPC (TLS), QUIC.
