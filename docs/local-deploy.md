# Local Deployment (MLP)

## Quick start

```sh
# Build and run
docker compose up --build

# Or build locally
cargo build --release --features cli
./target/release/mlp-core limits --json
```

## Testing the deployment

```sh
# Health check
mlp-core limits --json

# Fragment round-trip
mlp-core fragment build --class media --priority 6 --object-id 1 --payload "hello"

# Run conformance
cargo test --test conformance
bash scripts/interop.sh
```

## Environment variables

- `RUST_LOG` — log level (debug, info, warn, error)
- `MLP_PORT` — listen port (default 8080)

## Monitoring

Prometheus metrics available at `/metrics` when deployed with the metrics feature.
