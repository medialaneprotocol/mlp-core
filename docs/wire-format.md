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

bit 0 = ANCHOR, bit 1 = ECHO_REQUESTED, bit 2 = DUPLICATE.

## TLV header extension

Sequence of (kind: u8, length: u8, value: length bytes). Unknown kinds
are skipped by length (forward compatibility). See `src/media/pdu.rs::tlv`.

## UpdateMeta

Carried in the extension of every media fragment part:

- UPDATE_ID (u64)
- UPDATE_BASE (u64, 0 = self-contained)
- PART (u16 index, u16 count)
- UPDATE_BYTES (u32)
- CHAIN_EPOCH (u32)
- REFLECTS_INPUT_SEQ (u64)
- SEND_EPOCH_MS (u64)
