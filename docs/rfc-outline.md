# RFC Outline (MLP)

Draft outline for the eventual Internet-Draft. Section numbers follow
RFC conventions.

## 1. Introduction
- Motivation: real-time media over slow/lossy links.
- Goals and non-goals.

## 2. Conventions and Definitions
- RFC 2119/8174 keywords.
- Notation.

## 3. Overview
- Lane model: control, media, feedback.
- Sans-IO design.

## 4. Wire Format
- 4.1 Fragment header (22 bytes).
- 4.2 TLV header extension (registry).
- 4.3 Fragment classes.
- 4.4 UpdateMeta.

## 5. Object Assembly
- Dedup, open-object table, caps.

## 6. Media Sender
- 6.1 Update kinds.
- 6.2 Scheduling and repair plans.
- 6.3 Pacing (token bucket, INV-7).
- 6.4 Bandwidth estimation.
- 6.5 Cell accounting and convergence.
- 6.6 Input path.
- 6.7 Resume and parking.

## 7. Media Receiver
- 7.1 Assembly.
- 7.2 Apply and ack.
- 7.3 Feedback.

## 8. Session Lifecycle
- Handshake, park, resume, close.

## 9. Multipath and Connection Migration
- Path selection, path change, migration.

## 10. Stream Multiplexing
- stream_id TLV, per-stream state.

## 11. 0-RTT Resumption
- Session tickets, early data, anti-replay.

## 12. Security Considerations
- See docs/security.md.

## 13. IANA Considerations
- See docs/iana.md.

## 14. References
- Normative and informative.

## Appendices
- A. Test vectors (conformance/vectors.json).
- B. TLA+ model (docs/model/mlp.tla).
- C. Change log.
