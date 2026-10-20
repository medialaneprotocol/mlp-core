# The `mlp-core` command-line tool

The tool is the Media Lane Protocol's front door: it builds and reads the
bytes the protocol defines, assembles objects, builds and reads the receiver's
acknowledgement, prints the protocol's hard limits, and runs the published
conformance corpus against a build.

```sh
cargo build --release            # the library and the tool
./target/release/mlp-core --help
```

## The grammar

```
mlp-core <command> [flags]
```

| command | what it does |
|---|---|
| `fragment build` | build one fragment from its fields and print its bytes |
| `fragment inspect` | read a fragment's bytes and print its header, its TLVs and its body |
| `object assemble` | assemble fragments into one object and report what the assembler decided |
| `ack build` | build the receiver's acknowledgement and print its bytes |
| `ack inspect` | read an acknowledgement's bytes and print its fields and its window |
| `limits` | print the protocol's hard limits: the caps, the timeouts, the invariants |
| `conformance` | run the published conformance corpus against this build |

Every command accepts `--json`, and every command's `--help` prints its
flags and a worked example.

## The exit status

The status is a contract, and it is the same for every command:

| status | meaning |
|---|---|
| `0` | the command succeeded |
| `1` | the check failed — a corpus verdict, or a measurement past its budget |
| `2` | the usage was wrong — an unknown verb, a missing or unknown flag |
| `3` | the input could not be read — bad hex, a truncated fragment, a body that does not decode |

A refusal is a typed error with a named reason, on standard error, in the
form `reason: message`:

```
$ mlp-core fragment inspect --hex zz
input: the hex text holds 'z', which is not a hexadecimal digit
```

No input makes the tool panic. A bad hex, a truncated fragment, an unknown
TLV name, a body that does not decode: each is a refusal with its own reason.

## Input and output

| direction | form |
|---|---|
| bytes on the wire | hexadecimal, lower case, `:` and whitespace ignored |
| bytes in a file | hexadecimal text, one fragment per line for `object assemble` |
| bytes from a terminal | hexadecimal text on standard input |
| the machine contract | `--json`, the shapes below |
| the human contract | a table, for a person reading a terminal |

The tool never writes a temporary file, and it never asks for a secret.

## The JSON shapes

Every JSON document is one object. Its keys are the contract; the human
table is not. The shapes below are pinned by `tests/cli_tool.rs`, so a change
to one is a change to the contract.

### `fragment build`

```json
{
  "command": "fragment.build",
  "bytes_hex": "4d4c5000…",
  "bytes_len": 36,
  "fragment": {
    "version": 1,
    "class": "media",
    "flags": ["fin"],
    "priority": 6,
    "object_id": 7,
    "fragment_index": 0,
    "fragment_count": 1,
    "payload_len": 4,
    "header_ext_len": 10
  },
  "tlvs": [
    {
      "kind": 1,
      "name": "UPDATE_ID",
      "classification": "named",
      "length": 8,
      "value_hex": "0700000000000000",
      "value": { "update_id": 7 }
    }
  ],
  "body": { "length": 4, "sha256": "054edec1…" }
}
```

`class` is one of `control`, `cursor`, `media`, `feedback`, `path`.
`flags` is a list of `fin`, `anchor`, `retransmit`, `dup`,
`echo-requested`, `probe`, `urgent`. `classification` is `named`,
`reserved`, `experimental` or `unknown`; `value` is present only when the
TLV is named and its length is the one the registry declares.

### `fragment inspect`

```json
{
  "command": "fragment.inspect",
  "carrier": "ws",
  "bytes_len": 36,
  "header": { …the fragment's fields… },
  "tlvs": [ …the header extension's entries… ],
  "body": { "length": 4, "sha256": "054edec1…" }
}
```

### `object assemble`

```json
{
  "command": "object.assemble",
  "status": "complete",
  "reason": null,
  "fragments": 2,
  "dropped_fragments": 0,
  "open_objects": 0,
  "open_bytes": 0,
  "object": {
    "object_id": 7,
    "class": "media",
    "flags": ["fin"],
    "priority": 6,
    "payload_len": 0,
    "payload_sha256": "e3b0c442…",
    "is_anchor": false,
    "is_duplicate": false,
    "echo_of_object_id": null,
    "deadline_epoch_ms": null,
    "update": null,
    "tlvs": []
  }
}
```

`status` is `complete`, `pending` or `rejected`. `reason` is the rejection's
name (`duplicate`, `fragment_mismatch`, `expired`, …) when the status is
`rejected`, and `null` otherwise. `object` is `null` unless the status is
`complete`. `update` holds the update metadata when the object carries it.

### `ack build` and `ack inspect`

```json
{
  "command": "ack.build",
  "ack": {
    "highest_applied_update_id": 10,
    "ack_bitmap": "0000000000000015",
    "chain_epoch": 0,
    "applied_cell_count": 510,
    "stale_cells": 0,
    "converged": true,
    "render_latency_ms": 0.0,
    "has_render_latency_ms": false,
    "echo_of_input_seq": 0,
    "ack_epoch_ms": 0
  },
  "window": {
    "size": 64,
    "highest_applied_update_id": 10,
    "applied_count": 3,
    "applied_update_ids": [10, 8, 6],
    "empty": false
  },
  "cell_counts": {
    "applied_cell_count": 510,
    "stale_cells": 0,
    "converged": true
  },
  "bytes_hex": "080a101520fe033001",
  "bytes_len": 9
}
```

`ack inspect` prints the same `ack`, `window` and `cell_counts` objects, and
no `bytes_hex`.

The acknowledgement's bytes are the schema's `MediaAck`
([`mlp-proto`](https://github.com/medialaneprotocol/mlp-proto)): the field
numbers are that schema's normative table, and the encoding is its canonical
one — a field equal to its default is omitted.

### `limits`

```json
{
  "command": "limits",
  "protocol_version": 1,
  "groups": [
    {
      "name": "objects",
      "spec": "§4.4",
      "entries": [
        {
          "name": "MAX_OPEN_OBJECTS",
          "value": 4,
          "unit": "objects",
          "spec": "§4.4"
        }
      ]
    }
  ]
}
```

The groups are `objects`, `fragments`, `parts`, `updates`, `regions`,
`queues`, `continuity`, `timeouts` and `invariants`. An entry's `value` is a
number for a cap and the specification's own text for a rule or an
invariant; `unit` is `null` for a rule.

Every numeric cap is the crate's own constant, so the table cannot drift
from the code that enforces it.

### `conformance`

```json
{
  "command": "conformance",
  "corpus": "../mlp-conformance",
  "scenarios": [
    {
      "id": "s14-t0-baseline",
      "verdict": "match",
      "expected_digest": "1e1945c0f1ad9284",
      "actual_digest": "1e1945c0f1ad9284",
      "violations": 0,
      "duration_ms": 2000,
      "detail": null
    }
  ],
  "summary": { "total": 16, "matched": 16, "failed": 0 }
}
```

`verdict` is `match`, `mismatch`, `missing` or `error`. A scenario that
does not match its committed digest is a failed check: the command exits
with the status `1`.

## The corpus

The corpus is a sibling repository —
[`mlp-conformance`](https://github.com/medialaneprotocol/mlp-conformance) —
and the tool reads it. It is never copied into this repository.

```sh
mlp-core conformance --corpus ../mlp-conformance
```

The command reads the corpus's committed scenarios and link profiles,
replays each one through this build's simulator, and compares the decision
trace it produces with the trace the corpus committed. The corpus's own gate
is the authority on the corpus; this command is the authority on the build.

`MLP_CONFORMANCE_DIR` names the corpus when `--corpus` is not given.

## The digests

`body.sha256`, `payload_sha256` and `object.payload_sha256` are the SHA-256
of the bytes, as 64 lower-case hexadecimal digits. The tool computes SHA-256
itself, so it has no dependency that could change what it prints.
