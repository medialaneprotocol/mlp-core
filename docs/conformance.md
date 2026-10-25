# Conformance Specification (MLP)

This document defines the conformance requirements for any MLP implementation.
An implementation is **conformant** if it passes all tests in this specification.

## Conformance Levels

### Level 0: Wire Format
- Parse and encode fragments byte-for-byte.
- Parse and encode TLV header extensions.
- Parse and encode UpdateMeta.
- Handle unknown TLV kinds (skip by length).
- Reject malformed fragments with correct error codes.

**Test vectors:** `conformance/vectors.json`

### Level 1: Object Assembly
- Reassemble fragments by object_id.
- Deduplicate by (object_id, fragment_index).
- Handle partial assembly (pending state).
- Enforce caps (MAX_OPEN_OBJECTS, MAX_FRAGMENT_COUNT).
- Report assembly errors.

### Level 2: Session
- Handshake (MediaHello/MediaHelloAck).
- Media update enqueue and delivery.
- Acknowledgement with echo_of_input_seq.
- Convergence (receiver surface = sender emitted state).
- Park and resume.
- Close and teardown.

### Level 3: Full Protocol
- Pacing (token bucket + 1-second window, INV-7).
- Bandwidth estimation (probe-based).
- Cell accounting and convergence (INV-2).
- Input path (sequencing, deadlines, pressed set).
- Repair plans (anchor + delta).
- Flow control (per-stream + connection).
- Loss detection (gap-based).
- 0-RTT session resumption.
- Connection migration.
- Multipath and path probing.
- Stream multiplexing.
- QoS (priority, bandwidth allocation).

## Conformance Test Suite

### Required tests (all implementations)

1. **Wire format round-trip**: encode then parse equals original.
2. **Unknown TLV skip**: fragment with unknown TLV kind parses correctly.
3. **Malformed rejection**: truncated/bad-magic fragments rejected.
4. **Object assembly**: multi-fragment reassembly.
5. **Dedup**: duplicate fragments dropped.
6. **Convergence**: sender and receiver converge on test scene.

### Optional tests (Level 3)

7. **Pacing**: bytes in 1-second window ≤ budget.
8. **Echo promise**: echo p95 ≤ 2×RTT + 250 ms.
9. **Loss recovery**: heal after fragment loss.
10. **Flow control**: respect credit limits.

## Test Vectors

Located in `conformance/vectors.json`. Each vector is a hex-encoded fragment
that must parse and re-encode byte-for-byte.

## Interop Testing

Run `scripts/interop.sh` to validate all implementations against the same
vectors. All must pass.

## Compliance Matrix

See `docs/compliance.md` for the feature matrix.
