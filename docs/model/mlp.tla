---- MODULE mlp ----
\* Sketch of the MLP sender/receiver convergence property.
\* This is a starting point, not a complete model.

EXTENDS Naturals, Sequences

CONSTANTS Cells, MaxUpdates

VARIABLES sender_pending, receiver_applied, sender_emitted

Init ==
    /\ sender_pending = Cells
    /\ receiver_applied = 0
    /\ sender_emitted = 0

\* Sender emits an update covering some pending cells.
Emit ==
    /\ sender_emitted < MaxUpdates
    /\ sender_emitted' = sender_emitted + 1
    /\ UNCHANGED <<sender_pending, receiver_applied>>

\* Receiver applies an emitted update: pending decreases.
Apply ==
    /\ sender_emitted > receiver_applied
    /\ receiver_applied' = receiver_applied + 1
    /\ sender_pending' = 0  \* simplified: one update clears all
    /\ UNCHANGED sender_emitted

\* Convergence: no pending cells.
Converged == sender_pending = 0

Spec == Init /\ [][Emit \/ Apply]_<<sender_pending, receiver_applied, sender_emitted>>

\* Invariant: pending never exceeds Cells.
PendingBound == sender_pending <= Cells

====
