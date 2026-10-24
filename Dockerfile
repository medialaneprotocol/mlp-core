# Build the mlp-core CLI tool.
FROM rust:1.85 AS build
WORKDIR /app
COPY . .
RUN cargo build --release --features cli

FROM debian:bookworm-slim
COPY --from=build /app/target/release/mlp-core /usr/local/bin/mlp-core
ENTRYPOINT ["mlp-core"]
