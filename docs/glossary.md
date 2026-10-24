# Glossary (MLP)

- **Lane**: a full-duplex media session between host and viewer(s).
- **Fragment**: the wire-level unit (22-byte header + TLV + payload).
- **Object**: a reassembled fragment set; carries one update part.
- **Update**: a media state change (self-contained or chained).
- **Anchor**: an update that may apply onto any held state.
- **Cell**: a unit of the repair grid (default 64x64 px).
- **Convergence**: receiver surface equals sender emitted state.
- **Echo**: the round trip from input application to ack naming it.
- **Pacer**: token-bucket rate limiter (INV-7).
- **BWE**: bandwidth estimator.
- **Repair plan**: the set of cells to re-send after loss.
- **Parking**: suspending a session for later resume.
