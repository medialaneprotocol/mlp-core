# Contributing to mlp-core

## Building

```sh
cargo build
cargo test
```

## Feature gates

- `pacing`, `path-selection`, `resume` are optional extensions.
- Build without them: `cargo build --no-default-features`.
- `bwe` and `input` are core (always compiled).

## Style

- `cargo fmt` (rustfmt.toml provided).
- `cargo clippy` must be clean.
- No `unsafe` code.
- All public items need doc comments.

## Tests

- Unit tests in `src/` (inline).
- Integration tests in `tests/`.
- Property tests in `tests/prop_fuzz.rs`.
- Conformance vectors in `conformance/vectors.json`.

## Commit style

Imperative mood, short subject. Author: Vitaly Kuzyaev <vitkuz573@gmail.com>.

## No CI

This project does not use GitHub Actions. Run tests locally before pushing.
