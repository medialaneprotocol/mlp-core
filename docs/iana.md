# IANA Considerations (MLP)

Draft IANA registration requests for the Media Lane Protocol. To be folded
into the eventual Internet-Draft.

## Media type

`application/mlp-fragment` — a single MLP fragment as defined in MLP spec
section 6.2 (fixed 22-byte header + TLV extension + payload).

## TLV registry

A new registry "MLP Header Extension TLV Types" with the following initial
assignments (MLP spec section 6.3):

| Value | Name | Length | Reference |
|---|---|---|---|
| 0x01 | UPDATE_ID | 8 | MLP |
| 0x02 | UPDATE_BASE | 8 | MLP |
| 0x03 | PART (index, count) | 4 | MLP |
| 0x04 | UPDATE_BYTES | 4 | MLP |
| 0x05 | CHAIN_EPOCH | 4 | MLP |
| 0x06 | REFLECTS_INPUT_SEQ | 8 | MLP |
| 0x07 | SEND_EPOCH_MS | 8 | MLP |
| 0x08 | CELL_COUNT | 4 | MLP |
| 0x09 | BUDGET_BYTES | 4 | MLP |
| 0x0A-0x7F | Unassigned | variable | MLP |
| 0x80-0xFF | Private use | variable | MLP |

Allocation policy: Specification Required (RFC 8126).

## Fragment class registry

| Value | Name |
|---|---|
| 0 | CONTROL |
| 1 | CURSOR |
| 2 | MEDIA |
| 3 | FEEDBACK |
| 4 | PATH |

## Priority

0..7, lower is higher priority. 0-1 reserved for control/input.

## Port / ALPN

MLP is carrier-agnostic (WebSocket, gRPC, QUIC). No dedicated port or ALPN
string is requested; the carrier's own ALPN applies.
