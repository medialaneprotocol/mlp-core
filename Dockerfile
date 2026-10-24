# Build the mlp-core CLI tool.
FROM rust:1.88 AS build
WORKDIR /app
COPY . /app
# Remove dev-dependencies section (mlp-harness) so the build doesn't need them.
RUN sed -i '/^\[dev-dependencies\]/,/^\[/ { /^\[/!d; }' Cargo.toml
RUN cargo build --release --features cli

FROM debian:trixie-slim
COPY --from=build /app/target/release/mlp-core /usr/local/bin/mlp-core
ENTRYPOINT ["mlp-core"]
