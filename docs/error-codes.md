# Error Code Registry (MLP)

Adapted from the QUIC error code registry (RFC 9000, its section 20.1).
MLP uses a single byte error code on the wire (`RejectReason` / `MediaErrorCode`).

## RejectReason (protocol-level)

| Code | Name | Description |
|---|---|---|
| 0 | Ok | No error. |
| 1 | BadMagic | Fragment magic mismatch. |
| 2 | BadVersion | Unsupported protocol version. |
| 3 | BadClass | Unknown fragment class. |
| 4 | BadFlags | Invalid flag combination. |
| 5 | BadPriority | Priority out of range or reserved misuse. |
| 6 | BadFragment | Fragment index/count invalid. |
| 7 | BadLength | Payload or extension length invalid. |
| 8 | BadExt | Header extension malformed or too large. |
| 9 | MetaMismatch | Fragment metadata disagrees with decoded body. |
| 10 | Duplicate | Duplicate fragment or object. |
| 11 | FragmentMismatch | Fragment count mismatch within an object. |
| 12 | AssemblyEvicted | Assembly table full. |
| 13 | Expired | Deadline passed. |
| 14 | Oversize | Object exceeds cap. |
| 15 | Deferred | Deferred by in-flight cap or rate limit. |
| 16 | RateLimited | Pacer refused. |

## Allocation policy

New codes are allocated by IETF review (Specification Required). Codes
0x80-0xFF are reserved for private/experimental use.
