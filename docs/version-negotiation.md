# Version Negotiation (MLP)

MLP carries `MEDIA_PROTOCOL_VERSION` (currently 1) in byte 4 of every
fragment. There is no in-band version negotiation message; the carrier
(TLS ALPN, WebSocket subprotocol) is the recommended vehicle for major
version changes.

## Rules

1. A receiver that does not support the sender's version MUST reject the
   fragment with `UnsupportedVersion` and close the lane.
2. A sender MUST NOT change version mid-session.
3. Unknown TLV kinds (0x10..=0xFF) are skipped by length, so minor
   extensions do not require a version bump.
4. The magic (`MLP\0`) and fixed 22-byte header are version-independent
   (see `docs/invariants.md`).
