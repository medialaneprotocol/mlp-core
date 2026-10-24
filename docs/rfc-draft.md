# Internet-Draft: Media Lane Protocol (MLP)

**Status:** Draft (active development, v0.1.0)
**Author:** Vitaly Kuzyaev <vitkuz573@gmail.com>

## Abstract

The Media Lane Protocol (MLP) is a transport for real-time interactive
media over links that are slow, lossy, or both. A lane carries three kinds
of traffic — control, media, and feedback — and keeps them apart. MLP is
sans-IO: it never touches a socket, never reads a clock, and never
allocates on behalf of a runtime.

## 1. Introduction

### 1.1 Motivation

Interactive media (remote desktop, cloud gaming, live collaboration)
requires low latency even on poor links. Existing transports (WebRTC,
RTP) are complex and tightly coupled to specific use cases. MLP provides
a minimal, verifiable transport that can run over WebSocket, gRPC, or QUIC.

### 1.2 Design Goals

1. A receiver never holds state it cannot verify.
2. A sender always knows what to re-send, under a rate it measured.
3. The protocol moves bytes and accounts for them; it never interprets
   their content (codec-agnostic).
4. Sans-IO: time arrives as a parameter, outbound bytes leave as values.

### 1.3 Non-Goals

- MLP does not define what an update's bytes mean (codec's business).
- MLP does not provide encryption (carrier's responsibility).
- MLP does not define a signaling protocol.

## 2. Conventions

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and
"OPTIONAL" in this document are to be interpreted as described in BCP 14
[RFC2119] [RFC8174].

## 3. Overview

### 3.1 Lane Model

A lane carries three traffic classes:

- **Control**: handshake, session continuity, teardown.
- **Media**: updates moved whole and in order.
- **Feedback**: what the receiver applied, link rate, intermediary drops.

### 3.2 Sans-IO Boundary

- Time arrives as an `Instant` parameter.
- Outbound bytes leave as `Action` values.
- State changes are driven by `StateMachine`.

## 4. Wire Format

### 4.1 Fragment Header (22 bytes)

See docs/wire-format.md for the bit-level layout.

Magic: `MLP\0` (0x4D 0x4C 0x50 0x00).
Version: 1.
Fixed header: 22 bytes.

### 4.2 TLV Header Extension

Sequence of (kind: u8, length: u8, value: length bytes). Unknown kinds
are skipped by length (forward compatibility).

### 4.3 Fragment Classes

| Value | Name | Description |
|---|---|---|
| 0 | CONTROL | Handshake, session control |
| 1 | CURSOR | Pointer updates (unreliable, latest-wins) |
| 2 | MEDIA | Media updates (reliable, ordered) |
| 3 | FEEDBACK | Acknowledgements, reports |
| 4 | PATH | Path selection, migration |
| 5 | DATAGRAM | Unreliable datagrams |
| 6 | FEC | Forward error correction parity |

### 4.4 UpdateMeta

Carried in the extension of every media fragment part. See docs/wire-format.md.

## 5. Object Assembly

Fragments are reassembled by object_id. Dedup by (object_id, fragment_index).
Open-object table bounded by MAX_OPEN_OBJECTS.

## 6. Media Sender

### 6.1 Update Kinds

Delta, Anchor, Idle, Pulse, Repair.

### 6.2 Scheduling

Priority bands P0-P3. Repair plans with deadlines. Anchor threshold.

### 6.3 Pacing

Token bucket + 1-second window burst (INV-7). Reserved floor for control.

### 6.4 Bandwidth Estimation

Probe-based. Modes: StartUp, Steady, Degraded, Floor, NoFeedback.

### 6.5 Cell Accounting

Per-receiver convergence over a repair-cell grid. INV-2: receiver surface
equals sender emitted state at convergence.

### 6.6 Input Path

Sequencing, deadlines, pressed set. INV-3: no input applied twice.

### 6.7 Resume and Parking

Session parking, reconnect ladder, session continuity.

### 6.8 Flow Control

Per-stream and connection-level credits (QUIC-style).

### 6.9 Quality of Service

Per-stream priority and bandwidth allocation.

### 6.10 Loss Detection

Gap-based (QUIC-style). Packet threshold: 3.

### 6.11 Multipath

Multiple paths, path selection, migration.

### 6.12 Forward Error Correction

XOR parity over fragment groups.

### 6.13 0-RTT Resumption

Session tickets, early data, anti-replay.

### 6.14 Connection Migration

Path probing, challenge-response, seamless switch.

## 7. Media Receiver

### 7.1 Assembly

Fragment reassembly, dedup, open-object table.

### 7.2 Apply and Ack

Whole-update application. Ack with echo_of_input_seq.

### 7.3 Feedback

Goodput reporting, loss attribution.

## 8. Session Lifecycle

Handshake, park, resume, close. State machine with defined transitions.

## 9. Invariants

INV-1 through INV-10. See docs/invariants.md.

## 10. Security Considerations

See docs/security.md.

## 11. IANA Considerations

See docs/iana.md.

## 12. References

### 12.1 Normative

- RFC 2119: Key words
- RFC 8174: Ambiguity of uppercase
- RFC 9000: QUIC (transport parameters, flow control)
- RFC 9002: QUIC loss detection

### 12.2 Informative

- RFC 8999: QUIC version-independent properties
- qlog: structured logging

## Appendix A. Test Vectors

conformance/vectors.json

## Appendix B. TLA+ Model

docs/model/mlp.tla

## Appendix C. Change Log

CHANGELOG.md
