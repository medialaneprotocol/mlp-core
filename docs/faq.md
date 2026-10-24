# FAQ (MLP)

## Is MLP a replacement for WebRTC?

No. MLP is a transport for real-time interactive media over slow/lossy
links. It can run over WebSocket, gRPC, or QUIC. WebRTC is a full
peer-to-peer stack; MLP is a lane.

## Does MLP define the codec?

No. MLP moves bytes and accounts for them; the codec decides what the
bytes mean (MLP spec §1).

## Is MLP encrypted?

Encryption is the carrier's responsibility (TLS/WSS/gRPC-TLS). MLP
itself does not encrypt.

## Can I use MLP without the pacer?

Yes, via the `pacing` feature gate. Without it, the sender emits without
rate limiting (best effort).

## How do I measure against the harness?

See `mlp-harness` and `docs/telemetry.md`. The harness predicts echo p95,
convergence time, and byte counts.

## What is the minimum viable implementation?

Wire format (Level 0) + assembly (Level 1). See `docs/interop.md`.
