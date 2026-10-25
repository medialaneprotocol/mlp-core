# Roadmap (MLP)

## v0.1 (current)
- [x] Wire format (fragment, TLV, UpdateMeta)
- [x] Object assembly (dedup, open-object table)
- [x] Cell accounting and convergence
- [x] Pacing (token bucket + window)
- [x] Bandwidth estimation
- [x] Scheduler and repair plans
- [x] Input path
- [x] Resume and parking
- [x] Flow control (per-stream + connection)
- [x] QoS (tiers, bandwidth allocation)
- [x] Loss detection (QUIC-style)
- [x] Multipath and path probing
- [x] Stream multiplexing
- [x] 0-RTT session resumption
- [x] Connection migration
- [x] cfg-гейты (pacing, path-selection, resume, input)
- [x] Prometheus metrics
- [x] Conformance vectors
- [x] Reference implementations (Go, Python, C)
- [x] Interop runner
- [x] RFC draft (360+ lines)
- [x] Docker deployment

## v0.2 (planned)
- [ ] Full Internet-Draft submission
- [ ] Complete Go/Python/C implementations (100% parity)
- [ ] Real deployment (production server)
- [ ] Bench on real networks
- [ ] IANA registrations
- [ ] Interop matrix with independent implementations
- [ ] Formal verification (TLA+ complete model)
- [ ] BBR-style congestion control
- [ ] Session key rotation
- [ ] Multipath load balancing
- [x] Forward error correction (XOR parity on the wire, spec §6.9)

## v1.0 (target)
- [ ] RFC published
- [ ] 3+ independent implementations
- [ ] Production deployments
- [ ] Interop testing complete
