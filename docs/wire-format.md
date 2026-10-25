# Wire Format (MLP)

## Fragment (fixed 22-byte header)

```
 0                   1                   2
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                          Magic (4)                          |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|  Version  |  Class  |  Flags  |  Priority |                 |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                        Object ID (4)                         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|       Fragment Index (2)      |      Fragment Count (2)      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                      Payload Length (4)                      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|      Header Extension Length (2)      |                       |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

Magic = `MLP\0` (`0x4D 0x4C 0x50 0x00`). Version = 1.

## Fragment classes

0 = CONTROL, 1 = CURSOR, 2 = MEDIA, 3 = FEEDBACK, 4 = PATH.

## Priority

0..7, lower is higher. 0-1 reserved for control/input.

## Flags

bit 0 = FIN, bit 1 = ANCHOR, bit 2 = RETRANSMIT, bit 3 = DUP,
bit 4 = ECHO_REQUESTED, bit 5 = PROBE, bit 6 = URGENT. Bit 7 is reserved
and must be zero (`BAD_FLAGS`).

## TLV header extension

Sequence of (kind: u8, length: u8, value: length bytes). The registry
(MLP spec §6.3):

| kind | name | value |
|---|---|---|
| `0x01` | `UPDATE_ID` | u64 — the update id this part belongs to |
| `0x02` | `UPDATE_BASE` | u64 — the update this one continues (`0` = self-contained) |
| `0x03` | `PART` | u16 index ‖ u16 count |
| `0x04` | `UPDATE_BYTES` | u32 — total payload bytes of the whole update |
| `0x05` | `CHAIN_EPOCH` | u32 — changes when the sender breaks the chain |
| `0x06` | `REFLECTS_INPUT_SEQ` | u64 — highest input seq reflected by this update |
| `0x07` | `SEND_EPOCH_MS` | u64 — sender's session-relative ms at first emission |
| `0x08` | `DEADLINE_EPOCH_MS` | u64 — absolute session-relative deadline |
| `0x09` | `ECHO_OF_OBJECT_ID` | u32 — object being echoed or acked |
| `0x0A` | `BUDGET_BYTES` | u32 — the sender's current per-update budget |
| `0x0B` | `PACE_BPS` | u32 — the sender's current pace |
| `0x0C` | `PATH_HINT` | u8 — carrier the sender believes is active |
| `0x0D` | `INPUT_SEQ` | u64 — input object sequence |
| `0x0E` | `CELL_COUNT` | u32 — the sender's cell grid size |
| `0x0F` | `COST_MS` | u16 — measured processing cost reported by a hop |
| `0x10` | `STREAM_ID` | u32 — media stream (0 = default) |
| `0x11` | `FEC_GROUP_ID` | u64 — the update id whose parts this parity object protects (§6.9) |
| `0x12` | `FEC_K` | u16 — the protected group's part count (2..=8) |
| `0x13` | `PATH_ID` | u8 — the path this fragment was sent over |
| `0x7F` | `GREASE` | greasing; receivers must exercise their skip path |

Kinds `0x10..=0xEF` are skipped silently (forward compatibility);
`0xF0..=0xFF` are experimental: skipped and counted. See
`src/media/pdu.rs::tlv`.

## UpdateMeta

Carried in the extension of every media fragment part:

- UPDATE_ID (u64)
- UPDATE_BASE (u64, 0 = self-contained)
- PART (u16 index, u16 count)
- UPDATE_BYTES (u32)
- CHAIN_EPOCH (u32)
- REFLECTS_INPUT_SEQ (u64)
- SEND_EPOCH_MS (u64)
