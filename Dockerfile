# Build the mlp-core CLI tool.
FROM rust:1.85 AS build
WORKDIR /app
COPY . /app
# Remove dev-dependencies (mlp-harness) so the build doesn't need them.
RUN python3 -c "s=open('Cargo.toml').read(); s=s.replace('[dev-dependencies]\nserde_json = \"1\"\nproptest = \"1\"\n','[dev-dependencies]\n'); open('Cargo.toml','w').write(s)"
RUN cargo build --release --features cli

FROM debian:trixie-slim
COPY --from=build /app/target/release/mlp-core /usr/local/bin/mlp-core
ENTRYPOINT ["mlp-core"]
