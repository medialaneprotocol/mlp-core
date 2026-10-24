# Version-Independent Properties (MLP)

Adapted from QUIC's "Version-Independent Properties" (RFC 8999). These
properties hold for every version of MLP and MUST NOT change. An
implementation that observes them can identify MLP traffic and parse its
framing regardless of version.

## Invariants

1. **Magic.** Every fragment begins with the 4-byte magic `MLP\0`
   (`0x4D 0x4C 0x50 0x00`).
2. **Fixed header.** The fragment header is exactly 22 bytes, in the
   documented field order and endianness.
3. **Length prefix.** `payload_len` and `header_ext_len` are unsigned
   16/32-bit little-endian integers immediately following the fixed fields.
4. **TLV framing.** The header extension is a sequence of TLV entries:
   1-byte kind, 1-byte length, then value. Unknown kinds are skipped by
   length (forward compatibility).
5. **Fragment boundary.** A fragment never spans more than one MTU.
6. **Carrier independence.** The framing is identical on every carrier
   (WebSocket, gRPC, QUIC, datagram); only the outer framing differs.

## Version negotiation

MLP carries `MEDIA_PROTOCOL_VERSION` (currently 1) in byte 4 of every
fragment. A receiver that does not support the sender's version MUST
reject with `UnsupportedVersion` and close the lane. There is no
in-band version negotiation beyond this byte; carrier-level negotiation
(TLS ALPN) is the recommended vehicle for larger changes.

## Greasing

Implementations MUST occasionally send fragments with unknown TLV kinds
and unknown fragment classes so that receivers exercise their skip paths.
The reserved greasing TLV kind is `0x7F` (value: any 0..255 bytes).
The reserved greasing fragment class is `0x0F`. Receivers that reject
these are non-conformant.
